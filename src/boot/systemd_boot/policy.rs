use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tempfile::NamedTempFile;

use super::{canonical_partuuid, read_regular};
use crate::{
    boot::{btrfs_boot::BtrfsRecord, uki},
    model::validate_digest,
};

const OPTIONS: &str = "timeout 0\neditor no\nauto-entries no\nauto-firmware no\n";
const AUTO: &str = "default cps-*.conf\n";
pub(super) const MAX_ENTRY_BYTES: usize = 64 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum LoaderPolicy {
    Automatic,
    Pinned {
        default: String,
        preferred: Option<String>,
    },
}

impl LoaderPolicy {
    pub fn pinned(id: &str) -> Self {
        Self::Pinned {
            default: id.to_owned(),
            preferred: None,
        }
    }

    pub fn trial(base: &str, candidate: &str) -> Self {
        Self::Pinned {
            default: base.to_owned(),
            preferred: Some(candidate.to_owned()),
        }
    }

    pub fn selected<'a>(&self, entries: &'a [BootEntry]) -> Option<&'a BootEntry> {
        match self {
            Self::Automatic => entries
                .iter()
                .filter(|entry| entry.tries != Some(0))
                .max_by_key(|entry| (entry.revision, entry.id.as_str())),
            Self::Pinned { default, preferred } => preferred
                .as_deref()
                .and_then(|id| {
                    entries
                        .iter()
                        .find(|entry| entry.id == id && entry.tries != Some(0))
                })
                .or_else(|| {
                    entries
                        .iter()
                        .find(|entry| entry.id == *default && entry.tries.is_none())
                }),
        }
    }

    pub fn read(path: &Path) -> Result<Option<Self>> {
        let Some(bytes) = read_regular(path)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes)?;
        let selections = text
            .strip_suffix(OPTIONS)
            .context("EFI partition loader.conf differs from the CPS boot policy")?;
        if selections == AUTO {
            return Ok(Some(Self::Automatic));
        }
        let (default, remaining) = selections
            .split_once('\n')
            .context("missing default boot entry")?;
        let default = selector(default, "default ")?;
        let preferred = if remaining.is_empty() {
            None
        } else {
            let value = remaining
                .strip_suffix('\n')
                .context("invalid preferred boot entry")?;
            let preferred = selector(value, "preferred ")?;
            ensure!(
                preferred != default,
                "default and preferred boot entries must be distinct"
            );
            Some(preferred)
        };
        Ok(Some(Self::Pinned { default, preferred }))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let selections = match self {
            Self::Automatic => AUTO.to_owned(),
            Self::Pinned { default, preferred } => {
                let mut text = format!("default {default}.conf\n");
                if let Some(preferred) = preferred {
                    text.push_str(&format!("preferred {preferred}.conf\n"));
                }
                text
            }
        };
        let content = format!("{selections}{OPTIONS}");
        let parent = path.parent().context("missing loader directory")?;
        if read_regular(path)?.as_deref() == Some(content.as_bytes()) {
            File::open(path)?.sync_all()?;
        } else {
            let mut temp = NamedTempFile::new_in(parent)?;
            temp.write_all(content.as_bytes())?;
            temp.as_file().sync_all()?;
            temp.persist(path)?;
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn selector(line: &str, prefix: &str) -> Result<String> {
    let id = line
        .strip_prefix(prefix)
        .and_then(|line| line.strip_suffix(".conf"))
        .context("invalid CPS boot entry selector")?;
    entry_identity(id)?;
    Ok(id.to_owned())
}

pub(super) fn entry_identity(id: &str) -> Result<(&str, u64)> {
    let (image, revision) = id
        .strip_prefix("cps-")
        .and_then(|id| id.rsplit_once('-'))
        .context("invalid CPS entry ID")?;
    ensure!(
        canonical_partuuid(image)? == image,
        "invalid CPS image UUID"
    );
    let number: u64 = revision.parse().context("invalid CPS entry revision")?;
    ensure!(
        revision == number.to_string(),
        "noncanonical CPS entry revision"
    );
    Ok((image, number))
}

pub(super) fn check_parent(parent: &str, child: &str) -> Result<()> {
    let (parent_image, parent_revision) = entry_identity(parent)?;
    let (child_image, child_revision) = entry_identity(child)?;
    ensure!(
        parent_image != child_image && parent_revision < child_revision,
        "invalid boot environment parent"
    );
    Ok(())
}

#[derive(Debug)]
pub(super) struct BootEntry {
    pub path: PathBuf,
    pub id: String,
    pub revision: u64,
    pub tries: Option<u32>,
    pub description: String,
    pub title: String,
    pub root: BootRoot,
    pub parent: Option<String>,
    pub btrfs: Option<BtrfsRecord>,
    pub uki: Option<BootUki>,
}

#[derive(Debug)]
pub(super) struct BootUki {
    pub sha256: String,
    pub cmdline: String,
}

impl BootUki {
    fn parse(entry: &str, id: &str) -> Result<Option<Self>> {
        let values = [
            boot_field(entry, "uki")?,
            field(entry, uki::HASH)?,
            field(entry, uki::CMDLINE)?,
        ];
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        let [Some(path), Some(sha256), Some(cmdline)] = values else {
            anyhow::bail!("incomplete UKI boot entry");
        };
        ensure!(
            path == format!("/EFI/cps/{id}/{}", uki::NAME),
            "unexpected UKI path in boot entry"
        );
        for line in entry.lines().map(str::trim_ascii_start) {
            if !line.is_empty() && !line.starts_with('#') {
                let key = line.split_ascii_whitespace().next();
                ensure!(
                    matches!(key, Some("title" | "sort-key" | "version" | "uki")),
                    "unexpected field in UKI boot entry: {line}"
                );
            }
        }
        validate_digest(sha256)?;
        ensure!(
            cmdline
                .bytes()
                .all(|c| c == b' ' || c.is_ascii_graphic() && c != b'"'),
            "invalid UKI command line"
        );
        Ok(Some(Self {
            sha256: sha256.to_owned(),
            cmdline: cmdline.to_owned(),
        }))
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::boot) enum BootRoot {
    Partition(String),
    Btrfs(String),
}

impl BootRoot {
    fn parse(options: &str, btrfs: Option<&BtrfsRecord>) -> Result<Self> {
        let mut roots = options
            .split_ascii_whitespace()
            .filter(|arg| *arg == "root" || arg.starts_with("root="));
        let root = roots
            .next()
            .and_then(|arg| arg.strip_prefix("root="))
            .context("CPS boot entry is missing root=")?;
        ensure!(
            roots.next().is_none(),
            "multiple root options in CPS boot entry"
        );
        if let Some(uuid) = root.strip_prefix("PARTUUID=") {
            return Ok(Self::Partition(canonical_partuuid(uuid)?));
        }
        let record = btrfs.context("CPS boot entry is missing its root PARTUUID")?;
        ensure!(
            !options.contains('"') && root.strip_prefix("UUID=") == Some(record.fsid.as_str()),
            "Btrfs root UUID differs from the recorded filesystem"
        );
        Ok(Self::Btrfs(record.fsid.clone()))
    }

    pub fn partuuid(&self) -> Result<&str> {
        match self {
            Self::Partition(uuid) => Ok(uuid),
            Self::Btrfs(_) => anyhow::bail!("expected a root PARTUUID"),
        }
    }

    pub fn argument(&self) -> String {
        match self {
            Self::Partition(uuid) => format!("PARTUUID={uuid}"),
            Self::Btrfs(uuid) => format!("UUID={uuid}"),
        }
    }
}

pub(super) fn boot_entries(dir: &Path) -> Result<Vec<BootEntry>> {
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
        let parsed = read_entry(entry.path(), name)?;
        ensure!(
            !entries
                .iter()
                .any(|entry: &BootEntry| entry.id == parsed.id),
            "duplicate CPS entry {}",
            parsed.id
        );
        entries.push(parsed);
    }
    entries.sort_by(|a, b| (b.revision, &b.id).cmp(&(a.revision, &a.id)));
    Ok(entries)
}

/// Parse the original BLS text retained during an interrupted destruction.
pub(super) fn read_entry(path: PathBuf, name: &str) -> Result<BootEntry> {
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
    let (_, revision) = entry_identity(id)?;
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "boot entry is not a regular file"
    );
    let mut bytes = Vec::new();
    File::open(&path)?
        .take(MAX_ENTRY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_ENTRY_BYTES, "boot entry exceeds 64 KiB");
    let description = String::from_utf8(bytes)?;
    let title = boot_field(&description, "title")?
        .context("CPS boot entry has no title")?
        .to_owned();
    ensure!(
        boot_field(&description, "sort-key")? == Some("cps")
            && boot_field(&description, "version")? == Some(revision.to_string().as_str()),
        "boot entry sort key or version differs from CPS policy: {id}"
    );
    let uki = BootUki::parse(&description, id)?;
    let options = match &uki {
        Some(uki) => uki.cmdline.as_str(),
        None => boot_field(&description, "options")?.context("CPS boot entry has no options")?,
    };
    let btrfs = BtrfsRecord::parse(&description, options)?;
    ensure!(
        uki.is_none() || btrfs.is_some(),
        "UKI boot entries require a Btrfs root"
    );
    let root = BootRoot::parse(options, btrfs.as_ref())?;
    if matches!(root, BootRoot::Btrfs(_)) && uki.is_none() {
        ensure!(
            boot_field(&description, "initrd")?.is_some_and(|path| !path.is_empty()),
            "booting by Btrfs filesystem UUID requires an initramfs"
        );
    }
    let parent = field(&description, "# cps-parent ")?;
    if let Some(parent) = parent {
        check_parent(parent, id).with_context(|| format!("invalid parent for {id}"))?;
    }
    let parent = parent.map(str::to_owned);
    Ok(BootEntry {
        path,
        id: id.to_owned(),
        revision,
        tries,
        description,
        title,
        root,
        parent,
        btrfs,
        uki,
    })
}

pub(in crate::boot) fn field<'a>(entry: &'a str, prefix: &str) -> Result<Option<&'a str>> {
    let mut values = entry.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = values.next();
    ensure!(
        values.next().is_none(),
        "duplicate boot entry field {prefix}"
    );
    Ok(value)
}

fn boot_field<'a>(entry: &'a str, key: &str) -> Result<Option<&'a str>> {
    let mut values = entry.lines().filter_map(|line| {
        let (name, value) = line.trim_ascii_start().split_once([' ', '\t'])?;
        (name == key).then(|| value.trim_ascii())
    });
    let value = values.next();
    ensure!(values.next().is_none(), "duplicate boot entry field {key}");
    Ok(value)
}

pub(super) fn inventory(esp: &Path) -> Result<(Option<LoaderPolicy>, Vec<BootEntry>)> {
    let loader = esp.join("loader");
    if !directory_exists(&loader)? {
        return Ok((None, Vec::new()));
    }
    let dir = loader.join("entries");
    let entries = if directory_exists(&dir)? {
        boot_entries(&dir)?
    } else {
        Vec::new()
    };
    let policy = LoaderPolicy::read(&loader.join("loader.conf"))?;
    ensure!(
        policy.is_some() || entries.is_empty(),
        "missing CPS loader.conf"
    );
    Ok((policy, entries))
}

pub(super) fn directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "{} is not a directory", path.display());
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{BootRoot, BtrfsRecord, boot_field, check_parent, read_entry};

    #[test]
    fn boot_parent_precedes_a_distinct_staged_image_after_several_transactions()
    -> anyhow::Result<()> {
        let parent = "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2";
        let child = "cps-61d0a34e-1f51-4cda-963d-cdcd58d5acef";
        check_parent(parent, &format!("{child}-3"))?;
        check_parent(parent, &format!("{child}-5"))?;
        for invalid in [
            format!("{child}-2"),
            format!("{child}-1"),
            "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-3".to_owned(),
        ] {
            assert!(check_parent(parent, &invalid).is_err());
        }
        assert!(
            check_parent(
                "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-18446744073709551615",
                &format!("{child}-5")
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn boot_options_reject_duplicates_with_alternate_whitespace() -> anyhow::Result<()> {
        assert_eq!(
            boot_field("\toptions\troot=PARTUUID=test  \n", "options")?,
            Some("root=PARTUUID=test")
        );
        assert!(
            boot_field(
                "options root=PARTUUID=test\n\toptions\trootflags=subvolid=278\n",
                "options"
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn filesystem_uuid_boot_entry_requires_matching_btrfs_identity_and_initrd() -> anyhow::Result<()>
    {
        let record = BtrfsRecord {
            fsid: "ebcd29cb-f4ac-43ef-8f65-f7975a22d024".into(),
            uuid: "82ec1cf9-8e6f-4b7a-afb5-d3b171a2b4a2".into(),
            subvolid: 277,
            path: "@environments/trial/root".into(),
        };
        let file = tempfile::NamedTempFile::new()?;
        let name = "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2+2";
        let header = "title Canopa\nsort-key cps\nversion 2\n";
        let initrd = "initrd /EFI/cps/trial/initramfs.img\n";
        let options = format!(
            "options root=UUID={} rootfstype=btrfs rootflags=subvolid=277 rd.luks.name={}=root rw\n",
            record.fsid, record.uuid
        );
        let identity = record.render();
        let entry = format!("{header}{initrd}{identity}{options}");
        let read = |text: &str| {
            std::fs::write(file.path(), text)?;
            read_entry(file.path().to_owned(), name)
        };
        assert_eq!(read(&entry)?.root, BootRoot::Btrfs(record.fsid.clone()));
        for invalid in [
            entry.replace(
                &format!("root=UUID={}", record.fsid),
                &format!("root=UUID={}", record.uuid),
            ),
            entry.replace(initrd, ""),
            entry.replace(&identity, ""),
            entry.replace(" rw", " root=/dev/mapper/root rw"),
        ] {
            assert!(read(&invalid).is_err());
        }
        Ok(())
    }

    #[test]
    fn uki_boot_entry_binds_one_image_and_uses_its_embedded_command_line() -> anyhow::Result<()> {
        let record = BtrfsRecord {
            fsid: "ebcd29cb-f4ac-43ef-8f65-f7975a22d024".into(),
            uuid: "82ec1cf9-8e6f-4b7a-afb5-d3b171a2b4a2".into(),
            subvolid: 277,
            path: "@environments/trial/root".into(),
        };
        let id = "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2";
        let file = tempfile::NamedTempFile::new()?;
        let entry = format!(
            "title Canopa\nsort-key cps\nversion 2\nuki /EFI/cps/{id}/uki.efi\n{}{}\n{}root=UUID={} rootfstype=btrfs rootflags=subvolid=277 rw\n{}",
            super::uki::HASH,
            "a".repeat(64),
            super::uki::CMDLINE,
            record.fsid,
            record.render(),
        );
        let read = |text: &str| {
            std::fs::write(file.path(), text)?;
            read_entry(file.path().to_owned(), &format!("{id}+2"))
        };
        assert!(read(&entry)?.uki.is_some());
        for invalid in [
            format!("{entry}options rw\n"),
            format!("{entry}extra /addon.efi\n"),
            format!("{entry}profile 1\n"),
            entry.replace("/uki.efi", "/other.efi"),
            entry.replace("rootflags=subvolid=277", "rootflags=subvolid=278"),
            entry.replace(super::uki::HASH, "# unrelated "),
        ] {
            assert!(read(&invalid).is_err());
        }
        Ok(())
    }
}
