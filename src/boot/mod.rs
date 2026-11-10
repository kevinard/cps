mod btrfs;
mod btrfs_boot;
mod device;
mod disk;
mod esp;
pub(crate) mod etc_merge;
pub(crate) mod kernel;
mod offline;
mod systemd_boot;
mod tree;
mod uki;

use std::{
    env::{self, VarError},
    ffi::OsStr,
    os::unix::fs::MetadataExt,
    path::Path,
    process::Command,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::fstatfs;

use crate::{
    apply::apply,
    image::{BootParent, Image, root::Root},
    plan::{Plan, Request},
    verify::verify_image,
};

pub(crate) use btrfs::{
    backup_running_btrfs, excluded_subvolumes, make_writable, stage_running_btrfs,
};
pub use btrfs::{capture_boot_environment, stage_btrfs_boot_environment};
pub use btrfs_boot::{
    activate_btrfs_boot_environment, preview_btrfs_rollback, rollback_btrfs_boot_environment,
};
pub(crate) use btrfs_boot::{
    bless_uefi, pending_trial, with_registered_images, with_running_environment,
};
pub use etc_merge::{
    EtcConflict, EtcMergeReport, last_report as last_etc_merge,
    saved_versions as saved_etc_versions,
};
pub use offline::{BootImage, create_boot_image};
pub use systemd_boot::{
    Activation, BootEnvironment, BootPruneReport, BootRemoval, TrialDiscard,
    destroy_btrfs_boot_environment, list_boot_environments,
};
pub(crate) use systemd_boot::{discard_btrfs_trial, prune_btrfs_boot_environments};

fn check_staging_plan(base: &Image, plan: &Plan) -> Result<()> {
    ensure!(
        !matches!(plan.request, Request::ResolveConfig { .. }),
        "apply configuration review directly to an inactive root"
    );
    let settings = base.settings()?;
    ensure!(
        settings.id == plan.image && settings.revision == plan.base_revision,
        "plan does not match the base image and revision"
    );
    plan.check_publisher_generation(&settings)
}

fn finish_staged_root(staged: &Path, previous: BootParent, plan: &Plan) -> Result<String> {
    // The new root's /etc before the plan is the base of the merge at shutdown.
    etc_merge::record_base(&Root::open(staged)?)?;
    apply(staged, plan).context("applying the plan in the staged root")?;
    let image = Image::open(staged, true)?;
    check_image_integrity(&image)?;
    let state = image.state()?;
    if let Some(kernel) = kernel::selected(&state.installed, &state.paths)? {
        kernel.open_prepared(&image.state_dir)?;
    }
    let settings = image.fork_for_boot(previous)?;
    Ok(entry_id(&settings.id, settings.revision))
}

/// Format a byte count for space diagnostics.
pub(crate) fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

pub(crate) fn entry_id(image: &str, revision: u64) -> String {
    format!("cps-{image}-{revision}")
}

fn source_date_epoch() -> Result<Option<u32>> {
    match env::var("SOURCE_DATE_EPOCH") {
        Ok(value) => {
            let message = "SOURCE_DATE_EPOCH must be a decimal timestamp from 0 to 4294967295";
            ensure!(value.bytes().all(|byte| byte.is_ascii_digit()), "{message}");
            Ok(Some(value.parse().context(message)?))
        }
        Err(VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).context("reading SOURCE_DATE_EPOCH"),
    }
}

pub(crate) fn check_package_write_root(root: &Root) -> Result<()> {
    root.offline_path().context(
        "use the system commands or boot-env stage-btrfs to prepare package changes from an \
             installed root",
    )?;
    ensure!(
        !is_registered_btrfs(root)?,
        "use boot-env stage-btrfs to prepare package changes from a registered Btrfs root"
    );
    Ok(())
}

pub(crate) fn is_registered_btrfs(root: &Root) -> Result<bool> {
    Ok(root.as_file().metadata()?.ino() == 256
        && fstatfs(root.as_file())?.f_type == libc::BTRFS_SUPER_MAGIC
        && btrfs_boot::registered(root)?)
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

/// Clone an inactive image directory and apply a saved plan, as staging does
/// in a Btrfs snapshot. Unit tests use it to exercise staged transactions
/// without a Btrfs filesystem.
#[cfg(test)]
pub(crate) fn stage_directory_copy(
    source: &Path,
    destination: &Path,
    plan: &Plan,
) -> Result<String> {
    use std::process::Stdio;

    let base = Image::open_query(source)?;
    base.require_clean()?;
    check_staging_plan(&base, plan)?;
    let settings = base.settings()?;
    let previous = BootParent {
        id: settings.id,
        revision: settings.revision,
        source_revision: None,
    };
    std::fs::create_dir(destination)?;
    let status = Command::new("cp")
        .stdin(Stdio::from(base.root.as_file().try_clone()?))
        .args(["-a", "--reflink=auto", "--one-file-system", "--"])
        .args(["/proc/self/fd/0/.", "."])
        .current_dir(destination)
        .status()
        .context("copying the base image with GNU cp")?;
    ensure!(
        status.success(),
        "copying the base image failed with {status}"
    );
    drop(base);
    finish_staged_root(destination, previous, plan)
}
