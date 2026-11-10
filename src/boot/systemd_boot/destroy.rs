use std::{
    fs::{self, File},
    io,
    os::unix::fs::MetadataExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, FileType, FlockOperation, RenameFlags, flock, renameat_with, unlinkat};

use super::{
    Inventory, inventory, lock_esp,
    policy::{self, BootEntry, LoaderPolicy},
    uefi, verify_fallback_artifacts,
};
use crate::{
    boot::{btrfs_boot::BtrfsVolume, uki},
    image::root::Root,
};

const SUFFIX: &str = ".cps-destroy";

pub(super) fn pending(esp: &Path) -> Result<Vec<BootEntry>> {
    let directory = esp.join(entries_dir(esp)?);
    let mut entries = Vec::new();
    let children = match fs::read_dir(directory) {
        Ok(children) => children,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error.into()),
    };
    for entry in children {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(SUFFIX)) else {
            continue;
        };
        ensure!(
            entry.file_type()?.is_file(),
            "invalid boot retirement record"
        );
        entries.push(policy::read_entry(entry.path(), stem)?);
    }
    Ok(entries)
}

fn entries_dir(esp: &Path) -> Result<&'static str> {
    Ok(if uefi::present(esp)? {
        uefi::DIRECTORY
    } else {
        "loader/entries"
    })
}

/// Retire an inactive Btrfs root and its private boot artifacts, with retryable cleanup.
pub fn destroy_btrfs_boot_environment(
    esp: &Path,
    volume: &Path,
    id: &str,
    dry_run: bool,
) -> Result<String> {
    destroy(esp, volume, id, dry_run, false)
}

/// Run every destroy-btrfs check for a trial as if its remaining boot
/// attempts were already spent, without changing anything.
pub(super) fn check_exhausted_destruction(esp: &Path, volume: &Path, id: &str) -> Result<()> {
    destroy(esp, volume, id, true, true).map(drop)
}

fn destroy(esp: &Path, volume: &Path, id: &str, dry_run: bool, exhausted: bool) -> Result<String> {
    ensure!(
        dry_run || !exhausted,
        "only a dry run can assume exhaustion"
    );
    policy::entry_identity(id)?;
    let volume = BtrfsVolume::open(volume)?;
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let esp_lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        policy,
        mut entries,
        firmware,
    } = inventory(&esp, &esp_lock, false)?;
    let policy = policy.context("no CPS boot environments registered")?;
    let mut retiring = pending(&esp)?;
    let resumed = retiring.iter().position(|entry| entry.id == id);
    let target = if let Some(index) = resumed {
        ensure!(
            !entries.iter().any(|entry| entry.id == id),
            "duplicate live and retiring boot environment {id}"
        );
        retiring.swap_remove(index)
    } else {
        let index = entries
            .iter()
            .position(|entry| entry.id == id)
            .with_context(|| format!("unknown boot environment {id}"))?;
        entries.remove(index)
    };
    let mut target = target;
    if exhausted && target.tries.is_some() {
        target.tries = Some(0);
    }
    let record = &target.btrfs;
    volume.check_fsid(&record.fsid)?;
    volume.check_boot_device(&esp, &esp_lock, &target.root)?;
    let selected = check_selection(&policy, &entries, &target)?;
    if let Some(firmware) = &firmware {
        firmware.check_removal(id)?;
    }
    for entry in entries.iter().chain(&retiring) {
        ensure!(
            !record.same_subvolume(&entry.btrfs),
            "another boot entry references the Btrfs root being destroyed"
        );
        let artifact_prefix = format!("/EFI/cps/{id}/");
        ensure!(
            !entry
                .description
                .lines()
                .filter_map(|line| line.trim_ascii_start().split_once([' ', '\t']))
                .any(
                    |(key, value)| matches!(key, "linux" | "initrd" | "efi" | "uki")
                        && value.trim_ascii().starts_with(&artifact_prefix)
                ),
            "another boot entry references this environment's artifacts"
        );
    }
    let root = match volume.open_registered(record, id) {
        Ok(root) => Some(root),
        Err(error)
            if resumed.is_some()
                && error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) =>
        {
            None
        }
        Err(error) => return Err(error).context("checking the Btrfs root to destroy"),
    };
    // Activation takes its image lock before the ESP lock. Never wait for an
    // image lock here while holding the ESP: a concurrent operation must retry.
    let _image_lock = root
        .as_ref()
        .map(|root| -> Result<File> {
            let state = root.open_image_state()?;
            let lock = state.file("lock")?;
            flock(&lock, FlockOperation::NonBlockingLockExclusive)
                .context("the Btrfs root is busy with another CPS operation")?;
            check_inactive(root)?;
            Ok(lock)
        })
        .transpose()?;
    ensure!(
        volume.running_parent()?.as_deref() != Some(id),
        "cannot destroy the running environment's fallback"
    );
    verify_fallback_artifacts(&esp, selected)?;
    let selected_record = &selected.btrfs;
    volume.check_boot_device(&esp, &esp_lock, &selected.root)?;
    let (image, revision) = policy::entry_identity(&selected.id)?;
    let _selected = volume
        .verify(selected_record, image, revision)
        .context("checking the selected known-good root before destruction")?;
    let _fallback = if let Some(parent) = selected
        .parent
        .as_deref()
        .and_then(|id| entries.iter().find(|entry| entry.id == id))
    {
        ensure!(
            parent.tries.is_none(),
            "the selected environment's parent is not known-good"
        );
        verify_fallback_artifacts(&esp, parent)?;
        let record = &parent.btrfs;
        volume.check_boot_device(&esp, &esp_lock, &parent.root)?;
        let (image, revision) = policy::entry_identity(&parent.id)?;
        Some(
            volume
                .verify(record, image, revision)
                .context("checking the retained fallback root")?,
        )
    } else {
        None
    };
    let esp_root = Root::open(&esp)?;
    check_artifacts(&esp_root, id)?;
    if dry_run {
        return Ok(id.to_owned());
    }
    let loader_entries = esp_root.dir(entries_dir(&esp)?)?;
    let filename = target
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid boot entry filename")?;
    let marker = if resumed.is_some() {
        filename.to_owned()
    } else {
        format!(
            "{}{SUFFIX}",
            filename
                .strip_suffix(".conf")
                .context("invalid boot entry filename")?
        )
    };
    if resumed.is_none() {
        // An exhausted preferred trial is no longer selected. Remove its
        // obsolete preference before withdrawing the entry from systemd-boot.
        if firmware.is_none()
            && let LoaderPolicy::Pinned {
                default,
                preferred: Some(preferred),
            } = &policy
            && preferred == id
        {
            LoaderPolicy::pinned(default).write(&esp.join("loader/loader.conf"))?;
        }
        renameat_with(
            &loader_entries,
            target.path.file_name().context("missing boot entry name")?,
            &loader_entries,
            &marker,
            RenameFlags::NOREPLACE,
        )?;
        // On VFAT the renamed record's size is durable only with its inode.
        File::open(Root::fd_path(&loader_entries).join(&marker))?.sync_all()?;
        loader_entries.sync_all()?;
    }
    if let Some(firmware) = firmware {
        firmware.remove(id)?;
    }
    if let Some(root) = &root {
        check_inactive(root)?;
        volume.destroy(record, root)?;
    }
    remove_artifacts(&esp_root, id)?;
    unlinkat(&loader_entries, &marker, AtFlags::empty())?;
    loader_entries.sync_all()?;
    Ok(id.to_owned())
}

fn check_selection<'a>(
    policy: &LoaderPolicy,
    entries: &'a [BootEntry],
    target: &BootEntry,
) -> Result<&'a BootEntry> {
    // The target was removed from the candidate inventory. Explicitly check
    // selectors before choosing a retained entry so Automatic cannot hide it.
    if let LoaderPolicy::Pinned { default, preferred } = policy {
        ensure!(
            default != &target.id,
            "cannot destroy the configured default environment"
        );
        ensure!(
            preferred.as_deref() != Some(&target.id) || target.tries == Some(0),
            "cannot destroy the selected boot environment"
        );
    }
    let selected = policy
        .selected(entries)
        .context("no retained default CPS boot environment")?;
    if matches!(policy, LoaderPolicy::Automatic) {
        ensure!(
            target.tries == Some(0)
                || (target.revision, target.id.as_str())
                    < (selected.revision, selected.id.as_str()),
            "cannot destroy the selected boot environment"
        );
    }
    ensure!(
        selected.tries.is_none(),
        "select a known-good environment before destroying old roots"
    );
    ensure!(
        selected.parent.as_deref() != Some(&target.id),
        "cannot destroy the selected environment's fallback"
    );
    ensure!(
        target.tries.is_none_or(|tries| tries == 0),
        "cannot destroy a trial with boot attempts left"
    );
    Ok(selected)
}

fn check_inactive(root: &Root) -> Result<()> {
    crate::systemd::offline_path(root)?;
    let device = root.as_file().metadata()?.dev();
    let number = format!("{}:{}", libc::major(device), libc::minor(device));
    let mounts = fs::read("/proc/self/mountinfo")?;
    for line in mounts.split(|byte| *byte == b'\n') {
        let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
        ensure!(
            fields.get(2).copied() != Some(number.as_bytes()),
            "cannot destroy a mounted Btrfs root"
        );
        // Btrfs reports a per-subvolume anonymous device in stat(), while
        // mountinfo can report the shared superblock device. Inspect visible
        // Btrfs mount points as well, including bind mounts of subdirectories.
        if fields
            .iter()
            .position(|field| *field == b"-")
            .and_then(|separator| fields.get(separator + 1))
            .copied()
            == Some(b"btrfs".as_slice())
        {
            let encoded = fields.get(4).context("invalid Btrfs mountinfo record")?;
            let mounted = crate::systemd::decode_mount_path(encoded)?;
            let metadata = fs::metadata(&mounted)
                .with_context(|| format!("checking Btrfs mount {}", mounted.display()))?;
            ensure!(
                metadata.dev() != device,
                "cannot destroy a mounted Btrfs root"
            );
        }
    }
    root.walk(None, |path, _, info| {
        ensure!(
            FileType::from_raw_mode(info.st_mode) != FileType::Directory
                || !matches!(info.st_ino, 2 | 256),
            "cannot destroy a root containing nested subvolume {path:?}"
        );
        Ok(true)
    })
}

fn check_artifacts(esp: &Root, id: &str) -> Result<()> {
    let fat = super::is_fat(esp.as_file())?;
    let path = format!("EFI/cps/{id}");
    let directory = match esp.subdir(&path) {
        Ok(directory) => directory,
        Err(error)
            if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    directory.walk(None, |path, _, info| {
        ensure!(
            ["vmlinuz.efi", "initramfs.img", uki::NAME]
                .iter()
                .any(|name| path == *name || (fat && path.eq_ignore_ascii_case(name)))
                && FileType::from_raw_mode(info.st_mode) == FileType::RegularFile,
            "unexpected file in retired boot artifacts: {path:?}"
        );
        Ok(false)
    })
}

fn remove_artifacts(esp: &Root, id: &str) -> Result<()> {
    let path = format!("EFI/cps/{id}");
    match esp.dir(&path) {
        Ok(directory) => {
            for name in ["vmlinuz.efi", "initramfs.img", uki::NAME] {
                match unlinkat(&directory, name, AtFlags::empty()) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => (),
                    Err(error) => return Err(error.into()),
                }
            }
            directory.sync_all()?;
            let parent = esp.dir("EFI/cps")?;
            unlinkat(&parent, id, AtFlags::REMOVEDIR)?;
            parent.sync_all()?;
        }
        Err(error)
            if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(revision: u64, parent: Option<&str>, tries: Option<u32>) -> BootEntry {
        let id = format!("cps-a47593c2-e985-42d2-92b6-44c7057e1e96-{revision}");
        BootEntry {
            path: format!("{id}.conf").into(),
            id,
            revision,
            tries,
            description: String::new(),
            title: String::new(),
            root: policy::BootRoot::Partition("7adff456-af50-498f-b39a-dcf87cb4a7c1".into()),
            parent: parent.map(str::to_owned),
            btrfs: crate::boot::btrfs_boot::BtrfsRecord {
                fsid: "11111111-1111-4111-8111-111111111111".into(),
                uuid: format!("22222222-2222-4222-8222-{revision:012}"),
                subvolid: 1000 + revision,
                path: format!("@environments/{revision}/root"),
            },
            uki: None,
        }
    }

    #[test]
    fn destruction_protects_boot_selection_fallback_and_unspent_trials() -> Result<()> {
        let old = entry(1, None, None);
        let base = entry(2, Some(&old.id), None);
        let current = entry(3, Some(&base.id), None);
        let trial = entry(4, Some(&current.id), Some(2));
        assert!(
            check_selection(
                &LoaderPolicy::trial(&base.id, &current.id),
                &[entry(1, None, None), entry(2, Some(&old.id), None)],
                &current
            )
            .is_err()
        );
        assert!(
            check_selection(
                &LoaderPolicy::pinned(&current.id),
                &[entry(1, None, None), entry(3, Some(&base.id), None)],
                &base
            )
            .unwrap_err()
            .to_string()
            .contains("fallback")
        );
        assert_eq!(
            check_selection(
                &LoaderPolicy::pinned(&current.id),
                &[
                    entry(2, Some(&old.id), None),
                    entry(3, Some(&base.id), None)
                ],
                &old
            )?
            .id,
            current.id
        );
        assert!(
            check_selection(
                &LoaderPolicy::pinned(&current.id),
                &[entry(3, Some(&base.id), None)],
                &trial
            )
            .is_err()
        );
        assert!(
            check_selection(
                &LoaderPolicy::Automatic,
                &[entry(2, Some(&old.id), None)],
                &current
            )
            .is_err()
        );
        let exhausted = entry(4, Some(&current.id), Some(0));
        assert!(
            check_selection(
                &LoaderPolicy::Automatic,
                &[entry(3, Some(&base.id), None)],
                &exhausted
            )
            .is_ok()
        );
        assert!(
            check_selection(
                &LoaderPolicy::trial(&current.id, &exhausted.id),
                &[entry(3, Some(&base.id), None)],
                &exhausted
            )
            .is_ok()
        );
        Ok(())
    }

    #[test]
    fn retirement_inventory_preserves_exhaustion_and_bounds_its_records() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("loader/entries");
        fs::create_dir_all(&directory)?;
        let id = entry(2, None, None).id;
        let marker = directory.join(format!("{id}+0-2{SUFFIX}"));
        let text = format!(
            "title Canopa\nsort-key cps\nversion 2\n# cps-boot-entry-version 1\n# cps-parent cps-51c2d73c-f68f-4cc7-98b0-6efbbc03c785-1\n# cps-linux-sha256 {}\n# cps-btrfs-fsid 11111111-1111-4111-8111-111111111111\n# cps-btrfs-uuid 22222222-2222-4222-8222-222222222222\n# cps-btrfs-subvolid 277\n# cps-btrfs-path @environments/trial/root\noptions root=PARTUUID=7adff456-af50-498f-b39a-dcf87cb4a7c1 rootfstype=btrfs rootflags=subvolid=277\n",
            "a".repeat(64)
        );
        fs::write(&marker, &text)?;
        let records = pending(temp.path())?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, id);
        assert_eq!(records[0].tries, Some(0));
        let selected = entry(1, None, None).id;
        LoaderPolicy::pinned(&selected).write(&temp.path().join("loader/loader.conf"))?;
        let listed = super::super::list_boot_environments(temp.path())?;
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_retiring && !listed[0].is_default);
        fs::write(&marker, format!("{text}{}", "#".repeat(64 * 1024)))?;
        assert!(
            pending(temp.path())
                .unwrap_err()
                .to_string()
                .contains("64 KiB")
        );
        Ok(())
    }
}
