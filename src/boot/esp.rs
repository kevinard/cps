use std::{
    fs::{self, File, OpenOptions},
    io::Seek,
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::process::geteuid;

use super::{
    Activation, BootEnvironment, artifact_helper,
    deploy::{DiskStage, OfflineDisk, PartitionExtent},
    list_boot_environments,
    systemd_boot::{self, Deployment, RetiredEntry, RootDeployment},
};
use crate::model::digest_reader;

const FAT: &str = "esp.fat";
const MAX_ESP_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const FAT32_BOOT_FLAGS: usize = 65;
const FAT_DIRTY: u8 = 0x01;

/// Read the boot inventory from the EFI System Partition of an offline disk.
pub fn list_boot_environments_on_disk(
    disk: &Path,
    esp_partuuid: &str,
) -> Result<Vec<BootEnvironment>> {
    let disk = OfflineDisk::open(disk)?;
    let temp = tempfile::tempdir()?;
    let esp = Esp::read(&disk, disk.esp(esp_partuuid)?, temp.path())?;
    list_boot_environments(&esp.tree)
}

/// Register a boot environment on a new copy of an offline GPT disk.
pub fn activate_boot_environment_on_disk(
    root: &Path,
    activation: &Activation,
    disk: &Path,
    esp_partuuid: &str,
    receipt: &Path,
    output: &Path,
) -> Result<String> {
    activate_disk(root, activation, disk, esp_partuuid, receipt, output, None)
}

/// Deploy and activate a root together, optionally recycling a registered slot.
pub fn deploy_and_activate_boot_environment(
    root: &Path,
    activation: &Activation,
    disk: &Path,
    esp_partuuid: &str,
    receipt: &Path,
    output: &Path,
    install: RootDeployment<'_>,
) -> Result<String> {
    activate_disk(
        root,
        activation,
        disk,
        esp_partuuid,
        receipt,
        output,
        Some(install),
    )
}

fn activate_disk(
    root: &Path,
    activation: &Activation,
    disk: &Path,
    esp_partuuid: &str,
    receipt: &Path,
    output: &Path,
    install: Option<RootDeployment<'_>>,
) -> Result<String> {
    let stage = DiskStage::new(disk, output)?;
    let partition = stage.disk.esp(esp_partuuid)?;
    let esp = Esp::read(&stage.disk, partition, &stage.directory())?;
    esp.read_boot_files()?;
    let deployment = Deployment {
        disk: &stage.disk,
        receipt,
        root: install,
        target: None,
    };
    let update = systemd_boot::activate(root, &esp.tree, activation, Some(deployment))?;
    if let Some(retired) = &update.retired {
        esp.remove_entry(retired)?;
    }
    esp.write(
        &stage.disk,
        partition,
        update.files.iter().map(String::as_str),
    )?;
    stage.publish()?;
    Ok(update.id)
}

/// Select a known-good boot environment on a new copy of an offline GPT disk.
pub fn rollback_boot_environment_on_disk(
    disk: &Path,
    esp_partuuid: &str,
    id: Option<&str>,
    output: &Path,
) -> Result<String> {
    let stage = DiskStage::new(disk, output)?;
    let partition = stage.disk.esp(esp_partuuid)?;
    let esp = Esp::read(&stage.disk, partition, &stage.directory())?;
    esp.read_boot_files()?;
    let id = systemd_boot::rollback(&esp.tree, id, Some(&stage.disk))?;
    esp.write(&stage.disk, partition, ["loader/loader.conf"])?;
    stage.publish()?;
    Ok(id)
}

struct Esp {
    work: PathBuf,
    tree: PathBuf,
}

struct FatEntry {
    name: String,
    directory: bool,
}

impl Esp {
    fn read(disk: &OfflineDisk, partition: &PartitionExtent, work: &Path) -> Result<Self> {
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
        let output = esp_helper("fsck.fat", &self.work)?
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
                if entry.name.to_ascii_lowercase().ends_with(".conf") {
                    ensure!(
                        !entry.directory
                            && entry.name.starts_with("cps-")
                            && entry.name.ends_with(".conf")
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

    fn read_boot_files(&self) -> Result<()> {
        if !self.has_directory("", "EFI")? || !self.has_directory("EFI", "cps")? {
            return Ok(());
        }
        for entry in self.children("EFI/cps")? {
            if entry.name.to_ascii_lowercase().starts_with("cps-") {
                ensure!(
                    entry.directory && safe_name(&entry.name),
                    "invalid CPS boot files directory {:?}",
                    entry.name
                );
                let dir = format!("EFI/cps/{}", entry.name);
                self.read_file_if_present(&dir, "vmlinuz.efi", MAX_ESP_BYTES)?;
                self.read_file_if_present(&dir, "initramfs.img", MAX_ESP_BYTES)?;
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

    fn write<'a>(
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
            disk.write_partition(partition, &mut fat)? == expected,
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
        let status = self
            .fat_command("mcopy", &["-o"])?
            .arg(&target)
            .arg(filename)
            .status()?;
        ensure!(
            status.success(),
            "mcopy could not read back EFI file {path} ({status})"
        );
        ensure!(
            digest_reader(File::open(verify.path())?)? == digest_reader(File::open(source)?)?,
            "the written EFI file {path} differs from its staged contents"
        );
        Ok(())
    }

    fn remove_entry(&self, retired: &RetiredEntry) -> Result<()> {
        self.delete_file("loader/entries", &retired.file)?;
        if self.has_directory("EFI/cps", &retired.id)? {
            let directory = format!("EFI/cps/{}", retired.id);
            for artifact in ["vmlinuz.efi", "initramfs.img"] {
                if self
                    .children(&directory)?
                    .iter()
                    .any(|entry| entry.name.eq_ignore_ascii_case(artifact))
                {
                    self.delete_file(&directory, artifact)?;
                }
            }
            if self.children(&directory)?.is_empty() {
                let status = self
                    .fat_command("mrd", &[])?
                    .arg(format!("::/{directory}"))
                    .status()?;
                ensure!(
                    status.success() && !self.has_directory("EFI/cps", &retired.id)?,
                    "could not remove the retired EFI directory {directory}"
                );
            }
        }
        Ok(())
    }

    fn delete_file(&self, directory: &str, name: &str) -> Result<()> {
        let status = self
            .fat_command("mdel", &[])?
            .arg(format!("::/{directory}/{name}"))
            .status()?;
        ensure!(
            status.success()
                && !self
                    .children(directory)?
                    .iter()
                    .any(|entry| entry.name.eq_ignore_ascii_case(name)),
            "could not remove the retired EFI file {directory}/{name}"
        );
        Ok(())
    }

    fn fat_command(&self, program: &str, flags: &[&str]) -> Result<Command> {
        let mut command = esp_helper(program, &self.work)?;
        command
            .env("MTOOLSRC", "/dev/null")
            .args(flags)
            .args(["-i", FAT]);
        Ok(command)
    }
}

fn safe_name(name: &str) -> bool {
    name.len() <= 96
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".+-".contains(&byte)
        })
}

fn esp_helper(program: &str, work: &Path) -> Result<Command> {
    let program = if let Some(path) = std::env::var_os("CPS_ESP_TOOLS_DIR") {
        let path = PathBuf::from(path);
        ensure!(
            !geteuid().is_root() && path.is_absolute(),
            "CPS_ESP_TOOLS_DIR requires an unprivileged caller and an absolute path"
        );
        path.join(program)
    } else {
        PathBuf::from(program)
    };
    let mut command = artifact_helper(program, work);
    command.stdin(Stdio::null());
    Ok(command)
}
