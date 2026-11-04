mod deploy;
mod device;
mod esp;
mod export;
pub(crate) mod kernel;
mod systemd_boot;

use std::{
    ffi::OsStr,
    fs::{self, File},
    path::Path,
    process::Command,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{RenameFlags, renameat_with, syncfs};

use crate::{
    apply::apply,
    image::{BootParent, Image},
    plan::{Plan, Request},
    verify::verify_image,
};

pub use deploy::deploy_boot_environment;
pub use esp::{
    activate_boot_environment_on_disk, deploy_and_activate_boot_environment,
    list_boot_environments_on_disk, rollback_boot_environment_on_disk,
};
pub use export::{export_boot_environment, export_boot_environment_receipt};
pub use systemd_boot::{
    Activation, BootEnvironment, RootDeployment, activate_boot_environment,
    activate_deployed_boot_environment, activate_device_boot_environment, list_boot_environments,
    rollback_boot_environment,
};

pub fn stage_boot_environment(source: &Path, destination: &Path, plan: &Plan) -> Result<String> {
    ensure!(
        !matches!(plan.request, Request::ResolveConfig { .. }),
        "apply configuration review directly to an inactive root"
    );
    let source = fs::canonicalize(source).context("opening the base image directory")?;
    ensure!(
        source != Path::new("/"),
        "staging requires an inactive base image"
    );
    let name = destination
        .file_name()
        .context("expected a new environment directory name")?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).context("opening the destination parent")?;
    ensure!(
        !parent.starts_with(&source),
        "the new environment must be outside the base image"
    );
    ensure!(
        !parent.join(name).try_exists()?,
        "the destination already exists"
    );

    let base = Image::open(&source, false)?;
    base.require_clean()?;
    let settings = base.settings()?;
    ensure!(
        settings.id == plan.image && settings.revision == plan.base_revision,
        "plan does not match the base image and revision"
    );
    plan.check_publisher_generation(&settings)?;
    let previous = BootParent {
        id: settings.id,
        revision: settings.revision,
    };
    let staged = tempfile::Builder::new()
        .prefix(".cps-be-")
        .tempdir_in(&parent)?;
    let status = Command::new("cp")
        .args(["-a", "--reflink=auto", "--one-file-system", "--"])
        .arg(source.join("."))
        .arg(staged.path())
        .status()
        .context("copying the base image with GNU cp")?;
    ensure!(
        status.success(),
        "copying the base image failed with {status}"
    );
    drop(base);

    apply(staged.path(), plan).context("applying the plan in the staged root")?;
    let image = Image::open(staged.path(), true)?;
    check_image_integrity(&image)?;
    let state = image.state()?;
    if let Some(kernel) = kernel::selected(&state.installed, &state.paths)? {
        kernel.open_prepared(&image.root)?;
    }
    let settings = image.fork_for_boot(previous)?;
    drop(image);

    let parent = File::open(parent)?;
    let stage_name = staged
        .path()
        .file_name()
        .context("missing staging directory name")?;
    syncfs(&File::open(staged.path())?)?;
    renameat_with(&parent, stage_name, &parent, name, RenameFlags::NOREPLACE)
        .context("publishing the staged root")?;
    parent.sync_all()?;
    Ok(entry_id(&settings.id, settings.revision))
}

fn entry_id(image: &str, revision: u64) -> String {
    format!("cps-{image}-{revision}")
}

fn root_label(revision: u64) -> String {
    // A full revision fits in the 16 bytes available for an ext4 label.
    format!("{revision:016x}")
}

fn check_image_integrity(image: &Image) -> Result<()> {
    for finding in verify_image(image)? {
        ensure!(
            !finding.is_error,
            "image verification failed: {}",
            finding.message
        );
    }
    Ok(())
}

fn artifact_helper(program: impl AsRef<OsStr>, directory: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .current_dir(directory);
    command
}
