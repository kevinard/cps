use std::{fs, path::Path};

use anyhow::{Context, Result, ensure};
use rustix::fs::FlockOperation;

use super::{
    BootEntry, INITRD_HASH, Inventory, LINUX_HASH, LoaderPolicy, find_known_good, inventory,
    lock_esp, manage, policy, require_valid, same_root, uefi, verify_fallback_artifacts,
};
use crate::{
    boot::btrfs_boot::BtrfsVolume,
    image::root::Root,
    kernel::{handover, pinned_reader},
    verify::{require_quick, require_staged_quick},
};

const MARKER: &str = "cps.handover";

/// Validate and disarm the firmware trial before requesting shutdown. The
/// shutdown finalizer loads the target only after merging the running /etc.
pub(in crate::boot) fn prepare(esp: &Path, volume: &BtrfsVolume) -> Result<String> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        policy,
        entries,
        invalid,
        firmware,
    } = inventory(&esp, &lock, false)?;
    let policy = policy.context(super::NO_SELECTION)?;
    let id = policy
        .preferred
        .as_deref()
        .context("no staged trial is selected; stage the upgrade first")?;
    require_valid(&invalid, id)?;
    let trial = manage::find(&entries, id)?;
    ensure!(
        trial.tries.is_some_and(|left| left > 0),
        "the staged trial has no boot attempts left"
    );
    let parent = fallback(&policy, &entries, trial)?;
    let running = volume
        .running()?
        .context("the running root is not a registered boot environment")?;
    ensure!(
        running.0 == parent.id,
        "the staged trial's fallback is not the running root"
    );
    check_root(&esp, &lock, volume, parent, require_quick)?;
    verify_fallback_artifacts(&esp, parent)?;
    check_root(&esp, &lock, volume, trial, require_staged_quick)?;
    // Fail before stopping services if the kernel refuses this image, e.g.
    // because of its signature or the available KHO scratch space.
    load_target(&esp, trial)?;
    // A loaded image would let a plain `systemctl kexec` start the trial
    // without the /etc merge, so never return with it loaded.
    if let Err(error) = handover::unload().or_else(|_| handover::unload()) {
        return Err(error).context(format!(
            "the checked kexec image of {} is still loaded. Do not start kexec.target; a normal \
             reboot discards it and boots the staged trial",
            trial.id
        ));
    }
    if let Some(firmware) = firmware {
        firmware.disarm_handover(trial)?;
    } else {
        LoaderPolicy::pinned(&parent.id).write(&esp.join("loader/loader.conf"))?;
    }
    Ok(trial.id.clone())
}

/// Select the trial that [`prepare`] disarmed for the next firmware boot
/// again, after the shutdown request failed.
pub(in crate::boot) fn restore(esp: &Path, id: &str) -> Result<()> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        policy,
        entries,
        invalid,
        firmware,
    } = inventory(&esp, &lock, false)?;
    require_valid(&invalid, id)?;
    let policy = policy.context(super::NO_SELECTION)?;
    let trial = manage::find(&entries, id)?;
    ensure!(
        trial.tries.is_some_and(|left| left > 0),
        "the staged trial has no boot attempts left"
    );
    ensure!(
        policy.preferred.is_none(),
        "another trial is selected for the next boot"
    );
    let parent = fallback(&policy, &entries, trial)?;
    match firmware {
        Some(firmware) => firmware.rearm_handover(trial),
        None => LoaderPolicy::trial(&parent.id, &trial.id).write(&esp.join("loader/loader.conf")),
    }
}

/// Load the same registered trial after shutdown finalization succeeds.
pub(in crate::boot) fn finalize<T>(
    esp: &Path,
    volume: &BtrfsVolume,
    id: &str,
    merge: impl FnOnce(Root) -> Result<T>,
) -> Result<T> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        policy,
        entries,
        invalid,
        firmware,
    } = inventory(&esp, &lock, false)?;
    require_valid(&invalid, id)?;
    let policy = policy.context(super::NO_SELECTION)?;
    let trial = manage::find(&entries, id)?;
    let parent = fallback(&policy, &entries, trial)?;
    ensure!(
        policy.preferred.is_none(),
        "another trial is selected for the next boot"
    );
    if let Some(firmware) = firmware {
        firmware.require_no_next()?;
    }
    ensure!(
        volume.running()?.is_some_and(|(id, _)| id == parent.id),
        "the staged trial's fallback is not the running root"
    );
    // A second read of the registered entry and verification of the root
    // bind the shutdown action to the staged transaction we selected.
    ensure!(
        trial.tries.is_some(),
        "the selected handover target is no longer a trial"
    );
    check_root(&esp, &lock, volume, trial, require_staged_quick)?;
    let report = merge(volume.open_registered(&trial.btrfs, &trial.id)?)?;
    load_target(&esp, trial)?;
    Ok(report)
}

pub(in crate::boot) fn bless(esp: &Path, volume: &BtrfsVolume, id: &str) -> Result<Option<String>> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        policy,
        entries,
        invalid,
        firmware,
    } = inventory(&esp, &lock, false)?;
    require_valid(&invalid, id)?;
    let policy = policy.context(super::NO_SELECTION)?;
    let trial = manage::find(&entries, id)?;
    let running = volume
        .running()?
        .context("the running root is not a registered boot environment")?;
    ensure!(
        running.0 == trial.id,
        "running root differs from the staged handover trial"
    );
    // A direct UEFI blessing can stop after promoting BootOrder but before
    // renaming the already verified record. Recheck health and finish it.
    let selected = policy.default == trial.id && trial.tries.is_none();
    let resume_promotion = selected && firmware.is_some() && uefi::is_verified(trial);
    if selected && !resume_promotion {
        return Ok(None);
    }
    // systemd-boot renames the counted record before it pins the trial. A
    // known-good record that is not the default was rolled back or replaced
    // after its promotion, so pinning it again would undo that choice.
    ensure!(
        firmware.is_some() || trial.tries.is_some(),
        "{id} is already known-good but is not the default, either after a rollback or because \
         its promotion was interrupted. Select it with `cps boot activate {id}`"
    );
    ensure!(
        policy.preferred.is_none(),
        "another trial is selected for the next boot"
    );
    let parent = if resume_promotion {
        find_known_good(
            &entries,
            trial
                .parent
                .as_deref()
                .context("missing handover fallback")?,
        )?
    } else {
        fallback(&policy, &entries, trial)?
    };
    for entry in [parent, trial] {
        verify_fallback_artifacts(&esp, entry)?;
        check_root(&esp, &lock, volume, entry, require_quick)?;
    }
    let root = volume.open_registered(&trial.btrfs, id)?;
    ensure!(
        same_root(&Root::open(Path::new("/"))?, &root)?,
        "running Btrfs root differs from the handover entry"
    );
    handover::require_luo()?;
    crate::systemd::require_luo_systemd()?;
    crate::systemd::require_boot_health()?;
    if let Some(firmware) = firmware {
        firmware.bless_handover(&esp, trial, parent)?;
    } else {
        let dir = esp.join("loader/entries");
        uefi::rename_record(&dir, &trial.path, &dir.join(format!("{id}.conf")))?;
        LoaderPolicy::pinned(id).write(&esp.join("loader/loader.conf"))?;
    }
    Ok(Some(id.to_owned()))
}

fn fallback<'a>(
    policy: &LoaderPolicy,
    entries: &'a [BootEntry],
    trial: &BootEntry,
) -> Result<&'a BootEntry> {
    ensure!(
        trial.parent.as_deref() == Some(policy.default.as_str()),
        "the staged trial's fallback is no longer the configured default"
    );
    find_known_good(entries, &policy.default)
}

fn check_root(
    esp: &Path,
    lock: &fs::File,
    volume: &BtrfsVolume,
    entry: &BootEntry,
    verify: impl FnOnce(&crate::image::Image) -> Result<()>,
) -> Result<()> {
    volume.check_boot_device(esp, lock, &entry.root)?;
    let (image, rev) = policy::entry_identity(&entry.id)?;
    verify(&volume.open_image(&entry.btrfs, image, rev)?)
}

fn load_target(esp: &Path, trial: &BootEntry) -> Result<()> {
    let source = Root::open(esp)?;
    let description = &trial.description;
    let directory = format!("EFI/cps/{}", trial.id);
    let pinned = |path: &str, digest: &str| pinned_reader(source.file(path)?, digest, path);
    let (kernel, initrd, options) = if let Some(uki) = &trial.uki {
        let mut file = pinned(&format!("{directory}/{}", super::uki::NAME), &uki.sha256)?;
        let (kernel, initrd) = super::uki::handover_payloads(&mut file, &uki.cmdline)?;
        (kernel, initrd, uki.cmdline.as_str())
    } else {
        let artifact = |field, file, hash| {
            let path = format!("{directory}/{file}");
            ensure!(
                policy::boot_field(description, field)?.and_then(|value| value.strip_prefix('/'))
                    == Some(path.as_str()),
                "unexpected staged {field} path"
            );
            pinned(
                &path,
                policy::field(description, hash)?
                    .with_context(|| format!("missing staged {field} digest"))?,
            )
        };
        let kernel = artifact("linux", "vmlinuz.efi", LINUX_HASH)?;
        let initrd = artifact("initrd", "initramfs.img", INITRD_HASH)?;
        let options =
            policy::boot_field(description, "options")?.context("missing staged boot options")?;
        (kernel, initrd, options)
    };
    let cmdline = target_cmdline(options, &trial.id)?;
    handover::load(&kernel, &initrd, &cmdline)
}

fn target_cmdline(options: &str, id: &str) -> Result<String> {
    handover::require_options(options)?;
    ensure!(
        running_trial(options)?.is_none(),
        "{MARKER} is reserved for CPS staged handover"
    );
    Ok(format!("{MARKER}={id} {options}"))
}

pub(in crate::boot) fn running_trial(cmdline: &str) -> Result<Option<&str>> {
    let mut ids = cmdline
        .split_ascii_whitespace()
        .take_while(|arg| *arg != "--")
        .filter_map(|arg| arg.split_once('='))
        .filter_map(|(key, id)| (key == MARKER).then_some(id));
    let Some(id) = ids.next() else {
        return Ok(None);
    };
    ensure!(ids.next().is_none(), "multiple {MARKER} options");
    policy::entry_identity(id)?;
    Ok(Some(id))
}
