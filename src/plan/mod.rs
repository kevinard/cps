mod catalogs;
mod config;
pub(crate) mod freeze;
mod intent;
mod order;
mod paths;
mod rename;
mod repair;
mod resolve;
mod update;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    boot::kernel::{self, Artifacts},
    image::{
        CONFIG_UPDATES_DIRECTORY, IMAGE_FORMAT, Image, Installed, Managed, ProfileIntent,
        STATE_DIRECTORY, Settings, State, history::TransactionDetails, lock::ImageLock,
        publisher_generation_is_initial, root::Root,
    },
    model::{
        Node, PackageSpec, Selection, digest, facet_pattern_covers, validate_name,
        validate_publisher,
    },
    repo::{BuildReceipt, PackageInfo, Published, ReleaseAdvisory, ReleaseProfile},
    systemd::{self, Action as SystemdAction},
    triggers::{self, TriggerRun},
};

pub(crate) use catalogs::{CandidateSet, claim_source};
pub use config::{ConfigChoice, ConfigUpdate, config_diff_inputs, list_config_updates};
pub use freeze::{FreezeChange, FrozenPackage, list_freezes};
pub use intent::{IntentChange, MarkChoice};
use order::WriteOrder;
pub(crate) use paths::{
    check_directory, choose_mediators, effective_paths, hardlink_groups, mediator_offers,
    resolve_facets, select_paths,
};
pub(crate) use rename::Renames;
pub(crate) use resolve::{
    Candidates, Retention, check as check_dependencies, resolve as resolve_packages,
};
pub use update::{HeldBackUpdate, PhasedUpdateDeferred};

fn is_config_update(path: &str) -> bool {
    path.strip_prefix(CONFIG_UPDATES_DIRECTORY)
        .is_some_and(|rest| rest.starts_with('/'))
}

const PLAN_FORMAT: u32 = 1;

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
    InstallLock {
        lock: Box<ImageLock>,
    },
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
    /// Set (`Some`) or reset (`None`) image-wide `FACET` and per-package `PACKAGE:FACET`
    /// rules, reusing the installed manifests.
    ChangeFacet {
        facets: BTreeMap<String, Option<bool>>,
        /// Apply the per-package rules to each package's installed dependency closure.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        with_dependencies: bool,
    },
    /// Set or unset non-architecture variants, reusing the installed manifests.
    ChangeVariant {
        variants: BTreeMap<String, Option<String>>,
    },
    SetMediator {
        group: String,
        provider: Option<String>,
    },
    Repair {
        packages: Vec<String>,
    },
    RepairPaths {
        paths: Vec<String>,
    },
    Revert {
        paths: Vec<String>,
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
            Self::InstallLock { .. } => "install-lock",
            Self::Mark { .. } => "mark",
            Self::Freeze { .. } => "freeze",
            Self::Unfreeze { .. } => "unfreeze",
            Self::Remove { .. } => "remove",
            Self::Autoremove => "autoremove",
            Self::ChangeFacet { .. } => "change-facet",
            Self::ChangeVariant { .. } => "change-variant",
            Self::SetMediator { .. } => "set-mediator",
            Self::Repair { .. } => "repair",
            Self::RepairPaths { .. } => "repair-paths",
            Self::Revert { .. } => "revert",
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requires_reboot: bool,
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
    // Required to distinguish the current schema from development format 1 plans.
    pub(crate) install_recommends: bool,
    pub(crate) image: String,
    pub(crate) base_revision: u64,
    #[serde(default, skip_serializing_if = "publisher_generation_is_initial")]
    publisher_generation: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    force_phased_update: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    git_updates: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    deferred_publishers: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) catalogs: BTreeMap<String, String>,
    pub(crate) request: Request,
    selected: BTreeMap<String, String>,
    package_changes: Vec<PackageChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) intent_changes: Vec<IntentChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) freeze_changes: Vec<FreezeChange>,
    /// Installed packages whose selected actions a facet or variant change alters.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    recomposed: BTreeSet<String>,
    transfers: Vec<Transfer>,
    preserved: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_update_revision: Option<u64>,
    pub(crate) writes: Vec<Write>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) systemd: Vec<SystemdAction>,
    /// Triggers to run after the files are written, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) triggers: Vec<TriggerRun>,
    /// Triggers whose generated paths the transaction removes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) retired_triggers: Vec<TriggerRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kernel_install: Option<Artifacts>,
    payload_bytes: u64,
}

impl Plan {
    pub(crate) fn is_recovery(&self) -> bool {
        matches!(
            self.request,
            Request::Repair { .. } | Request::RepairPaths { .. } | Request::Revert { .. }
        )
    }

    pub(crate) fn boot_reason(&self, stage_packages: &BTreeSet<String>) -> Option<String> {
        if self.kernel_install.is_some() {
            return Some("the transaction changes the boot kernel or initramfs".into());
        }
        if !self.git_updates.is_empty() {
            return Some("the transaction updates a tracked Git commit".into());
        }
        if let Some(change) = self
            .package_changes
            .iter()
            .find(|change| change.requires_reboot)
        {
            let package = change.publisher.as_ref().map_or_else(
                || change.name.clone(),
                |publisher| format!("cps://{publisher}/{}", change.name),
            );
            return Some(format!(
                "{package} declares requires-reboot in its manifest"
            ));
        }
        if let Some(change) = self
            .package_changes
            .iter()
            .find(|change| stage_packages.contains(&change.name))
        {
            return Some(format!("{} is configured for staged changes", change.name));
        }
        if let Some(trigger) = self
            .triggers
            .iter()
            .chain(&self.retired_triggers)
            .find(|trigger| trigger.boot)
        {
            return Some(format!(
                "trigger {} generates boot artifacts",
                trigger.label()
            ));
        }
        self.writes
            .iter()
            .find(|write| write.boot_sensitive())
            .map(|write| format!("the transaction changes {}", write.path))
    }

    pub(crate) fn has_changes(&self) -> bool {
        !self.package_changes.is_empty()
            || !self.intent_changes.is_empty()
            || !self.writes.is_empty()
            || !self.systemd.is_empty()
            || !self.triggers.is_empty()
            || !self.retired_triggers.is_empty()
            || self.kernel_install.is_some()
    }

    pub fn deferred_publishers(&self) -> &BTreeSet<String> {
        &self.deferred_publishers
    }

    pub(crate) fn package_changes(&self) -> &[PackageChange] {
        &self.package_changes
    }

    fn with_selection(
        settings: &Settings,
        request: Request,
        install_recommends: bool,
        installed: &BTreeMap<String, Installed>,
    ) -> Self {
        Self {
            format: PLAN_FORMAT,
            install_recommends,
            image: settings.id.clone(),
            base_revision: settings.revision,
            publisher_generation: settings.publisher_generation,
            force_phased_update: false,
            git_updates: Vec::new(),
            deferred_publishers: BTreeSet::new(),
            catalogs: BTreeMap::new(),
            request,
            selected: installed
                .iter()
                .map(|(name, pkg)| (name.clone(), pkg.published.sha256.clone()))
                .collect(),
            package_changes: Vec::new(),
            intent_changes: Vec::new(),
            freeze_changes: Vec::new(),
            recomposed: BTreeSet::new(),
            transfers: Vec::new(),
            preserved: Vec::new(),
            config_update_revision: None,
            writes: Vec::new(),
            systemd: Vec::new(),
            triggers: Vec::new(),
            retired_triggers: Vec::new(),
            kernel_install: None,
            payload_bytes: 0,
        }
    }

    pub(crate) fn transaction_details(&self) -> TransactionDetails {
        TransactionDetails {
            base_image: self.image.clone(),
            base_revision: self.base_revision,
            request: self.request.clone(),
            force_phased_update: self.force_phased_update,
            package_changes: self.package_changes.clone(),
            intent_changes: self.intent_changes.clone(),
            freeze_changes: self.freeze_changes.clone(),
            catalogs: self.catalogs.clone(),
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
        if let Request::SetMediator { group, provider } = &self.request {
            lines.push(match provider {
                Some(provider) => format!("Prefer {provider} for mediator {group}"),
                None => format!("Use the default provider for mediator {group}"),
            });
        }
        if self.force_phased_update {
            lines.push("Allow upgrade outside the phased rollout".into());
        }
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
        let composition = match &self.request {
            Request::ChangeFacet {
                facets,
                with_dependencies,
            } => {
                for (name, value) in facets {
                    let scope = if *with_dependencies && name.contains(':') {
                        " with dependencies"
                    } else {
                        ""
                    };
                    lines.push(match value {
                        Some(value) => format!("Set facet {name}={value}{scope}"),
                        None => format!("Reset facet {name} to its default"),
                    });
                }
                true
            }
            Request::ChangeVariant { variants } => {
                for (name, value) in variants {
                    lines.push(match value {
                        Some(value) => format!("Set variant {name}={value}"),
                        None => format!("Unset variant {name}"),
                    });
                }
                true
            }
            _ => false,
        };
        if composition {
            for name in &self.recomposed {
                lines.push(format!("Recompose {name}"));
            }
            for write in &self.writes {
                if write.config_move.is_some() || is_config_update(&write.path) {
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
            .filter(|write| is_config_update(&write.path) && write.replaces_file())
        {
            lines.push(format!("Stage vendor config {:?}", write.path));
        }
        if let (Request::ResolveConfig { path, take }, Some(revision)) =
            (&self.request, self.config_update_revision)
        {
            match take {
                ConfigChoice::Local => lines.push(format!("Keep local config {path}")),
                ConfigChoice::Vendor => lines.push(format!(
                    "Install vendor config {path} from revision {revision}"
                )),
            }
        }
        if matches!(
            self.request,
            Request::Repair { .. } | Request::RepairPaths { .. }
        ) {
            for write in &self.writes {
                lines.push(format!("Repair {}", write.path));
            }
        }
        if matches!(self.request, Request::Revert { .. }) {
            for write in &self.writes {
                lines.push(format!("Restore vendor configuration {}", write.path));
            }
        }
        let transitions: Vec<_> = self
            .writes
            .iter()
            .filter(|write| write.transition().is_some())
            .collect();
        for write in &transitions {
            let kind = match &write.after {
                Some(Node::Dir { .. }) => "a directory".to_owned(),
                Some(Node::Symlink { target, .. }) => format!("a symbolic link to {target}"),
                Some(Node::Hardlink { target }) => format!("a hard link to {target}"),
                _ => "a file".to_owned(),
            };
            lines.push(format!("Replace {} with {kind}", write.path));
        }
        for write in self.writes.iter().filter(|write| {
            write.removes_directory()
                && !transitions
                    .iter()
                    .any(|transition| write.beneath(&transition.path))
        }) {
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
        for trigger in &self.retired_triggers {
            lines.push(format!(
                "Remove paths generated by trigger {}",
                trigger.label()
            ));
        }
        for trigger in &self.triggers {
            lines.push(format!("Run trigger {}", trigger.label()));
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
        ensure!(self.format == PLAN_FORMAT, "unsupported plan format");
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
                && after.format == IMAGE_FORMAT,
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
        let mut mediators = before.mediators.clone();
        match &self.request {
            Request::SetMediator { group, provider } => {
                if let Some(provider) = provider {
                    mediators.insert(group.clone(), provider.clone());
                } else {
                    mediators.remove(group);
                }
            }
            Request::InstallLock { lock } => mediators.clone_from(&lock.mediators),
            _ => {}
        }
        ensure!(
            after.mediators == mediators,
            "pending mediator preferences differ from the verified plan"
        );
        let mut policy = before.policy();
        match &self.request {
            Request::ChangeFacet {
                facets,
                with_dependencies,
            } => policy.change_facets(facets, *with_dependencies)?,
            Request::ChangeVariant { variants } => policy.change_variants(variants)?,
            _ => {}
        }
        ensure!(
            after.policy() == policy,
            "pending facet or variant policy differs from the verified plan"
        );
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
    plan_with_options(path, request, PlanOptions::default())
}

pub fn plan_with_recommends(
    path: &Path,
    request: Request,
    install_recommends: bool,
) -> Result<Plan> {
    plan_with_options(
        path,
        request,
        PlanOptions {
            install_recommends,
            ..PlanOptions::default()
        },
    )
}

#[derive(Clone, Copy, Debug)]
pub struct PlanOptions {
    pub install_recommends: bool,
    pub force_phased_update: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            install_recommends: true,
            force_phased_update: false,
        }
    }
}

pub fn plan_with_options(path: &Path, request: Request, options: PlanOptions) -> Result<Plan> {
    Ok(plan_image(path, request, options, None)?.0)
}

pub(crate) struct UpdateDiagnostics<'a> {
    pub held_back: &'a mut Vec<HeldBackUpdate>,
    pub explain: Option<&'a str>,
    pub advisories: Option<&'a mut Vec<ReleaseAdvisory>>,
}

pub(crate) fn inspect_update(
    path: &Path,
    diagnostics: UpdateDiagnostics<'_>,
    deferred_publishers: &BTreeSet<String>,
) -> Result<Plan> {
    let UpdateDiagnostics {
        held_back,
        explain,
        mut advisories,
    } = diagnostics;
    held_back.clear();
    if let Some(advisories) = &mut advisories {
        advisories.clear();
    }
    if let Some(name) = explain {
        validate_name(name).context("invalid package name for --explain")?;
    }
    let image = Image::open_query(path)?;
    image.require_clean()?;
    let (plan, next, _) = plan_with_state(
        &image,
        image.state()?,
        Request::Update,
        PlanOptions::default(),
        Some(UpdateDiagnostics {
            held_back,
            explain,
            advisories,
        }),
        UpdateSelection {
            deferred_publishers: Some(deferred_publishers),
            ..UpdateSelection::default()
        },
    )?;
    held_back.retain_mut(|update| {
        let Some(package) = next.installed.get(&update.name) else {
            return false;
        };
        let selected = &package.published.manifest.version;
        update.selected = Some(selected.clone());
        selected.semantic() < update.available.semantic()
    });
    Ok(plan)
}

pub(crate) fn plan_image(
    path: &Path,
    request: Request,
    options: PlanOptions,
    diagnostics: Option<UpdateDiagnostics<'_>>,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    let image = Image::open_query(path)?;
    image.require_clean()?;
    let state = image.state()?;
    plan_with_state(
        &image,
        state,
        request,
        options,
        diagnostics,
        UpdateSelection::default(),
    )
}

#[derive(Clone, Copy, Default)]
pub(crate) struct UpdateSelection<'a> {
    pub git_updates: &'a [String],
    pub deferred_publishers: Option<&'a BTreeSet<String>>,
}

pub(crate) fn plan_system(
    path: &Path,
    request: Request,
    options: PlanOptions,
    selection: UpdateSelection<'_>,
) -> Result<(Plan, crate::systemd::LiveServices)> {
    let image = Image::open_query(path)?;
    image.require_clean()?;
    let before = image.state()?;
    let (plan, after, _) =
        plan_with_state(&image, before.clone(), request, options, None, selection)?;
    let services = crate::systemd::LiveServices::plan(&plan, &before, &after);
    Ok((plan, services))
}

fn plan_with_state(
    image: &Image,
    state: State,
    request: Request,
    options: PlanOptions,
    diagnostics: Option<UpdateDiagnostics<'_>>,
    selection: UpdateSelection<'_>,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    let install_recommends = options.install_recommends
        && matches!(
            request,
            Request::Install { .. }
                | Request::ExactInstall { .. }
                | Request::Update
                | Request::UpdateProfile
                | Request::Autoremove
        );
    build_with_update_diagnostics(
        image,
        state,
        request,
        None,
        install_recommends,
        options.force_phased_update,
        diagnostics,
        selection,
    )
}

fn resolve_local<'a, P: PackageInfo + 'a>(
    state: &'a State,
    available: Candidates<'_, impl IntoIterator<Item = &'a P>>,
    requested: &BTreeMap<&'a str, PackageSpec<'a>>,
    update: bool,
    exact: bool,
    install_recommends: bool,
) -> Result<(BTreeMap<String, Installed>, BTreeMap<String, Managed>)> {
    let mut pinned = requested.clone();
    for name in state.settings.profile_packages.keys() {
        let manifest = &state.installed[name].published.manifest;
        ensure!(
            requested
                .get(name.as_str())
                .and_then(|spec| spec.version.as_ref())
                .is_none_or(|version| version == &manifest.version),
            "{name} is protected by the installed signed profile; apply a signed profile update to change it"
        );
        ensure!(
            available
                .obsoletions
                .get(name)
                .is_none_or(|cutoff| manifest.version.semantic() > cutoff.semantic()),
            "the current catalog obsoletes protected package {name}; apply a signed profile update first"
        );
        pinned.insert(
            name.as_str(),
            PackageSpec {
                publisher: Some(&manifest.publisher),
                name,
                version: Some(manifest.version.clone()),
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
        install_recommends,
        |installed| composition_paths(&state.settings, installed),
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

/// Installed packages whose selected actions differ between two selections.
fn recomposed_packages(
    before: Selection<'_>,
    after: Selection<'_>,
    installed: &BTreeMap<String, Installed>,
) -> Result<BTreeSet<String>> {
    let before = resolve_facets(before, installed)?;
    let after = resolve_facets(after, installed)?;
    Ok(installed
        .iter()
        .filter(|(name, package)| {
            package
                .published
                .manifest
                .action
                .iter()
                .any(|action| before.applies(name, action) != after.applies(name, action))
        })
        .map(|(name, _)| name.clone())
        .collect())
}

/// Refuse per-package facet rules that reach packages protected by a signed release
/// profile, unless the profile allows overriding those facets.
fn check_profile_facets(
    image: &Image,
    state: &State,
    profile: Option<&ReleaseProfile>,
    installed: &BTreeMap<String, Installed>,
) -> Result<()> {
    if state.settings.package_facets.is_empty() || state.settings.profile_packages.is_empty() {
        return Ok(());
    }
    let archived;
    let profile = match profile {
        Some(profile) => profile,
        None => {
            archived = image.archived_profile(state)?;
            archived
                .as_ref()
                .context("missing archived release profile")?
        }
    };
    let facets = resolve_facets(state.settings.selection(), installed)?;
    for name in state.settings.profile_packages.keys() {
        for (facet, rule) in facets.overrides(name) {
            ensure!(
                profile
                    .facet_overrides
                    .iter()
                    .any(|allowed| facet_pattern_covers(allowed, &facet)),
                "facet rule {rule} reaches {name}, which the signed release profile protects; the profile does not allow overriding facet {facet}"
            );
        }
    }
    Ok(())
}

fn composition_paths(
    settings: &Settings,
    installed: &BTreeMap<String, Installed>,
) -> Result<BTreeMap<String, Managed>> {
    let paths = effective_paths(settings, installed)?;
    kernel::selected(installed, &paths)?;
    Ok(paths)
}

pub(crate) fn build(
    image: &Image,
    state: State,
    request: Request,
    saved: Option<&Plan>,
    install_recommends: bool,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    build_with_update_diagnostics(
        image,
        state,
        request,
        saved,
        install_recommends,
        saved.is_some_and(|plan| plan.force_phased_update),
        None,
        UpdateSelection::default(),
    )
}

fn build_with_update_diagnostics(
    image: &Image,
    mut state: State,
    request: Request,
    saved: Option<&Plan>,
    install_recommends: bool,
    force_phased_update: bool,
    mut diagnostics: Option<UpdateDiagnostics<'_>>,
    selection: UpdateSelection<'_>,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    let git_updates = saved.map_or(selection.git_updates, |plan| plan.git_updates.as_slice());
    let empty = BTreeSet::new();
    let deferred_publishers = saved
        .map_or(selection.deferred_publishers.unwrap_or(&empty), |plan| {
            &plan.deferred_publishers
        });
    let root = &image.root;
    let updating = matches!(request, Request::Update | Request::UpdateProfile);
    if !deferred_publishers.is_empty() {
        ensure!(updating, "deferring publishers requires an upgrade");
        let publishers = &state.publishers;
        let required = publishers.signed_publishers().len().max(1);
        for name in deferred_publishers {
            ensure!(
                publishers.as_slice()[required..]
                    .iter()
                    .any(|publisher| &publisher.name == name),
                "cannot defer required or unknown publisher {name}"
            );
        }
    }
    ensure!(
        git_updates.is_empty() || updating,
        "Git update selections require an upgrade"
    );
    let git_selected = if git_updates.is_empty() {
        BTreeMap::new()
    } else {
        let selected = catalogs::requested(&state, git_updates)?;
        for (name, target) in &selected {
            let package = state
                .installed
                .get(*name)
                .context("Git update package is not installed")?;
            let publisher = &package.published.manifest.publisher;
            ensure!(
                target.publisher == Some(publisher)
                    && target.version.is_some()
                    && state
                        .publishers
                        .get(publisher)
                        .and_then(|publisher| publisher.git.as_ref())
                        .and_then(crate::git_install::GitSource::branch)
                        .is_some(),
                "Git update must select an exact version from the installed tracking branch"
            );
        }
        selected
    };
    ensure!(
        !force_phased_update || updating,
        "--force-phased-update requires an update request"
    );
    ensure!(
        state.settings.bootstrap_catalog.is_none()
            || matches!(
                request,
                Request::InstallProfile | Request::InstallLock { .. }
            ),
        "install the pinned bootstrap release with plan install-profile first, or provision it with plan install-lock"
    );
    if let Request::Mark { choice, packages } = &request {
        let (installed, intent_changes) = intent::mark(image, &mut state, *choice, packages)?;
        let mut plan =
            Plan::with_selection(&state.settings, request, install_recommends, &installed);
        plan.intent_changes = intent_changes;
        state.settings.revision = state.settings.next_revision()?;

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
        let mut plan = Plan::with_selection(
            &state.settings,
            request,
            install_recommends,
            &state.installed,
        );
        plan.freeze_changes = changes;
        state.settings.freezes = freezes;
        state.settings.revision = state.settings.next_revision()?;

        return Ok((plan, state, Vec::new()));
    }
    let mut catalogs = catalogs::Catalogs::default();
    let mut receipts = Vec::new();
    let mut next_policy = None;
    let mut recomposed = BTreeSet::new();
    let mut update_profile = matches!(request, Request::UpdateProfile);
    let (mut installed, paths) = match &request {
        Request::ChangeFacet { .. } | Request::ChangeVariant { .. } => {
            let mut policy = state.settings.policy();
            let profile = state.publishers.release_profile().is_some();
            match &request {
                Request::ChangeFacet {
                    facets,
                    with_dependencies,
                } => {
                    ensure!(!facets.is_empty(), "specify at least one facet change");
                    ensure!(
                        !profile || facets.keys().all(|key| key.contains(':')),
                        "the signed release profile fixes this image's image-wide facets; change PACKAGE:FACET rules instead"
                    );
                    policy.change_facets(facets, *with_dependencies)?;
                }
                Request::ChangeVariant { variants } => {
                    ensure!(!variants.is_empty(), "specify at least one variant change");
                    ensure!(
                        !profile,
                        "the signed release profile fixes this image's variants"
                    );
                    policy.change_variants(variants)?;
                }
                _ => unreachable!(),
            }
            ensure!(
                crate::verify::manifest_paths(&state)? == state.paths,
                "path ownership index differs from installed manifests"
            );
            image.verify_catalog_archives(&state)?;
            let paths = select_paths(
                policy.selection(&state.settings.arch),
                &state.settings.mediators,
                state.settings.directory(),
                &state.installed,
            )?;
            recomposed = recomposed_packages(
                state.settings.selection(),
                policy.selection(&state.settings.arch),
                &state.installed,
            )?;
            next_policy = Some(policy);
            (state.installed.clone(), paths)
        }
        Request::SetMediator { group, provider } => {
            validate_publisher(group).context("invalid mediator group")?;
            image.verify_catalog_archives(&state)?;
            ensure!(
                crate::verify::manifest_paths(&state)? == state.paths,
                "path ownership index differs from installed manifests"
            );
            if let Some(provider) = provider {
                validate_name(provider)?;
                ensure!(
                    mediator_offers(
                        &resolve_facets(state.settings.selection(), &state.installed)?,
                        &state.installed
                    )
                    .get(group.as_str())
                    .is_some_and(|offers| offers.contains_key(provider.as_str())),
                    "{provider} is not an installed provider for mediator {group}"
                );
                state
                    .settings
                    .mediators
                    .insert(group.clone(), provider.clone());
            } else {
                state.settings.mediators.remove(group);
            }
            (
                state.installed.clone(),
                effective_paths(&state.settings, &state.installed)?,
            )
        }
        Request::Repair { .. }
        | Request::RepairPaths { .. }
        | Request::Revert { .. }
        | Request::ResolveConfig { .. } => {
            image.verify_catalog_archives(&state)?;
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
                            version: Some(manifest.version.clone()),
                        },
                    )
                })
                .collect();
            resolve_local(
                &state,
                Candidates {
                    packages: std::iter::empty::<&Published>(),
                    obsoletions: &BTreeMap::new(),
                    freezes: &state.settings.freezes,
                    profile_packages: &state.settings.profile_packages,
                },
                &requested,
                false,
                true,
                install_recommends,
            )?
        }
        _ => {
            let lock = match &request {
                Request::InstallLock { lock } => {
                    lock.check_target(&state)?;
                    Some(lock.as_ref())
                }
                _ => None,
            };
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
                Request::InstallLock { lock } => lock.selectors(),
                _ => BTreeMap::new(),
            };
            catalogs = catalogs::load(
                image,
                &state,
                &requested,
                saved,
                updating,
                deferred_publishers,
                update::Rollout {
                    force: force_phased_update,
                    advisories: diagnostics
                        .as_mut()
                        .and_then(|report| report.advisories.as_deref_mut()),
                },
                lock,
            )?;
            // A shared Git catalog can contain approved builds from other boot roots.
            // Only a transaction that selected an exact version can advance this root.
            catalogs.candidates.packages.retain(|package| {
                let manifest = &package.manifest;
                if state
                    .publishers
                    .get(&manifest.publisher)
                    .is_none_or(|publisher| publisher.git.is_none())
                {
                    return true;
                }
                state
                    .installed
                    .get(&manifest.name)
                    .is_some_and(|installed| installed.published.sha256 == package.sha256)
                    || requested
                        .get(manifest.name.as_str())
                        .into_iter()
                        .chain(git_selected.get(manifest.name.as_str()))
                        .any(|target| {
                            target.publisher == Some(manifest.publisher.as_str())
                                && target.version.as_ref() == Some(&manifest.version)
                        })
            });
            update_profile |= matches!(request, Request::Update)
                && !state.installed.is_empty()
                && catalogs.profile.as_ref().is_some();
            if let Some(diagnostics) = &mut diagnostics {
                // Keep the known candidates available even if resolution fails.
                *diagnostics.held_back = update::candidates(&state, &catalogs, update_profile);
            }
            let track_profile_intent = install_profile
                || update_profile
                || (!requested.is_empty() && !state.settings.profile_packages.is_empty());
            if track_profile_intent && state.settings.profile_intent.is_none() {
                state.settings.profile_intent = Some(Image::profile_intent(&state));
            }
            catalogs.renames.check_freezes(&state.settings.freezes)?;
            catalogs.renames.check_profile(
                |name| state.settings.profile_packages.contains_key(name),
                catalogs.profile.as_ref().filter(|_| update_profile),
            )?;
            let mut successors = catalogs.renames.requirements();
            successors.extend(
                git_selected
                    .iter()
                    .map(|(&name, target)| (name, target.clone())),
            );
            let retained = state
                .settings
                .profile_intent
                .as_ref()
                .filter(|_| track_profile_intent)
                .map(|intent| {
                    let mut retained = intent.local.clone();
                    retained.extend(state.installed.iter().filter_map(|(name, package)| {
                        deferred_publishers
                            .contains(&package.published.manifest.publisher)
                            .then(|| name.clone())
                    }));
                    retained
                });
            let select = |state: &State, requirements: &BTreeMap<&str, PackageSpec<'_>>| {
                if update_profile {
                    let profile = catalogs
                        .profile
                        .as_ref()
                        .context("missing signed release profile")?;
                    catalogs.candidates.update_profile(
                        &state.installed,
                        profile,
                        &state.settings.freezes,
                        retained.as_ref(),
                        requirements,
                        install_recommends,
                        |installed| composition_paths(&state.settings, installed),
                    )
                } else {
                    resolve_local(
                        state,
                        Candidates {
                            packages: &catalogs.candidates.packages,
                            obsoletions: &catalogs.candidates.obsoletions,
                            freezes: &state.settings.freezes,
                            profile_packages: &state.settings.profile_packages,
                        },
                        requirements,
                        matches!(request, Request::Update),
                        matches!(request, Request::ExactInstall { .. }),
                        install_recommends,
                    )
                }
            };
            let (mut installed, paths) = if let Some(lock) = lock {
                let mut installed = lock.replay(&catalogs.candidates.packages)?;
                state.settings.mediators.clone_from(&lock.mediators);
                if let Some(profile) = &catalogs.profile {
                    state
                        .settings
                        .profile_packages
                        .clone_from(&profile.composition);
                    state.settings.check_profile_packages(&installed)?;
                    let roots: BTreeSet<_> = profile.roots.keys().cloned().collect();
                    for name in &roots {
                        installed
                            .get_mut(name)
                            .context("locked image omits a signed profile root")?
                            .explicit = true;
                    }
                    state.settings.profile_intent = Some(ProfileIntent {
                        roots,
                        local: lock.local_roots.clone(),
                    });
                }
                resolve::check(&installed).context("checking dependencies of the locked image")?;
                freeze::check(&state.settings.freezes, &installed)?;
                let paths = composition_paths(&state.settings, &installed)?;
                (installed, paths)
            } else if install_profile {
                let profile = catalogs
                    .profile
                    .as_ref()
                    .context("missing signed release profile")?;
                let installed = catalogs.candidates.replay(profile)?;
                freeze::check(&state.settings.freezes, &installed)?;
                let paths = effective_paths(&state.settings, &installed)?;
                (installed, paths)
            } else {
                select(&state, if updating { &successors } else { &requested })?
            };
            if let Some(diagnostics) = diagnostics
                && let Some(name) = diagnostics.explain
                && !state.settings.freezes.contains_key(name)
                && let Some(candidate) = diagnostics
                    .held_back
                    .iter_mut()
                    .find(|update| update.name == name)
                && installed.get(name).is_some_and(|package| {
                    package.published.manifest.version.semantic() < candidate.available.semantic()
                })
            {
                let mut forced = successors.clone();
                forced.insert(
                    name,
                    PackageSpec {
                        publisher: Some(&candidate.publisher),
                        name,
                        version: Some(candidate.available.clone()),
                    },
                );
                candidate.reason = match select(&state, &forced).and_then(|(installed, _)| {
                    catalogs.verify_builds(image, &state, &installed)
                }) {
                    Ok(_) => "The newer version fits when explicitly required; the automatic upgrade selected another combination".into(),
                    Err(error) => format!("{error:#}"),
                };
            }
            for name in git_selected.keys() {
                if let Some(package) = installed.get_mut(*name) {
                    package.explicit = state.installed[*name].explicit;
                }
            }
            if let Some(intent) = &mut state.settings.profile_intent {
                catalogs.renames.transfer_local(&mut intent.local);
            } else if !update_profile {
                catalogs
                    .renames
                    .transfer_explicit(&state.installed, &mut installed);
            }
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
            if lock.is_none()
                && let Some(intent) = &mut state.settings.profile_intent
            {
                if matches!(request, Request::ExactInstall { .. }) {
                    intent.local.clear();
                }
                intent
                    .local
                    .extend(requested.keys().map(|name| (*name).to_owned()));
            }
            state.settings.catalog_checkpoints = std::mem::take(&mut catalogs.checkpoints);
            (installed, paths)
        }
    };
    if let Some(intent) = &mut state.settings.profile_intent {
        intent.local.retain(|name| installed.contains_key(name));
        for (name, package) in &mut installed {
            package.explicit = intent.roots.contains(name) || intent.local.contains(name);
        }
    }
    freeze::check(&state.settings.freezes, &installed)?;
    state.settings.check_state_paths(&paths)?;
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
        force_phased_update,
        git_updates: git_updates.to_vec(),
        deferred_publishers: deferred_publishers.clone(),
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
            install_recommends,
            &installed,
        )
    };
    for name in state
        .installed
        .keys()
        .chain(installed.keys())
        .collect::<BTreeSet<_>>()
    {
        let previous = state.installed.get(name).map(|old| &old.published.manifest);
        let selected = installed.get(name).map(|new| &new.published.manifest);
        let from = previous.map(|manifest| manifest.version.to_string());
        let to = selected.map(|manifest| manifest.version.to_string());
        if from != to {
            let publisher = if state.publishers.as_slice().len() > 1 {
                selected
                    .or(previous)
                    .map(|manifest| manifest.publisher.clone())
            } else {
                None
            };
            plan.package_changes.push(PackageChange {
                name: name.clone(),
                publisher,
                from,
                to,
                requires_reboot: previous
                    .into_iter()
                    .chain(selected)
                    .any(|manifest| manifest.requires_reboot),
            });
        }
    }

    triggers::check_conflicts(&installed, &paths)?;
    let mut writes = match &plan.request {
        Request::Repair { packages } => {
            repair::writes(root, &state, packages, &[], &mut plan.preserved)?
        }
        Request::RepairPaths { paths } => {
            ensure!(!paths.is_empty(), "path repair requires at least one path");
            repair::writes(root, &state, &[], paths, &mut plan.preserved)?
        }
        Request::Revert { paths: requested } => {
            ensure!(!requested.is_empty(), "revert requires at least one path");
            let selected = crate::verify::selected_paths(&state, &[], requested)?;
            let selected = selected
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            repair::revert(root, &mut state.settings, &paths, &selected)?
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
    // Directories that this transaction replaces with a file or symlink.
    // Paths sort after their ancestors, so each is known before its contents.
    let mut replaced_directories = BTreeSet::new();
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
        if !after.is_some_and(|target| target.preserve)
            || before.map(|old| &old.node) != after.map(|new| &new.node)
        {
            retired_update = state.settings.config_updates.remove(path);
        }
        let observed = root.inspect(path)?;
        let mut local_config = false;
        let replaced = within(path, &replaced_directories);
        let destination = match after {
            Some(target) if target.node.is_dir() => match &observed {
                None => Some(target.node.clone()),
                Some(existing)
                    if !existing.is_dir() && before.is_some_and(|old| !old.node.is_dir()) =>
                {
                    // A managed file or symlink becomes a directory. The new
                    // tree is assembled in staging and swapped into place.
                    ensure!(
                        !matches!(existing, Node::Special),
                        "path type conflict at {path}"
                    );
                    if let Some(old) = before
                        && old.preserve
                        && observed.as_ref() != Some(&old.node)
                    {
                        anyhow::bail!(
                            "replacing {path} with a directory would discard local config; \
                             move it aside or restore the vendor configuration with revert, then plan again"
                        );
                    }
                    Some(target.node.clone())
                }
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
            Some(target)
                if matches!(observed, Some(Node::Dir { .. }))
                    && before.is_some_and(|old| old.node.is_dir()) =>
            {
                // A managed directory becomes a file or symlink. Its contents
                // must all leave in this transaction; see check_replaced_directory.
                replaced_directories.insert(path.as_str());
                Some(target.node.clone())
            }
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
                if replaced
                    || state.settings.created_directories.contains(path)
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
                    if next_policy.is_some() || observed.is_some() {
                        config::remember_unowned(
                            &state.installed,
                            &mut state.settings,
                            path,
                            old,
                            observed.clone(),
                            retired_update,
                            next_policy.is_none(),
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
    let generated = triggers::active_outputs(&state.installed, &state.paths);
    for directory in &replaced_directories {
        check_replaced_directory(root, directory, &state, &writes, &generated)?;
    }
    let order = WriteOrder::new(
        (&state.installed, &state.paths),
        (&installed, &paths),
        &retired,
    );
    plan.writes = ordered_writes(root, &paths, writes, &order)?;
    let planned = triggers::plan(
        &state.installed,
        &state.paths,
        &installed,
        &paths,
        &plan.writes,
    )?;
    plan.triggers = planned.run;
    plan.retired_triggers = planned.retire;
    if plan.kernel_install.is_none() {
        plan.kernel_install = regenerated_initrd(&plan.triggers, &installed, &paths)?;
    }
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
    if let Some(policy) = next_policy {
        state.settings.set_policy(policy);
    }
    plan.recomposed = recomposed;
    check_profile_facets(image, &state, catalogs.profile.as_ref(), &installed)?;
    config::retain_disabled(&mut state.settings, &installed, &paths);

    state
        .settings
        .created_directories
        .retain(|path| paths.get(path).is_some_and(|managed| managed.node.is_dir()));
    state.settings.created_directories.extend(
        plan.writes
            .iter()
            .filter(|write| {
                write.before.as_ref().is_none_or(|node| !node.is_dir())
                    && write.after.as_ref().is_some_and(Node::is_dir)
                    && paths.contains_key(&write.path)
            })
            .map(|write| write.path.clone()),
    );
    state.installed = installed;
    state.paths = paths;
    Ok((plan, state, receipts))
}

/// The selected kernel of a composition, when planned triggers regenerate
/// its generated initramfs. The prepared boot artifacts must follow it.
fn regenerated_initrd(
    runs: &[TriggerRun],
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
) -> Result<Option<Artifacts>> {
    let Some(selected) = kernel::selected(installed, paths)? else {
        return Ok(None);
    };
    Ok((selected.generated_initrd
        && triggers::regenerates(runs, installed, &selected.kernel.initrd_path())?)
    .then_some(selected))
}

pub(crate) fn pending_repairs(root: &Root, state: &State) -> Result<Vec<Write>> {
    let paths = state.paths.keys().map(String::as_str);
    let writes = repair::write_paths(root, state, paths, &mut Vec::new())?;
    let current = (&state.installed, &state.paths);
    let retired = BTreeSet::new();
    ordered_writes(
        root,
        &state.paths,
        writes,
        &WriteOrder::new(current, current, &retired),
    )
}

/// Whether `path` lies beneath one of `directories`.
fn within(path: &str, directories: &BTreeSet<&str>) -> bool {
    path.match_indices('/')
        .any(|(index, _)| directories.contains(&path[..index]))
}

/// Refuse to replace a directory whose contents would not all leave with it.
///
/// Every entry beneath the directory must be a managed path that this
/// transaction removes, or a path that a committed trigger generated, which
/// leaves with the old tree. CPS never discards unmanaged files or locally
/// modified configuration, and it does not move them elsewhere either: the
/// administrator decides where they belong before the directory goes away.
fn check_replaced_directory(
    root: &Root,
    directory: &str,
    committed: &State,
    writes: &BTreeMap<String, Write>,
    generated: &[&str],
) -> Result<()> {
    const LISTED: usize = 20;
    let mut kept = Vec::new();
    root.walk_beneath(directory, |path, _| {
        if writes.get(path).is_some_and(|write| write.after.is_none()) {
            return Ok(true);
        }
        if !committed.paths.contains_key(path) && triggers::generated(generated, path) {
            return Ok(false);
        }
        kept.push(match committed.paths.get(path) {
            Some(managed) if managed.preserve => format!("{path} (locally modified configuration)"),
            Some(_) => format!("{path} (kept by this transaction)"),
            None if committed.settings.dormant_configs.contains_key(path) => {
                format!("{path} (retained local configuration)")
            }
            None => format!("{path} (not managed by CPS)"),
        });
        Ok(false)
    })
    .with_context(|| format!("checking the contents of {directory} before replacing it"))?;
    if kept.is_empty() {
        return Ok(());
    }
    let more = kept.len().saturating_sub(LISTED);
    kept.truncate(LISTED);
    let mut message = format!(
        "cannot replace directory {directory}: this transaction would not remove these paths beneath it:\n  {}",
        kept.join("\n  ")
    );
    if more > 0 {
        message.push_str(&format!("\n  and {more} more"));
    }
    message.push_str(
        "\nMove or remove them, or restore vendor configuration with revert, then plan again",
    );
    anyhow::bail!(message)
}

fn ordered_writes(
    root: &Root,
    paths: &BTreeMap<String, Managed>,
    mut writes: BTreeMap<String, Write>,
    order: &WriteOrder<'_>,
) -> Result<Vec<Write>> {
    // Replacing a regular file swaps its inode. Its aliases still refer to
    // the old inode even when their actions have not changed.
    for (path, managed) in paths {
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
    let mut ordered: Vec<_> = writes.into_values().collect();
    order.sort(&mut ordered);
    Ok(ordered)
}

/// Paths whose changes take effect only after a reboot, so a transaction
/// that changes them must use a staged boot environment.
const BOOT_SENSITIVE: &[&str] = &[
    "boot",
    "efi",
    "usr/lib/modules",
    "usr/lib/firmware",
    "usr/lib/kernel",
    "usr/lib/systemd/boot",
    "usr/lib/systemd/systemd",
    "usr/bin/cps",
    "etc/cps",
    "etc/fstab",
    "etc/crypttab",
];
const BOOT_SENSITIVE_LOADERS: &[&str] = &["usr/lib/ld-linux", "lib64/ld-linux"];

/// Whether a trigger output pattern can generate a boot-sensitive path.
pub(crate) fn boot_sensitive_pattern(pattern: &str) -> bool {
    BOOT_SENSITIVE
        .iter()
        .any(|prefix| triggers::intersects(pattern, prefix))
        || BOOT_SENSITIVE_LOADERS
            .iter()
            .any(|prefix| triggers::intersects(pattern, &format!("{prefix}*")))
}

impl Plan {
    /// A plan that returns every path this plan journals to its state
    /// before the plan, for aborting it. Applied to the image in any mix of
    /// before and after states, it leaves the committed revision `base`.
    ///
    /// Each write runs backwards. Local configuration copied by a move or a
    /// metadata change is copied back from where the plan put it, aliases
    /// declared as hard links in `base` are linked again to their restored
    /// targets, and other files come from package payloads. Systemd actions
    /// and triggers are those of the change from `next` back to `base`, so
    /// generated outputs follow the restored files: triggers that `next`
    /// added are retired, and those it retired or whose inputs it changed
    /// run again.
    ///
    /// The prepared artifacts of a kernel live in a directory named after
    /// its packaged files, so preparing `base`'s own kernel again for a
    /// regenerated initramfs replaced them. The abort then prepares that
    /// kernel again, as it does whenever its triggers regenerate the
    /// initramfs of `base`'s kernel. A different kernel that the plan
    /// prepared left `base`'s artifacts untouched.
    pub(crate) fn inverse(&self, base: &State, next: &State) -> Result<Self> {
        let mut writes = BTreeMap::new();
        let mut retired = BTreeSet::new();
        for write in &self.writes {
            if write.before == write.after && !write.detach {
                continue;
            }
            let before = match &write.after {
                // A hard link reads as its target's inode.
                Some(Node::Hardlink { target }) => Some(
                    next.paths
                        .get(target)
                        .with_context(|| format!("missing hard link target {target}"))?
                        .node
                        .clone(),
                ),
                other => other.clone(),
            };
            let after = match base.paths.get(&write.path) {
                Some(
                    managed @ Managed {
                        node: Node::Hardlink { .. },
                        ..
                    },
                ) if write.before.is_some() => Some(managed.node.clone()),
                _ => write.before.clone(),
            };
            if let Some(migration) = &write.config_move
                && migration.from != write.path
            {
                retired.insert(write.path.as_str());
            }
            writes.insert(
                write.path.clone(),
                Write {
                    path: write.path.clone(),
                    before,
                    after,
                    detach: false,
                    local_config: write.local_config,
                    config_move: None,
                },
            );
        }
        // A moved configuration's local contents go back to their source.
        for write in &self.writes {
            if let Some(migration) = &write.config_move
                && migration.local.is_some()
                && migration.from != write.path
                && let Some(source) = writes.get_mut(&migration.from)
            {
                let local = write
                    .after
                    .clone()
                    .context("moved configuration has no destination contents")?;
                source.config_move = Some(ConfigMove {
                    from: write.path.clone(),
                    local: Some(local),
                });
            }
        }
        for (path, managed) in &base.paths {
            if let Node::Hardlink { target } = &managed.node {
                ensure!(
                    !writes.get(target).is_some_and(Write::replaces_file)
                        || writes.contains_key(path),
                    "aborting would leave {path} linked to the replaced {target}"
                );
            }
        }
        let mut writes: Vec<_> = writes.into_values().collect();
        WriteOrder::new(
            (&next.installed, &next.paths),
            (&base.installed, &base.paths),
            &retired,
        )
        .sort(&mut writes);
        let mut inverse = self.clone();
        inverse.writes = writes;
        inverse.systemd = systemd::plan_actions(next, &base.installed, &base.paths);
        let planned = triggers::plan(
            &next.installed,
            &next.paths,
            &base.installed,
            &base.paths,
            &inverse.writes,
        )?;
        inverse.triggers = planned.run;
        inverse.retired_triggers = planned.retire;
        inverse.kernel_install = match kernel::selected(&base.installed, &base.paths)? {
            Some(selected) if self.kernel_install.as_ref() == Some(&selected) => Some(selected),
            _ => regenerated_initrd(&inverse.triggers, &base.installed, &base.paths)?,
        };
        inverse.transfers.clear();
        inverse.preserved.clear();
        for change in &mut inverse.package_changes {
            std::mem::swap(&mut change.from, &mut change.to);
        }
        Ok(inverse)
    }
}

impl Write {
    pub(crate) fn boot_sensitive(&self) -> bool {
        let path = Path::new(&self.path);
        (self.before != self.after || self.detach)
            && (BOOT_SENSITIVE.iter().any(|prefix| path.starts_with(prefix))
                || BOOT_SENSITIVE_LOADERS
                    .iter()
                    .any(|prefix| self.path.starts_with(prefix)))
    }

    pub(crate) fn beneath(&self, directory: &str) -> bool {
        self.path
            .strip_prefix(directory)
            .is_some_and(|rest| rest.starts_with('/'))
    }

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

    /// Whether this write replaces a directory with a file or symlink
    /// (`Some(false)`) or a file or symlink with a directory (`Some(true)`).
    ///
    /// Such a write also carries every write beneath its path: the new
    /// object, with any new directory tree, is assembled in staging and
    /// exchanged with the old one in a single rename.
    pub(crate) fn transition(&self) -> Option<bool> {
        match (&self.before, &self.after) {
            (Some(before), Some(after))
                if before.is_dir() != after.is_dir() && !matches!(before, Node::Special) =>
            {
                Some(after.is_dir())
            }
            _ => None,
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
    format!("{CONFIG_UPDATES_DIRECTORY}/{revision}/")
}

fn stage_config(
    root: &Root,
    settings: &mut Settings,
    revision: u64,
    path: &str,
    node: &Node,
    writes: &mut BTreeMap<String, Write>,
) -> Result<()> {
    settings.config_updates.insert(path.to_owned(), revision);
    let path = config::vendor_copy(revision, path);
    let depth = STATE_DIRECTORY.split('/').count();
    for (index, _) in path.match_indices('/').skip(depth) {
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
