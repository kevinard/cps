use std::{
    ffi::OsStr,
    fs::{self, File},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{XattrFlags, fgetxattr, fremovexattr, fsetxattr, fstatfs},
    io::Errno,
    ioctl::{Opcode, Updater, ioctl, opcode},
};

use super::{
    Activation, check_image_integrity,
    device::{mounted_esp_disk, mounted_root_disk},
    systemd_boot::{
        self, BootMode, canonical_partuuid,
        policy::{BootRoot, field},
    },
};
use crate::{
    image::{Image, root::Root},
    model::validate_path,
};

const FSID: &str = "# cps-btrfs-fsid ";
const UUID: &str = "# cps-btrfs-uuid ";
const SUBVOLUME: &str = "# cps-btrfs-subvolid ";
const PATH: &str = "# cps-btrfs-path ";
const REGISTRATION: &str = "user.cps.boot-environment";

pub(super) fn registered(root: &Root) -> Result<bool> {
    Ok(registration(root)?.is_some())
}

pub(super) fn clear_registration(root: &Root) -> Result<()> {
    match fremovexattr(root.as_file(), REGISTRATION) {
        Ok(()) | Err(Errno::NODATA) => Ok(()),
        Err(error) => Err(error).context("clearing the staging snapshot's boot registration"),
    }
}

pub(super) fn mark_registered(root: &Root, id: &str) -> Result<()> {
    match registration(root)? {
        Some(existing) => ensure!(
            existing == id.as_bytes(),
            "Btrfs root has a different boot registration"
        ),
        None => {
            fsetxattr(
                root.as_file(),
                REGISTRATION,
                id.as_bytes(),
                XattrFlags::CREATE,
            )?;
        }
    }
    root.sync()
}

fn registration(root: &Root) -> Result<Option<Vec<u8>>> {
    let mut existing = [0u8; 128];
    match fgetxattr(root.as_file(), REGISTRATION, &mut existing[..]) {
        Ok(len) => Ok(Some(existing[..len].to_vec())),
        Err(Errno::NODATA) => Ok(None),
        Err(error) => Err(error).context("reading the Btrfs boot registration"),
    }
}

#[repr(C)]
struct FsInfo {
    max_id: u64,
    num_devices: u64,
    fsid: [u8; 16],
    reserved: [u8; 992],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IoctlTimespec {
    sec: u64,
    nsec: u32,
}

#[repr(C)]
struct SubvolInfo {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    parent_uuid: [u8; 16],
    received_uuid: [u8; 16],
    transids: [u64; 4],
    times: [IoctlTimespec; 4],
    reserved: [u64; 8],
}

#[repr(C)]
struct DeviceInfo {
    devid: u64,
    uuid: [u8; 16],
    bytes_used: u64,
    total_bytes: u64,
    fsid: [u8; 16],
    reserved: [u64; 377],
    path: [u8; 1024],
}

#[repr(C)]
struct SearchKey {
    tree_id: u64,
    min_objectid: u64,
    max_objectid: u64,
    min_offset: u64,
    max_offset: u64,
    min_transid: u64,
    max_transid: u64,
    min_type: u32,
    max_type: u32,
    nr_items: u32,
    // The UAPI's unused u32 and four unused u64 fields.
    unused: [u32; 9],
}

#[repr(C)]
struct SearchArgs {
    key: SearchKey,
    buf: [u8; 4096 - size_of::<SearchKey>()],
}

const _: () = assert!(size_of::<SearchKey>() == 104);
const _: () = assert!(size_of::<SearchArgs>() == 4096);
const _: () = assert!(size_of::<FsInfo>() == 1024);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<SubvolInfo>() == 504);
const _: () = assert!(size_of::<DeviceInfo>() == 4096);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BtrfsRecord {
    pub fsid: String,
    pub uuid: String,
    pub subvolid: u64,
    pub path: String,
}

impl BtrfsRecord {
    pub fn parse(entry: &str, options: &str) -> Result<Option<Self>> {
        let values = [
            field(entry, FSID)?,
            field(entry, UUID)?,
            field(entry, SUBVOLUME)?,
            field(entry, PATH)?,
        ];
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        let [fsid, uuid, subvolid, path] =
            values.map(|value| value.context("incomplete Btrfs boot identity"));
        let (fsid, uuid, subvolid, path) = (fsid?, uuid?, subvolid?, path?);
        ensure!(
            canonical_partuuid(fsid)? == fsid && canonical_partuuid(uuid)? == uuid,
            "noncanonical Btrfs boot UUID"
        );
        validate_root_path(path)?;
        let number: u64 = subvolid
            .parse()
            .context("invalid Btrfs boot subvolume ID")?;
        ensure!(
            number >= 256 && number.to_string() == subvolid,
            "invalid Btrfs boot subvolume ID"
        );
        let flags: Vec<_> = options
            .split_ascii_whitespace()
            .filter(|arg| *arg == "rootflags" || arg.starts_with("rootflags="))
            .collect();
        let types: Vec<_> = options
            .split_ascii_whitespace()
            .filter(|arg| *arg == "rootfstype" || arg.starts_with("rootfstype="))
            .collect();
        ensure!(
            flags == [format!("rootflags=subvolid={number}")] && types == ["rootfstype=btrfs"],
            "Btrfs boot options differ from the recorded subvolume"
        );
        Ok(Some(Self {
            fsid: fsid.to_owned(),
            uuid: uuid.to_owned(),
            subvolid: number,
            path: path.to_owned(),
        }))
    }

    pub fn render(&self) -> String {
        format!(
            "{FSID}{}\n{UUID}{}\n{SUBVOLUME}{}\n{PATH}{}\n",
            self.fsid, self.uuid, self.subvolid, self.path
        )
    }

    pub fn same_subvolume(&self, other: &Self) -> bool {
        self.fsid == other.fsid
            && (self.subvolid == other.subvolid
                || self.uuid == other.uuid
                || self.path == other.path)
    }
}

pub(super) struct BtrfsVolume {
    root: Root,
    path: PathBuf,
    fsid: String,
}

impl BtrfsVolume {
    pub fn open(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path).context("opening the Btrfs top-level volume")?;
        let root = Root::open(&path)?;
        ensure!(
            subvolume_info(&root)?.treeid == 5,
            "--btrfs-volume must expose the top-level subvolume (ID 5)"
        );
        let fsid = uuid_string(&filesystem_info(&root)?.fsid);
        Ok(Self { root, path, fsid })
    }

    fn single_device(&self) -> Result<PathBuf> {
        let fs = filesystem_info(&self.root)?;
        ensure!(
            fs.num_devices == 1,
            "root-partuuid requires a single-device Btrfs filesystem"
        );
        let mut device = DeviceInfo {
            devid: fs.max_id,
            uuid: [0; 16],
            bytes_used: 0,
            total_bytes: 0,
            fsid: [0; 16],
            reserved: [0; 377],
            path: [0; 1024],
        };
        const DEVICE: Opcode = opcode::read_write::<DeviceInfo>(0x94, 30);
        // SAFETY: the initialized buffer matches btrfs_ioctl_dev_info_args.
        unsafe { ioctl(self.root.as_file(), Updater::<DEVICE, _>::new(&mut device)) }
            .context("identifying the mounted Btrfs device")?;
        let end = device
            .path
            .iter()
            .position(|byte| *byte == 0)
            .context("unterminated Btrfs device path")?;
        let device = PathBuf::from(OsStr::from_bytes(&device.path[..end]));
        ensure!(
            device.is_absolute(),
            "Btrfs device has no absolute block path"
        );
        Ok(device)
    }

    pub fn record(&self, root: &Root, path: &Path) -> Result<BtrfsRecord> {
        let path = path
            .strip_prefix(&self.path)
            .context("the root must be beneath --btrfs-volume")?
            .to_str()
            .context("Btrfs root path is not UTF-8")?;
        validate_root_path(path)?;
        let opened = self.root.subvolume(path)?;
        let original = root.as_file().metadata()?;
        let current = opened.as_file().metadata()?;
        ensure!(
            (original.dev(), original.ino()) == (current.dev(), current.ino()),
            "Btrfs root path changed"
        );
        let info = subvolume_info(root)?;
        ensure!(
            info.treeid >= 256 && info.parent_id != 0,
            "expected a named, attached Btrfs root subvolume"
        );
        ensure!(
            uuid_string(&filesystem_info(root)?.fsid) == self.fsid,
            "Btrfs root belongs to a different filesystem"
        );
        Ok(BtrfsRecord {
            fsid: self.fsid.clone(),
            uuid: uuid_string(&info.uuid),
            subvolid: info.treeid,
            path: path.to_owned(),
        })
    }

    pub fn boot_root(&self, partuuid: Option<&str>) -> Result<BootRoot> {
        match partuuid {
            Some(uuid) => Ok(BootRoot::Partition(canonical_partuuid(uuid)?)),
            None => Ok(BootRoot::Btrfs(self.fsid.clone())),
        }
    }

    pub fn check_boot_device(&self, esp: &Path, locked: &File, root: &BootRoot) -> Result<()> {
        match root {
            BootRoot::Partition(uuid) => {
                mounted_root_disk(esp, locked, &self.single_device()?, uuid)?;
            }
            BootRoot::Btrfs(uuid) => {
                self.check_fsid(uuid)?;
                ensure!(
                    mounted_esp_disk(esp, locked)?.is_some(),
                    "--esp must name the root of a mounted EFI partition"
                );
            }
        }
        Ok(())
    }

    pub fn check_fsid(&self, fsid: &str) -> Result<()> {
        ensure!(
            self.has_fsid(fsid),
            "the boot entry belongs to a different Btrfs filesystem"
        );
        Ok(())
    }

    pub fn has_fsid(&self, fsid: &str) -> bool {
        fsid == self.fsid
    }

    pub fn verify(&self, record: &BtrfsRecord, image_id: &str, revision: u64) -> Result<Image> {
        let image = self.open_image(record, image_id, revision)?;
        check_image_integrity(&image)?;
        Ok(image)
    }

    pub fn open_image(&self, record: &BtrfsRecord, image_id: &str, revision: u64) -> Result<Image> {
        let root = self.open_registered(record, &super::entry_id(image_id, revision))?;
        let image = Image::open_query_root(root)?;
        let settings = image.settings()?;
        ensure!(
            settings.id == image_id && settings.boot_revision() == revision,
            "Btrfs root image or revision differs from its boot entry"
        );
        Ok(image)
    }

    pub fn open_registered(&self, record: &BtrfsRecord, id: &str) -> Result<Root> {
        self.check_fsid(&record.fsid)?;
        let root = self.root.subvolume(&record.path)?;
        ensure!(
            self.record(&root, &self.path.join(&record.path))? == *record,
            "Btrfs subvolume identity differs from its boot entry"
        );
        ensure!(
            registration(&root)?.as_deref() == Some(id.as_bytes()),
            "Btrfs root registration differs from its boot entry"
        );
        Ok(root)
    }

    pub fn destroy(&self, record: &BtrfsRecord, root: &Root) -> Result<()> {
        ensure!(
            self.record(root, &self.path.join(&record.path))? == *record,
            "Btrfs subvolume changed before destruction"
        );
        super::btrfs::destroy_by_id(&self.root, record.subvolid)
    }

    /// The filesystem's default subvolume ID, which the kernel refuses to delete.
    pub fn default_subvolume(&self) -> Result<u64> {
        // The root tree's directory item named "default" points at the default subvolume.
        const ROOT_TREE: u64 = 1;
        const ROOT_TREE_DIR: u64 = 6;
        const DIR_ITEM: u32 = 84;
        let mut args = SearchArgs {
            key: SearchKey {
                tree_id: ROOT_TREE,
                min_objectid: ROOT_TREE_DIR,
                max_objectid: ROOT_TREE_DIR,
                min_offset: 0,
                max_offset: u64::MAX,
                min_transid: 0,
                max_transid: u64::MAX,
                min_type: DIR_ITEM,
                max_type: DIR_ITEM,
                nr_items: 16,
                unused: [0; 9],
            },
            buf: [0; 4096 - size_of::<SearchKey>()],
        };
        const SEARCH: Opcode = opcode::read_write::<SearchArgs>(0x94, 17);
        // SAFETY: the initialized buffer matches btrfs_ioctl_search_args.
        unsafe { ioctl(self.root.as_file(), Updater::<SEARCH, _>::new(&mut args)) }
            .context("reading the Btrfs default subvolume")?;
        default_from_search(&args.buf, args.key.nr_items)
    }

    pub fn running_parent(&self) -> Result<Option<String>> {
        Ok(self.running()?.and_then(|(_, parent)| parent))
    }

    /// The registered entry ID of the running root on this volume, and its recorded parent.
    pub fn running(&self) -> Result<Option<(String, Option<String>)>> {
        let root = Root::open(Path::new("/"))?;
        if fstatfs(root.as_file())?.f_type != libc::BTRFS_SUPER_MAGIC
            || uuid_string(&filesystem_info(&root)?.fsid) != self.fsid
        {
            return Ok(None);
        }
        let Some(registered) = registration(&root)? else {
            return Ok(None);
        };
        let image = Image::open_query_root(root)?;
        let settings = image.settings()?;
        let id = super::entry_id(&settings.id, settings.boot_revision());
        ensure!(
            registered == id.as_bytes(),
            "the running root's boot registration differs from its image identity"
        );
        let parent = settings
            .boot_parent
            .map(|parent| {
                Ok::<_, anyhow::Error>(super::entry_id(
                    &canonical_partuuid(&parent.id)?,
                    parent.revision,
                ))
            })
            .transpose()?;
        Ok(Some((id, parent)))
    }
}

pub fn activate_btrfs_boot_environment(
    root: &Path,
    volume: &Path,
    esp: &Path,
    activation: &Activation,
) -> Result<String> {
    let volume = BtrfsVolume::open(volume)?;
    Ok(systemd_boot::activate(root, esp, activation, None, Some(&volume), BootMode::Split)?.id)
}

impl Activation<Option<String>> {
    /// Activate a Btrfs root, using the mounted filesystem UUID when no PARTUUID is supplied.
    pub fn activate_btrfs(&self, root: &Path, volume: &Path, esp: &Path) -> Result<String> {
        let volume = BtrfsVolume::open(volume)?;
        Ok(systemd_boot::activate(root, esp, self, None, Some(&volume), BootMode::Split)?.id)
    }

    /// Build and sign a UKI for the exact Btrfs subvolume being activated.
    pub fn activate_btrfs_uki(&self, root: &Path, volume: &Path, esp: &Path) -> Result<String> {
        let volume = BtrfsVolume::open(volume)?;
        Ok(systemd_boot::activate(root, esp, self, None, Some(&volume), BootMode::Uki)?.id)
    }

    /// Activate a signed UKI using the firmware's one-shot boot selection.
    pub fn activate_btrfs_uefi(&self, root: &Path, volume: &Path, esp: &Path) -> Result<String> {
        let volume = BtrfsVolume::open(volume)?;
        Ok(systemd_boot::activate(root, esp, self, None, Some(&volume), BootMode::Uefi)?.id)
    }
}

pub(crate) fn bless_uefi(esp: &Path, volume: &Path) -> Result<Option<String>> {
    systemd_boot::bless_uefi(esp, &BtrfsVolume::open(volume)?)
}

pub fn rollback_btrfs_boot_environment(
    esp: &Path,
    volume: &Path,
    id: Option<&str>,
) -> Result<String> {
    let volume = BtrfsVolume::open(volume)?;
    systemd_boot::rollback(esp, id, None, Some(&volume))
}

/// Preview rollback from the configured default without changing boot selection.
pub fn preview_btrfs_rollback(
    esp: &Path,
    volume: &Path,
    id: Option<&str>,
    paths: bool,
) -> Result<(String, crate::ImageDiff)> {
    systemd_boot::preview_btrfs_rollback(esp, &BtrfsVolume::open(volume)?, id, paths)
}

pub(crate) fn with_registered_images<T>(
    volume: &Path,
    esp: &Path,
    partuuid: Option<&str>,
    action: impl FnOnce(&[(String, Image)]) -> Result<T>,
) -> Result<T> {
    systemd_boot::with_registered_images(esp, &BtrfsVolume::open(volume)?, partuuid, action)
}

pub(crate) fn with_running_environment<T>(
    volume: &Path,
    esp: &Path,
    partuuid: Option<&str>,
    action: impl FnOnce(Root, &str, u64) -> Result<T>,
) -> Result<T> {
    let volume = BtrfsVolume::open(volume)?;
    systemd_boot::with_running_environment(&volume, esp, partuuid, action)
}

pub(crate) fn pending_trial(
    volume: &Path,
    esp: &Path,
    partuuid: Option<&str>,
) -> Result<Option<systemd_boot::PendingTrial>> {
    systemd_boot::pending_trial(&BtrfsVolume::open(volume)?, esp, partuuid)
}

fn filesystem_info(root: &Root) -> Result<FsInfo> {
    ensure!(
        fstatfs(root.as_file())?.f_type == libc::BTRFS_SUPER_MAGIC,
        "expected a Btrfs filesystem"
    );
    let mut info = FsInfo {
        max_id: 0,
        num_devices: 0,
        fsid: [0; 16],
        reserved: [0; 992],
    };
    const INFO: Opcode = opcode::read::<FsInfo>(0x94, 31);
    // SAFETY: the initialized buffer matches btrfs_ioctl_fs_info_args.
    unsafe { ioctl(root.as_file(), Updater::<INFO, _>::new(&mut info)) }?;
    Ok(info)
}

/// Find the "default" directory item in a tree search result buffer.
fn default_from_search(buf: &[u8], nr_items: u32) -> Result<u64> {
    let mut items = buf;
    for _ in 0..nr_items {
        // btrfs_ioctl_search_header: transid, objectid, offset (8 each), type, len (4 each).
        let header = items.get(..32).context("truncated Btrfs search result")?;
        let len = u32::from_ne_bytes(header[28..32].try_into()?) as usize;
        let item = items
            .get(32..32 + len)
            .context("truncated Btrfs search item")?;
        // btrfs_dir_item: location key (17), transid (8), data_len (2), name_len (2), type (1).
        if item.len() >= 30 {
            let name_len = u16::from_le_bytes(item[27..29].try_into()?) as usize;
            if item.get(30..30 + name_len) == Some(b"default".as_slice()) {
                return Ok(u64::from_le_bytes(item[..8].try_into()?));
            }
        }
        items = &items[32 + len..];
    }
    // Without the item the top level (ID 5) is the default.
    Ok(5)
}

fn subvolume_info(root: &Root) -> Result<SubvolInfo> {
    ensure!(
        fstatfs(root.as_file())?.f_type == libc::BTRFS_SUPER_MAGIC
            && root.as_file().metadata()?.ino() == 256,
        "expected the root of a Btrfs subvolume"
    );
    let mut info = SubvolInfo {
        treeid: 0,
        name: [0; 256],
        parent_id: 0,
        dirid: 0,
        generation: 0,
        flags: 0,
        uuid: [0; 16],
        parent_uuid: [0; 16],
        received_uuid: [0; 16],
        transids: [0; 4],
        times: [IoctlTimespec { sec: 0, nsec: 0 }; 4],
        reserved: [0; 8],
    };
    const INFO: Opcode = opcode::read::<SubvolInfo>(0x94, 60);
    // SAFETY: the initialized buffer matches btrfs_ioctl_get_subvol_info_args.
    unsafe { ioctl(root.as_file(), Updater::<INFO, _>::new(&mut info)) }?;
    Ok(info)
}

fn validate_root_path(path: &str) -> Result<()> {
    validate_path(path)?;
    ensure!(
        !path.chars().any(char::is_control),
        "invalid Btrfs root path"
    );
    Ok(())
}

fn uuid_string(bytes: &[u8; 16]) -> String {
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::{BtrfsRecord, default_from_search};

    /// A tree search result item holding a directory item named `name`.
    fn dir_item(name: &[u8], target: u64) -> Vec<u8> {
        let mut item = target.to_le_bytes().to_vec();
        item.push(132); // ROOT_ITEM key type
        item.extend_from_slice(&u64::MAX.to_le_bytes());
        item.extend_from_slice(&7u64.to_le_bytes()); // transid
        item.extend_from_slice(&0u16.to_le_bytes()); // data_len
        item.extend_from_slice(&(name.len() as u16).to_le_bytes());
        item.push(2); // BTRFS_FT_DIR
        item.extend_from_slice(name);
        let mut header = vec![0; 28];
        header.extend_from_slice(&(item.len() as u32).to_ne_bytes());
        header.extend(item);
        header
    }

    #[test]
    fn default_subvolume_comes_from_the_default_directory_item() -> anyhow::Result<()> {
        let mut buf = dir_item(b"other", 300);
        buf.extend(dir_item(b"default", 257));
        assert_eq!(default_from_search(&buf, 2)?, 257);
        // Items beyond the reported count are ignored.
        assert_eq!(default_from_search(&buf, 1)?, 5);
        // Without a default item the top level is the default.
        assert_eq!(default_from_search(&[], 0)?, 5);
        assert_eq!(default_from_search(&dir_item(b"defaults", 300), 1)?, 5);
        // A truncated result is an error, not a guess.
        assert!(default_from_search(&buf[..20], 1).is_err());
        assert!(default_from_search(&buf[..40], 1).is_err());
        Ok(())
    }

    #[test]
    fn boot_record_binds_exact_subvolume_options() -> anyhow::Result<()> {
        let record = BtrfsRecord {
            fsid: "ebcd29cb-f4ac-43ef-8f65-f7975a22d024".into(),
            uuid: "82ec1cf9-8e6f-4b7a-afb5-d3b171a2b4a2".into(),
            subvolid: 277,
            path: "@environments/trial/root".into(),
        };
        let text = record.render();
        let options = "root=PARTUUID=7adff456-af50-498f-b39a-dcf87cb4a7c1 rootfstype=btrfs rootflags=subvolid=277 rootwait";
        assert_eq!(BtrfsRecord::parse(&text, options)?, Some(record));
        for invalid in [
            options.replace("subvolid=277", "subvolid=278"),
            format!("{options} rootflags=subvolid=278"),
            format!("{options} rootfstype=ext4"),
        ] {
            assert!(BtrfsRecord::parse(&text, &invalid).is_err());
        }
        for invalid in [
            format!("{text}# cps-btrfs-subvolid 278\n"),
            text.replace("@environments/trial/root", "../trial"),
            text.replace("277", "0277"),
            text.replace("# cps-btrfs-uuid ", "# removed "),
        ] {
            assert!(BtrfsRecord::parse(&invalid, options).is_err());
        }
        Ok(())
    }

    #[test]
    fn ext4_entries_have_no_btrfs_record() -> anyhow::Result<()> {
        assert!(BtrfsRecord::parse("title Canopa\n", "root=PARTUUID=old")?.is_none());
        assert!(BtrfsRecord::parse("# cps-btrfs-subvolid 277\n", "").is_err());
        Ok(())
    }
}
