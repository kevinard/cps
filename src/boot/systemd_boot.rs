mod destroy;
pub(super) mod loader;
mod manage;
pub(super) mod policy;
mod prune;
mod uefi;
pub(crate) use destroy::destroy_btrfs_boot_environment;
pub(crate) use manage::validate_name as validate_environment_name;
pub(crate) use manage::{
    mount_boot_environment, rename_boot_environment, unmount_boot_environment,
};
pub(crate) use prune::{
    BootPruneReport, BootRemoval, TrialDiscard, discard_btrfs_trial, prune_btrfs_boot_environments,
};

use std::{
    fs::{self, File},
    io::{self, Read, Seek},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FlockOperation, flock};

use super::{
    btrfs_boot::{BtrfsRecord, BtrfsVolume, mark_registered},
    check_image_integrity,
    device::is_fat,
    entry_id, kernel, uki,
};
use crate::{
    image::{
        Image, State,
        diff::{ImageDiff, diff_open_images},
        root::Root,
    },
    model::{digest, digest_reader, is_uuid, validate_digest},
};

use self::policy::{BootEntry, BootRoot, InvalidEntry, LoaderPolicy, field};

const LINUX_HASH: &str = "# cps-linux-sha256 ";
const INITRD_HASH: &str = "# cps-initrd-sha256 ";
const NO_SELECTION: &str = "loader/loader.conf selects no CPS boot environment; select a \
    known-good environment with `cps boot rollback --esp ESP ID`";
/// Free space kept on the ESP beyond the boot files of an activation.
const ESP_MARGIN: u64 = 1024 * 1024;

#[derive(Debug, serde::Serialize)]
pub(crate) struct BootEnvironment {
    pub id: String,
    pub name: Option<String>,
    pub title: String,
    /// Empty when the entry boots by Btrfs filesystem UUID.
    pub root_partuuid: String,
    pub root_uuid: Option<String>,
    pub tries_left: Option<u32>,
    pub is_default: bool,
    pub is_retiring: bool,
    /// Why the entry cannot be read. CPS never selects an invalid entry.
    pub invalid: Option<String>,
}

struct Inventory {
    policy: Option<LoaderPolicy>,
    entries: Vec<BootEntry>,
    /// CPS entry files that cannot be read. No command selects them.
    invalid: Vec<InvalidEntry>,
    firmware: Option<uefi::Firmware>,
}

/// Refuse a change that must know every boot entry, such as removing a root
/// that an unreadable entry might still boot.
fn require_readable(invalid: &[InvalidEntry]) -> Result<()> {
    if let Some(entry) = invalid.first() {
        bail!(
            "{}; repair or remove it before removing boot environments",
            entry.describe()
        );
    }
    Ok(())
}

/// Explain why `selector` names no usable environment when it is the ID of
/// an unreadable entry.
fn require_valid(invalid: &[InvalidEntry], selector: &str) -> Result<()> {
    if let Some(entry) = invalid.iter().find(|entry| entry.id == selector) {
        bail!(entry.describe());
    }
    Ok(())
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
        let (mut entries, invalid) = if uefi::present(esp)? {
            policy::read_entries(&esp.join(uefi::DIRECTORY))?
        } else {
            Default::default()
        };
        let policy = firmware.inventory(&mut entries)?;
        return Ok(Inventory {
            policy,
            entries,
            invalid,
            firmware: Some(firmware),
        });
    }
    let (policy, (entries, invalid)) = policy::inventory(esp)?;
    // systemd-boot gives these EFI variables precedence over loader.conf.
    if let Some(overrides) = uefi::loader_overrides(esp, lock)? {
        overrides.check(policy.as_ref())?;
    }
    Ok(Inventory {
        policy,
        entries,
        invalid,
        firmware: None,
    })
}

pub(crate) fn list_boot_environments(esp: &Path) -> Result<Vec<BootEnvironment>> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let _lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let Inventory {
        policy,
        entries,
        invalid,
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
    let unreadable = invalid.into_iter().map(|entry| BootEnvironment {
        id: entry.id,
        name: None,
        title: entry.path.display().to_string(),
        root_partuuid: String::new(),
        root_uuid: None,
        tries_left: None,
        is_default: false,
        is_retiring: false,
        invalid: Some(entry.error),
    });
    Ok(all
        .into_iter()
        .map(|(entry, is_retiring)| BootEnvironment {
            is_default: selected.as_deref() == Some(&entry.id),
            id: entry.id,
            name: entry.name,
            title: entry.title,
            root_partuuid: entry.root.partuuid().unwrap_or_default().to_owned(),
            root_uuid: match entry.root {
                BootRoot::Btrfs(uuid) => Some(uuid),
                BootRoot::Partition(_) => None,
            },
            tries_left: entry.tries,
            is_retiring,
            invalid: None,
        })
        .chain(unreadable)
        .collect())
}

pub(super) fn rollback(esp: &Path, id: Option<&str>, volume: &BtrfsVolume) -> Result<String> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        policy,
        entries,
        invalid,
        firmware,
    } = inventory(&esp, &lock, false)?;
    if let Some(id) = id {
        require_valid(&invalid, id)?;
    }
    ensure!(!entries.is_empty(), "no CPS boot environments registered");
    // An explicit ID can restore a selection after the `default` line was removed.
    let current = match &policy {
        Some(policy) => rollback_source(policy, &entries, firmware.as_ref())?,
        None => {
            ensure!(id.is_some(), NO_SELECTION);
            None
        }
    };
    let target = rollback_target(current, &entries, id)?;
    verify_fallback_artifacts(&esp, target)?;
    volume.check_boot_device(&esp, &lock, &target.root)?;
    let (image, revision) = policy::entry_identity(&target.id)?;
    let _root_lock = volume
        .verify(&target.btrfs, image, revision)
        .context("checking the Btrfs rollback root")?;
    // A backup stays read-only until it is selected to boot.
    super::btrfs::make_writable(&_root_lock.root)?;
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
        if entry.root != root && !volume.has_fsid(&entry.btrfs.fsid) {
            continue;
        }
        let record = &entry.btrfs;
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
        invalid,
        firmware,
    } = inventory(&esp, &lock, false)?;
    if let Some(id) = id {
        require_valid(&invalid, id)?;
    }
    let policy = policy.context(NO_SELECTION)?;
    let current = rollback_source(&policy, &entries, firmware.as_ref())?
        .context("no configured default CPS boot environment")?;
    let target = rollback_target(Some(current), &entries, id)?;
    let open = |entry: &BootEntry| -> Result<Image> {
        volume.check_boot_device(&esp, &lock, &entry.root)?;
        let (image, revision) = policy::entry_identity(&entry.id)?;
        volume.open_image(&entry.btrfs, image, revision)
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
    let parent = current
        .parent
        .as_deref()
        .context("no recorded parent boot environment")?;
    find_known_good(entries, parent).context("the recorded parent is unavailable")
}

fn find_known_good<'a>(entries: &'a [BootEntry], id: &str) -> Result<&'a BootEntry> {
    let entry = manage::find(entries, id)?;
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
    if operation == FlockOperation::LockExclusive {
        reconcile(esp)?;
    }
    Ok(file)
}

/// Clean up after an interrupted CPS run before changing the ESP.
///
/// Every CPS writer holds the exclusive ESP lock, so a temporary file that
/// remains once this process holds it belongs to no running operation and
/// is removed. The states that need a decision are not changed here: an
/// entry marked `.cps-destroy` waits for `cps boot destroy`, which every
/// other operation refuses to touch, and a half-blessed direct UEFI trial
/// waits for `cps boot bless`. See the ESP state specification in
/// docs/reference/formats.md.
fn reconcile(esp: &Path) -> Result<()> {
    let mut directories = vec![
        esp.join("loader"),
        esp.join("loader/entries"),
        esp.join("EFI/cps-uefi"),
    ];
    match fs::read_dir(esp.join("EFI/cps")) {
        Ok(artifacts) => {
            for entry in artifacts {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    directories.push(entry.path());
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("listing EFI/cps"),
    }
    for directory in directories {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("listing {}", directory.display()));
            }
        };
        let mut removed = false;
        for entry in entries {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(policy::TEMP_PREFIX))
                && entry.file_type()?.is_file()
            {
                fs::remove_file(entry.path())?;
                removed = true;
            }
        }
        if removed {
            File::open(&directory)?.sync_all()?;
        }
    }
    Ok(())
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
    let verify = |entry: &BootEntry| -> Result<crate::image::Image> {
        verify_fallback_artifacts(&esp, entry)?;
        volume.check_boot_device(&esp, &lock, &entry.root)?;
        let (id, revision) = policy::entry_identity(&entry.id)?;
        let image = volume.open_image(&entry.btrfs, id, revision)?;
        // Matching fingerprints prove unchanged files without re-hashing
        // both whole roots on every boot.
        crate::verify::require_quick(&image)?;
        Ok(image)
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
        invalid,
        firmware,
    } = inventory(&esp, &lock, false)?;
    if let Some(firmware) = &firmware {
        firmware.require_no_next().context(
            "boot the scheduled trial, or cancel it with `cps boot discard-trial`, before \
             making more package changes",
        )?;
    }
    let policy = policy.context(NO_SELECTION)?;
    require_valid(&invalid, &policy.default)?;
    let selected = policy
        .selected(&entries)
        .context("no known-good CPS boot environment is selected")?;
    ensure!(
        selected.tries.is_none(),
        "a trial is pending ({}); boot it, or cancel it with `cps boot discard-trial`, \
         before making more package changes",
        selected.id
    );
    if let (BootRoot::Partition(expected), BootRoot::Partition(selected)) = (&root, &selected.root)
    {
        ensure!(
            selected == expected,
            "the selected root uses a different partition"
        );
    }
    if selected.root != root {
        volume.check_boot_device(&esp, &lock, &selected.root)?;
    }
    let registered = volume.open_registered(&selected.btrfs, &selected.id)?;
    let running = Root::open(Path::new("/"))?;
    ensure!(
        same_root(&registered, &running)?,
        "a different system is selected for the next boot; reboot before changing packages"
    );
    let (id, revision) = policy::entry_identity(&selected.id)?;
    action(running, id, revision)
}

/// A trial selected for the next boot whose fallback is the running root.
pub(crate) struct PendingTrial {
    pub id: String,
    pub parent: String,
    pub root: Root,
}

/// The trial that the next boot starts in place of the running root, if any.
pub(super) fn pending_trial(
    volume: &BtrfsVolume,
    esp: &Path,
    partuuid: Option<&str>,
) -> Result<Option<PendingTrial>> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let root = volume.boot_root(partuuid)?;
    volume.check_boot_device(&esp, &lock, &root)?;
    let Some((running, _)) = volume.running()? else {
        return Ok(None);
    };
    let Inventory {
        policy, entries, ..
    } = inventory(&esp, &lock, false)?;
    let Some(trial) = policy
        .as_ref()
        .and_then(|policy| policy.selected(&entries))
        .filter(|entry| entry.tries.is_some_and(|tries| tries > 0))
    else {
        return Ok(None);
    };
    if trial.parent.as_deref() != Some(running.as_str()) {
        return Ok(None);
    }
    if trial.root != root {
        volume.check_boot_device(&esp, &lock, &trial.root)?;
    }
    let trial_root = volume.open_registered(&trial.btrfs, &trial.id)?;
    ensure!(
        !same_root(&trial_root, &Root::open(Path::new("/"))?)?,
        "the pending trial is the running root"
    );
    Ok(Some(PendingTrial {
        id: trial.id.clone(),
        parent: running,
        root: trial_root,
    }))
}

pub(crate) struct Activation {
    /// Boot the root by this GPT partition instead of the Btrfs filesystem UUID.
    pub root_partuuid: Option<String>,
    pub kernel: String,
    pub initrd: Option<String>,
    pub options: String,
    pub tries: u32,
    pub known_good: bool,
}

impl Activation {
    /// Check the settings that live activation and offline images share.
    pub(super) fn check(&self) -> Result<()> {
        validate_options(&self.options)?;
        ensure!(
            !self
                .options
                .split_ascii_whitespace()
                .any(|arg| matches!(arg.split('=').next(), Some("rootflags" | "rootfstype"))),
            "Btrfs activation supplies rootflags and rootfstype"
        );
        ensure!(
            (1..=99).contains(&self.tries),
            "tries must be between 1 and 99"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BootMode {
    Split,
    Uki,
    Uefi,
}

/// The kernel and initramfs that boot one environment, with their digests.
pub(super) struct BootFiles {
    pub kernel: File,
    pub initrd: Option<File>,
    pub linux_sha256: String,
    pub initrd_sha256: Option<String>,
    pub kernel_release: Option<String>,
}

impl BootFiles {
    pub(super) fn open(image: &Image, state: &State, activation: &Activation) -> Result<Self> {
        if let Some(artifacts) = kernel::selected(&state.installed, &state.paths)? {
            ensure!(
                activation.kernel == "boot/vmlinuz" && activation.initrd.is_none(),
                "the image uses kernel-install artifacts; omit --kernel and --initrd"
            );
            let (kernel, initrd) = artifacts.open_prepared(&image.state_dir)?;
            return Ok(Self {
                kernel,
                initrd,
                linux_sha256: artifacts.linux_sha256.clone(),
                initrd_sha256: artifacts.prepared_initrd_sha256(&image.state_dir)?,
                kernel_release: Some(artifacts.kernel.release.clone()),
            });
        }
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
        Ok(Self {
            linux_sha256: digest_artifact(&mut kernel)?,
            initrd_sha256: initrd.as_mut().map(digest_artifact).transpose()?,
            kernel,
            initrd,
            kernel_release: None,
        })
    }
}

/// The kernel command line that selects a Btrfs root subvolume.
fn command_line(root: &BootRoot, record: &BtrfsRecord, options: &str) -> String {
    format!(
        "root={} rootwait rootfstype=btrfs rootflags=subvolid={} {options}",
        root.argument(),
        record.subvolid
    )
}

pub(super) fn activate(
    root: &Path,
    esp: &Path,
    activation: &Activation,
    volume: &BtrfsVolume,
    mode: BootMode,
) -> Result<String> {
    activate_as(root, esp, activation, volume, mode, false, None)
}

/// Register a read-only backup of the running Btrfs root as a known-good
/// environment without selecting it.
pub(super) fn register_backup(
    root: &Path,
    esp: &Path,
    activation: &Activation,
    volume: &BtrfsVolume,
    mode: BootMode,
    name: Option<&str>,
) -> Result<String> {
    activate_as(root, esp, activation, volume, mode, true, name)
}

fn activate_as(
    root: &Path,
    esp: &Path,
    activation: &Activation,
    volume: &BtrfsVolume,
    mode: BootMode,
    backup: bool,
    name: Option<&str>,
) -> Result<String> {
    ensure!(
        backup || name.is_none(),
        "only a known-good backup is named when it is registered"
    );
    ensure!(
        !backup || activation.known_good,
        "a backup is a known-good Btrfs environment"
    );
    let sign_uki = mode != BootMode::Split;
    let direct = mode == BootMode::Uefi;
    let boot_root = volume.boot_root(activation.root_partuuid.as_deref())?;
    activation.check()?;
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
    if !activation.known_good {
        image.root.offline_path()?;
    }
    let btrfs = volume.record(&image.root, &root)?;
    if backup {
        // A backup is a snapshot of the running root, which was just checked.
        crate::verify::require_quick(&image)?;
    } else {
        check_image_integrity(&image)?;
    }
    let state = image.state()?;
    let settings = &state.settings;
    let image_id = canonical_partuuid(&settings.id).context("invalid image identity")?;
    let boot_revision = settings.boot_revision();
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

    let BootFiles {
        mut kernel,
        mut initrd,
        linux_sha256,
        initrd_sha256,
        kernel_release,
    } = BootFiles::open(&image, &state, activation)?;
    ensure!(
        !matches!(boot_root, BootRoot::Btrfs(_)) || initrd.is_some(),
        "booting by Btrfs filesystem UUID requires an initramfs"
    );
    ensure!(
        !sign_uki || initrd.is_some(),
        "UKI activation requires an initramfs"
    );
    volume.check_boot_device(&esp, &_lock, &boot_root)?;
    ensure!(
        direct || !uefi::present(&esp)?,
        "this ESP uses direct UEFI boot; use --uefi or configure uefi=#true"
    );
    let Inventory {
        policy,
        entries: existing,
        invalid,
        firmware,
    } = inventory(&esp, &_lock, direct)?;
    // Writing this environment's entry would leave two files that claim it.
    require_valid(&invalid, &id)?;
    if let Some(firmware) = &firmware {
        firmware.check_next(&existing, false)?;
        if activation.known_good && !backup {
            let running = Root::open(Path::new("/"))?;
            ensure!(
                same_root(&running, &image.root)?,
                "bootstrap direct UEFI boot from the running root"
            );
        }
    }
    let retiring = destroy::pending(&esp)?;
    ensure!(
        !retiring.iter().any(|entry| entry.id == id),
        "this boot environment is retiring; finish it with `cps boot destroy`"
    );
    let existing_entry = existing.iter().find(|entry| entry.id == id);
    if let Some(name) = name {
        manage::require_unused_name(existing.iter().chain(&retiring), name, &id)?;
    }
    if let Some(entry) = existing_entry {
        ensure!(
            sign_uki == entry.uki.is_some(),
            "this environment already has a different boot artifact type"
        );
    }
    let mut cmdline = command_line(&boot_root, &btrfs, &activation.options);
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

    let mut description = Entry {
        id: &id,
        revision: boot_revision,
        parent: fallback.as_deref(),
        cmdline: &cmdline,
        btrfs: &btrfs,
        linux_sha256: &linux_sha256,
        initrd_sha256: initrd_sha256.as_deref(),
        uki_sha256: signed_uki.as_ref().map(|(_, hash)| hash.as_str()),
        kernel_release: kernel_release.as_deref(),
        backup,
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
    if let Some(name) = name.or_else(|| existing_entry.and_then(|entry| entry.name.as_deref())) {
        description = manage::named_description(&description, name);
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
            "boot attempts are exhausted; remove the failed trial with \
             `cps boot discard-trial`, then stage again"
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
    // CPS names every systemd-boot selection in `default` and `preferred`,
    // and only systemd-boot 262 or newer reads `preferred`.
    if firmware.is_none() {
        loader::require_preferred(&esp)?;
    }
    let prefer_trial = firmware.is_none() && select_trial;
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
        volume.check_boot_device(&esp, &_lock, &base.root)?;
        let (image, revision) = policy::entry_identity(&base.id)?;
        let parent = volume
            .verify(&base.btrfs, image, revision)
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
        ensure!(
            !existing.iter().any(|entry| entry.id != id
                && if entry.btrfs.fsid == btrfs.fsid {
                    entry.btrfs.same_subvolume(&btrfs)
                } else {
                    entry.root == boot_root
                }),
            "each environment requires a distinct Btrfs subvolume"
        );
    } else if backup && existing_entry.is_none() {
        check_backup(policy.as_ref(), &existing, &btrfs)?;
    } else if existing_entry.is_none() {
        ensure!(
            existing.is_empty(),
            "--known-good is only used to bootstrap the first environment"
        );
        ensure!(
            policy.is_none(),
            "EFI partition is already configured for another boot environment"
        );
    }

    {
        // Check before the first write. Existing files are reused on retries.
        let artifacts = esp.join("EFI/cps").join(&id);
        let mut files = Vec::new();
        if let Some((uki, _)) = &signed_uki {
            files.push((artifacts.join(uki::NAME), uki.metadata()?.len()));
        } else {
            files.push((artifacts.join("vmlinuz.efi"), kernel.metadata()?.len()));
            if let Some(initrd) = &initrd {
                files.push((artifacts.join("initramfs.img"), initrd.metadata()?.len()));
            }
        }
        if existing_entry.is_none() {
            files.push((
                esp.join("loader/entries/new.conf"),
                description.len() as u64,
            ));
        }
        // loader.conf is replaced through a temporary copy.
        files.push((esp.join("loader/loader.conf.new"), 64 * 1024));
        check_esp_space(&esp, &id, &files)?;
    }
    if !sign_uki {
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
    if let Some((uki, hash)) = signed_uki {
        install_checked(&artifacts.join(uki::NAME), uki, &hash, |candidate| {
            // sbctl's verification sandbox can read the mounted ESP.
            if existing_entry.is_none() {
                uki::verify_signature(&esp, candidate)?;
            }
            Ok(())
        })?;
        mark_registered(&image.root, &id)?;
    } else {
        install_split(
            &artifacts,
            kernel,
            &linux_sha256,
            initrd,
            initrd_sha256.as_deref(),
        )?;
    }
    if backup && let Some(firmware) = &firmware {
        firmware.register(&id)?;
    }
    if let Some(entry) = existing_entry {
        File::open(&entry.path)?.sync_all()?;
        File::open(&entries)?.sync_all()?;
    } else {
        let file = match fallback {
            None => format!("{id}.conf"),
            Some(_) => format!("{id}+{}.conf", if direct { 1 } else { activation.tries }),
        };
        install_text(&entries.join(&file), &description)?;
        // The first environment becomes the default once its entry is durable.
        if fallback.is_none() && !direct && !backup {
            LoaderPolicy::pinned(&id).write(&config_path)?;
        }
    }
    if let Some(firmware) = firmware {
        if !backup && (fallback.is_none() || select_trial) {
            firmware.activate(&id, fallback.is_some())?;
        }
    } else if let Some(fallback) = fallback.as_deref().filter(|_| prefer_trial) {
        // The counted entry and its files are durable before selection.
        LoaderPolicy::trial(fallback, &id).write(&config_path)?;
    }

    Ok(id)
}

/// Check that a new backup can join `existing` without changing the next
/// boot. The selection names its entries, so a new entry never changes it.
fn check_backup(
    policy: Option<&LoaderPolicy>,
    existing: &[BootEntry],
    record: &BtrfsRecord,
) -> Result<()> {
    let policy = policy.context(NO_SELECTION)?;
    let selected = policy.selected(existing).context(NO_SELECTION)?;
    ensure!(
        selected.tries.is_none(),
        "a trial is selected for the next boot; boot it or cancel it before backing up"
    );
    ensure!(
        !existing
            .iter()
            .any(|entry| entry.btrfs.fsid == record.fsid && entry.btrfs.same_subvolume(record)),
        "each environment requires a distinct Btrfs subvolume"
    );
    Ok(())
}

/// Register the first, known-good environment of an offline disk on a
/// detached copy of its ESP, and return the ESP paths written.
pub(super) fn bootstrap_offline(
    esp: &Path,
    id: &str,
    revision: u64,
    record: &BtrfsRecord,
    root: &BootRoot,
    activation: &Activation,
    files: BootFiles,
) -> Result<Vec<String>> {
    let (policy, (entries, invalid)) = policy::inventory(esp)?;
    ensure!(
        entries.is_empty()
            && invalid.is_empty()
            && destroy::pending(esp)?.is_empty()
            && !uefi::present(esp)?,
        "the EFI partition already has CPS boot environments"
    );
    ensure!(
        policy.is_none(),
        "the EFI partition's loader.conf already selects another boot entry"
    );
    let cmdline = command_line(root, record, &activation.options);
    let description = Entry {
        id,
        revision,
        parent: None,
        cmdline: &cmdline,
        btrfs: record,
        linux_sha256: &files.linux_sha256,
        initrd_sha256: files.initrd_sha256.as_deref(),
        uki_sha256: None,
        kernel_release: files.kernel_release.as_deref(),
        backup: false,
    }
    .render();
    let efi = ensure_dir(esp, "EFI")?;
    let artifacts = ensure_dir(&ensure_dir(&efi, "cps")?, id)?;
    let mut written = vec![format!("EFI/cps/{id}/vmlinuz.efi")];
    if files.initrd.is_some() {
        written.push(format!("EFI/cps/{id}/initramfs.img"));
    }
    install_split(
        &artifacts,
        files.kernel,
        &files.linux_sha256,
        files.initrd,
        files.initrd_sha256.as_deref(),
    )?;
    let entries = ensure_dir(&ensure_dir(esp, "loader")?, "entries")?;
    install_text(&entries.join(format!("{id}.conf")), &description)?;
    LoaderPolicy::pinned(id).write(&esp.join("loader/loader.conf"))?;
    written.push(format!("loader/entries/{id}.conf"));
    written.push("loader/loader.conf".into());
    Ok(written)
}

fn install_split(
    artifacts: &Path,
    kernel: File,
    linux_sha256: &str,
    initrd: Option<File>,
    initrd_sha256: Option<&str>,
) -> Result<()> {
    install(&artifacts.join("vmlinuz.efi"), kernel, linux_sha256)?;
    if let Some(initrd) = initrd {
        install(
            &artifacts.join("initramfs.img"),
            initrd,
            initrd_sha256.context("missing initramfs digest")?,
        )?;
    }
    Ok(())
}

/// Fail before writing boot files that would not fit on the ESP.
fn check_esp_space(esp: &Path, id: &str, files: &[(PathBuf, u64)]) -> Result<()> {
    let stat = rustix::fs::statvfs(esp).context("reading EFI system partition free space")?;
    let cluster = stat.f_frsize.max(1);
    let mut needed = ESP_MARGIN;
    for (path, bytes) in files {
        if !path.try_exists()? {
            // Each new file and directory occupies whole clusters.
            needed = needed.saturating_add(bytes.div_ceil(cluster).saturating_add(1) * cluster);
        }
    }
    needed = needed.saturating_add(2 * cluster);
    let available = stat.f_bavail.saturating_mul(stat.f_frsize);
    ensure!(
        available >= needed,
        "insufficient free space on the EFI system partition for {id}: {} available, {} \
         needed for its boot files and a {} margin. Remove old boot environments with \
         `cps boot prune`, or free space on the ESP, then retry",
        super::mib(available),
        super::mib(needed),
        super::mib(ESP_MARGIN),
    );
    Ok(())
}

struct Entry<'a> {
    id: &'a str,
    revision: u64,
    cmdline: &'a str,
    btrfs: &'a BtrfsRecord,
    parent: Option<&'a str>,
    linux_sha256: &'a str,
    initrd_sha256: Option<&'a str>,
    uki_sha256: Option<&'a str>,
    kernel_release: Option<&'a str>,
    backup: bool,
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
            uki_sha256,
            kernel_release,
            backup,
        } = self;
        let name = if *backup { "Canopa backup" } else { "Canopa" };
        let title = match kernel_release {
            Some(release) => format!("{name} ({id}, Linux {release})"),
            None => format!("{name} ({id})"),
        };
        let mut entry = format!(
            "title {title}\nsort-key cps\nversion {revision}\n{}{}\n",
            policy::ENTRY_FORMAT_FIELD,
            policy::ENTRY_FORMAT,
        );
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
            entry.push_str(&format!("{LINUX_HASH}{linux_sha256}\n"));
            if let Some(initrd_sha256) = initrd_sha256 {
                entry.push_str(&format!("initrd /EFI/cps/{id}/initramfs.img\n"));
                entry.push_str(&format!("{INITRD_HASH}{initrd_sha256}\n"));
            }
        }
        entry.push_str(&btrfs.render());
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
    let linux_sha256 = field(entry, LINUX_HASH)?.context("missing boot kernel digest")?;
    let initrd_sha256 = field(entry, INITRD_HASH)?;
    ensure!(
        initrd_sha256.is_some() == initrd.is_some(),
        "incomplete boot artifact digests in entry {id}"
    );
    verify_artifact(&esp, &kernel, linux_sha256)
        .context("checking the known-good fallback kernel")?;
    if let (Some(path), Some(initrd_sha256)) = (initrd, initrd_sha256) {
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

fn verify_artifact(esp: &Root, path: &str, sha256: &str) -> Result<()> {
    validate_digest(sha256)?;
    ensure!(
        digest_reader(esp.file(path)?)? == sha256,
        "boot artifact digest mismatch at {path}"
    );
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
    let mut temp = policy::esp_temp(parent)?;
    io::copy(&mut bytes, &mut temp)?;
    temp.as_file_mut().rewind()?;
    ensure!(
        digest_reader(temp.as_file())? == sha256,
        "boot file {} changed while being copied",
        path.display()
    );
    verify(temp.path())?;
    temp.as_file().sync_all()?;
    // VFAT writes a renamed file's size and first cluster with its inode,
    // not with the directory, so sync the file again after the rename.
    temp.persist_noclobber(path)?.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{
        BootEntry, BtrfsRecord, Entry, LoaderPolicy, check_backup, check_esp_space, digest,
        install_checked, policy::BootRoot,
    };

    fn record(subvolid: u64) -> BtrfsRecord {
        BtrfsRecord {
            fsid: "11111111-1111-4111-8111-111111111111".into(),
            uuid: format!("22222222-2222-4222-8222-{subvolid:012}"),
            subvolid,
            path: format!("@environments/{subvolid}/root"),
        }
    }

    fn entry(id: &str, revision: u64, tries: Option<u32>, subvolid: u64) -> BootEntry {
        BootEntry {
            path: format!("{id}.conf").into(),
            id: id.into(),
            name: None,
            revision,
            tries,
            description: String::new(),
            title: String::new(),
            root: BootRoot::Btrfs("11111111-1111-4111-8111-111111111111".into()),
            parent: None,
            btrfs: record(subvolid),
            uki: None,
        }
    }

    #[test]
    fn backups_never_change_the_next_boot() -> anyhow::Result<()> {
        let running = entry("cps-a47593c2-e985-42d2-92b6-44c7057e1e96-4", 4, None, 256);
        let existing = [running];
        let pinned = LoaderPolicy::pinned(&existing[0].id);
        check_backup(Some(&pinned), &existing, &record(300))?;
        // The backup needs its own subvolume.
        assert!(check_backup(Some(&pinned), &existing, &record(256)).is_err());
        // No backup while a trial waits for the next boot.
        let trial = entry(
            "cps-0f6a2c1e-3b1d-4c9e-8a55-2d0e4c3b9f10-5",
            5,
            Some(2),
            257,
        );
        let with_trial = [
            entry("cps-a47593c2-e985-42d2-92b6-44c7057e1e96-4", 4, None, 256),
            trial,
        ];
        let policy = LoaderPolicy::trial(&existing[0].id, &with_trial[1].id);
        let error = check_backup(Some(&policy), &with_trial, &record(300))
            .unwrap_err()
            .to_string();
        assert!(error.contains("a trial is selected"), "{error}");
        assert!(check_backup(None, &existing, &record(300)).is_err());
        Ok(())
    }

    #[test]
    fn exclusive_esp_locks_remove_interrupted_temporary_files() -> anyhow::Result<()> {
        use std::fs;
        let esp = tempfile::tempdir()?;
        let path = |relative: &str| esp.path().join(relative);
        for directory in ["loader/entries", "EFI/cps/cps-a-1", "EFI/cps-uefi"] {
            fs::create_dir_all(path(directory))?;
        }
        let stale = [
            "loader/.cps-tmp-1a2b3c",
            "loader/entries/.cps-tmp-4d5e6f",
            "EFI/cps/cps-a-1/.cps-tmp-abcdef",
            "EFI/cps-uefi/.cps-tmp-012345",
        ];
        for file in stale {
            fs::write(path(file), "partial")?;
        }
        let kept = ["loader/loader.conf", "EFI/cps/cps-a-1/vmlinuz.efi"];
        for file in kept {
            fs::write(path(file), "kept")?;
        }
        // A shared lock only reads the ESP.
        drop(super::lock_esp(
            esp.path(),
            super::FlockOperation::LockShared,
        )?);
        assert!(stale.iter().all(|file| path(file).exists()));
        drop(super::lock_esp(
            esp.path(),
            super::FlockOperation::LockExclusive,
        )?);
        assert!(stale.iter().all(|file| !path(file).exists()));
        assert!(kept.iter().all(|file| path(file).exists()));
        Ok(())
    }

    #[test]
    fn backup_entries_are_titled_as_backups() {
        let record = record(256);
        let render = |backup| {
            Entry {
                id: "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-4",
                revision: 4,
                cmdline: "root=UUID=x rw",
                btrfs: &record,
                parent: None,
                linux_sha256: "a",
                initrd_sha256: None,
                uki_sha256: None,
                kernel_release: Some("6.12.0"),
                backup,
            }
            .render()
        };
        assert!(render(true).starts_with(
            "title Canopa backup (cps-a47593c2-e985-42d2-92b6-44c7057e1e96-4, Linux 6.12.0)\n"
        ));
        assert!(render(false).starts_with("title Canopa (cps-"));
        // A backup is known-good: no fallback and no boot counter.
        assert!(!render(true).contains("# cps-parent"));
    }

    #[test]
    fn esp_space_counts_only_missing_files_and_suggests_pruning() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let existing = dir.path().join("vmlinuz.efi");
        std::fs::write(&existing, b"kernel")?;
        let id = "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2";
        check_esp_space(dir.path(), id, &[(dir.path().join("initramfs.img"), 4096)])?;
        // A retry reuses the file that is already in place.
        check_esp_space(dir.path(), id, &[(existing, u64::MAX / 2)])?;
        let message = check_esp_space(
            dir.path(),
            id,
            &[(dir.path().join("uki.efi"), u64::MAX / 2)],
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("insufficient free space on the EFI system partition"));
        assert!(message.contains(id) && message.contains("cps boot prune"));
        Ok(())
    }

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
