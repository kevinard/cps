use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    process::Stdio,
};

use anyhow::{Context, Result, ensure};
use object::{Object, ObjectSection, ReadCache, ReadRef};
use rustix::io::Errno;
use serde::Deserialize;

use crate::{image::root::Root, model::digest_reader};

use super::artifact_helper;

pub(super) const NAME: &str = "uki.efi";
pub(super) const HASH: &str = "# cps-uki-sha256 ";
pub(super) const CMDLINE: &str = "# cps-uki-cmdline ";

pub(super) fn prepare(
    esp: &Path,
    id: &str,
    cmdline: &str,
    kernel: (&mut File, &str),
    initrd: (&mut File, &str),
    registered_digest: Option<&str>,
) -> Result<(File, String)> {
    let path = format!("EFI/cps/{id}/{NAME}");
    let mut file = match Root::open(esp)?.file(&path) {
        Ok(file) => file,
        Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::NOENT) => {
            ensure!(
                registered_digest.is_none(),
                "registered UKI is missing at {path}"
            );
            let scratch = tempfile::Builder::new()
                .prefix(".cps-uki-")
                .tempdir_in("/var/tmp")?;
            let work = scratch.path();
            copy_input(kernel.0, &work.join("linux"), kernel.1)?;
            copy_input(initrd.0, &work.join("initrd"), initrd.1)?;
            let output = artifact_helper("ukify", work)
                .env("TMPDIR", work)
                .stdin(Stdio::null())
                .args([
                    "build",
                    "--config=/dev/null",
                    "--linux=linux",
                    "--initrd=initrd",
                    "--no-sign-kernel",
                    "--os-release=ID=canopa\nNAME=Canopa\n",
                    "--output=uki.efi",
                ])
                .arg(format!("--cmdline={cmdline}"))
                .output()
                .context("running ukify (install systemd-ukify)")?;
            ensure!(
                output.status.success(),
                "ukify failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let output = artifact_helper("sbctl", work)
                .env("SYSTEMD_ESP_PATH", esp)
                .stdin(Stdio::null())
                .arg("sign")
                .arg(work.join(NAME))
                .output()
                .context("running sbctl to sign the UKI")?;
            ensure!(
                output.status.success(),
                "sbctl sign failed with {}: {}{}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            Root::open(work)?.file(NAME)?
        }
        Err(error) => return Err(error).context("opening existing UKI"),
    };
    let sha256 = digest_reader(&mut file)?;
    if let Some(expected) = registered_digest {
        ensure!(
            sha256 == expected,
            "boot artifact digest mismatch at {path}"
        );
    }
    verify_contents(&mut file, cmdline, Some((kernel.1, initrd.1)))?;
    file.rewind()?;
    Ok((file, sha256))
}

fn copy_input(source: &mut File, path: &Path, expected: &str) -> Result<()> {
    let mut copy = File::create(path)?;
    io::copy(source, &mut copy)?;
    ensure!(
        digest_reader(File::open(path)?)? == expected,
        "UKI input changed while being copied"
    );
    Ok(())
}

pub(super) fn verify_signature(esp: &Path, path: &Path) -> Result<()> {
    #[derive(Deserialize)]
    struct Verification {
        is_signed: i8,
    }
    let output = artifact_helper("sbctl", Path::new("/"))
        .env("SYSTEMD_ESP_PATH", esp)
        .stdin(Stdio::null())
        .args(["--json", "verify"])
        .arg(path)
        .output()
        .context("running sbctl to verify the UKI")?;
    ensure!(
        output.status.success(),
        "sbctl verify failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let results: Option<Vec<Verification>> =
        serde_json::from_slice(&output.stdout).context("reading sbctl verification result")?;
    ensure!(
        matches!(results.as_deref(), Some([Verification { is_signed: 1 }])),
        "sbctl did not verify the UKI against the host's db key"
    );
    Ok(())
}

pub(super) fn verify_contents(
    file: &mut File,
    cmdline: &str,
    components: Option<(&str, &str)>,
) -> Result<()> {
    let (linux, initrd) = {
        let cache = ReadCache::new(&mut *file);
        let image = object::File::parse(&cache).context("reading UKI PE headers")?;
        ensure!(
            matches!(image, object::File::Pe32(_) | object::File::Pe64(_)),
            "expected a PE executable for the UKI"
        );
        ensure!(
            image.section_by_name(".profile").is_none(),
            "CPS UKIs require a single boot profile"
        );
        let section = |name| -> Result<(u64, u64)> {
            let mut sections = image
                .sections()
                .filter(|section| section.name().ok() == Some(name));
            let range = sections
                .next()
                .and_then(|section| section.file_range())
                .with_context(|| format!("missing UKI section {name}"))?;
            ensure!(
                range.1 > 0 && sections.next().is_none(),
                "invalid UKI section {name}"
            );
            Ok(range)
        };
        let (offset, size) = section(".cmdline")?;
        ensure!(size <= 64 * 1024, "UKI command line exceeds 64 KiB");
        let embedded = (&cache)
            .read_bytes_at(offset, size)
            .map_err(|()| anyhow::anyhow!("truncated UKI command line"))?;
        let embedded = std::str::from_utf8(embedded)?.trim_end_matches('\0');
        ensure!(
            embedded == cmdline,
            "UKI command line differs from the boot entry"
        );
        (section(".linux")?, section(".initrd")?)
    };
    if let Some((kernel, ramdisk)) = components {
        for ((offset, size), expected) in [(linux, kernel), (initrd, ramdisk)] {
            file.seek(SeekFrom::Start(offset))?;
            let mut section = (&mut *file).take(size);
            ensure!(
                digest_reader(&mut section)? == expected && section.limit() == 0,
                "UKI kernel or initramfs differs from the image"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::model::digest;

    const CMDLINE: &str =
        "root=UUID=ebcd29cb-f4ac-43ef-8f65-f7975a22d024 rootfstype=btrfs rootflags=subvolid=277 rw";

    /// Build a minimal PE32+ image with the given sections.
    fn pe(sections: &[(&str, &[u8])]) -> Vec<u8> {
        let headers = 64 + 4 + 20 + 240 + 40 * sections.len();
        let mut data_offset = headers.next_multiple_of(512);
        let mut image = vec![0u8; data_offset];
        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        image[64..68].copy_from_slice(b"PE\0\0");
        let coff = 68;
        image[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        image[coff + 2..coff + 4].copy_from_slice(&(sections.len() as u16).to_le_bytes());
        image[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
        image[coff + 18..coff + 20].copy_from_slice(&0x22u16.to_le_bytes());
        let optional = coff + 20;
        image[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        image[optional + 32..optional + 36].copy_from_slice(&4096u32.to_le_bytes());
        image[optional + 36..optional + 40].copy_from_slice(&512u32.to_le_bytes());
        image[optional + 60..optional + 64].copy_from_slice(&(headers as u32).to_le_bytes());
        image[optional + 108..optional + 112].copy_from_slice(&16u32.to_le_bytes());
        let mut table = optional + 240;
        let mut address = 0x1000u32;
        for (name, bytes) in sections {
            let mut header = [0u8; 40];
            header[..name.len()].copy_from_slice(name.as_bytes());
            let size = bytes.len() as u32;
            header[8..12].copy_from_slice(&size.to_le_bytes());
            header[12..16].copy_from_slice(&address.to_le_bytes());
            header[16..20].copy_from_slice(&size.to_le_bytes());
            header[20..24].copy_from_slice(&(data_offset as u32).to_le_bytes());
            header[36..40].copy_from_slice(&0x4000_0040u32.to_le_bytes());
            image[table..table + 40].copy_from_slice(&header);
            table += 40;
            image.extend_from_slice(bytes);
            image.resize(image.len().next_multiple_of(512), 0);
            data_offset = image.len();
            address += size.next_multiple_of(4096).max(4096);
        }
        image
    }

    fn file(bytes: &[u8]) -> Result<File> {
        let mut file = tempfile::tempfile()?;
        file.write_all(bytes)?;
        file.rewind()?;
        Ok(file)
    }

    fn valid() -> Vec<u8> {
        pe(&[
            (".linux", b"kernel image"),
            (".initrd", b"initramfs image"),
            (".cmdline", format!("{CMDLINE}\0\0").as_bytes()),
        ])
    }

    fn components() -> (String, String) {
        (digest(b"kernel image"), digest(b"initramfs image"))
    }

    #[test]
    fn uki_contents_bind_the_command_line_kernel_and_initramfs() -> Result<()> {
        let (linux, initrd) = components();
        verify_contents(&mut file(&valid())?, CMDLINE, Some((&linux, &initrd)))?;
        verify_contents(&mut file(&valid())?, CMDLINE, None)?;
        let error = |bytes: &[u8], cmdline: &str, components: Option<(&str, &str)>| {
            format!(
                "{:#}",
                verify_contents(&mut file(bytes).unwrap(), cmdline, components).unwrap_err()
            )
        };
        assert!(error(&valid(), "rw", None).contains("command line differs"));
        assert!(
            error(&valid(), CMDLINE, Some((&initrd, &linux))).contains("differs from the image")
        );
        let missing = pe(&[
            (".linux", b"kernel image"),
            (".cmdline", CMDLINE.as_bytes()),
        ]);
        assert!(error(&missing, CMDLINE, None).contains("missing UKI section .initrd"));
        let duplicate = pe(&[
            (".linux", b"kernel image"),
            (".linux", b"other kernel"),
            (".initrd", b"initramfs image"),
            (".cmdline", CMDLINE.as_bytes()),
        ]);
        assert!(error(&duplicate, CMDLINE, None).contains("invalid UKI section .linux"));
        let profiles = pe(&[
            (".linux", b"kernel image"),
            (".initrd", b"initramfs image"),
            (".cmdline", CMDLINE.as_bytes()),
            (".profile", b"ID=1"),
        ]);
        assert!(error(&profiles, CMDLINE, None).contains("single boot profile"));
        let long = "a".repeat(64 * 1024 + 1);
        let oversized = pe(&[
            (".linux", b"kernel image"),
            (".initrd", b"initramfs image"),
            (".cmdline", long.as_bytes()),
        ]);
        assert!(error(&oversized, &long, None).contains("exceeds 64 KiB"));
        assert!(verify_contents(&mut file(b"\x7fELF not a PE")?, CMDLINE, None).is_err());
        Ok(())
    }

    #[test]
    fn registered_uki_is_reused_only_with_its_recorded_digest() -> Result<()> {
        let esp = tempfile::tempdir()?;
        let id = "cps-a47593c2-e985-42d2-92b6-44c7057e1e96-2";
        let (linux, initrd) = components();
        let mut kernel = file(b"kernel image")?;
        let mut ramdisk = file(b"initramfs image")?;
        let mut run = |registered: Option<&str>| {
            prepare(
                esp.path(),
                id,
                CMDLINE,
                (&mut kernel, &linux),
                (&mut ramdisk, &initrd),
                registered,
            )
        };
        let missing = run(Some(&"a".repeat(64))).unwrap_err().to_string();
        assert!(missing.contains("registered UKI is missing"), "{missing}");
        let directory = esp.path().join("EFI/cps").join(id);
        std::fs::create_dir_all(&directory)?;
        std::fs::write(directory.join(NAME), valid())?;
        let (_, sha256) = run(Some(&digest(&valid())))?;
        assert_eq!(sha256, digest(&valid()));
        let (_, sha256) = run(None)?;
        assert_eq!(sha256, digest(&valid()));
        let changed = run(Some(&"b".repeat(64))).unwrap_err().to_string();
        assert!(changed.contains("digest mismatch"), "{changed}");
        Ok(())
    }
}
