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

const LOADER_CONFIG: &[u8] =
    b"default cps-*.conf\ntimeout 0\neditor no\nauto-entries no\nauto-firmware no\n";

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
            Some((entry_id(&parent_id, parent.revision), parent.revision))
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
    let esp_dir = File::open(&esp)?;
    ensure!(
        esp_dir.metadata()?.is_dir(),
        "expected an EFI system partition directory"
    );
    flock(&esp_dir, FlockOperation::LockExclusive)?;

    let loader = ensure_dir(&esp, "loader")?;
    let entries = ensure_dir(&loader, "entries")?;
    let existing = boot_entries(&entries)?;
    let existing_entry = existing.iter().find(|entry| entry.id == id);
    let config_path = loader.join("loader.conf");
    if let Some(config) = read_regular(&config_path)? {
        ensure!(
            config == LOADER_CONFIG,
            "EFI partition loader.conf differs from the CPS boot policy"
        );
    } else {
        ensure!(
            fallback.is_none() && existing.is_empty(),
            "missing CPS loader.conf"
        );
    }

    let description = Entry {
        id: &id,
        revision: settings.revision,
        partuuid: &partuuid,
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
            read_regular(&entry.path)?.as_deref() == Some(description.as_bytes()),
            "this environment already has a different boot entry"
        );
    } else if let Some((fallback, base_revision)) = &fallback {
        ensure!(
            existing
                .iter()
                .any(|entry| &entry.id == fallback && entry.tries.is_none()),
            "the base environment must have a known-good boot entry: {fallback}"
        );
        for entry in &existing {
            ensure!(
                entry.tries.is_none_or(|tries| tries == 0),
                "another boot trial is still pending: {}",
                entry.id
            );
            if entry.tries.is_none() && &entry.id != fallback {
                ensure!(
                    entry.revision < *base_revision,
                    "the base is not the newest known-good environment"
                );
            }
            let bytes = read_regular(&entry.path)?.context("boot entry disappeared")?;
            let config = std::str::from_utf8(&bytes)?;
            if &entry.id == fallback {
                verify_fallback_artifacts(&esp, fallback, config)?;
            }
            let root = config
                .lines()
                .find_map(|line| line.strip_prefix("options "))
                .and_then(|options| {
                    options
                        .split_ascii_whitespace()
                        .find_map(|arg| arg.strip_prefix("root=PARTUUID="))
                })
                .context("CPS boot entry is missing its root PARTUUID")?;
            ensure!(
                !root.eq_ignore_ascii_case(&partuuid),
                "each environment requires a distinct root PARTUUID"
            );
        }
    } else {
        ensure!(
            existing.is_empty(),
            "--known-good is only used to bootstrap the first environment"
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
            install(&config_path, io::Cursor::new(LOADER_CONFIG))?;
        }
        let file = match fallback {
            None => format!("{id}.conf"),
            Some(_) => format!("{id}+{}.conf", activation.tries),
        };
        // Publishing the entry is the activation boundary. All paths named
        // by the entry are already durable, as is the known-good fallback.
        install(&entries.join(file), io::Cursor::new(description.as_bytes()))?;
    }
    Ok(id)
}

struct Entry<'a> {
    id: &'a str,
    revision: u64,
    partuuid: &'a str,
    options: &'a str,
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
        if *initrd {
            entry.push_str(&format!("initrd /EFI/cps/{id}/initramfs.img\n"));
        }
        entry.push_str(&format!(
            "options root=PARTUUID={partuuid} rootwait {options}\n"
        ));
        entry
    }
}

struct BootEntry {
    path: PathBuf,
    id: String,
    revision: u64,
    tries: Option<u32>,
}

fn boot_entries(dir: &Path) -> Result<Vec<BootEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|s| s.strip_suffix(".conf")) else {
            continue;
        };
        ensure!(
            name.starts_with("cps-"),
            "the EFI partition has a non-CPS boot entry: {name}"
        );
        ensure!(entry.file_type()?.is_file(), "invalid CPS entry {name}");
        let (id, tries) = match name.split_once('+') {
            None => (name, None),
            Some((id, count)) => {
                let left = if let Some((left, done)) = count.split_once('-') {
                    done.parse::<u32>().context("invalid boot attempts done")?;
                    left
                } else {
                    count
                };
                (
                    id,
                    Some(left.parse::<u32>().context("invalid boot attempts left")?),
                )
            }
        };
        let (image, revision) = id
            .strip_prefix("cps-")
            .and_then(|id| id.rsplit_once('-'))
            .context("invalid CPS entry ID")?;
        canonical_partuuid(image).context("invalid image UUID in CPS entry")?;
        ensure!(
            !entries.iter().any(|entry: &BootEntry| entry.id == id),
            "duplicate CPS entry {id}"
        );
        entries.push(BootEntry {
            path: entry.path(),
            id: id.to_owned(),
            revision: revision.parse()?,
            tries,
        });
    }
    Ok(entries)
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
