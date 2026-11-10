use std::{
    fs::{self, File, OpenOptions},
    io::Seek,
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::process::geteuid;

use super::{
    artifact_helper,
    disk::{OfflineDisk, PartitionExtent},
};
use crate::model::digest_reader;

const FAT: &str = "esp.fat";
const MAX_ESP_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const FAT32_BOOT_FLAGS: usize = 65;
const FAT_DIRTY: u8 = 0x01;
const FAT_EPOCH: u32 = 315532800; // 1980-01-01 00:00:00 UTC
/// Directory of trusted disk tools for unprivileged callers.
pub(super) const TOOLS_DIR: &str = "CPS_DISK_TOOLS_DIR";

/// A checked private copy of an offline EFI System Partition, and the files read from it.
pub(super) struct Esp {
    pub tree: PathBuf,
    work: PathBuf,
    epoch: Option<u32>,
}

struct FatEntry {
    name: String,
    directory: bool,
}

impl Esp {
    pub fn read(
        disk: &OfflineDisk,
        partition: &PartitionExtent,
        work: &Path,
        epoch: Option<u32>,
    ) -> Result<Self> {
        ensure!(
            partition.bytes <= MAX_ESP_BYTES,
            "EFI partition exceeds 1 GiB"
        );
        let tree = work.join("esp");
        fs::create_dir(&tree)?;
        let mut fat = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(work.join(FAT))?;
        disk.read_partition(partition, &mut fat)?;
        drop(fat);
        let esp = Self {
            work: work.to_owned(),
            tree,
            // FAT timestamps start in 1980 and have two-second resolution.
            epoch: epoch.map(|epoch| epoch.max(FAT_EPOCH) & !1),
        };
        esp.clear_fat32_dirty_flag()?;
        esp.check()?;
        esp.read_loader()?;
        Ok(esp)
    }

    fn clear_fat32_dirty_flag(&self) -> Result<()> {
        // A shutdown can leave the FAT32 dirty flag set with consistent
        // metadata. Change only that bit on the scratch image and still
        // require a full read-only filesystem check.
        let fat = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.work.join(FAT))?;
        let mut primary = [0; 512];
        fat.read_exact_at(&mut primary, 0)?;
        let field = |offset| u16::from_le_bytes([primary[offset], primary[offset + 1]]);
        let sector_bytes = u64::from(field(11));
        let backup_sector = field(50);
        let backup_offset = u64::from(backup_sector) * sector_bytes;
        if primary[510..] != [0x55, 0xaa]
            || field(17) != 0
            || field(22) != 0
            || primary[36..40] == [0; 4]
            || !(512..=4096).contains(&sector_bytes)
            || !sector_bytes.is_power_of_two()
            || backup_sector == 0
            || backup_sector >= field(14)
            || backup_offset + 512 > fat.metadata()?.len()
        {
            return Ok(());
        }
        let mut backup = [0; 512];
        fat.read_exact_at(&mut backup, backup_offset)?;
        let flags = [primary[FAT32_BOOT_FLAGS], backup[FAT32_BOOT_FLAGS]];
        primary[FAT32_BOOT_FLAGS] &= !FAT_DIRTY;
        backup[FAT32_BOOT_FLAGS] &= !FAT_DIRTY;
        if primary == backup {
            for (offset, flag) in [0, backup_offset].into_iter().zip(flags) {
                if flag & FAT_DIRTY != 0 {
                    fat.write_all_at(&[flag & !FAT_DIRTY], offset + FAT32_BOOT_FLAGS as u64)?;
                }
            }
        }
        Ok(())
    }

    fn check(&self) -> Result<()> {
        let output = disk_helper("fsck.fat", &self.work)?
            .args(["-n", FAT])
            .output()
            .context("checking the EFI partition with fsck.fat")?;
        ensure!(
            output.status.success(),
            "fsck.fat rejected the EFI partition ({})\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
        Ok(())
    }

    fn read_loader(&self) -> Result<()> {
        if !self.has_directory("", "loader")? {
            return Ok(());
        }
        self.read_file_if_present("loader", "loader.conf", MAX_CONFIG_BYTES)?;
        if self.has_directory("loader", "entries")? {
            for entry in self.children("loader/entries")? {
                let lower = entry.name.to_ascii_lowercase();
                if lower.ends_with(".conf") || lower.ends_with(".cps-destroy") {
                    ensure!(
                        !entry.directory
                            && entry.name.starts_with("cps-")
                            && (entry.name.ends_with(".conf")
                                || entry.name.ends_with(".cps-destroy"))
                            && safe_name(&entry.name),
                        "the EFI partition has an invalid or non-CPS boot entry {:?}",
                        entry.name
                    );
                    self.copy_out(&format!("loader/entries/{}", entry.name), MAX_CONFIG_BYTES)?;
                }
            }
        }
        Ok(())
    }

    /// Refuse an ESP that already holds CPS boot files.
    pub fn require_no_boot_files(&self) -> Result<()> {
        if self.has_directory("", "EFI")? {
            for name in ["cps", "cps-uefi"] {
                ensure!(
                    !self.has_directory("EFI", name)?,
                    "the EFI partition already has CPS boot files in EFI/{name}"
                );
            }
        }
        Ok(())
    }

    /// Copy out the systemd-boot binaries that firmware can start.
    pub fn read_loaders(&self) -> Result<()> {
        if !self.has_directory("", "EFI")? {
            return Ok(());
        }
        for (directory, prefix) in [("systemd", "systemd-boot"), ("BOOT", "boot")] {
            if !self.has_directory("EFI", directory)? {
                continue;
            }
            let directory = format!("EFI/{directory}");
            for entry in self.children(&directory)? {
                let name = entry.name.to_ascii_lowercase();
                if !entry.directory && name.starts_with(prefix) && name.ends_with(".efi") {
                    self.copy_out(&format!("{directory}/{}", entry.name), MAX_ESP_BYTES)?;
                }
            }
        }
        Ok(())
    }

    fn read_file_if_present(&self, directory: &str, name: &str, limit: u64) -> Result<()> {
        if let Some(entry) = self
            .children(directory)?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
        {
            ensure!(
                !entry.directory,
                "EFI path {directory}/{name} is a directory"
            );
            self.copy_out(&format!("{directory}/{name}"), limit)?;
        }
        Ok(())
    }

    fn copy_out(&self, path: &str, limit: u64) -> Result<()> {
        let destination = self.tree.join(path);
        fs::create_dir_all(destination.parent().context("missing EFI file parent")?)?;
        let status = self
            .fat_command("mcopy", &[])?
            .arg(format!("::/{path}"))
            .arg(Path::new("esp").join(path))
            .status()
            .with_context(|| format!("reading EFI file {path} with mcopy"))?;
        ensure!(
            status.success(),
            "mcopy could not read EFI file {path} ({status})"
        );
        ensure!(
            destination.metadata()?.len() <= limit,
            "EFI file {path} exceeds its size limit"
        );
        Ok(())
    }

    fn has_directory(&self, parent: &str, name: &str) -> Result<bool> {
        let entry = self
            .children(parent)?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name));
        match entry {
            Some(entry) if entry.directory => Ok(true),
            Some(_) => bail!("expected an EFI directory at {parent}/{name}"),
            None => Ok(false),
        }
    }

    fn children(&self, directory: &str) -> Result<Vec<FatEntry>> {
        let target = if directory.is_empty() {
            "::/".into()
        } else {
            format!("::/{directory}/")
        };
        let output = self
            .fat_command("mdir", &["-a", "-b"])?
            .arg(&target)
            .output()
            .with_context(|| format!("listing EFI directory {target} with mdir"))?;
        ensure!(
            output.status.success(),
            "mdir could not list EFI directory {target}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let directory = line.ends_with('/');
                let (_, name) = line.trim_end_matches('/').rsplit_once('/')?;
                Some(FatEntry {
                    name: name.to_owned(),
                    directory,
                })
            })
            .collect())
    }

    pub fn write<'a>(
        &self,
        disk: &OfflineDisk,
        partition: &PartitionExtent,
        files: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let mut files = files.into_iter().peekable();
        if files.peek().is_none() {
            return Ok(());
        }
        for path in files {
            self.copy_in(path)?;
        }
        self.check()?;
        let mut fat = File::open(self.work.join(FAT))?;
        ensure!(
            fat.metadata()?.len() == partition.bytes,
            "the EFI partition was resized"
        );
        let expected = digest_reader(&mut fat)?;
        fat.rewind()?;
        ensure!(
            disk.write_partition(partition, &fat)? == expected,
            "the deployed EFI partition differs from the checked FAT image"
        );
        Ok(())
    }

    fn copy_in(&self, path: &str) -> Result<()> {
        let (directory, _) = path.rsplit_once('/').context("missing EFI file parent")?;
        let mut parent = String::new();
        for part in directory.split('/') {
            let exists = self.has_directory(&parent, part)?;
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
            if !exists {
                let status = self
                    .fat_command("mmd", &[])?
                    .arg(format!("::/{parent}"))
                    .status()?;
                ensure!(
                    status.success(),
                    "mmd could not create EFI directory {parent} ({status})"
                );
            }
        }
        let source = self.tree.join(path);
        let target = format!("::/{path}");
        let status = self
            .fat_command("mcopy", &["-o"])?
            .arg(Path::new("esp").join(path))
            .arg(&target)
            .status()?;
        ensure!(
            status.success(),
            "mcopy could not write EFI file {path} ({status})"
        );
        let verify = tempfile::NamedTempFile::new_in(&self.work)?;
        let filename = verify
            .path()
            .file_name()
            .context("missing EFI verification filename")?;
        let mut read_back = self.fat_command("mcopy", &["-o"])?;
        if self.epoch.is_some() {
            read_back.arg("-m");
        }
        let status = read_back.arg(&target).arg(filename).status()?;
        ensure!(
            status.success(),
            "mcopy could not read back EFI file {path} ({status})"
        );
        let checked = File::open(verify.path())?;
        if let Some(epoch) = self.epoch {
            ensure!(
                checked.metadata()?.mtime() == i64::from(epoch),
                "mcopy did not apply SOURCE_DATE_EPOCH to EFI file {path}"
            );
        }
        ensure!(
            digest_reader(checked)? == digest_reader(File::open(source)?)?,
            "the written EFI file {path} differs from its staged contents"
        );
        Ok(())
    }

    fn fat_command(&self, program: &str, flags: &[&str]) -> Result<Command> {
        let mut command = disk_helper(program, &self.work)?;
        command
            .env("MTOOLSRC", "/dev/null")
            .args(flags)
            .args(["-i", FAT]);
        if let Some(epoch) = self.epoch {
            command
                .env("SOURCE_DATE_EPOCH", epoch.to_string())
                .env("TZ", "UTC");
        }
        Ok(command)
    }
}

fn safe_name(name: &str) -> bool {
    name.len() <= 96
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".+-".contains(&byte)
        })
}

/// Run a disk tool from `CPS_DISK_TOOLS_DIR`, or else from the system path.
pub(super) fn disk_helper(program: &str, work: &Path) -> Result<Command> {
    let program = if let Some(path) = std::env::var_os(TOOLS_DIR) {
        let path = PathBuf::from(path);
        ensure!(
            !geteuid().is_root() && path.is_absolute(),
            "{TOOLS_DIR} requires an unprivileged caller and an absolute path"
        );
        path.join(program)
    } else {
        PathBuf::from(program)
    };
    let mut command = artifact_helper(program, work);
    command.stdin(Stdio::null());
    Ok(command)
}
