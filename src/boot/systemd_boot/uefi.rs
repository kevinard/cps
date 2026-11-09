use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{FlockOperation, flock, fstatfs};

use super::policy::{self, BootEntry, LoaderPolicy, field};
use crate::boot::{
    device::{EspDevicePath, esp_device_path},
    uki,
};

pub(super) const DIRECTORY: &str = "EFI/cps-uefi";
pub(super) const ARMED: &str = "# cps-uefi-armed ";
const GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const EFIVARFS: &str = "/sys/firmware/efi/efivars";
const EFIVARFS_MAGIC: u32 = 0xde5e81e4;
const ATTRIBUTES: u32 = 7; // Non-volatile, boot service and runtime access.

pub(super) fn present(esp: &Path) -> Result<bool> {
    policy::directory_exists(&esp.join(DIRECTORY))
}

pub(super) struct Firmware {
    _lock: File,
    device: EspDevicePath,
    order: Vec<u16>,
    next: Option<u16>,
    current: Option<u16>,
    options: BTreeMap<u16, Vec<u8>>,
    boot_id: String,
}

impl Firmware {
    pub fn open(esp: &Path, locked_esp: &File) -> Result<Self> {
        let dir = Path::new(EFIVARFS);
        let lock = File::open(dir).context("direct UEFI boot requires mounted efivarfs")?;
        ensure!(
            fstatfs(&lock)?.f_type as u32 == EFIVARFS_MAGIC,
            "direct UEFI boot requires mounted efivarfs"
        );
        // BootOrder and BootNext are shared by all ESPs on this host.
        flock(&lock, FlockOperation::LockExclusive)?;
        let mut options = BTreeMap::new();
        for child in fs::read_dir(dir)? {
            let name = child?.file_name();
            let Some(stem) = name
                .to_str()
                .and_then(|name| name.strip_suffix(GLOBAL))
                .and_then(|name| name.strip_suffix('-'))
                .and_then(|name| name.strip_prefix("Boot"))
                .filter(|name| name.len() == 4)
            else {
                continue;
            };
            if let Ok(number) = u16::from_str_radix(stem, 16) {
                let data =
                    variable(&format!("Boot{stem}"))?.context("UEFI boot option disappeared")?;
                ensure!(
                    options.insert(number, data).is_none(),
                    "duplicate UEFI boot number"
                );
            }
        }
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let boot_id = super::canonical_partuuid(boot_id.trim())?;
        Ok(Self {
            _lock: lock,
            device: esp_device_path(esp, locked_esp)?,
            order: boot_order()?,
            next: boot_number("BootNext")?,
            current: boot_number("BootCurrent")?,
            options,
            boot_id,
        })
    }

    fn option(&self, id: &str) -> Result<Option<u16>> {
        let expected = load_option(&self.device, id)?;
        let mut matches = self.options.iter().filter(|(_, value)| **value == expected);
        let number = matches.next().map(|(&number, _)| number);
        ensure!(
            matches.next().is_none(),
            "duplicate firmware options for {id}"
        );
        Ok(number)
    }

    pub fn inventory(&self, entries: &mut [BootEntry]) -> Result<Option<LoaderPolicy>> {
        let mut default = None;
        let mut preferred = None;
        for entry in entries {
            ensure!(
                entry.btrfs.is_some() && entry.uki.is_some(),
                "direct UEFI entries require signed Btrfs UKIs"
            );
            if entry.tries.is_some() {
                let armed = self.armed_boot(Some(entry))?;
                ensure!(
                    entry.parent.is_some() && super::canonical_partuuid(armed)? == armed,
                    "invalid UEFI trial identity"
                );
            }
            let number = self.option(&entry.id)?;
            let is_default = number.is_some() && self.order.first() == number.as_ref();
            let is_next = number.is_some() && self.next == number;
            if is_trial(entry) || is_cancelled(entry) {
                ensure!(
                    !number.is_some_and(|n| self.order.contains(&n)),
                    "unverified UEFI trial {} appears in BootOrder",
                    entry.id
                );
                entry.tries = Some(u32::from(is_trial(entry) && is_next));
                if is_trial(entry) && is_next {
                    preferred = Some(entry.id.clone());
                }
            } else {
                ensure!(
                    entry.tries.is_none() || is_verified(entry),
                    "invalid UEFI trial record"
                );
                entry.tries = None;
                if is_default {
                    default = Some(entry.id.clone());
                }
            }
        }
        ensure!(
            preferred.is_none() || default.is_some(),
            "a UEFI trial requires a known-good default"
        );
        Ok(default.map(|default| LoaderPolicy::Pinned { default, preferred }))
    }

    pub fn armed_boot<'a>(&'a self, entry: Option<&'a BootEntry>) -> Result<&'a str> {
        if let Some(entry) = entry {
            return field(&entry.description, ARMED)?.context("missing UEFI trial boot identity");
        }
        Ok(&self.boot_id)
    }

    pub fn can_arm(&self, entry: &BootEntry) -> Result<bool> {
        Ok(is_trial(entry) && self.armed_boot(Some(entry))? == self.boot_id)
    }

    pub fn require_no_next(&self) -> Result<()> {
        ensure!(self.next.is_none(), "a firmware boot is already scheduled");
        Ok(())
    }

    pub fn check_next(&self, entries: &[BootEntry], cancelling: bool) -> Result<()> {
        if let Some(next) = self.next {
            for entry in entries
                .iter()
                .filter(|entry| is_trial(entry) || (cancelling && is_cancelled(entry)))
            {
                if self.option(&entry.id)? == Some(next) {
                    return Ok(());
                }
            }
            anyhow::bail!("BootNext already selects another firmware boot option");
        }
        Ok(())
    }

    fn ensure_option(&self, id: &str) -> Result<u16> {
        let existing = self.option(id)?;
        self.check_references(id, existing)?;
        if let Some(number) = existing {
            self.check_option(number, id)?;
            return Ok(number);
        }
        let number = (0..=u16::MAX)
            .find(|n| {
                !self.options.contains_key(n)
                    && !self.order.contains(n)
                    && self.next != Some(*n)
                    && self.current != Some(*n)
            })
            .context("no free UEFI boot numbers")?;
        write_variable(&option_name(number), &load_option(&self.device, id)?, true)?;
        Ok(number)
    }

    fn check_option(&self, number: u16, id: &str) -> Result<()> {
        let name = option_name(number);
        ensure!(
            variable(&name)?.as_deref() == Some(load_option(&self.device, id)?.as_slice()),
            "firmware option {name} changed during the CPS operation"
        );
        Ok(())
    }

    pub fn activate(&self, id: &str, trial: bool) -> Result<()> {
        ensure!(
            boot_number("BootNext")? == self.next,
            "BootNext changed during activation"
        );
        let number = self.ensure_option(id)?;
        if trial {
            ensure!(
                !self.order.contains(&number),
                "UEFI trial already appears in BootOrder"
            );
            ensure!(
                self.next.is_none() || self.next == Some(number),
                "BootNext already selects another firmware boot option"
            );
            if self.next.is_none() {
                ensure!(
                    boot_order()? == self.order && boot_number("BootNext")?.is_none(),
                    "firmware boot selection changed during activation"
                );
                write_variable("BootNext", &number.to_le_bytes(), true)?;
            }
        } else {
            ensure!(
                self.next.is_none(),
                "BootNext already selects another firmware boot option"
            );
            self.promote(number)?;
        }
        Ok(())
    }

    pub fn rollback(&self, esp: &Path, id: &str, entries: &[BootEntry]) -> Result<()> {
        self.check_next(entries, true)?;
        let target = self
            .option(id)?
            .context("missing firmware boot option for rollback")?;
        self.check_option(target, id)?;
        ensure!(
            boot_number("BootNext")? == self.next,
            "BootNext changed during rollback"
        );
        let running_trial = self.running_trial(entries)?;
        for entry in entries
            .iter()
            .filter(|entry| is_trial(entry) && entry.id != id)
        {
            let scheduled = self.next.is_some() && self.option(&entry.id)? == self.next;
            let running = running_trial.is_some_and(|trial| trial.id == entry.id);
            if scheduled || running {
                let dir = esp.join(DIRECTORY);
                rename_record(&dir, &entry.path, &dir.join(format!("{}+0.conf", entry.id)))?;
            }
        }
        if self.next.is_some() {
            ensure!(
                boot_number("BootNext")? == self.next,
                "BootNext changed during rollback"
            );
            fs::remove_file(variable_path("BootNext"))
                .context("cancelling the pending UEFI trial")?;
        }
        if let Some(trial) = running_trial.filter(|entry| is_verified(entry) && entry.id != id) {
            let dir = esp.join(DIRECTORY);
            rename_record(&dir, &trial.path, &dir.join(format!("{}.conf", trial.id)))?;
        }
        self.promote(target)
    }

    pub fn running_trial<'a>(&self, entries: &'a [BootEntry]) -> Result<Option<&'a BootEntry>> {
        for entry in entries
            .iter()
            .filter(|entry| is_trial(entry) || is_verified(entry))
        {
            if self.current.is_some()
                && self.option(&entry.id)? == self.current
                && self.armed_boot(Some(entry))? != self.boot_id
            {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    pub fn bless(&self, esp: &Path, entry: &BootEntry, parent: &BootEntry) -> Result<()> {
        let number = self
            .option(&entry.id)?
            .context("missing UEFI trial boot option")?;
        let fallback = self
            .option(&parent.id)?
            .context("missing UEFI fallback boot option")?;
        self.check_option(number, &entry.id)?;
        self.check_option(fallback, &parent.id)?;
        ensure!(
            self.current == Some(number) && parent.tries.is_none(),
            "expected a running UEFI trial and a known-good fallback"
        );
        ensure!(
            self.order.first() == Some(&fallback)
                || (is_verified(entry) && self.order.first() == Some(&number)),
            "UEFI trial's fallback is no longer the configured default"
        );
        ensure!(
            self.next.is_none() && boot_number("BootNext")?.is_none(),
            "BootNext is already scheduled"
        );
        let dir = esp.join(DIRECTORY);
        let verified = dir.join(format!("{}+0-1.conf", entry.id));
        if is_trial(entry) {
            // Persist the successful health result before making a lasting firmware change.
            rename_record(&dir, &entry.path, &verified)?;
        }
        self.promote(number)?;
        rename_record(&dir, &verified, &dir.join(format!("{}.conf", entry.id)))
    }

    fn promote(&self, number: u16) -> Result<()> {
        ensure!(
            boot_number("BootNext")?.is_none(),
            "BootNext is already scheduled"
        );
        let order = preferred_order(&self.order, number);
        self.set_order(&order)
    }

    fn set_order(&self, order: &[u16]) -> Result<()> {
        ensure!(
            boot_order()? == self.order,
            "BootOrder changed during the CPS operation"
        );
        if order != self.order {
            let bytes: Vec<_> = order.iter().flat_map(|n| n.to_le_bytes()).collect();
            write_variable("BootOrder", &bytes, false)?;
        }
        Ok(())
    }

    pub fn check_removal(&self, id: &str) -> Result<Option<u16>> {
        let number = self.option(id)?;
        self.check_references(id, number)?;
        if let Some(number) = number {
            ensure!(
                self.current != Some(number)
                    && self.next != Some(number)
                    && self.order.first() != Some(&number),
                "cannot remove a running or selected firmware boot option"
            );
        }
        Ok(number)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        if let Some(number) = self.check_removal(id)? {
            ensure!(
                boot_number("BootNext")? != Some(number),
                "UEFI boot option is now scheduled"
            );
            let order: Vec<_> = self
                .order
                .iter()
                .copied()
                .filter(|n| *n != number)
                .collect();
            self.set_order(&order)?;
            let name = option_name(number);
            ensure!(
                boot_number("BootNext")? != Some(number),
                "UEFI boot option is now scheduled"
            );
            self.check_option(number, id)?;
            fs::remove_file(variable_path(&name))
                .context("removing the retired firmware boot option")?;
        }
        Ok(())
    }

    fn check_references(&self, id: &str, owned: Option<u16>) -> Result<()> {
        let path = efi_path(id);
        for (&number, bytes) in &self.options {
            if Some(number) != owned {
                ensure!(
                    !references_path(bytes, &path)?,
                    "firmware option Boot{number:04X} also references {path}"
                );
            }
        }
        Ok(())
    }
}

fn rename_record(dir: &Path, from: &Path, to: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn is_trial(entry: &BootEntry) -> bool {
    entry
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("+1.conf"))
}

fn is_verified(entry: &BootEntry) -> bool {
    entry
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("+0-1.conf"))
}

fn is_cancelled(entry: &BootEntry) -> bool {
    entry
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("+0.conf"))
}

fn preferred_order(order: &[u16], first: u16) -> Vec<u16> {
    std::iter::once(first)
        .chain(order.iter().copied().filter(|&n| n != first))
        .collect()
}

#[cfg(not(test))]
fn efivarfs() -> PathBuf {
    PathBuf::from(EFIVARFS)
}

#[cfg(test)]
thread_local! {
    /// A directory of regular files standing in for efivarfs in unit tests.
    static TEST_EFIVARFS: std::cell::RefCell<PathBuf> = std::cell::RefCell::new(PathBuf::from(EFIVARFS));
}

#[cfg(test)]
fn efivarfs() -> PathBuf {
    TEST_EFIVARFS.with(|dir| dir.borrow().clone())
}

fn variable_path(name: &str) -> PathBuf {
    efivarfs().join(format!("{name}-{GLOBAL}"))
}

fn option_name(number: u16) -> String {
    format!("Boot{number:04X}")
}

fn variable(name: &str) -> Result<Option<Vec<u8>>> {
    let path = variable_path(name);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("opening UEFI variable {name}")),
    };
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    ensure!(
        (4..=65536).contains(&bytes.len()),
        "invalid size for UEFI variable {name}"
    );
    Ok(Some(bytes.split_off(4)))
}

fn boot_number(name: &str) -> Result<Option<u16>> {
    variable(name)?
        .map(|data| {
            Ok(u16::from_le_bytes(
                data.as_slice()
                    .try_into()
                    .with_context(|| format!("invalid UEFI {name}"))?,
            ))
        })
        .transpose()
}

fn boot_order() -> Result<Vec<u16>> {
    let bytes = variable("BootOrder")?.unwrap_or_default();
    let (numbers, tail) = bytes.as_chunks::<2>();
    ensure!(tail.is_empty(), "invalid UEFI BootOrder");
    Ok(numbers.iter().copied().map(u16::from_le_bytes).collect())
}

fn write_variable(name: &str, data: &[u8], new: bool) -> Result<()> {
    let path = variable_path(name);
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).create_new(new);
    // efivarfs replaces the whole variable on write. Regular test files need truncation.
    #[cfg(test)]
    opts.truncate(true);
    let mut file = opts
        .open(&path)
        .with_context(|| format!("opening UEFI variable {name} for writing"))?;
    let mut bytes = ATTRIBUTES.to_le_bytes().to_vec();
    bytes.extend_from_slice(data);
    // efivarfs passes a single write to SetVariable.
    ensure!(
        file.write(&bytes)? == bytes.len(),
        "short write to UEFI variable {name}"
    );
    ensure!(
        variable(name)?.as_deref() == Some(data),
        "firmware did not retain {name}"
    );
    Ok(())
}

fn efi_path(id: &str) -> String {
    format!(r"\EFI\cps\{id}\{}", uki::NAME)
}

fn ucs2(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn load_option(device: &EspDevicePath, id: &str) -> Result<Vec<u8>> {
    policy::entry_identity(id)?;
    let mut guid = hex::decode(device.uuid.replace('-', ""))?;
    ensure!(guid.len() == 16, "invalid GPT partition UUID");
    guid[..4].reverse();
    guid[4..6].reverse();
    guid[6..8].reverse();
    let filename = ucs2(&efi_path(id));
    let mut path = vec![4, 1, 42, 0]; // GPT hard-drive media device path.
    path.extend(device.number.to_le_bytes());
    path.extend(device.start.to_le_bytes());
    path.extend(device.sectors.to_le_bytes());
    path.extend(guid);
    path.extend([2, 2]); // GPT format and GUID signature.
    path.extend([4, 4]); // File path media device path.
    path.extend(u16::try_from(filename.len() + 4)?.to_le_bytes());
    path.extend(filename);
    path.extend([0x7f, 0xff, 4, 0]); // End of device path.
    let mut option = 1u32.to_le_bytes().to_vec(); // LOAD_OPTION_ACTIVE.
    option.extend(u16::try_from(path.len())?.to_le_bytes());
    option.extend(ucs2(&format!("CPS {id}")));
    option.extend(path);
    Ok(option)
}

fn references_path(data: &[u8], expected: &str) -> Result<bool> {
    let size = u16::from_le_bytes(
        data.get(4..6)
            .context("invalid firmware boot option")?
            .try_into()?,
    ) as usize;
    let description = data[6..]
        .chunks_exact(2)
        .position(|c| c == [0, 0])
        .context("unterminated firmware boot description")?;
    let start = 6 + 2 * (description + 1);
    let mut path = data
        .get(start..start + size)
        .context("truncated firmware device path")?;
    while !path.is_empty() {
        let [kind, subtype, a, b, ..] = path else {
            anyhow::bail!("invalid firmware device path node")
        };
        let len = u16::from_le_bytes([*a, *b]) as usize;
        ensure!(
            len >= 4 && len <= path.len(),
            "invalid firmware device path node length"
        );
        if (*kind, *subtype) == (4, 4) {
            let (chars, tail) = path[4..len].as_chunks::<2>();
            ensure!(tail.is_empty(), "invalid firmware file path");
            let name = String::from_utf16_lossy(
                &chars
                    .iter()
                    .copied()
                    .map(u16::from_le_bytes)
                    .collect::<Vec<_>>(),
            );
            if name
                .trim_end_matches('\0')
                .replace('/', r"\")
                .eq_ignore_ascii_case(expected)
            {
                return Ok(true);
            }
        }
        path = &path[len..];
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::super::policy::{BootRoot, BootUki};
    use super::*;
    use crate::boot::btrfs_boot::BtrfsRecord;

    const BASE: &str = "cps-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa-1";
    const TRIAL: &str = "cps-bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb-2";
    const FIRST_BOOT: &str = "c0d8d7b0-5d26-4a5c-8748-9ada383267cd";
    const SECOND_BOOT: &str = "19299012-d055-4ffc-b49f-62c62e6f6956";

    fn device() -> EspDevicePath {
        EspDevicePath {
            number: 1,
            start: 2048,
            sectors: 131072,
            uuid: "00112233-4455-6677-8899-aabbccddeeff".into(),
        }
    }

    fn entry(id: &str, trial: bool) -> BootEntry {
        BootEntry {
            path: PathBuf::from(format!("{id}{}.conf", if trial { "+1" } else { "" })),
            id: id.to_owned(),
            revision: if trial { 2 } else { 1 },
            tries: trial.then_some(1),
            description: if trial {
                format!("{ARMED}{FIRST_BOOT}\n")
            } else {
                String::new()
            },
            title: id.to_owned(),
            root: BootRoot::Btrfs(device().uuid),
            parent: trial.then(|| BASE.to_owned()),
            btrfs: Some(BtrfsRecord {
                fsid: device().uuid,
                uuid: device().uuid,
                subvolid: if trial { 257 } else { 256 },
                path: id.to_owned(),
            }),
            uki: Some(BootUki {
                sha256: String::new(),
                cmdline: String::new(),
            }),
        }
    }

    /// Firmware state backed by a temporary directory instead of efivarfs.
    struct FakeFirmware {
        _vars: tempfile::TempDir,
        esp: tempfile::TempDir,
    }

    impl FakeFirmware {
        fn new() -> Result<Self> {
            let vars = tempfile::tempdir()?;
            TEST_EFIVARFS.with(|dir| *dir.borrow_mut() = vars.path().to_owned());
            let esp = tempfile::tempdir()?;
            fs::create_dir_all(esp.path().join(DIRECTORY))?;
            Ok(Self { _vars: vars, esp })
        }

        fn set(&self, name: &str, data: &[u8]) -> Result<()> {
            let mut bytes = ATTRIBUTES.to_le_bytes().to_vec();
            bytes.extend_from_slice(data);
            fs::write(variable_path(name), bytes)?;
            Ok(())
        }

        /// Store the firmware's cached view and return it.
        fn firmware(
            &self,
            order: &[u16],
            next: Option<u16>,
            current: Option<u16>,
            options: &[(u16, &str)],
            boot_id: &str,
        ) -> Result<Firmware> {
            for variable in fs::read_dir(efivarfs())? {
                fs::remove_file(variable?.path())?;
            }
            let order_bytes: Vec<_> = order.iter().flat_map(|n| n.to_le_bytes()).collect();
            self.set("BootOrder", &order_bytes)?;
            if let Some(next) = next {
                self.set("BootNext", &next.to_le_bytes())?;
            }
            if let Some(current) = current {
                self.set("BootCurrent", &current.to_le_bytes())?;
            }
            let mut stored = BTreeMap::new();
            for (number, id) in options {
                let option = load_option(&device(), id)?;
                self.set(&option_name(*number), &option)?;
                stored.insert(*number, option);
            }
            Ok(Firmware {
                _lock: tempfile::tempfile()?,
                device: device(),
                order: order.to_vec(),
                next,
                current,
                options: stored,
                boot_id: boot_id.into(),
            })
        }

        fn record(&self, id: &str, suffix: &str) -> Result<BootEntry> {
            let mut entry = entry(id, !suffix.is_empty());
            entry.path = self
                .esp
                .path()
                .join(DIRECTORY)
                .join(format!("{id}{suffix}.conf"));
            fs::write(&entry.path, &entry.description)?;
            Ok(entry)
        }

        fn records(&self) -> Result<Vec<String>> {
            let mut names: Vec<_> = fs::read_dir(self.esp.path().join(DIRECTORY))?
                .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
                .collect::<Result<_>>()?;
            names.sort();
            Ok(names)
        }
    }

    #[test]
    fn variables_round_trip_and_reject_malformed_values() -> Result<()> {
        let fake = FakeFirmware::new()?;
        assert_eq!(variable("BootNext")?, None);
        assert_eq!(boot_order()?, Vec::<u16>::new());
        write_variable("BootOrder", &[1, 0, 0x2a, 0], false)?;
        assert_eq!(boot_order()?, [1, 0x2a]);
        assert!(write_variable("BootOrder", &[1, 0], true).is_err());
        fake.set("BootNext", &[1, 0, 0])?;
        assert!(boot_number("BootNext").is_err());
        fake.set("BootOrder", &[1])?;
        assert!(boot_order().is_err());
        fs::write(variable_path("BootCurrent"), [7, 0])?;
        assert!(variable("BootCurrent").is_err());
        Ok(())
    }

    #[test]
    fn trial_activation_sets_only_boot_next_and_bootstrap_promotes() -> Result<()> {
        let fake = FakeFirmware::new()?;
        // Bootstrap creates an option and moves it first in BootOrder.
        let firmware = fake.firmware(&[1, 42], None, Some(1), &[], FIRST_BOOT)?;
        firmware.activate(BASE, false)?;
        let order = boot_order()?;
        assert_eq!(order.len(), 3);
        assert!(order.ends_with(&[1, 42]));
        let base = order[0];
        assert_eq!(
            variable(&option_name(base))?,
            Some(load_option(&device(), BASE)?)
        );
        // A trial gets a fresh option that is scheduled once and kept out of BootOrder.
        let firmware = fake.firmware(&order, None, Some(base), &[(base, BASE)], FIRST_BOOT)?;
        firmware.activate(TRIAL, true)?;
        let next = boot_number("BootNext")?.context("missing BootNext")?;
        assert!(![1, 42, base].contains(&next));
        assert_eq!(boot_order()?, order);
        assert_eq!(
            variable(&option_name(next))?,
            Some(load_option(&device(), TRIAL)?)
        );
        // Retrying the same activation is idempotent.
        let firmware = fake.firmware(
            &order,
            Some(next),
            Some(base),
            &[(base, BASE), (next, TRIAL)],
            FIRST_BOOT,
        )?;
        firmware.activate(TRIAL, true)?;
        assert_eq!(boot_number("BootNext")?, Some(next));
        // An unrelated BootNext is preserved and blocks activation.
        let firmware = fake.firmware(&order, Some(42), Some(base), &[(base, BASE)], FIRST_BOOT)?;
        assert!(firmware.activate(TRIAL, true).is_err());
        assert!(firmware.activate(BASE, false).is_err());
        assert_eq!(boot_number("BootNext")?, Some(42));
        // A concurrent change to BootNext is detected.
        let firmware = fake.firmware(&order, None, Some(base), &[(base, BASE)], FIRST_BOOT)?;
        fake.set("BootNext", &42u16.to_le_bytes())?;
        assert!(firmware.activate(TRIAL, true).is_err());
        Ok(())
    }

    #[test]
    fn blessing_promotes_the_running_trial_after_recording_its_health() -> Result<()> {
        let fake = FakeFirmware::new()?;
        let base = fake.record(BASE, "")?;
        let trial = fake.record(TRIAL, "+1")?;
        // The firmware consumed BootNext and booted the trial in a new boot.
        let firmware = fake.firmware(
            &[1, 42],
            None,
            Some(2),
            &[(1, BASE), (2, TRIAL)],
            SECOND_BOOT,
        )?;
        assert_eq!(
            firmware
                .running_trial(std::slice::from_ref(&trial))?
                .map(|entry| entry.id.as_str()),
            Some(TRIAL)
        );
        firmware.bless(fake.esp.path(), &trial, &base)?;
        assert_eq!(boot_order()?, [2, 1, 42]);
        assert_eq!(
            fake.records()?,
            [format!("{BASE}.conf"), format!("{TRIAL}.conf")]
        );
        Ok(())
    }

    #[test]
    fn blessing_resumes_after_an_interrupted_promotion() -> Result<()> {
        let fake = FakeFirmware::new()?;
        let base = fake.record(BASE, "")?;
        let mut verified = fake.record(TRIAL, "+0-1")?;
        verified.tries = None;
        // BootOrder was already rewritten before the final rename.
        let firmware = fake.firmware(
            &[2, 1, 42],
            None,
            Some(2),
            &[(1, BASE), (2, TRIAL)],
            SECOND_BOOT,
        )?;
        firmware.bless(fake.esp.path(), &verified, &base)?;
        assert_eq!(boot_order()?, [2, 1, 42]);
        assert_eq!(
            fake.records()?,
            [format!("{BASE}.conf"), format!("{TRIAL}.conf")]
        );
        Ok(())
    }

    #[test]
    fn blessing_refuses_other_boots_and_scheduled_firmware_changes() -> Result<()> {
        let fake = FakeFirmware::new()?;
        let base = fake.record(BASE, "")?;
        let trial = fake.record(TRIAL, "+1")?;
        let options = [(1, BASE), (2, TRIAL)];
        // The fallback booted instead of the trial.
        let firmware = fake.firmware(&[1, 42], None, Some(1), &options, SECOND_BOOT)?;
        assert!(firmware.bless(fake.esp.path(), &trial, &base).is_err());
        // Another BootNext is pending.
        let firmware = fake.firmware(&[1, 42], Some(42), Some(2), &options, SECOND_BOOT)?;
        assert!(firmware.bless(fake.esp.path(), &trial, &base).is_err());
        // The fallback is no longer the firmware default.
        let firmware = fake.firmware(&[42, 1], None, Some(2), &options, SECOND_BOOT)?;
        assert!(firmware.bless(fake.esp.path(), &trial, &base).is_err());
        // A changed option is not trusted.
        let firmware = fake.firmware(&[1, 42], None, Some(2), &options, SECOND_BOOT)?;
        fake.set(&option_name(2), &load_option(&device(), BASE)?)?;
        assert!(firmware.bless(fake.esp.path(), &trial, &base).is_err());
        assert_eq!(boot_order()?, [1, 42]);
        assert_eq!(
            fake.records()?,
            [format!("{BASE}.conf"), format!("{TRIAL}+1.conf")]
        );
        Ok(())
    }

    #[test]
    fn rollback_cancels_a_scheduled_trial_and_keeps_the_default() -> Result<()> {
        let fake = FakeFirmware::new()?;
        let base = fake.record(BASE, "")?;
        let trial = fake.record(TRIAL, "+1")?;
        let entries = [base, trial];
        let firmware = fake.firmware(
            &[1, 42],
            Some(2),
            Some(1),
            &[(1, BASE), (2, TRIAL)],
            FIRST_BOOT,
        )?;
        firmware.rollback(fake.esp.path(), BASE, &entries)?;
        assert_eq!(boot_number("BootNext")?, None);
        assert_eq!(boot_order()?, [1, 42]);
        assert_eq!(
            fake.records()?,
            [format!("{BASE}.conf"), format!("{TRIAL}+0.conf")]
        );
        Ok(())
    }

    #[test]
    fn rollback_from_a_running_trial_marks_it_failed() -> Result<()> {
        let fake = FakeFirmware::new()?;
        let base = fake.record(BASE, "")?;
        let trial = fake.record(TRIAL, "+1")?;
        let entries = [base, trial];
        let firmware = fake.firmware(
            &[1, 42],
            None,
            Some(2),
            &[(1, BASE), (2, TRIAL)],
            SECOND_BOOT,
        )?;
        firmware.rollback(fake.esp.path(), BASE, &entries)?;
        assert_eq!(boot_order()?, [1, 42]);
        assert_eq!(
            fake.records()?,
            [format!("{BASE}.conf"), format!("{TRIAL}+0.conf")]
        );
        // An unrelated BootNext is never cancelled.
        let fake = FakeFirmware::new()?;
        let entries = [fake.record(BASE, "")?, fake.record(TRIAL, "+1")?];
        let firmware = fake.firmware(
            &[1, 42],
            Some(42),
            Some(1),
            &[(1, BASE), (2, TRIAL)],
            FIRST_BOOT,
        )?;
        assert!(firmware.rollback(fake.esp.path(), BASE, &entries).is_err());
        assert_eq!(boot_number("BootNext")?, Some(42));
        Ok(())
    }

    #[test]
    fn removal_drops_only_an_unselected_cps_option() -> Result<()> {
        let fake = FakeFirmware::new()?;
        let firmware = fake.firmware(
            &[1, 2, 42],
            None,
            Some(1),
            &[(1, BASE), (2, TRIAL)],
            FIRST_BOOT,
        )?;
        assert!(firmware.remove(BASE).is_err());
        firmware.remove(TRIAL)?;
        assert_eq!(boot_order()?, [1, 42]);
        assert_eq!(variable(&option_name(2))?, None);
        assert!(variable(&option_name(1))?.is_some());
        // A third-party option that boots the same UKI blocks removal.
        let fake = FakeFirmware::new()?;
        let mut firmware =
            fake.firmware(&[1], None, Some(1), &[(1, BASE), (2, TRIAL)], FIRST_BOOT)?;
        let mut copy = load_option(&device(), TRIAL)?;
        copy[6] ^= 1; // Another description.
        firmware.options.insert(9, copy);
        assert!(firmware.remove(TRIAL).is_err());
        assert!(variable(&option_name(2))?.is_some());
        Ok(())
    }

    #[test]
    fn builds_gpt_uki_path_and_identifies_other_references() -> Result<()> {
        let option = load_option(&device(), TRIAL)?;
        let node = 6 + ucs2(&format!("CPS {TRIAL}")).len();
        assert_eq!(&option[node..node + 4], &[4, 1, 42, 0]);
        assert_eq!(
            &option[node + 24..node + 40],
            &[
                0x33, 0x22, 0x11, 0x00, 0x55, 0x44, 0x77, 0x66, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
        assert!(references_path(&option, &efi_path(TRIAL).to_uppercase())?);
        assert!(!references_path(&option, &efi_path(BASE))?);
        assert!(references_path(&option[..option.len() - 1], &efi_path(TRIAL)).is_err());
        assert_eq!(preferred_order(&[1, 42, 2, 99], 2), [2, 1, 42, 99]);
        Ok(())
    }

    #[test]
    fn one_shot_selection_and_promotion_require_distinct_states() -> Result<()> {
        let mut firmware = Firmware {
            _lock: tempfile::tempfile()?,
            device: device(),
            order: vec![1, 42],
            next: Some(2),
            current: Some(1),
            options: BTreeMap::from([
                (1, load_option(&device(), BASE)?),
                (2, load_option(&device(), TRIAL)?),
            ]),
            boot_id: FIRST_BOOT.into(),
        };
        let mut entries = [entry(BASE, false), entry(TRIAL, true)];
        let policy = firmware
            .inventory(&mut entries)?
            .context("missing policy")?;
        assert_eq!(
            policy.selected(&entries).map(|e| e.id.as_str()),
            Some(TRIAL)
        );
        assert!(firmware.running_trial(&entries)?.is_none());
        assert!(firmware.can_arm(&entries[1])?);
        assert!(firmware.check_removal(TRIAL).is_err());

        firmware.next = None;
        firmware.current = Some(2);
        firmware.boot_id = SECOND_BOOT.into();
        let policy = firmware
            .inventory(&mut entries)?
            .context("missing policy")?;
        assert_eq!(policy.selected(&entries).map(|e| e.id.as_str()), Some(BASE));
        assert_eq!(entries[1].tries, Some(0));
        assert!(!firmware.can_arm(&entries[1])?);
        assert_eq!(
            firmware.running_trial(&entries)?.map(|e| e.id.as_str()),
            Some(TRIAL)
        );
        assert!(firmware.check_removal(TRIAL).is_err());

        firmware.order.insert(0, 2);
        assert!(firmware.inventory(&mut entries).is_err());
        entries[1].path = PathBuf::from(format!("{TRIAL}+0-1.conf"));
        let policy = firmware
            .inventory(&mut entries)?
            .context("missing policy")?;
        assert_eq!(
            policy.selected(&entries).map(|e| e.id.as_str()),
            Some(TRIAL)
        );
        assert!(entries[1].tries.is_none());

        firmware.order = vec![1, 42];
        firmware.next = Some(2);
        entries[1].path = PathBuf::from(format!("{TRIAL}+0.conf"));
        entries[1].tries = Some(0);
        let policy = firmware
            .inventory(&mut entries)?
            .context("missing policy")?;
        assert_eq!(policy.selected(&entries).map(|e| e.id.as_str()), Some(BASE));
        assert!(firmware.running_trial(&entries)?.is_none());
        assert!(firmware.check_next(&entries, false).is_err());
        firmware.check_next(&entries, true)?;
        assert!(firmware.require_no_next().is_err());
        firmware.next = Some(42);
        assert!(firmware.check_next(&entries, false).is_err());
        assert!(firmware.check_next(&entries, true).is_err());
        Ok(())
    }
}
