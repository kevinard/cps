use std::{
    env::{self, VarError},
    fs::{self, OpenOptions},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::Path,
    process::Stdio,
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, FileType, RenameFlags, renameat_with, statat},
    io::Errno,
};

use super::{
    artifact_helper, check_image_integrity, entry_id, kernel, root_label,
    systemd_boot::canonical_partuuid,
};
use crate::{
    image::{Image, root::Root},
    kdl_format::boot_export::ExportReceipt,
    model::digest_reader,
};

const PAYLOAD: &str = "root.ext4";
const FORMATTER_CONFIG: &str = "mke2fs.conf";

/// Export an inactive CPS root to a new, checked ext4 partition payload.
pub fn export_boot_environment(root: &Path, output: &Path, size_mib: u64) -> Result<String> {
    let (id, _) = export(root, output, size_mib, false)?;
    Ok(id)
}

/// Export a root and return its KDL receipt for activation against a GPT disk.
pub fn export_boot_environment_receipt(
    root: &Path,
    output: &Path,
    size_mib: u64,
) -> Result<String> {
    let (_, receipt) = export(root, output, size_mib, true)?;
    receipt.context("missing root export receipt")
}

fn export(
    root: &Path,
    output: &Path,
    size_mib: u64,
    emit_receipt: bool,
) -> Result<(String, Option<String>)> {
    let bytes = size_mib
        .checked_mul(1024 * 1024)
        .filter(|bytes| *bytes <= i64::MAX as u64)
        .context("ext4 image size is out of range")?;
    ensure!(size_mib >= 16, "an ext4 image requires at least 16 MiB");
    let epoch = source_date_epoch()?;

    let root = fs::canonicalize(root).context("opening the image to export")?;
    ensure!(
        root != Path::new("/"),
        "export requires an inactive image root"
    );
    let name = output
        .file_name()
        .context("expected a new image file name")?;
    let parent_path = fs::canonicalize(
        output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
    .context("opening the export destination directory")?;
    ensure!(
        !parent_path.starts_with(&root),
        "the export destination must be outside the image"
    );
    let parent = Root::open(&parent_path)?;
    match statat(parent.as_file(), name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => anyhow::bail!("the export destination already exists"),
        Err(Errno::NOENT) => {}
        Err(error) => return Err(error).context("checking the export destination"),
    }

    // Keep the shared image lock until formatting and publication finish.
    let image = Image::open_query(&root)?;
    image.root.offline_path()?;
    check_image_integrity(&image)?;
    let state = image.state()?;
    if let Some(artifacts) = kernel::selected(&state.installed, &state.paths)? {
        artifacts.open_prepared(&image.root)?;
    }
    let uuid = canonical_partuuid(&state.settings.id).context("invalid image identity")?;
    let source = image.root.as_file().metadata()?;
    let destination = parent.as_file().metadata()?;
    let dest_inode = (destination.dev(), destination.ino());
    ensure!(
        (source.dev(), source.ino()) != dest_inode,
        "the export destination must be outside the image"
    );
    image.root.walk(None, |path, _, info| {
        let kind = FileType::from_raw_mode(info.st_mode);
        ensure!(
            matches!(
                kind,
                FileType::Directory | FileType::RegularFile | FileType::Symlink
            ),
            "cannot export special file {path:?}"
        );
        ensure!(
            (info.st_dev, info.st_ino) != dest_inode,
            "the export destination must be outside the image"
        );
        Ok(true)
    })?;

    let temp = tempfile::Builder::new()
        .prefix(".cps-export-")
        .tempdir_in(Root::fd_path(parent.as_file()))?;
    let stage = Root::open(temp.path())?;
    let stage_path = Root::fd_path(stage.as_file());
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage_path.join(PAYLOAD))?;
    file.set_len(bytes)?;
    let mut options = format!(
        "root_owner={}:{},root_perms={:04o},lazy_itable_init=0,lazy_journal_init=0",
        source.uid(),
        source.gid(),
        source.mode() & 0o7777,
    );
    let mut formatter = artifact_helper("mke2fs", &stage_path);
    if let Some(epoch) = epoch {
        fs::write(
            stage_path.join(FORMATTER_CONFIG),
            include_str!("mke2fs.conf"),
        )?;
        formatter
            .env("MKE2FS_CONFIG", FORMATTER_CONFIG)
            .env("SOURCE_DATE_EPOCH", epoch.to_string())
            .args(["-T", "ext4"]);
        options.push_str(&format!(",hash_seed={uuid}"));
    }
    let status = formatter
        // Pin the source across exec and resolve it as a directory, even if
        // the caller renames the original source path.
        .stdin(Stdio::from(image.root.as_file().try_clone()?))
        .stdout(Stdio::null())
        .args([
            "-q", "-t", "ext4", "-F", "-b", "4096", "-U", &uuid, "-E", &options,
        ])
        .args(["-L", &root_label(state.settings.revision)])
        .args(["-d", "/proc/self/fd/0/.", PAYLOAD])
        .status()
        .context("formatting the partition payload with mke2fs")?;
    ensure!(status.success(), "mke2fs failed with {status}");
    let status = artifact_helper("e2fsck", &stage_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .args(["-f", "-n", PAYLOAD])
        .status()
        .context("checking the exported ext4 filesystem with e2fsck")?;
    ensure!(status.success(), "e2fsck failed with {status}");
    let checked = stage.file(PAYLOAD)?;
    let before = file.metadata()?;
    let after = checked.metadata()?;
    ensure!(
        (before.dev(), before.ino(), bytes) == (after.dev(), after.ino(), after.len()),
        "the checked ext4 image was replaced or resized"
    );
    if let Some(epoch) = epoch {
        // s_mkfs_time starts at offset 0x108 of the ext4 primary superblock.
        let mut created = [0; 4];
        checked.read_exact_at(&mut created, 1024 + 0x108)?;
        ensure!(
            u32::from_le_bytes(created) == epoch,
            "mke2fs did not honor SOURCE_DATE_EPOCH. Use e2fsprogs with reproducible timestamp support"
        );
    }
    checked.sync_all()?;
    let receipt = if emit_receipt {
        Some(
            ExportReceipt {
                image: uuid.clone(),
                revision: state.settings.revision,
                state_sha256: digest_reader(image.root.file(".cps/state.sqlite")?)?,
                bytes,
                sha256: digest_reader(checked)?,
            }
            .render(),
        )
    } else {
        None
    };
    renameat_with(
        stage.as_file(),
        PAYLOAD,
        parent.as_file(),
        name,
        RenameFlags::NOREPLACE,
    )
    .context("publishing the ext4 image")?;
    parent.as_file().sync_all()?;
    Ok((entry_id(&uuid, state.settings.revision), receipt))
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
