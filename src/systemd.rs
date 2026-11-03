use std::{
    collections::BTreeMap,
    fs,
    io::{Seek, Write},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    image::{Installed, Managed, State, root::Root},
    model::Node,
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
            Self::Sysusers(_) => "sysusers.d",
            Self::Tmpfiles(_) => "tmpfiles.d",
            Self::Preset(_) => "systemd/system",
        };
        format!("usr/lib/{directory}/{}", self.name())
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
    active_actions(installed, paths)
        .into_iter()
        .filter_map(|(action, input)| {
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

pub(crate) fn validate_tmpfiles(bytes: &[u8]) -> Result<()> {
    for (index, line) in std::str::from_utf8(bytes)?.lines().enumerate() {
        if let Some(kind) = line.split_ascii_whitespace().next() {
            ensure!(
                kind.starts_with('#') || matches!(kind, "d" | "f" | "L" | "d!" | "f!" | "L!"),
                "tmpfiles line {} uses unsupported offline rule {kind:?}; use d, f, or L",
                index + 1
            );
        }
    }
    Ok(())
}

pub(crate) fn run(root: &Root, path: &Path, actions: &[Action], state: &State) -> Result<()> {
    let mut sysusers = Vec::new();
    let mut presets = Vec::new();
    let mut rules = tempfile::tempfile_in(path.join(".cps/stage"))?;
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
                if let Some(bytes) = tmpfiles_config(root, name)? {
                    validate_tmpfiles(&bytes).with_context(|| format!("validating {name}"))?;
                    rules.write_all(&bytes)?;
                    rules.write_all(b"\n")?;
                }
            }
            Action::Preset(unit) => {
                // Administrator unit overrides, including masks, take priority.
                if !unit_overridden(root, unit)? {
                    presets.push(unit);
                }
            }
        }
    }

    if !sysusers.is_empty() {
        run_tool(
            command("systemd-sysusers", path).arg("--").args(sysusers),
            "sysusers",
        )?;
    }
    if rules.stream_position()? != 0 {
        rules.rewind()?;
        run_tool(
            command("systemd-tmpfiles", path)
                .args(["--create", "-E", "--exclude-prefix=/.cps", "--", "-"])
                .stdin(Stdio::from(rules)),
            "tmpfiles",
        )?;
    }
    if !presets.is_empty() {
        run_tool(
            command("systemctl", path)
                .args(["--no-reload", "--preset-mode=enable-only", "preset", "--"])
                .args(presets),
            "presets",
        )?;
    }
    root.sync()?;
    Ok(())
}

fn unit_overridden(root: &Root, unit: &str) -> Result<bool> {
    for directory in ["etc", "run", "usr/local/lib"] {
        if root
            .inspect(&format!("{directory}/systemd/system/{unit}"))?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn tmpfiles_config(root: &Root, name: &str) -> Result<Option<Vec<u8>>> {
    // Read and validate the exact bytes given to tmpfiles. A target-local
    // replacement or mask takes precedence over the packaged configuration.
    for directory in ["etc", "run", "usr/local/lib", "usr/lib"] {
        let path = format!("{directory}/tmpfiles.d/{name}");
        match root.inspect(&path)? {
            None => {}
            Some(Node::Symlink { target, .. }) if target == "/dev/null" => return Ok(None),
            Some(Node::File { .. }) => return Ok(Some(root.read_file(&path)?)),
            Some(_) => bail!("invalid tmpfiles configuration at {path}"),
        }
    }
    bail!("missing tmpfiles configuration {name}")
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

fn run_tool(command: &mut Command, action: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("running systemd {action}"))?;
    ensure!(
        status.success(),
        "offline systemd {action} failed with {status}"
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

fn decode_mount_path(encoded: &[u8]) -> Result<PathBuf> {
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
