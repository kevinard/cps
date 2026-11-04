mod deploy;
mod device;
mod esp;
mod export;
pub(crate) mod kernel;
mod systemd_boot;

use std::{
    ffi::OsStr,
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{AtFlags, FileType, RenameFlags, fstatfs, renameat_with, statat, syncfs},
    io::Errno,
    ioctl::{Opcode, Updater, ioctl, opcode},
};

use crate::{
    apply::apply,
    image::{BootParent, Image, root::Root},
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
    let parent = Root::open(&parent)?;
    match statat(parent.as_file(), name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => bail!("the destination already exists"),
        Err(Errno::NOENT) => {}
        Err(error) => return Err(error).context("checking the staging destination"),
    }

    let base = Image::open_query(&source)?;
    base.root.offline_path()?;
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
    let destination = parent.as_file().metadata()?;
    let destination = (destination.dev(), destination.ino());
    let origin = base.root.as_file().metadata()?;
    ensure!(
        destination != (origin.dev(), origin.ino()),
        "the new environment must be outside the base image"
    );
    base.root.walk(None, |path, _, info| {
        ensure!(
            destination != (info.st_dev, info.st_ino),
            "the new environment must be outside the base image"
        );
        ensure!(
            matches!(
                FileType::from_raw_mode(info.st_mode),
                FileType::Directory | FileType::RegularFile | FileType::Symlink
            ),
            "cannot stage special file {path:?}"
        );
        Ok(true)
    })?;
    let staged = tempfile::Builder::new()
        .prefix(".cps-be-")
        .tempdir_in(Root::fd_path(parent.as_file()))?;
    let status = artifact_helper("cp", staged.path())
        .stdin(Stdio::from(base.root.as_file().try_clone()?))
        .args(["-a", "--reflink=auto", "--one-file-system", "--"])
        .args(["/proc/self/fd/0/.", "."])
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

    let stage_name = staged
        .path()
        .file_name()
        .context("missing staging directory name")?;
    syncfs(&File::open(staged.path())?)?;
    renameat_with(
        parent.as_file(),
        stage_name,
        parent.as_file(),
        name,
        RenameFlags::NOREPLACE,
    )
    .context("publishing the staged root")?;
    parent.as_file().sync_all()?;
    Ok(entry_id(&settings.id, settings.revision))
}

fn entry_id(image: &str, revision: u64) -> String {
    format!("cps-{image}-{revision}")
}

fn root_label(revision: u64) -> String {
    // A full revision fits in the 16 bytes available for an ext4 label.
    format!("{revision:016x}")
}

pub(crate) fn check_package_write_root(root: &Root) -> Result<()> {
    root.offline_path()
        .context("use boot-env stage to prepare package changes from an installed root")?;
    let directory = root.as_file();
    // Inode 2 is the ext4 filesystem root, including through a bind mount.
    if directory.metadata()?.ino() != 2 || fstatfs(directory)?.f_type != libc::EXT4_SUPER_MAGIC {
        return Ok(());
    }
    const GETFSLABEL: Opcode = opcode::read::<[u8; 256]>(0x94, 49);
    let mut label = [0u8; 256];
    // SAFETY: FS_IOC_GETFSLABEL writes at most FSLABEL_MAX bytes to this buffer.
    unsafe { ioctl(directory, Updater::<GETFSLABEL, _>::new(&mut label)) }
        .context("checking the mounted ext4 root label before applying package changes")?;
    let deployed = label[16] == 0
        && label[..16]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
    ensure!(
        !deployed,
        "use boot-env stage to prepare package changes from a deployed ext4 root"
    );
    Ok(())
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
