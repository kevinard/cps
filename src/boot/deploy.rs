use std::{
    collections::{BTreeMap, btree_map::Entry},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, FlockOperation, Mode, OFlags, RenameFlags, flock, open, renameat_with, statat},
    io::Errno,
};
use serde::Deserialize;
use tempfile::TempDir;

use super::{artifact_helper, root_label, systemd_boot::canonical_partuuid};
use crate::{
    image::root::Root,
    kdl_format::boot_export::ExportReceipt,
    model::{NIL_UUID, digest_reader},
};

const DISK: &str = "disk.raw";
pub(super) const SECTOR_SIZE: u64 = 512;
const LINUX_FILESYSTEM: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
const EFI_SYSTEM: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";

/// Deploy an exported root to one partition in a new offline GPT disk image.
pub fn deploy_boot_environment(
    disk: &Path,
    root_image: &Path,
    root_partuuid: &str,
    output: &Path,
) -> Result<String> {
    let partuuid = canonical_partuuid(root_partuuid)?;
    let stage = DiskStage::new(disk, output)?;
    let actual = stage.disk.write_root(&partuuid, root_image, None)?;
    stage.publish()?;
    Ok(actual)
}

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
        let parent_path = fs::canonicalize(
            output
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .context("opening the deployment destination directory")?;
        let parent = Root::open(&parent_path)?;
        match statat(parent.as_file(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => anyhow::bail!("the deployment destination already exists"),
            Err(Errno::NOENT) => {}
            Err(error) => return Err(error).context("checking the deployment destination"),
        }
        let source = offline_file(source)?;
        let temp = tempfile::Builder::new()
            .prefix(".cps-deploy-")
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
        .context("publishing the deployed disk image")?;
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
    partitions: BTreeMap<String, PartitionExtent>,
}

impl OfflineDisk {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_file(offline_file(path)?)
    }

    fn from_file(file: File) -> Result<Self> {
        let bytes = file.metadata()?.len();
        Self::from_sized_file(file, bytes)
    }

    pub fn from_sized_file(file: File, bytes: u64) -> Result<Self> {
        let partitions = read_partitions(&file, bytes)?;
        Ok(Self { file, partitions })
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

    pub fn esp(&self, partuuid: &str) -> Result<&PartitionExtent> {
        self.partition(&canonical_partuuid(partuuid)?, EFI_SYSTEM)
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
        ensure!(
            self.partitions
                .values()
                .any(|part| part.kind.eq_ignore_ascii_case(EFI_SYSTEM)
                    && (part.offset, part.bytes) == esp),
            "mounted filesystem does not match a GPT EFI System Partition"
        );
        Ok(())
    }

    pub fn write_root(
        &self,
        partuuid: &str,
        root_image: &Path,
        receipt: Option<&ExportReceipt>,
    ) -> Result<String> {
        let partition = self.partition(partuuid, LINUX_FILESYSTEM)?;
        let (mut payload, expected) = checked_payload(root_image, partition.bytes, receipt)?;
        let actual = self.write_partition(partition, &mut payload)?;
        ensure!(
            actual == expected && payload.metadata()?.len() == partition.bytes,
            "deployed partition does not match the checked root payload"
        );
        Ok(actual)
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

    pub fn write_partition(&self, part: &PartitionExtent, payload: &mut File) -> Result<String> {
        write_extent(&self.file, part.offset, part.bytes, payload)
    }

    pub fn verify_payload(&self, partuuid: &str, bytes: u64, sha256: &str) -> Result<()> {
        let partition = self.partition(partuuid, LINUX_FILESYSTEM)?;
        ensure!(
            partition.bytes == bytes
                && digest_extent(&self.file, partition.offset, partition.bytes)? == sha256,
            "deployed partition {partuuid} does not match the root export receipt"
        );
        Ok(())
    }

    pub fn verify_filesystem(&self, partuuid: &str, image: &str, revision: u64) -> Result<()> {
        let partition = self.partition(partuuid, LINUX_FILESYSTEM)?;
        verify_filesystem_at(
            &self.file,
            partition.offset,
            partition.bytes,
            partuuid,
            image,
            revision,
        )
    }
}

pub(super) fn checked_payload(
    root_image: &Path,
    bytes: u64,
    receipt: Option<&ExportReceipt>,
) -> Result<(File, String)> {
    let mut payload = offline_file(root_image)?;
    ensure!(
        payload.metadata()?.len() == bytes,
        "root payload size must match the target partition ({bytes} bytes)"
    );
    let expected = digest_extent(&payload, 0, bytes)?;
    if let Some(receipt) = receipt {
        ensure!(
            receipt.bytes == bytes && receipt.sha256 == expected,
            "root payload does not match the root export receipt"
        );
    }
    let status = artifact_helper("e2fsck", Path::new("/"))
        .stdin(Stdio::from(payload.try_clone()?))
        .stdout(Stdio::null())
        .args(["-f", "-n", "/proc/self/fd/0"])
        .status()
        .context("checking the root payload with e2fsck")?;
    ensure!(status.success(), "e2fsck failed with {status}");
    payload.rewind()?;
    Ok((payload, expected))
}

pub(super) fn write_extent(
    mut disk: &File,
    offset: u64,
    bytes: u64,
    payload: &mut File,
) -> Result<String> {
    disk.seek(SeekFrom::Start(offset))?;
    let written = io::copy(&mut payload.take(bytes), &mut disk)?;
    ensure!(
        written == bytes,
        "partition payload was truncated during deployment"
    );
    disk.sync_all()?;
    digest_extent(disk, offset, bytes)
}

pub(super) fn verify_filesystem_at(
    disk: &File,
    offset: u64,
    bytes: u64,
    partuuid: &str,
    image: &str,
    revision: u64,
) -> Result<()> {
    let output = artifact_helper("blkid", Path::new("/"))
        .env("BLKID_CONF", "/dev/null")
        .stdin(Stdio::from(disk.try_clone()?))
        .args(["--probe", "--offset", &offset.to_string()])
        .args(["--size", &bytes.to_string()])
        .args([
            "--output",
            "export",
            "--match-tag",
            "UUID",
            "--match-tag",
            "TYPE",
            "--match-tag",
            "LABEL",
        ])
        .arg("/proc/self/fd/0")
        .output()
        .context("probing the deployed filesystem with blkid")?;
    let tags = std::str::from_utf8(&output.stdout)?;
    let uuid = format!("UUID={image}");
    let label = format!("LABEL={}", root_label(revision));
    ensure!(
        output.status.success()
            && tags.lines().any(|line| line == "TYPE=ext4")
            && tags.lines().any(|line| line == uuid)
            && tags.lines().any(|line| line == label),
        "partition {partuuid} does not contain the expected ext4 filesystem {image} at revision {revision}"
    );
    Ok(())
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

fn read_partitions(disk: &File, disk_bytes: u64) -> Result<BTreeMap<String, PartitionExtent>> {
    let output = sfdisk(disk, "--json")?;
    let mut table = serde_json::from_slice::<DiskTable>(&output)
        .context("expected a GPT partition table from sfdisk")?
        .partitiontable;
    ensure!(
        table.label == "gpt" && table.unit == "sectors" && table.sectorsize == SECTOR_SIZE,
        "expected a GPT disk with 512-byte sectors"
    );
    ensure!(
        disk_bytes.is_multiple_of(SECTOR_SIZE)
            && table.firstlba >= 2
            && table.firstlba <= table.lastlba
            && table.lastlba < (disk_bytes / SECTOR_SIZE).saturating_sub(1),
        "invalid GPT usable sector range"
    );
    sfdisk(disk, "--verify")?;
    check_gpt_copies(disk, disk_bytes / SECTOR_SIZE - 1, &table)?;

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
            offset: part.start * SECTOR_SIZE,
            bytes: part.size * SECTOR_SIZE,
            kind: part.kind,
        });
    }
    Ok(partitions)
}

fn check_gpt_copies(mut disk: &File, backup_lba: u64, table: &Gpt) -> Result<()> {
    // libfdisk can accept two independently valid but disagreeing tables.
    // Compare their shared geometry and the entry arrays before writing.
    let mut primary = [0; 92];
    let mut backup = [0; 92];
    disk.read_exact_at(&mut primary, SECTOR_SIZE)?;
    disk.read_exact_at(&mut backup, backup_lba * SECTOR_SIZE)?;
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
    let array_sectors = array_bytes.div_ceil(SECTOR_SIZE);
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
    disk.seek(SeekFrom::Start(first * SECTOR_SIZE))?;
    let primary = digest_reader((&mut disk).take(array_bytes))?;
    disk.seek(SeekFrom::Start(last * SECTOR_SIZE))?;
    let backup = digest_reader(disk.take(array_bytes))?;
    ensure!(
        primary == backup,
        "primary and backup GPT entry arrays disagree"
    );
    Ok(())
}

fn sfdisk(disk: &File, action: &str) -> Result<Vec<u8>> {
    let output = artifact_helper("sfdisk", Path::new("/"))
        .stdin(Stdio::from(disk.try_clone()?))
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
