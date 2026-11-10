use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Seek, Write as _},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::{
    image::{Installed, Managed, State, root::Root},
    model::Node,
    plan::{Plan, Write},
};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(
    tag = "kind",
    content = "name",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Action {
    Sysusers(String),
    Tmpfiles(String),
    Preset(String),
}

impl Action {
    pub fn kind(&self) -> &str {
        match self {
            Self::Sysusers(_) => "sysusers",
            Self::Tmpfiles(_) => "tmpfiles",
            Self::Preset(_) => "preset",
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Sysusers(name) | Self::Tmpfiles(name) | Self::Preset(name) => name,
        }
    }

    pub fn path(&self) -> String {
        let directory = match self {
            Self::Sysusers(_) => "usr/lib/sysusers.d",
            Self::Tmpfiles(_) => "usr/lib/tmpfiles.d",
            Self::Preset(_) => "usr/lib/systemd/system",
        };
        format!("{directory}/{}", self.name())
    }

    pub fn validate(&self) -> Result<()> {
        let name = self.name();
        let valid_suffix = match self {
            Self::Sysusers(_) | Self::Tmpfiles(_) => name.ends_with(".conf"),
            Self::Preset(_) => name.rsplit_once('.').is_some_and(|(_, suffix)| {
                matches!(
                    suffix,
                    "service"
                        | "socket"
                        | "timer"
                        | "path"
                        | "target"
                        | "mount"
                        | "automount"
                        | "swap"
                        | "slice"
                )
            }),
        };
        ensure!(
            name.len() <= 255
                && valid_suffix
                && name.starts_with(|c: char| c.is_ascii_alphanumeric())
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c)),
            "invalid {} name {name:?}",
            self.kind()
        );
        Ok(())
    }
}

pub(crate) fn plan_actions(
    state: &State,
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
) -> Vec<Action> {
    let previous = active_actions(&state.installed, &state.paths);
    let next = active_actions(installed, paths);
    next.iter()
        .filter_map(|(&action, &input)| {
            let changed = match action {
                Action::Preset(_) => !state.paths.contains_key(&action.path()),
                _ => previous.get(action) != Some(&input),
            };
            changed.then(|| action.clone())
        })
        .collect()
}

fn active_actions<'a>(
    installed: &'a BTreeMap<String, Installed>,
    paths: &'a BTreeMap<String, Managed>,
) -> BTreeMap<&'a Action, &'a Node> {
    let mut actions = BTreeMap::new();
    for (name, package) in installed {
        for action in &package.published.manifest.systemd {
            if let Some(file) = paths.get(&action.path())
                && file.owners.contains(name)
            {
                actions.insert(action, &file.node);
            }
        }
    }
    actions
}

/// Accept only rules that create missing paths: directories, files,
/// symlinks, and copies, such as a copy of `/usr/share/factory/var/...` that
/// seeds the shared `/var`. Paths are absolute and unquoted, without `.` or
/// `..`, so that CPS sees the same path that `systemd-tmpfiles` writes.
pub(crate) fn validate_tmpfiles(bytes: &[u8]) -> Result<()> {
    for (index, line) in std::str::from_utf8(bytes)?.lines().enumerate() {
        let Some(rule) = TmpfilesRule::parse(line) else {
            continue;
        };
        let line = index + 1;
        ensure!(
            matches!(rule.kind, "d" | "f" | "L" | "C" | "d!" | "f!" | "L!" | "C!"),
            "tmpfiles line {line} uses unsupported offline rule {:?}; use d, f, L, or C",
            rule.kind
        );
        let path = rule
            .path
            .context(format!("tmpfiles line {line} has no path"))?;
        ensure!(
            (path.starts_with('/') || path.starts_with('%'))
                && !path.contains(['"', '\'', '\\'])
                && path
                    .split('/')
                    .skip(1)
                    .all(|part| !matches!(part, "." | "..")),
            "tmpfiles line {line} has path {path:?}; use an absolute, unquoted path without . or .."
        );
    }
    Ok(())
}

/// The fields of a tmpfiles.d line that decide where it writes.
struct TmpfilesRule<'a> {
    kind: &'a str,
    path: Option<&'a str>,
    /// The symlink target of `L`, or the source of `C`.
    argument: Option<&'a str>,
}

impl<'a> TmpfilesRule<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        fn field(text: &str) -> (Option<&str>, &str) {
            let text = text.trim_start_matches([' ', '\t']);
            match text.find([' ', '\t']) {
                _ if text.is_empty() => (None, text),
                Some(end) => (Some(&text[..end]), &text[end..]),
                None => (Some(text), ""),
            }
        }
        let (kind, mut rest) = field(line);
        let kind = kind.filter(|kind| !kind.starts_with('#'))?;
        let (path, after) = field(rest);
        rest = after;
        for _ in 0..4 {
            rest = field(rest).1;
        }
        let argument =
            Some(rest.trim()).filter(|argument| !argument.is_empty() && *argument != "-");
        Some(Self {
            kind,
            path,
            argument,
        })
    }
}

/// The root-relative trees that tmpfiles does not write beneath, as `run`
/// excludes them: the CPS image state, the kernel file systems, and for an
/// inactive root, `-E`'s /run and the shared /var.
fn tmpfiles_exclusions(live: bool) -> Vec<&'static str> {
    let mut excluded = vec![crate::image::STATE_DIRECTORY, "dev", "proc", "sys"];
    if !live {
        excluded.extend(["run", crate::image::VAR_DIRECTORY]);
    }
    excluded
}

fn beneath(path: &[String], prefix: &str) -> bool {
    let prefix = prefix.split('/').collect::<Vec<_>>();
    path.len() >= prefix.len() && path.iter().zip(&prefix).all(|(part, want)| part == want)
}

/// Where a tmpfiles path lands in the root. `systemd-tmpfiles --root` follows
/// the root's symlinks, with absolute targets taken from the root, so a rule
/// for `/lib/sysimage/cps/x` or for `/srv/x`, when `/srv` links to `/var/srv`,
/// writes beneath a prefix that its text does not name. `links` holds the
/// symlinks that earlier rules of the same run create. A component with a
/// specifier such as `%m` names a machine value, so resolution stops there.
fn resolve_tmpfiles_path(
    root: &fs::File,
    path: &str,
    follow_last: bool,
    links: &BTreeMap<Vec<String>, String>,
) -> Result<Vec<String>> {
    let mut parts = path
        .split('/')
        .map(str::to_owned)
        .collect::<std::collections::VecDeque<_>>();
    // systemd's directory specifiers, as a system-mode run expands them.
    if let Some(first) = parts.front()
        && let Some(directory) = match first.as_str() {
            "%S" => Some("var/lib"),
            "%C" => Some("var/cache"),
            "%L" => Some("var/log"),
            "%t" => Some("run"),
            "%T" => Some("tmp"),
            "%V" => Some("var/tmp"),
            "%h" => Some("root"),
            _ => None,
        }
    {
        parts.pop_front();
        for part in directory.rsplit('/') {
            parts.push_front(part.to_owned());
        }
    }
    let mut resolved = Vec::<String>::new();
    let mut hops = 0;
    while let Some(part) = parts.pop_front() {
        match part.as_str() {
            "" | "." => continue,
            ".." => {
                resolved.pop();
                continue;
            }
            _ if part.contains('%') => {
                resolved.push(part);
                resolved.extend(parts);
                break;
            }
            _ => {}
        }
        resolved.push(part);
        if parts.is_empty() && !follow_last {
            break;
        }
        let target = match links.get(&resolved) {
            Some(target) => Some(target.clone()),
            None => {
                let relative = resolved.join("/");
                match rustix::fs::statat(root, &relative, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(info) if rustix::fs::FileType::from_raw_mode(info.st_mode).is_symlink() => {
                        let target = rustix::fs::readlinkat(root, &relative, Vec::new())?;
                        Some(target.into_string().context("non-UTF-8 symlink target")?)
                    }
                    Ok(_) | Err(Errno::NOENT | Errno::NOTDIR) => None,
                    Err(error) => {
                        return Err(error).with_context(|| format!("inspecting /{relative}"));
                    }
                }
            }
        };
        if let Some(target) = target {
            hops += 1;
            ensure!(
                hops <= 40,
                "too many symlinks resolving tmpfiles path {path}"
            );
            resolved.pop();
            if target.starts_with('/') {
                resolved.clear();
            }
            for part in target.rsplit('/') {
                parts.push_front(part.to_owned());
            }
        }
    }
    Ok(resolved)
}

/// A tmpfiles rule type and the root-relative components of where it writes.
type Resolved<'a> = (&'a str, Vec<String>);

/// Resolve the lines of tmpfiles configurations to where they write, and
/// drop the lines that write beneath an excluded tree. Their text may name
/// another path, so each one is resolved in the root first. Like the
/// exclusions of `systemd-tmpfiles` itself, a dropped line is not an error:
/// systemd-tmpfiles-setup.service creates the /var paths at boot.
fn resolve_tmpfiles<'a>(
    root: &Root,
    rules: &'a str,
    live: bool,
) -> Result<Vec<(&'a str, Option<Resolved<'a>>)>> {
    let root = root.as_file();
    let excluded = tmpfiles_exclusions(live);
    let is_excluded = |path: &[String]| excluded.iter().any(|prefix| beneath(path, prefix));
    let parsed = rules
        .lines()
        .map(|line| (line, TmpfilesRule::parse(line)))
        .collect::<Vec<_>>();

    // The symlinks that these rules create, as later rules traverse them.
    let mut links = BTreeMap::new();
    for (_, rule) in &parsed {
        if let Some(rule) = rule
            && rule.kind.starts_with('L')
            && let Some(path) = rule.path
        {
            let location = resolve_tmpfiles_path(root, path, false, &links)?;
            if !is_excluded(&location) {
                let target = rule
                    .argument
                    .map_or_else(|| format!("/usr/share/factory{path}"), str::to_owned);
                links.insert(location, target);
            }
        }
    }

    let mut copies = Vec::new();
    let mut kept = Vec::new();
    for (line, rule) in parsed {
        let Some(rule) = rule else {
            kept.push((line, None));
            continue;
        };
        let path = rule.path.context("tmpfiles rule without a path")?;
        let location = resolve_tmpfiles_path(root, path, !rule.kind.starts_with('L'), &links)?;
        if is_excluded(&location) {
            continue;
        }
        if rule.kind.starts_with('C')
            && rustix::fs::statat(
                root,
                location.join("/"),
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .is_err()
        {
            copies.push((location.clone(), path));
        }
        kept.push((line, Some((rule.kind, location))));
    }
    for (_, rule) in &kept {
        if let Some((_, location)) = rule
            && let Some((_, copy)) = copies
                .iter()
                .find(|(copy, _)| location.len() > copy.len() && beneath(location, &copy.join("/")))
        {
            bail!(
                "tmpfiles rule for /{} lies inside the tree that the rule for {copy} copies; \
                 CPS cannot tell where it writes. Ship the path in the copied tree instead",
                location.join("/")
            );
        }
    }
    Ok(kept)
}

/// The lines of tmpfiles configurations that do not write beneath an
/// excluded tree.
fn filter_tmpfiles(root: &Root, rules: &str, live: bool) -> Result<String> {
    let mut output = String::new();
    for (line, _) in resolve_tmpfiles(root, rules, live)? {
        output.push_str(line);
        output.push('\n');
    }
    Ok(output)
}

/// The rule types and resolved locations of the kept tmpfiles rules.
fn tmpfiles_locations<'a>(root: &Root, rules: &'a str, live: bool) -> Result<Vec<Resolved<'a>>> {
    Ok(resolve_tmpfiles(root, rules, live)?
        .into_iter()
        .filter_map(|(_, rule)| rule)
        .collect())
}

pub(crate) fn run(
    root: &Root,
    path: &Path,
    actions: &[Action],
    state: &State,
    live: bool,
) -> Result<()> {
    let runtime = live.then(|| Root::open(Path::new("/run"))).transpose()?;
    let mut sysusers = Vec::new();
    let mut presets = Vec::new();
    let state_path = crate::image::STATE_DIRECTORY;
    let mut configs = String::new();
    for action in actions {
        action.validate()?;
        let vendor_path = action.path();
        let input = state
            .paths
            .get(&vendor_path)
            .context("missing planned systemd input")?;
        ensure!(
            root.inspect(&vendor_path)?.as_ref() == Some(&input.node),
            "systemd input differs from its manifest at {vendor_path}"
        );
        match action {
            Action::Sysusers(name) => sysusers.push(name),
            Action::Tmpfiles(name) => {
                if let Some(bytes) = tmpfiles_config(root, runtime.as_ref(), name)? {
                    validate_tmpfiles(&bytes).with_context(|| format!("validating {name}"))?;
                    configs.push_str(std::str::from_utf8(&bytes)?);
                    configs.push('\n');
                }
            }
            Action::Preset(unit) => {
                // Administrator unit overrides, including masks, take priority.
                if !unit_overridden(root, runtime.as_ref(), unit)? {
                    presets.push(unit);
                }
            }
        }
    }

    if !sysusers.is_empty() {
        run_tool(
            command("systemd-sysusers", path).arg("--").args(sysusers),
            "sysusers",
            live,
        )?;
    }
    let configs = filter_tmpfiles(root, &configs, live)?;
    if configs
        .lines()
        .any(|line| TmpfilesRule::parse(line).is_some())
    {
        let mut rules = tempfile::tempfile_in(path.join(state_path).join("stage"))?;
        rules.write_all(configs.as_bytes())?;
        rules.rewind()?;
        let mut tool = command("systemd-tmpfiles", path);
        if live {
            tool.args([
                "--exclude-prefix=/dev",
                "--exclude-prefix=/proc",
                "--exclude-prefix=/sys",
            ]);
        } else {
            // An inactive root leaves /var empty: in a boot environment it
            // is the mount point of the /var that every environment shares,
            // and systemd-tmpfiles-setup.service creates these paths there
            // at boot, like the paths under /dev, /proc, /run and /sys.
            // These prefixes match the text of a rule; filter_tmpfiles has
            // already dropped the rules that reach them through a symlink.
            tool.args(["-E", "--exclude-prefix=/var"]);
        }
        run_tool(
            tool.arg("--create")
                .arg(format!("--exclude-prefix=/{state_path}"))
                .args(["--", "-"])
                .stdin(Stdio::from(rules)),
            "tmpfiles",
            live,
        )?;
    }
    if !presets.is_empty() {
        run_tool(
            command("systemctl", path)
                .args(["--no-reload", "--preset-mode=enable-only", "preset", "--"])
                .args(presets),
            "presets",
            live,
        )?;
    }
    root.sync()?;
    Ok(())
}

/// The local state that the active systemd actions of an inactive root write,
/// which pristine verification accepts like trigger outputs.
#[derive(Debug, Default)]
pub(crate) struct ActionOutputs {
    /// Exact paths: the account databases, and what tmpfiles rules create
    /// and the directories above them.
    paths: BTreeSet<String>,
    /// Trees that tmpfiles copies.
    trees: Vec<String>,
    /// Whether presets are active, so enablement links are expected.
    presets: bool,
}

impl ActionOutputs {
    pub(crate) fn of(root: &Root, state: &State) -> Result<Self> {
        let mut outputs = Self::default();
        let mut configs = String::new();
        for action in active_actions(&state.installed, &state.paths).into_keys() {
            match action {
                Action::Sysusers(_) => {
                    for database in ["passwd", "group", "shadow", "gshadow"] {
                        outputs.paths.insert(format!("etc/{database}"));
                        outputs.paths.insert(format!("etc/{database}-"));
                    }
                    outputs.paths.insert("etc/.pwd.lock".to_owned());
                }
                Action::Tmpfiles(name) => {
                    if let Some(bytes) = tmpfiles_config(root, None, name)? {
                        configs.push_str(std::str::from_utf8(&bytes)?);
                        configs.push('\n');
                    }
                }
                Action::Preset(_) => outputs.presets = true,
            }
        }
        for (kind, location) in tmpfiles_locations(root, &configs, false)? {
            let path = location.join("/");
            for (index, _) in path.match_indices('/') {
                outputs.paths.insert(path[..index].to_owned());
            }
            if kind.starts_with('C') {
                outputs.trees.push(path.clone());
            }
            outputs.paths.insert(path);
        }
        Ok(outputs)
    }

    /// Whether an action wrote this unmanaged path. The entries of a
    /// directory that an action creates still need an owner of their own.
    pub(crate) fn covers(&self, path: &str, entry: &fs::File, kind: rustix::fs::FileType) -> bool {
        use rustix::fs::FileType;
        let in_tree = |tree: &String| {
            path.strip_prefix(tree.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        };
        if self.paths.contains(path) || self.trees.iter().any(in_tree) {
            return true;
        }
        if !self.presets {
            return false;
        }
        let units = "etc/systemd/system";
        let unit = path
            .strip_prefix(units)
            .and_then(|rest| rest.strip_prefix('/'));
        match (kind, unit) {
            (FileType::Directory, _) if path == "etc/systemd" || path == units => true,
            (FileType::Directory, Some(unit))
                if !unit.contains('/')
                    && [".wants", ".requires", ".upholds"]
                        .iter()
                        .any(|suffix| unit.ends_with(suffix)) =>
            {
                true
            }
            // `systemctl preset` links each enabled unit and alias to its
            // vendor unit by absolute path.
            (FileType::Symlink, Some(_)) => rustix::fs::readlinkat(entry, "", Vec::new())
                .ok()
                .and_then(|target| target.into_string().ok())
                .and_then(|target| {
                    target
                        .strip_prefix("/usr/lib/systemd/system/")
                        .map(|name| !name.is_empty() && !name.contains('/'))
                })
                .unwrap_or(false),
            _ => false,
        }
    }
}

fn unit_overridden(root: &Root, runtime: Option<&Root>, unit: &str) -> Result<bool> {
    for directory in ["etc", "run", "usr/local/lib"] {
        let (tree, prefix) = if directory == "run"
            && let Some(runtime) = runtime
        {
            (runtime, String::new())
        } else {
            (root, format!("{directory}/"))
        };
        if tree
            .inspect(&format!("{prefix}systemd/system/{unit}"))?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn tmpfiles_config(root: &Root, runtime: Option<&Root>, name: &str) -> Result<Option<Vec<u8>>> {
    // Read and validate the exact bytes given to tmpfiles. A target-local
    // replacement or mask takes precedence over the packaged configuration.
    for directory in ["etc", "run", "usr/local/lib", "usr/lib"] {
        let (tree, prefix) = if directory == "run"
            && let Some(runtime) = runtime
        {
            (runtime, String::new())
        } else {
            (root, format!("{directory}/"))
        };
        let path = format!("{prefix}tmpfiles.d/{name}");
        match tree.inspect(&path)? {
            None => {}
            Some(Node::Symlink { target, .. }) if target == "/dev/null" => return Ok(None),
            Some(Node::File { .. }) => return Ok(Some(tree.read_file(&path)?)),
            Some(_) => bail!("invalid tmpfiles configuration at {path}"),
        }
    }
    bail!("missing tmpfiles configuration {name}")
}

/// Discard links to retired vendor units.
pub(crate) fn clean_enablement(
    root: &Root,
    writes: &[Write],
    paths: &BTreeMap<String, Managed>,
) -> Result<()> {
    let targets = writes
        .iter()
        .filter(|write| {
            write.after.is_none() && write.before.as_ref().is_some_and(|node| !node.is_dir())
        })
        .filter_map(|write| {
            let name = write.path.strip_prefix("usr/lib/systemd/system/")?;
            let preset = Action::Preset(name.to_owned());
            preset.validate().ok().map(|()| format!("/{}", write.path))
        })
        .collect::<BTreeSet<_>>();
    if targets.is_empty() {
        return Ok(());
    }

    let directory = "etc/systemd/system";
    let units = match root.dir(directory) {
        Ok(units) => units,
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error).context("opening persistent systemd enablement"),
    };
    let remove_link = |path: &str| -> Result<()> {
        if !paths.contains_key(path)
            && let Some(Node::Symlink { target, .. }) = root.inspect(path)?
            && targets.contains(&target)
        {
            root.remove_file(path)?;
        }
        Ok(())
    };
    for entry in fs::read_dir(Root::fd_path(&units))? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = format!("{directory}/{name}");
        let kind = entry.file_type()?;
        if kind.is_symlink() && !targets.contains(&format!("/usr/lib/systemd/system/{name}")) {
            // A same-name entry here is a local unit override.
            remove_link(&path)?;
        } else if kind.is_dir()
            && [".wants", ".requires", ".upholds"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
        {
            let dependencies = root.dir(&path)?;
            for link in fs::read_dir(Root::fd_path(&dependencies))? {
                let link = link?;
                if link.file_type()?.is_symlink()
                    && let Ok(name) = link.file_name().into_string()
                {
                    remove_link(&format!("{path}/{name}"))?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) struct LiveServices {
    reload: bool,
    restart: BTreeSet<String>,
    stop: BTreeSet<String>,
    /// Services that triggers reload or restart after they run, if active.
    trigger_reload: BTreeSet<String>,
    trigger_restart: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LiveJobs {
    reload: bool,
    restart: Vec<String>,
    /// Active services whose unit files the transaction removes, to stop
    /// before writing files.
    stop: Vec<String>,
    /// Services from `stop` that have been stopped. The journal records
    /// them before and after stopping, so an abort can start them again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stopped: Vec<String>,
    /// Services to start after writing files: those that the aborted
    /// transaction stopped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    start: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trigger_reload: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trigger_restart: Vec<String>,
}

impl LiveServices {
    pub(crate) fn preview(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.stop.is_empty() {
            lines.push(format!(
                "Stop active services matching {}",
                self.stop.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if self.reload {
            lines.push("Reload the systemd manager".into());
        }
        if !self.restart.is_empty() {
            lines.push(format!(
                "Restart active services matching {}",
                self.restart.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.trigger_reload.is_empty() {
            lines.push(format!(
                "Reload or restart active services {} after triggers",
                self.trigger_reload
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.trigger_restart.is_empty() {
            lines.push(format!(
                "Restart active services {} after triggers",
                self.trigger_restart
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }

    pub(crate) fn plan(plan: &Plan, current: &State, next: &State) -> Self {
        let changed = plan
            .package_changes()
            .iter()
            .map(|change| change.name.as_str())
            .collect::<BTreeSet<_>>();
        // The plan was verified against these manifests, so its triggers exist.
        let (trigger_reload, trigger_restart) =
            crate::triggers::actuators(&plan.triggers, &next.installed).unwrap_or_default();
        let mut services = Self {
            reload: plan
                .systemd
                .iter()
                .any(|action| matches!(action, Action::Preset(_))),
            restart: BTreeSet::new(),
            stop: BTreeSet::new(),
            trigger_reload,
            trigger_restart,
        };
        for write in &plan.writes {
            if write.before == write.after && !write.detach {
                continue;
            }
            if let Some(path) = unit_path(&write.path) {
                services.reload = true;
                let name = path.split('/').next().unwrap_or(path);
                let name = name.strip_suffix(".d").unwrap_or(name);
                if service_name(name) {
                    services.restart.insert(name.to_owned());
                }
            }
        }
        for (path, managed) in &current.paths {
            let Some(unit) = unit_path(path).filter(|name| service_name(name)) else {
                continue;
            };
            if !next.paths.contains_key(path) {
                services.stop.insert(unit.to_owned());
                services.restart.remove(unit);
            } else if managed
                .owners
                .iter()
                .any(|owner| changed.contains(owner.as_str()))
            {
                services.restart.insert(unit.to_owned());
            }
        }
        services
    }

    pub(crate) fn active_jobs(&self) -> Result<LiveJobs> {
        let mut jobs = LiveJobs {
            reload: self.reload,
            stop: Vec::new(),
            stopped: Vec::new(),
            start: Vec::new(),
            restart: Vec::new(),
            trigger_reload: self.trigger_reload.iter().cloned().collect(),
            trigger_restart: self.trigger_restart.iter().cloned().collect(),
        };
        if !self.restart.is_empty() || !self.stop.is_empty() {
            let output = live_systemctl()
                .args([
                    "list-units",
                    "--type=service",
                    "--state=active,activating,reloading",
                    "--plain",
                    "--no-legend",
                    "--full",
                ])
                .output()
                .context("listing running services")?;
            ensure!(
                output.status.success(),
                "listing running services failed with {}",
                output.status
            );
            let units = std::str::from_utf8(&output.stdout)?;
            for unit in units
                .lines()
                .filter_map(|line| line.split_ascii_whitespace().next())
                .filter(|name| service_name(name))
            {
                if matches_service(&self.stop, unit) {
                    jobs.stop.push(unit.to_owned());
                } else if matches_service(&self.restart, unit) {
                    jobs.restart.push(unit.to_owned());
                }
            }
        }
        Ok(jobs)
    }

    pub(crate) fn check_jobs(&self, jobs: &LiveJobs) -> Result<()> {
        ensure!(
            self.reload == jobs.reload
                && jobs.trigger_reload.iter().eq(&self.trigger_reload)
                && jobs.trigger_restart.iter().eq(&self.trigger_restart)
                && jobs
                    .stop
                    .iter()
                    .chain(&jobs.stopped)
                    .all(|unit| service_name(unit) && matches_service(&self.stop, unit))
                && jobs
                    .restart
                    .iter()
                    .chain(&jobs.start)
                    .all(|unit| service_name(unit) && matches_service(&self.restart, unit)),
            "pending systemd jobs differ from the transaction"
        );
        Ok(())
    }
}

impl LiveJobs {
    /// Stop the services in `stop`. Returns whether the caller must
    /// journal the jobs again, now that `stopped` holds those services.
    pub(crate) fn stop_removed_units(&mut self) -> Result<bool> {
        if self.stop.is_empty() {
            return Ok(false);
        }
        run_live_job(
            live_systemctl().args(["stop", "--"]).args(&self.stop),
            "stop",
        )?;
        self.mark_stopped();
        Ok(true)
    }

    fn mark_stopped(&mut self) {
        for unit in std::mem::take(&mut self.stop) {
            if !self.stopped.contains(&unit) {
                self.stopped.push(unit);
            }
        }
    }

    /// Start, after an abort restores their unit files, the services that
    /// the aborted transaction stopped or was about to stop. Starting an
    /// active service changes nothing, so a service whose stop the
    /// transaction never journaled as done is covered too.
    pub(crate) fn start_stopped_by(&mut self, aborted: &Self) {
        for unit in aborted.stop.iter().chain(&aborted.stopped) {
            if !self.restart.contains(unit) && !self.start.contains(unit) {
                self.start.push(unit.clone());
            }
        }
    }

    /// The systemctl invocations that follow the file writes, in order.
    fn jobs(&self) -> Vec<(&'static str, Vec<&str>)> {
        let mut jobs = Vec::new();
        if self.reload {
            jobs.push(("daemon-reload", Vec::new()));
        }
        // These units were active before writing files. A failed restart
        // may leave them inactive, so a retry must still attempt to start them.
        for (action, units) in [("restart", &self.restart), ("start", &self.start)] {
            if !units.is_empty() {
                jobs.push((action, units.iter().map(String::as_str).collect()));
            }
        }
        // Triggers ran after the files were written. Like IPS actuators,
        // they refresh or restart only services that are running.
        for (action, units) in [
            ("try-reload-or-restart", &self.trigger_reload),
            ("try-restart", &self.trigger_restart),
        ] {
            if !units.is_empty() {
                jobs.push((action, units.iter().map(String::as_str).collect()));
            }
        }
        jobs
    }

    pub(crate) fn run(&self) -> Result<()> {
        for (action, units) in self.jobs() {
            let mut command = live_systemctl();
            command.arg(action);
            if !units.is_empty() {
                command.arg("--").args(units);
            }
            run_live_job(&mut command, action)?;
        }
        Ok(())
    }
}

fn unit_path(path: &str) -> Option<&str> {
    path.strip_prefix("usr/lib/systemd/system/")
        .or_else(|| path.strip_prefix("etc/systemd/system/"))
}

pub(crate) fn service_name(name: &str) -> bool {
    name.ends_with(".service")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.@:\\-".contains(&c))
}

fn matches_service(services: &BTreeSet<String>, unit: &str) -> bool {
    services.contains(unit)
        || unit
            .split_once('@')
            .is_some_and(|(template, _)| services.contains(&format!("{template}@.service")))
}

fn live_systemctl() -> Command {
    let mut command = Command::new("systemctl");
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .args(["--system", "--no-pager"])
        .stdin(Stdio::null());
    command
}

pub(crate) fn require_boot_health() -> Result<()> {
    for unit in ["cps-boot-health.service", "boot-complete.target"] {
        let status = live_systemctl()
            .args(["is-active", "--quiet", unit])
            .status()
            .with_context(|| format!("checking {unit}"))?;
        ensure!(status.success(), "{unit} has not completed successfully");
    }
    Ok(())
}

fn run_live_job(command: &mut Command, action: &str) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(120);
    let mut child = command
        .spawn()
        .with_context(|| format!("running systemd {action}"))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(
                status.success(),
                "live systemd {action} failed with {status}"
            );
            return Ok(());
        }
        if started.elapsed() >= TIMEOUT {
            let _ = child.kill();
            child.wait()?;
            bail!(
                "live systemd {action} did not finish within {} seconds",
                TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn command(program: &str, root: &Path) -> Command {
    let mut command = Command::new(program);
    let mut argument = std::ffi::OsString::from("--root=");
    argument.push(root);
    command
        .arg(argument)
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

fn run_tool(command: &mut Command, action: &str, live: bool) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("running systemd {action}"))?;
    let phase = if live { "live" } else { "offline" };
    ensure!(
        status.success(),
        "{phase} systemd {action} failed with {status}"
    );
    Ok(())
}

pub(crate) fn offline_path(root: &Root) -> Result<PathBuf> {
    let path = root.offline_path()?;
    let mounts = fs::read("/proc/self/mountinfo").context("checking image mount boundaries")?;
    for line in mounts
        .split(|&c| c == b'\n')
        .filter(|line| !line.is_empty())
    {
        let encoded = line
            .split(|&c| c == b' ')
            .nth(4)
            .context("invalid mountinfo line")?;
        let mount = decode_mount_path(encoded)?;
        ensure!(
            mount == path || !mount.starts_with(&path),
            "offline image contains a mounted path: {}",
            mount.display()
        );
    }
    Ok(path)
}

pub(crate) fn decode_mount_path(encoded: &[u8]) -> Result<PathBuf> {
    let mut remaining = encoded;
    let mut decoded = Vec::with_capacity(encoded.len());
    while let Some((&first, rest)) = remaining.split_first() {
        if first == b'\\' {
            let escape = rest.get(..3).context("invalid mountinfo escape")?;
            let byte = u8::from_str_radix(std::str::from_utf8(escape)?, 8)?;
            decoded.push(byte);
            remaining = &rest[3..];
        } else {
            decoded.push(first);
            remaining = rest;
        }
    }
    Ok(std::ffi::OsString::from_vec(decoded).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn services(stop: &[&str], restart: &[&str]) -> LiveServices {
        LiveServices {
            reload: true,
            restart: restart.iter().map(|unit| (*unit).to_owned()).collect(),
            stop: stop.iter().map(|unit| (*unit).to_owned()).collect(),
            trigger_reload: BTreeSet::new(),
            trigger_restart: BTreeSet::new(),
        }
    }

    fn jobs(stop: &[&str], restart: &[&str]) -> LiveJobs {
        LiveJobs {
            reload: true,
            restart: restart.iter().map(|unit| (*unit).to_owned()).collect(),
            stop: stop.iter().map(|unit| (*unit).to_owned()).collect(),
            stopped: Vec::new(),
            start: Vec::new(),
            trigger_reload: Vec::new(),
            trigger_restart: Vec::new(),
        }
    }

    #[test]
    fn an_abort_starts_the_services_that_the_transaction_stopped() -> Result<()> {
        // The transaction removes the units of two running services.
        let forward_services = services(&["app.service", "worker@.service"], &[]);
        let mut forward = jobs(&["app.service", "worker@1.service"], &[]);
        forward_services.check_jobs(&forward)?;
        // Interrupted before the stop was journaled, the abort still
        // starts both: they were running before the transaction.
        let restored = services(&[], &["app.service", "worker@.service", "other.service"]);
        let mut abort = jobs(&[], &["other.service"]);
        abort.start_stopped_by(&forward);
        assert_eq!(abort.start, ["app.service", "worker@1.service"]);
        restored.check_jobs(&abort)?;

        forward.mark_stopped();
        assert!(forward.stop.is_empty());
        assert_eq!(forward.stopped, ["app.service", "worker@1.service"]);
        forward_services.check_jobs(&forward)?;
        let journaled: LiveJobs = serde_json::from_str(&serde_json::to_string(&forward)?)?;
        let mut abort = jobs(&[], &["app.service"]);
        abort.start_stopped_by(&journaled);
        // A service that is active again is restarted rather than started.
        assert_eq!(abort.start, ["worker@1.service"]);
        abort.start_stopped_by(&journaled);
        assert_eq!(abort.start, ["worker@1.service"]);
        restored.check_jobs(&abort)?;
        assert_eq!(
            abort.jobs(),
            [
                ("daemon-reload", vec![]),
                ("restart", vec!["app.service"]),
                ("start", vec!["worker@1.service"]),
            ]
        );
        // The journal accepts only services whose unit files the abort restores.
        assert!(services(&[], &["app.service"]).check_jobs(&abort).is_err());
        assert!(services(&[], &[]).check_jobs(&forward).is_err());
        Ok(())
    }

    #[test]
    fn tmpfiles_rules_cannot_reach_excluded_trees_through_symlinks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let top = dir.path();
        fs::create_dir_all(top.join("usr/lib/sysimage/cps"))?;
        fs::create_dir_all(top.join("usr/share/factory"))?;
        fs::create_dir_all(top.join("var"))?;
        fs::create_dir_all(top.join("etc"))?;
        std::os::unix::fs::symlink("usr/lib", top.join("lib"))?;
        std::os::unix::fs::symlink("var/srv", top.join("srv"))?;
        std::os::unix::fs::symlink("/var/opt", top.join("opt"))?;
        std::os::unix::fs::symlink("../var/log/app.log", top.join("etc/app.log"))?;
        let root = Root::open(top)?;
        let rules = "# kept\n\
            d /etc/example 0755 - - -\n\
            L /etc/app.log - - - - /var/log/app.log\n\
            L /etc/cache - - - - /var/cache\n\
            d /var/lib/example 0755 - - -\n\
            d /lib/sysimage/cps/stage 0755 - - -\n\
            d /srv/www 0755 - - -\n\
            d /opt/app 0755 - - -\n\
            f /etc/app.log 0644 - - -\n\
            d /etc/cache/app 0755 - - -\n\
            d %S/app 0755 - - -\n\
            d /etc/%m 0755 - - -\n";
        let kept = filter_tmpfiles(&root, rules, false)?;
        assert_eq!(
            kept,
            "# kept\nd /etc/example 0755 - - -\nL /etc/app.log - - - - /var/log/app.log\n\
             L /etc/cache - - - - /var/cache\nd /etc/%m 0755 - - -\n"
        );

        // A running root keeps /var, and still never writes the CPS state.
        let kept = filter_tmpfiles(&root, rules, true)?;
        assert!(kept.contains("d /srv/www") && kept.contains("d /etc/cache/app"));
        assert!(!kept.contains("sysimage"));

        // A copy that creates a tree hides where the paths inside it lead.
        let error =
            filter_tmpfiles(&root, "C /etc/tree\nd /etc/tree/sub 0755 - - -\n", false).unwrap_err();
        assert!(error.to_string().contains("inside the tree"), "{error}");
        fs::create_dir(top.join("etc/tree"))?;
        filter_tmpfiles(&root, "C /etc/tree\nd /etc/tree/sub 0755 - - -\n", false)?;
        Ok(())
    }

    #[test]
    fn tmpfiles_paths_must_be_plain() {
        for rule in [
            "d /etc/../var/x 0755",
            "d /etc/./x 0755",
            "d etc/x 0755",
            "d \"/etc/a b\" 0755",
            "d /etc/a\\x20b 0755",
            "d",
        ] {
            assert!(super::validate_tmpfiles(rule.as_bytes()).is_err(), "{rule}");
        }
        super::validate_tmpfiles(b"d %S/app 0755\nd /etc/%m\n").unwrap();
    }

    #[test]
    fn tmpfiles_accepts_only_creation_rules() {
        super::validate_tmpfiles(
            b"# state\nd /var/lib/example 0750 example example -\nf /etc/example.conf 0644\n\
              L /etc/example - - - - /usr/lib/example\nC /var/lib/example/seed.db\nd! /run/x\n",
        )
        .unwrap();
        for rule in [
            "z /var/lib/example",
            "R /var/lib",
            "C+ /var/lib/example",
            "x /tmp/*",
        ] {
            let error = super::validate_tmpfiles(rule.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("use d, f, L, or C"), "{rule}");
        }
    }
}
