mod policy;

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FlockOperation, flock};
use tempfile::NamedTempFile;

use super::{check_image_integrity, entry_id, kernel};
use crate::{
    image::{Image, root::Root},
    model::digest_reader,
};

use self::policy::{BootEntry, LoaderPolicy, inventory};

#[derive(Debug)]
pub struct BootEnvironment {
    pub id: String,
    pub title: String,
    pub root_partuuid: String,
    pub tries_left: Option<u32>,
    pub is_default: bool,
}

pub fn list_boot_environments(esp: &Path) -> Result<Vec<BootEnvironment>> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let _lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let (policy, entries) = inventory(&esp)?;
    let selected = policy
        .as_ref()
        .and_then(|policy| policy.selected(&entries))
        .map(|entry| entry.id.clone());
    Ok(entries
        .into_iter()
        .map(|entry| BootEnvironment {
            is_default: selected.as_deref() == Some(&entry.id),
            id: entry.id,
            title: entry.title,
            root_partuuid: entry.partuuid,
            tries_left: entry.tries,
        })
        .collect())
}

pub fn rollback_boot_environment(esp: &Path, id: Option<&str>) -> Result<String> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let _lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let (policy, entries) = inventory(&esp)?;
    let policy = policy.context("no CPS boot environments registered")?;
    let target = if let Some(id) = id {
        find_known_good(&entries, id)?
    } else {
        let current = policy
            .selected(&entries)
            .context("no configured default CPS boot environment")?;
        if let Some(parent) = &current.parent {
            find_known_good(&entries, parent)?
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

pub struct Activation {
    pub root_partuuid: String,
    pub kernel: String,
    pub initrd: Option<String>,
    pub options: String,
    pub tries: u32,
    pub known_good: bool,
}

pub fn activate_boot_environment(
    root: &Path,
    esp: &Path,
    activation: &Activation,
) -> Result<String> {
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
    let image = Image::open(&root, false)?;
    check_image_integrity(&image)?;
    let state = image.state()?;
    let settings = &state.settings;
    let image_id = canonical_partuuid(&settings.id).context("invalid image identity")?;
    let id = entry_id(&image_id, settings.revision);
    let fallback = match (activation.known_good, &settings.boot_parent) {
        (true, None) => None,
        (false, Some(parent)) => {
            ensure!(
                parent.revision.checked_add(1) == Some(settings.revision),
                "the staged image has changed since staging"
            );
            let parent_id =
                canonical_partuuid(&parent.id).context("invalid base image identity")?;
            Some(entry_id(&parent_id, parent.revision))
        }
        (true, Some(_)) => bail!("a staged environment must be activated as a counted trial"),
        (false, None) => {
            bail!("stage an upgrade first, or use --known-good to bootstrap the base entry")
        }
    };

    let managed = kernel::selected(&state.installed, &state.paths)?;
    let (kernel, initrd) = if let Some(artifacts) = &managed {
        ensure!(
            activation.kernel == "boot/vmlinuz" && activation.initrd.is_none(),
            "the image uses kernel-install artifacts; omit --kernel and --initrd"
        );
        artifacts.open_prepared(&image.root)?
    } else {
        let kernel = image
            .root
            .file(&activation.kernel)
            .context("opening the environment kernel")?;
        let initrd = activation
            .initrd
            .as_deref()
            .map(|path| image.root.file(path))
            .transpose()
            .context("opening the environment initramfs")?;
        (kernel, initrd)
    };
    let _lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let loader = ensure_dir(&esp, "loader")?;
    let entries = ensure_dir(&loader, "entries")?;
    let (policy, existing) = inventory(&esp)?;
    let existing_entry = existing.iter().find(|entry| entry.id == id);
    let config_path = loader.join("loader.conf");

    let description = Entry {
        id: &id,
        revision: settings.revision,
        partuuid: &partuuid,
        parent: if existing_entry.is_some_and(|entry| entry.parent.is_none()) {
            None // Keep activation retries compatible with previously registered entries.
        } else {
            fallback.as_deref()
        },
        options: &activation.options,
        initrd: initrd.is_some(),
        kernel_release: managed
            .as_ref()
            .map(|artifacts| artifacts.kernel.release.as_str()),
    }
    .render();
    if let Some(entry) = existing_entry {
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
        ensure!(
            !existing
                .iter()
                .any(|entry| entry.id != id && entry.partuuid == partuuid),
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

    let efi = ensure_dir(&esp, "EFI")?;
    let artifacts = ensure_dir(&ensure_dir(&efi, "cps")?, &id)?;
    install(&artifacts.join("vmlinuz.efi"), kernel)?;
    if let Some(initrd) = initrd {
        install(&artifacts.join("initramfs.img"), initrd)?;
    }
    if let Some(entry) = existing_entry {
        File::open(&entry.path)?.sync_all()?;
        File::open(&entries)?.sync_all()?;
    } else {
        if fallback.is_none() {
            LoaderPolicy::Automatic.write(&config_path)?;
        }
        let file = match fallback {
            None => format!("{id}.conf"),
            Some(_) => format!("{id}+{}.conf", activation.tries),
        };
        install(&entries.join(file), io::Cursor::new(description.as_bytes()))?;
    }
    if let Some(fallback) = fallback
        .as_deref()
        .filter(|_| select_trial && policy != Some(LoaderPolicy::Automatic))
    {
        // The counted entry and its files are durable before selection.
        LoaderPolicy::trial(fallback, &id).write(&config_path)?;
    }

    Ok(id)
}

struct Entry<'a> {
    id: &'a str,
    revision: u64,
    partuuid: &'a str,
    options: &'a str,
    parent: Option<&'a str>,
    initrd: bool,
    kernel_release: Option<&'a str>,
}

impl Entry<'_> {
    fn render(&self) -> String {
        let Self {
            id,
            revision,
            partuuid,
            options,
            parent,
            initrd,
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
        if *initrd {
            entry.push_str(&format!("initrd /EFI/cps/{id}/initramfs.img\n"));
        }
        entry.push_str(&format!(
            "options root=PARTUUID={partuuid} rootwait {options}\n"
        ));
        entry
    }
}

fn verify_fallback_artifacts(esp: &Path, id: &str, entry: &str) -> Result<()> {
    let esp = Root::open(esp)?;
    let kernel = format!("EFI/cps/{id}/vmlinuz.efi");
    let expected = format!("linux /{kernel}");
    ensure!(
        entry.lines().any(|line| line == expected),
        "unexpected fallback kernel path"
    );
    esp.file(&kernel)
        .context("opening the known-good fallback kernel")?;
    let initrd = format!("EFI/cps/{id}/initramfs.img");
    for path in entry
        .lines()
        .filter_map(|line| line.strip_prefix("initrd /"))
    {
        ensure!(path == initrd, "unexpected fallback initramfs path");
        esp.file(path)
            .context("opening the known-good fallback initramfs")?;
    }
    Ok(())
}

fn canonical_partuuid(value: &str) -> Result<String> {
    ensure!(
        value.len() == 36
            && value.bytes().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => c == b'-',
                _ => c.is_ascii_hexdigit(),
            }),
        "expected a GPT PARTUUID in UUID form"
    );
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

fn install(path: &Path, mut bytes: impl Read) -> Result<()> {
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
                digest_reader(&current)? == digest_reader(&mut bytes)?,
                "boot file {} already exists with different contents",
                path.display()
            );
            current.sync_all()?;
            File::open(parent)?.sync_all()?;
            return Ok(());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut temp = NamedTempFile::new_in(parent)?;
    io::copy(&mut bytes, &mut temp)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
