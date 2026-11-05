mod destroy;
pub(super) mod policy;
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
    entry_id, kernel, tree,
};
use crate::{
    image::{Image, root::Root},
    kdl_format::boot_export::ExportReceipt,
    model::{digest, digest_reader, is_uuid, validate_digest},
};

use self::policy::{BootEntry, LoaderPolicy, field, inventory};

const LINUX_HASH: &str = "# cps-linux-sha256 ";
const INITRD_HASH: &str = "# cps-initrd-sha256 ";

#[derive(Debug)]
pub struct BootEnvironment {
    pub id: String,
    pub title: String,
    pub root_partuuid: String,
    pub tries_left: Option<u32>,
    pub is_default: bool,
    pub is_retiring: bool,
}

pub fn list_boot_environments(esp: &Path) -> Result<Vec<BootEnvironment>> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let _lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let (policy, entries) = inventory(&esp)?;
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
            root_partuuid: entry.partuuid,
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
    let (policy, entries) = inventory(&esp)?;
    let policy = policy.context("no CPS boot environments registered")?;
    let target = if let Some(id) = id {
        find_known_good(&entries, id)?
    } else {
        let current = policy
            .selected(&entries)
            .context("no configured default CPS boot environment")?;
        if let Some(parent) = &current.parent {
            find_known_good(&entries, parent).context("the recorded parent is unavailable")?
        } else {
            // Entries made before CPS recorded their parent were activated
            // from the newest known-good image at the time.
            entries
                .iter()
                .find(|entry| entry.tries.is_none() && entry.revision < current.revision)
                .context("no previous known-good boot environment")?
        }
    };
    verify_fallback_artifacts(&esp, &target.id, &target.description)?;
    let _root_lock = if let Some(record) = &target.btrfs {
        let volume = volume.context("use --btrfs-volume to verify this rollback root")?;
        volume.check_disk(&esp, &lock, &target.partuuid)?;
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
        disk.verify_filesystem(&target.partuuid, image, revision)
            .context("checking the rollback root partition")?;
    }
    LoaderPolicy::pinned(&target.id).write(&esp.join("loader/loader.conf"))?;
    Ok(target.id.clone())
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

pub(super) fn with_running_environment<T>(
    volume: &BtrfsVolume,
    esp: &Path,
    partuuid: &str,
    action: impl FnOnce(Root, &str, u64) -> Result<T>,
) -> Result<T> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let partuuid = canonical_partuuid(partuuid)?;
    volume.check_disk(&esp, &lock, &partuuid)?;
    let (policy, entries) = inventory(&esp)?;
    let selected = policy
        .and_then(|policy| policy.selected(&entries))
        .context("no CPS boot environment is selected")?;
    ensure!(
        selected.tries.is_none(),
        "a trial is pending; boot it before making more package changes"
    );
    ensure!(
        selected.partuuid == partuuid,
        "the selected root uses a different partition"
    );
    let record = selected
        .btrfs
        .as_ref()
        .context("expected a registered Btrfs root")?;
    let registered = volume.open_registered(record, &selected.id)?;
    let running = Root::open(Path::new("/"))?;
    let expected = registered.as_file().metadata()?;
    let actual = running.as_file().metadata()?;
    ensure!(
        (expected.dev(), expected.ino()) == (actual.dev(), actual.ino()),
        "a different system is selected for the next boot; reboot before changing packages"
    );
    let (id, revision) = policy::entry_identity(&selected.id)?;
    action(running, id, revision)
}

pub struct Activation {
    pub root_partuuid: String,
    pub kernel: String,
    pub initrd: Option<String>,
    pub options: String,
    pub tries: u32,
    pub known_good: bool,
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
    Ok(activate(root, esp, activation, None, None)?.id)
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
    Ok(activate(root, esp, activation, Some(deployment), None)?.id)
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
    Ok(activate(root, esp, activation, Some(deployment), None)?.id)
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

pub(super) fn activate(
    root: &Path,
    esp: &Path,
    activation: &Activation,
    deployment: Option<Deployment<'_>>,
    volume: Option<&BtrfsVolume>,
) -> Result<ActivationUpdate> {
    let partuuid = canonical_partuuid(&activation.root_partuuid)?;
    validate_options(&activation.options)?;
    ensure!(
        (1..=99).contains(&activation.tries),
        "tries must be between 1 and 99"
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
                .any(|arg| arg.starts_with("rootflags=") || arg.starts_with("rootfstype=")),
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
    let (kernel, initrd, linux_sha256, initrd_sha256) = if let Some(artifacts) = &managed {
        ensure!(
            activation.kernel == "boot/vmlinuz" && activation.initrd.is_none(),
            "the image uses kernel-install artifacts; omit --kernel and --initrd"
        );
        let (kernel, initrd) = artifacts.open_prepared(&image.root)?;
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
    let receipt = if let Some(deployment) = deployment {
        let receipt = read_export_receipt(deployment.receipt)?;
        ensure!(
            receipt.image == image_id && receipt.revision == settings.revision,
            "root export receipt is for a different image or revision"
        );
        ensure!(
            receipt.state_sha256 == digest_reader(image.root.file(".cps/state.sqlite")?)?,
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
                target.verify(&partuuid, &receipt)?;
            } else {
                deployment
                    .disk
                    .verify_payload(&partuuid, receipt.bytes, &receipt.sha256)?;
                deployment
                    .disk
                    .verify_filesystem(&partuuid, &image_id, settings.revision)?;
            }
        }
        Some(receipt)
    } else {
        None
    };
    if let Some(volume) = volume {
        volume.check_disk(&esp, &_lock, &partuuid)?;
    } else if let Some(target) = deployment.and_then(|deployment| deployment.target) {
        target.check_esp(&esp, &_lock)?;
    } else {
        ensure!(
            !is_fat(&_lock)?,
            "use boot-env activate-device for a mounted VFAT ESP"
        );
    }
    let (policy, existing) = inventory(&esp)?;
    ensure!(
        !destroy::pending(&esp)?.iter().any(|entry| entry.id == id),
        "this boot environment is retiring; finish destroy-btrfs first"
    );
    let existing_entry = existing.iter().find(|entry| entry.id == id);

    // Retries of entries created before artifact hashes keep the original BLS text.
    let record_hashes = if let Some(entry) = existing_entry {
        field(&entry.description, LINUX_HASH)?.is_some()
            || field(&entry.description, INITRD_HASH)?.is_some()
    } else {
        true
    };
    let description = Entry {
        id: &id,
        revision: boot_revision,
        partuuid: &partuuid,
        parent: if existing_entry.is_some_and(|entry| entry.parent.is_none()) {
            None // Keep activation retries compatible with previously registered entries.
        } else {
            fallback.as_deref()
        },
        options: &activation.options,
        btrfs: btrfs.as_ref(),
        linux_sha256: &linux_sha256,
        initrd_sha256: initrd_sha256.as_deref(),
        record_hashes,
        kernel_release: managed
            .as_ref()
            .map(|artifacts| artifacts.kernel.release.as_str()),
    }
    .render();
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
            entry.tries != Some(0),
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
    let marker = esp.join(format!("loader/entries/{partuuid}.cps-retired"));
    let pending = read_regular(&marker)?;
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
        verify_fallback_artifacts(&esp, fallback, &base.description)?;
        if let Some(volume) = volume {
            let record = base
                .btrfs
                .as_ref()
                .context("the fallback must be a verified Btrfs environment")?;
            volume.check_disk(&esp, &_lock, &base.partuuid)?;
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
                replace != base.id && partuuid != base.partuuid,
                "cannot replace the known-good base"
            );
            let disk = deployment
                .context("slot replacement requires a verified disk")?
                .disk;
            if let Some(entry) = existing.iter().find(|entry| entry.id == replace) {
                retired = Some(check_replacement(disk, entry, &partuuid)?);
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
                && entry.partuuid == partuuid
                && !matches!((&entry.btrfs, &btrfs), (Some(existing), Some(candidate))
                    if existing.fsid == candidate.fsid
                        && existing.subvolid != candidate.subvolid
                        && existing.uuid != candidate.uuid)),
            "each environment requires a distinct root PARTUUID"
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
                &base.partuuid,
                &canonical_partuuid(&parent.id)?,
                parent.revision,
            )
            .context("checking the known-good fallback partition")?;
    }
    if let Some(root) = root_deployment {
        let deployment = deployment.context("root deployment requires a verified disk")?;
        let receipt = receipt
            .as_ref()
            .context("root deployment requires an export receipt")?;
        if let Some(target) = deployment.target {
            let payload = target.prepare(root.image, &partuuid, receipt)?;
            if let Some(replace) = replace {
                if matches!(policy, Some(LoaderPolicy::Pinned { .. })) {
                    let base = fallback.as_deref().context("missing known-good base")?;
                    LoaderPolicy::pinned(base).write(&esp.join("loader/loader.conf"))?;
                }
                retire_device_entry(&esp, &marker, replace, retired.take())?;
            }
            target.write(payload, &partuuid, receipt)?;
            if replace.is_some() {
                fs::remove_file(&marker)?;
                File::open(esp.join("loader/entries"))?.sync_all()?;
            }
        } else {
            deployment
                .disk
                .write_root(&partuuid, root.image, Some(receipt))?;
            deployment
                .disk
                .verify_filesystem(&partuuid, &image_id, settings.revision)?;
        }
    }
    if btrfs.is_some() {
        mark_registered(&image.root, &id)?;
    }
    let loader = ensure_dir(&esp, "loader")?;
    let entries = ensure_dir(&loader, "entries")?;
    let config_path = loader.join("loader.conf");
    let efi = ensure_dir(&esp, "EFI")?;
    let artifacts = ensure_dir(&ensure_dir(&efi, "cps")?, &id)?;
    let mut files = Vec::new();
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
    if let Some(entry) = existing_entry {
        File::open(&entry.path)?.sync_all()?;
        File::open(&entries)?.sync_all()?;
    } else {
        if fallback.is_none() {
            LoaderPolicy::Automatic.write(&config_path)?;
            files.push("loader/loader.conf".into());
        }
        let file = match fallback {
            None => format!("{id}.conf"),
            Some(_) => format!("{id}+{}.conf", activation.tries),
        };
        install_text(&entries.join(&file), &description)?;
        files.push(format!("loader/entries/{file}"));
    }
    if let Some(fallback) = fallback
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
        entry.partuuid == partuuid,
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
    partuuid: &'a str,
    options: &'a str,
    btrfs: Option<&'a BtrfsRecord>,
    parent: Option<&'a str>,
    linux_sha256: &'a str,
    initrd_sha256: Option<&'a str>,
    record_hashes: bool,
    kernel_release: Option<&'a str>,
}

impl Entry<'_> {
    fn render(&self) -> String {
        let Self {
            id,
            revision,
            partuuid,
            options,
            btrfs,
            parent,
            linux_sha256,
            initrd_sha256,
            record_hashes,
            kernel_release,
        } = self;
        let title = match kernel_release {
            Some(release) => format!("Canopa ({id}, Linux {release})"),
            None => format!("Canopa ({id})"),
        };
        let mut entry = format!(
            "title {title}\nsort-key cps\nversion {revision}\nlinux /EFI/cps/{id}/vmlinuz.efi\n"
        );
        if let Some(parent) = parent {
            entry.push_str(&format!("# cps-parent {parent}\n"));
        }
        if *record_hashes {
            entry.push_str(&format!("{LINUX_HASH}{linux_sha256}\n"));
        }
        if let Some(initrd_sha256) = initrd_sha256 {
            entry.push_str(&format!("initrd /EFI/cps/{id}/initramfs.img\n"));
            if *record_hashes {
                entry.push_str(&format!("{INITRD_HASH}{initrd_sha256}\n"));
            }
        }
        if let Some(btrfs) = btrfs {
            entry.push_str(&btrfs.render());
            entry.push_str(&format!(
                "options root=PARTUUID={partuuid} rootwait rootfstype=btrfs rootflags=subvolid={} {options}\n", btrfs.subvolid
            ));
        } else {
            entry.push_str(&format!(
                "options root=PARTUUID={partuuid} rootwait {options}\n"
            ));
        }
        entry
    }
}

fn verify_fallback_artifacts(esp: &Path, id: &str, entry: &str) -> Result<()> {
    let esp = Root::open(esp)?;
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
            .any(|arg| arg.contains('"') || arg.starts_with("root=")),
        "boot options must be unquoted tokens and root= comes from --root-partuuid"
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

fn install(path: &Path, mut bytes: impl Read, sha256: &str) -> Result<bool> {
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
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)?;
    File::open(parent)?.sync_all()?;
    Ok(true)
}
