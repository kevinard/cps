mod btrfs;
mod btrfs_boot;
mod deploy;
mod device;
mod esp;
mod export;
pub(crate) mod kernel;
mod systemd_boot;
mod tree;

use std::{
    env::{self, VarError},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
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

pub(crate) use btrfs::stage_running_btrfs;
pub use btrfs::{capture_boot_environment, stage_btrfs_boot_environment};
pub use btrfs_boot::{
    activate_btrfs_boot_environment, preview_btrfs_rollback, rollback_btrfs_boot_environment,
};
pub(crate) use btrfs_boot::{with_registered_images, with_running_environment};
pub use deploy::deploy_boot_environment;
pub use esp::{
    activate_boot_environment_on_disk, deploy_and_activate_boot_environment,
    list_boot_environments_on_disk, rollback_boot_environment_on_disk,
};
pub use export::{export_boot_environment, export_boot_environment_receipt};
pub use systemd_boot::{
    Activation, BootEnvironment, RootDeployment, activate_boot_environment,
    activate_deployed_boot_environment, activate_device_boot_environment,
    destroy_btrfs_boot_environment, list_boot_environments, rollback_boot_environment,
};

pub fn stage_boot_environment(source: &Path, destination: &Path, plan: &Plan) -> Result<String> {
    let inputs = staging_inputs(source, destination, plan)?;
    let staged = tempfile::Builder::new()
        .prefix(".cps-be-")
        .tempdir_in(Root::fd_path(inputs.parent.as_file()))?;
    let id = stage_root(inputs.base, inputs.previous, staged.path(), plan)?;
    publish_directory(&inputs.parent, staged.path(), inputs.name)
        .context("publishing the staged root")?;
    Ok(id)
}

/// Stage and export a saved plan as one directory ready for explicit activation.
pub fn prepare_boot_environment(
    source: &Path,
    output: &Path,
    plan: &Plan,
    size_mib: u64,
) -> Result<String> {
    let spec = export::ExportSpec::new(size_mib)?;
    let inputs = staging_inputs(source, output, plan)?;
    let bundle = tempfile::Builder::new()
        .prefix(".cps-prepare-")
        .tempdir_in(Root::fd_path(inputs.parent.as_file()))?;
    let root = bundle.path().join("root");
    fs::create_dir(&root)?;
    let id = stage_root(inputs.base, inputs.previous, &root, plan)?;
    let receipt = export::export_with_receipt(&root, &bundle.path().join("root.ext4"), spec)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(bundle.path().join("receipt.kdl"))?;
    file.write_all(receipt.as_bytes())?;
    file.sync_all()?;
    publish_directory(&inputs.parent, bundle.path(), inputs.name)
        .context("publishing the prepared boot environment")?;
    Ok(id)
}

struct StagingInputs<'a> {
    base: Image,
    previous: BootParent,
    parent: Root,
    name: &'a OsStr,
}

fn staging_inputs<'a>(
    source: &Path,
    destination: &'a Path,
    plan: &Plan,
) -> Result<StagingInputs<'a>> {
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
    check_staging_plan(&base, plan)?;
    let settings = base.settings()?;
    let previous = BootParent {
        id: settings.id,
        revision: settings.revision,
        source_revision: None,
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
    Ok(StagingInputs {
        base,
        previous,
        parent,
        name,
    })
}

fn stage_root(base: Image, previous: BootParent, staged: &Path, plan: &Plan) -> Result<String> {
    let status = artifact_helper("cp", staged)
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

    finish_staged_root(staged, previous, plan)
}

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
    apply(staged, plan).context("applying the plan in the staged root")?;
    let image = Image::open(staged, true)?;
    check_image_integrity(&image)?;
    let state = image.state()?;
    if let Some(kernel) = kernel::selected(&state.installed, &state.paths)? {
        kernel.open_prepared(&image.root)?;
    }
    let settings = image.fork_for_boot(previous)?;
    Ok(entry_id(&settings.id, settings.revision))
}

fn publish_directory(parent: &Root, staged: &Path, name: &OsStr) -> Result<()> {
    let stage_name = staged
        .file_name()
        .context("missing staging directory name")?;
    syncfs(&File::open(staged)?)?;
    renameat_with(
        parent.as_file(),
        stage_name,
        parent.as_file(),
        name,
        RenameFlags::NOREPLACE,
    )?;
    parent.as_file().sync_all()?;
    Ok(())
}

pub(crate) fn entry_id(image: &str, revision: u64) -> String {
    format!("cps-{image}-{revision}")
}

fn root_label(revision: u64) -> String {
    // A full revision fits in the 16 bytes available for an ext4 label.
    format!("{revision:016x}")
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
    root.offline_path()
        .context("use boot-env stage to prepare package changes from an installed root")?;
    let directory = root.as_file();
    let filesystem = fstatfs(directory)?.f_type;
    if directory.metadata()?.ino() == 256 && filesystem == libc::BTRFS_SUPER_MAGIC {
        ensure!(
            !btrfs_boot::registered(root)?,
            "use boot-env stage-btrfs to prepare package changes from a registered Btrfs root"
        );
        return Ok(());
    }
    // Inode 2 is the ext4 filesystem root, including through a bind mount.
    if directory.metadata()?.ino() != 2 || filesystem != libc::EXT4_SUPER_MAGIC {
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
