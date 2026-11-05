mod catalogs;
mod config;
pub(crate) mod freeze;
mod intent;
mod paths;
mod repair;
mod resolve;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    boot::kernel::{self, Artifacts},
    image::{
        Image, Installed, Managed, Settings, State, history::TransactionDetails,
        publisher_generation_is_initial, root::Root,
    },
    model::{FORMAT, Node, PackageSpec, digest, validate_name, validate_publisher},
    repo::{BuildReceipt, Published},
    systemd::{self, Action as SystemdAction},
};

pub(crate) use catalogs::{CandidateSet, claim_source};
pub use config::{ConfigChoice, ConfigUpdate, list_config_updates};
pub use freeze::{FreezeChange, FrozenPackage, list_freezes};
pub use intent::{IntentChange, MarkChoice};
pub(crate) use paths::{
    check_directory, check_path_type, effective_paths, hardlink_groups, select_paths,
};
pub(crate) use resolve::{
    Candidates, Retention, check as check_dependencies, resolve as resolve_packages,
};

const CONFIG_UPDATES_DIR: &str = ".cps/config-updates";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SolverRevision {
    SatV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Install {
        packages: Vec<String>,
    },
    ExactInstall {
        packages: Vec<String>,
    },
    InstallProfile,
    Mark {
        choice: MarkChoice,
        packages: Vec<String>,
    },
    Freeze {
        packages: Vec<String>,
        reason: Option<String>,
    },
    Unfreeze {
        packages: Vec<String>,
    },
    Remove {
        packages: Vec<String>,
    },
    Autoremove,
    ChangeFacet {
        facets: BTreeMap<String, Option<bool>>,
    },
    Repair {
        packages: Vec<String>,
    },
    ResolveConfig {
        path: String,
        take: ConfigChoice,
    },
    Update,
    UpdateProfile,
}

impl Request {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Install { .. } => "install",
            Self::ExactInstall { .. } => "exact-install",
            Self::InstallProfile => "install-profile",
            Self::Mark { .. } => "mark",
            Self::Freeze { .. } => "freeze",
            Self::Unfreeze { .. } => "unfreeze",
            Self::Remove { .. } => "remove",
            Self::Autoremove => "autoremove",
            Self::ChangeFacet { .. } => "change-facet",
            Self::Repair { .. } => "repair",
            Self::ResolveConfig { .. } => "resolve-config",
            Self::Update => "update",
            Self::UpdateProfile => "update-profile",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Write {
    pub path: String,
    pub before: Option<Node>,
    pub after: Option<Node>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) detach: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) local_config: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_move: Option<ConfigMove>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigMove {
    pub(crate) from: String,
    pub(crate) local: Option<Node>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageChange {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
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
    #[serde(default, skip_serializing_if = "publisher_generation_is_initial")]
    publisher_generation: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    prune_directories: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) track_profile_intent: bool,
    pub(crate) catalog: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) catalogs: BTreeMap<String, String>,
    pub(crate) request: Request,
    selected: BTreeMap<String, String>,
    package_changes: Vec<PackageChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) intent_changes: Vec<IntentChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) freeze_changes: Vec<FreezeChange>,
    transfers: Vec<Transfer>,
    preserved: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_update_revision: Option<u64>,
    pub(crate) writes: Vec<Write>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) systemd: Vec<SystemdAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kernel_install: Option<Artifacts>,
    payload_bytes: u64,
}

impl Plan {
    fn with_selection(
        settings: &Settings,
        request: Request,
        saved: Option<&Plan>,
        solver: Option<SolverRevision>,
        installed: &BTreeMap<String, Installed>,
    ) -> Self {
        Self {
            format: FORMAT,
            solver,
            image: settings.id.clone(),
            base_revision: settings.revision,
            publisher_generation: settings.publisher_generation,
            prune_directories: saved.map_or(solver.is_some(), |plan| plan.prune_directories),
            track_profile_intent: false,
            catalog: None,
            catalogs: BTreeMap::new(),
            request,
            selected: installed
                .iter()
                .map(|(name, pkg)| (name.clone(), pkg.published.sha256.clone()))
                .collect(),
            package_changes: Vec::new(),
            intent_changes: Vec::new(),
            freeze_changes: Vec::new(),
            transfers: Vec::new(),
            preserved: Vec::new(),
            config_update_revision: None,
            writes: Vec::new(),
            systemd: Vec::new(),
            kernel_install: None,
            payload_bytes: 0,
        }
    }

    pub(crate) fn transaction_details(&self, state: &State) -> TransactionDetails {
        let mut catalogs = self.catalogs.clone();
        if let Some(hash) = &self.catalog {
            catalogs.insert(state.publishers.as_slice()[0].name.clone(), hash.clone());
        }
        TransactionDetails {
            base_image: self.image.clone(),
            base_revision: self.base_revision,
            request: self.request.clone(),
            package_changes: self.package_changes.clone(),
            intent_changes: self.intent_changes.clone(),
            freeze_changes: self.freeze_changes.clone(),
            catalogs,
            planned_path_changes: self
                .writes
                .iter()
                .filter(|write| write.before != write.after || write.detach)
                .count(),
            payload_bytes: self.payload_bytes,
            systemd_actions: self.systemd.len(),
            kernel_release: self
                .kernel_install
                .as_ref()
                .map(|artifacts| artifacts.kernel.release.clone()),
        }
    }

    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "Image {} at revision {}",
            self.image, self.base_revision
        )];
        for change in &self.package_changes {
            let name = change.publisher.as_ref().map_or_else(
                || change.name.clone(),
                |publisher| format!("cps://{publisher}/{}", change.name),
            );
            lines.push(match (&change.from, &change.to) {
                (Some(old), Some(new)) => format!("Update {name} {old} -> {new}"),
                (None, Some(new)) => format!("Install {name} {new}"),
                (Some(old), None) => format!("Remove {name} {old}"),
                (None, None) => continue,
            });
        }
        if matches!(self.request, Request::Mark { .. }) && self.intent_changes.is_empty() {
            lines.push("Package requests already match the requested state".to_owned());
        }
        for change in &self.intent_changes {
            let mark = if change.requested {
                "locally requested"
            } else {
                "automatic"
            };
            lines.push(format!("Mark {} as {mark}", change.package));
        }
        if matches!(
            self.request,
            Request::Freeze { .. } | Request::Unfreeze { .. }
        ) && self.freeze_changes.is_empty()
        {
            lines.push("Package freezes already match the requested state".to_owned());
        }
        for change in &self.freeze_changes {
            lines.push(match &change.to {
                Some(frozen) => format!(
                    "Freeze {} at {}{}",
                    change.name,
                    frozen.version,
                    freeze::reason_suffix(frozen)
                ),
                None => format!("Unfreeze {}", change.name),
            });
        }
        if let Request::ChangeFacet { facets } = &self.request {
            for (name, value) in facets {
                lines.push(match value {
                    Some(value) => format!("Set facet {name}={value}"),
                    None => format!("Reset facet {name} to its default"),
                });
            }
            for write in &self.writes {
                if write.config_move.is_some() || write.path.starts_with(CONFIG_UPDATES_DIR) {
                    continue;
                }
                match (&write.before, &write.after) {
                    (None, Some(Node::Dir { .. } | Node::Hardlink { .. })) => (),
                    (None, Some(_)) => lines.push(format!("Add {}", write.path)),
                    (Some(Node::Dir { .. }), None) => (),
                    (Some(_), None) => lines.push(format!("Remove {}", write.path)),
                    (Some(before), Some(after)) if before != after => {
                        lines.push(format!("Replace {}", write.path));
                    }
                    _ => (),
                }
            }
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
        for write in &self.writes {
            if let Some(migration) = &write.config_move {
                lines.push(format!("Move config {} to {}", migration.from, write.path));
            }
        }
        for write in self
            .writes
            .iter()
            .filter(|write| write.path.starts_with(CONFIG_UPDATES_DIR) && write.replaces_file())
        {
            lines.push(format!("Stage vendor config {:?}", write.path));
        }
        if let (Request::ResolveConfig { path, take }, Some(revision)) =
            (&self.request, self.config_update_revision)
        {
            match take {
                ConfigChoice::Local => lines.push(format!("Keep local config {path}")),
                ConfigChoice::Vendor => lines.push(format!(
                    "Install vendor config {path} from {}",
                    config::vendor_copy(revision, path)
                )),
            }
        }
        if matches!(self.request, Request::Repair { .. }) {
            for write in &self.writes {
                lines.push(format!("Repair {}", write.path));
            }
        }
        for write in self.writes.iter().filter(|write| write.removes_directory()) {
            lines.push(format!("Prune if empty {}", write.path));
        }
        for write in &self.writes {
            if write.local_config {
                lines.push(format!(
                    "Set package metadata on local config {}",
                    write.path
                ));
            }
            if let Some(target) = write.hardlink_target() {
                lines.push(format!("Link {} to {target}", write.path));
            }
            if write.detach {
                lines.push(format!(
                    "Detach {} from its old hard link group",
                    write.path
                ));
            }
            if write.replaces_file()
                && let Some(caps) = write.after.as_ref().and_then(Node::capabilities)
            {
                lines.push(format!("Set file capabilities on {} to {caps}", write.path));
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

    pub(crate) fn check_recovery_state(&self, current: &State, next: &State) -> Result<()> {
        let before = &current.settings;
        let after = &next.settings;
        ensure!(
            self.image == before.id && self.base_revision == before.revision,
            "pending plan does not match the committed image revision"
        );
        self.check_publisher_generation(before)?;
        let revision = if self.config_update_revision.is_some() {
            before.revision
        } else {
            before.next_revision()?
        };
        ensure!(
            after.id == before.id
                && after.revision == revision
                && after.publisher_generation == before.publisher_generation
                && after.boot_parent == before.boot_parent
                && after.arch == before.arch
                && after.uid == before.uid
                && after.gid == before.gid
                && next.publishers == current.publishers
                && after.format == next.publishers.image_format(after),
            "pending state does not match the image identity or transaction revision"
        );
        ensure!(
            self.selected
                .iter()
                .map(|(name, hash)| (name, hash.as_str()))
                .eq(next
                    .installed
                    .iter()
                    .map(|(name, pkg)| (name, pkg.published.sha256.as_str()))),
            "pending package selection differs from the verified plan"
        );
        after.check_profile_intent(&next.installed)?;
        let (freezes, changes) = freeze::changes(current, &self.request)?;
        ensure!(
            after.freezes == freezes && self.freeze_changes == changes,
            "pending package freezes differ from the verified plan"
        );
        freeze::check(&after.freezes, &next.installed)?;
        Ok(())
    }

    pub(crate) fn check_publisher_generation(&self, settings: &Settings) -> Result<()> {
        ensure!(
            self.publisher_generation == settings.publisher_generation,
            "publisher configuration changed after this plan was saved; generate a new plan"
        );
        Ok(())
    }

    pub(crate) fn config_payload(&self) -> Option<(String, &Node)> {
        let Request::ResolveConfig {
            path,
            take: ConfigChoice::Vendor,
        } = &self.request
        else {
            return None;
        };
        let revision = self.config_update_revision?;
        let write = self
            .writes
            .iter()
            .find(|write| write.path == *path && write.replaces_file())?;
        Some((config::vendor_copy(revision, path), write.after.as_ref()?))
    }
}

pub fn plan(path: &Path, request: Request) -> Result<Plan> {
    let image = Image::open_query(path)?;
    image.require_clean()?;
    let state = image.state()?;
    Ok(build(&image, state, request, None, Some(SolverRevision::SatV1))?.0)
}

fn resolve_local<'a>(
    state: &'a State,
    available: Candidates<'_, impl IntoIterator<Item = &'a Published>>,
    requested: &BTreeMap<&'a str, PackageSpec<'a>>,
    update: bool,
    exact: bool,
    solver: Option<SolverRevision>,
) -> Result<(BTreeMap<String, Installed>, BTreeMap<String, Managed>)> {
    let mut pinned = requested.clone();
    for name in state.settings.profile_packages.keys() {
        let manifest = &state.installed[name].published.manifest;
        ensure!(
            requested
                .get(name.as_str())
                .and_then(|spec| spec.version)
                .is_none_or(|version| version == manifest.version),
            "{name} is protected by the installed signed profile; apply a signed profile update to change it"
        );
        ensure!(
            available
                .obsoletions
                .get(name)
                .is_none_or(|cutoff| manifest.version > *cutoff),
            "the current catalog obsoletes protected package {name}; apply a signed profile update first"
        );
        pinned.insert(
            name.as_str(),
            PackageSpec {
                publisher: Some(&manifest.publisher),
                name,
                version: Some(manifest.version),
            },
        );
    }
    let (mut installed, paths) = resolve::resolve(
        &state.installed,
        available,
        &pinned,
        update,
        if exact {
            Retention::None
        } else {
            Retention::All
        },
        solver,
        |installed| effective_paths(&state.settings, installed),
    )?;
    state.settings.check_profile_packages(&installed)?;
    for name in state.settings.profile_packages.keys() {
        installed
            .get_mut(name)
            .expect("checked profile package")
            .explicit = requested.contains_key(name.as_str()) || state.installed[name].explicit;
    }
    Ok((installed, paths))
}

pub(crate) fn build(
    image: &Image,
    mut state: State,
    request: Request,
    saved: Option<&Plan>,
    solver: Option<SolverRevision>,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    let root = &image.root;
    ensure!(
        state.settings.bootstrap_catalog.is_none() || matches!(request, Request::InstallProfile),
        "install the pinned bootstrap release with plan install-profile first"
    );
    if let Request::Mark { choice, packages } = &request {
        let (installed, intent_changes) = intent::mark(image, &mut state, *choice, packages)?;
        let mut plan = Plan::with_selection(&state.settings, request, saved, solver, &installed);
        plan.intent_changes = intent_changes;
        state.settings.revision = state.settings.next_revision()?;
        state.settings.format = state.publishers.image_format(&state.settings);
        state.installed = installed;
        return Ok((plan, state, Vec::new()));
    }
    if matches!(request, Request::Freeze { .. } | Request::Unfreeze { .. }) {
        ensure!(
            crate::verify::manifest_paths(&state)? == state.paths,
            "path ownership index differs from installed manifests"
        );
        image.verify_catalog_archives(&state)?;
        let (freezes, changes) = freeze::changes(&state, &request)?;
        let mut plan =
            Plan::with_selection(&state.settings, request, saved, solver, &state.installed);
        plan.freeze_changes = changes;
        state.settings.freezes = freezes;
        state.settings.revision = state.settings.next_revision()?;
        state.settings.format = state.publishers.image_format(&state.settings);
        return Ok((plan, state, Vec::new()));
    }
    let mut catalogs = catalogs::Catalogs::default();
    let mut receipts = Vec::new();
    let mut next_facets = None;
    let mut update_profile = matches!(request, Request::UpdateProfile);
    let mut track_profile_intent = false;
    let (mut installed, paths) = match &request {
        Request::ChangeFacet { facets } => {
            ensure!(
                state.publishers.release_profile().is_none(),
                "the signed release profile fixes this image's facets"
            );
            ensure!(!facets.is_empty(), "specify at least one facet change");
            ensure!(
                crate::verify::manifest_paths(&state)? == state.paths,
                "path ownership index differs from installed manifests"
            );
            image.verify_catalog_archives(&state)?;
            if state.settings.config_updates.is_none() {
                state.settings.config_updates =
                    Some(config::updates(root, &state.settings, &state.paths)?);
            }
            let mut settings = state.settings.facets.clone();
            for (name, value) in facets {
                validate_publisher(name).with_context(|| format!("invalid facet {name:?}"))?;
                if let Some(value) = value {
                    settings.insert(name.clone(), *value);
                } else {
                    settings.remove(name);
                }
            }
            let paths = select_paths(
                &state.settings.arch,
                &settings,
                state.settings.directory(),
                &state.installed,
            )?;
            next_facets = Some(settings);
            (state.installed.clone(), paths)
        }
        Request::Repair { .. } | Request::ResolveConfig { .. } => {
            let paths = crate::verify::manifest_paths(&state)?;
            ensure!(
                paths == state.paths,
                "path ownership index differs from installed manifests"
            );
            (state.installed.clone(), paths)
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
            state.settings.check_profile_packages(&installed)?;
            resolve::check(&installed).context("removal would break installed dependencies")?;
            let paths = effective_paths(&state.settings, &installed)?;
            (installed, paths)
        }
        Request::Autoremove => {
            let requested = state
                .installed
                .iter()
                .filter(|(_, package)| package.explicit)
                .map(|(name, package)| {
                    let manifest = &package.published.manifest;
                    (
                        name.as_str(),
                        PackageSpec {
                            publisher: Some(&manifest.publisher),
                            name,
                            version: Some(manifest.version),
                        },
                    )
                })
                .collect();
            resolve_local(
                &state,
                Candidates {
                    packages: std::iter::empty(),
                    obsoletions: &BTreeMap::new(),
                    freezes: &state.settings.freezes,
                },
                &requested,
                false,
                true,
                solver,
            )?
        }
        _ => {
            let install_profile = matches!(request, Request::InstallProfile);
            if install_profile {
                ensure!(
                    state.publishers.release_profile().is_some(),
                    "install-profile requires a configured signed release profile"
                );
                ensure!(
                    state.installed.is_empty(),
                    "install-profile requires an image with no installed packages"
                );
            }
            let requested = match &request {
                Request::Install { packages } | Request::ExactInstall { packages } => {
                    catalogs::requested(&state, packages)?
                }
                _ => BTreeMap::new(),
            };
            catalogs = catalogs::load(image, &state, &requested, saved)?;
            update_profile |= saved.is_none()
                && matches!(request, Request::Update)
                && !state.installed.is_empty()
                && catalogs
                    .profile
                    .as_ref()
                    .is_some_and(|profile| !profile.composition.is_empty());
            track_profile_intent = saved.map_or(
                update_profile
                    || (!requested.is_empty() && !state.settings.profile_packages.is_empty()),
                |plan| plan.track_profile_intent,
            );
            if track_profile_intent && state.settings.profile_intent.is_none() {
                state.settings.profile_intent = Some(image.reconstruct_profile_intent(&state)?);
            }
            let (installed, paths) = if install_profile {
                let profile = catalogs
                    .profile
                    .as_ref()
                    .context("missing signed release profile")?;
                ensure!(
                    !profile.composition.is_empty(),
                    "signed release profile has no recorded composition"
                );
                let installed = catalogs.candidates.replay(profile)?;
                freeze::check(&state.settings.freezes, &installed)?;
                let paths = effective_paths(&state.settings, &installed)?;
                (installed, paths)
            } else if update_profile {
                let profile = catalogs
                    .profile
                    .as_ref()
                    .context("missing signed release profile")?;
                ensure!(
                    !profile.composition.is_empty(),
                    "signed release profile has no recorded composition"
                );
                catalogs.candidates.update_profile(
                    &state.installed,
                    profile,
                    &state.settings.freezes,
                    state
                        .settings
                        .profile_intent
                        .as_ref()
                        .filter(|_| track_profile_intent)
                        .map(|intent| &intent.local),
                    solver,
                    |installed| effective_paths(&state.settings, installed),
                )?
            } else {
                resolve_local(
                    &state,
                    Candidates {
                        packages: &catalogs.candidates.packages,
                        obsoletions: &catalogs.candidates.obsoletions,
                        freezes: &state.settings.freezes,
                    },
                    &requested,
                    matches!(request, Request::Update),
                    matches!(request, Request::ExactInstall { .. }),
                    solver,
                )?
            };
            receipts = catalogs.verify_builds(image, &state, &installed)?;
            if install_profile || update_profile {
                let profile = catalogs.profile.as_ref().expect("applied profile");
                state
                    .settings
                    .profile_packages
                    .clone_from(&profile.composition);
                if let Some(intent) = &mut state.settings.profile_intent {
                    intent.roots = profile.roots.keys().cloned().collect();
                }
                state.settings.check_profile_packages(&installed)?;
            }
            if let Some(intent) = &mut state.settings.profile_intent {
                if matches!(request, Request::ExactInstall { .. }) {
                    intent.local.clear();
                }
                intent
                    .local
                    .extend(requested.keys().map(|name| (*name).to_owned()));
            }
            state.settings.catalog_checkpoints = std::mem::take(&mut catalogs.checkpoints);
            state.settings.catalog_archives = true;
            state.settings.archived_profile_catalog = None;
            (installed, paths)
        }
    };
    if let Some(intent) = &mut state.settings.profile_intent {
        intent.local.retain(|name| installed.contains_key(name));
        for (name, package) in &mut installed {
            package.explicit = intent.roots.contains(name) || intent.local.contains(name);
        }
        // Preserve the old image format when the archived signed roots and
        // explicit marks can reconstruct both sets on their own.
        if track_profile_intent
            && intent.roots.is_disjoint(&intent.local)
            && catalogs.profile.as_ref().is_some_and(|profile| {
                profile.composition == state.settings.profile_packages
                    && profile.roots.keys().eq(&intent.roots)
            })
        {
            state.settings.profile_intent = None;
        }
    }
    freeze::check(&state.settings.freezes, &installed)?;
    // Resolving a vendor proposal changes local configuration, so a staged
    // boot environment can still be activated at its original revision.
    let revision = if matches!(request, Request::ResolveConfig { .. }) {
        state.settings.revision
    } else {
        state.settings.next_revision()?
    };
    let kernel_install = kernel::transition(
        kernel::selected(&state.installed, &state.paths)?,
        kernel::selected(&installed, &paths)?,
    )?;
    let mut plan = Plan {
        track_profile_intent,
        catalog: catalogs.legacy,
        catalogs: catalogs.pins,
        systemd: systemd::plan_actions(&state, &installed, &paths),
        kernel_install,
        ..Plan::with_selection(
            &state.settings,
            if update_profile {
                Request::UpdateProfile
            } else {
                request
            },
            saved,
            solver,
            &installed,
        )
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
            let publisher = if state.publishers.as_slice().len() > 1 {
                installed
                    .get(name)
                    .or_else(|| state.installed.get(name))
                    .map(|entry| entry.published.manifest.publisher.clone())
            } else {
                None
            };
            plan.package_changes.push(PackageChange {
                name: name.clone(),
                publisher,
                from,
                to,
            });
        }
    }

    let mut writes = match &plan.request {
        Request::Repair { packages } => {
            repair::writes(root, &state, packages, &mut plan.preserved)?
        }
        Request::ResolveConfig { path, take } => {
            let (revision, write) =
                config::resolve(root, &mut state.settings, &paths, path, *take)?;
            plan.config_update_revision = Some(revision);
            BTreeMap::from([(path.clone(), write)])
        }
        _ => BTreeMap::new(),
    };
    let moves = config::moves(&state, &installed, &paths)?;
    for (to, from) in &moves {
        config::plan_move(
            root,
            &mut state.settings,
            revision,
            (from, &state.paths[from].node),
            (to, &paths[to]),
            &mut writes,
        )?;
    }
    let reclaimed =
        config::reclaim_disabled(root, &mut state, revision, &installed, &paths, &mut writes)?;
    plan.preserved.extend(reclaimed.keys().cloned());
    let retired: BTreeSet<_> = moves
        .iter()
        .chain(&reclaimed)
        .filter(|(to, from)| to != from)
        .map(|(_, from)| from.as_str())
        .collect();
    for path in state
        .paths
        .keys()
        .chain(paths.keys())
        .collect::<BTreeSet<_>>()
    {
        if moves.contains_key(path)
            || reclaimed.contains_key(path)
            || retired.contains(path.as_str())
        {
            continue;
        }
        let before = state.paths.get(path);
        let after = paths.get(path);
        if before == after {
            continue;
        }
        let mut retired_update = None;
        if let Some(updates) = &mut state.settings.config_updates
            && (!after.is_some_and(|target| target.preserve)
                || before.map(|old| &old.node) != after.map(|new| &new.node))
        {
            retired_update = updates.remove(path);
        }
        if let (Some(old), Some(new)) = (before, after) {
            check_path_type(path, old, new)?;
        }
        let observed = root.inspect(path)?;
        let mut local_config = false;
        let destination = match after {
            Some(target) if target.node.is_dir() => match &observed {
                None => Some(target.node.clone()),
                Some(existing) => {
                    if !target.implicit
                        && before.is_some_and(|old| !old.implicit && existing == &old.node)
                    {
                        // Update an explicitly owned directory only while its
                        // metadata still matches the installed declaration.
                        root.dir(path)?;
                        Some(target.node.clone())
                    } else {
                        check_directory(path, existing, target)?;
                        observed.clone()
                    }
                }
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
                        observed.as_ref().and_then(Node::capabilities).is_none(),
                        "unexpected file capabilities on preserved config {path}"
                    );
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
                            &mut state.settings,
                            revision,
                            path,
                            &target.node,
                            &mut writes,
                        )?;
                    }
                    if config::file_metadata(&old.node) != config::file_metadata(&target.node)
                        && let Some(after) = observed
                            .as_ref()
                            .and_then(|local| config::with_vendor_metadata(local, &target.node))
                    {
                        local_config = true;
                        Some(after)
                    } else {
                        observed.clone()
                    }
                } else {
                    Some(target.node.clone())
                }
            }
            None if before.is_some_and(|old| old.node.is_dir()) => {
                if plan.prune_directories
                    && state.settings.created_directories.contains(path)
                    && observed.as_ref() == before.map(|old| &old.node)
                {
                    None
                } else {
                    observed.clone()
                }
            }
            None => {
                ensure!(
                    !matches!(observed, Some(Node::Dir { .. } | Node::Special)),
                    "path type conflict at {path}"
                );
                if let Some(old) = before
                    && old.preserve
                    && observed.as_ref() != Some(&old.node)
                {
                    ensure!(
                        observed.as_ref().and_then(Node::capabilities).is_none(),
                        "unexpected file capabilities on preserved config {path}"
                    );
                    plan.preserved.push(path.clone());
                    if next_facets.is_some() {
                        config::remember_disabled(
                            &state.installed,
                            &mut state.settings,
                            path,
                            old,
                            observed.clone(),
                            retired_update,
                        )?;
                    }
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
        let detach = before.is_some_and(|old| matches!(old.node, Node::Hardlink { .. }))
            && matches!(destination, Some(Node::File { .. }))
            && observed == destination;
        writes.insert(
            path.clone(),
            Write {
                path: path.clone(),
                before: observed,
                after: destination,
                detach,
                local_config,
                config_move: None,
            },
        );
    }
    // Replacing a regular file swaps its inode. Its aliases still refer to
    // the old inode even when their actions have not changed.
    for (path, managed) in &paths {
        if let Node::Hardlink { target } = &managed.node
            && writes.get(target).is_some_and(Write::replaces_file)
            && !writes.contains_key(path)
        {
            let before = root.inspect(path)?;
            ensure!(
                !matches!(before, Some(Node::Dir { .. } | Node::Special)),
                "path type conflict at {path}"
            );
            writes.insert(
                path.clone(),
                Write {
                    path: path.clone(),
                    before,
                    after: Some(managed.node.clone()),
                    detach: false,
                    local_config: false,
                    config_move: None,
                },
            );
        }
    }
    for target in writes
        .values()
        .filter_map(Write::hardlink_target)
        .collect::<BTreeSet<_>>()
    {
        if !writes.get(target).is_some_and(Write::replaces_file) {
            let file = paths
                .get(target)
                .with_context(|| format!("missing hard link target {target}"))?;
            root.checked_hardlink_target(target, &file.node)?;
        }
    }
    plan.writes = writes.into_values().collect();
    plan.writes
        .sort_by(|a, b| a.order(&retired).cmp(&b.order(&retired)));
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
    state.settings.bootstrap_catalog = None;
    if let Some(facets) = next_facets {
        state.settings.facets = facets;
    }
    config::retain_disabled(&mut state.settings, &installed, &paths);
    state.settings.format = state.publishers.image_format(&state.settings);
    state
        .settings
        .created_directories
        .retain(|path| paths.contains_key(path));
    state.settings.created_directories.extend(
        plan.writes
            .iter()
            .filter(|write| {
                write.before.is_none()
                    && write.after.as_ref().is_some_and(Node::is_dir)
                    && paths.contains_key(&write.path)
            })
            .map(|write| write.path.clone()),
    );
    state.installed = installed;
    state.paths = paths;
    Ok((plan, state, receipts))
}

impl Write {
    pub(crate) fn removes_directory(&self) -> bool {
        self.after.is_none() && self.before.as_ref().is_some_and(Node::is_dir)
    }

    pub(crate) fn replaces_file(&self) -> bool {
        matches!(self.after, Some(Node::File { .. })) && (self.before != self.after || self.detach)
    }

    pub(crate) fn needs_blob(&self) -> bool {
        self.replaces_file() && self.local_config_source().is_none()
    }

    pub(crate) fn local_config_source(&self) -> Option<(&str, &Node)> {
        if let Some(migration) = &self.config_move {
            return migration
                .local
                .as_ref()
                .map(|node| (migration.from.as_str(), node));
        }
        if self.local_config {
            return self.before.as_ref().map(|node| (self.path.as_str(), node));
        }
        None
    }

    pub(crate) fn hardlink_target(&self) -> Option<&str> {
        match &self.after {
            Some(Node::Hardlink { target }) => Some(target),
            _ => None,
        }
    }

    fn order(&self, retired: &BTreeSet<&str>) -> (u8, usize, &str) {
        let depth = self.path.bytes().filter(|b| *b == b'/').count();
        if self.removes_directory() {
            (4, usize::MAX - depth, "")
        } else if retired.contains(self.path.as_str()) {
            // Publish both the new path and its vendor proposal before retiring the old path.
            (3, depth, "")
        } else if let Some(target) = self.hardlink_target() {
            (2, 0, target)
        } else if self.after.as_ref().is_some_and(Node::is_dir) {
            (0, depth, "")
        } else {
            (1, depth, "")
        }
    }
}

fn requested_names(packages: &[String]) -> Result<BTreeSet<String>> {
    ensure!(!packages.is_empty(), "request requires a package name");
    for name in packages {
        validate_name(name)?;
    }
    Ok(packages.iter().cloned().collect())
}

pub(crate) fn config_update_prefix(revision: u64) -> String {
    format!("{CONFIG_UPDATES_DIR}/{revision}/")
}

fn stage_config(
    root: &Root,
    settings: &mut Settings,
    revision: u64,
    path: &str,
    node: &Node,
    writes: &mut BTreeMap<String, Write>,
) -> Result<()> {
    if let Some(updates) = &mut settings.config_updates {
        updates.insert(path.to_owned(), revision);
    }
    let path = format!("{}{path}", config_update_prefix(revision));
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
                    detach: false,
                    local_config: false,
                    config_move: None,
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
            detach: false,
            local_config: false,
            config_move: None,
        },
    );
    Ok(())
}

pub(crate) fn payloads(writes: &[Write]) -> BTreeMap<&str, &Node> {
    writes
        .iter()
        .filter_map(|write| match &write.after {
            Some(node @ Node::File { sha256, .. }) if write.needs_blob() => {
                Some((sha256.as_str(), node))
            }
            _ => None,
        })
        .collect()
}
