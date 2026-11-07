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
    BootEnvironment, PackageChange, PhasedUpdateDeferred, Plan, PlanOptions, Request,
    apply::{apply_live, resume_live},
    boot::{
        activate_btrfs_boot_environment, entry_id, stage_running_btrfs, with_running_environment,
    },
    fetch_plan,
    image::Image,
    kdl_format::{BtrfsConfig, ImageDefinition, read_image_definition},
    list_boot_environments,
    model::validate_publisher,
    plan::{UpdateDiagnostics, inspect_update, plan_system},
    query::{InstallSource, SelectSource, select_install_sources},
    release::{SyncOptions, refresh_url_publishers, sync_profile_for_image, sync_with_options},
    systemd::LiveServices,
};

pub use crate::plan::HeldBackUpdate;

pub const CONFIG: &str = "/etc/cps/image.kdl";
const ROOT: &str = "/";

pub struct System {
    config: PathBuf,
    definition: ImageDefinition,
    plan_options: PlanOptions,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phased_update: Option<PhasedUpdateDeferred>,
    pub blockers: Vec<String>,
    pub reboot_required: Option<bool>,
    pub reboot_reason: Option<String>,
}

impl UpdateReport {
    fn from_plan(result: Result<Change>, held_back: Vec<HeldBackUpdate>) -> Self {
        let phased_update = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<PhasedUpdateDeferred>())
            .cloned();
        let (status, changes, blockers, reboot_reason) = match result {
            Ok(change) => {
                let status = if !change.is_empty() {
                    UpdateStatus::UpdatesAvailable
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
        let _lock = self.lock()?;
        let boot = self.boot();
        crate::boot::with_registered_images(
            &boot.volume,
            &boot.esp,
            &boot.activation.root_partuuid,
            |images| {
                crate::release::prune_with_registered_roots(
                    repo, publisher, key, keep, images, apply,
                )
            },
        )
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

    pub fn plan(&self, request: Request, refresh: bool) -> Result<Change> {
        self.plan_request(request, refresh, None, None)
    }

    pub fn plan_with_source_selection(
        &self,
        request: Request,
        refresh: bool,
        mut choose: impl FnMut(&str, &[InstallSource]) -> Result<usize>,
    ) -> Result<Change> {
        self.plan_request(request, refresh, Some(&mut choose), None)
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
        let mut held_back = Vec::new();
        let result = (|| {
            let system = Self::load(config)?;
            let _lock = system.lock()?;
            let diagnostics = UpdateDiagnostics {
                held_back: &mut held_back,
                explain,
            };
            system.plan_request(Request::Update, refresh, None, Some(diagnostics))
        })();
        UpdateReport::from_plan(result, held_back)
    }

    fn plan_request(
        &self,
        mut request: Request,
        refresh: bool,
        choose: Option<&mut SelectSource<'_>>,
        diagnostics: Option<UpdateDiagnostics<'_>>,
    ) -> Result<Change> {
        let uses_current_catalogs = matches!(request, Request::Install { .. } | Request::Update);
        ensure!(
            uses_current_catalogs
                || matches!(request, Request::Remove { .. } | Request::Autoremove),
            "expected an install, upgrade, remove, or autoremove request"
        );
        self.check_running()?;
        if refresh && uses_current_catalogs {
            self.refresh(matches!(request, Request::Update))?;
        }
        if let (Request::Install { packages }, Some(choose)) = (&mut request, choose) {
            select_install_sources(Path::new(ROOT), packages, choose)?;
        }
        let root = Path::new(ROOT);
        if let Some(diagnostics) = diagnostics {
            let plan = inspect_update(root, diagnostics)?;
            return Ok(Change::from_plan(plan, &self.definition.stage_packages));
        }
        let (plan, services) = plan_system(root, request, self.plan_options)?;
        let mut change = Change::from_plan(plan, &self.definition.stage_packages);
        change.live_services = Some(services);
        Ok(change)
    }

    pub fn apply(&self, change: &Change) -> Result<Option<String>> {
        self.check_running()?;
        fetch_plan(Path::new(ROOT), &change.plan).context("fetching the approved packages")?;
        let boot = self.boot();
        if change.boot_reason.is_none() {
            with_running_environment(
                &boot.volume,
                &boot.esp,
                &boot.activation.root_partuuid,
                |root, id, revision| {
                    apply_live(
                        Image::open_for_live_apply(root, id, revision)?,
                        &change.plan,
                        &self.definition.stage_packages,
                    )
                },
            )?;
            return Ok(None);
        }

        let plan_id = change.plan.id()?;
        let output = boot.environments.join(format!("upgrade-{plan_id}"));
        let root = output.join("root");
        if output.try_exists()? {
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
        } else {
            stage_running_btrfs(&output, &change.plan)?;
        }
        let id = activate_btrfs_boot_environment(&root, &boot.volume, &boot.esp, &boot.activation)
            .with_context(|| {
                format!("activating the prepared environment at {}", root.display())
            })?;
        Ok(Some(id))
    }

    pub fn resume(&self) -> Result<Option<u64>> {
        let boot = self.boot();
        with_running_environment(
            &boot.volume,
            &boot.esp,
            &boot.activation.root_partuuid,
            |root, id, revision| resume_live(Image::open_for_live_apply(root, id, revision)?),
        )
    }

    pub fn boot_status(&self) -> Result<(String, Vec<BootEnvironment>)> {
        let settings = Image::open_query(Path::new(ROOT))?.settings()?;
        Ok((
            entry_id(&settings.id, settings.boot_revision()),
            list_boot_environments(&self.boot().esp)?,
        ))
    }

    fn check_running(&self) -> Result<()> {
        let boot = self.boot();
        with_running_environment(
            &boot.volume,
            &boot.esp,
            &boot.activation.root_partuuid,
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

    fn refresh(&self, upgrade: bool) -> Result<()> {
        self.refresh_configured_mirrors(upgrade)?;
        if upgrade {
            refresh_url_publishers(Path::new(ROOT))?;
        }
        Ok(())
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
        let report = UpdateReport::from_plan(Err(deferred.clone().into()), Vec::new());
        assert_eq!(report.status, UpdateStatus::Deferred);
        assert!(report.blockers.is_empty());
        assert_eq!(report.reboot_required, Some(false));
        let json = serde_json::to_value(report)?;
        assert_eq!(json["status"], "deferred");
        assert_eq!(json["phased_update"], serde_json::to_value(deferred)?);
        Ok(())
    }
}
