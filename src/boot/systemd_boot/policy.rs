use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tempfile::NamedTempFile;

use super::{INITRD_HASH, LINUX_HASH, canonical_partuuid, read_regular};
use crate::{
    boot::{btrfs_boot::BtrfsRecord, uki},
    model::validate_digest,
};

/// Defaults for a new loader.conf. Administrators may change these lines.
const OPTIONS: &str = "timeout 0\neditor no\nauto-entries no\nauto-firmware no\n";
const AUTO: &str = "cps-*.conf";
const LOADER: &str = "loader/loader.conf";
pub(super) const MAX_ENTRY_BYTES: usize = 64 * 1024;
pub(super) const ENTRY_FORMAT_FIELD: &str = "# cps-boot-entry-version ";
pub(super) const ENTRY_FORMAT: &str = "1";

/// The boot selection CPS owns in loader.conf.
///
/// CPS owns the `default` and `preferred` keys and rejects `timeout
/// menu-force`. Every other line belongs to the administrator and is
/// preserved when CPS changes the selection.
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

    /// Read the CPS selection. A missing file or `default` line selects nothing.
    pub fn read(path: &Path) -> Result<Option<Self>> {
        let Some(bytes) = read_regular(path)? else {
            return Ok(None);
        };
        Self::parse(std::str::from_utf8(&bytes).context("loader.conf is not UTF-8")?)
    }

    fn parse(text: &str) -> Result<Option<Self>> {
        let mut default = None;
        let mut preferred = None;
        for (key, value, quote) in text.lines().filter_map(loader_line) {
            if matches!(key, "default" | "preferred") {
                ensure!(
                    quote != Some('\''),
                    "{LOADER} sets `{key}` to a single-quoted value. systemd-boot before version \
                     260 reads single quotes literally, so the selection would differ between \
                     versions; remove the quotes"
                );
            }
            let slot = match key {
                "default" => &mut default,
                "preferred" => &mut preferred,
                "timeout" => {
                    ensure!(
                        !timeout_stalls(value, false),
                        "{LOADER} sets `timeout {value}`, which waits at the boot menu \
                         indefinitely like `menu-force` and stops unattended trial and fallback \
                         boots. CPS expects `timeout` to be a number of seconds, `menu-hidden` or \
                         `menu-disabled`; change or remove that line"
                    );
                    continue;
                }
                _ => continue,
            };
            ensure!(
                slot.replace(value).is_none(),
                "{LOADER} sets `{key}` more than once. CPS owns this key; keep only the line \
                 that CPS wrote"
            );
        }
        let Some(default) = default else {
            ensure!(
                preferred.is_none(),
                "{LOADER} sets `preferred` without a CPS `default`. Remove the `preferred` line, \
                 then select a known-good environment with `cps boot rollback`"
            );
            return Ok(None);
        };
        if default == AUTO {
            ensure!(
                preferred.is_none(),
                "{LOADER} sets `preferred` with `default {AUTO}`. CPS expects no `preferred` \
                 line in automatic mode; remove it"
            );
            return Ok(Some(Self::Automatic));
        }
        let default = selector(default).with_context(|| {
            format!(
                "{LOADER} sets `default {default}`, but CPS owns this key and expects \
                 `default <CPS entry ID>.conf` or `default {AUTO}`. Restore the CPS value, or \
                 remove the line and select a known-good environment with \
                 `cps boot rollback --esp ESP ID`"
            )
        })?;
        let preferred = preferred
            .map(|value| {
                selector(value).with_context(|| {
                    format!(
                        "{LOADER} sets `preferred {value}`, but CPS owns this key and expects \
                         `preferred <CPS trial ID>.conf` or no `preferred` line. Remove the \
                         line to cancel the trial selection"
                    )
                })
            })
            .transpose()?;
        ensure!(
            preferred.as_ref() != Some(&default),
            "default and preferred boot entries must be distinct"
        );
        Ok(Some(Self::Pinned { default, preferred }))
    }

    fn selection(&self) -> String {
        match self {
            Self::Automatic => format!("default {AUTO}\n"),
            Self::Pinned { default, preferred } => {
                let mut text = format!("default {default}.conf\n");
                if let Some(preferred) = preferred {
                    text.push_str(&format!("preferred {preferred}.conf\n"));
                }
                text
            }
        }
    }

    /// Replace the owned selection lines in place and keep every other line.
    fn render(&self, existing: Option<&str>) -> Result<String> {
        let Some(text) = existing else {
            return Ok(format!("{}{OPTIONS}", self.selection()));
        };
        Self::parse(text)?;
        let mut content = String::with_capacity(text.len() + 128);
        let mut placed = false;
        for line in text.split_inclusive('\n') {
            match loader_line(line).map(|(key, ..)| key) {
                Some("default") if !placed => {
                    content.push_str(&self.selection());
                    placed = true;
                }
                Some("default" | "preferred") => {}
                _ => {
                    content.push_str(line);
                    if !line.ends_with('\n') {
                        content.push('\n');
                    }
                }
            }
        }
        if !placed {
            content.insert_str(0, &self.selection());
        }
        Ok(content)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let existing = read_regular(path)?;
        let existing = existing
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .context("loader.conf is not UTF-8")?;
        let content = self.render(existing)?;
        let parent = path.parent().context("missing loader directory")?;
        if existing == Some(content.as_str()) {
            File::open(path)?.sync_all()?;
        } else {
            let mut temp = NamedTempFile::new_in(parent)?;
            temp.write_all(content.as_bytes())?;
            temp.as_file().sync_all()?;
            // On VFAT the renamed file's size is durable only with its inode.
            temp.persist(path)?.sync_all()?;
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

/// Whether systemd-boot waits at its menu indefinitely for this timeout.
///
/// systemd-boot reads `4294967295` as `menu-force`, and `4294967294` as an
/// unset timeout that shows the menu with a countdown of 136 years. A
/// one-shot timeout always shows the menu, and without a countdown for
/// `menu-hidden` or `0`. Values that systemd-boot cannot parse are ignored.
fn timeout_stalls(value: &str, one_shot: bool) -> bool {
    const UNSET: u64 = u32::MAX as u64 - 1;
    match value {
        "menu-force" => true,
        "menu-hidden" | "menu-disabled" => one_shot,
        value => value
            .parse::<u64>()
            .is_ok_and(|seconds| seconds >= UNSET || (one_shot && seconds == 0)),
    }
}

/// The systemd-boot EFI variables that override loader.conf, as strings.
#[derive(Debug, Default)]
pub(super) struct LoaderOverrides {
    pub timeout: Option<String>,
    pub timeout_one_shot: Option<String>,
    pub default: Option<String>,
    pub preferred: Option<String>,
}

impl LoaderOverrides {
    /// Reject firmware variables that would stall the boot menu or select a
    /// different entry than the CPS selection in loader.conf.
    pub fn check(&self, policy: Option<&LoaderPolicy>) -> Result<()> {
        for (name, value, one_shot, clear) in [
            (
                "LoaderConfigTimeout",
                &self.timeout,
                false,
                "bootctl set-timeout ''",
            ),
            (
                "LoaderConfigTimeoutOneShot",
                &self.timeout_one_shot,
                true,
                "bootctl set-timeout-oneshot ''",
            ),
        ] {
            if let Some(value) = value {
                ensure!(
                    !timeout_stalls(value, one_shot),
                    "the EFI variable {name} sets the boot menu timeout to {value:?}, which \
                     overrides {LOADER}, waits at the boot menu indefinitely and stops \
                     unattended trial and fallback boots. Clear it with `{clear}`"
                );
            }
        }
        let (default, preferred) = match policy {
            None => (None, None),
            Some(LoaderPolicy::Automatic) => (Some(AUTO.to_owned()), None),
            Some(LoaderPolicy::Pinned { default, preferred }) => (
                Some(format!("{default}.conf")),
                preferred.as_ref().map(|id| format!("{id}.conf")),
            ),
        };
        for (name, value, expected, command) in [
            (
                "LoaderEntryPreferred",
                &self.preferred,
                preferred,
                "set-preferred",
            ),
            ("LoaderEntryDefault", &self.default, default, "set-default"),
        ] {
            if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
                // systemd-boot compares entry IDs and patterns case-insensitively.
                ensure!(
                    expected.is_some_and(|expected| expected.eq_ignore_ascii_case(value)),
                    "the EFI variable {name} selects {value:?}, which takes precedence over the \
                     CPS selection in {LOADER}. Clear it with `bootctl {command} ''`"
                );
            }
        }
        Ok(())
    }
}

/// Split a loader.conf line the way systemd-boot does, and report the quote
/// that surrounded the value.
///
/// systemd-boot removes one pair of matching double quotes, and since
/// version 260 one pair of matching single quotes as well.
fn loader_line(line: &str) -> Option<(&str, &str, Option<char>)> {
    let line = line.trim_ascii();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line
        .split_once(|c: char| c.is_ascii_whitespace())
        .unwrap_or((line, ""));
    let value = value.trim_ascii();
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote)
            && value.ends_with(quote)
        {
            // A lone quote character unquotes to an empty value.
            let inner = inner.strip_suffix(quote).unwrap_or(inner);
            return Some((key, inner, Some(quote)));
        }
    }
    Some((key, value, None))
}

fn selector(value: &str) -> Result<String> {
    let id = value
        .strip_suffix(".conf")
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
    pub btrfs: BtrfsRecord,
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
    fn parse(options: &str, btrfs: &BtrfsRecord) -> Result<Self> {
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
        ensure!(
            !options.contains('"') && root.strip_prefix("UUID=") == Some(btrfs.fsid.as_str()),
            "Btrfs root UUID differs from the recorded filesystem"
        );
        Ok(Self::Btrfs(btrfs.fsid.clone()))
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
    ensure!(
        field(&description, ENTRY_FORMAT_FIELD)? == Some(ENTRY_FORMAT),
        "unsupported CPS boot entry format for {id}"
    );
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
    let btrfs = BtrfsRecord::parse(&description, options)?
        .with_context(|| format!("boot entry {id} does not record a Btrfs root"))?;
    let root = BootRoot::parse(options, &btrfs)?;
    if matches!(root, BootRoot::Btrfs(_)) && uki.is_none() {
        ensure!(
            boot_field(&description, "initrd")?.is_some_and(|path| !path.is_empty()),
            "booting by Btrfs filesystem UUID requires an initramfs"
        );
    }
    if uki.is_none() {
        let linux = field(&description, LINUX_HASH)?.context("missing boot kernel digest")?;
        validate_digest(linux)?;
        let initrd = field(&description, INITRD_HASH)?;
        ensure!(
            initrd.is_some() == boot_field(&description, "initrd")?.is_some(),
            "incomplete boot artifact digests in entry {id}"
        );
        if let Some(initrd) = initrd {
            validate_digest(initrd)?;
        }
    }
    let parent = field(&description, "# cps-parent ")?;
    ensure!(
        tries.is_none() || parent.is_some(),
        "counted boot entry has no recorded parent"
    );
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
    // Callers that need a selection report the missing `default` line.
    let policy = LoaderPolicy::read(&loader.join("loader.conf"))?;
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
    use super::{
        AUTO, BootRoot, BtrfsRecord, INITRD_HASH, LINUX_HASH, LoaderOverrides, LoaderPolicy,
        OPTIONS, boot_field, check_parent, loader_line, read_entry,
    };

    const BASE: &str = "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2";
    const TRIAL: &str = "cps-61d0a34e-1f51-4cda-963d-cdcd58d5acef-3";

    #[test]
    fn loader_policy_preserves_administrator_lines() -> anyhow::Result<()> {
        // A new file keeps the established layout and defaults.
        let fresh = LoaderPolicy::pinned(BASE).render(None)?;
        assert_eq!(fresh, format!("default {BASE}.conf\n{OPTIONS}"));
        assert_eq!(
            LoaderPolicy::parse(&fresh)?,
            Some(LoaderPolicy::pinned(BASE))
        );

        let edited = format!(
            "# Local settings\ntimeout 5\ndefault {BASE}.conf\nconsole-mode max\neditor yes\n"
        );
        let trial = LoaderPolicy::trial(BASE, TRIAL).render(Some(&edited))?;
        assert_eq!(
            trial,
            format!(
                "# Local settings\ntimeout 5\ndefault {BASE}.conf\npreferred {TRIAL}.conf\nconsole-mode max\neditor yes\n"
            )
        );
        assert_eq!(
            LoaderPolicy::parse(&trial)?,
            Some(LoaderPolicy::trial(BASE, TRIAL))
        );
        // Withdrawing the trial removes only the preferred line.
        assert_eq!(LoaderPolicy::pinned(BASE).render(Some(&trial))?, edited);
        // A file without a selection gains one at the top and keeps its last line.
        assert_eq!(
            LoaderPolicy::Automatic.render(Some("timeout 3"))?,
            "default cps-*.conf\ntimeout 3\n"
        );
        assert_eq!(
            LoaderPolicy::parse("\t default  \"cps-*.conf\" \n")?,
            Some(LoaderPolicy::Automatic)
        );
        assert_eq!(
            LoaderPolicy::parse("timeout 0\n# default other.conf\n")?,
            None
        );
        Ok(())
    }

    #[test]
    fn firmware_variables_cannot_override_the_cps_selection() -> anyhow::Result<()> {
        let pinned = LoaderPolicy::pinned(BASE);
        let trial = LoaderPolicy::trial(BASE, TRIAL);
        let set = |timeout: Option<&str>, one_shot: Option<&str>| LoaderOverrides {
            timeout: timeout.map(str::to_owned),
            timeout_one_shot: one_shot.map(str::to_owned),
            ..LoaderOverrides::default()
        };
        LoaderOverrides::default().check(Some(&trial))?;
        for value in [
            "0",
            "5",
            "menu-hidden",
            "menu-disabled",
            "4294967293",
            "garbage",
        ] {
            set(Some(value), None).check(Some(&pinned))?;
        }
        for value in [
            "menu-force",
            "4294967294",
            "4294967295",
            "18446744073709551615",
        ] {
            let message = set(Some(value), None)
                .check(Some(&pinned))
                .unwrap_err()
                .to_string();
            assert!(message.contains("LoaderConfigTimeout "), "{message}");
            assert!(message.contains("bootctl set-timeout ''"), "{message}");
        }
        // A one-shot timeout forces the menu, so only a countdown boots on.
        set(None, Some("3")).check(Some(&pinned))?;
        for value in [
            "0",
            "menu-hidden",
            "menu-disabled",
            "menu-force",
            "4294967295",
        ] {
            let message = set(None, Some(value))
                .check(Some(&pinned))
                .unwrap_err()
                .to_string();
            assert!(message.contains("LoaderConfigTimeoutOneShot"), "{message}");
        }

        let entry = |default: Option<&str>, preferred: Option<&str>| LoaderOverrides {
            default: default.map(str::to_owned),
            preferred: preferred.map(str::to_owned),
            ..LoaderOverrides::default()
        };
        // Variables that repeat the CPS selection, or are empty, change nothing.
        entry(Some(&format!("{BASE}.conf")), None).check(Some(&pinned))?;
        entry(Some(&format!("{}.conf", BASE.to_uppercase())), None).check(Some(&trial))?;
        entry(None, Some(&format!("{TRIAL}.conf"))).check(Some(&trial))?;
        entry(Some(""), Some("")).check(Some(&pinned))?;
        entry(Some(AUTO), None).check(Some(&LoaderPolicy::Automatic))?;
        let message = entry(Some("arch.conf"), None)
            .check(Some(&trial))
            .unwrap_err()
            .to_string();
        assert!(message.contains("LoaderEntryDefault"), "{message}");
        assert!(message.contains("bootctl set-default ''"), "{message}");
        let message = entry(None, Some(&format!("{BASE}.conf")))
            .check(Some(&pinned))
            .unwrap_err()
            .to_string();
        assert!(message.contains("bootctl set-preferred ''"), "{message}");
        assert!(entry(Some("@saved"), None).check(Some(&pinned)).is_err());
        assert!(
            entry(Some(&format!("{BASE}.conf")), None)
                .check(None)
                .is_err()
        );
        // loader.conf numbers that systemd-boot reads as menu-force stall as well.
        assert!(
            LoaderPolicy::parse(&format!("timeout 4294967295\ndefault {BASE}.conf\n")).is_err()
        );
        Ok(())
    }

    #[test]
    fn loader_lines_unquote_like_systemd_boot() {
        assert_eq!(
            loader_line("  default \"a.conf\" "),
            Some(("default", "a.conf", Some('"')))
        );
        assert_eq!(
            loader_line("timeout 'menu-force'"),
            Some(("timeout", "menu-force", Some('\'')))
        );
        // Only one matching pair is removed.
        assert_eq!(
            loader_line("default \"'a.conf'\""),
            Some(("default", "'a.conf'", Some('"')))
        );
        assert_eq!(
            loader_line("default \"a.conf'"),
            Some(("default", "\"a.conf'", None))
        );
        assert_eq!(loader_line("default \""), Some(("default", "", Some('"'))));
        assert_eq!(loader_line("editor"), Some(("editor", "", None)));
        assert_eq!(loader_line("  # default a.conf"), None);
        // A quoted menu-force is rejected as well.
        assert!(
            LoaderPolicy::parse(&format!("timeout 'menu-force'\ndefault {BASE}.conf\n")).is_err()
        );
    }

    #[test]
    fn loader_policy_names_the_conflicting_key_and_its_fix() {
        let error = |text: &str| format!("{:#}", LoaderPolicy::parse(text).unwrap_err());
        let message = error("default arch.conf\n");
        assert!(message.contains("`default arch.conf`"), "{message}");
        assert!(message.contains("default <CPS entry ID>.conf"), "{message}");
        assert!(message.contains("cps boot rollback"), "{message}");
        let message = error(&format!("default {BASE}.conf\npreferred @saved\n"));
        assert!(message.contains("`preferred @saved`"), "{message}");
        let message = error(&format!("default {BASE}.conf\ndefault {TRIAL}.conf\n"));
        assert!(message.contains("`default` more than once"), "{message}");
        let message = error(&format!("timeout menu-force\ndefault {BASE}.conf\n"));
        assert!(message.contains("`timeout menu-force`"), "{message}");
        assert!(message.contains("menu-hidden"), "{message}");
        let message = error(&format!("preferred {TRIAL}.conf\n"));
        assert!(message.contains("without a CPS `default`"), "{message}");
        let message = error(&format!("default cps-*.conf\npreferred {TRIAL}.conf\n"));
        assert!(message.contains("automatic mode"), "{message}");
        assert!(
            LoaderPolicy::parse(&format!("default {BASE}.conf\npreferred {BASE}.conf\n")).is_err()
        );
        // Older systemd-boot reads single quotes literally.
        let message = error(&format!("default '{BASE}.conf'\n"));
        assert!(message.contains("single-quoted"), "{message}");
        let message = error(&format!("default {BASE}.conf\npreferred '{TRIAL}.conf'\n"));
        assert!(message.contains("`preferred`"), "{message}");
        // Rendering never overwrites a conflicting selection.
        assert!(
            LoaderPolicy::pinned(BASE)
                .render(Some("default arch.conf\n"))
                .is_err()
        );
    }

    #[test]
    fn loader_policy_writes_through_a_temporary_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("loader.conf");
        LoaderPolicy::Automatic.write(&path)?;
        assert_eq!(LoaderPolicy::read(&path)?, Some(LoaderPolicy::Automatic));
        std::fs::write(&path, "default cps-*.conf\nbeep yes\n")?;
        LoaderPolicy::trial(BASE, TRIAL).write(&path)?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            format!("default {BASE}.conf\npreferred {TRIAL}.conf\nbeep yes\n")
        );
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

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
        let header = format!(
            "title Canopa\nsort-key cps\nversion 2\n# cps-boot-entry-version 1\n# cps-parent cps-51c2d73c-f68f-4cc7-98b0-6efbbc03c785-1\n{LINUX_HASH}{}\n{INITRD_HASH}{}\n",
            "a".repeat(64),
            "b".repeat(64),
        );
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
            entry.replace(&format!("{LINUX_HASH}{}\n", "a".repeat(64)), ""),
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
            "title Canopa\nsort-key cps\nversion 2\n# cps-boot-entry-version 1\n# cps-parent cps-51c2d73c-f68f-4cc7-98b0-6efbbc03c785-1\nuki /EFI/cps/{id}/uki.efi\n{}{}\n{}root=UUID={} rootfstype=btrfs rootflags=subvolid=277 rw\n{}",
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
