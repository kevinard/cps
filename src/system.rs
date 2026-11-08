//! Everyday package operations on a running, registered Btrfs system.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, flock};
use serde::Serialize;

use crate::{
    BootEnvironment, GitSnapshotRef, PackageChange, PhasedUpdateDeferred, Plan, PlanOptions,
    ReleaseAdvisory, Request, TransactionPhase, TransactionProgress,
    apply::{PublisherFetchError, apply_live, repair_pending_live, resume_live},
    boot::{entry_id, stage_running_btrfs, with_running_environment},
    fetch_plan,
    git_install::{GitBuild, GitSource, GitStore},
    image::Image,
    image::publishers::register_git_publisher,
    kdl_format::{BtrfsConfig, ImageDefinition, read_image_definition},
    list_boot_environments,
    model::validate_publisher,
    plan::{UpdateDiagnostics, UpdateSelection, inspect_update, plan_system},
    query::{InstallSource, SelectSource, select_install_sources},
    release::{
        SyncOptions, prune_system_payloads, refresh_url_publishers, sync_profile_for_image,
        sync_with_options,
    },
    systemd::LiveServices,
    transaction_progress,
};

pub use crate::git_install::GitChange;
pub use crate::plan::HeldBackUpdate;

pub const CONFIG: &str = "/etc/cps/image.kdl";
const ROOT: &str = "/";
const GIT_TOOLCHAIN: &str = "/usr/share/cps/git-build/toolchain.kdl";

pub struct System {
    config: PathBuf,
    definition: ImageDefinition,
    plan_options: PlanOptions,
    base_only: bool,
}

#[derive(Debug)]
pub struct RetryBaseOnly;

impl std::fmt::Display for RetryBaseOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an optional publisher could not supply its payloads")
    }
}

impl std::error::Error for RetryBaseOnly {}

pub struct Change {
    plan: Plan,
    boot_reason: Option<String>,
    live_services: Option<LiveServices>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    UpToDate,
    UpdatesAvailable,
    Deferred,
    Blocked,
}

#[derive(Debug, Serialize)]
pub struct UpdateReport {
    pub status: UpdateStatus,
    pub changes: Vec<PackageChange>,
    pub held_back: Vec<HeldBackUpdate>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub deferred_publishers: BTreeSet<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<ReleaseAdvisory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phased_update: Option<PhasedUpdateDeferred>,
    pub blockers: Vec<String>,
    pub reboot_required: Option<bool>,
    pub reboot_reason: Option<String>,
}

impl UpdateReport {
    fn from_plan(
        result: Result<Change>,
        held_back: Vec<HeldBackUpdate>,
        advisories: Vec<ReleaseAdvisory>,
        deferred_publishers: BTreeSet<String>,
    ) -> Self {
        let phased_update = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<PhasedUpdateDeferred>())
            .cloned();
        let (status, changes, blockers, reboot_reason) = match result {
            Ok(change) => {
                let status = if !change.is_empty() {
                    UpdateStatus::UpdatesAvailable
                } else if !deferred_publishers.is_empty() && held_back.is_empty() {
                    UpdateStatus::Deferred
                } else if held_back.is_empty() {
                    UpdateStatus::UpToDate
                } else {
                    UpdateStatus::Blocked
                };
                (
                    status,
                    change.packages().to_vec(),
                    Vec::new(),
                    change.boot_reason,
                )
            }
            Err(_) if phased_update.is_some() => {
                (UpdateStatus::Deferred, Vec::new(), Vec::new(), None)
            }
            Err(error) => (
                UpdateStatus::Blocked,
                Vec::new(),
                vec![format!("{error:#}")],
                None,
            ),
        };
        Self {
            status,
            changes,
            held_back,
            deferred_publishers,
            advisories,
            phased_update,
            blockers,
            reboot_required: (status != UpdateStatus::Blocked).then_some(reboot_reason.is_some()),
            reboot_reason,
        }
    }
}

impl Change {
    pub(crate) fn from_plan(plan: Plan, stage_packages: &BTreeSet<String>) -> Self {
        let boot_reason = plan.boot_reason(stage_packages);
        Self {
            plan,
            boot_reason,
            live_services: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.plan.has_changes()
    }

    pub fn packages(&self) -> &[PackageChange] {
        self.plan.package_changes()
    }

    pub fn saved_plan(&self) -> &Plan {
        &self.plan
    }

    pub fn boot_reason(&self) -> Option<&str> {
        self.boot_reason.as_deref()
    }

    pub fn service_effects(&self) -> Vec<String> {
        let mut effects = self
            .plan
            .systemd
            .iter()
            .map(|action| format!("Run systemd {} for {}", action.kind(), action.name()))
            .collect::<Vec<_>>();
        if self.boot_reason.is_none()
            && let Some(services) = &self.live_services
        {
            effects.extend(services.preview());
        }
        effects
    }

    pub fn summary(&self) -> String {
        self.plan.summary()
    }

    /// Route a nonempty change through the trial boot workflow.
    pub fn with_staging(mut self) -> Self {
        if !self.is_empty() {
            self.boot_reason
                .get_or_insert_with(|| "--stage was requested".into());
        }
        self
    }
}

impl System {
    pub fn load(config: &Path) -> Result<Self> {
        let definition = read_image_definition(config)?;
        ensure!(
            definition.boot_btrfs.is_some(),
            "{} needs a boot-btrfs node for system package commands",
            config.display()
        );
        Ok(Self {
            config: config.to_owned(),
            definition,
            plan_options: PlanOptions::default(),
            base_only: false,
        })
    }

    pub fn with_install_recommends(mut self, enabled: bool) -> Self {
        self.plan_options.install_recommends = enabled;
        self
    }

    pub fn with_force_phased_update(mut self, enabled: bool) -> Self {
        self.plan_options.force_phased_update = enabled;
        self
    }

    pub fn with_base_only(mut self, enabled: bool) -> Self {
        self.base_only = enabled;
        self
    }

    fn boot(&self) -> &BtrfsConfig {
        self.definition
            .boot_btrfs
            .as_ref()
            .expect("validated boot configuration")
    }

    /// Hold this across refresh, confirmation, and application.
    pub fn lock(&self) -> Result<File> {
        let boot = self.boot();
        let volume = fs::canonicalize(&boot.volume)?;
        let environments = fs::canonicalize(&boot.environments)?;
        ensure!(
            environments != volume && environments.starts_with(&volume),
            "the environments directory must be beneath the Btrfs top-level mount"
        );
        let lock = File::open(&environments)?;
        let metadata = lock.metadata()?;
        ensure!(
            metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "the environments directory must be owned by root and writable only by root"
        );
        flock(&lock, FlockOperation::LockExclusive)?;
        Ok(lock)
    }

    /// Prune a system cache while retaining payloads for its registered boot roots.
    pub fn prune_cache(
        &self,
        repo: &Path,
        publisher: &str,
        key: &Path,
        keep: crate::release::CacheRetention<'_>,
        apply: bool,
    ) -> Result<crate::release::PruneReport> {
        self.prune_cache_with_locks(repo, publisher, key, keep, &[], apply)
    }

    pub fn prune_cache_with_locks(
        &self,
        repo: &Path,
        publisher: &str,
        key: &Path,
        keep: crate::release::CacheRetention<'_>,
        locks: &[crate::ImageLock],
        apply: bool,
    ) -> Result<crate::release::PruneReport> {
        let _lock = self.lock()?;
        let boot = self.boot();
        crate::boot::with_registered_images(
            &boot.volume,
            &boot.esp,
            boot.activation.root_partuuid.as_deref(),
            |images| {
                crate::release::prune_with_registered_roots(
                    repo, publisher, key, keep, images, locks, apply,
                )
            },
        )
    }

    // The system operation already holds the environments lock.
    fn prune_after_transaction(&self) {
        let Some(versions) = self.definition.cache_keep_versions else {
            return;
        };
        let boot = self.boot();
        let result = crate::boot::with_registered_images(
            &boot.volume,
            &boot.esp,
            boot.activation.root_partuuid.as_deref(),
            |images| {
                let mut seen = BTreeSet::new();
                for (_, image) in images {
                    let publishers = image.publishers()?;
                    for publisher in publishers.as_slice() {
                        if !seen.insert(publisher.repo.clone()) {
                            continue;
                        }
                        let result = (|| -> Result<()> {
                            if publisher.repo.join(".tuf").try_exists()? {
                                let report = prune_system_payloads(publisher, images, versions)?;
                                if !report.blobs.is_empty() {
                                    eprintln!(
                                        "Pruned {} cached payload(s) for {}, {} bytes",
                                        report.blobs.len(),
                                        publisher.name,
                                        report.bytes,
                                    );
                                }
                            }
                            Ok(())
                        })();
                        if let Err(error) = result {
                            eprintln!(
                                "Warning: could not prune {}'s cache: {error:#}",
                                publisher.name
                            );
                        }
                    }
                }
                Ok(())
            },
        );
        if let Err(error) = result {
            eprintln!("Warning: could not prune system caches: {error:#}");
        }
    }

    pub fn publisher_cache(&self, publisher: &str) -> Result<PathBuf> {
        validate_publisher(publisher)?;
        self.check_running()?;
        // This sibling subvolume is shared by the running root and its trials.
        Ok(self
            .boot()
            .environments
            .join("repositories")
            .join(publisher))
    }

    fn git_toolchain(&self) -> Result<&Path> {
        if let Some(config) = &self.definition.git_toolchain_config {
            return Ok(config);
        }
        let config = Path::new(GIT_TOOLCHAIN);
        ensure!(
            config.is_file(),
            "install system/git-build or set git-toolchain-config in the system image configuration"
        );
        Ok(config)
    }

    /// Prepare an explicitly selected Git commit and return its local package ID.
    pub fn prepare_git_install(
        &self,
        url: &str,
        reference: GitSnapshotRef<'_>,
        approve: impl FnOnce(&GitChange) -> Result<bool>,
    ) -> Result<Option<String>> {
        self.check_running()?;
        let source = GitSource::new(url, reference)?;
        let build =
            GitBuild::prepare(&source, None)?.context("Git install has no selected commit")?;
        let state = Image::open_query(Path::new(ROOT))?.state()?;
        if let Some(package) = state.installed.get(&build.manifest.name) {
            ensure!(
                package.published.manifest.publisher == build.manifest.publisher,
                "{} is already installed from another source; remove it before changing its source",
                build.manifest.name
            );
        }
        if !approve(&build.change)? {
            return Ok(None);
        }
        let toolchain = self.git_toolchain()?;
        let store = GitStore::open(self.publisher_cache(&source.publisher())?)?;
        let package = build.publish(&source, &store, toolchain)?;
        let boot = self.boot();
        with_running_environment(
            &boot.volume,
            &boot.esp,
            boot.activation.root_partuuid.as_deref(),
            |root, id, revision| {
                register_git_publisher(
                    &mut Image::open_for_live_apply(root, id, revision)?,
                    source,
                    &store.repo(),
                    &store.public_key(),
                    true,
                )
            },
        )?;
        Ok(Some(package))
    }

    /// Check opted-in Git sources and build the exact commits approved for this upgrade.
    pub fn prepare_git_updates(
        &self,
        mut approve: impl FnMut(&GitChange) -> Result<bool>,
    ) -> Result<Vec<String>> {
        self.check_running()?;
        let state = Image::open_query(Path::new(ROOT))?.state()?;
        let mut updates = Vec::new();
        for (name, package) in &state.installed {
            let manifest = &package.published.manifest;
            let publisher = state
                .publishers
                .get(&manifest.publisher)
                .context("missing installed publisher")?;
            let Some(source) = publisher
                .git
                .as_ref()
                .filter(|source| source.branch().is_some())
            else {
                continue;
            };
            if state.settings.freezes.contains_key(name) {
                continue;
            }
            let build = match GitBuild::prepare(source, Some(manifest)) {
                Ok(Some(build)) => build,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("Could not check Git update for {name}: {error:#}");
                    continue;
                }
            };
            if approve(&build.change)? {
                let path = self.publisher_cache(&publisher.name)?;
                ensure!(path.is_dir(), "missing Git publisher cache for {name}");
                let store = GitStore::open(path)?;
                ensure!(
                    fs::canonicalize(store.repo())? == publisher.repo
                        && crate::repo::read_key(&store.public_key())? == publisher.key,
                    "Git publisher cache or signing key changed for {name}"
                );
                updates.push(build.publish(source, &store, self.git_toolchain()?)?);
            }
        }
        Ok(updates)
    }

    pub fn plan(&self, request: Request, refresh: bool) -> Result<Change> {
        self.plan_request(request, refresh, None, &[])
    }

    pub fn plan_git_upgrade(&self, updates: &[String], refresh: bool) -> Result<Change> {
        self.plan_request(Request::Update, refresh, None, updates)
    }

    pub fn plan_with_source_selection(
        &self,
        request: Request,
        refresh: bool,
        mut choose: impl FnMut(&str, &[InstallSource]) -> Result<usize>,
    ) -> Result<Change> {
        self.plan_request(request, refresh, Some(&mut choose), &[])
    }

    /// Check the running system with the same refresh and planner as an upgrade.
    pub fn check_updates(config: &Path, refresh: bool) -> UpdateReport {
        Self::check_updates_with_explanation(config, refresh, None)
    }

    pub fn check_updates_with_explanation(
        config: &Path,
        refresh: bool,
        explain: Option<&str>,
    ) -> UpdateReport {
        Self::check_updates_with_scope(config, refresh, explain, false)
    }

    pub fn check_updates_with_scope(
        config: &Path,
        refresh: bool,
        explain: Option<&str>,
        base_only: bool,
    ) -> UpdateReport {
        let mut held_back = Vec::new();
        let mut advisories = Vec::new();
        let mut deferred = BTreeSet::new();
        let result = (|| {
            let system = Self::load(config)?.with_base_only(base_only);
            let _lock = system.lock()?;
            system.check_running()?;
            deferred = system.refresh_for_request(&Request::Update, refresh)?;
            let diagnostics = UpdateDiagnostics {
                held_back: &mut held_back,
                explain,
                advisories: Some(&mut advisories),
            };
            let plan = inspect_update(Path::new(ROOT), diagnostics, &deferred)?;
            Ok(Change::from_plan(plan, &system.definition.stage_packages))
        })();
        UpdateReport::from_plan(result, held_back, advisories, deferred)
    }

    fn plan_request(
        &self,
        mut request: Request,
        refresh: bool,
        choose: Option<&mut SelectSource<'_>>,
        git_updates: &[String],
    ) -> Result<Change> {
        let uses_current_catalogs = matches!(request, Request::Install { .. } | Request::Update);
        ensure!(
            uses_current_catalogs
                || matches!(
                    request,
                    Request::Remove { .. }
                        | Request::Autoremove
                        | Request::Repair { .. }
                        | Request::RepairPaths { .. }
                        | Request::Revert { .. }
                ),
            "unsupported system package request"
        );
        self.check_running()?;
        let deferred = self.refresh_for_request(&request, refresh)?;
        if let (Request::Install { packages }, Some(choose)) = (&mut request, choose) {
            select_install_sources(Path::new(ROOT), packages, choose)?;
        }
        let (plan, services) = plan_system(
            Path::new(ROOT),
            request,
            self.plan_options,
            UpdateSelection {
                git_updates,
                deferred_publishers: Some(&deferred),
            },
        )?;
        let mut change = Change::from_plan(plan, &self.definition.stage_packages);
        change.live_services = Some(services);
        Ok(change)
    }

    pub fn apply(&self, change: &Change) -> Result<Option<String>> {
        self.check_running()?;
        if let Err(error) = fetch_plan(Path::new(ROOT), &change.plan) {
            if !self.base_only
                && matches!(
                    change.plan.request,
                    Request::Update | Request::UpdateProfile
                )
                && let Some(PublisherFetchError(name)) = error.downcast_ref::<PublisherFetchError>()
                && self.is_optional_publisher(name).unwrap_or(false)
            {
                return Err(error.context(RetryBaseOnly));
            }
            return Err(error.context("fetching the approved packages"));
        }
        let boot = self.boot();
        if change.boot_reason.is_none() {
            with_running_environment(
                &boot.volume,
                &boot.esp,
                boot.activation.root_partuuid.as_deref(),
                |root, id, revision| {
                    apply_live(
                        Image::open_for_live_apply(root, id, revision)?,
                        &change.plan,
                        &self.definition.stage_packages,
                    )
                },
            )?;
            self.prune_after_transaction();
            return Ok(None);
        }

        let root = self.stage_change(change)?;
        let id = if boot.uefi {
            boot.activation
                .activate_btrfs_uefi(&root, &boot.volume, &boot.esp)
        } else if boot.uki {
            boot.activation
                .activate_btrfs_uki(&root, &boot.volume, &boot.esp)
        } else {
            boot.activation
                .activate_btrfs(&root, &boot.volume, &boot.esp)
        }
        .with_context(|| format!("activating the prepared environment at {}", root.display()))?;
        self.prune_after_transaction();
        Ok(Some(id))
    }

    /// Stage a recovery plan and verify the result before returning its root.
    pub fn prepare_recovery(&self, change: &Change) -> Result<PathBuf> {
        ensure!(
            change.plan.is_recovery(),
            "expected a repair or revert plan"
        );
        self.check_running()?;
        fetch_plan(Path::new(ROOT), &change.plan)
            .context("fetching the approved repair payloads")?;
        self.stage_change(change)
    }

    fn stage_change(&self, change: &Change) -> Result<PathBuf> {
        let boot = self.boot();
        let plan_id = change.plan.id()?;
        let output = boot.environments.join(format!("upgrade-{plan_id}"));
        let root = output.join("root");
        if root.try_exists()? {
            let staged = Image::open_query(&root)?;
            staged.require_clean()?;
            let settings = staged.settings()?;
            ensure!(
                settings.boot_parent.as_ref().is_some_and(|parent| {
                    parent.id == change.plan.image
                        && parent.image_revision() == change.plan.base_revision
                }) && change.plan.base_revision.checked_add(1) == Some(settings.revision)
                    && staged.applied(&plan_id)? == Some(settings.revision),
                "existing staged environment does not match the approved transaction"
            );
            for finding in crate::verify::verify_image(&staged)? {
                ensure!(
                    !finding.is_error,
                    "prepared environment verification failed: {}",
                    finding.message
                );
            }
        } else {
            if output.try_exists()? {
                ensure!(
                    list_boot_environments(&boot.esp)?
                        .iter()
                        .all(|entry| !entry.is_retiring),
                    "finish retiring boot environments before reusing the staging directory"
                );
                fs::remove_dir(&output).with_context(|| {
                    format!("removing the empty staging directory {}", output.display())
                })?;
            }
            stage_running_btrfs(&output, &change.plan)?;
        }
        Ok(root)
    }

    pub fn resume(&self) -> Result<Option<u64>> {
        self.resume_transaction(false)
    }

    pub fn resume_with_repair(&self) -> Result<Option<u64>> {
        self.resume_transaction(true)
    }

    fn resume_transaction(&self, repair: bool) -> Result<Option<u64>> {
        let boot = self.boot();
        let revision = with_running_environment(
            &boot.volume,
            &boot.esp,
            boot.activation.root_partuuid.as_deref(),
            |root, id, revision| {
                let image = Image::open_for_live_apply(root, id, revision)?;
                if repair {
                    repair_pending_live(image, &self.definition.stage_packages)
                } else {
                    resume_live(image)
                }
            },
        )?;
        self.prune_after_transaction();
        Ok(revision)
    }

    pub fn boot_status(&self) -> Result<(String, Vec<BootEnvironment>)> {
        let settings = Image::open_query(Path::new(ROOT))?.settings()?;
        Ok((
            entry_id(&settings.id, settings.boot_revision()),
            list_boot_environments(&self.boot().esp)?,
        ))
    }

    /// Promote the running one-shot firmware trial after verifying its health.
    pub fn bless_uefi(&self) -> Result<Option<String>> {
        let boot = self.boot();
        ensure!(boot.uefi, "configure uefi=#true in boot-btrfs");
        crate::boot::bless_uefi(&boot.esp, &boot.volume)
    }

    pub fn transaction_progress(&self, plan: &Plan, validate: bool) -> Result<TransactionProgress> {
        let _lock = self.lock()?;
        let boot = self.boot();
        let (running, entries) = self.boot_status()?;
        let plan_id = plan.id()?;
        let staged = |image: &Image, phase| -> Result<Option<TransactionProgress>> {
            let settings = image.settings()?;
            let revision = settings.boot_revision();
            let from_plan = settings.boot_parent.as_ref().is_some_and(|parent| {
                parent.id == plan.image && parent.image_revision() == plan.base_revision
            }) && plan.base_revision.checked_add(1) == Some(revision);
            if !from_plan || image.applied(&plan_id)? != Some(revision) {
                return Ok(None);
            }
            Ok(Some(TransactionProgress {
                plan_id: plan_id.clone(),
                phase,
                revision,
                boot_environment: Some(entry_id(&settings.id, revision)),
            }))
        };
        let registered = crate::boot::with_registered_images(
            &boot.volume,
            &boot.esp,
            boot.activation.root_partuuid.as_deref(),
            |images| {
                for (id, image) in images {
                    let Some(entry) = entries.iter().find(|entry| entry.id == *id) else {
                        continue;
                    };
                    let phase = if *id == running {
                        if entry.tries_left.is_some() {
                            TransactionPhase::TrialBoot
                        } else {
                            TransactionPhase::BootedKnownGood
                        }
                    } else if entry.is_default {
                        TransactionPhase::StagedForBoot
                    } else {
                        TransactionPhase::InactiveBootEnvironment
                    };
                    if let Some(progress) = staged(image, phase)? {
                        return Ok(Some(progress));
                    }
                }
                Ok(None)
            },
        )?;
        if let Some(progress) = registered {
            return Ok(progress);
        }
        let prepared = boot.environments.join(format!("upgrade-{plan_id}/root"));
        if prepared.try_exists()?
            && let Some(progress) = staged(
                &Image::open_query(&prepared)?,
                TransactionPhase::PreparedForBoot,
            )?
        {
            return Ok(progress);
        }
        transaction_progress(Path::new(ROOT), plan, validate)
    }

    fn check_running(&self) -> Result<()> {
        let boot = self.boot();
        with_running_environment(
            &boot.volume,
            &boot.esp,
            boot.activation.root_partuuid.as_deref(),
            |root, id, revision| {
                let image = Image::open_query_root(root)?;
                let settings = image.settings()?;
                ensure!(
                    settings.id == id && settings.boot_revision() == revision,
                    "the running image differs from its boot entry"
                );
                image
                    .require_clean()
                    .context("run cps resume to finish an interrupted live change")
            },
        )
    }

    fn is_optional_publisher(&self, name: &str) -> Result<bool> {
        let publishers = Image::open_query(Path::new(ROOT))?.publishers()?;
        let required = publishers.signed_publishers().len().max(1);
        Ok(publishers.as_slice()[required..]
            .iter()
            .any(|publisher| publisher.name == name)
            && !self
                .definition
                .publishers
                .iter()
                .any(|publisher| publisher.name == name))
    }

    fn refresh_for_request(&self, request: &Request, refresh: bool) -> Result<BTreeSet<String>> {
        let upgrade = matches!(request, Request::Update);
        if refresh && (upgrade || matches!(request, Request::Install { .. })) {
            self.refresh_configured_mirrors(upgrade)?;
        }
        if upgrade && (refresh || self.base_only) {
            let required = self
                .definition
                .publishers
                .iter()
                .map(|publisher| publisher.name.as_str())
                .collect();
            return refresh_url_publishers(Path::new(ROOT), &required, self.base_only, refresh);
        }
        Ok(BTreeSet::new())
    }

    fn refresh_configured_mirrors(&self, upgrade: bool) -> Result<()> {
        let definition = &self.definition;
        if definition.mirrors.is_empty() {
            return Ok(());
        }
        let image = Image::open_query(Path::new(ROOT))?;
        let publishers = image.publishers()?;
        for publisher in &definition.publishers {
            if definition.mirrors.contains_key(&publisher.name) {
                let installed = publishers.get(&publisher.name).with_context(|| {
                    format!("{} is not configured in the running image", publisher.name)
                })?;
                ensure!(
                    fs::canonicalize(&publisher.repository)? == installed.repo,
                    "the cache path for {} differs from the running image",
                    publisher.name
                );
            }
        }
        drop(image);
        // Reuse a recently synchronized catalog for an install. An explicit
        // upgrade always checks the mirrors for newer releases.
        if !upgrade
            && definition
                .publishers
                .iter()
                .filter(|publisher| definition.mirrors.contains_key(&publisher.name))
                .all(|publisher| fresh(&publisher.repository.join("current")))
        {
            return Ok(());
        }
        eprintln!("Refreshing package catalogs…");
        if definition.release_profile.is_some() {
            sync_profile_for_image(&self.config, true, Path::new(ROOT))?;
        } else {
            for publisher in &definition.publishers {
                if let Some(mirror) = definition.mirrors.get(&publisher.name) {
                    sync_with_options(
                        &publisher.repository,
                        &mirror.root,
                        &mirror.metadata,
                        &mirror.targets,
                        &publisher.name,
                        &publisher.trust_key,
                        SyncOptions {
                            index_only: true,
                            image: Some(Path::new(ROOT)),
                        },
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(3600))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_report_distinguishes_rollout_deferral_from_a_blocker() -> Result<()> {
        let deferred = PhasedUpdateDeferred {
            publisher: "canopa".into(),
            profile: "desktop".into(),
            catalog: "a".repeat(64),
            percentage: 10,
            cohort: 30,
        };
        let report = UpdateReport::from_plan(
            Err(deferred.clone().into()),
            Vec::new(),
            Vec::new(),
            BTreeSet::new(),
        );
        assert_eq!(report.status, UpdateStatus::Deferred);
        assert!(report.blockers.is_empty());
        assert_eq!(report.reboot_required, Some(false));
        let json = serde_json::to_value(report)?;
        assert_eq!(json["status"], "deferred");
        assert_eq!(json["phased_update"], serde_json::to_value(deferred)?);
        Ok(())
    }
}
