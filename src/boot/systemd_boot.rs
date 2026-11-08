mod destroy;
pub(super) mod policy;
mod uefi;
pub use destroy::destroy_btrfs_boot_environment;

use std::{
    fs::{self, File},
    io::{self, Read, Seek},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FlockOperation, flock};
use tempfile::NamedTempFile;

use super::{
    btrfs_boot::{BtrfsRecord, BtrfsVolume, mark_registered},
    check_image_integrity,
    deploy::{OfflineDisk, offline_file},
    device::{is_fat, mounted_esp_disk},
    entry_id, kernel, tree, uki,
};
use crate::{
    image::{
        Image,
        diff::{ImageDiff, diff_open_images},
        root::Root,
    },
    kdl_format::boot_export::ExportReceipt,
    model::{digest, digest_reader, is_uuid, validate_digest},
};

use self::policy::{BootEntry, BootRoot, LoaderPolicy, field};

const LINUX_HASH: &str = "# cps-linux-sha256 ";
const INITRD_HASH: &str = "# cps-initrd-sha256 ";

#[derive(Debug)]
pub struct BootEnvironment {
    pub id: String,
    pub title: String,
    /// Empty when the entry boots by Btrfs filesystem UUID.
    pub root_partuuid: String,
    pub root_uuid: Option<String>,
    pub tries_left: Option<u32>,
    pub is_default: bool,
    pub is_retiring: bool,
}

struct Inventory {
    policy: Option<LoaderPolicy>,
    entries: Vec<BootEntry>,
    firmware: Option<uefi::Firmware>,
}

fn inventory(esp: &Path, lock: &File, direct: bool) -> Result<Inventory> {
    if direct || uefi::present(esp)? {
        if policy::directory_exists(&esp.join("loader/entries"))? {
            for entry in fs::read_dir(esp.join("loader/entries"))? {
                let name = entry?.file_name();
                ensure!(
                    !name
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .starts_with("cps-"),
                    "the ESP already contains CPS systemd-boot entries"
                );
            }
        }
        let firmware = uefi::Firmware::open(esp, lock)?;
        let mut entries = if uefi::present(esp)? {
            policy::boot_entries(&esp.join(uefi::DIRECTORY))?
        } else {
            Vec::new()
        };
        let policy = firmware.inventory(&mut entries)?;
        return Ok(Inventory {
            policy,
            entries,
            firmware: Some(firmware),
        });
    }
    let (policy, entries) = policy::inventory(esp)?;
    Ok(Inventory {
        policy,
        entries,
        firmware: None,
    })
}

pub fn list_boot_environments(esp: &Path) -> Result<Vec<BootEnvironment>> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let _lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let Inventory {
        policy,
        entries,
        firmware: _firmware,
    } = inventory(&esp, &_lock, false)?;
    let selected = policy
        .as_ref()
        .and_then(|policy| policy.selected(&entries))
        .map(|entry| entry.id.clone());
    let mut all: Vec<_> = entries.into_iter().map(|entry| (entry, false)).collect();
    for entry in destroy::pending(&esp)? {
        ensure!(
            !all.iter().any(|(live, _)| live.id == entry.id),
            "duplicate live and retiring boot environment {}",
            entry.id
        );
        all.push((entry, true));
    }
    all.sort_by(|(a, _), (b, _)| (b.revision, &b.id).cmp(&(a.revision, &a.id)));
    Ok(all
        .into_iter()
        .map(|(entry, is_retiring)| BootEnvironment {
            is_default: selected.as_deref() == Some(&entry.id),
            id: entry.id,
            title: entry.title,
            root_partuuid: entry.root.partuuid().unwrap_or_default().to_owned(),
            root_uuid: match entry.root {
                BootRoot::Btrfs(uuid) => Some(uuid),
                BootRoot::Partition(_) => None,
            },
            tries_left: entry.tries,
            is_retiring,
        })
        .collect())
}

pub fn rollback_boot_environment(esp: &Path, id: Option<&str>) -> Result<String> {
    rollback(esp, id, None, None)
}

pub(super) fn rollback(
    esp: &Path,
    id: Option<&str>,
    disk: Option<&OfflineDisk>,
    volume: Option<&BtrfsVolume>,
) -> Result<String> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let mounted = if disk.is_none() {
        mounted_esp_disk(&esp, &lock)?
    } else {
        None
    };
    let Inventory {
        policy,
        entries,
        firmware,
    } = inventory(&esp, &lock, false)?;
    let policy = policy.context("no CPS boot environments registered")?;
    let current = rollback_source(&policy, &entries, firmware.as_ref())?;
    let target = rollback_target(current, &entries, id)?;
    verify_fallback_artifacts(&esp, target)?;
    let _root_lock = if let Some(record) = &target.btrfs {
        let volume = volume.context("use --btrfs-volume to verify this rollback root")?;
        volume.check_boot_device(&esp, &lock, &target.root)?;
        let (image, revision) = policy::entry_identity(&target.id)?;
        Some(
            volume
                .verify(record, image, revision)
                .context("checking the Btrfs rollback root")?,
        )
    } else {
        None
    };
    if target.btrfs.is_none()
        && let Some(disk) = disk.or(mounted.as_ref())
    {
        let (image, revision) = policy::entry_identity(&target.id)?;
        disk.verify_filesystem(target.root.partuuid()?, image, revision)
            .context("checking the rollback root partition")?;
    }
    if let Some(firmware) = firmware {
        firmware.rollback(&esp, &target.id, &entries)?;
    } else {
        LoaderPolicy::pinned(&target.id).write(&esp.join("loader/loader.conf"))?;
    }
    Ok(target.id.clone())
}

pub(super) fn with_registered_images<T>(
    esp: &Path,
    volume: &BtrfsVolume,
    partuuid: Option<&str>,
    action: impl FnOnce(&[(String, Image)]) -> Result<T>,
) -> Result<T> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let root = volume.boot_root(partuuid)?;
    volume.check_boot_device(&esp, &lock, &root)?;
    let Inventory {
        entries,
        firmware: _firmware,
        ..
    } = inventory(&esp, &lock, false)?;
    let mut images = Vec::new();
    for entry in entries {
        if entry.root != root
            && !entry
                .btrfs
                .as_ref()
                .is_some_and(|record| volume.has_fsid(&record.fsid))
        {
            continue;
        }
        let record = entry
            .btrfs
            .as_ref()
            .context("expected a Btrfs entry for this partition")?;
        if entry.root != root {
            volume.check_boot_device(&esp, &lock, &entry.root)?;
        }
        let (image, revision) = policy::entry_identity(&entry.id)?;
        let image = volume
            .open_image(record, image, revision)
            .with_context(|| format!("opening registered root {}", entry.id))?;
        images.push((entry.id, image));
    }
    ensure!(
        !images.is_empty(),
        "no registered roots on the configured Btrfs volume"
    );
    action(&images)
}

pub(super) fn preview_btrfs_rollback(
    esp: &Path,
    volume: &BtrfsVolume,
    id: Option<&str>,
    paths: bool,
) -> Result<(String, ImageDiff)> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let Inventory {
        policy,
        entries,
        firmware,
    } = inventory(&esp, &lock, false)?;
    let policy = policy.context("no CPS boot environments registered")?;
    let current = rollback_source(&policy, &entries, firmware.as_ref())?
        .context("no configured default CPS boot environment")?;
    let target = rollback_target(Some(current), &entries, id)?;
    let open = |entry: &BootEntry| -> Result<Image> {
        let record = entry
            .btrfs
            .as_ref()
            .context("expected a Btrfs boot entry")?;
        volume.check_boot_device(&esp, &lock, &entry.root)?;
        let (image, revision) = policy::entry_identity(&entry.id)?;
        volume.open_image(record, image, revision)
    };
    verify_fallback_artifacts(&esp, target)?;
    let before = open(current).context("opening the configured default root")?;
    let after = open(target).context("opening the rollback root")?;
    check_image_integrity(&after)?;
    Ok((target.id.clone(), diff_open_images(&before, &after, paths)?))
}

fn rollback_source<'a>(
    policy: &LoaderPolicy,
    entries: &'a [BootEntry],
    firmware: Option<&uefi::Firmware>,
) -> Result<Option<&'a BootEntry>> {
    // Firmware has already consumed BootNext while the trial is running.
    // Its recorded parent is still the rollback target for that boot.
    let trial = firmware
        .map(|fw| fw.running_trial(entries))
        .transpose()?
        .flatten();
    Ok(trial.or_else(|| policy.selected(entries)))
}

fn rollback_target<'a>(
    current: Option<&'a BootEntry>,
    entries: &'a [BootEntry],
    id: Option<&str>,
) -> Result<&'a BootEntry> {
    if let Some(id) = id {
        return find_known_good(entries, id);
    }
    let current = current.context("no configured default CPS boot environment")?;
    if let Some(parent) = &current.parent {
        return find_known_good(entries, parent).context("the recorded parent is unavailable");
    }
    // Legacy entries predate explicit parent records.
    entries
        .iter()
        .find(|entry| entry.tries.is_none() && entry.revision < current.revision)
        .context("no previous known-good boot environment")
}

fn find_known_good<'a>(entries: &'a [BootEntry], id: &str) -> Result<&'a BootEntry> {
    let entry = entries
        .iter()
        .find(|entry| entry.id == id)
        .with_context(|| format!("unknown boot environment {id}"))?;
    ensure!(
        entry.tries.is_none(),
        "boot environment {id} is not known-good"
    );
    Ok(entry)
}

fn lock_esp(esp: &Path, operation: FlockOperation) -> Result<File> {
    ensure!(
        esp != Path::new("/"),
        "specify a dedicated EFI system partition"
    );
    let file = File::open(esp)?;
    ensure!(
        file.metadata()?.is_dir(),
        "expected an EFI system partition directory"
    );
    flock(&file, operation)?;
    Ok(file)
}

fn same_root(a: &Root, b: &Root) -> Result<bool> {
    let (a, b) = (a.as_file().metadata()?, b.as_file().metadata()?);
    Ok((a.dev(), a.ino()) == (b.dev(), b.ino()))
}

pub(super) fn bless_uefi(esp: &Path, volume: &BtrfsVolume) -> Result<Option<String>> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        entries, firmware, ..
    } = inventory(&esp, &lock, true)?;
    let firmware = firmware.context("expected the direct UEFI backend")?;
    let Some(trial) = firmware.running_trial(&entries)? else {
        return Ok(None);
    };
    let parent = find_known_good(
        &entries,
        trial
            .parent
            .as_deref()
            .context("missing UEFI trial fallback")?,
    )?;
    let verify = |entry: &BootEntry| {
        verify_fallback_artifacts(&esp, entry)?;
        volume.check_boot_device(&esp, &lock, &entry.root)?;
        let (id, revision) = policy::entry_identity(&entry.id)?;
        volume.verify(
            entry
                .btrfs
                .as_ref()
                .context("missing UEFI Btrfs identity")?,
            id,
            revision,
        )
    };
    let _fallback = verify(parent).context("checking the UEFI fallback")?;
    let image = verify(trial).context("checking the UEFI trial's health")?;
    let running = Root::open(Path::new("/"))?;
    ensure!(
        same_root(&running, &image.root)?,
        "running root differs from the firmware trial"
    );
    crate::systemd::require_boot_health()?;
    firmware.bless(&esp, trial, parent)?;
    Ok(Some(trial.id.clone()))
}

pub(super) fn with_running_environment<T>(
    volume: &BtrfsVolume,
    esp: &Path,
    partuuid: Option<&str>,
    action: impl FnOnce(Root, &str, u64) -> Result<T>,
) -> Result<T> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let root = volume.boot_root(partuuid)?;
    volume.check_boot_device(&esp, &lock, &root)?;
    let Inventory {
        policy,
        entries,
        firmware,
    } = inventory(&esp, &lock, false)?;
    if let Some(firmware) = &firmware {
        firmware.require_no_next()?;
    }
    let selected = policy
        .and_then(|policy| policy.selected(&entries))
        .context("no CPS boot environment is selected")?;
    ensure!(
        selected.tries.is_none(),
        "a trial is pending; boot it before making more package changes"
    );
    if let (BootRoot::Partition(expected), BootRoot::Partition(selected)) = (&root, &selected.root)
    {
        ensure!(
            selected == expected,
            "the selected root uses a different partition"
        );
    }
    let record = selected
        .btrfs
        .as_ref()
        .context("expected a registered Btrfs root")?;
    if selected.root != root {
        volume.check_boot_device(&esp, &lock, &selected.root)?;
    }
    let registered = volume.open_registered(record, &selected.id)?;
    let running = Root::open(Path::new("/"))?;
    ensure!(
        same_root(&registered, &running)?,
        "a different system is selected for the next boot; reboot before changing packages"
    );
    let (id, revision) = policy::entry_identity(&selected.id)?;
    action(running, id, revision)
}

pub struct Activation<R = String> {
    /// Btrfs activation accepts `None` to boot by filesystem UUID.
    pub root_partuuid: R,
    pub kernel: String,
    pub initrd: Option<String>,
    pub options: String,
    pub tries: u32,
    pub known_good: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BootMode {
    Split,
    Uki,
    Uefi,
}

/// A root payload to deploy before activation, optionally replacing a retired slot.
#[derive(Clone, Copy)]
pub struct RootDeployment<'a> {
    pub image: &'a Path,
    pub replace: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub(super) struct Deployment<'a> {
    pub disk: &'a OfflineDisk,
    pub receipt: &'a Path,
    pub root: Option<RootDeployment<'a>>,
    pub target: Option<&'a super::device::DevicePartition>,
}

pub fn activate_boot_environment(
    root: &Path,
    esp: &Path,
    activation: &Activation,
) -> Result<String> {
    Ok(activate(root, esp, activation, None, None, BootMode::Split)?.id)
}

/// Activate an environment after verifying its deployed root in an offline disk.
pub fn activate_deployed_boot_environment(
    root: &Path,
    esp: &Path,
    activation: &Activation,
    disk: &Path,
    receipt: &Path,
) -> Result<String> {
    let disk = OfflineDisk::open(disk)?;
    let deployment = Deployment {
        disk: &disk,
        receipt,
        root: None,
        target: None,
    };
    Ok(activate(
        root,
        esp,
        activation,
        Some(deployment),
        None,
        BootMode::Split,
    )?
    .id)
}

/// Deploy or verify an inactive partition before publishing its boot entry.
pub fn activate_device_boot_environment(
    root: &Path,
    esp: &Path,
    activation: &Activation,
    device: &Path,
    receipt: &Path,
    root_deployment: Option<RootDeployment<'_>>,
) -> Result<String> {
    let partuuid = canonical_partuuid(&activation.root_partuuid)?;
    let (disk, target) =
        super::device::DevicePartition::open(device, esp, &partuuid, root_deployment.is_some())?;
    let deployment = Deployment {
        disk: &disk,
        receipt,
        root: root_deployment,
        target: Some(&target),
    };
    Ok(activate(
        root,
        esp,
        activation,
        Some(deployment),
        None,
        BootMode::Split,
    )?
    .id)
}

pub(super) struct ActivationUpdate {
    pub id: String,
    pub files: Vec<String>,
    pub retired: Option<RetiredEntry>,
}

pub(super) struct RetiredEntry {
    pub id: String,
    pub file: String,
}

pub(super) fn activate<R: Clone + Into<Option<String>>>(
    root: &Path,
    esp: &Path,
    activation: &Activation<R>,
    deployment: Option<Deployment<'_>>,
    volume: Option<&BtrfsVolume>,
    mode: BootMode,
) -> Result<ActivationUpdate> {
    let sign_uki = mode != BootMode::Split;
    let direct = mode == BootMode::Uefi;
    let requested_partuuid = activation.root_partuuid.clone().into();
    let boot_root = if let Some(volume) = volume {
        volume.boot_root(requested_partuuid.as_deref())?
    } else {
        let uuid = requested_partuuid
            .as_deref()
            .context("expected a root PARTUUID")?;
        BootRoot::Partition(canonical_partuuid(uuid)?)
    };
    let partuuid = boot_root.partuuid().ok();
    validate_options(&activation.options)?;
    ensure!(
        (1..=99).contains(&activation.tries),
        "tries must be between 1 and 99"
    );
    ensure!(
        !direct || activation.tries == 1,
        "direct UEFI trials allow exactly one boot attempt"
    );
    let root = fs::canonicalize(root)?;
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    ensure!(
        root != Path::new("/")
            && esp != Path::new("/")
            && !esp.starts_with(&root)
            && !root.starts_with(&esp),
        "use separate, inactive image and EFI directories"
    );
    // Rollback and live writes acquire the ESP lock before any image lock.
    let _lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let image = Image::open_query(&root)?;
    if volume.is_none() || !activation.known_good {
        image.root.offline_path()?;
    }
    let btrfs = volume
        .map(|volume| volume.record(&image.root, &root))
        .transpose()?;
    if btrfs.is_some() {
        ensure!(
            !activation
                .options
                .split_ascii_whitespace()
                .any(|arg| matches!(arg.split('=').next(), Some("rootflags" | "rootfstype"))),
            "Btrfs activation supplies rootflags and rootfstype"
        );
    }
    check_image_integrity(&image)?;
    let state = image.state()?;
    let settings = &state.settings;
    let image_id = canonical_partuuid(&settings.id).context("invalid image identity")?;
    let boot_revision = if btrfs.is_some() {
        settings.boot_revision()
    } else {
        settings.revision
    };
    let id = entry_id(&image_id, boot_revision);
    let fallback = match (activation.known_good, &settings.boot_parent) {
        (true, None) => None,
        (false, Some(parent)) => {
            let parent_id =
                canonical_partuuid(&parent.id).context("invalid base image identity")?;
            let parent = entry_id(&parent_id, parent.revision);
            policy::check_parent(&parent, &id)
                .context("the staged image does not descend from its recorded base")?;
            Some(parent)
        }
        (true, Some(_)) => bail!("a staged environment must be activated as a counted trial"),
        (false, None) => {
            bail!("stage an upgrade first, or use --known-good to bootstrap the base entry")
        }
    };

    let managed = kernel::selected(&state.installed, &state.paths)?;
    let (mut kernel, mut initrd, linux_sha256, initrd_sha256) = if let Some(artifacts) = &managed {
        ensure!(
            activation.kernel == "boot/vmlinuz" && activation.initrd.is_none(),
            "the image uses kernel-install artifacts; omit --kernel and --initrd"
        );
        let (kernel, initrd) = artifacts.open_prepared(&image.state_dir)?;
        (
            kernel,
            initrd,
            artifacts.linux_sha256.clone(),
            artifacts.initrd_sha256.clone(),
        )
    } else {
        let mut kernel = image
            .root
            .file(&activation.kernel)
            .context("opening the environment kernel")?;
        let mut initrd = activation
            .initrd
            .as_deref()
            .map(|path| image.root.file(path))
            .transpose()
            .context("opening the environment initramfs")?;
        let linux_sha256 = digest_artifact(&mut kernel)?;
        let initrd_sha256 = initrd.as_mut().map(digest_artifact).transpose()?;
        (kernel, initrd, linux_sha256, initrd_sha256)
    };
    ensure!(
        !matches!(boot_root, BootRoot::Btrfs(_)) || initrd.is_some(),
        "booting by Btrfs filesystem UUID requires an initramfs"
    );
    ensure!(
        !sign_uki || initrd.is_some(),
        "UKI activation requires an initramfs"
    );
    let receipt = if let Some(deployment) = deployment {
        let partuuid = boot_root.partuuid()?;
        let receipt = read_export_receipt(deployment.receipt)?;
        ensure!(
            receipt.image == image_id && receipt.revision == settings.revision,
            "root export receipt is for a different image or revision"
        );
        ensure!(
            receipt.state_sha256 == digest_reader(image.state_dir.file("state.sqlite")?)?,
            "image state differs from the root export receipt"
        );
        if let Some(expected) = &receipt.tree_sha256 {
            ensure!(
                tree::digest(&image.root).context("fingerprinting the root for activation")?
                    == *expected,
                "root tree differs from the root export receipt"
            );
        }
        if deployment.root.is_none() {
            if let Some(target) = deployment.target {
                target.verify(partuuid, &receipt)?;
            } else {
                deployment
                    .disk
                    .verify_payload(partuuid, receipt.bytes, &receipt.sha256)?;
                deployment
                    .disk
                    .verify_filesystem(partuuid, &image_id, settings.revision)?;
            }
        }
        Some(receipt)
    } else {
        None
    };
    if let Some(volume) = volume {
        volume.check_boot_device(&esp, &_lock, &boot_root)?;
    } else if let Some(target) = deployment.and_then(|deployment| deployment.target) {
        target.check_esp(&esp, &_lock)?;
    } else {
        ensure!(
            !is_fat(&_lock)?,
            "use boot-env activate-device for a mounted VFAT ESP"
        );
    }
    ensure!(
        direct || !uefi::present(&esp)?,
        "this ESP uses direct UEFI boot; use --uefi or configure uefi=#true"
    );
    let Inventory {
        policy,
        entries: existing,
        firmware,
    } = inventory(&esp, &_lock, direct)?;
    if let Some(firmware) = &firmware {
        firmware.check_next(&existing, false)?;
        if activation.known_good {
            let running = Root::open(Path::new("/"))?;
            ensure!(
                same_root(&running, &image.root)?,
                "bootstrap direct UEFI boot from the running root"
            );
        }
    }
    ensure!(
        !destroy::pending(&esp)?.iter().any(|entry| entry.id == id),
        "this boot environment is retiring; finish destroy-btrfs first"
    );
    let existing_entry = existing.iter().find(|entry| entry.id == id);
    if let Some(entry) = existing_entry {
        ensure!(
            sign_uki == entry.uki.is_some(),
            "this environment already has a different boot artifact type"
        );
    }
    let root_arg = boot_root.argument();
    let options = &activation.options;
    let mut cmdline = match &btrfs {
        Some(record) => format!(
            "root={root_arg} rootwait rootfstype=btrfs rootflags=subvolid={} {options}",
            record.subvolid
        ),
        None => format!("root={root_arg} rootwait {options}"),
    };
    let signed_uki = if sign_uki {
        cmdline = cmdline
            .split_ascii_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let registered_digest = existing_entry
            .and_then(|entry| entry.uki.as_ref())
            .map(|uki| uki.sha256.as_str());
        Some(uki::prepare(
            &esp,
            &id,
            &cmdline,
            (&mut kernel, &linux_sha256),
            (
                initrd.as_mut().context("missing UKI initramfs")?,
                initrd_sha256
                    .as_deref()
                    .context("missing initramfs digest")?,
            ),
            registered_digest,
        )?)
    } else {
        None
    };

    // Retries of entries created before artifact hashes keep the original BLS text.
    let record_hashes = if let Some(entry) = existing_entry {
        field(&entry.description, LINUX_HASH)?.is_some()
            || field(&entry.description, INITRD_HASH)?.is_some()
    } else {
        true
    };
    let mut description = Entry {
        id: &id,
        revision: boot_revision,
        parent: if existing_entry.is_some_and(|entry| entry.parent.is_none()) {
            None // Keep activation retries compatible with previously registered entries.
        } else {
            fallback.as_deref()
        },
        cmdline: &cmdline,
        btrfs: btrfs.as_ref(),
        linux_sha256: &linux_sha256,
        initrd_sha256: initrd_sha256.as_deref(),
        record_hashes,
        uki_sha256: signed_uki.as_ref().map(|(_, hash)| hash.as_str()),
        kernel_release: managed
            .as_ref()
            .map(|artifacts| artifacts.kernel.release.as_str()),
    }
    .render();
    if let Some(firmware) = &firmware
        && fallback.is_some()
    {
        description.push_str(&format!(
            "{}{}\n",
            uefi::ARMED,
            firmware.armed_boot(existing_entry)?
        ));
    }
    ensure!(
        description.len() <= policy::MAX_ENTRY_BYTES,
        "boot entry exceeds 64 KiB"
    );
    if let Some(entry) = existing_entry {
        ensure!(
            !activation.known_good || entry.tries.is_none(),
            "the registered boot environment is a trial"
        );
        ensure!(
            entry.tries != Some(0)
                || firmware
                    .as_ref()
                    .map(|fw| fw.can_arm(entry))
                    .transpose()?
                    .unwrap_or(false),
            "boot attempts are exhausted; stage a new environment"
        );
        ensure!(
            entry.description == description,
            "this environment already has a different boot entry"
        );
    }
    let select_trial = fallback.is_some()
        && existing_entry.is_none_or(|entry| entry.tries.is_some())
        && !policy
            .as_ref()
            .and_then(|policy| policy.selected(&existing))
            .is_some_and(|entry| entry.id == id);
    let root_deployment = deployment.and_then(|deployment| deployment.root);
    if root_deployment.is_some() {
        ensure!(
            existing_entry.is_none(),
            "the environment is already registered"
        );
    }
    let replace = root_deployment.and_then(|root| root.replace);
    ensure!(
        replace.is_none() || select_trial,
        "a slot replacement requires a new trial and a known-good base"
    );
    let marker = partuuid.map(|uuid| esp.join(format!("loader/entries/{uuid}.cps-retired")));
    let pending = match &marker {
        Some(path) => read_regular(path)?,
        None => None,
    };
    if let Some(pending) = &pending {
        ensure!(
            deployment.is_some_and(|deployment| deployment.target.is_some())
                && replace.is_some_and(|id| pending.strip_suffix(b"\n") == Some(id.as_bytes())),
            "an interrupted slot replacement must be resumed with activate-device and its original --replace ID"
        );
    }
    let mut retired = None;
    let mut _fallback_lock = None;
    if let Some(fallback) = fallback.as_deref().filter(|_| select_trial) {
        let base = find_known_good(&existing, fallback)
            .context("the base must have a known-good boot entry")?;
        ensure!(
            policy
                .as_ref()
                .and_then(|policy| policy.selected(&existing))
                .is_some_and(|entry| entry.id == fallback),
            "select the known-good base environment {fallback} before activating its upgrade"
        );
        ensure!(
            sign_uki == base.uki.is_some(),
            "the known-good fallback must use the same boot artifact type as its trial"
        );
        verify_fallback_artifacts(&esp, base)?;
        if let Some(volume) = volume {
            let record = base
                .btrfs
                .as_ref()
                .context("the fallback must be a verified Btrfs environment")?;
            volume.check_boot_device(&esp, &_lock, &base.root)?;
            let (image, revision) = policy::entry_identity(&base.id)?;
            let parent = volume
                .verify(record, image, revision)
                .context("checking the Btrfs fallback root")?;
            ensure!(
                parent.settings()?.revision
                    == settings
                        .boot_parent
                        .as_ref()
                        .context("missing boot parent")?
                        .image_revision(),
                "the base changed after staging; prepare a new upgrade"
            );
            _fallback_lock = Some(parent);
        }
        if let Some(replace) = replace {
            ensure!(
                replace != base.id && boot_root != base.root,
                "cannot replace the known-good base"
            );
            let disk = deployment
                .context("slot replacement requires a verified disk")?
                .disk;
            if let Some(entry) = existing.iter().find(|entry| entry.id == replace) {
                retired = Some(check_replacement(disk, entry, boot_root.partuuid()?)?);
            } else {
                ensure!(
                    pending.is_some(),
                    "unknown replacement boot environment {replace}"
                );
                policy::entry_identity(replace)?;
            }
        }
        ensure!(
            !existing.iter().any(|entry| entry.id != id
                && Some(entry.id.as_str()) != replace
                && match (&entry.btrfs, &btrfs) {
                    (Some(existing), Some(candidate)) if existing.fsid == candidate.fsid =>
                        existing.same_subvolume(candidate),
                    _ => entry.root == boot_root,
                }),
            "each environment requires a distinct root PARTUUID or Btrfs subvolume"
        );
    } else if existing_entry.is_none() {
        ensure!(
            existing.is_empty(),
            "--known-good is only used to bootstrap the first environment"
        );
        ensure!(
            policy
                .as_ref()
                .is_none_or(|policy| *policy == LoaderPolicy::Automatic),
            "EFI partition is already configured for another boot environment"
        );
    }

    if let (Some(deployment), Some(fallback), Some(parent)) =
        (deployment, &fallback, &settings.boot_parent)
    {
        let base = find_known_good(&existing, fallback)?;
        deployment
            .disk
            .verify_filesystem(
                base.root.partuuid()?,
                &canonical_partuuid(&parent.id)?,
                parent.revision,
            )
            .context("checking the known-good fallback partition")?;
    }
    if let Some(root) = root_deployment {
        let partuuid = boot_root.partuuid()?;
        let deployment = deployment.context("root deployment requires a verified disk")?;
        let receipt = receipt
            .as_ref()
            .context("root deployment requires an export receipt")?;
        if let Some(target) = deployment.target {
            let payload = target.prepare(root.image, partuuid, receipt)?;
            let marker = marker.context("missing slot retirement path")?;
            if let Some(replace) = replace {
                if matches!(policy, Some(LoaderPolicy::Pinned { .. })) {
                    let base = fallback.as_deref().context("missing known-good base")?;
                    LoaderPolicy::pinned(base).write(&esp.join("loader/loader.conf"))?;
                }
                retire_device_entry(&esp, &marker, replace, retired.take())?;
            }
            target.write(payload, partuuid, receipt)?;
            if replace.is_some() {
                fs::remove_file(&marker)?;
                File::open(esp.join("loader/entries"))?.sync_all()?;
            }
        } else {
            deployment
                .disk
                .write_root(partuuid, root.image, Some(receipt))?;
            deployment
                .disk
                .verify_filesystem(partuuid, &image_id, settings.revision)?;
        }
    }
    if btrfs.is_some() && !sign_uki {
        mark_registered(&image.root, &id)?;
    }
    let efi = ensure_dir(&esp, "EFI")?;
    let entries = if direct {
        ensure_dir(&efi, "cps-uefi")?
    } else {
        ensure_dir(&ensure_dir(&esp, "loader")?, "entries")?
    };
    let config_path = esp.join("loader/loader.conf");
    let artifacts = ensure_dir(&ensure_dir(&efi, "cps")?, &id)?;
    let mut files = Vec::new();
    if let Some((uki, hash)) = signed_uki {
        let path = artifacts.join(uki::NAME);
        if install_checked(&path, uki, &hash, |candidate| {
            // sbctl's verification sandbox can read the mounted ESP.
            if existing_entry.is_none() {
                uki::verify_signature(&esp, candidate)?;
            }
            Ok(())
        })? {
            files.push(format!("EFI/cps/{id}/{}", uki::NAME));
        }
        mark_registered(&image.root, &id)?;
    } else {
        if install(&artifacts.join("vmlinuz.efi"), kernel, &linux_sha256)? {
            files.push(format!("EFI/cps/{id}/vmlinuz.efi"));
        }
        if let Some(initrd) = initrd
            && install(
                &artifacts.join("initramfs.img"),
                initrd,
                initrd_sha256
                    .as_deref()
                    .context("missing initramfs digest")?,
            )?
        {
            files.push(format!("EFI/cps/{id}/initramfs.img"));
        }
    }
    if let Some(entry) = existing_entry {
        File::open(&entry.path)?.sync_all()?;
        File::open(&entries)?.sync_all()?;
    } else {
        if fallback.is_none() && !direct {
            LoaderPolicy::Automatic.write(&config_path)?;
            files.push("loader/loader.conf".into());
        }
        let file = match fallback {
            None => format!("{id}.conf"),
            Some(_) => format!("{id}+{}.conf", if direct { 1 } else { activation.tries }),
        };
        install_text(&entries.join(&file), &description)?;
        let directory = if direct {
            uefi::DIRECTORY
        } else {
            "loader/entries"
        };
        files.push(format!("{directory}/{file}"));
    }
    if let Some(firmware) = firmware {
        if fallback.is_none() || select_trial {
            firmware.activate(&id, fallback.is_some())?;
        }
    } else if let Some(fallback) = fallback
        .as_deref()
        .filter(|_| select_trial && policy != Some(LoaderPolicy::Automatic))
    {
        // The counted entry and its files are durable before selection.
        LoaderPolicy::trial(fallback, &id).write(&config_path)?;
        files.push("loader/loader.conf".into());
    }

    Ok(ActivationUpdate { id, files, retired })
}

fn check_replacement(
    disk: &OfflineDisk,
    entry: &BootEntry,
    partuuid: &str,
) -> Result<RetiredEntry> {
    let id = &entry.id;
    ensure!(
        entry.root.partuuid()? == partuuid,
        "replacement entry {id} belongs to a different root PARTUUID"
    );
    ensure!(
        entry.tries.is_none_or(|left| left == 0),
        "cannot replace boot environment {id} while its trial has attempts left"
    );
    let (image, revision) = policy::entry_identity(id)?;
    disk.verify_filesystem(partuuid, image, revision)
        .context("checking the occupied root partition before replacement")?;
    let file = entry
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid boot entry filename")?;
    Ok(RetiredEntry {
        id: id.clone(),
        file: file.to_owned(),
    })
}

fn retire_device_entry(
    esp: &Path,
    marker: &Path,
    id: &str,
    retired: Option<RetiredEntry>,
) -> Result<()> {
    // Remember the retired identity before removing its boot entry. The
    // directory fsync must complete before any write to the root partition.
    install_text(marker, &format!("{id}\n"))?;
    let entries = File::open(esp.join("loader/entries"))?;
    if let Some(retired) = retired {
        fs::remove_file(esp.join("loader/entries").join(retired.file))?;
    }
    entries.sync_all()?;

    let artifacts = esp.join("EFI/cps").join(id);
    match File::open(&artifacts) {
        Ok(dir) => {
            for name in ["vmlinuz.efi", "initramfs.img"] {
                match fs::remove_file(artifacts.join(name)) {
                    Ok(()) => (),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error.into()),
                }
            }
            dir.sync_all()?;
            if fs::read_dir(&artifacts)?.next().is_none() {
                fs::remove_dir(&artifacts)?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    File::open(esp.join("EFI/cps"))?.sync_all()?;
    Ok(())
}

fn read_export_receipt(path: &Path) -> Result<ExportReceipt> {
    const MAX_RECEIPT_BYTES: u64 = 4096;
    let file = offline_file(path)?;
    let mut text = String::new();
    file.take(MAX_RECEIPT_BYTES + 1).read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= MAX_RECEIPT_BYTES,
        "root export receipt is too large"
    );
    ExportReceipt::parse(&text)
        .with_context(|| format!("reading root export receipt {}", path.display()))
}

struct Entry<'a> {
    id: &'a str,
    revision: u64,
    cmdline: &'a str,
    btrfs: Option<&'a BtrfsRecord>,
    parent: Option<&'a str>,
    linux_sha256: &'a str,
    initrd_sha256: Option<&'a str>,
    record_hashes: bool,
    uki_sha256: Option<&'a str>,
    kernel_release: Option<&'a str>,
}

impl Entry<'_> {
    fn render(&self) -> String {
        let Self {
            id,
            revision,
            cmdline,
            btrfs,
            parent,
            linux_sha256,
            initrd_sha256,
            record_hashes,
            uki_sha256,
            kernel_release,
        } = self;
        let title = match kernel_release {
            Some(release) => format!("Canopa ({id}, Linux {release})"),
            None => format!("Canopa ({id})"),
        };
        let mut entry = format!("title {title}\nsort-key cps\nversion {revision}\n");
        if uki_sha256.is_some() {
            entry.push_str(&format!("uki /EFI/cps/{id}/{}\n", uki::NAME));
        } else {
            entry.push_str(&format!("linux /EFI/cps/{id}/vmlinuz.efi\n"));
        }
        if let Some(parent) = parent {
            entry.push_str(&format!("# cps-parent {parent}\n"));
        }
        if let Some(sha256) = uki_sha256 {
            entry.push_str(&format!("{}{sha256}\n", uki::HASH));
        } else {
            if *record_hashes {
                entry.push_str(&format!("{LINUX_HASH}{linux_sha256}\n"));
            }
            if let Some(initrd_sha256) = initrd_sha256 {
                entry.push_str(&format!("initrd /EFI/cps/{id}/initramfs.img\n"));
                if *record_hashes {
                    entry.push_str(&format!("{INITRD_HASH}{initrd_sha256}\n"));
                }
            }
        }
        if let Some(btrfs) = btrfs {
            entry.push_str(&btrfs.render());
        }
        if uki_sha256.is_some() {
            entry.push_str(&format!("{}{cmdline}\n", uki::CMDLINE));
        } else {
            entry.push_str(&format!("options {cmdline}\n"));
        }
        entry
    }
}

fn verify_fallback_artifacts(esp: &Path, boot: &BootEntry) -> Result<()> {
    let esp = Root::open(esp)?;
    let id = &boot.id;
    if let Some(uki) = &boot.uki {
        let path = format!("EFI/cps/{id}/{}", uki::NAME);
        let mut file = esp
            .file(&path)
            .context("opening the known-good fallback UKI")?;
        ensure!(
            digest_reader(&mut file)? == uki.sha256,
            "boot artifact digest mismatch at {path}"
        );
        return uki::verify_contents(&mut file, &uki.cmdline, None);
    }
    let entry = &boot.description;
    let kernel = format!("EFI/cps/{id}/vmlinuz.efi");
    let mut linux = None;
    let mut initrd = None;
    // systemd-boot recognizes tabs as separators and skips leading whitespace.
    for (key, value) in entry
        .lines()
        .filter_map(|line| line.trim_ascii_start().split_once([' ', '\t']))
    {
        match key {
            "linux" => ensure!(
                linux.replace(value.trim_ascii()).is_none(),
                "duplicate fallback kernel path"
            ),
            "initrd" => ensure!(
                initrd.replace(value.trim_ascii()).is_none(),
                "duplicate fallback initramfs path"
            ),
            "efi" | "uki" | "uki-url" => bail!("unexpected fallback loader type {key}"),
            _ => {}
        }
    }
    ensure!(
        linux.and_then(|path| path.strip_prefix('/')) == Some(kernel.as_str()),
        "unexpected fallback kernel path"
    );
    let linux_sha256 = field(entry, LINUX_HASH)?;
    let initrd_sha256 = field(entry, INITRD_HASH)?;
    if linux_sha256.is_some() || initrd_sha256.is_some() {
        ensure!(
            linux_sha256.is_some() && initrd_sha256.is_some() == initrd.is_some(),
            "incomplete boot artifact digests in entry {id}"
        );
    }
    verify_artifact(&esp, &kernel, linux_sha256)
        .context("checking the known-good fallback kernel")?;
    if let Some(path) = initrd {
        let expected = format!("EFI/cps/{id}/initramfs.img");
        ensure!(
            path.strip_prefix('/') == Some(expected.as_str()),
            "unexpected fallback initramfs path"
        );
        verify_artifact(&esp, &expected, initrd_sha256)
            .context("checking the known-good fallback initramfs")?;
    }
    Ok(())
}

fn verify_artifact(esp: &Root, path: &str, sha256: Option<&str>) -> Result<()> {
    let file = esp.file(path)?;
    if let Some(sha256) = sha256 {
        validate_digest(sha256)?;
        ensure!(
            digest_reader(file)? == sha256,
            "boot artifact digest mismatch at {path}"
        );
    }
    Ok(())
}

fn digest_artifact(file: &mut File) -> Result<String> {
    let hash = digest_reader(&mut *file)?;
    file.rewind()?;
    Ok(hash)
}

pub(super) fn canonical_partuuid(value: &str) -> Result<String> {
    ensure!(is_uuid(value), "expected a GPT PARTUUID in UUID form");
    Ok(value.to_ascii_lowercase())
}

fn validate_options(options: &str) -> Result<()> {
    ensure!(
        options.bytes().all(|c| c == b' ' || c.is_ascii_graphic()),
        "invalid boot options"
    );
    ensure!(
        !options
            .split_ascii_whitespace()
            .any(|arg| arg.contains('"') || arg.split('=').next() == Some("root")),
        "boot options must be unquoted tokens and CPS supplies root="
    );
    Ok(())
}

fn ensure_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(name);
    match fs::create_dir(&path) {
        Ok(()) => File::open(parent)?.sync_all()?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(&path)?.is_dir(),
                "{} is not a directory",
                path.display()
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn read_regular(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "{} is not a regular file",
                path.display()
            );
            Ok(Some(fs::read(path)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn install_text(path: &Path, text: &str) -> Result<()> {
    install(path, text.as_bytes(), &digest(text.as_bytes()))?;
    Ok(())
}

fn install(path: &Path, bytes: impl Read, sha256: &str) -> Result<bool> {
    install_checked(path, bytes, sha256, |_| Ok(()))
}

fn install_checked(
    path: &Path,
    mut bytes: impl Read,
    sha256: &str,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Result<bool> {
    let parent = path.parent().context("missing EFI destination directory")?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "{} is not a regular file",
                path.display()
            );
            let current = File::open(path)?;
            ensure!(
                digest_reader(&current)? == sha256,
                "boot file {} already exists with different contents",
                path.display()
            );
            verify(path)?;
            current.sync_all()?;
            File::open(parent)?.sync_all()?;
            return Ok(false);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut temp = NamedTempFile::new_in(parent)?;
    io::copy(&mut bytes, &mut temp)?;
    temp.as_file_mut().rewind()?;
    ensure!(
        digest_reader(temp.as_file())? == sha256,
        "boot file {} changed while being copied",
        path.display()
    );
    verify(temp.path())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)?;
    File::open(parent)?.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{digest, install_checked};

    #[test]
    fn rejected_boot_artifact_stays_unpublished() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("uki.efi");
        let bytes = b"fixture UKI";
        let hash = digest(bytes);
        let result = install_checked(&path, bytes.as_slice(), &hash, |candidate| {
            assert_eq!(std::fs::read(candidate)?, bytes);
            anyhow::bail!("signature rejected")
        });
        assert_eq!(result.unwrap_err().to_string(), "signature rejected");
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
        Ok(())
    }
}
