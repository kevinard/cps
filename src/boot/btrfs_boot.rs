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
    device::mounted_root_disk,
    systemd_boot::{self, canonical_partuuid, policy::field},
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
            .filter(|arg| arg.starts_with("rootflags="))
            .collect();
        let types: Vec<_> = options
            .split_ascii_whitespace()
            .filter(|arg| arg.starts_with("rootfstype="))
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
}

pub(super) struct BtrfsVolume {
    root: Root,
    path: PathBuf,
    fsid: String,
    device: PathBuf,
}

impl BtrfsVolume {
    pub fn open(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path).context("opening the Btrfs top-level volume")?;
        let root = Root::open(&path)?;
        ensure!(
            subvolume_info(&root)?.treeid == 5,
            "--btrfs-volume must expose the top-level subvolume (ID 5)"
        );
        let fs = filesystem_info(&root)?;
        ensure!(
            fs.num_devices == 1,
            "Btrfs boot activation requires a single-device filesystem"
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
        unsafe { ioctl(root.as_file(), Updater::<DEVICE, _>::new(&mut device)) }
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
        Ok(Self {
            root,
            path,
            fsid: uuid_string(&fs.fsid),
            device,
        })
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

    pub fn check_disk(&self, esp: &Path, locked: &File, partuuid: &str) -> Result<()> {
        mounted_root_disk(esp, locked, &self.device, partuuid)?;
        Ok(())
    }

    pub fn verify(&self, record: &BtrfsRecord, image_id: &str, revision: u64) -> Result<Image> {
        let root = self.open_registered(record, &super::entry_id(image_id, revision))?;
        let image = Image::open_query_root(root)?;
        let settings = image.settings()?;
        ensure!(
            settings.id == image_id && settings.revision == revision,
            "Btrfs root image or revision differs from its boot entry"
        );
        check_image_integrity(&image)?;
        Ok(image)
    }

    pub fn open_registered(&self, record: &BtrfsRecord, id: &str) -> Result<Root> {
        ensure!(
            record.fsid == self.fsid,
            "the boot entry belongs to a different Btrfs filesystem"
        );
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

    pub fn running_parent(&self) -> Result<Option<String>> {
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
        ensure!(
            registered == super::entry_id(&settings.id, settings.revision).as_bytes(),
            "the running root's boot registration differs from its image identity"
        );
        settings
            .boot_parent
            .map(|parent| {
                Ok(super::entry_id(
                    &canonical_partuuid(&parent.id)?,
                    parent.revision,
                ))
            })
            .transpose()
    }
}

pub fn activate_btrfs_boot_environment(
    root: &Path,
    volume: &Path,
    esp: &Path,
    activation: &Activation,
) -> Result<String> {
    let volume = BtrfsVolume::open(volume)?;
    Ok(systemd_boot::activate(root, esp, activation, None, Some(&volume))?.id)
}

pub fn rollback_btrfs_boot_environment(
    esp: &Path,
    volume: &Path,
    id: Option<&str>,
) -> Result<String> {
    let volume = BtrfsVolume::open(volume)?;
    systemd_boot::rollback(esp, id, None, Some(&volume))
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
    use super::BtrfsRecord;

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
    fn legacy_entries_have_no_btrfs_record() -> anyhow::Result<()> {
        assert!(BtrfsRecord::parse("title Canopa\n", "root=PARTUUID=old")?.is_none());
        assert!(BtrfsRecord::parse("# cps-btrfs-subvolid 277\n", "").is_err());
        Ok(())
    }
}
