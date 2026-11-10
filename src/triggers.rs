//! Declarative file-pattern triggers.
//!
//! A package declares a trigger that watches path patterns, possibly over
//! files that other packages deliver, and names a handler executable that
//! the same package ships. After a transaction writes every file, each
//! trigger whose watched paths changed, or whose own package was installed
//! or upgraded, runs exactly once in an order derived from what triggers
//! generate and watch.
//!
//! A trigger declares the paths its handler generates. The declaring
//! package owns everything those output patterns cover: no package may
//! ship a path there, verification ignores the contents, and CPS removes
//! the generated paths when the trigger goes away. The handler runs under
//! Bubblewrap with the image read-only. The directories that hold its
//! outputs are overlays whose writes CPS inspects afterward, and only
//! changes to declared outputs reach the image.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io,
    os::{
        fd::{AsFd, AsRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{AtFlags, FileType, statat},
    io::{Errno, FdFlags, fcntl_setfd},
};
use serde::{Deserialize, Serialize};

use crate::{
    build::pattern,
    image::{
        Installed, Managed, STATE_ANCESTORS, STATE_DIRECTORY, State,
        root::{DirSyncs, ReplacementSource, Root},
    },
    model::{Node, digest_reader, validate_path},
    plan::{Write, boot_sensitive_pattern},
};

/// Seconds a handler may run unless its trigger declares a timeout.
pub(crate) const DEFAULT_TIMEOUT: u32 = 300;
const MAX_TIMEOUT: u32 = 3600;
const MAX_ARGUMENTS: usize = 64;
/// Generated files larger than this fail the handler.
const FILE_LIMIT: u64 = 4 << 30;
const BWRAP: &str = "/usr/bin/bwrap";
/// Top-level image entries that the sandbox replaces or never exposes.
const HIDDEN: &[&str] = &[
    "proc",
    "sys",
    "dev",
    "run",
    "tmp",
    "home",
    "root",
    "boot",
    "efi",
    "mnt",
    "media",
    "lost+found",
];
const SCRATCH: &str = "triggers";

/// A trigger declared by a package manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    pub name: String,
    /// The image-relative handler executable, shipped by the same package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
    /// Fixed arguments passed to the handler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    pub watch: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    /// Services to reload, when active, after a live run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reload: Vec<String>,
    /// Services to restart, when active, after a live run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
}

impl Trigger {
    pub(crate) fn validate(&self) -> Result<()> {
        let name = &self.name;
        ensure!(
            name.len() <= 64
                && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c)),
            "invalid trigger name {name:?}"
        );
        ensure!(
            !self.watch.is_empty(),
            "trigger {name} must watch at least one path pattern"
        );
        for watch in &self.watch {
            pattern::validate(watch).with_context(|| format!("trigger {name} watch"))?;
        }
        for output in &self.outputs {
            validate_output(output).with_context(|| format!("trigger {name} output"))?;
        }
        for (kind, values) in [
            ("watch", &self.watch),
            ("output", &self.outputs),
            ("reload", &self.reload),
            ("restart", &self.restart),
        ] {
            let mut seen = BTreeSet::new();
            for value in values {
                ensure!(
                    seen.insert(value),
                    "trigger {name} repeats {kind} {value:?}"
                );
            }
        }
        for unit in self.reload.iter().chain(&self.restart) {
            ensure!(
                crate::systemd::service_name(unit),
                "trigger {name} can only reload or restart services, not {unit:?}"
            );
        }
        match &self.handler {
            Some(handler) => {
                validate_path(handler).with_context(|| format!("trigger {name} handler"))?;
                ensure!(
                    self.args.len() <= MAX_ARGUMENTS
                        && self
                            .args
                            .iter()
                            .all(|arg| arg.len() <= 4096 && !arg.contains('\0')),
                    "trigger {name} has invalid handler arguments"
                );
            }
            None => ensure!(
                self.outputs.is_empty() && !(self.reload.is_empty() && self.restart.is_empty()),
                "trigger {name} needs a run handler unless it only reloads or restarts services"
            ),
        }
        if let Some(timeout) = self.timeout {
            ensure!(
                (1..=MAX_TIMEOUT).contains(&timeout),
                "trigger {name} timeout must be between 1 and {MAX_TIMEOUT} seconds"
            );
        }
        Ok(())
    }

    fn generates(&self, path: &str) -> bool {
        self.outputs.iter().any(|output| covers(output, path))
    }

    fn watches(&self, path: &str) -> bool {
        self.watch.iter().any(|watch| covers(watch, path))
    }

    fn boot_output(&self) -> bool {
        self.outputs
            .iter()
            .any(|output| boot_sensitive_pattern(output))
    }
}

fn validate_output(output: &str) -> Result<()> {
    pattern::validate(output)?;
    let base = static_base(output);
    ensure!(
        !base.is_empty() && !STATE_ANCESTORS.contains(&base.as_str()),
        "output {output:?} must name a file inside a directory below the image root, outside var and var/lib"
    );
    ensure!(
        output.split('/').next() != Some(".cps") && !intersects(output, STATE_DIRECTORY),
        "output {output:?} covers reserved image state"
    );
    let top = output.split('/').next().unwrap_or_default();
    ensure!(
        !matches!(top, "proc" | "sys" | "dev" | "run" | "tmp"),
        "output {output:?} is on a file system that the handler sandbox replaces"
    );
    Ok(())
}

/// The directory that holds an output pattern's matches, before climbing
/// to an existing ancestor.
fn static_base(output: &str) -> String {
    let parts: Vec<_> = output.split('/').collect();
    let literal = parts.iter().take_while(|part| !wildcard(part)).count();
    let end = if literal == parts.len() {
        literal - 1
    } else {
        literal
    };
    parts[..end].join("/")
}

fn wildcard(part: &str) -> bool {
    part.contains(['*', '?'])
}

/// The leading literal components of a pattern.
fn literal_prefix(pattern: &str) -> String {
    pattern
        .split('/')
        .take_while(|part| !wildcard(part))
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a pattern matches a path or one of its ancestors. A pattern
/// therefore names whole trees: a matched directory includes its contents.
pub(crate) fn covers(pattern: &str, path: &str) -> bool {
    let mut end = path.len();
    loop {
        if pattern::matches(pattern, &path[..end]) {
            return true;
        }
        match path[..end].rfind('/') {
            Some(index) => end = index,
            None => return false,
        }
    }
}

/// Whether some path is covered by both patterns.
pub(crate) fn intersects(left: &str, right: &str) -> bool {
    let left: Vec<_> = left.split('/').chain(["**"]).collect();
    let right: Vec<_> = right.split('/').chain(["**"]).collect();
    components_intersect(&left, &right)
}

fn components_intersect(left: &[&str], right: &[&str]) -> bool {
    match (left.first(), right.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            components_intersect(&left[1..], right)
                || (!right.is_empty() && components_intersect(left, &right[1..]))
        }
        (_, Some(&"**")) => {
            components_intersect(left, &right[1..])
                || (!left.is_empty() && components_intersect(&left[1..], right))
        }
        (Some(a), Some(b)) => {
            names_intersect(a.as_bytes(), b.as_bytes())
                && components_intersect(&left[1..], &right[1..])
        }
        _ => false,
    }
}

fn names_intersect(left: &[u8], right: &[u8]) -> bool {
    match (left.first(), right.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            names_intersect(&left[1..], right)
                || (!right.is_empty() && names_intersect(left, &right[1..]))
        }
        (_, Some(b'*')) => {
            names_intersect(left, &right[1..])
                || (!left.is_empty() && names_intersect(&left[1..], right))
        }
        (Some(a), Some(b)) => {
            (a == b || *a == b'?' || *b == b'?') && names_intersect(&left[1..], &right[1..])
        }
        _ => false,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TriggerRun {
    pub package: String,
    pub name: String,
    /// The trigger's outputs are boot artifacts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boot: bool,
}

impl TriggerRun {
    pub(crate) fn label(&self) -> String {
        format!("{}:{}", self.package, self.name)
    }

    fn key(&self) -> (&str, &str) {
        (&self.package, &self.name)
    }

    /// Find the declaration in a state's installed manifests.
    pub(crate) fn find<'a>(
        &self,
        installed: &'a BTreeMap<String, Installed>,
    ) -> Result<&'a Trigger> {
        installed
            .get(&self.package)
            .and_then(|package| {
                package
                    .published
                    .manifest
                    .triggers
                    .iter()
                    .find(|trigger| trigger.name == self.name)
            })
            .with_context(|| format!("missing trigger {}", self.label()))
    }
}

type Key<'a> = (&'a str, &'a str);

struct ActiveTrigger<'a> {
    trigger: &'a Trigger,
    handler: Option<&'a Node>,
    sha256: &'a str,
}

/// Triggers of installed packages whose handler is selected and owned by
/// the declaring package, so a handler follows its file's architecture,
/// facet, and variant selection.
fn active<'a>(
    installed: &'a BTreeMap<String, Installed>,
    paths: &'a BTreeMap<String, Managed>,
) -> BTreeMap<Key<'a>, ActiveTrigger<'a>> {
    let mut active = BTreeMap::new();
    for (name, package) in installed {
        for trigger in &package.published.manifest.triggers {
            let handler = match &trigger.handler {
                Some(handler) => match paths.get(handler) {
                    Some(file) if file.owners.contains(name) => Some(&file.node),
                    _ => continue,
                },
                None => None,
            };
            active.insert(
                (name.as_str(), trigger.name.as_str()),
                ActiveTrigger {
                    trigger,
                    handler,
                    sha256: &package.published.sha256,
                },
            );
        }
    }
    active
}

/// The output patterns of the active triggers of a state.
pub(crate) fn active_outputs<'a>(
    installed: &'a BTreeMap<String, Installed>,
    paths: &'a BTreeMap<String, Managed>,
) -> Vec<&'a str> {
    active(installed, paths)
        .values()
        .flat_map(|active| active.trigger.outputs.iter().map(String::as_str))
        .collect()
}

/// Whether an active trigger generates a path.
pub(crate) fn generated(outputs: &[&str], path: &str) -> bool {
    outputs.iter().any(|output| covers(output, path))
}

#[derive(Debug, Default)]
pub(crate) struct Planned {
    pub run: Vec<TriggerRun>,
    pub retire: Vec<TriggerRun>,
}

/// Plan the triggers for a transaction from the committed composition to
/// the next one.
pub(crate) fn plan(
    before_installed: &BTreeMap<String, Installed>,
    before_paths: &BTreeMap<String, Managed>,
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
    writes: &[Write],
) -> Result<Planned> {
    let before = active(before_installed, before_paths);
    let after = active(installed, paths);

    let edges = after
        .iter()
        .map(|(&from, source)| {
            let targets = after
                .iter()
                .filter(|&(&to, target)| {
                    to != from
                        && source.trigger.outputs.iter().any(|output| {
                            target
                                .trigger
                                .watch
                                .iter()
                                .any(|watch| intersects(output, watch))
                        })
                })
                .map(|(&to, _)| to)
                .collect::<BTreeSet<_>>();
            (from, targets)
        })
        .collect::<BTreeMap<_, _>>();
    check_cycles(&edges)?;

    let changed: Vec<&str> = writes
        .iter()
        .filter(|write| write.before != write.after || write.detach)
        .map(|write| write.path.as_str())
        .collect();
    let mut pending: Vec<Key> = after
        .iter()
        .filter(|&(key, current)| {
            let previous = before.get(key);
            previous.is_none_or(|previous| {
                previous.trigger != current.trigger
                    || previous.handler != current.handler
                    || previous.sha256 != current.sha256
            }) || changed.iter().any(|path| current.trigger.watches(path))
        })
        .map(|(&key, _)| key)
        .collect();
    // A trigger whose outputs another trigger watches makes that one run too.
    let mut selected = BTreeSet::new();
    while let Some(key) = pending.pop() {
        if selected.insert(key) {
            pending.extend(edges[&key].iter().copied());
        }
    }

    let mut incoming = selected
        .iter()
        .map(|&key| (key, 0_usize))
        .collect::<BTreeMap<_, _>>();
    for key in &selected {
        for target in &edges[key] {
            if let Some(count) = incoming.get_mut(target) {
                *count += 1;
            }
        }
    }
    let mut ready: BTreeSet<Key> = incoming
        .iter()
        .filter(|&(_, &count)| count == 0)
        .map(|(&key, _)| key)
        .collect();
    let mut run = Vec::new();
    while let Some(key) = ready.pop_first() {
        run.push(TriggerRun {
            package: key.0.to_owned(),
            name: key.1.to_owned(),
            boot: after[&key].trigger.boot_output(),
        });
        for target in &edges[&key] {
            if let Some(count) = incoming.get_mut(target) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(*target);
                }
            }
        }
    }
    debug_assert_eq!(run.len(), selected.len());

    let retire = before
        .iter()
        .filter(|&(key, previous)| {
            !previous.trigger.outputs.is_empty()
                && after
                    .get(key)
                    .is_none_or(|current| current.trigger.outputs != previous.trigger.outputs)
        })
        .map(|(&(package, name), previous)| TriggerRun {
            package: package.to_owned(),
            name: name.to_owned(),
            boot: previous.trigger.boot_output(),
        })
        .collect();
    Ok(Planned { run, retire })
}

/// Reject a composition in which a package ships a path that a trigger
/// generates, or two triggers generate the same path.
pub(crate) fn check_conflicts(
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
) -> Result<()> {
    let active = active(installed, paths);
    let outputs: Vec<_> = active
        .iter()
        .flat_map(|(key, active)| {
            active
                .trigger
                .outputs
                .iter()
                .map(move |output| (key, output))
        })
        .collect();
    for (index, &((package, name), output)) in outputs.iter().enumerate() {
        let prefix = literal_prefix(output);
        let nested = format!("{prefix}/");
        let end = format!("{prefix}0");
        let candidates =
            paths
                .get_key_value(prefix.as_str())
                .into_iter()
                .chain(paths.range::<str, _>((
                    std::ops::Bound::Included(nested.as_str()),
                    std::ops::Bound::Excluded(end.as_str()),
                )));
        for (path, managed) in candidates {
            ensure!(
                !covers(output, path),
                "path conflict for {path} between {} and trigger {package}:{name}, which generates {output}",
                managed
                    .owners
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for &((other_package, other_name), other) in &outputs[index + 1..] {
            ensure!(
                (package, name) == (other_package, other_name) || !intersects(output, other),
                "triggers {package}:{name} and {other_package}:{other_name} both generate paths matching {output} and {other}"
            );
        }
    }
    Ok(())
}

fn check_cycles(edges: &BTreeMap<Key<'_>, BTreeSet<Key<'_>>>) -> Result<()> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit<'a>(
        key: Key<'a>,
        edges: &BTreeMap<Key<'a>, BTreeSet<Key<'a>>>,
        marks: &mut BTreeMap<Key<'a>, Mark>,
        stack: &mut Vec<Key<'a>>,
    ) -> Result<()> {
        match marks.get(&key) {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::Visiting) => {
                let start = stack.iter().position(|&entry| entry == key).unwrap_or(0);
                let cycle = stack[start..]
                    .iter()
                    .chain([&key])
                    .map(|(package, name)| format!("{package}:{name}"))
                    .collect::<Vec<_>>();
                bail!(
                    "trigger cycle: {}; each trigger in the cycle watches what the previous one generates",
                    cycle.join(" -> ")
                );
            }
            None => {}
        }
        marks.insert(key, Mark::Visiting);
        stack.push(key);
        for &target in &edges[&key] {
            visit(target, edges, marks, stack)?;
        }
        stack.pop();
        marks.insert(key, Mark::Done);
        Ok(())
    }
    let mut marks = BTreeMap::new();
    for &key in edges.keys() {
        visit(key, edges, &mut marks, &mut Vec::new())?;
    }
    Ok(())
}

/// Whether a planned run generates the path.
pub(crate) fn regenerates(
    runs: &[TriggerRun],
    installed: &BTreeMap<String, Installed>,
    path: &str,
) -> Result<bool> {
    for run in runs {
        if run.find(installed)?.generates(path) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The services that live triggers reload and restart.
pub(crate) fn actuators(
    runs: &[TriggerRun],
    installed: &BTreeMap<String, Installed>,
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut reload = BTreeSet::new();
    let mut restart = BTreeSet::new();
    for run in runs {
        let trigger = run.find(installed)?;
        reload.extend(trigger.reload.iter().cloned());
        restart.extend(trigger.restart.iter().cloned());
    }
    for unit in &restart {
        reload.remove(unit);
    }
    Ok((reload, restart))
}

/// The type of the object at a path, without following symlinks. A path
/// beneath a file or a symlink is absent: CPS never generates or removes
/// anything through a link.
fn kind(root: &Root, path: &str) -> Result<Option<FileType>> {
    validate_path(path)?;
    let (parent, leaf) = match path.rsplit_once('/') {
        Some((parent, leaf)) => match root.dir(parent) {
            Ok(parent) => (parent, leaf),
            Err(error)
                if matches!(
                    error.downcast_ref::<Errno>(),
                    Some(&(Errno::NOENT | Errno::NOTDIR | Errno::LOOP))
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).with_context(|| format!("opening {parent}")),
        },
        None => (root.as_file().try_clone()?, path),
    };
    match statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(info) => Ok(Some(FileType::from_raw_mode(info.st_mode))),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {path}")),
    }
}

fn children(root: &Root, path: &str) -> Result<Vec<String>> {
    let directory = if path.is_empty() {
        root.as_file().try_clone()?
    } else {
        root.dir(path)?
    };
    let mut names = Vec::new();
    for entry in fs::read_dir(Root::fd_path(&directory))? {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(if path.is_empty() {
                name
            } else {
                format!("{path}/{name}")
            });
        }
    }
    names.sort();
    Ok(names)
}

/// Remove a generated tree from the image.
fn remove_tree(root: &Root, path: &str, syncs: &mut DirSyncs) -> Result<()> {
    match kind(root, path)? {
        None => Ok(()),
        Some(FileType::Directory) => {
            for child in children(root, path)? {
                remove_tree(root, &child, syncs)?;
            }
            ensure!(
                root.remove_empty_dir(path, syncs)?,
                "could not remove generated directory {path}"
            );
            Ok(())
        }
        Some(_) => root.remove_file_deferred(path, syncs),
    }
}

/// Paths in the image that a pattern matches, without following symlinks.
fn expand(root: &Root, parts: &[&str], prefix: &str, found: &mut BTreeSet<String>) -> Result<()> {
    let Some((&first, rest)) = parts.split_first() else {
        if !prefix.is_empty() {
            found.insert(prefix.to_owned());
        }
        return Ok(());
    };
    let join = |name: &str| {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        }
    };
    let directory = prefix.is_empty() || kind(root, prefix)? == Some(FileType::Directory);
    if first == "**" {
        if !rest.is_empty() {
            expand(root, rest, prefix, found)?;
        }
        if directory {
            for child in children(root, prefix)? {
                if rest.is_empty() {
                    found.insert(child.clone());
                }
                if kind(root, &child)? == Some(FileType::Directory) {
                    expand(root, parts, &child, found)?;
                }
            }
        }
    } else if !wildcard(first) {
        let path = join(first);
        if kind(root, &path)?.is_some() {
            expand(root, rest, &path, found)?;
        }
    } else if directory {
        for child in children(root, prefix)? {
            let name = child.rsplit('/').next().unwrap_or(&child);
            if pattern::matches(first, name) {
                expand(root, rest, &child, found)?;
            }
        }
    }
    Ok(())
}

/// Remove generated paths that retired or changed triggers no longer declare.
pub(crate) fn retire(
    root: &Root,
    retired: &[TriggerRun],
    base: &State,
    next: &State,
) -> Result<()> {
    if retired.is_empty() {
        return Ok(());
    }
    let keep = active_outputs(&next.installed, &next.paths);
    let mut syncs = DirSyncs::default();
    for run in retired {
        let trigger = run.find(&base.installed)?;
        for output in &trigger.outputs {
            let mut found = BTreeSet::new();
            let parts: Vec<_> = output.split('/').collect();
            expand(root, &parts, "", &mut found)?;
            for path in found {
                remove_unclaimed(root, &path, &keep, &next.paths, &mut syncs)?;
            }
        }
    }
    root.sync_dirs(&mut syncs, None)
}

fn remove_unclaimed(
    root: &Root,
    path: &str,
    keep: &[&str],
    paths: &BTreeMap<String, Managed>,
    syncs: &mut DirSyncs,
) -> Result<()> {
    if paths.contains_key(path) || generated(keep, path) {
        return Ok(());
    }
    match kind(root, path)? {
        None => {}
        Some(FileType::Directory) => {
            for child in children(root, path)? {
                remove_unclaimed(root, &child, keep, paths, syncs)?;
            }
            root.remove_empty_dir(path, syncs)?;
        }
        Some(_) => root.remove_file_deferred(path, syncs)?,
    }
    Ok(())
}

/// Finish directory removals that generated outputs prevented. A generated
/// path inside a directory that no package ships any more is orphaned.
///
/// A directory beneath a transition left with the old tree when the
/// transition replaced it, so only removals outside transitions remain.
pub(crate) fn prune(root: &Root, writes: &[Write], next: &State) -> Result<()> {
    let outputs = active_outputs(&next.installed, &next.paths);
    let transitions: Vec<&str> = writes
        .iter()
        .filter(|write| write.transition().is_some())
        .map(|write| write.path.as_str())
        .collect();
    let mut removed: Vec<_> = writes
        .iter()
        .filter(|write| {
            write.removes_directory()
                && !transitions
                    .iter()
                    .any(|transition| write.beneath(transition))
        })
        .map(|write| write.path.as_str())
        .collect();
    removed.sort_unstable_by(|a, b| b.cmp(a));
    let mut syncs = DirSyncs::default();
    for path in removed {
        if kind(root, path)? != Some(FileType::Directory) {
            continue;
        }
        for child in children(root, path)? {
            if !next.paths.contains_key(&child) && generated(&outputs, &child) {
                remove_tree(root, &child, &mut syncs)?;
            }
        }
        root.remove_empty_dir(path, &mut syncs)?;
    }
    root.sync_dirs(&mut syncs, None)
}

/// One overlay: the image directory and its private upper and work directories.
struct Layer {
    base: String,
    upper: PathBuf,
    work: PathBuf,
}

/// Run one planned trigger against the image and publish its outputs.
pub(crate) fn run(
    root: &Root,
    stage: &Root,
    path: &Path,
    planned: &TriggerRun,
    next: &State,
) -> Result<()> {
    let trigger = planned.find(&next.installed)?;
    let Some(handler) = &trigger.handler else {
        return Ok(());
    };
    let label = planned.label();
    let managed = next
        .paths
        .get(handler)
        .filter(|managed| managed.owners.contains(&planned.package))
        .with_context(|| format!("trigger {label} handler {handler} is not installed"))?;
    ensure!(
        matches!(managed.node, Node::File { mode, .. } if mode & 0o111 != 0),
        "trigger {label} handler {handler} is not executable"
    );
    ensure!(
        root.inspect(handler)?.as_ref() == Some(&managed.node),
        "trigger {label} handler differs from its manifest at {handler}"
    );
    let mut bases = BTreeSet::new();
    for output in &trigger.outputs {
        bases.insert(
            existing_base(root, output)
                .with_context(|| format!("preparing output {output} of trigger {label}"))?,
        );
    }
    let bases: Vec<_> = bases
        .iter()
        .filter(|base| {
            !bases
                .iter()
                .any(|other| other != *base && base.starts_with(&format!("{other}/")))
        })
        .cloned()
        .collect();

    let scratch = path.join(STATE_DIRECTORY).join("stage").join(SCRATCH);
    remove_scratch(&scratch)?;
    fs::create_dir(&scratch).context("creating trigger scratch directory")?;
    let result = (|| -> Result<()> {
        let layers = bases
            .into_iter()
            .enumerate()
            .map(|(index, base)| -> Result<Layer> {
                let upper = scratch.join(format!("upper-{index}"));
                let work = scratch.join(format!("work-{index}"));
                fs::create_dir(&upper)?;
                fs::create_dir(&work)?;
                Ok(Layer { base, upper, work })
            })
            .collect::<Result<Vec<_>>>()?;
        execute(path, &layers, trigger, handler, &label)?;
        let others: Vec<&str> = active(&next.installed, &next.paths)
            .iter()
            .filter(|&(&key, _)| key != planned.key())
            .flat_map(|(_, active)| active.trigger.outputs.iter().map(String::as_str))
            .collect();
        let mut changes = Vec::new();
        for layer in &layers {
            collect(
                root,
                &layer.upper,
                &layer.base,
                trigger,
                &others,
                &mut changes,
            )
            .with_context(|| format!("trigger {label} wrote outside its declared outputs"))?;
        }
        publish(root, stage, &changes)
            .with_context(|| format!("publishing outputs of trigger {label}"))
    })();
    let cleanup = remove_scratch(&scratch);
    result?;
    cleanup
}

/// Find the directory that a trigger's overlay covers for an output.
fn existing_base(root: &Root, output: &str) -> Result<String> {
    let mut base = static_base(output);
    loop {
        ensure!(
            !base.is_empty()
                && !STATE_ANCESTORS.contains(&base.as_str())
                && !STATE_DIRECTORY.starts_with(&format!("{base}/")),
            "the image must contain the directory that holds {output}; ship it in the trigger's package"
        );
        match kind(root, &base)? {
            Some(FileType::Directory) => return Ok(base),
            None => {}
            Some(_) => bail!("{base} is not a directory"),
        }
        base = match base.rsplit_once('/') {
            Some((parent, _)) => parent.to_owned(),
            None => String::new(),
        };
    }
}

fn remove_scratch(scratch: &Path) -> Result<()> {
    if fs::symlink_metadata(scratch).is_err() {
        return Ok(());
    }
    // Overlay work directories contain an inaccessible directory.
    fn open_up(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            for entry in fs::read_dir(path)? {
                open_up(&entry?.path())?;
            }
        }
        Ok(())
    }
    open_up(scratch).context("opening trigger scratch directory")?;
    fs::remove_dir_all(scratch).context("removing trigger scratch directory")
}

fn execute(
    path: &Path,
    layers: &[Layer],
    trigger: &Trigger,
    handler: &str,
    label: &str,
) -> Result<()> {
    ensure!(
        Path::new(BWRAP).is_file(),
        "running trigger {label} requires Bubblewrap at {BWRAP}"
    );
    let privileged = rustix::process::geteuid().is_root();
    let filter = crate::build::sandbox::filter(false).context("creating seccomp trigger policy")?;
    let mut command = Command::new(BWRAP);
    command.env_clear().args([
        "--unshare-ipc",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--die-with-parent",
        "--new-session",
        "--hostname",
        "cps-trigger",
        "--cap-drop",
        "ALL",
    ]);
    if privileged {
        // A live image runs its handlers as root, with only the file
        // ownership capabilities that cache tools rely on.
        for capability in ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER"] {
            command.args(["--cap-add", capability]);
        }
    } else {
        command.args([
            "--unshare-user",
            "--disable-userns",
            "--uid",
            "0",
            "--gid",
            "0",
        ]);
    }
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    for name in entries {
        let Some(text) = name.to_str() else {
            continue;
        };
        if HIDDEN.contains(&text) {
            continue;
        }
        let source = path.join(&name);
        let destination = Path::new("/").join(&name);
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            command
                .arg("--symlink")
                .arg(fs::read_link(&source)?)
                .arg(destination);
        } else if metadata.is_dir() || metadata.is_file() {
            command.arg("--ro-bind").arg(source).arg(destination);
        }
    }
    let state = STATE_ANCESTORS[STATE_ANCESTORS.len() - 1];
    if path.join(state).is_dir() {
        let hidden = format!("/{state}");
        command.args(["--tmpfs", &hidden, "--remount-ro", &hidden]);
    }
    for layer in layers {
        command
            .arg("--overlay-src")
            .arg(path.join(&layer.base))
            .arg("--overlay")
            .arg(&layer.upper)
            .arg(&layer.work)
            .arg(format!("/{}", layer.base));
    }
    command.args([
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/run",
        "--remount-ro",
        "/",
        "--chdir",
        "/",
        "--clearenv",
    ]);
    for (key, value) in [
        ("PATH", "/usr/sbin:/usr/bin"),
        ("LC_ALL", "C"),
        ("LANG", "C"),
        ("TZ", "UTC"),
        ("HOME", "/tmp"),
        ("TMPDIR", "/tmp"),
        ("CPS_TRIGGER", label),
    ] {
        command.args(["--setenv", key, value]);
    }
    command
        .arg("--seccomp")
        .arg(filter.as_raw_fd().to_string())
        .arg("--")
        .arg(format!("/{handler}"))
        .args(&trigger.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(io::stderr().as_fd().try_clone_to_owned()?));
    // Only async-signal-safe system calls run here.
    unsafe {
        command.pre_exec(move || {
            for (resource, limit) in [(libc::RLIMIT_CORE, 0), (libc::RLIMIT_FSIZE, FILE_LIMIT)] {
                let mut inherited = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(resource, &mut inherited) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let limit = libc::rlimit {
                    rlim_cur: inherited.rlim_cur.min(limit),
                    rlim_max: inherited.rlim_max.min(limit),
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as _) != 0 {
                return Err(io::Error::last_os_error());
            }
            fcntl_setfd(&filter, FdFlags::empty())?;
            Ok(())
        });
    }
    let timeout = Duration::from_secs(trigger.timeout.unwrap_or(DEFAULT_TIMEOUT).into());
    let mut child = command
        .spawn()
        .with_context(|| format!("starting trigger {label}"))?;
    drop(command);
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "trigger {label} failed with {status}");
            return Ok(());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            child.wait()?;
            bail!(
                "trigger {label} did not finish within {} seconds",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

enum Change {
    Dir {
        path: String,
        node: Node,
        replace: bool,
    },
    File {
        path: String,
        source: PathBuf,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    Symlink {
        path: String,
        node: Node,
    },
    Remove {
        path: String,
    },
}

fn opaque(path: &Path) -> Result<bool> {
    for name in ["user.overlay.opaque", "trusted.overlay.opaque"] {
        let mut value = [0_u8; 1];
        match rustix::fs::lgetxattr(path, name, &mut value[..]) {
            Ok(1) if value == *b"y" => return Ok(true),
            Ok(_) | Err(Errno::NODATA | Errno::NOTSUP | Errno::RANGE) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

/// Turn an overlay's upper directory into changes, refusing every change
/// that is not a declared output of the trigger.
fn collect(
    root: &Root,
    upper: &Path,
    logical: &str,
    trigger: &Trigger,
    others: &[&str],
    changes: &mut Vec<Change>,
) -> Result<()> {
    let mut entries = fs::read_dir(upper)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    for name in entries {
        let name = name
            .into_string()
            .map_err(|name| anyhow::anyhow!("non-UTF-8 name {name:?} in {logical}"))?;
        let source = upper.join(&name);
        let path = format!("{logical}/{name}");
        validate_path(&path)?;
        let output = trigger.generates(&path);
        ensure!(
            !output || !generated(others, &path),
            "{path} is also generated by another trigger"
        );
        let metadata = fs::symlink_metadata(&source)?;
        let file_type = metadata.file_type();
        let mode = metadata.mode() & 0o777;
        let (uid, gid) = (metadata.uid(), metadata.gid());
        if file_type.is_char_device() && metadata.rdev() == 0 {
            ensure!(output, "removed {path}");
            changes.push(Change::Remove { path });
        } else if file_type.is_dir() {
            let replace = opaque(&source)?;
            if output {
                changes.push(Change::Dir {
                    path: path.clone(),
                    node: Node::Dir { mode, uid, gid },
                    replace,
                });
            } else {
                // Overlayfs copies up the unchanged ancestors of a change.
                let existing = root.inspect(&path)?;
                ensure!(
                    !replace
                        && matches!(existing, Some(Node::Dir { mode: old, uid: old_uid, gid: old_gid })
                            if old & 0o777 == mode && (old_uid, old_gid) == (uid, gid)),
                    "changed directory {path}"
                );
            }
            collect(root, &source, &path, trigger, others, changes)?;
        } else if file_type.is_file() {
            ensure!(output, "wrote {path}");
            changes.push(Change::File {
                path,
                source,
                mode,
                uid,
                gid,
            });
        } else if file_type.is_symlink() {
            ensure!(output, "wrote {path}");
            let target = fs::read_link(&source)?
                .into_os_string()
                .into_string()
                .map_err(|target| anyhow::anyhow!("non-UTF-8 link target {target:?}"))?;
            changes.push(Change::Symlink {
                path,
                node: Node::Symlink { target, uid, gid },
            });
        } else {
            bail!("created special file {path}");
        }
    }
    Ok(())
}

fn publish(root: &Root, stage: &Root, changes: &[Change]) -> Result<()> {
    let mut syncs = DirSyncs::default();
    let staging = stage.as_file();
    for change in changes {
        match change {
            Change::Remove { path } => remove_tree(root, path, &mut syncs)?,
            Change::Dir {
                path,
                node,
                replace,
            } => {
                let existing = kind(root, path)?;
                if *replace || existing.is_some_and(|kind| kind != FileType::Directory) {
                    remove_tree(root, path, &mut syncs)?;
                }
                match root.inspect(path)? {
                    None => root.replace(path, node, None, staging, true, &mut syncs)?,
                    Some(current) if current == *node => {}
                    Some(_) => Root::set_directory_metadata(&root.dir(path)?, node)?,
                }
            }
            Change::File {
                path,
                source,
                mode,
                uid,
                gid,
            } => {
                if kind(root, path)? == Some(FileType::Directory) {
                    remove_tree(root, path, &mut syncs)?;
                }
                let mut file = File::open(source)?;
                let size = file.metadata()?.len();
                let sha256 = digest_reader(&mut file)?;
                let node = Node::File {
                    sha256,
                    size,
                    mode: *mode,
                    uid: *uid,
                    gid: *gid,
                    capabilities: None,
                };
                root.replace(
                    path,
                    &node,
                    Some(ReplacementSource::Staged(File::open(source)?)),
                    staging,
                    false,
                    &mut syncs,
                )?;
            }
            Change::Symlink { path, node } => {
                if kind(root, path)? == Some(FileType::Directory) {
                    remove_tree(root, path, &mut syncs)?;
                }
                root.replace(path, node, None, staging, false, &mut syncs)?;
            }
        }
    }
    root.sync_dirs(&mut syncs, Some(staging))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_cover_trees_and_intersect() {
        assert!(covers(
            "usr/lib/modules/*/initrd",
            "usr/lib/modules/6.1/initrd"
        ));
        assert!(covers("var/cache/fontconfig", "var/cache/fontconfig/a/b"));
        assert!(!covers("var/cache/fontconfig", "var/cache"));
        assert!(!covers(
            "usr/share/icons/*/icon-theme.cache",
            "usr/share/icons/hicolor"
        ));
        assert!(intersects(
            "usr/lib/modules/*/modules.*",
            "usr/lib/modules/**"
        ));
        assert!(intersects("usr/lib/modules/*/initrd", "usr/lib/modules"));
        assert!(intersects("usr/share/*/x", "usr/*/y/**"));
        assert!(!intersects("usr/share/*/x", "usr/lib/*/x"));
        assert!(intersects("usr/share/a*", "usr/share/*b"));
        assert!(!intersects(
            "usr/lib/modules/*/initrd",
            "usr/lib/modules/*/kernel/**"
        ));
        assert!(!intersects(
            "etc/udev/hwdb.bin",
            "usr/lib/udev/hwdb.d/*.hwdb"
        ));
        assert!(intersects(
            "var/cache/fontconfig",
            "var/cache/fontconfig/*.cache-9"
        ));
    }

    #[test]
    fn outputs_need_a_shipped_parent_outside_image_state() {
        assert_eq!(static_base("etc/udev/hwdb.bin"), "etc/udev");
        assert_eq!(
            static_base("usr/share/icons/*/icon-theme.cache"),
            "usr/share/icons"
        );
        assert_eq!(static_base("var/cache/fontconfig"), "var/cache");
        for invalid in [
            "run/cache/x",
            "hwdb.bin",
            "*/x",
            "var/x",
            "var/lib/x",
            "var/lib/cps/image/x",
            "var/lib/**",
        ] {
            assert!(validate_output(invalid).is_err(), "accepted {invalid}");
        }
        validate_output("var/cache/fontconfig").unwrap();
    }
}
