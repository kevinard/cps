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
