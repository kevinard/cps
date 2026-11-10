//! The systemd-boot release that reads the CPS selection in loader.conf.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};

/// The first systemd-boot release that reads the `preferred` key.
pub(super) const PREFERRED_SINCE: u32 = 262;
const MARKER: &[u8] = b"#### LoaderInfo: systemd-boot ";
const MAX_LOADER_BYTES: u64 = 64 * 1024 * 1024;
const LOADER_INFO: &str = "LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
const EFIVARS: &str = "/sys/firmware/efi/efivars";

/// A systemd-boot release, as embedded in its EFI image or reported by firmware.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct LoaderVersion {
    pub major: u32,
    pub text: String,
}

impl LoaderVersion {
    /// Parse `262.1`, `v262.1` or `257.5-1-arch`, as systemd-boot formats its version.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let digits = text.strip_prefix('v').unwrap_or(text);
        let end = digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len());
        Some(Self {
            major: digits[..end].parse().ok()?,
            text: text.to_owned(),
        })
    }
}

/// The lowest systemd-boot release named by the `LoaderInfo` markers in an EFI image.
pub(super) fn binary_version(bytes: &[u8]) -> Result<Option<LoaderVersion>> {
    let mut lowest: Option<LoaderVersion> = None;
    let mut rest = bytes;
    while let Some(start) = rest
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    {
        rest = &rest[start + MARKER.len()..];
        let end = rest
            .iter()
            .take(64)
            .position(|byte| *byte == b' ')
            .context("unterminated systemd-boot version marker")?;
        let version = std::str::from_utf8(&rest[..end])
            .ok()
            .and_then(LoaderVersion::parse)
            .context("unreadable systemd-boot version marker")?;
        if lowest
            .as_ref()
            .is_none_or(|known| version.major < known.major)
        {
            lowest = Some(version);
        }
    }
    Ok(lowest)
}

/// The systemd-boot binaries on the ESP that firmware can start, by path on the
/// ESP, with their releases.
///
/// Every `EFI/*/*.efi` file is a candidate, since a firmware boot option can
/// name any of them, as `EFI/systemd`, `EFI/BOOT`, and vendor directories do.
/// CPS's own boot files and unified kernel images are skipped. Other loaders,
/// such as shim, carry no systemd-boot marker and are skipped too.
pub(super) fn esp_loaders(esp: &Path) -> Result<Vec<(PathBuf, LoaderVersion)>> {
    let mut loaders = Vec::new();
    let Some(efi) = child(esp, "EFI")? else {
        return Ok(loaders);
    };
    let mut directories = Vec::new();
    for entry in fs::read_dir(&efi)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if entry.file_type()?.is_dir() && !matches!(name.as_str(), "cps" | "cps-uefi" | "linux") {
            directories.push(entry.path());
        }
    }
    directories.sort();
    for directory in directories {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !name.ends_with(".efi") || !entry.file_type()?.is_file() {
                continue;
            }
            let mut bytes = Vec::new();
            File::open(entry.path())?
                .take(MAX_LOADER_BYTES)
                .read_to_end(&mut bytes)?;
            let path = entry.path();
            let path = path.strip_prefix(esp).unwrap_or(&path).to_owned();
            if let Some(version) =
                binary_version(&bytes).with_context(|| format!("reading {}", path.display()))?
            {
                loaders.push((path, version));
            }
        }
    }
    Ok(loaders)
}

/// The release of the systemd-boot that started this system, from its EFI variable.
pub(super) fn running_version(efivars: &Path) -> Result<Option<LoaderVersion>> {
    let bytes = match fs::read(efivars.join(LOADER_INFO)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading the LoaderInfo EFI variable"),
    };
    // efivarfs prefixes the UTF-16LE value with four attribute bytes.
    let units: Vec<u16> = bytes
        .get(4..)
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    let info = String::from_utf16(&units).context("LoaderInfo is not UTF-16")?;
    Ok(info
        .strip_prefix("systemd-boot ")
        .and_then(LoaderVersion::parse))
}

/// Refuse a trial selection that systemd-boot would ignore.
///
/// After a rollback pins `default`, CPS selects a new trial with `preferred`.
/// Older loaders skip that key and boot the default, so the trial would never
/// run. The binaries on the ESP decide the next boot. Without one, the
/// running loader's `LoaderInfo` variable stands in.
pub(super) fn require_preferred(esp: &Path) -> Result<()> {
    check_preferred(esp, Some(Path::new(EFIVARS)))
}

pub(in crate::boot) fn check_preferred(esp: &Path, efivars: Option<&Path>) -> Result<()> {
    let update = format!(
        "CPS selects a trial after a rollback with the loader.conf `preferred` key, which \
         systemd-boot reads from version {PREFERRED_SINCE}. Install systemd-boot \
         {PREFERRED_SINCE} or newer on the ESP, for example with `bootctl update`, then retry"
    );
    let loaders = esp_loaders(esp)?;
    if loaders.is_empty() {
        let running = efivars.map(running_version).transpose()?.flatten();
        let Some(version) = running else {
            bail!(
                "found no systemd-boot binary under EFI on {}. {update}",
                esp.display()
            );
        };
        ensure!(
            version.major >= PREFERRED_SINCE,
            "this system was started by systemd-boot {}, and the ESP holds no systemd-boot \
             binary to replace it. {update}",
            version.text
        );
        return Ok(());
    }
    for (path, version) in loaders {
        ensure!(
            version.major >= PREFERRED_SINCE,
            "{} on the ESP is systemd-boot {}, which would ignore the trial and boot the \
             pinned default. {update}",
            path.display(),
            version.text
        );
    }
    Ok(())
}

/// The name of a FAT directory entry, matched case-insensitively.
fn child(parent: &Path, name: &str) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(name)
            && entry.file_type()?.is_dir()
        {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loader(version: &str) -> Vec<u8> {
        let mut bytes = b"MZ\0\0padding".to_vec();
        bytes.extend_from_slice(format!("#### LoaderInfo: systemd-boot {version} ####").as_bytes());
        bytes.extend_from_slice(b"\0more");
        bytes
    }

    #[test]
    fn reads_the_release_embedded_in_systemd_boot() -> Result<()> {
        for (text, major) in [
            ("262.1", 262),
            ("v262.1", 262),
            ("257.5-1-arch", 257),
            ("263", 263),
        ] {
            let version = binary_version(&loader(text))?.context("missing version")?;
            assert_eq!(
                version,
                LoaderVersion {
                    major,
                    text: text.into()
                }
            );
        }
        assert_eq!(binary_version(b"MZ shim without a marker")?, None);
        // Two markers report the older release.
        let mut both = loader("v262.1");
        both.extend(loader("255.4"));
        assert_eq!(
            binary_version(&both)?.map(|version| version.major),
            Some(255)
        );
        assert!(binary_version(b"#### LoaderInfo: systemd-boot unknown ####").is_err());
        assert!(binary_version(b"#### LoaderInfo: systemd-boot 262").is_err());
        Ok(())
    }

    #[test]
    fn reads_the_running_loader_from_efivarfs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(running_version(dir.path())?, None);
        let mut value = vec![6, 0, 0, 0];
        for unit in "systemd-boot 260.2-2-arch".encode_utf16().chain([0]) {
            value.extend(unit.to_le_bytes());
        }
        fs::write(dir.path().join(LOADER_INFO), &value)?;
        let version = running_version(dir.path())?.context("missing version")?;
        assert_eq!(
            (version.major, version.text.as_str()),
            (260, "260.2-2-arch")
        );
        Ok(())
    }

    #[test]
    fn trial_selection_requires_a_loader_that_reads_preferred() -> Result<()> {
        let esp = tempfile::tempdir()?;
        let efivars = tempfile::tempdir()?;
        let message = check_preferred(esp.path(), Some(efivars.path()))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("found no systemd-boot binary"),
            "{message}"
        );
        assert!(message.contains("bootctl update"), "{message}");

        // Without a binary on the ESP, the running loader decides.
        let mut value = vec![6, 0, 0, 0];
        for unit in "systemd-boot 255.4".encode_utf16().chain([0]) {
            value.extend(unit.to_le_bytes());
        }
        fs::write(efivars.path().join(LOADER_INFO), &value)?;
        let message = check_preferred(esp.path(), Some(efivars.path()))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("started by systemd-boot 255.4"),
            "{message}"
        );

        // FAT names match case-insensitively. A shim fallback has no marker.
        fs::create_dir_all(esp.path().join("efi/BOOT"))?;
        fs::write(esp.path().join("efi/BOOT/BOOTX64.EFI"), b"MZ shim")?;
        fs::create_dir_all(esp.path().join("efi/systemd"))?;
        fs::write(
            esp.path().join("efi/systemd/systemd-bootx64.efi"),
            loader("v262.1"),
        )?;
        check_preferred(esp.path(), Some(efivars.path()))?;

        // Every systemd-boot copy that firmware can start must read the key.
        fs::write(esp.path().join("efi/BOOT/BOOTX64.EFI"), loader("261.3"))?;
        let message = check_preferred(esp.path(), None).unwrap_err().to_string();
        assert!(
            message.contains("efi/BOOT/BOOTX64.EFI on the ESP is systemd-boot 261.3"),
            "{message}"
        );
        assert!(message.contains("version 262"), "{message}");

        // A vendor directory counts too; CPS's own boot files do not.
        fs::write(esp.path().join("efi/BOOT/BOOTX64.EFI"), loader("262.1"))?;
        fs::create_dir_all(esp.path().join("efi/cps/cps-x-1"))?;
        fs::write(esp.path().join("efi/cps/cps-x-1/uki.efi"), loader("200"))?;
        check_preferred(esp.path(), None)?;
        fs::create_dir_all(esp.path().join("efi/canopa"))?;
        fs::write(esp.path().join("efi/canopa/grubx64.efi"), loader("257.9"))?;
        let message = check_preferred(esp.path(), None).unwrap_err().to_string();
        assert!(
            message.contains("efi/canopa/grubx64.efi on the ESP is systemd-boot 257.9"),
            "{message}"
        );
        Ok(())
    }
}
