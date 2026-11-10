use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{
        AtFlags, FallocateFlags, FlockOperation, Mode, OFlags, RenameFlags, SeekFrom as Whence,
        fallocate, flock, open, renameat_with, seek, statat,
    },
    io::Errno,
};
use serde::Deserialize;
use tempfile::TempDir;

use super::{artifact_helper, systemd_boot::canonical_partuuid};
use crate::{
    image::root::Root,
    model::{NIL_UUID, digest_reader},
};

const DISK: &str = "disk.raw";
/// Sector size of offline disk images, and the unit of sysfs block extents.
pub(super) const SECTOR_SIZE: u64 = 512;
const LINUX_FILESYSTEM: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
const EFI_SYSTEM: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// A private clone of an offline GPT disk, published under a new name once checked.
pub(super) struct DiskStage<'a> {
    pub disk: OfflineDisk,
    stage: Root,
    _temp: TempDir,
    parent: Root,
    name: &'a std::ffi::OsStr,
    bytes: u64,
}

impl<'a> DiskStage<'a> {
    pub fn new(source: &Path, output: &'a Path) -> Result<Self> {
        let name = output
            .file_name()
            .context("expected a new disk file name")?;
        let parent_path = std::fs::canonicalize(
            output
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .context("opening the disk destination directory")?;
        let parent = Root::open(&parent_path)?;
        match statat(parent.as_file(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => anyhow::bail!("the disk destination already exists"),
            Err(Errno::NOENT) => {}
            Err(error) => return Err(error).context("checking the disk destination"),
        }
        let source = offline_file(source)?;
        let temp = tempfile::Builder::new()
            .prefix(".cps-disk-")
            .tempdir_in(Root::fd_path(parent.as_file()))?;
        let stage = Root::open(temp.path())?;
        let directory = Root::fd_path(stage.as_file());
        let clone = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(DISK))?;
        let status = artifact_helper("cp", &directory)
            .stdin(Stdio::from(source.try_clone()?))
            .args([
                "--reflink=auto",
                "--sparse=always",
                "--",
                "/proc/self/fd/0",
                DISK,
            ])
            .status()
            .context("cloning the offline disk with GNU cp")?;
        ensure!(
            status.success(),
            "cloning the offline disk failed with {status}"
        );
        let bytes = source.metadata()?.len();
        ensure!(
            clone.metadata()?.len() == bytes,
            "offline disk size changed during cloning"
        );
        Ok(Self {
            disk: OfflineDisk::from_file(clone)?,
            stage,
            _temp: temp,
            parent,
            name,
            bytes,
        })
    }

    pub fn directory(&self) -> PathBuf {
        Root::fd_path(self.stage.as_file())
    }

    pub fn publish(self) -> Result<()> {
        let checked = self.disk.file.metadata()?;
        let published = self.stage.file(DISK)?.metadata()?;
        ensure!(
            (checked.dev(), checked.ino(), self.bytes)
                == (published.dev(), published.ino(), published.len()),
            "the verified disk image was replaced or resized"
        );
        self.disk.file.sync_all()?;
        renameat_with(
            self.stage.as_file(),
            DISK,
            self.parent.as_file(),
            self.name,
            RenameFlags::NOREPLACE,
        )
        .context("publishing the disk image")?;
        self.parent.as_file().sync_all()?;
        Ok(())
    }
}

pub(super) fn offline_file(path: &Path) -> Result<File> {
    let pinned = File::from(
        open(
            path,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("opening offline input {}", path.display()))?,
    );
    ensure!(
        pinned.metadata()?.is_file(),
        "{} must be an offline regular file",
        path.display()
    );
    let file = File::open(Root::fd_path(&pinned))?;
    flock(&file, FlockOperation::LockShared)?;
    Ok(file)
}

#[derive(Deserialize)]
struct DiskTable {
    partitiontable: Gpt,
}

#[derive(Deserialize)]
struct Gpt {
    label: String,
    unit: String,
    sectorsize: u64,
    firstlba: u64,
    lastlba: u64,
    partitions: Vec<Partition>,
}

#[derive(Deserialize)]
struct Partition {
    start: u64,
    size: u64,
    #[serde(rename = "type")]
    kind: String,
    uuid: String,
}

pub(super) struct PartitionExtent {
    offset: u64,
    pub bytes: u64,
    kind: String,
}

pub(super) struct OfflineDisk {
    file: File,
    sector: u64,
    partitions: BTreeMap<String, PartitionExtent>,
}

impl OfflineDisk {
    fn from_file(file: File) -> Result<Self> {
        let bytes = file.metadata()?.len();
        Self::from_sized_file(file, bytes)
    }

    /// Read the GPT using the device's logical block size, or 512 bytes for an image file.
    pub fn from_sized_file(file: File, bytes: u64) -> Result<Self> {
        let sector = logical_sector_size(&file)?;
        let partitions = read_partitions(&file, bytes, sector)?;
        Ok(Self {
            file,
            sector,
            partitions,
        })
    }

    /// The logical block size used for GPT LBAs on this disk.
    pub fn sector_size(&self) -> u64 {
        self.sector
    }

    fn partition(&self, partuuid: &str, kind: &str) -> Result<&PartitionExtent> {
        let partition = self
            .partitions
            .get(partuuid)
            .with_context(|| format!("GPT partition {partuuid} was not found"))?;
        ensure!(
            partition.kind.eq_ignore_ascii_case(kind),
            "target is not a GPT {} partition",
            if kind == EFI_SYSTEM {
                "EFI System"
            } else {
                "Linux filesystem"
            }
        );
        Ok(partition)
    }

    /// The disk's only EFI System Partition.
    pub fn esp(&self) -> Result<&PartitionExtent> {
        let mut found = self
            .partitions
            .values()
            .filter(|part| part.kind.eq_ignore_ascii_case(EFI_SYSTEM));
        let esp = found
            .next()
            .context("the disk has no GPT EFI System Partition")?;
        ensure!(
            found.next().is_none(),
            "the disk has more than one GPT EFI System Partition"
        );
        Ok(esp)
    }

    /// The Linux filesystem partition that will hold the root.
    pub fn root(&self, partuuid: &str) -> Result<&PartitionExtent> {
        self.partition(&canonical_partuuid(partuuid)?, LINUX_FILESYSTEM)
    }

    pub fn check_device_extents(
        &self,
        root: &str,
        target: (u64, u64),
        esp: (u64, u64),
    ) -> Result<()> {
        let part = self.partition(root, LINUX_FILESYSTEM)?;
        ensure!(
            (part.offset, part.bytes) == target,
            "target device does not match GPT root PARTUUID {root}"
        );
        self.check_esp_extent(esp)
    }

    pub fn check_esp_extent(&self, esp: (u64, u64)) -> Result<()> {
        self.esp_uuid_at(esp).map(|_| ())
    }

    pub fn esp_uuid_at(&self, esp: (u64, u64)) -> Result<&str> {
        self.partitions
            .iter()
            .find_map(|(uuid, part)| {
                (part.kind.eq_ignore_ascii_case(EFI_SYSTEM) && (part.offset, part.bytes) == esp)
                    .then_some(uuid.as_str())
            })
            .context("mounted filesystem does not match a GPT EFI System Partition")
    }

    pub fn read_partition(&self, part: &PartitionExtent, output: &mut File) -> Result<()> {
        let mut disk = &self.file;
        disk.seek(SeekFrom::Start(part.offset))?;
        let read = io::copy(&mut disk.take(part.bytes), output)?;
        ensure!(
            read == part.bytes,
            "disk partition was truncated while reading"
        );
        Ok(())
    }

    pub fn write_partition(&self, part: &PartitionExtent, payload: &File) -> Result<String> {
        write_extent(&self.file, part.offset, part.bytes, payload)
    }
}

/// Write a payload of exactly `bytes` at `offset`, keeping its holes sparse,
/// and return the SHA-256 of the written extent.
pub(super) fn write_extent(disk: &File, offset: u64, bytes: u64, payload: &File) -> Result<String> {
    ensure!(
        payload.metadata()?.len() == bytes,
        "the payload size differs from its partition"
    );
    // Zero the old extent first. Payload holes then need no writes.
    let sparse = match fallocate(
        disk,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        offset,
        bytes,
    ) {
        Ok(()) => true,
        Err(Errno::OPNOTSUPP) => false,
        Err(error) => return Err(error).context("clearing the target partition"),
    };
    let mut buffer = vec![0; 1024 * 1024];
    let mut position = 0;
    while position < bytes {
        let (start, end) = if sparse {
            let start = match seek(payload, Whence::Data(position)) {
                Ok(start) => start.min(bytes),
                Err(Errno::NXIO) => bytes,
                Err(error) => return Err(error).context("reading the partition payload"),
            };
            let end = seek(payload, Whence::Hole(start))
                .context("reading the partition payload")?
                .min(bytes);
            (start, end)
        } else {
            (position, bytes)
        };
        let mut cursor = start;
        while cursor < end {
            let len = buffer.len().min(usize::try_from(end - cursor)?);
            payload.read_exact_at(&mut buffer[..len], cursor)?;
            disk.write_all_at(&buffer[..len], offset + cursor)?;
            cursor += len as u64;
        }
        position = end.max(start);
        if start == bytes {
            break;
        }
    }
    disk.sync_all()?;
    digest_extent(disk, offset, bytes)
}

pub(super) fn digest_extent(mut disk: &File, offset: u64, bytes: u64) -> Result<String> {
    disk.seek(SeekFrom::Start(offset))?;
    let mut extent = disk.take(bytes);
    let sha256 = digest_reader(&mut extent)?;
    ensure!(
        extent.limit() == 0,
        "disk partition was truncated while reading"
    );
    Ok(sha256)
}

fn logical_sector_size(file: &File) -> Result<u64> {
    if !file.metadata()?.file_type().is_block_device() {
        return Ok(SECTOR_SIZE);
    }
    let size = u64::from(
        rustix::fs::ioctl_blksszget(file).context("reading the disk's logical block size")?,
    );
    ensure!(
        size.is_power_of_two() && (512..=65536).contains(&size),
        "unsupported logical block size {size}"
    );
    Ok(size)
}

fn read_partitions(
    disk: &File,
    disk_bytes: u64,
    sector: u64,
) -> Result<BTreeMap<String, PartitionExtent>> {
    let output = sfdisk(disk, "--json", sector)?;
    let mut table = serde_json::from_slice::<DiskTable>(&output)
        .context("expected a GPT partition table from sfdisk")?
        .partitiontable;
    ensure!(
        table.label == "gpt" && table.unit == "sectors" && table.sectorsize == sector,
        "expected a GPT disk with {sector}-byte logical sectors"
    );
    ensure!(
        disk_bytes.is_multiple_of(sector)
            && table.firstlba >= 2
            && table.firstlba <= table.lastlba
            && table.lastlba < (disk_bytes / sector).saturating_sub(1),
        "invalid GPT usable sector range"
    );
    sfdisk(disk, "--verify", sector)?;
    check_gpt_copies(disk, disk_bytes / sector - 1, &table, sector)?;

    table.partitions.sort_unstable_by_key(|part| part.start);
    let mut next = table.firstlba;
    let mut partitions = BTreeMap::new();
    for part in table.partitions {
        let uuid = canonical_partuuid(&part.uuid)?;
        let Entry::Vacant(entry) = partitions.entry(uuid) else {
            anyhow::bail!("GPT contains a duplicate partition UUID");
        };
        ensure!(
            entry.key() != NIL_UUID,
            "GPT contains a missing partition UUID"
        );
        let end = part
            .start
            .checked_add(part.size)
            .context("GPT partition overflows")?;
        ensure!(
            part.size > 0 && part.start >= next && end <= table.lastlba + 1,
            "GPT contains overlapping or out-of-range partitions"
        );
        next = end;
        entry.insert(PartitionExtent {
            offset: part.start * sector,
            bytes: part.size * sector,
            kind: part.kind,
        });
    }
    Ok(partitions)
}

fn check_gpt_copies(mut disk: &File, backup_lba: u64, table: &Gpt, sector: u64) -> Result<()> {
    // libfdisk can accept two independently valid but disagreeing tables.
    // Compare their shared geometry and the entry arrays before writing.
    let mut primary = [0; 92];
    let mut backup = [0; 92];
    disk.read_exact_at(&mut primary, sector)?;
    disk.read_exact_at(&mut backup, backup_lba * sector)?;
    let lba = |header: &[u8], offset: usize| {
        u64::from_le_bytes(header[offset..offset + 8].try_into().unwrap())
    };
    ensure!(
        &primary[..8] == b"EFI PART"
            && primary[..16] == backup[..16]
            && primary[40..72] == backup[40..72]
            && primary[80..92] == backup[80..92]
            && lba(&primary, 24) == 1
            && lba(&primary, 32) == backup_lba
            && lba(&backup, 24) == backup_lba
            && lba(&backup, 32) == 1
            && lba(&primary, 40) == table.firstlba
            && lba(&primary, 48) == table.lastlba,
        "primary and backup GPT headers disagree"
    );
    let count = u32::from_le_bytes(primary[80..84].try_into().unwrap());
    let entry_size = u32::from_le_bytes(primary[84..88].try_into().unwrap());
    let array_bytes = u64::from(count) * u64::from(entry_size);
    let array_sectors = array_bytes.div_ceil(sector);
    let first = lba(&primary, 72);
    let last = lba(&backup, 72);
    ensure!(
        array_bytes > 0
            && first >= 2
            && first <= table.firstlba
            && array_sectors <= table.firstlba - first
            && last > table.lastlba
            && last <= backup_lba
            && array_sectors <= backup_lba - last,
        "invalid GPT entry array extent"
    );
    disk.seek(SeekFrom::Start(first * sector))?;
    let primary = digest_reader((&mut disk).take(array_bytes))?;
    disk.seek(SeekFrom::Start(last * sector))?;
    let backup = digest_reader(disk.take(array_bytes))?;
    ensure!(
        primary == backup,
        "primary and backup GPT entry arrays disagree"
    );
    Ok(())
}

fn sfdisk(disk: &File, action: &str, sector: u64) -> Result<Vec<u8>> {
    let mut command = artifact_helper("sfdisk", Path::new("/"));
    command.stdin(Stdio::from(disk.try_clone()?));
    if sector != SECTOR_SIZE {
        // libfdisk reads the size of a block device itself. An image file needs it stated.
        command.arg(format!("--sector-size={sector}"));
    }
    let output = command
        .args([action, "/proc/self/fd/0"])
        .output()
        .with_context(|| format!("reading the offline disk with sfdisk {action}"))?;
    // libfdisk can recover from a damaged GPT copy while returning success.
    // Reject repair warnings because the firmware reads the original bytes.
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "sfdisk {action} rejected the disk ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpt_image(dir: &Path, sector: u64) -> Result<File> {
        let path = dir.join(format!("disk-{sector}.raw"));
        let file = File::create(&path)?;
        file.set_len(16 * 1024 * 1024)?;
        drop(file);
        let mut child = std::process::Command::new("sfdisk")
            .arg("--quiet")
            .arg(format!("--sector-size={sector}"))
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .context("running sfdisk")?;
        let esp_start = 1024 * 1024 / sector;
        let size = 4 * 1024 * 1024 / sector;
        let script = format!(
            "label: gpt\nstart={esp_start},size={size},type=uefi,uuid=9a138d0b-f9aa-4a2a-b527-93392ad7e812\nstart={},size={size},type=linux,uuid=7adff456-af50-498f-b39a-dcf87cb4a7c1\n",
            esp_start + size
        );
        std::io::Write::write_all(
            child.stdin.as_mut().context("sfdisk stdin")?,
            script.as_bytes(),
        )?;
        ensure!(child.wait()?.success(), "sfdisk failed");
        Ok(File::open(path)?)
    }

    #[test]
    fn gpt_offsets_follow_the_logical_sector_size() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for sector in [512, 4096] {
            let disk = gpt_image(dir.path(), sector)?;
            let bytes = disk.metadata()?.len();
            let partitions = read_partitions(&disk, bytes, sector)?;
            let esp = &partitions["9a138d0b-f9aa-4a2a-b527-93392ad7e812"];
            let root = &partitions["7adff456-af50-498f-b39a-dcf87cb4a7c1"];
            assert_eq!((esp.offset, esp.bytes), (1024 * 1024, 4 * 1024 * 1024));
            assert_eq!(
                (root.offset, root.bytes),
                (5 * 1024 * 1024, 4 * 1024 * 1024)
            );
            let other = if sector == 512 { 4096 } else { 512 };
            assert!(read_partitions(&disk, bytes, other).is_err());
        }
        // An image file is read with 512-byte sectors unless it is a block device.
        let disk = gpt_image(dir.path(), 512)?;
        assert_eq!(logical_sector_size(&disk)?, SECTOR_SIZE);
        Ok(())
    }
}
