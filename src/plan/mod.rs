mod paths;
mod repair;
mod resolve;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    boot::kernel::{self, Artifacts},
    image::{Image, Settings, State, root::Root},
    model::{FORMAT, Node, digest, validate_name},
    repo::snapshot,
    systemd::{self, Action as SystemdAction},
};

pub(crate) use paths::{effective_paths, select_paths};
pub(crate) use resolve::{check as check_dependencies, resolve as resolve_packages};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SolverRevision {
    SatV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Install { packages: Vec<String> },
    ExactInstall { packages: Vec<String> },
    Remove { packages: Vec<String> },
    Repair { packages: Vec<String> },
    Update,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Write {
    pub path: String,
    pub before: Option<Node>,
    pub after: Option<Node>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PackageChange {
    name: String,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    path: String,
    from: BTreeSet<String>,
    to: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    format: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) solver: Option<SolverRevision>,
    pub(crate) image: String,
    pub(crate) base_revision: u64,
    pub(crate) catalog: Option<String>,
    pub(crate) request: Request,
    selected: BTreeMap<String, String>,
    package_changes: Vec<PackageChange>,
    transfers: Vec<Transfer>,
    preserved: Vec<String>,
    pub(crate) writes: Vec<Write>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) systemd: Vec<SystemdAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kernel_install: Option<Artifacts>,
    payload_bytes: u64,
}

impl Plan {
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "Image {} at revision {}",
            self.image, self.base_revision
        )];
        for change in &self.package_changes {
            lines.push(match (&change.from, &change.to) {
                (Some(old), Some(new)) => format!("Update {} {old} -> {new}", change.name),
                (None, Some(new)) => format!("Install {} {new}", change.name),
                (Some(old), None) => format!("Remove {} {old}", change.name),
                (None, None) => continue,
            });
        }
        for transfer in &self.transfers {
            lines.push(format!(
                "Transfer {} from {:?} to {:?}",
                transfer.path, transfer.from, transfer.to
            ));
        }
        for path in &self.preserved {
            lines.push(format!("Preserve local config {path}"));
        }
        if matches!(self.request, Request::Repair { .. }) {
            for write in &self.writes {
                lines.push(format!("Repair {}", write.path));
            }
        }
        for effect in &self.systemd {
            lines.push(format!(
                "Offline systemd {} {}",
                effect.kind(),
                effect.name()
            ));
        }
        if let Some(artifacts) = &self.kernel_install {
            lines.push(format!(
                "Prepare Linux {} with kernel-install",
                artifacts.kernel.release
            ));
        }
        lines.push(format!("{} payload bytes required", self.payload_bytes));
        lines.join("\n")
    }

    pub(crate) fn id(&self) -> Result<String> {
        ensure!(self.format == FORMAT, "unsupported plan format");
        Ok(digest(&serde_json::to_vec(self)?))
    }
}

pub fn plan(path: &Path, request: Request) -> Result<Plan> {
    let image = Image::open(path, false)?;
    image.require_clean()?;
    let state = image.state()?;
    Ok(build(
        &image.root,
        state,
        request,
        None,
        Some(SolverRevision::SatV1),
    )?
    .0)
}

pub(crate) fn build(
    root: &Root,
    mut state: State,
    request: Request,
    catalog: Option<&str>,
    solver: Option<SolverRevision>,
) -> Result<(Plan, State)> {
    let (catalog, installed, paths) = match &request {
        Request::Repair { .. } => {
            let paths = crate::verify::manifest_paths(&state)?;
            ensure!(
                paths == state.paths,
                "path ownership index differs from installed manifests"
            );
            (None, state.installed.clone(), paths)
        }
        Request::Remove { packages } => {
            let names = requested_names(packages)?;
            let mut installed = state.installed.clone();
            for name in names {
                ensure!(
                    installed.remove(&name).is_some(),
                    "package is not installed: {name}"
                );
            }
            resolve::check(&installed).context("removal would break installed dependencies")?;
            let paths = effective_paths(&state.settings, &installed)?;
            (None, installed, paths)
        }
        _ => {
            let requested = match &request {
                Request::Install { packages } | Request::ExactInstall { packages } => {
                    requested_names(packages)?
                }
                _ => BTreeSet::new(),
            };
            let packages = snapshot(
                &state.publisher.repo,
                &state.publisher.name,
                &state.publisher.key,
                catalog,
            )?;
            let (installed, paths) = resolve::resolve(
                &state.installed,
                &packages.packages,
                &requested,
                matches!(request, Request::Update),
                matches!(request, Request::ExactInstall { .. }),
                solver,
                |installed| effective_paths(&state.settings, installed),
            )?;
            (Some(packages.sha256), installed, paths)
        }
    };
    let revision = state
        .settings
        .revision
        .checked_add(1)
        .context("image revision exhausted")?;
    let previous_kernel = kernel::selected(&state.installed, &state.paths)?;
    let kernel = kernel::selected(&installed, &paths)?;
    ensure!(
        previous_kernel.is_none() || kernel.is_some(),
        "transaction would remove the image's selected kernel"
    );
    let kernel_install = kernel.filter(|selected| Some(selected) != previous_kernel.as_ref());
    let mut plan = Plan {
        format: FORMAT,
        solver,
        image: state.settings.id.clone(),
        base_revision: state.settings.revision,
        catalog,
        request,
        selected: installed
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.published.sha256.clone()))
            .collect(),
        package_changes: Vec::new(),
        transfers: Vec::new(),
        preserved: Vec::new(),
        writes: Vec::new(),
        systemd: systemd::plan_actions(&state, &installed, &paths),
        kernel_install,
        payload_bytes: 0,
    };
    for name in state
        .installed
        .keys()
        .chain(installed.keys())
        .collect::<BTreeSet<_>>()
    {
        let from = state
            .installed
            .get(name)
            .map(|old| old.published.manifest.version.to_string());
        let to = installed
            .get(name)
            .map(|new| new.published.manifest.version.to_string());
        if from != to {
            plan.package_changes.push(PackageChange {
                name: name.clone(),
                from,
                to,
            });
        }
    }

    let mut writes = match &plan.request {
        Request::Repair { packages } => {
            repair::writes(root, &state, packages, &mut plan.preserved)?
        }
        _ => BTreeMap::new(),
    };
    for path in state
        .paths
        .keys()
        .chain(paths.keys())
        .collect::<BTreeSet<_>>()
    {
        let before = state.paths.get(path);
        let after = paths.get(path);
        if before == after {
            continue;
        }
        if let (Some(old), Some(new)) = (before, after) {
            ensure!(
                old.node.is_dir() == new.node.is_dir(),
                "cannot change between directory and leaf actions at {path}"
            );
        }
        let observed = root.inspect(path)?;
        let destination = match after {
            Some(target) if target.node.is_dir() => match &observed {
                None => Some(target.node.clone()),
                Some(existing)
                    if existing == &target.node || (target.implicit && existing.is_dir()) =>
                {
                    observed.clone()
                }
                Some(_) => bail!("directory or metadata conflict at {path}"),
            },
            Some(target) => {
                ensure!(
                    !matches!(observed, Some(Node::Dir { .. } | Node::Special)),
                    "path type conflict at {path}"
                );
                ensure!(
                    before.is_some() || observed.is_none(),
                    "unmanaged path already exists at {path}"
                );
                if let Some(old) = before
                    && old.preserve
                    && observed.as_ref() != Some(&old.node)
                {
                    ensure!(
                        matches!(observed, None | Some(Node::File { .. })),
                        "config type changed at {path}"
                    );
                    ensure!(
                        target.preserve,
                        "replacement would discard local config {path}"
                    );
                    plan.preserved.push(path.clone());
                    if target.node != old.node {
                        stage_config(
                            root,
                            &state.settings,
                            revision,
                            path,
                            &target.node,
                            &mut writes,
                        )?;
                    }
                    observed.clone()
                } else {
                    Some(target.node.clone())
                }
            }
            None if before.is_some_and(|old| old.node.is_dir()) => observed.clone(),
            None => {
                ensure!(
                    !matches!(observed, Some(Node::Dir { .. } | Node::Special)),
                    "path type conflict at {path}"
                );
                if let Some(old) = before
                    && old.preserve
                    && observed.as_ref() != Some(&old.node)
                {
                    plan.preserved.push(path.clone());
                    observed.clone()
                } else {
                    None
                }
            }
        };
        if let (Some(old), Some(new)) = (before, after)
            && !old.owners.is_empty()
            && !new.owners.is_empty()
            && old.owners != new.owners
        {
            plan.transfers.push(Transfer {
                path: path.clone(),
                from: old.owners.clone(),
                to: new.owners.clone(),
            });
        }
        writes.insert(
            path.clone(),
            Write {
                path: path.clone(),
                before: observed,
                after: destination,
            },
        );
    }
    plan.writes = writes.into_values().collect();
    plan.writes.sort_by_key(|write| {
        (
            if write.after.as_ref().is_some_and(Node::is_dir) {
                0
            } else {
                1
            },
            write.path.bytes().filter(|b| *b == b'/').count(),
        )
    });
    plan.payload_bytes = payloads(&plan.writes)
        .values()
        .try_fold(0_u64, |total, node| {
            let Node::File { size, .. } = node else {
                unreachable!()
            };
            total
                .checked_add(*size)
                .context("payload byte count overflow")
        })?;
    state.settings.revision = revision;
    state.installed = installed;
    state.paths = paths;
    Ok((plan, state))
}

fn requested_names(packages: &[String]) -> Result<BTreeSet<String>> {
    ensure!(!packages.is_empty(), "request requires a package name");
    for name in packages {
        validate_name(name)?;
    }
    Ok(packages.iter().cloned().collect())
}

fn stage_config(
    root: &Root,
    settings: &Settings,
    revision: u64,
    path: &str,
    node: &Node,
    writes: &mut BTreeMap<String, Write>,
) -> Result<()> {
    let path = format!(".cps/config-updates/{revision}/{path}");
    for (index, _) in path.match_indices('/').skip(1) {
        let parent = &path[..index];
        if !writes.contains_key(parent) {
            let before = root.inspect(parent)?;
            ensure!(
                before.as_ref().is_none_or(Node::is_dir),
                "config update directory conflict at {parent}"
            );
            let after = before.clone().unwrap_or_else(|| settings.directory());
            writes.insert(
                parent.to_owned(),
                Write {
                    path: parent.to_owned(),
                    before,
                    after: Some(after),
                },
            );
        }
    }
    ensure!(
        root.inspect(&path)?.is_none(),
        "config update already exists at {path}"
    );
    writes.insert(
        path.clone(),
        Write {
            path,
            before: None,
            after: Some(node.clone()),
        },
    );
    Ok(())
}

pub(crate) fn payloads(writes: &[Write]) -> BTreeMap<&str, &Node> {
    writes
        .iter()
        .filter_map(|write| match &write.after {
            Some(node @ Node::File { sha256, .. }) if write.before != write.after => {
                Some((sha256.as_str(), node))
            }
            _ => None,
        })
        .collect()
}
