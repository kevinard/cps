use std::{
    collections::BTreeSet,
    fs::{self, File},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, RenameFlags, Stat, Timespec, Timestamps, fchmod, fchown,
        fstatfs, fstatvfs, futimens, mkdirat, openat, renameat_with, statat, syncfs, unlinkat,
    },
    io::Errno,
    ioctl::{Opcode, Updater, ioctl, opcode},
    process::{Gid, Uid},
};

use crate::{
    image::{Image, STATE_DIRECTORY, root::Root},
    model::{Node, validate_path},
    plan::{Plan, Write, payloads},
};

use super::{
    check_image_integrity, check_staging_plan, entry_id, finish_staged_root, kernel::Artifacts,
};

const READ_ONLY: u64 = 1 << 1;
/// Space kept free beyond the plan's payloads and kernel artifacts for metadata.
const STAGING_RESERVE: u64 = 32 * 1024 * 1024;
/// Nested subvolumes that systemd creates on Btrfs. Staged roots get an empty directory.
pub const DEFAULT_EXCLUDED_SUBVOLUMES: [&str; 2] = ["var/lib/machines", "var/lib/portables"];

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

const _: () = assert!(size_of::<SnapshotArgs>() == 4096);
const _: () = assert!(size_of::<VolumeArgs>() == 4096);

/// Publish a verified read-only snapshot inside a new capture directory.
///
/// Nested subvolumes at the default paths and at `excluded` become empty directories.
pub fn capture_boot_environment(
    source: &Path,
    output: &Path,
    excluded: &[String],
) -> Result<String> {
    snapshot_environment(source, output, None, false, excluded)
}

/// Stage a saved plan in a new writable Btrfs snapshot of an inactive baseline.
pub fn stage_btrfs_boot_environment(
    source: &Path,
    output: &Path,
    plan: &Plan,
    excluded: &[String],
) -> Result<String> {
    snapshot_environment(source, output, Some(plan), false, excluded)
}

pub(crate) fn stage_running_btrfs(
    output: &Path,
    plan: &Plan,
    excluded: &[String],
) -> Result<String> {
    snapshot_environment(Path::new("/"), output, Some(plan), true, excluded)
}

/// Normalize configured subvolume exclusions and add the defaults.
pub(crate) fn excluded_subvolumes(extra: &[String]) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    for path in DEFAULT_EXCLUDED_SUBVOLUMES
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
    {
        let path = path.trim_start_matches('/').trim_end_matches('/');
        validate_path(path).with_context(|| format!("invalid excluded subvolume {path:?}"))?;
        let state = format!("{STATE_DIRECTORY}/");
        ensure!(
            !state.starts_with(&format!("{path}/")),
            "excluded subvolume {path} contains the CPS image state"
        );
        paths.insert(path.to_owned());
    }
    Ok(paths.into_iter().collect())
}

fn check_staging_space(parent: &Root, source: &Path, plan: &Plan) -> Result<()> {
    let payload = payloads(&plan.writes)
        .values()
        .map(|node| match node {
            Node::File { size, .. } => *size,
            _ => 0,
        })
        .fold(0u64, u64::saturating_add);
    let kernel = kernel_preparation(
        plan.kernel_install.as_ref(),
        &plan.writes,
        |path| file_size(&source.join(path)),
        || largest_initrd(source),
    )?;
    // Btrfs statfs reports the space new data can use under the data profile,
    // including unallocated device space.
    let stat = fstatvfs(parent.as_file()).context("reading Btrfs free space")?;
    let available = stat.f_bavail.saturating_mul(stat.f_frsize);
    let needed = payload
        .saturating_add(kernel)
        .saturating_add(STAGING_RESERVE);
    ensure!(
        available >= needed,
        "insufficient free space on the Btrfs filesystem to stage this transaction: {} available, \
         {} needed ({} of payloads, {} of kernel artifacts and a {} reserve). Remove old boot \
         environments with `cps boot-env prune`, or free space on the filesystem, then retry",
        super::mib(available),
        super::mib(needed),
        super::mib(payload),
        super::mib(kernel),
        super::mib(STAGING_RESERVE),
    );
    Ok(())
}

/// Bytes that kernel preparation writes beyond the plan's payloads.
///
/// Preparation copies the kernel and its initramfs into a private
/// `kernel-<digest>` directory of the image state, so each costs its size
/// once more, whether the plan writes it or the base already holds it. A
/// generated initramfs is not a payload: its trigger writes it into the root
/// and preparation copies it, so it counts twice. Its size is known only
/// after the trigger runs, so `generated` estimates it from the base.
fn kernel_preparation(
    artifacts: Option<&Artifacts>,
    writes: &[Write],
    base_size: impl Fn(&str) -> Result<Option<u64>>,
    generated: impl FnOnce() -> Result<Option<u64>>,
) -> Result<u64> {
    let Some(artifacts) = artifacts else {
        return Ok(0);
    };
    let size = |path: &str| -> Result<u64> {
        let written = writes.iter().find_map(|write| match &write.after {
            Some(Node::File { size, .. }) if write.path == path => Some(*size),
            _ => None,
        });
        Ok(match written {
            Some(size) => size,
            None => base_size(path)?.unwrap_or(0),
        })
    };
    let kernel = &artifacts.kernel;
    let mut bytes = size(&kernel.image())?;
    if let Some(initrd) = kernel.initrd() {
        bytes = bytes.saturating_add(size(&initrd)?);
    } else if artifacts.generated_initrd {
        let estimate = match base_size(&kernel.initrd_path())? {
            Some(size) => size,
            None => generated()?.unwrap_or(0),
        };
        bytes = bytes.saturating_add(estimate.saturating_mul(2));
    }
    Ok(bytes)
}

fn file_size(path: &Path) -> Result<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The largest initramfs of any kernel release in the base root.
fn largest_initrd(root: &Path) -> Result<Option<u64>> {
    let modules = root.join("usr/lib/modules");
    let children = match fs::read_dir(&modules) {
        Ok(children) => children,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut largest = None;
    for child in children {
        let child = child?;
        if child.file_type()?.is_dir()
            && let Some(size) = file_size(&child.path().join("initrd"))?
        {
            largest = largest.max(Some(size));
        }
    }
    Ok(largest)
}

fn snapshot_environment(
    source: &Path,
    output: &Path,
    plan: Option<&Plan>,
    from_running: bool,
    excluded: &[String],
) -> Result<String> {
    let excluded = excluded_subvolumes(excluded)?;
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
    if let Some(plan) = plan {
        if !from_running {
            image.root.offline_path()?;
            ensure!(
                read_only(&image.root)?,
                "Btrfs staging requires a read-only baseline"
            );
        }
        check_staging_plan(&image, plan)?;
    }
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
    if let Some(plan) = plan {
        check_staging_space(&parent, &source, plan)?;
    }

    let scratch = tempfile::Builder::new()
        .prefix(".cps-capture-")
        .tempdir_in(Root::fd_path(parent.as_file()))?;
    let directory = Root::open(scratch.path())?;
    // A capture becomes read-only after its excluded subvolume stubs are replaced.
    let mut args = SnapshotArgs {
        fd: i64::from(origin.as_raw_fd()),
        transid: 0,
        flags: 0,
        reserved: [0; 4],
        name: [0; 4040],
    };
    args.name[..4].copy_from_slice(b"root");
    const SNAPSHOT: Opcode = opcode::write::<SnapshotArgs>(0x94, 23);
    // SAFETY: the argument has the exact Btrfs UAPI layout and a NUL-terminated name.
    unsafe { ioctl(directory.as_file(), Updater::<SNAPSHOT, _>::new(&mut args)) }
        .context("creating a snapshot on the same Btrfs filesystem")?;

    let mut published = false;
    let result = (|| {
        {
            let snapshot = Root::open(&scratch.path().join("root"))?;
            let path = scratch.path().join("root");
            replace_excluded_stubs(&image.root, &snapshot, &path, &excluded)?;
            if plan.is_none() {
                set_read_only(&snapshot)?;
            }
        }
        let captured = Image::open_query(&scratch.path().join("root"))?;
        ensure!(
            read_only(&captured.root)? == plan.is_none(),
            "the snapshot read-only flag differs from the requested mode"
        );
        let captured_settings = captured.settings()?;
        ensure!(
            captured_settings.id == settings.id && captured_settings.revision == settings.revision,
            "captured image identity or revision changed"
        );
        check_snapshot_tree(&captured.root)?;
        if from_running && plan.is_some_and(Plan::is_recovery) {
            captured.verify_catalog_archives(&captured.state()?)?;
        } else {
            check_image_integrity(&captured).context("verifying the captured root")?;
        }
        drop(captured);
        let id = if let Some(plan) = plan {
            super::btrfs_boot::clear_registration(&Root::open(&scratch.path().join("root"))?)?;
            finish_staged_root(
                &scratch.path().join("root"),
                settings.as_boot_parent(),
                plan,
            )?
        } else {
            entry_id(&settings.id, settings.revision)
        };
        check_snapshot_tree(&Root::open(&scratch.path().join("root"))?)?;
        syncfs(directory.as_file())?;
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
        Ok(id)
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

fn subvolume_flags(root: &Root) -> Result<u64> {
    let mut flags = 0u64;
    const FLAGS: Opcode = opcode::read::<u64>(0x94, 25);
    // SAFETY: BTRFS_IOC_SUBVOL_GETFLAGS writes one u64 to the provided buffer.
    unsafe { ioctl(root.as_file(), Updater::<FLAGS, _>::new(&mut flags)) }?;
    Ok(flags)
}

fn read_only(root: &Root) -> Result<bool> {
    Ok(subvolume_flags(root)? & READ_ONLY != 0)
}

fn set_read_only(root: &Root) -> Result<()> {
    root.sync()?;
    let mut flags = subvolume_flags(root)? | READ_ONLY;
    const SET_FLAGS: Opcode = opcode::write::<u64>(0x94, 26);
    // SAFETY: BTRFS_IOC_SUBVOL_SETFLAGS reads one u64 from the provided buffer.
    unsafe { ioctl(root.as_file(), Updater::<SET_FLAGS, _>::new(&mut flags)) }
        .context("making the capture read-only")?;
    Ok(())
}

/// Open the parent directory of a relative path, or `None` if it is absent.
fn parent_dir<'a>(root: &Root, path: &'a str) -> Result<Option<(File, &'a str)>> {
    let (parent, leaf) = path.rsplit_once('/').unwrap_or(("", path));
    let directory = if parent.is_empty() {
        root.as_file().try_clone()?
    } else {
        match root.dir(parent) {
            Ok(directory) => directory,
            Err(error) if matches!(error.downcast_ref::<Errno>(), Some(&Errno::NOENT)) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
    };
    Ok(Some((directory, leaf)))
}

fn stat_entry(root: &Root, path: &str) -> Result<Option<Stat>> {
    let Some((parent, leaf)) = parent_dir(root, path)? else {
        return Ok(None);
    };
    match statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {path}")),
    }
}

fn is_stub(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::Directory && stat.st_ino == 2
}

/// Replace the empty stubs a snapshot leaves for excluded nested subvolumes.
///
/// Each stub becomes an empty directory with the mode, owner and timestamps
/// of the source subvolume's root directory. Its contents stay in the source.
fn replace_excluded_stubs(
    source: &Root,
    snapshot: &Root,
    snapshot_path: &Path,
    excluded: &[String],
) -> Result<()> {
    let mut managed: Option<BTreeSet<String>> = None;
    for path in excluded {
        if !stat_entry(snapshot, path)?.is_some_and(|stat| is_stub(&stat)) {
            continue;
        }
        if managed.is_none() {
            managed = Some(
                Image::open_query(snapshot_path)?
                    .state()?
                    .paths
                    .into_keys()
                    .collect(),
            );
        }
        let prefix = format!("{path}/");
        if let Some(inside) = managed
            .iter()
            .flatten()
            .find(|managed| managed.starts_with(&prefix))
        {
            anyhow::bail!(
                "nested Btrfs subvolume {path} contains the CPS-managed path {inside}, which a \
                 snapshot omits. Move that subvolume out of the root, or remove {path} from the \
                 subvolume exclusions"
            );
        }
        let original = stat_entry(source, path)?
            .filter(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Directory);
        let (parent, leaf) = parent_dir(snapshot, path)?.context("excluded subvolume moved")?;
        unlinkat(&parent, leaf, AtFlags::REMOVEDIR)
            .with_context(|| format!("removing the snapshot stub for {path}"))?;
        let mode = original.map_or(0o755, |stat| stat.st_mode & 0o7777);
        mkdirat(&parent, leaf, Mode::from_raw_mode(0o700))?;
        let directory = File::from(openat(
            &parent,
            leaf,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if let Some(stat) = &original {
            fchown(
                &directory,
                Some(Uid::from_raw(stat.st_uid)),
                Some(Gid::from_raw(stat.st_gid)),
            )?;
        }
        fchmod(&directory, Mode::from_raw_mode(mode))?;
        if let Some(stat) = &original {
            let time = |sec: i64, nsec: i64| Timespec {
                tv_sec: sec,
                tv_nsec: nsec as _,
            };
            futimens(
                &directory,
                &Timestamps {
                    last_access: time(stat.st_atime, stat.st_atime_nsec as i64),
                    last_modification: time(stat.st_mtime, stat.st_mtime_nsec as i64),
                },
            )?;
        }
        directory.sync_all()?;
        parent.sync_all()?;
    }
    Ok(())
}

pub(super) fn destroy_by_id(volume: &Root, subvolid: u64) -> Result<()> {
    ensure!(subvolid >= 256, "cannot destroy the Btrfs top-level volume");
    let mut args = SnapshotArgs {
        fd: 0,
        transid: 0,
        flags: 1 << 4, // BTRFS_SUBVOL_SPEC_BY_ID
        reserved: [0; 4],
        name: [0; 4040],
    };
    args.name[..8].copy_from_slice(&subvolid.to_ne_bytes());
    const DESTROY: Opcode = opcode::write::<SnapshotArgs>(0x94, 63);
    // SAFETY: the UAPI union holds subvolid when SPEC_BY_ID is set.
    unsafe { ioctl(volume.as_file(), Updater::<DESTROY, _>::new(&mut args)) }
        .context("deleting the registered Btrfs subvolume by ID")?;
    volume.sync().context("committing Btrfs subvolume deletion")
}

fn check_snapshot_tree(root: &Root) -> Result<()> {
    root.walk(None, |path, _, info| {
        ensure!(
            !is_stub(info),
            "capture omits nested Btrfs subvolume {path:?}. Add `exclude-subvolume \"{path}\"` \
             to boot-btrfs, or pass --exclude-subvolume {path}, to start new roots with an empty \
             directory there; otherwise move the subvolume out of the root"
        );
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::{excluded_subvolumes, kernel_preparation};
    use crate::{
        boot::kernel::{Artifacts, Kernel},
        model::Node,
        plan::Write,
    };

    fn artifacts(initrd: bool, generated_initrd: bool) -> Artifacts {
        Artifacts {
            kernel: Kernel {
                release: "6.12.0".into(),
                initrd,
            },
            linux_sha256: "a".repeat(64),
            initrd_sha256: initrd.then(|| "b".repeat(64)),
            generated_initrd,
        }
    }

    fn write(path: &str, size: u64) -> Write {
        Write {
            path: path.into(),
            before: None,
            after: Some(Node::File {
                sha256: "c".repeat(64),
                size,
                mode: 0o644,
                uid: 0,
                gid: 0,
                capabilities: None,
            }),
            detach: false,
            local_config: false,
            config_move: None,
        }
    }

    #[test]
    fn staging_space_counts_kernel_preparation() -> anyhow::Result<()> {
        let base = |path: &str| {
            Ok(match path {
                "usr/lib/modules/6.12.0/vmlinuz" => Some(10),
                "usr/lib/modules/6.12.0/initrd" => Some(30),
                _ => None,
            })
        };
        let none = |_: &str| Ok(None);
        let unused = || -> anyhow::Result<Option<u64>> { panic!("no estimate needed") };
        assert_eq!(kernel_preparation(None, &[], base, unused)?, 0);
        // Written payloads give the sizes of the prepared copies.
        let writes = [
            write("usr/lib/modules/6.12.0/vmlinuz", 100),
            write("usr/lib/modules/6.12.0/initrd", 200),
        ];
        assert_eq!(
            kernel_preparation(Some(&artifacts(true, false)), &writes, base, unused)?,
            300
        );
        // A kernel the plan does not rewrite is still copied from the base.
        assert_eq!(
            kernel_preparation(Some(&artifacts(true, false)), &[], base, unused)?,
            40
        );
        // A generated initramfs counts twice: the trigger output and its copy.
        assert_eq!(
            kernel_preparation(Some(&artifacts(false, true)), &writes[..1], base, unused)?,
            100 + 2 * 30
        );
        assert_eq!(
            kernel_preparation(Some(&artifacts(false, true)), &writes[..1], none, || Ok(
                Some(50)
            ))?,
            100 + 2 * 50
        );
        Ok(())
    }

    #[test]
    fn subvolume_exclusions_extend_the_systemd_defaults() -> anyhow::Result<()> {
        assert_eq!(
            excluded_subvolumes(&["/var/lib/docker/".into(), "var/lib/machines".into()])?,
            ["var/lib/docker", "var/lib/machines", "var/lib/portables"]
        );
        for invalid in ["", "/", "var/../etc", "var/lib/cps", "var"] {
            assert!(
                excluded_subvolumes(&[invalid.into()]).is_err(),
                "{invalid} was accepted"
            );
        }
        Ok(())
    }
}
