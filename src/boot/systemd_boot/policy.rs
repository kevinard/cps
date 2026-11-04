use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tempfile::NamedTempFile;

use super::{canonical_partuuid, read_regular};

const OPTIONS: &str = "timeout 0\neditor no\nauto-entries no\nauto-firmware no\n";
const AUTO: &str = "default cps-*.conf\n";

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

pub(super) struct BootEntry {
    pub path: PathBuf,
    pub id: String,
    pub revision: u64,
    pub tries: Option<u32>,
    pub description: String,
    pub title: String,
    pub partuuid: String,
    pub parent: Option<String>,
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
            !entries.iter().any(|entry: &BootEntry| entry.id == id),
            "duplicate CPS entry {id}"
        );
        let path = entry.path();
        let description =
            String::from_utf8(read_regular(&path)?.context("boot entry disappeared")?)?;
        let title = field(&description, "title ")?
            .context("CPS boot entry has no title")?
            .to_owned();
        ensure!(
            field(&description, "sort-key ")? == Some("cps")
                && field(&description, "version ")? == Some(revision.to_string().as_str()),
            "boot entry sort key or version differs from CPS policy: {id}"
        );
        let options = field(&description, "options ")?.context("CPS boot entry has no options")?;
        let mut roots = options
            .split_ascii_whitespace()
            .filter_map(|arg| arg.strip_prefix("root="));
        let partuuid = roots
            .next()
            .and_then(|root| root.strip_prefix("PARTUUID="))
            .context("CPS boot entry is missing its root PARTUUID")?;
        ensure!(
            roots.next().is_none(),
            "multiple root options in CPS boot entry {id}"
        );
        let partuuid = canonical_partuuid(partuuid)?;
        let parent = field(&description, "# cps-parent ")?;
        if let Some(parent) = parent {
            ensure!(
                entry_identity(parent)?.1.checked_add(1) == Some(revision),
                "invalid parent for {id}"
            );
        }
        let parent = parent.map(str::to_owned);
        entries.push(BootEntry {
            path,
            id: id.to_owned(),
            revision,
            tries,
            description,
            title,
            partuuid,
            parent,
        });
    }
    entries.sort_by(|a, b| (b.revision, &b.id).cmp(&(a.revision, &a.id)));
    Ok(entries)
}

fn field<'a>(entry: &'a str, prefix: &str) -> Result<Option<&'a str>> {
    let mut values = entry.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = values.next();
    ensure!(
        values.next().is_none(),
        "duplicate boot entry field {prefix}"
    );
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

fn directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "{} is not a directory", path.display());
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
