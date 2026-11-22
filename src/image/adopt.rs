//! Hand an image that an unprivileged user built to root.
//!
//! CPS trusts only image state that the user running it owns, so root never
//! reads an image that another user could have configured. An image that a
//! user built without privileges, such as a disk image made by
//! `cps image disk`, belongs to that user, and every file in it does too:
//! the user could not give files to other accounts, so the image maps every
//! owner name to its creator. Adoption is root's explicit decision to take
//! such an image over. It checks that the installed files still match their
//! packages, hands everything the creator owns on the image's filesystem to
//! root, creates the accounts the packages name, gives each package path the
//! owner its manifest names, and records root as the image's owner.
//!
//! The ownership changes cannot be one atomic step. The image records its
//! new owner only once every file has it, so an interrupted adoption leaves
//! the image the creator's, and running the command again finishes it.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, Stat, XattrFlags, chmodat, chownat, fstat, lgetxattr,
        lsetxattr, openat, statat,
    },
    io::Errno,
    process::{Gid, Uid, geteuid},
};

use super::{
    Image,
    accounts::ROOT,
    root::{Fingerprint, Observation, Root, settle},
};
use crate::model::Node;

const CAPABILITY: &str = "security.capability";

/// Adopt the image at `path` for root. Returns the UID it belonged to.
pub(crate) fn adopt_image(path: &Path) -> Result<u32> {
    ensure!(geteuid().is_root(), "only root can adopt an image");
    let path = fs::canonicalize(path).with_context(|| format!("opening {}", path.display()))?;
    let mut image = Image::open_for_adoption(Root::open(&path)?)?;
    image.require_clean()?;
    // Root vouches for the image from now on, so its packages' files must
    // still be what they installed.
    crate::verify::require_quick(&image).context("checking the image before adopting it")?;
    let state = image.state()?;
    let creator = (state.settings.uid, state.settings.gid);
    let records = image.file_records(None)?;

    let device = image.root.as_file().metadata()?.dev();
    hand_over_tree(image.root.as_file(), creator, device)?;
    let info = fstat(image.root.as_file())?;
    hand_over_entry(image.root.as_file(), None, &info, creator)?;

    let mut settings = state.settings.clone();
    settings.uid = 0;
    settings.gid = 0;
    let paths = crate::plan::effective_paths(&settings, &state.installed)?;
    let accounts = image.root.accounts();
    accounts.map_by_name();
    let unresolved = paths
        .values()
        .filter_map(|managed| managed.node.ownership())
        .any(|(owner, group)| accounts.ids(owner, group).is_err());
    if unresolved {
        crate::systemd::create_all_accounts(&path, path == Path::new("/"))
            .context("creating the accounts that the image's packages name")?;
    }
    for (name, managed) in &paths {
        if managed.implicit {
            continue;
        }
        set_owner(&image.root, name, &managed.node)
            .with_context(|| format!("giving /{name} its owner"))?;
    }
    image.root.sync()?;

    // The files kept their contents, which the check above verified, so
    // their fingerprints only need their new owners. A fingerprint must
    // settle before it is recorded.
    std::thread::sleep(settle());
    let mut observed = BTreeMap::new();
    for (name, record) in records {
        let Some(managed) = paths.get(&name) else {
            continue;
        };
        if matches!(
            &managed.node,
            Node::File {
                capabilities: Some(_),
                ..
            }
        ) {
            continue;
        }
        let Some(file) = image.root.regular_entry(&name)? else {
            continue;
        };
        let fingerprint = Fingerprint::of(&fstat(&file)?);
        let before = &record.observation.fingerprint;
        if (fingerprint.ino, fingerprint.size, fingerprint.mtime_ns)
            == (before.ino, before.size, before.mtime_ns)
            && fingerprint.is_settled()
        {
            observed.insert(
                name,
                Observation {
                    fingerprint,
                    ..record.observation
                },
            );
        }
    }
    image.record_adoption(&settings, &paths, &observed)?;
    Ok(creator.0)
}

/// Hand every entry below `directory` on the image's filesystem that the
/// creator owns to root. Mount points and nested subvolumes are other
/// filesystems and stay as they are.
fn hand_over_tree(directory: &File, creator: (u32, u32), device: u64) -> Result<()> {
    for entry in fs::read_dir(Root::fd_path(directory))? {
        let name = entry?.file_name();
        let info = match statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(info) => info,
            Err(Errno::NOENT) => continue,
            Err(error) => return Err(error).with_context(|| format!("checking {name:?}")),
        };
        if info.st_dev != device {
            continue;
        }
        if FileType::from_raw_mode(info.st_mode) == FileType::Directory {
            let child: File = openat(
                directory,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
            .into();
            hand_over_tree(&child, creator, device)?;
        }
        hand_over_entry(directory, Some(&name), &info, creator)?;
    }
    Ok(())
}

/// Give one entry root's IDs in place of the creator's. Changing a file's
/// owner clears its set-ID bits and file capabilities, so they are restored.
fn hand_over_entry(
    directory: &File,
    name: Option<&OsStr>,
    info: &Stat,
    (uid, gid): (u32, u32),
) -> Result<()> {
    let owner = if info.st_uid == uid { 0 } else { info.st_uid };
    let group = if info.st_gid == gid { 0 } else { info.st_gid };
    if (owner, group) == (info.st_uid, info.st_gid) {
        return Ok(());
    }
    let name = name.unwrap_or(OsStr::new("."));
    let regular = FileType::from_raw_mode(info.st_mode) == FileType::RegularFile;
    let path = Root::fd_path(directory).join(name);
    let capability = if regular {
        read_capability(&path)?
    } else {
        None
    };
    chownat(
        directory,
        name,
        Some(Uid::from_raw(owner)),
        Some(Gid::from_raw(group)),
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .with_context(|| format!("changing the owner of {name:?}"))?;
    if regular && info.st_mode & 0o6000 != 0 {
        chmodat(
            directory,
            name,
            Mode::from_raw_mode(info.st_mode & 0o7777),
            AtFlags::empty(),
        )?;
    }
    if let Some(value) = capability {
        lsetxattr(&path, CAPABILITY, &value, XattrFlags::empty())?;
    }
    Ok(())
}

fn read_capability(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut value = vec![0; 64];
    match lgetxattr(path, CAPABILITY, &mut value[..]) {
        Ok(size) => {
            value.truncate(size);
            Ok(Some(value))
        }
        Err(Errno::NODATA | Errno::NOTSUP) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Give a package path the owner, group and mode its manifest names, and
/// the file capabilities it declares.
fn set_owner(root: &Root, path: &str, node: &Node) -> Result<()> {
    match node {
        Node::File {
            owner,
            group,
            capabilities,
            ..
        } if owner != ROOT || group != ROOT || capabilities.is_some() => {
            root.set_file_metadata(&root.file(path)?, node)
        }
        Node::Dir { owner, group, .. } if owner != ROOT || group != ROOT => {
            let directory = root.dir(path)?;
            root.set_directory_metadata(&directory, node)
        }
        Node::Symlink { owner, group, .. } if owner != ROOT || group != ROOT => {
            let (uid, gid) = root.accounts().ids(owner, group)?;
            root.set_link_owner(path, uid, gid)
        }
        _ => Ok(()),
    }
}
