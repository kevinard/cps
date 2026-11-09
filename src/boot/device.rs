use std::{
    fs::{self, File},
    io::Seek,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, fstatfs, open};

use crate::{image::root::Root, kdl_format::boot_export::ExportReceipt, model::validate_path};

use super::deploy::{
    OfflineDisk, SECTOR_SIZE, checked_payload, digest_extent, verify_filesystem_at, write_extent,
};

pub(super) fn is_fat(file: &File) -> Result<bool> {
    Ok(fstatfs(file)?.f_type == libc::MSDOS_SUPER_MAGIC)
}

pub(super) fn mounted_esp_disk(path: &Path, locked: &File) -> Result<Option<OfflineDisk>> {
    if !is_fat(locked)? {
        return Ok(None);
    }
    let esp = esp_partition(path, locked)?;
    let disk = esp.open_disk()?;
    disk.check_esp_extent(esp.extent)?;
    Ok(Some(disk))
}

pub(super) fn mounted_root_disk(
    esp: &Path,
    locked: &File,
    device: &Path,
    partuuid: &str,
) -> Result<OfflineDisk> {
    let esp_part = esp_partition(esp, locked)?;
    let target = block_node(device)?;
    let root_part = KernelPartition::read(target.metadata()?.rdev())?;
    ensure!(
        esp_part.disk == root_part.disk,
        "Btrfs root and mounted ESP must be on the same GPT disk"
    );
    let disk = esp_part.open_disk()?;
    disk.check_device_extents(partuuid, root_part.extent, esp_part.extent)?;
    Ok(disk)
}

pub(super) struct DevicePartition {
    file: File,
    bytes: u64,
    esp: File,
}

impl DevicePartition {
    pub fn open(
        device: &Path,
        esp: &Path,
        partuuid: &str,
        write: bool,
    ) -> Result<(OfflineDisk, Self)> {
        let esp = fs::canonicalize(esp).context("opening the mounted EFI partition")?;
        let mount = Root::open(&esp)?;
        let esp_part = esp_partition(&esp, mount.as_file())?;
        let target = block_node(device)?;
        let root_part = KernelPartition::read(target.metadata()?.rdev())
            .context("identifying the target root partition")?;
        ensure!(
            esp_part.disk == root_part.disk,
            "target root partition and mounted ESP must be on the same GPT disk"
        );
        // Claim only the target partition. The fallback and ESP stay accessible.
        let access = if write { OFlags::RDWR } else { OFlags::RDONLY };
        let file = File::from(
            open(
                Root::fd_path(&target),
                access | OFlags::EXCL | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .context("claiming the target root partition exclusively")?,
        );
        let disk = esp_part.open_disk()?;
        disk.check_device_extents(partuuid, root_part.extent, esp_part.extent)?;
        let target = Self {
            file,
            bytes: root_part.extent.1,
            esp: mount.as_file().try_clone()?,
        };
        Ok((disk, target))
    }

    pub fn check_esp(&self, path: &Path, locked: &File) -> Result<()> {
        let original = self.esp.metadata()?;
        let current = locked.metadata()?;
        ensure!(
            (original.dev(), original.ino()) == (current.dev(), current.ino()),
            "mounted ESP changed during activation"
        );
        check_esp_mount(path, locked)
    }

    pub fn prepare(&self, image: &Path, partuuid: &str, receipt: &ExportReceipt) -> Result<File> {
        let (mut payload, _) = checked_payload(image, self.bytes, Some(receipt))?;
        verify_filesystem_at(
            &payload,
            0,
            self.bytes,
            partuuid,
            &receipt.image,
            receipt.revision,
        )?;
        payload.rewind()?;
        Ok(payload)
    }

    pub fn write(&self, mut payload: File, partuuid: &str, receipt: &ExportReceipt) -> Result<()> {
        let actual = write_extent(&self.file, 0, self.bytes, &mut payload)?;
        ensure!(
            actual == receipt.sha256 && payload.metadata()?.len() == self.bytes,
            "deployed partition does not match the checked root payload"
        );
        self.verify_filesystem(partuuid, receipt)
    }

    pub fn verify(&self, partuuid: &str, receipt: &ExportReceipt) -> Result<()> {
        ensure!(
            receipt.bytes == self.bytes
                && digest_extent(&self.file, 0, self.bytes)? == receipt.sha256,
            "deployed partition {partuuid} does not match the root export receipt"
        );
        self.verify_filesystem(partuuid, receipt)
    }

    fn verify_filesystem(&self, partuuid: &str, receipt: &ExportReceipt) -> Result<()> {
        verify_filesystem_at(
            &self.file,
            0,
            self.bytes,
            partuuid,
            &receipt.image,
            receipt.revision,
        )
    }
}

struct KernelPartition {
    disk: PathBuf,
    number: u32,
    extent: (u64, u64),
}

impl KernelPartition {
    fn read(device: u64) -> Result<Self> {
        let name = device_number(device);
        let path = fs::canonicalize(Path::new("/sys/dev/block").join(&name))?;
        let number =
            u32::try_from(sysfs_number(&path, "partition")?).context("invalid partition number")?;
        ensure!(
            fs::read_to_string(path.join("dev"))?.trim() == name && number != 0,
            "expected a partition block device"
        );
        let disk = path.parent().context("missing parent disk")?.to_owned();
        let extent = (sectors(&path, "start")?, sectors(&path, "size")?);
        Ok(Self {
            disk,
            number,
            extent,
        })
    }

    fn open_disk(&self) -> Result<OfflineDisk> {
        OfflineDisk::from_sized_file(open_parent(&self.disk)?, sectors(&self.disk, "size")?)
    }
}

pub(super) struct EspDevicePath {
    pub number: u32,
    pub start: u64,
    pub sectors: u64,
    pub uuid: String,
}

pub(super) fn esp_device_path(path: &Path, file: &File) -> Result<EspDevicePath> {
    let esp = esp_partition(path, file)?;
    let disk = esp.open_disk()?;
    Ok(EspDevicePath {
        number: esp.number,
        // The UEFI hard drive device path counts the disk's logical blocks.
        start: esp.extent.0 / disk.sector_size(),
        sectors: esp.extent.1 / disk.sector_size(),
        uuid: disk.esp_uuid_at(esp.extent)?.to_owned(),
    })
}

fn esp_partition(path: &Path, file: &File) -> Result<KernelPartition> {
    check_esp_mount(path, file)?;
    KernelPartition::read(file.metadata()?.dev()).context("identifying the mounted EFI partition")
}

/// Sysfs reports block extents in 512-byte units, whatever the logical block size.
fn sectors(path: &Path, name: &str) -> Result<u64> {
    sysfs_number(path, name)?
        .checked_mul(SECTOR_SIZE)
        .context("block device extent overflows")
}

fn sysfs_number(path: &Path, name: &str) -> Result<u64> {
    Ok(fs::read_to_string(path.join(name))?.trim().parse()?)
}

fn device_number(device: u64) -> String {
    format!("{}:{}", libc::major(device), libc::minor(device))
}

fn block_node(path: &Path) -> Result<File> {
    let node = fs::canonicalize(path)
        .with_context(|| format!("opening block device {}", path.display()))?;
    let file = File::from(open(
        node,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    ensure!(
        file.metadata()?.file_type().is_block_device(),
        "{} is not a block device",
        path.display()
    );
    Ok(file)
}

fn open_parent(path: &Path) -> Result<File> {
    let event = fs::read_to_string(path.join("uevent"))?;
    let name = event
        .lines()
        .find_map(|line| line.strip_prefix("DEVNAME="))
        .context("missing parent block device name")?;
    validate_path(name)?;
    let node = block_node(&Path::new("/dev").join(name))?;
    ensure!(
        device_number(node.metadata()?.rdev()) == fs::read_to_string(path.join("dev"))?.trim(),
        "parent disk device differs from kernel metadata"
    );
    Ok(File::open(Root::fd_path(&node))?)
}

fn check_esp_mount(path: &Path, file: &File) -> Result<()> {
    let device = device_number(file.metadata()?.dev());
    let mut escaped = Vec::new();
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b' ' => escaped.extend(b"\\040"),
            b'\t' => escaped.extend(b"\\011"),
            b'\n' => escaped.extend(b"\\012"),
            b'\\' => escaped.extend(b"\\134"),
            byte => escaped.push(*byte),
        }
    }
    let mounts = fs::read("/proc/self/mountinfo").context("reading mounted filesystems")?;
    if esp_mount_state(&mounts, &escaped, device.as_bytes())? == EspMount::Automount {
        // An autofs trigger (systemd-gpt-auto or x-systemd.automount) mounts
        // the ESP on first access. Open fds keep it from expiring.
        drop(fs::read_dir(path).context("triggering the EFI partition automount")?);
        let mounts = fs::read("/proc/self/mountinfo").context("reading mounted filesystems")?;
        ensure!(
            esp_mount_state(&mounts, &escaped, device.as_bytes())? == EspMount::Mounted,
            "--esp names an automount point that did not mount the opened EFI partition"
        );
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum EspMount {
    Mounted,
    /// Only the autofs trigger is mounted at the path.
    Automount,
}

fn esp_mount_state(mounts: &[u8], escaped: &[u8], device: &[u8]) -> Result<EspMount> {
    let mut automount = None;
    let mut found = false;
    for line in mounts.split(|byte| *byte == b'\n') {
        let mut fields = line.split(|byte| *byte == b' ');
        let (Some(id), Some(parent), Some(number), Some(root), Some(mount), Some(options)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        if mount != escaped {
            continue;
        }
        let filesystem = fields.skip_while(|field| *field != b"-").nth(1);
        if filesystem == Some(b"autofs") && !found {
            ensure!(
                automount.replace(id).is_none(),
                "--esp names more than one automount point"
            );
            continue;
        }
        ensure!(
            !found
                && automount.is_none_or(|trigger| trigger == parent)
                && number == device
                && root == b"/"
                && options
                    .split(|byte| *byte == b',')
                    .any(|option| option == b"rw")
                && filesystem == Some(b"vfat"),
            "--esp must name the root of one writable VFAT mount"
        );
        found = true;
    }
    match (found, automount) {
        (true, _) => Ok(EspMount::Mounted),
        (false, Some(_)) => Ok(EspMount::Automount),
        (false, None) => anyhow::bail!("--esp must name the root of a mounted EFI partition"),
    }
}

#[cfg(test)]
mod tests {
    use super::{EspMount, esp_mount_state};

    const AUTOFS: &str =
        "40 1 0:39 / /efi rw,relatime shared:20 - autofs systemd-1 rw,fd=50,pgrp=1,direct\n";
    const VFAT: &str =
        "77 40 259:1 / /efi rw,relatime shared:40 - vfat /dev/nvme0n1p1 rw,fmask=0077\n";

    #[test]
    fn automounted_esp_uses_the_vfat_mount_stacked_on_its_trigger() -> anyhow::Result<()> {
        let state = |lines: &[&str]| esp_mount_state(lines.concat().as_bytes(), b"/efi", b"259:1");
        assert_eq!(state(&[AUTOFS, VFAT])?, EspMount::Mounted);
        assert_eq!(state(&[VFAT])?, EspMount::Mounted);
        assert_eq!(state(&[AUTOFS])?, EspMount::Automount);
        assert!(state(&[]).is_err());
        // A second filesystem over the ESP, or a VFAT mount beside the trigger.
        assert!(state(&[AUTOFS, VFAT, VFAT]).is_err());
        assert!(state(&[AUTOFS, &VFAT.replace("77 40", "77 41")]).is_err());
        assert!(state(&[AUTOFS, &VFAT.replace("259:1", "259:2")]).is_err());
        assert!(state(&[AUTOFS, &VFAT.replace("/efi rw", "/efi ro")]).is_err());
        assert!(state(&[AUTOFS, AUTOFS]).is_err());
        Ok(())
    }
}
