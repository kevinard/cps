//! Everyday package operations on a running, registered Btrfs system.

use std::{
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, flock};

use crate::{
    BootEnvironment, PackageChange, Plan, Request,
    apply::{apply_live, resume_live},
    boot::{
        activate_btrfs_boot_environment, entry_id, stage_running_btrfs, with_running_environment,
    },
    fetch_plan,
    image::Image,
    kdl_format::{BtrfsConfig, ImageDefinition, read_image_definition},
    list_boot_environments, plan,
    release::{SyncOptions, sync_profile_for_image, sync_with_options},
};

pub const CONFIG: &str = "/etc/cps/image.kdl";
const ROOT: &str = "/";

pub struct System {
    config: PathBuf,
    definition: ImageDefinition,
}

pub struct Change {
    plan: Plan,
    boot_reason: Option<String>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        !self.plan.has_changes()
    }

    pub fn packages(&self) -> &[PackageChange] {
        self.plan.package_changes()
    }

    pub fn boot_reason(&self) -> Option<&str> {
        self.boot_reason.as_deref()
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
        })
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

    pub fn plan(&self, request: Request, refresh: bool) -> Result<Change> {
        let upgrade = matches!(request, Request::Update);
        ensure!(
            upgrade || matches!(request, Request::Install { .. }),
            "expected an install or upgrade request"
        );
        self.check_running()?;
        if refresh {
            self.refresh(upgrade)?;
        }
        let plan = plan(Path::new(ROOT), request)?;
        let boot_reason = plan.boot_reason(&self.definition.stage_packages);
        Ok(Change { plan, boot_reason })
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
