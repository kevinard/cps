//! Three-way merge of `/etc` into a pending trial when the running system
//! shuts down.
//!
//! Staging records the new root's `/etc`, before the plan changes it, as the
//! merge base. At shutdown, [`finalize`] compares the running root's `/etc`
//! ("ours") and the trial's ("theirs") with that base. It builds the merged
//! tree beside the trial's `/etc` and swaps it in with one `RENAME_EXCHANGE`,
//! so a crash leaves the trial's `/etc` either untouched or fully merged.
//!
//! The merge is idempotent: running it again with the same inputs changes
//! nothing, and after a merge the recorded base becomes the running `/etc`
//! it merged, so a later run carries only the edits made since.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Write as _},
    os::unix::fs::PermissionsExt,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat, Timespec, Timestamps,
        XattrFlags, chmodat, chownat, fchmod, fchown, fstat, fstatfs, futimens, ioctl_ficlone,
        lgetxattr, linkat, llistxattr, lremovexattr, lsetxattr, mkdirat, mknodat, openat, openat2,
        readlinkat, renameat_with, statat, symlinkat, syncfs, unlinkat, utimensat,
    },
    io::Errno,
    process::{Gid, Uid},
};
use serde::{Deserialize, Serialize};

use crate::{
    image::{CONFIG_UPDATES_DIRECTORY, Image, Managed, root::Root},
    model::{Node, digest},
    plan::config_update_prefix,
};

/// The merge base in the trial's image state, written at staging.
pub(crate) const BASE: &str = "etc-base.json";
/// The report of the last merge into this root, in its image state.
pub(crate) const REPORT: &str = "etc-merge.json";
/// Private working directory in the trial's image state.
const SCRATCH: &str = "etc-merge";
/// Local versions saved by the merge, below the configuration review area.
pub(crate) const STASH: &str = "etc-merge";
const ETC: &str = "etc";
const FORMAT: u32 = 1;
/// Account databases merged record by record, keyed by their first field.
const RECORDS: [&str; 6] = [
    "etc/passwd",
    "etc/group",
    "etc/shadow",
    "etc/gshadow",
    "etc/subuid",
    "etc/subgid",
];

/// One path below `/etc`. Timestamps are not compared.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Entry {
    File {
        mode: u32,
        uid: u32,
        gid: u32,
        size: u64,
        sha256: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        xattrs: BTreeMap<String, String>,
    },
    Dir {
        mode: u32,
        uid: u32,
        gid: u32,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        xattrs: BTreeMap<String, String>,
    },
    Symlink {
        target: String,
        uid: u32,
        gid: u32,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        xattrs: BTreeMap<String, String>,
    },
    /// A FIFO, socket or device node. `mode` includes the file type.
    Special {
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        xattrs: BTreeMap<String, String>,
    },
}

impl Entry {
    fn xattrs(&self) -> &BTreeMap<String, String> {
        match self {
            Self::File { xattrs, .. }
            | Self::Dir { xattrs, .. }
            | Self::Symlink { xattrs, .. }
            | Self::Special { xattrs, .. } => xattrs,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self, Self::Dir { .. })
    }

    /// The mode, owner and attributes, without the contents.
    fn metadata(&self) -> Option<(u32, u32, u32, &BTreeMap<String, String>)> {
        match self {
            Self::File {
                mode,
                uid,
                gid,
                xattrs,
                ..
            } => Some((*mode, *uid, *gid, xattrs)),
            _ => None,
        }
    }
}

/// A recorded `/etc`: the merge base, or the result of a scan.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    format: u32,
    entries: BTreeMap<String, Entry>,
    /// Mount points and nested subvolumes, which the merge leaves alone.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    opaque: BTreeSet<String>,
    /// The contents of the account databases, for the record merge.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    records: BTreeMap<String, String>,
    /// In a merge base: the stash directories of completed merges, which a
    /// later merge into the same trial must keep.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    stashes: BTreeSet<String>,
}

impl Manifest {
    fn record(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.records
            .get(path)
            .map(|text| {
                STANDARD
                    .decode(text)
                    .context("invalid recorded account database")
            })
            .transpose()
    }
}

/// A scan, with the inode of each regular file for hard link groups.
struct Scan {
    manifest: Manifest,
    inodes: BTreeMap<String, (u64, u64)>,
}

/// What the merge does with one path that the administrator changed.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Action {
    /// Copy the running version into the trial, or remove the path.
    Take,
    /// Keep the local edit of a preserved file, and offer the trial's
    /// version for review as a pending configuration update.
    ConfigUpdate,
    /// Write account databases merged record by record.
    Records {
        content: Vec<u8>,
        /// Keep the running file's metadata, which only it changed.
        running_metadata: bool,
    },
    /// Keep the trial's version and save the running one for review.
    Conflict(Reason),
    /// Keep the trial's version without saving anything.
    Keep(Reason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reason {
    BothChanged,
    Drift,
    RemovedManaged,
    Generated,
    ParentRemoved,
    NotEmpty,
    Opaque,
}

impl Reason {
    fn describe(self) -> &'static str {
        match self {
            Self::BothChanged => "changed both on the running system and by the update",
            Self::Drift => {
                "differs from its package, and only configuration marked preserve is carried \
                 into a new environment"
            }
            Self::RemovedManaged => "removed on the running system but installed by a package",
            Self::Generated => "generated by a trigger in the new environment",
            Self::ParentRemoved => "its directory was removed or replaced by the update",
            Self::NotEmpty => "removed on the running system, but the update added files to it",
            Self::Opaque => "a mount point or nested subvolume",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EtcConflict {
    pub path: String,
    pub reason: String,
    /// The saved running version, relative to the root, if there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<String>,
}

/// What a merge carried into the trial.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EtcMergeReport {
    pub revision: u64,
    /// Paths copied from, or removed as on, the running system.
    pub applied: Vec<String>,
    /// Account databases merged record by record.
    pub merged: Vec<String>,
    /// Preserved files that keep the local edit, with the update's version
    /// pending review.
    pub config_updates: Vec<String>,
    /// Paths that keep the trial's version.
    pub conflicts: Vec<EtcConflict>,
}

impl EtcMergeReport {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
            && self.merged.is_empty()
            && self.config_updates.is_empty()
            && self.conflicts.is_empty()
    }
}

/// The trial's package state that decides what the merge may change.
pub(crate) struct Policy<'a> {
    pub paths: &'a BTreeMap<String, Managed>,
    pub generated: Vec<&'a str>,
}

/// Record the merge base in a new root before its plan changes `/etc`.
///
/// It also removes merge leftovers that the new root inherited.
pub(crate) fn record_base(root: &Root) -> Result<()> {
    let state = root.open_image_state()?;
    remove_tree(&state, SCRATCH)?;
    match unlinkat(state.as_file(), REPORT, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(error) => return Err(error).context("removing an inherited /etc merge report"),
    }
    let scan = scan(root, true).context("recording /etc as the merge base")?;
    write_private(&state, BASE, &serde_json::to_vec(&scan.manifest)?)
}

/// Remove the scratch tree that an interrupted merge left in a root.
pub(crate) fn remove_leftovers(root: &Root) -> Result<()> {
    remove_tree(&root.open_image_state()?, SCRATCH)
}

/// The stash directory for this merge: `REVISION`, or `REVISION-N` when an
/// earlier, completed merge into the same trial revision saved versions
/// already, so a later run never replaces them. A retry of an interrupted
/// run gets the same name, since its stash is not recorded as completed.
fn stash_name(completed: &BTreeSet<String>, revision: u64) -> String {
    (1u32..)
        .map(|attempt| match attempt {
            1 => revision.to_string(),
            n => format!("{revision}-{n}"),
        })
        .find(|name| !completed.contains(name))
        .expect("the stash names are unbounded")
}

/// The report of the last merge into a root.
pub fn last_report(root: &Path) -> Result<Option<EtcMergeReport>> {
    let state = Root::open(root)?.open_image_state()?;
    match state.read_file(REPORT) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The steps of [`finalize`], for interruption tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    Built,
    Recorded,
    Stashed,
    Exchanged,
    Rebased,
}

/// Merge the running `/etc` into the trial whose image is open exclusively.
///
/// Returns `None` when the trial has no merge base.
pub(crate) fn finalize(trial: &Image, running: &Root) -> Result<Option<EtcMergeReport>> {
    finalize_with(trial, running, |_| Ok(()))
}

pub(crate) fn finalize_with(
    trial: &Image,
    running: &Root,
    mut step: impl FnMut(Step) -> Result<()>,
) -> Result<Option<EtcMergeReport>> {
    trial.require_clean()?;
    let state_dir = &trial.state_dir;
    // Whatever an earlier attempt left here is either an unfinished merged
    // tree or the replaced /etc. Neither is needed.
    remove_tree(state_dir, SCRATCH)?;
    let base: Manifest = match state_dir.read_file(BASE) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading the /etc merge base")?,
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error).context("reading the /etc merge base"),
    };
    ensure!(base.format == FORMAT, "unsupported /etc merge base format");
    let state = trial.state()?;
    let revision = state.settings.revision;
    let policy = Policy {
        paths: &state.paths,
        generated: crate::triggers::active_outputs(&state.installed, &state.paths),
    };
    let ours = scan(running, true).context("reading the running /etc")?;
    ensure!(
        ours.manifest.entries.contains_key(ETC),
        "the running root has no /etc"
    );
    let theirs = scan(&trial.root, true).context("reading the trial's /etc")?;
    let actions = plan(&base, &ours.manifest, &theirs.manifest, &policy)?;
    let mut report = EtcMergeReport {
        revision,
        ..EtcMergeReport::default()
    };
    if actions.is_empty() {
        write_private(state_dir, REPORT, &serde_json::to_vec_pretty(&report)?)?;
        return Ok(Some(report));
    }
    let stash_name = stash_name(&base.stashes, revision);
    let scratch = state_dir.ensure_dir(SCRATCH)?;
    let changes_tree = actions.values().any(|action| {
        matches!(
            action,
            Action::Take | Action::ConfigUpdate | Action::Records { .. }
        )
    });
    let mut builder = Builder {
        trial: &trial.root,
        running,
        scratch: &scratch,
        ours: &ours,
        theirs: &theirs.manifest,
        links: BTreeMap::new(),
        dir_owner: (state.settings.uid, state.settings.gid),
        stash_prefix: format!("{CONFIG_UPDATES_DIRECTORY}/{STASH}/{stash_name}"),
        report: &mut report,
    };
    if changes_tree {
        builder.clone_theirs(&theirs)?;
    }
    let config_updates = builder.apply(&actions, &policy, revision)?;
    syncfs(trial.root.as_file()).context("flushing the merged /etc")?;
    step(Step::Built)?;

    // The steps before the exchange are correct with either /etc. A pending
    // update whose vendor copy matches the trial's file is resolved either way.
    trial.add_config_updates(&config_updates)?;
    step(Step::Recorded)?;
    if statat(scratch.as_file(), "stash", AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        let parent = ensure_dirs(
            &trial.root,
            &format!("{CONFIG_UPDATES_DIRECTORY}/{STASH}"),
            (state.settings.uid, state.settings.gid),
            0o755,
        )?;
        // A stash that an interrupted run of this merge saved is replaced.
        remove_entry_tree(&parent, &stash_name)?;
        renameat_with(
            scratch.as_file(),
            "stash",
            &parent,
            stash_name.as_str(),
            RenameFlags::NOREPLACE,
        )
        .context("saving the running versions of conflicting paths")?;
        parent.sync_all()?;
        scratch.as_file().sync_all()?;
    }
    step(Step::Stashed)?;

    if changes_tree {
        let flags = if statat(trial.root.as_file(), ETC, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
            RenameFlags::EXCHANGE
        } else {
            RenameFlags::NOREPLACE
        };
        renameat_with(scratch.as_file(), ETC, trial.root.as_file(), ETC, flags)
            .context("exchanging the trial's /etc with the merged tree")?;
        trial.root.as_file().sync_all()?;
        scratch.as_file().sync_all()?;
    }
    step(Step::Exchanged)?;

    // Later runs only need the edits made after this one, and keep the
    // versions this one saved.
    let mut rebased = ours.manifest;
    rebased.stashes = base.stashes;
    if report
        .conflicts
        .iter()
        .any(|conflict| conflict.saved.is_some())
    {
        rebased.stashes.insert(stash_name);
    }
    write_private(state_dir, BASE, &serde_json::to_vec(&rebased)?)?;
    write_private(state_dir, REPORT, &serde_json::to_vec_pretty(&report)?)?;
    step(Step::Rebased)?;
    drop(scratch);
    remove_tree(state_dir, SCRATCH)?;
    syncfs(trial.root.as_file())?;
    Ok(Some(report))
}

/// Decide what to do with each path the administrator changed since the base.
fn plan(
    base: &Manifest,
    ours: &Manifest,
    theirs: &Manifest,
    policy: &Policy<'_>,
) -> Result<BTreeMap<String, Action>> {
    let opaque: Vec<&String> = base
        .opaque
        .iter()
        .chain(&ours.opaque)
        .chain(&theirs.opaque)
        .collect();
    let paths: BTreeSet<&String> = base
        .entries
        .keys()
        .chain(ours.entries.keys())
        .chain(theirs.entries.keys())
        .collect();
    let mut actions = BTreeMap::new();
    for path in paths {
        let (b, o, t) = (
            base.entries.get(path),
            ours.entries.get(path),
            theirs.entries.get(path),
        );
        if o == b || o == t {
            continue;
        }
        if opaque.contains(&path) {
            actions.insert(path.clone(), Action::Keep(Reason::Opaque));
            continue;
        }
        if opaque
            .iter()
            .any(|top| path.starts_with(&format!("{top}/")))
        {
            continue;
        }
        let action = if crate::triggers::generated(&policy.generated, path) {
            Action::Keep(Reason::Generated)
        } else if let Some(managed) = policy.paths.get(path.as_str()) {
            if !acceptable(o, managed) {
                if o.is_some() {
                    Action::Conflict(Reason::Drift)
                } else {
                    Action::Keep(Reason::RemovedManaged)
                }
            } else if t == b {
                Action::Take
            } else if let Some((content, running_metadata)) =
                merge_record_file(path, base, ours, theirs)?
            {
                Action::Records {
                    content,
                    running_metadata,
                }
            } else if managed.preserve && matches_node(t, &managed.node) {
                Action::ConfigUpdate
            } else {
                Action::Conflict(Reason::BothChanged)
            }
        } else if t == b {
            Action::Take
        } else if let Some((content, running_metadata)) =
            merge_record_file(path, base, ours, theirs)?
        {
            Action::Records {
                content,
                running_metadata,
            }
        } else {
            Action::Conflict(Reason::BothChanged)
        };
        actions.insert(path.clone(), action);
    }
    Ok(actions)
}

/// Whether the trial's package state accepts the running version of a managed path.
///
/// Preserved files accept any contents with the manifest's metadata. A
/// directory that packages only create implicitly accepts any metadata, as
/// verification does. Other paths must match their manifest exactly.
fn acceptable(ours: Option<&Entry>, managed: &Managed) -> bool {
    match (ours, &managed.node) {
        (Some(Entry::Dir { .. }), _) if managed.implicit => true,
        (
            Some(Entry::File {
                mode,
                uid,
                gid,
                xattrs,
                ..
            }),
            Node::File {
                mode: expected_mode,
                uid: expected_uid,
                gid: expected_gid,
                capabilities: None,
                ..
            },
        ) if managed.preserve => {
            (*mode, *uid, *gid) == (*expected_mode, *expected_uid, *expected_gid)
                && !xattrs.contains_key("security.capability")
        }
        (Some(entry), node) => matches_node(Some(entry), node),
        (None, _) => false,
    }
}

fn matches_node(entry: Option<&Entry>, node: &Node) -> bool {
    match (entry, node) {
        (
            Some(Entry::File {
                mode,
                uid,
                gid,
                size,
                sha256,
                xattrs,
            }),
            Node::File {
                mode: m,
                uid: u,
                gid: g,
                size: s,
                sha256: h,
                capabilities: None,
            },
        ) => {
            (mode, uid, gid, size, sha256) == (m, u, g, s, h)
                && !xattrs.contains_key("security.capability")
        }
        (
            Some(Entry::Dir { mode, uid, gid, .. }),
            Node::Dir {
                mode: m,
                uid: u,
                gid: g,
            },
        ) => (mode, uid, gid) == (m, u, g),
        (
            Some(Entry::Symlink {
                target, uid, gid, ..
            }),
            Node::Symlink {
                target: t,
                uid: u,
                gid: g,
            },
        ) => (target, uid, gid) == (t, u, g),
        _ => false,
    }
}

/// Merge an account database changed on both sides, if its records allow it.
///
/// Returns the merged contents, and whether the running file's metadata
/// replaces the trial's because only the running system changed it.
fn merge_record_file(
    path: &str,
    base: &Manifest,
    ours: &Manifest,
    theirs: &Manifest,
) -> Result<Option<(Vec<u8>, bool)>> {
    if !RECORDS.contains(&path) {
        return Ok(None);
    }
    let (Some(o), Some(t)) = (ours.entries.get(path), theirs.entries.get(path)) else {
        return Ok(None);
    };
    let (Some(o_meta), Some(t_meta)) = (o.metadata(), t.metadata()) else {
        return Ok(None);
    };
    // Only the contents merge. Metadata changed on both sides is a conflict.
    let b_meta = base.entries.get(path).and_then(Entry::metadata);
    if o_meta != t_meta && b_meta != Some(o_meta) && b_meta != Some(t_meta) {
        return Ok(None);
    }
    let running_metadata = o_meta != t_meta && b_meta == Some(t_meta);
    let base_text = base.record(path)?.unwrap_or_default();
    let (Some(ours_text), Some(theirs_text)) = (ours.record(path)?, theirs.record(path)?) else {
        return Ok(None);
    };
    let Some(merged) = merge_records(&base_text, &ours_text, &theirs_text) else {
        return Ok(None);
    };
    // Accounts that each side added on its own can take the same free ID.
    if matches!(path, "etc/passwd" | "etc/group") && !unique_ids(&merged) {
        return Ok(None);
    }
    Ok(Some((merged, running_metadata)))
}

/// Whether no two records share the numeric ID in their third field.
fn unique_ids(text: &[u8]) -> bool {
    let mut ids = BTreeSet::new();
    text.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .all(|line| {
            line.split(|byte| *byte == b':')
                .nth(2)
                .is_some_and(|id| ids.insert(id))
        })
}

type Records<'a> = Vec<(&'a [u8], &'a [u8])>;

fn parse_records(text: &[u8]) -> Option<Records<'_>> {
    let mut records = Vec::new();
    let mut keys = BTreeSet::new();
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    if body.is_empty() {
        return Some(records);
    }
    for line in body.split(|byte| *byte == b'\n') {
        let key = line.split(|byte| *byte == b':').next()?;
        if key.is_empty() || key.len() == line.len() || !keys.insert(key) {
            return None;
        }
        records.push((key, line));
    }
    Some(records)
}

/// Merge colon-separated account records keyed by their first field.
///
/// Each record takes the side that changed it. The trial's order is kept and
/// records only the running system added are appended in their order. A
/// record changed differently on both sides, a duplicate key or a line
/// without a key makes the whole file a conflict.
fn merge_records(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let base: BTreeMap<_, _> = parse_records(base)?.into_iter().collect();
    let ours_records = parse_records(ours)?;
    let ours: BTreeMap<_, _> = ours_records.iter().copied().collect();
    let theirs_records = parse_records(theirs)?;
    let theirs_keys: BTreeSet<_> = theirs_records.iter().map(|(key, _)| *key).collect();
    let mut lines = Vec::new();
    for (key, line) in &theirs_records {
        let (b, o) = (base.get(key), ours.get(key));
        if o == b || o == Some(line) {
            lines.push(*line);
        } else if b == Some(line) {
            lines.extend(o);
        } else {
            return None;
        }
    }
    for (key, line) in &ours_records {
        if theirs_keys.contains(key) {
            continue;
        }
        match base.get(key) {
            // The trial removed a record that the running system kept as it was.
            Some(b) if b == line => {}
            Some(_) => return None,
            None => lines.push(*line),
        }
    }
    let mut merged = lines.join(&b'\n');
    if !merged.is_empty() {
        merged.push(b'\n');
    }
    Some(merged)
}

struct Builder<'a> {
    trial: &'a Root,
    running: &'a Root,
    scratch: &'a Root,
    ours: &'a Scan,
    theirs: &'a Manifest,
    /// Merged paths written for each source inode, to recreate hard links.
    links: BTreeMap<(bool, u64), String>,
    dir_owner: (u32, u32),
    stash_prefix: String,
    report: &'a mut EtcMergeReport,
}

impl Builder<'_> {
    /// Copy the trial's `/etc` into the scratch directory. File contents are
    /// cloned where the filesystem supports it.
    fn clone_theirs(&mut self, theirs: &Scan) -> Result<()> {
        ensure!(
            theirs.manifest.opaque.is_empty(),
            "the trial's /etc contains a mount point or nested subvolume at {}",
            theirs.manifest.opaque.first().map_or("", String::as_str)
        );
        let mut inside = BTreeMap::<u64, u64>::new();
        for (ino, _) in theirs.inodes.values() {
            *inside.entry(*ino).or_default() += 1;
        }
        for path in theirs.manifest.entries.keys() {
            let (source, leaf) = parent(self.trial.as_file(), path)?
                .with_context(|| format!("{path} disappeared from the trial"))?;
            let (target, _) = parent(self.scratch.as_file(), path)?
                .with_context(|| format!("missing merged directory for {path}"))?;
            let inode = theirs.inodes.get(path).copied();
            if let Some((ino, links)) = inode
                && links > inside[&ino]
            {
                // A hard link to a file outside /etc must stay one. The
                // replaced /etc keeps its extra link until it is removed.
                linkat(&source, leaf, &target, leaf, AtFlags::empty())
                    .with_context(|| format!("linking the trial's {path}"))?;
                continue;
            }
            self.copy(
                &source,
                leaf,
                &target,
                path,
                inode.map(|(ino, links)| (false, ino, links)),
            )
            .with_context(|| format!("copying the trial's {path}"))?;
        }
        Ok(())
    }

    fn apply(
        &mut self,
        actions: &BTreeMap<String, Action>,
        policy: &Policy<'_>,
        revision: u64,
    ) -> Result<Vec<String>> {
        let mut config_updates = Vec::new();
        let mut conflicts = BTreeMap::new();
        // Removals, and directories that the running system replaced, go
        // deepest first, so that emptied directories can go too.
        for (path, action) in actions.iter().rev() {
            let ours = self.ours.manifest.entries.get(path);
            let replaced_dir = ours.is_some_and(|entry| !entry.is_dir())
                && self.theirs.entries.get(path).is_some_and(Entry::is_dir);
            if *action == Action::Take
                && (ours.is_none() || replaced_dir)
                && !self.remove_merged(path)?
            {
                conflicts.insert(path.clone(), Reason::NotEmpty);
            }
        }
        for (path, action) in actions {
            if let Some(reason) = conflicts.get(path) {
                self.conflict(path, *reason)?;
                continue;
            }
            match action {
                Action::Take => match self.ours.manifest.entries.get(path) {
                    None => {
                        if !self.remove_merged(path)? {
                            self.conflict(path, Reason::NotEmpty)?;
                            continue;
                        }
                        self.report.applied.push(path.clone());
                    }
                    Some(_) => {
                        if self.take_ours(path)? {
                            self.report.applied.push(path.clone());
                        } else {
                            self.conflict(path, Reason::ParentRemoved)?;
                        }
                    }
                },
                Action::ConfigUpdate => {
                    let node = &policy.paths[path.as_str()].node;
                    if !self.take_ours(path)? {
                        self.conflict(path, Reason::ParentRemoved)?;
                        continue;
                    }
                    self.vendor_copy(path, node, revision)?;
                    self.report.config_updates.push(path.clone());
                    config_updates.push(path.clone());
                }
                Action::Records {
                    content,
                    running_metadata,
                } => {
                    self.write_records(path, content, *running_metadata)?;
                    self.report.merged.push(path.clone());
                }
                Action::Conflict(reason) => self.conflict(path, *reason)?,
                Action::Keep(reason) => self.report.conflicts.push(EtcConflict {
                    path: path.clone(),
                    reason: reason.describe().into(),
                    saved: None,
                }),
            }
        }
        Ok(config_updates)
    }

    /// Remove a path from the merged tree. Returns false for a nonempty directory.
    fn remove_merged(&self, path: &str) -> Result<bool> {
        let Some((directory, leaf)) = parent(self.scratch.as_file(), path)? else {
            return Ok(true);
        };
        match statat(&directory, leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => Ok(true),
            Err(error) => Err(error).with_context(|| format!("inspecting merged {path}")),
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => {
                match unlinkat(&directory, leaf, AtFlags::REMOVEDIR) {
                    Ok(()) => Ok(true),
                    Err(Errno::NOTEMPTY | Errno::EXIST) => Ok(false),
                    Err(error) => Err(error).with_context(|| format!("removing merged {path}")),
                }
            }
            Ok(_) => {
                unlinkat(&directory, leaf, AtFlags::empty())
                    .with_context(|| format!("removing merged {path}"))?;
                Ok(true)
            }
        }
    }

    /// Put the running version of `path` into the merged tree. Returns false
    /// when its directory is missing there or a nonempty directory is in the way.
    fn take_ours(&mut self, path: &str) -> Result<bool> {
        let entry = &self.ours.manifest.entries[path];
        let Some((target, leaf)) = parent(self.scratch.as_file(), path)? else {
            return Ok(false);
        };
        if !target.metadata()?.is_dir() {
            return Ok(false);
        }
        if let Ok(stat) = statat(&target, leaf, AtFlags::SYMLINK_NOFOLLOW) {
            if entry.is_dir() && FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                let (source, name) = parent(self.running.as_file(), path)?
                    .with_context(|| format!("{path} disappeared from the running system"))?;
                let directory = open_dir(&target, leaf)?;
                let info = statat(&source, name, AtFlags::SYMLINK_NOFOLLOW)?;
                set_metadata(&directory, &info)?;
                copy_xattrs(&source, name, &target, leaf, path)?;
                return Ok(true);
            }
            if !self.remove_merged(path)? {
                return Ok(false);
            }
        }
        let (source, name) = parent(self.running.as_file(), path)?
            .with_context(|| format!("{path} disappeared from the running system"))?;
        let inode = self
            .ours
            .inodes
            .get(path)
            .map(|(ino, links)| (true, *ino, *links));
        self.copy(&source, name, &target, path, inode)
            .with_context(|| format!("copying the running {path}"))?;
        Ok(true)
    }

    /// Copy one entry. A regular file with other links in the same tree is
    /// linked to the copy already made.
    fn copy(
        &mut self,
        source: &File,
        name: &str,
        target: &File,
        path: &str,
        inode: Option<(bool, u64, u64)>,
    ) -> Result<()> {
        let leaf = leaf(path);
        if let Some((side, ino, links)) = inode
            && links > 1
        {
            if let Some(first) = self.links.get(&(side, ino)) {
                linkat(
                    self.scratch.as_file(),
                    first.as_str(),
                    target,
                    leaf,
                    AtFlags::empty(),
                )
                .with_context(|| format!("linking {path} to {first}"))?;
                return Ok(());
            }
            self.links.insert((side, ino), path.to_owned());
        }
        copy_entry(source, name, target, leaf, path)
    }

    fn conflict(&mut self, path: &str, reason: Reason) -> Result<()> {
        let mut saved = None;
        if let Some(entry) = self.ours.manifest.entries.get(path)
            && !entry.is_dir()
        {
            let stash = format!("stash/{}", path);
            let (directory, leaf) = stash.rsplit_once('/').expect("stash path has a parent");
            let target = ensure_dirs(self.scratch, directory, self.dir_owner, 0o700)?;
            let (source, name) = parent(self.running.as_file(), path)?
                .with_context(|| format!("{path} disappeared from the running system"))?;
            remove_entry_tree(&target, leaf)?;
            copy_entry(&source, name, &target, leaf, path)
                .with_context(|| format!("saving the running {path}"))?;
            saved = Some(format!("{}/{path}", self.stash_prefix));
        }
        self.report.conflicts.push(EtcConflict {
            path: path.to_owned(),
            reason: reason.describe().into(),
            saved,
        });
        Ok(())
    }

    /// Clone the trial's version of a preserved file as its pending vendor copy.
    fn vendor_copy(&self, path: &str, node: &Node, revision: u64) -> Result<()> {
        let copy = format!("{}{path}", config_update_prefix(revision));
        let (directory, leaf) = copy.rsplit_once('/').expect("vendor copy has a parent");
        if self.trial.inspect(&copy).ok().flatten().as_ref() == Some(node) {
            return Ok(());
        }
        let target = ensure_dirs(self.trial, directory, self.dir_owner, 0o755)?;
        remove_entry_tree(&target, leaf)?;
        let (source, name) = parent(self.trial.as_file(), path)?
            .with_context(|| format!("{path} disappeared from the trial"))?;
        copy_entry(&source, name, &target, leaf, path)?;
        ensure!(
            self.trial.inspect(&copy)?.as_ref() == Some(node),
            "the trial's {path} differs from its manifest"
        );
        Ok(())
    }

    /// Replace a merged file with merged account records, keeping the
    /// metadata the merge selected.
    fn write_records(&self, path: &str, content: &[u8], running_metadata: bool) -> Result<()> {
        let picked = if running_metadata {
            self.ours.manifest.entries.get(path)
        } else {
            self.theirs.entries.get(path)
        };
        let Some(picked @ Entry::File { mode, uid, gid, .. }) = picked else {
            bail!("expected regular files at {path}");
        };
        let (target, leaf) = parent(self.scratch.as_file(), path)?
            .with_context(|| format!("missing merged directory for {path}"))?;
        unlinkat(&target, leaf, AtFlags::empty())
            .with_context(|| format!("replacing merged {path}"))?;
        let file = File::from(openat(
            &target,
            leaf,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        (&file).write_all(content)?;
        fchown(&file, Some(Uid::from_raw(*uid)), Some(Gid::from_raw(*gid)))?;
        fchmod(&file, Mode::from_raw_mode(*mode))?;
        // The attributes, such as a security label, come from the same side
        // as the rest of the metadata.
        let side = if running_metadata {
            self.running
        } else {
            self.trial
        };
        let (source, name) = parent(side.as_file(), path)?
            .with_context(|| format!("{path} disappeared during the merge"))?;
        if !picked.xattrs().is_empty() {
            copy_xattrs(&source, name, &target, leaf, path)?;
        }
        file.sync_all()?;
        Ok(())
    }
}

/// Read the `/etc` of a root. A missing `/etc` reads as empty.
fn scan(root: &Root, records: bool) -> Result<Scan> {
    let mut scan = Scan {
        manifest: Manifest {
            format: FORMAT,
            ..Manifest::default()
        },
        inodes: BTreeMap::new(),
    };
    let top = match openat(
        root.as_file(),
        ETC,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(top) => File::from(top),
        Err(Errno::NOENT) => return Ok(scan),
        Err(error) => return Err(error).context("opening /etc"),
    };
    let info = fstat(&top)?;
    let btrfs = fstatfs(&top)?.f_type == libc::BTRFS_SUPER_MAGIC;
    let xattrs = xattr_digests(&Root::fd_path(&top))?;
    scan.manifest.entries.insert(
        ETC.into(),
        Entry::Dir {
            mode: info.st_mode & 0o7777,
            uid: info.st_uid,
            gid: info.st_gid,
            xattrs,
        },
    );
    visit(&top, ETC, info.st_dev, btrfs, records, &mut scan)?;
    Ok(scan)
}

fn visit(
    directory: &File,
    prefix: &str,
    device: u64,
    btrfs: bool,
    records: bool,
    scan: &mut Scan,
) -> Result<()> {
    let mut names = Vec::new();
    for entry in fs::read_dir(Root::fd_path(directory))? {
        let name = entry?.file_name();
        let name = name
            .to_str()
            .with_context(|| format!("non-UTF-8 file name in {prefix}: {name:?}"))?
            .to_owned();
        names.push(name);
    }
    names.sort();
    for name in names {
        let path = format!("{prefix}/{name}");
        let info = statat(directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .with_context(|| format!("inspecting {path}"))?;
        let entry_path = Root::fd_path(directory).join(&name);
        let (mode, uid, gid) = (info.st_mode & 0o7777, info.st_uid, info.st_gid);
        let entry = match FileType::from_raw_mode(info.st_mode) {
            FileType::Directory => {
                if info.st_dev != device || (btrfs && matches!(info.st_ino, 2 | 256)) {
                    scan.manifest.opaque.insert(path);
                    continue;
                }
                let child = open_dir(directory, &name)?;
                let xattrs = xattr_digests(&entry_path)?;
                scan.manifest.entries.insert(
                    path.clone(),
                    Entry::Dir {
                        mode,
                        uid,
                        gid,
                        xattrs,
                    },
                );
                visit(&child, &path, device, btrfs, records, scan)?;
                continue;
            }
            FileType::RegularFile => {
                let mut file = File::from(
                    openat(
                        directory,
                        name.as_str(),
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .with_context(|| format!("opening {path}"))?,
                );
                let opened = fstat(&file)?;
                ensure!(
                    (opened.st_dev, opened.st_ino) == (info.st_dev, info.st_ino),
                    "{path} changed while reading /etc"
                );
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .with_context(|| format!("reading {path}"))?;
                if records && RECORDS.contains(&path.as_str()) {
                    scan.manifest
                        .records
                        .insert(path.clone(), STANDARD.encode(&bytes));
                }
                scan.inodes
                    .insert(path.clone(), (opened.st_ino, opened.st_nlink as u64));
                Entry::File {
                    mode,
                    uid,
                    gid,
                    size: bytes.len() as u64,
                    sha256: digest(&bytes),
                    xattrs: xattr_digests(&entry_path)?,
                }
            }
            FileType::Symlink => {
                let target = readlinkat(directory, name.as_str(), Vec::new())?;
                Entry::Symlink {
                    target: target
                        .into_string()
                        .with_context(|| format!("non-UTF-8 symlink target at {path}"))?,
                    uid,
                    gid,
                    xattrs: xattr_digests(&entry_path)?,
                }
            }
            _ => Entry::Special {
                mode: info.st_mode,
                uid,
                gid,
                rdev: info.st_rdev as u64,
                xattrs: xattr_digests(&entry_path)?,
            },
        };
        scan.manifest.entries.insert(path, entry);
    }
    Ok(())
}

/// Extended attribute names and values of an entry, without following a symlink.
fn xattrs(path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let empty: &mut [u8] = &mut [];
    let size = match llistxattr(path, empty) {
        Ok(size) => size,
        Err(Errno::NOTSUP) => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("listing attributes of {path:?}")),
    };
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0u8; size];
    let size = llistxattr(path, &mut names[..])?;
    let mut attributes = Vec::new();
    for name in names[..size]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::str::from_utf8(name)
            .context("non-UTF-8 extended attribute name")?
            .to_owned();
        let empty: &mut [u8] = &mut [];
        let length = match lgetxattr(path, name.as_str(), empty) {
            Ok(length) => length,
            Err(Errno::NODATA) => continue,
            Err(error) => return Err(error).with_context(|| format!("reading {name}")),
        };
        let mut value = vec![0u8; length];
        let length = lgetxattr(path, name.as_str(), &mut value[..])?;
        value.truncate(length);
        attributes.push((name, value));
    }
    attributes.sort();
    Ok(attributes)
}

fn xattr_digests(path: &Path) -> Result<BTreeMap<String, String>> {
    Ok(xattrs(path)?
        .into_iter()
        .map(|(name, value)| (name, digest(&value)))
        .collect())
}

/// Give `target/leaf` the extended attributes of `source/name`.
///
/// An attribute the kernel or security module refuses is reported and
/// skipped, never set to a different value.
fn copy_xattrs(source: &File, name: &str, target: &File, leaf: &str, path: &str) -> Result<()> {
    let from = Root::fd_path(source).join(name);
    let to = Root::fd_path(target).join(leaf);
    let wanted = xattrs(&from)?;
    for (attribute, _) in xattrs(&to)? {
        if !wanted.iter().any(|(name, _)| *name == attribute) {
            match lremovexattr(&to, attribute.as_str()) {
                Ok(()) | Err(Errno::NODATA) => {}
                Err(Errno::NOTSUP | Errno::PERM | Errno::ACCESS) => {
                    eprintln!("Warning: could not remove the {attribute} attribute of /{path}");
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("removing {attribute} of {path}"));
                }
            }
        }
    }
    for (attribute, value) in wanted {
        match lsetxattr(&to, attribute.as_str(), &value, XattrFlags::empty()) {
            Ok(()) => {}
            Err(Errno::NOTSUP | Errno::PERM | Errno::ACCESS) => {
                eprintln!("Warning: could not copy the {attribute} attribute of /{path}");
            }
            Err(error) => {
                return Err(error).with_context(|| format!("copying {attribute} of {path}"));
            }
        }
    }
    Ok(())
}

/// Copy one entry with its owner, mode, attributes and timestamps.
fn copy_entry(source: &File, name: &str, target: &File, leaf: &str, path: &str) -> Result<()> {
    let info = statat(source, name, AtFlags::SYMLINK_NOFOLLOW)?;
    match FileType::from_raw_mode(info.st_mode) {
        FileType::Directory => {
            mkdirat(target, leaf, Mode::from_raw_mode(0o700))?;
            let directory = open_dir(target, leaf)?;
            set_metadata(&directory, &info)?;
            copy_xattrs(source, name, target, leaf, path)?;
            set_times(&directory, &info)?;
        }
        FileType::RegularFile => {
            let from = File::from(openat(
                source,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            let to = File::from(openat(
                target,
                leaf,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?);
            if ioctl_ficlone(&to, &from).is_err() {
                io::copy(&mut &from, &mut &to)?;
            }
            set_metadata(&to, &info)?;
            copy_xattrs(source, name, target, leaf, path)?;
            set_times(&to, &info)?;
        }
        FileType::Symlink => {
            let link = readlinkat(source, name, Vec::new())?;
            symlinkat(link.as_c_str(), target, leaf)?;
            chownat(
                target,
                leaf,
                Some(Uid::from_raw(info.st_uid)),
                Some(Gid::from_raw(info.st_gid)),
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
            copy_xattrs(source, name, target, leaf, path)?;
            utimensat(target, leaf, &timestamps(&info), AtFlags::SYMLINK_NOFOLLOW)?;
        }
        kind => {
            mknodat(
                target,
                leaf,
                kind,
                Mode::from_raw_mode(0o600),
                info.st_rdev as _,
            )?;
            chownat(
                target,
                leaf,
                Some(Uid::from_raw(info.st_uid)),
                Some(Gid::from_raw(info.st_gid)),
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
            chmodat(
                target,
                leaf,
                Mode::from_raw_mode(info.st_mode & 0o7777),
                AtFlags::empty(),
            )?;
            copy_xattrs(source, name, target, leaf, path)?;
            utimensat(target, leaf, &timestamps(&info), AtFlags::SYMLINK_NOFOLLOW)?;
        }
    }
    Ok(())
}

/// Set the owner, then the mode, which a change of owner can clear.
fn set_metadata(file: &File, info: &Stat) -> Result<()> {
    fchown(
        file,
        Some(Uid::from_raw(info.st_uid)),
        Some(Gid::from_raw(info.st_gid)),
    )?;
    fchmod(file, Mode::from_raw_mode(info.st_mode & 0o7777))?;
    Ok(())
}

fn timestamps(info: &Stat) -> Timestamps {
    let time = |sec: i64, nsec: i64| Timespec {
        tv_sec: sec,
        tv_nsec: nsec as _,
    };
    Timestamps {
        last_access: time(info.st_atime, info.st_atime_nsec as i64),
        last_modification: time(info.st_mtime, info.st_mtime_nsec as i64),
    }
}

fn set_times(file: &File, info: &Stat) -> Result<()> {
    futimens(file, &timestamps(info))?;
    Ok(())
}

fn leaf(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, leaf)| leaf)
}

fn open_dir(directory: &File, name: &str) -> Result<File> {
    Ok(File::from(openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

/// Open the directory holding `path` beneath `root`, or `None` if it is
/// missing or not a directory.
fn parent<'a>(root: &File, path: &'a str) -> Result<Option<(File, &'a str)>> {
    let Some((directory, leaf)) = path.rsplit_once('/') else {
        return Ok(Some((root.try_clone()?, path)));
    };
    match openat2(
        root,
        directory,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    ) {
        Ok(file) => Ok(Some((File::from(file), leaf))),
        Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("opening {directory}")),
    }
}

/// Create the directories of `path` beneath `root` that are missing, and
/// return the last one.
fn ensure_dirs(root: &Root, path: &str, owner: (u32, u32), mode: u32) -> Result<File> {
    let mut current = root.as_file().try_clone()?;
    for component in path.split('/') {
        match mkdirat(&current, component, Mode::from_raw_mode(0o700)) {
            Ok(()) => {
                let created = open_dir(&current, component)?;
                fchown(
                    &created,
                    Some(Uid::from_raw(owner.0)),
                    Some(Gid::from_raw(owner.1)),
                )?;
                fchmod(&created, Mode::from_raw_mode(mode))?;
                current.sync_all()?;
                current = created;
            }
            Err(Errno::EXIST) => current = open_dir(&current, component)?,
            Err(error) => return Err(error).with_context(|| format!("creating {path}")),
        }
    }
    Ok(current)
}

fn remove_entry_tree(directory: &File, name: &str) -> Result<()> {
    match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(info) if FileType::from_raw_mode(info.st_mode) == FileType::Directory => {
            fs::remove_dir_all(Root::fd_path(directory).join(name))?;
            Ok(())
        }
        Ok(_) => Ok(unlinkat(directory, name, AtFlags::empty())?),
    }
}

fn remove_tree(state: &Root, name: &str) -> Result<()> {
    remove_entry_tree(state.as_file(), name)
        .with_context(|| format!("removing the /etc merge scratch directory {name}"))?;
    state.as_file().sync_all()?;
    Ok(())
}

/// Replace a root-only file in the image state.
fn write_private(state: &Root, name: &str, bytes: &[u8]) -> Result<()> {
    let directory = Root::fd_path(state.as_file());
    let mut file = tempfile::Builder::new()
        .prefix(".cps-etc-")
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(&directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(directory.join(name))?;
    state.as_file().sync_all()?;
    Ok(())
}

/// Saved running versions in a root: (revision, path, saved copy).
pub fn saved_versions(root: &Path) -> Result<Vec<(u64, String, String)>> {
    let root = Root::open(root)?;
    let prefix = format!("{CONFIG_UPDATES_DIRECTORY}/{STASH}");
    let stash = match root.dir(&prefix) {
        Ok(stash) => stash,
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let mut saved = Vec::new();
    for revision in fs::read_dir(Root::fd_path(&stash))? {
        let revision = revision?;
        let file_name = revision.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(number) = name
            .split_once('-')
            .map_or(name, |(number, _)| number)
            .parse::<u64>()
            .ok()
        else {
            continue;
        };
        let directory = Root::open(&revision.path())?;
        let mut scan = Scan {
            manifest: Manifest::default(),
            inodes: BTreeMap::new(),
        };
        let top = match open_dir(directory.as_file(), ETC) {
            Ok(top) => top,
            Err(_) => continue,
        };
        let info = fstat(&top)?;
        visit(&top, ETC, info.st_dev, false, false, &mut scan)?;
        for (path, entry) in scan.manifest.entries {
            if !entry.is_dir() {
                saved.push((number, path.clone(), format!("{prefix}/{name}/{path}")));
            }
        }
    }
    saved.sort();
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> Entry {
        file_with(text, 0o644)
    }

    fn file_with(text: &str, mode: u32) -> Entry {
        Entry::File {
            mode,
            uid: 0,
            gid: 0,
            size: text.len() as u64,
            sha256: digest(text.as_bytes()),
            xattrs: BTreeMap::new(),
        }
    }

    fn dir() -> Entry {
        Entry::Dir {
            mode: 0o755,
            uid: 0,
            gid: 0,
            xattrs: BTreeMap::new(),
        }
    }

    fn manifest(entries: &[(&str, Entry)]) -> Manifest {
        let mut manifest = Manifest {
            format: FORMAT,
            ..Manifest::default()
        };
        manifest.entries.insert(ETC.into(), dir());
        for (path, entry) in entries {
            manifest.entries.insert((*path).into(), entry.clone());
        }
        manifest
    }

    fn managed(text: &str, preserve: bool) -> Managed {
        Managed {
            node: Node::File {
                sha256: digest(text.as_bytes()),
                size: text.len() as u64,
                mode: 0o644,
                uid: 0,
                gid: 0,
                capabilities: None,
            },
            owners: BTreeSet::from(["system/base".to_owned()]),
            preserve,
            implicit: false,
            original_name: None,
        }
    }

    #[test]
    fn records_merge_by_key_and_conflict_on_shared_changes() {
        let base = b"root:x:0:0::/root:/bin/sh\nbin:x:1:1::/:/bin/false\nold:x:2:2::/:/bin/false\n";
        // The trial adds a system user. The running system changes root's
        // shell, adds alice and removes old.
        let theirs = b"root:x:0:0::/root:/bin/sh\nbin:x:1:1::/:/bin/false\nold:x:2:2::/:/bin/false\nsysd:x:9:9::/:/bin/false\n";
        let ours = b"root:x:0:0::/root:/bin/zsh\nbin:x:1:1::/:/bin/false\nalice:x:1000:1000::/home/alice:/bin/sh\n";
        assert_eq!(
            merge_records(base, ours, theirs).as_deref(),
            Some(
                &b"root:x:0:0::/root:/bin/zsh\nbin:x:1:1::/:/bin/false\nsysd:x:9:9::/:/bin/false\nalice:x:1000:1000::/home/alice:/bin/sh\n"[..]
            )
        );
        // The same new record on both sides is no conflict.
        assert_eq!(
            merge_records(b"", b"a:1\n", b"a:1\n").as_deref(),
            Some(&b"a:1\n"[..])
        );
        // One record changed differently on both sides.
        assert_eq!(merge_records(b"a:1\n", b"a:2\n", b"a:3\n"), None);
        // The trial removed a record that the running system changed.
        assert_eq!(merge_records(b"a:1\nb:1\n", b"a:2\nb:1\n", b"b:1\n"), None);
        // The trial removed a record that the running system kept.
        assert_eq!(
            merge_records(b"a:1\nb:1\n", b"a:1\nb:2\n", b"b:1\n").as_deref(),
            Some(&b"b:2\n"[..])
        );
        // Lines that are not keyed records make the file a conflict.
        assert_eq!(merge_records(b"", b"a:1\na:2\n", b""), None);
        assert_eq!(merge_records(b"", b"# comment\n", b""), None);
        assert_eq!(merge_records(b"", b":x\n", b""), None);
        assert_eq!(
            merge_records(b"a:1\n", b"", b"a:1\n").as_deref(),
            Some(&b""[..])
        );
    }

    #[test]
    fn plan_carries_local_changes_and_keeps_package_files_consistent() -> Result<()> {
        let paths = BTreeMap::from([
            ("etc/preserved".to_owned(), managed("vendor 2", true)),
            ("etc/unchanged.conf".to_owned(), managed("vendor", true)),
            ("etc/package".to_owned(), managed("package 2", false)),
            ("etc/required".to_owned(), managed("required", false)),
            ("etc/moded".to_owned(), managed("vendor", true)),
        ]);
        let base = manifest(&[
            ("etc/preserved", file("vendor 1")),
            ("etc/unchanged.conf", file("vendor")),
            ("etc/package", file("package 1")),
            ("etc/required", file("required")),
            ("etc/moded", file("vendor")),
            ("etc/hostname", file("canopa")),
            ("etc/motd", file("hello")),
            ("etc/deleted", file("old")),
            ("etc/same", file("before")),
            ("etc/ld.so.cache", file("cache 1")),
            (
                "etc/localtime",
                Entry::Symlink {
                    target: "UTC".into(),
                    uid: 0,
                    gid: 0,
                    xattrs: BTreeMap::new(),
                },
            ),
            ("etc/mounted", dir()),
            ("etc/mounted/inner", file("inner")),
        ]);
        let mut ours = base.clone();
        let mut theirs = base.clone();
        for (path, entry) in [
            ("etc/preserved", file("vendor 2")),
            ("etc/package", file("package 2")),
            ("etc/hostname", file("staged")),
            ("etc/same", file("after")),
            ("etc/ld.so.cache", file("cache 2")),
        ] {
            theirs.entries.insert(path.into(), entry);
        }
        for (path, entry) in [
            ("etc/preserved", file("local")),
            ("etc/unchanged.conf", file("local")),
            ("etc/package", file("drift")),
            ("etc/moded", file_with("vendor", 0o600)),
            ("etc/hostname", file("workstation")),
            ("etc/motd", file_with("hello", 0o600)),
            ("etc/new", file("new")),
            ("etc/same", file("after")),
            ("etc/ld.so.cache", file("cache 3")),
            (
                "etc/localtime",
                Entry::Symlink {
                    target: "Europe/Paris".into(),
                    uid: 0,
                    gid: 0,
                    xattrs: BTreeMap::new(),
                },
            ),
        ] {
            ours.entries.insert(path.into(), entry);
        }
        ours.entries.remove("etc/deleted");
        ours.entries.remove("etc/required");
        // A mount point hides what the snapshot recorded beneath it.
        ours.entries.remove("etc/mounted");
        ours.entries.remove("etc/mounted/inner");
        ours.opaque.insert("etc/mounted".into());
        let policy = Policy {
            paths: &paths,
            generated: vec!["etc/ld.so.cache"],
        };
        let actions = plan(&base, &ours, &theirs, &policy)?;
        let expected = BTreeMap::from([
            ("etc/preserved", Action::ConfigUpdate),
            ("etc/unchanged.conf", Action::Take),
            ("etc/package", Action::Conflict(Reason::Drift)),
            ("etc/required", Action::Keep(Reason::RemovedManaged)),
            ("etc/moded", Action::Conflict(Reason::Drift)),
            ("etc/hostname", Action::Conflict(Reason::BothChanged)),
            ("etc/motd", Action::Take),
            ("etc/new", Action::Take),
            ("etc/deleted", Action::Take),
            ("etc/ld.so.cache", Action::Keep(Reason::Generated)),
            ("etc/localtime", Action::Take),
            ("etc/mounted", Action::Keep(Reason::Opaque)),
        ]);
        assert_eq!(
            actions,
            expected
                .into_iter()
                .map(|(path, action)| (path.to_owned(), action))
                .collect()
        );
        // Once the running /etc is the base, nothing is left to merge.
        assert!(plan(&ours, &ours, &theirs, &policy)?.is_empty());
        Ok(())
    }

    #[test]
    fn preserved_account_databases_merge_and_keep_manifest_metadata() -> Result<()> {
        let passwd = |text: &str| -> (Entry, String) { (file(text), STANDARD.encode(text)) };
        let paths = BTreeMap::from([("etc/group".to_owned(), managed("wheel:x:10:\n", true))]);
        let mut base = manifest(&[]);
        let mut ours = manifest(&[]);
        let mut theirs = manifest(&[]);
        for (manifest, text) in [
            (&mut base, "wheel:x:10:\n"),
            (&mut ours, "wheel:x:10:alice\nalice:x:1000:\n"),
            (&mut theirs, "wheel:x:10:\nsysd:x:9:\n"),
        ] {
            let (entry, record) = passwd(text);
            manifest.entries.insert("etc/group".into(), entry);
            manifest.records.insert("etc/group".into(), record);
        }
        let policy = Policy {
            paths: &paths,
            generated: Vec::new(),
        };
        assert_eq!(
            plan(&base, &ours, &theirs, &policy)?["etc/group"],
            Action::Records {
                content: b"wheel:x:10:alice\nsysd:x:9:\nalice:x:1000:\n".to_vec(),
                running_metadata: false,
            }
        );
        // Mode changed on the running system makes the package file drift.
        ours.entries
            .insert("etc/group".into(), file_with("wheel:x:10:alice\n", 0o600));
        assert_eq!(
            plan(&base, &ours, &theirs, &policy)?["etc/group"],
            Action::Conflict(Reason::Drift)
        );
        Ok(())
    }

    #[test]
    fn account_merges_keep_the_changed_metadata_and_unique_ids() -> Result<()> {
        let accounts = |text: &str, mode: u32| -> (Entry, String) {
            (file_with(text, mode), STANDARD.encode(text))
        };
        let paths = BTreeMap::new();
        let policy = Policy {
            paths: &paths,
            generated: Vec::new(),
        };
        let root = "root:x:0:0::/root:/bin/sh\n";
        let case = |ours: (&str, u32), theirs: (&str, u32)| -> Result<Action> {
            let mut manifests = [manifest(&[]), manifest(&[]), manifest(&[])];
            for (manifest, (text, mode)) in manifests.iter_mut().zip([(root, 0o644), ours, theirs])
            {
                let (entry, record) = accounts(text, mode);
                manifest.entries.insert("etc/passwd".into(), entry);
                manifest.records.insert("etc/passwd".into(), record);
            }
            let [base, ours, theirs] = &manifests;
            Ok(plan(base, ours, theirs, &policy)?
                .remove("etc/passwd")
                .unwrap())
        };
        let alice = format!("{root}alice:x:1000:1000::/home/alice:/bin/sh\n");
        let sysd = format!("{root}sysd:x:999:999::/:/bin/false\n");
        let merged =
            format!("{root}sysd:x:999:999::/:/bin/false\nalice:x:1000:1000::/home/alice:/bin/sh\n");
        // Only the trial changed the mode: the merge keeps the trial's.
        assert_eq!(
            case((&alice, 0o644), (&sysd, 0o640))?,
            Action::Records {
                content: merged.clone().into_bytes(),
                running_metadata: false,
            }
        );
        // Only the running system changed it: the merge keeps the running one's.
        assert_eq!(
            case((&alice, 0o600), (&sysd, 0o644))?,
            Action::Records {
                content: merged.into_bytes(),
                running_metadata: true,
            }
        );
        // Both changed it differently.
        assert_eq!(
            case((&alice, 0o600), (&sysd, 0o640))?,
            Action::Conflict(Reason::BothChanged)
        );
        // Each side gave a new account the same free UID.
        let backup = format!("{root}backup:x:999:999::/:/bin/false\n");
        assert_eq!(
            case((&backup, 0o644), (&sysd, 0o644))?,
            Action::Conflict(Reason::BothChanged)
        );
        assert!(unique_ids(b"a:x:1:\nb:x:2:\n"));
        assert!(!unique_ids(b"a:x:1:\nb:x:1:\n"));
        assert!(!unique_ids(b"a:x\n"));
        Ok(())
    }

    #[test]
    fn implicit_directories_take_local_metadata() -> Result<()> {
        let paths = BTreeMap::from([(
            "etc/ssh".to_owned(),
            Managed {
                node: Node::Dir {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                },
                owners: BTreeSet::from(["system/base".to_owned()]),
                preserve: false,
                implicit: true,
                original_name: None,
            },
        )]);
        let policy = Policy {
            paths: &paths,
            generated: Vec::new(),
        };
        let base = manifest(&[("etc/ssh", dir())]);
        let mut ours = base.clone();
        ours.entries.insert(
            "etc/ssh".into(),
            Entry::Dir {
                mode: 0o750,
                uid: 0,
                gid: 0,
                xattrs: BTreeMap::new(),
            },
        );
        assert_eq!(plan(&base, &ours, &base, &policy)?["etc/ssh"], Action::Take);
        Ok(())
    }

    #[test]
    fn scan_records_types_metadata_and_mount_points() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir()?;
        let etc = dir.path().join("etc");
        fs::create_dir_all(etc.join("sub"))?;
        fs::write(etc.join("passwd"), "root:x:0:0::/:/bin/sh\n")?;
        fs::write(etc.join("sub/file"), "data")?;
        fs::set_permissions(etc.join("sub/file"), fs::Permissions::from_mode(0o640))?;
        fs::hard_link(etc.join("sub/file"), etc.join("sub/link"))?;
        symlink("target", etc.join("link"))?;
        let scan = scan(&Root::open(dir.path())?, true)?;
        let entries = &scan.manifest.entries;
        assert!(entries["etc"].is_dir() && entries["etc/sub"].is_dir());
        assert!(matches!(
            &entries["etc/sub/file"],
            Entry::File {
                mode: 0o640,
                size: 4,
                ..
            }
        ));
        assert!(
            matches!(&entries["etc/link"], Entry::Symlink { target, .. } if target == "target")
        );
        assert_eq!(scan.inodes["etc/sub/file"].0, scan.inodes["etc/sub/link"].0);
        assert_eq!(scan.inodes["etc/sub/file"].1, 2);
        assert_eq!(
            scan.manifest.record("etc/passwd")?.as_deref(),
            Some(&b"root:x:0:0::/:/bin/sh\n"[..])
        );
        assert!(scan.manifest.record("etc/sub/file")?.is_none());
        // A root without /etc reads as empty.
        let empty = tempfile::tempdir()?;
        assert!(scan_empty(empty.path())?);
        Ok(())
    }

    fn scan_empty(path: &Path) -> Result<bool> {
        Ok(scan(&Root::open(path)?, false)?.manifest.entries.is_empty())
    }
}
