use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, FileType, RenameFlags, fstatfs, renameat_with, statat, syncfs},
    io::Errno,
    ioctl::{Opcode, Updater, ioctl, opcode},
};

use crate::image::{Image, root::Root};

use super::{check_image_integrity, entry_id};

const READ_ONLY: u64 = 1 << 1;

// Linux UAPI btrfs_ioctl_vol_args_v2. The reserved union is zeroed.
#[repr(C)]
struct SnapshotArgs {
    fd: i64,
    transid: u64,
    flags: u64,
    reserved: [u64; 4],
    name: [u8; 4040],
}

#[repr(C)]
struct VolumeArgs {
    fd: i64,
    name: [u8; 4088],
}

/// Publish a verified read-only snapshot inside a new capture directory.
pub fn capture_boot_environment(source: &Path, output: &Path) -> Result<String> {
    let source = fs::canonicalize(source).context("opening the root to capture")?;
    let root = Root::open(&source)?;
    ensure!(
        fstatfs(root.as_file())?.f_type == libc::BTRFS_SUPER_MAGIC
            && root.as_file().metadata()?.ino() == 256,
        "capture requires the root of a Btrfs subvolume"
    );
    let image = Image::open_query_root(root)?;
    image.require_clean()?;
    let origin = image.root.as_file();
    let settings = image.settings()?;
    let name = output.file_name().context("expected a new capture name")?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = Root::open(&fs::canonicalize(parent)?)?;
    ensure!(
        fstatfs(parent.as_file())?.f_type == libc::BTRFS_SUPER_MAGIC
            && origin.metadata()?.dev() != parent.as_file().metadata()?.dev(),
        "the capture destination must be in a separate Btrfs subvolume"
    );
    match statat(parent.as_file(), name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => {}
        Ok(_) => anyhow::bail!("the capture destination already exists"),
        Err(error) => return Err(error).context("checking the capture destination"),
    }

    let scratch = tempfile::Builder::new()
        .prefix(".cps-capture-")
        .tempdir_in(Root::fd_path(parent.as_file()))?;
    let directory = Root::open(scratch.path())?;
    let mut args = SnapshotArgs {
        fd: i64::from(origin.as_raw_fd()),
        transid: 0,
        flags: READ_ONLY,
        reserved: [0; 4],
        name: [0; 4040],
    };
    args.name[..4].copy_from_slice(b"root");
    const SNAPSHOT: Opcode = opcode::write::<SnapshotArgs>(0x94, 23);
    // SAFETY: the argument has the exact Btrfs UAPI layout and a NUL-terminated name.
    unsafe { ioctl(directory.as_file(), Updater::<SNAPSHOT, _>::new(&mut args)) }
        .context("creating a read-only snapshot on the same Btrfs filesystem")?;

    let mut published = false;
    let result = (|| {
        let captured = Image::open_query(&scratch.path().join("root"))?;
        let mut flags = 0u64;
        const FLAGS: Opcode = opcode::read::<u64>(0x94, 25);
        // SAFETY: BTRFS_IOC_SUBVOL_GETFLAGS writes one u64 to the provided buffer.
        unsafe {
            ioctl(
                captured.root.as_file(),
                Updater::<FLAGS, _>::new(&mut flags),
            )
        }?;
        ensure!(flags & READ_ONLY != 0, "the captured subvolume is writable");
        let captured_settings = captured.settings()?;
        ensure!(
            captured_settings.id == settings.id && captured_settings.revision == settings.revision,
            "captured image identity or revision changed"
        );
        captured.root.walk(None, |path, _, info| {
            ensure!(
                FileType::from_raw_mode(info.st_mode) != FileType::Directory || info.st_ino != 2,
                "capture omits nested Btrfs subvolume {path:?}"
            );
            Ok(true)
        })?;
        check_image_integrity(&captured).context("verifying the captured root")?;
        syncfs(captured.root.as_file())?;
        renameat_with(
            parent.as_file(),
            scratch
                .path()
                .file_name()
                .context("missing capture directory name")?,
            parent.as_file(),
            name,
            RenameFlags::NOREPLACE,
        )
        .context("publishing the captured root")?;
        published = true;
        parent.as_file().sync_all()?;
        Ok(entry_id(&settings.id, settings.revision))
    })();
    if let Err(error) = &result
        && !published
    {
        let mut args = VolumeArgs {
            fd: 0,
            name: [0; 4088],
        };
        args.name[..4].copy_from_slice(b"root");
        const DESTROY: Opcode = opcode::write::<VolumeArgs>(0x94, 15);
        // SAFETY: BTRFS_IOC_SNAP_DESTROY reads the initialized UAPI buffer.
        unsafe { ioctl(directory.as_file(), Updater::<DESTROY, _>::new(&mut args)) }
            .with_context(|| format!("removing the failed capture after {error:#}"))?;
    }
    result
}
