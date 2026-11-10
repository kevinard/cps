//! Offline Btrfs disk images, for installers and VM tests.
//!
//! The volume directory becomes the Btrfs top level. Its root and
//! environments directories become subvolumes, laid out like an installed
//! system. The ESP gets a known-good systemd-boot entry for the root, as
//! `activate-btrfs --known-good` would write on a mounted system.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    process::{Output, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::FileType;

use super::{
    btrfs_boot::{BtrfsRecord, clear_registration, mark_registered},
    check_image_integrity,
    disk::DiskStage,
    entry_id,
    esp::{Esp, disk_helper},
    is_registered_btrfs, source_date_epoch,
    systemd_boot::{
        Activation, BootFiles, bootstrap_offline, canonical_partuuid, policy::BootRoot,
    },
    tree,
};
use crate::{
    image::{Image, root::Root},
    model::digest_reader,
};

const FILESYSTEM: &str = "root.btrfs";

/// What `cps boot-env image` turns into a bootable disk.
pub struct BootImage<'a> {
    /// Directory that becomes the Btrfs top-level volume.
    pub volume: &'a Path,
    /// Inactive CPS root in the volume directory, for the root subvolume.
    pub root: &'a Path,
    /// Empty directory in the volume directory, for the environments subvolume.
    pub environments: &'a Path,
    /// GPT disk with a systemd-boot ESP and the root partition.
    pub disk: &'a Path,
    pub root_partuuid: &'a str,
    pub output: &'a Path,
    pub kernel: String,
    pub initrd: Option<String>,
    pub options: String,
}

/// Write a Btrfs root and its known-good boot entry to a new copy of a GPT disk.
pub fn create_boot_image(spec: BootImage<'_>) -> Result<String> {
    let epoch = source_date_epoch()?;
    let partuuid = canonical_partuuid(spec.root_partuuid)?;
    let activation = Activation {
        root_partuuid: Some(partuuid.clone()),
        kernel: spec.kernel,
        initrd: spec.initrd,
        options: spec.options,
        tries: 1,
        known_good: true,
    };
    activation.check()?;
    let volume = fs::canonicalize(spec.volume).context("opening the volume directory")?;
    ensure!(
        volume != Path::new("/"),
        "the volume must be a directory for the new filesystem"
    );
    let root_name = child_name(&volume, spec.root, "--root")?;
    let environments = child_name(&volume, spec.environments, "--environments")?;
    ensure!(
        root_name != environments,
        "the root and environments directories must differ"
    );
    let mut names = Vec::new();
    for entry in fs::read_dir(&volume)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    let mut expected = vec![root_name.clone(), environments.clone()];
    expected.sort();
    ensure!(
        names == expected,
        "the volume directory must contain only the root and environments directories"
    );
    let environments_path = volume.join(&environments);
    ensure!(
        fs::symlink_metadata(&environments_path)?.is_dir()
            && fs::read_dir(&environments_path)?.next().is_none(),
        "the environments directory must be empty"
    );
    let output_parent = fs::canonicalize(
        spec.output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
    .context("opening the disk image destination directory")?;
    ensure!(
        !output_parent.starts_with(&volume),
        "the disk image must be outside the volume directory"
    );

    // Exclude package changes until the filesystem is written and checked.
    let image = Image::open(&volume.join(&root_name), true)?;
    image.root.offline_path()?;
    ensure!(
        !is_registered_btrfs(&image.root)?,
        "the root is registered on a Btrfs volume; image an inactive root"
    );
    image.require_clean()?;
    check_image_integrity(&image)?;
    let state = image.state()?;
    let settings = &state.settings;
    ensure!(
        settings.boot_parent.is_none(),
        "the root is a staged trial; an offline image starts from a known-good base"
    );
    let image_id = canonical_partuuid(&settings.id).context("invalid image identity")?;
    let revision = settings.boot_revision();
    let id = entry_id(&image_id, revision);
    let files = BootFiles::open(&image, &state, &activation)?;
    check_tree(&image.root)?;
    let fingerprint = tree::digest(&image.root).context("fingerprinting the root")?;

    let stage = DiskStage::new(spec.disk, spec.output)?;
    let work = stage.directory();
    let partition = stage.disk.root(&partuuid)?;
    let esp_partition = stage.disk.esp()?;
    let esp = Esp::read(&stage.disk, esp_partition, &work, epoch)?;
    esp.require_no_boot_files()?;

    let filesystem = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(work.join(FILESYSTEM))?;
    filesystem.set_len(partition.bytes)?;
    let registration = Registration::mark(&image.root, &id)?;
    let mut mkfs = disk_helper("mkfs.btrfs", &work)?;
    if let Some(epoch) = epoch {
        // btrfs-progs then derives its internal UUIDs from the filesystem UUID.
        mkfs.env("SOURCE_DATE_EPOCH", epoch.to_string())
            .env("DETERMINISTIC_SEED", "1");
    }
    run(
        mkfs.args(["-q", "-f", "-U", &image_id, "--rootdir"])
            .arg(&volume)
            .arg("--subvol")
            .arg(format!("rw:{root_name}"))
            .arg("--subvol")
            .arg(format!("rw:{environments}"))
            .arg(FILESYSTEM),
        "formatting the root filesystem with mkfs.btrfs",
    )?;
    // mkfs.btrfs grows an image file that is too small for the tree.
    ensure!(
        filesystem.metadata()?.len() == partition.bytes,
        "the root does not fit in its {} partition",
        super::mib(partition.bytes)
    );
    run(
        disk_helper("btrfs", &work)?.args(["check", "--readonly", FILESYSTEM]),
        "checking the root filesystem with btrfs check",
    )?;
    let identity = Identity::read(&work, &root_name, &environments)?;
    ensure!(
        identity.fsid == image_id,
        "mkfs.btrfs did not apply the filesystem UUID {image_id}"
    );
    ensure!(
        identity.registration.as_deref() == Some(id.as_str()),
        "mkfs.btrfs did not copy the root's boot registration"
    );
    registration.clear()?;
    ensure!(
        tree::digest(&image.root)? == fingerprint,
        "the root changed while its filesystem was written"
    );
    let record = BtrfsRecord {
        fsid: image_id,
        uuid: identity.root_uuid,
        subvolid: identity.root_id,
        path: root_name,
    };

    let checked = File::open(work.join(FILESYSTEM))?;
    let before = filesystem.metadata()?;
    let after = checked.metadata()?;
    ensure!(
        (before.dev(), before.ino(), partition.bytes) == (after.dev(), after.ino(), after.len()),
        "the checked filesystem image was replaced or resized"
    );
    let sha256 = digest_reader(&checked)?;
    ensure!(
        stage.disk.write_partition(partition, &checked)? == sha256,
        "the root partition differs from the checked filesystem image"
    );
    let written = bootstrap_offline(
        &esp.tree,
        &id,
        revision,
        &record,
        &BootRoot::Partition(partuuid),
        &activation,
        files,
    )?;
    esp.write(
        &stage.disk,
        esp_partition,
        written.iter().map(String::as_str),
    )?;
    stage.publish()?;
    Ok(id)
}

/// The name of a directory directly inside the volume directory.
fn child_name(volume: &Path, path: &Path, option: &str) -> Result<String> {
    let path = fs::canonicalize(path).with_context(|| format!("opening {option}"))?;
    ensure!(
        path.parent() == Some(volume),
        "{option} must name a directory directly inside the volume directory"
    );
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .with_context(|| format!("{option} must have a UTF-8 name"))?;
    ensure!(
        !name.starts_with('.') && !name.chars().any(|c| c.is_control() || c == ':'),
        "{option} has an unsupported subvolume name {name:?}"
    );
    Ok(name.to_owned())
}

/// Reject content that mkfs.btrfs would copy from another filesystem.
fn check_tree(root: &Root) -> Result<()> {
    let device = root.as_file().metadata()?.dev();
    root.walk(None, |path, _, info| {
        ensure!(
            matches!(
                FileType::from_raw_mode(info.st_mode),
                FileType::Directory | FileType::RegularFile | FileType::Symlink
            ),
            "cannot image special file {path:?}"
        );
        ensure!(
            info.st_dev == device,
            "cannot image {path:?}, which is on another filesystem"
        );
        Ok(true)
    })
}

/// A boot registration on the source root, which mkfs.btrfs copies into the image.
struct Registration<'a>(Option<&'a Root>);

impl<'a> Registration<'a> {
    fn mark(root: &'a Root, id: &str) -> Result<Self> {
        mark_registered(root, id).context(
            "marking the root for the image. A root with a stale user.cps.boot-environment \
             attribute from an interrupted image needs it removed first",
        )?;
        Ok(Self(Some(root)))
    }

    fn clear(mut self) -> Result<()> {
        if let Some(root) = self.0.take() {
            clear_registration(root)?;
        }
        Ok(())
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        if let Some(root) = self.0.take()
            && let Err(error) = clear_registration(root)
        {
            eprintln!("Warning: could not clear the root's temporary boot registration: {error:#}");
        }
    }
}

fn run(command: &mut std::process::Command, action: &str) -> Result<Output> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| action.to_owned())?;
    ensure!(
        output.status.success(),
        "{action} failed ({})\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    Ok(output)
}

/// The identifiers mkfs.btrfs assigned, read back from the filesystem image.
#[derive(Debug, Eq, PartialEq)]
struct Identity {
    fsid: String,
    root_id: u64,
    root_uuid: String,
    registration: Option<String>,
}

impl Identity {
    fn read(work: &Path, root: &str, environments: &str) -> Result<Self> {
        let dump = |args: &[&str]| -> Result<String> {
            let output = run(
                disk_helper("btrfs", work)?
                    .args(["inspect-internal"])
                    .args(args)
                    .arg(FILESYSTEM),
                "reading the root filesystem with btrfs inspect-internal",
            )?;
            Ok(String::from_utf8(output.stdout)?)
        };
        let fsid = superblock_fsid(&dump(&["dump-super"])?)?;
        let subvolumes = subvolumes(&dump(&["dump-tree", "-t", "root"])?)?;
        ensure!(
            subvolumes.contains_key(environments),
            "mkfs.btrfs did not create the environments subvolume"
        );
        let (root_id, root_uuid) = subvolumes
            .get(root)
            .cloned()
            .context("mkfs.btrfs did not create the root subvolume")?;
        let registration = root_registration(&dump(&["dump-tree", "-t", &root_id.to_string()])?)?;
        Ok(Self {
            fsid,
            root_id,
            root_uuid,
            registration,
        })
    }
}

fn superblock_fsid(text: &str) -> Result<String> {
    let mut found = text.lines().filter_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        (fields.next() == Some("fsid"))
            .then(|| fields.next())
            .flatten()
    });
    let fsid = found.next().context("btrfs dump-super printed no fsid")?;
    ensure!(found.next().is_none(), "btrfs dump-super printed two fsids");
    canonical_partuuid(fsid)
}

/// Subvolume IDs and UUIDs by name, for the subvolumes directly below the top level.
fn subvolumes(text: &str) -> Result<BTreeMap<String, (u64, String)>> {
    let mut names = BTreeMap::new();
    let mut uuids = BTreeMap::new();
    let mut item: Option<(&str, u64)> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(key) = line
            .strip_prefix("item ")
            .and_then(|rest| rest.split_once(" key ("))
            .and_then(|(_, key)| key.split_once(')'))
            .map(|(key, _)| key)
        {
            let fields: Vec<_> = key.split_ascii_whitespace().collect();
            item = match fields[..] {
                ["FS_TREE", "ROOT_REF", id] => Some(("ref", id.parse()?)),
                [id, "ROOT_ITEM", _] => id.parse().ok().map(|id| ("item", id)),
                _ => None,
            };
            continue;
        }
        match item {
            Some(("ref", id)) => {
                if let Some(name) = line
                    .strip_prefix("root ref key dirid 256 ")
                    .and_then(|rest| rest.split_once(" name "))
                    .map(|(_, name)| name)
                {
                    ensure!(
                        names.insert(name.to_owned(), id).is_none(),
                        "duplicate Btrfs subvolume name {name}"
                    );
                    item = None;
                }
            }
            Some(("item", id)) => {
                if let Some(uuid) = line.strip_prefix("uuid ") {
                    uuids.insert(id, canonical_partuuid(uuid.trim())?);
                    item = None;
                }
            }
            _ => {}
        }
    }
    names
        .into_iter()
        .map(|(name, id)| {
            let uuid = uuids
                .get(&id)
                .with_context(|| format!("Btrfs subvolume {name} has no root item"))?;
            Ok((name, (id, uuid.clone())))
        })
        .collect()
}

/// The boot registration on the top directory of a subvolume's tree dump.
fn root_registration(text: &str) -> Result<Option<String>> {
    let mut lines = text.lines().map(str::trim);
    let mut found = None;
    while let Some(line) = lines.next() {
        if !line.contains(" key (256 XATTR_ITEM ") {
            continue;
        }
        let mut name = None;
        for line in lines.by_ref() {
            if let Some(value) = line.strip_prefix("name: ") {
                name = Some(value);
            } else if let Some(value) = line.strip_prefix("data ") {
                if name == Some(super::btrfs_boot::REGISTRATION) {
                    if found.replace(value.to_owned()).is_some() {
                        bail!("duplicate boot registration in the root subvolume");
                    }
                }
                break;
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_TREE: &str = "\
root tree
leaf 30539776 items 18 free space 11117 generation 9 owner ROOT_TREE
fs uuid 11111111-2222-4333-8444-555555555555
\titem 3 key (FS_TREE ROOT_ITEM 0) itemoff 14949 itemsize 439
\t\tgeneration 6 root_dirid 256 bytenr 30408704 byte_limit 0 bytes_used 16384
\t\tuuid ca43c0f5-4a8f-40be-b815-04170b80fea6
\t\tparent_uuid 00000000-0000-0000-0000-000000000000
\titem 4 key (FS_TREE ROOT_REF 256) itemoff 14918 itemsize 31
\t\troot ref key dirid 256 sequence 2 name @environments
\titem 5 key (FS_TREE ROOT_REF 257) itemoff 14899 itemsize 19
\t\troot ref key dirid 256 sequence 3 name @
\titem 13 key (256 ROOT_ITEM 0) itemoff 12495 itemsize 439
\t\tgeneration 6 root_dirid 256 bytenr 30425088 byte_limit 0 bytes_used 16384
\t\tlast_snapshot 0 flags 0x0(none) refs 1
\t\tuuid 333FB1EC-9AEA-4E96-AEE5-DD5FB9A5C0B8
\t\tparent_uuid 00000000-0000-0000-0000-000000000000
\titem 14 key (256 ROOT_BACKREF 5) itemoff 12476 itemsize 19
\t\troot backref key dirid 256 sequence 2 name @environments
\titem 15 key (257 ROOT_ITEM 0) itemoff 12037 itemsize 439
\t\tuuid 03c23ede-5c3e-4f12-a51f-a744dad9511f
\t\tparent_uuid 00000000-0000-0000-0000-000000000000
\titem 16 key (257 ROOT_BACKREF 5) itemoff 12006 itemsize 31
\t\troot backref key dirid 256 sequence 3 name @
";

    #[test]
    fn reads_subvolume_identities_from_the_root_tree() -> Result<()> {
        let found = subvolumes(ROOT_TREE)?;
        assert_eq!(
            found.get("@"),
            Some(&(257, "03c23ede-5c3e-4f12-a51f-a744dad9511f".to_owned()))
        );
        assert_eq!(
            found.get("@environments"),
            Some(&(256, "333fb1ec-9aea-4e96-aee5-dd5fb9a5c0b8".to_owned()))
        );
        assert_eq!(found.len(), 2);
        // A reference whose root item is missing is an error, not a guess.
        let truncated = &ROOT_TREE[..ROOT_TREE.find("\titem 15").unwrap()];
        assert!(subvolumes(truncated).is_err());
        Ok(())
    }

    #[test]
    fn reads_the_filesystem_uuid_and_registration() -> Result<()> {
        let super_block = "superblock: bytenr=65536, device=root.btrfs\n\
            csum_type\t\t0 (crc32c)\n\
            fsid\t\t\tA47593C2-E985-42D2-92B6-44C7057E1E96\n\
            metadata_uuid\t\t00000000-0000-0000-0000-000000000000\n";
        assert_eq!(
            superblock_fsid(super_block)?,
            "a47593c2-e985-42d2-92b6-44c7057e1e96"
        );
        assert!(superblock_fsid("csum_type 0\n").is_err());

        let tree = "\
\titem 1 key (256 INODE_REF 256) itemoff 16111 itemsize 12
\t\tindex 0 namelen 2 name: ..
\titem 2 key (256 XATTR_ITEM 1368964456) itemoff 16051 itemsize 60
\t\tlocation key (0 UNKNOWN.0 0) type XATTR
\t\ttransid 0 data_len 5 name_len 25
\t\tname: user.cps.boot-environment
\t\tdata cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2
\titem 3 key (256 XATTR_ITEM 9) itemoff 16000 itemsize 50
\t\tname: security.selinux
\t\tdata system_u:object_r:root_t:s0
";
        assert_eq!(
            root_registration(tree)?.as_deref(),
            Some("cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2")
        );
        assert_eq!(
            root_registration("\titem 2 key (257 XATTR_ITEM 1)\n")?,
            None
        );
        Ok(())
    }

    #[test]
    fn subvolumes_are_direct_children_of_the_volume() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let volume = fs::canonicalize(dir.path())?;
        fs::create_dir_all(volume.join("@/nested"))?;
        assert_eq!(child_name(&volume, &volume.join("@"), "--root")?, "@");
        assert!(child_name(&volume, &volume.join("@/nested"), "--root").is_err());
        assert!(child_name(&volume, &volume, "--root").is_err());
        fs::create_dir(volume.join("rw:@"))?;
        assert!(child_name(&volume, &volume.join("rw:@"), "--root").is_err());
        Ok(())
    }
}
