use std::{
    fs::{self, File},
    io,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, FlockOperation, StatVfsMountFlags, StatxFlags, fstatvfs, statx},
    mount::{MoveMountFlags, OpenTreeFlags, UnmountFlags, move_mount, open_tree, unmount},
};

use super::{
    BtrfsVolume, Inventory, destroy, inventory, lock_esp,
    policy::{self, BootEntry, MAX_ENTRY_BYTES, NAME_FIELD},
    require_valid,
};
use crate::image::root::Root;

pub(crate) fn validate_name(name: &str) -> Result<()> {
    ensure!(
        (1..=48).contains(&name.len())
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
            && !name.starts_with("cps-"),
        "use 1 to 48 lowercase ASCII letters, digits, hyphens or underscores, starting with \
         a letter or digit and not with cps-"
    );
    Ok(())
}

pub(super) fn find<'a>(
    entries: impl IntoIterator<Item = &'a BootEntry>,
    selector: &str,
) -> Result<&'a BootEntry> {
    let mut matches = entries.into_iter().filter(|entry| matches(entry, selector));
    let found = matches
        .next()
        .with_context(|| format!("unknown boot environment {selector}"))?;
    ensure!(
        matches.next().is_none(),
        "ambiguous boot environment {selector}"
    );
    Ok(found)
}

pub(super) fn matches(entry: &BootEntry, selector: &str) -> bool {
    entry.id == selector || entry.name.as_deref() == Some(selector)
}

pub(super) fn require_unused_name<'a>(
    entries: impl IntoIterator<Item = &'a BootEntry>,
    name: &str,
    except: &str,
) -> Result<()> {
    validate_name(name)?;
    ensure!(
        !entries
            .into_iter()
            .any(|entry| entry.id != except && matches(entry, name)),
        "boot environment name {name} is already in use"
    );
    Ok(())
}

pub(super) fn named_description(description: &str, name: &str) -> String {
    let mut updated = String::new();
    for line in description.split_inclusive('\n') {
        if !line.starts_with(NAME_FIELD) {
            updated.push_str(line);
        }
    }
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("{NAME_FIELD}{name}\n"));
    updated
}

/// Set a user-facing name without changing the environment's boot identity.
pub(crate) fn rename_boot_environment(esp: &Path, selector: &str, name: &str) -> Result<String> {
    validate_name(name)?;
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        entries, invalid, ..
    } = inventory(&esp, &lock, false)?;
    require_valid(&invalid, selector)?;
    let entry = find(&entries, selector)?;
    ensure!(
        entry.tries.is_none(),
        "boot environment {selector} is not known-good"
    );
    require_unused_name(
        entries.iter().chain(&destroy::pending(&esp)?),
        name,
        &entry.id,
    )?;
    if entry.name.as_deref() != Some(name) {
        let updated = named_description(&entry.description, name);
        ensure!(
            updated.len() <= MAX_ENTRY_BYTES,
            "boot entry exceeds 64 KiB"
        );
        policy::replace_file(&entry.path, updated.as_bytes())?;
    }
    Ok(entry.id.clone())
}

/// Attach a nonrecursive, read-only inspection mount of a registered root.
pub(crate) fn mount_boot_environment(
    esp: &Path,
    volume: &Path,
    selector: &str,
    target: &Path,
) -> Result<()> {
    with_root(esp, volume, selector, |root| {
        let MountTarget {
            parent, directory, ..
        } = MountTarget::open(target)?;
        ensure!(
            mount_id(directory.as_file())? == mount_id(parent.as_file())?,
            "the inspection directory is already a mount point"
        );
        require_root_owned(&directory)?;
        ensure!(
            fs::read_dir(Root::fd_path(directory.as_file()))?
                .next()
                .is_none(),
            "the inspection directory is not empty"
        );
        let tree = open_tree(
            root.as_file(),
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::OPEN_TREE_CLOEXEC
                | OpenTreeFlags::AT_EMPTY_PATH,
        )
        .context("cloning the Btrfs root mount")?;
        let attrs = libc::mount_attr {
            attr_set: libc::MOUNT_ATTR_RDONLY
                | libc::MOUNT_ATTR_NOSUID
                | libc::MOUNT_ATTR_NODEV
                | libc::MOUNT_ATTR_NOEXEC,
            attr_clr: 0,
            propagation: libc::MS_PRIVATE,
            userns_fd: 0,
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                tree.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                &attrs as *const libc::mount_attr,
                size_of::<libc::mount_attr>(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error())
                .context("making the inspection mount read-only");
        }
        move_mount(
            &tree,
            "",
            directory.as_file(),
            "",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )
        .with_context(|| format!("attaching the inspection mount at {}", target.display()))
    })
}

/// Detach a matching inspection mount without forcing it or detaching a busy root.
///
/// The mount is matched against the entry's Btrfs record alone, so that a
/// root failing its boot checks can still be unmounted, and then destroyed.
pub(crate) fn unmount_boot_environment(esp: &Path, selector: &str, target: &Path) -> Result<()> {
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        entries, invalid, ..
    } = inventory(&esp, &lock, false)?;
    require_valid(&invalid, selector)?;
    let entry = find(&entries, selector)?;
    let MountTarget {
        parent,
        path,
        directory,
    } = MountTarget::open(target)?;
    let mounted = mount_id(directory.as_file())?;
    ensure!(
        mounted != mount_id(parent.as_file())?
            && entry.btrfs.describes(&directory).unwrap_or(false)
            && fstatvfs(directory.as_file())?.f_flag.contains(
                StatVfsMountFlags::RDONLY
                    | StatVfsMountFlags::NOSUID
                    | StatVfsMountFlags::NODEV
                    | StatVfsMountFlags::NOEXEC
            ),
        "the target is not a read-only inspection mount of {selector}"
    );
    drop(directory);
    unmount(&path, UnmountFlags::NOFOLLOW)
        .with_context(|| format!("unmounting inspection directory {}", target.display()))
}

fn with_root<T>(
    esp: &Path,
    volume: &Path,
    selector: &str,
    action: impl FnOnce(&Root) -> Result<T>,
) -> Result<T> {
    let volume = BtrfsVolume::open(volume)?;
    let esp = fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        entries, invalid, ..
    } = inventory(&esp, &lock, false)?;
    require_valid(&invalid, selector)?;
    let entry = find(&entries, selector)?;
    volume.check_boot_device(&esp, &lock, &entry.root)?;
    let (id, revision) = policy::entry_identity(&entry.id)?;
    let image = volume.open_image(&entry.btrfs, id, revision)?;
    action(&image.root)
}

struct MountTarget {
    parent: Root,
    path: PathBuf,
    directory: Root,
}

impl MountTarget {
    fn open(target: &Path) -> Result<Self> {
        let name = target
            .file_name()
            .context("specify a dedicated inspection directory")?;
        let parent = target
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = Root::open(&fs::canonicalize(parent)?)?;
        require_root_owned(&parent)?;
        let path = Root::fd_path(parent.as_file()).join(name);
        let directory = Root::open(&path)?;
        Ok(Self {
            parent,
            path,
            directory,
        })
    }
}

fn require_root_owned(root: &Root) -> Result<()> {
    let metadata = root.as_file().metadata()?;
    ensure!(
        metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "the inspection directory and its parent must be writable only by root"
    );
    Ok(())
}

fn mount_id(file: &File) -> Result<u64> {
    let stat = statx(file, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    ensure!(
        stat.stx_mask & StatxFlags::MNT_ID.bits() != 0,
        "kernel did not report a mount ID"
    );
    Ok(stat.stx_mnt_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::btrfs_boot::BtrfsRecord;
    use crate::boot::systemd_boot::{Entry, LoaderPolicy, list_boot_environments};

    const BASE: &str = "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-4";
    const CHILD: &str = "cps-5713ac62-4310-4ad4-8272-4825bb09a546-5";

    fn description(id: &str, revision: u64, parent: Option<&str>) -> String {
        Entry {
            id,
            revision,
            parent,
            cmdline: "root=PARTUUID=3f93dbde-d29d-4c3f-9a3b-3d165490c830 rootfstype=btrfs rootflags=subvolid=256",
            btrfs: &BtrfsRecord {
                fsid: "11111111-1111-4111-8111-111111111111".into(),
                uuid: "22222222-2222-4222-8222-222222222222".into(),
                subvolid: 256,
                path: "@root".into(),
            },
            linux_sha256: &"a".repeat(64),
            initrd_sha256: None,
            uki_sha256: None,
            kernel_release: None,
            backup: false,
        }
        .render()
    }

    #[test]
    fn names_are_safe_and_distinct_from_boot_ids() {
        for valid in ["baseline", "before-6_12", "0", "cps"] {
            assert!(validate_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "../root", "-foo", "foo.bar", "A", "x\nlinux", BASE] {
            assert!(validate_name(invalid).is_err(), "{invalid}");
        }
        assert!(validate_name(&"a".repeat(48)).is_ok());
        assert!(validate_name(&"a".repeat(49)).is_err());
    }

    #[test]
    fn rename_preserves_identity_and_selection_and_reserves_retiring_names() -> Result<()> {
        let esp = tempfile::tempdir()?;
        let dir = esp.path().join("loader/entries");
        fs::create_dir_all(&dir)?;
        let loader = esp.path().join("loader/loader.conf");
        LoaderPolicy::pinned(BASE).write(&loader)?;
        let original_policy = fs::read(&loader)?;
        let base = description(BASE, 4, None);
        let base_path = dir.join(format!("{BASE}.conf"));
        fs::write(&base_path, &base)?;
        let child = description(CHILD, 5, Some(BASE));
        let trial_path = dir.join(format!("{CHILD}+2.conf"));
        fs::write(&trial_path, &child)?;
        assert!(rename_boot_environment(esp.path(), CHILD, "trial").is_err());
        fs::write(
            dir.join(format!("{CHILD}.cps-destroy")),
            named_description(&child, "retiring"),
        )?;
        fs::remove_file(trial_path)?;
        assert!(rename_boot_environment(esp.path(), BASE, "retiring").is_err());
        assert_eq!(rename_boot_environment(esp.path(), BASE, "baseline")?, BASE);
        assert_eq!(
            rename_boot_environment(esp.path(), "baseline", "before-update")?,
            BASE
        );
        assert_eq!(
            fs::read_to_string(base_path)?,
            format!("{base}{NAME_FIELD}before-update\n")
        );
        assert_eq!(fs::read(loader)?, original_policy);
        let environments = list_boot_environments(esp.path())?;
        assert_eq!(environments[0].name.as_deref(), Some("retiring"));
        assert!(environments[0].is_retiring);
        assert_eq!(environments[1].name.as_deref(), Some("before-update"));
        assert!(environments[1].is_default);
        let entries = policy::boot_entries(&dir)?;
        assert_eq!(
            super::super::find_known_good(&entries, "before-update")?.id,
            BASE
        );
        assert!(find(&entries, "baseline").is_err());
        Ok(())
    }

    #[test]
    fn unreadable_entries_are_listed_but_block_only_removal() -> Result<()> {
        const OTHER: &str = "cps-0c3e5a8e-41c5-4f43-9f53-58d6a3b0c6f1-6";
        let esp = tempfile::tempdir()?;
        let dir = esp.path().join("loader/entries");
        fs::create_dir_all(&dir)?;
        LoaderPolicy::pinned(BASE).write(&esp.path().join("loader/loader.conf"))?;
        fs::write(dir.join(format!("{BASE}.conf")), description(BASE, 4, None))?;
        // A dual-boot distribution's entry is not CPS's to read.
        fs::write(
            dir.join("fedora-6.1.conf"),
            "title Fedora\nlinux /vmlinuz\n",
        )?;
        // A power loss truncated one entry, and two files claim another.
        fs::write(dir.join(format!("{CHILD}.conf")), "title CPS\n")?;
        let other = description(OTHER, 6, None);
        fs::write(dir.join(format!("{OTHER}.conf")), &other)?;
        fs::write(dir.join(format!("{OTHER}+1.conf")), &other)?;

        let environments = list_boot_environments(esp.path())?;
        let summary: Vec<_> = environments
            .iter()
            .map(|entry| (entry.id.as_str(), entry.is_default, entry.invalid.is_some()))
            .collect();
        assert_eq!(
            summary,
            [
                (BASE, true, false),
                (OTHER, false, true),
                (OTHER, false, true),
                (CHILD, false, true),
            ]
        );
        assert!(environments.iter().any(|entry| {
            entry
                .invalid
                .as_deref()
                .is_some_and(|error| error.contains("another entry also claims"))
        }));

        // Commands that name a readable environment still work.
        assert_eq!(rename_boot_environment(esp.path(), BASE, "kept")?, BASE);
        let error = rename_boot_environment(esp.path(), CHILD, "broken").unwrap_err();
        assert!(format!("{error:#}").contains("is invalid"), "{error:#}");
        // Removal must know every root an entry could boot.
        let (_, (_, invalid)) = policy::inventory(esp.path())?;
        let error = super::super::require_readable(&invalid).unwrap_err();
        assert!(
            format!("{error:#}").contains("repair or remove it"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn an_invalid_name_is_ignored_and_can_be_replaced() -> Result<()> {
        let esp = tempfile::tempdir()?;
        let dir = esp.path().join("loader/entries");
        fs::create_dir_all(&dir)?;
        LoaderPolicy::pinned(BASE).write(&esp.path().join("loader/loader.conf"))?;
        let path = dir.join(format!("{BASE}.conf"));
        let base = description(BASE, 4, None);
        fs::write(&path, format!("{base}{NAME_FIELD}Before-Update\n"))?;
        let environments = list_boot_environments(esp.path())?;
        assert_eq!(environments[0].name, None);
        assert!(rename_boot_environment(esp.path(), "Before-Update", "fixed").is_err());
        assert_eq!(rename_boot_environment(esp.path(), BASE, "fixed")?, BASE);
        assert_eq!(
            fs::read_to_string(path)?,
            format!("{base}{NAME_FIELD}fixed\n")
        );
        Ok(())
    }
}
