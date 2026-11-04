use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::Path,
    process::Stdio,
};

use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{AtFlags, FlockOperation, Mode, OFlags, RenameFlags, flock, open, renameat_with, statat},
    io::Errno,
};
use serde::Deserialize;

use super::{artifact_helper, systemd_boot::canonical_partuuid};
use crate::{image::root::Root, model::digest_reader};

const DISK: &str = "disk.raw";
const SECTOR_SIZE: u64 = 512;
const LINUX_FILESYSTEM: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

/// Deploy an exported root to one partition in a new offline GPT disk image.
pub fn deploy_boot_environment(
    disk: &Path,
    root_image: &Path,
    root_partuuid: &str,
    output: &Path,
) -> Result<String> {
    let partuuid = canonical_partuuid(root_partuuid)?;
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

    let source = offline_file(disk)?;
    let mut payload = offline_file(root_image)?;
    let temp = tempfile::Builder::new()
        .prefix(".cps-deploy-")
        .tempdir_in(Root::fd_path(parent.as_file()))?;
    let stage = Root::open(temp.path())?;
    let stage_path = Root::fd_path(stage.as_file());
    let mut clone = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage_path.join(DISK))?;
    let status = artifact_helper("cp", &stage_path)
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

    let (offset, bytes) = partition_extent(&clone, &stage_path, &partuuid)?;
    ensure!(
        payload.metadata()?.len() == bytes,
        "root payload size must match the target partition ({bytes} bytes)"
    );
    let expected = digest_reader((&mut payload).take(bytes))?;
    let status = artifact_helper("e2fsck", &stage_path)
        .stdin(Stdio::from(payload.try_clone()?))
        .stdout(Stdio::null())
        .args(["-f", "-n", "/proc/self/fd/0"])
        .status()
        .context("checking the root payload with e2fsck")?;
    ensure!(status.success(), "e2fsck failed with {status}");
    payload.rewind()?;
    clone.seek(SeekFrom::Start(offset))?;
    let written = io::copy(&mut (&mut payload).take(bytes), &mut clone)?;
    ensure!(
        written == bytes,
        "root payload was truncated during deployment"
    );
    clone.sync_all()?;
    clone.seek(SeekFrom::Start(offset))?;
    let actual = digest_reader((&mut clone).take(bytes))?;
    ensure!(
        actual == expected && payload.metadata()?.len() == bytes,
        "deployed partition does not match the checked root payload"
    );
    let checked = clone.metadata()?;
    let published = stage.file(DISK)?.metadata()?;
    ensure!(
        (checked.dev(), checked.ino(), checked.len())
            == (published.dev(), published.ino(), published.len()),
        "the verified disk image was replaced or resized"
    );
    renameat_with(
        stage.as_file(),
        DISK,
        parent.as_file(),
        name,
        RenameFlags::NOREPLACE,
    )
    .context("publishing the deployed disk image")?;
    parent.as_file().sync_all()?;
    Ok(actual)
}

fn offline_file(path: &Path) -> Result<File> {
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

fn partition_extent(disk: &File, directory: &Path, partuuid: &str) -> Result<(u64, u64)> {
    let output = sfdisk(disk, directory, "--json")?;
    let mut table = serde_json::from_slice::<DiskTable>(&output)
        .context("expected a GPT partition table from sfdisk")?
        .partitiontable;
    ensure!(
        table.label == "gpt" && table.unit == "sectors" && table.sectorsize == SECTOR_SIZE,
        "expected a GPT disk image with 512-byte sectors"
    );
    let disk_bytes = disk.metadata()?.len();
    ensure!(
        disk_bytes.is_multiple_of(SECTOR_SIZE)
            && table.firstlba >= 2
            && table.firstlba <= table.lastlba
            && table.lastlba < (disk_bytes / SECTOR_SIZE).saturating_sub(1),
        "invalid GPT usable sector range"
    );
    sfdisk(disk, directory, "--verify")?;
    check_gpt_copies(disk, disk_bytes / SECTOR_SIZE - 1, &table)?;

    table.partitions.sort_unstable_by_key(|part| part.start);
    let mut next = table.firstlba;
    let mut seen = BTreeSet::new();
    let mut target = None;
    for part in table.partitions {
        let uuid = canonical_partuuid(&part.uuid)?;
        ensure!(
            uuid != NIL_UUID && seen.insert(uuid.clone()),
            "GPT contains a missing or duplicate partition UUID"
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
        if uuid == partuuid {
            ensure!(
                part.kind.eq_ignore_ascii_case(LINUX_FILESYSTEM),
                "target is not a GPT Linux filesystem partition"
            );
            target = Some((part.start * SECTOR_SIZE, part.size * SECTOR_SIZE));
        }
    }
    target.with_context(|| format!("GPT partition {partuuid} was not found"))
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

fn sfdisk(disk: &File, directory: &Path, action: &str) -> Result<Vec<u8>> {
    let output = artifact_helper("sfdisk", directory)
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
