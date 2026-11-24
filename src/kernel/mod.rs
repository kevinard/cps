//! Explicit runtime operations. Package transactions continue to stage kernel changes.

pub(crate) mod handover;
mod livepatch;
mod livepatch_build;

use std::{
    fs::{File, OpenOptions},
    io::{self, Seek},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};

use crate::model::{digest_reader, validate_digest};

#[derive(Subcommand)]
pub(crate) enum KernelCommand {
    /// Inspect or change Linux livepatches on the running kernel
    Livepatch {
        #[command(subcommand)]
        command: livepatch::Command,
    },
    /// Experiment with kexec handover of one cooperating workload's memfd
    Handover {
        #[command(subcommand)]
        command: handover::Command,
    },
}

impl KernelCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Livepatch { command } => command.run(),
            Self::Handover { command } => command.run(),
        }
    }
}

/// Pass the exact bytes we checked to the kernel, even if the source changes.
fn pinned_file(path: &Path, sha256: &str) -> Result<File> {
    let input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    ensure!(
        input.metadata()?.is_file(),
        "{} is not a regular file",
        path.display()
    );
    pinned_reader(input, sha256, &path.display().to_string())
}

pub(crate) fn pinned_reader(input: impl io::Read, sha256: &str, label: &str) -> Result<File> {
    validate_digest(sha256)?;
    let mut file = sealed_payload(input, label)?;
    ensure!(
        digest_reader(&mut file)? == sha256,
        "SHA-256 mismatch for {label}"
    );
    file.rewind()?;
    Ok(file)
}

pub(crate) fn sealed_payload(mut input: impl io::Read, label: &str) -> Result<File> {
    let mut file = File::from(memfd_create(
        "cps-kernel-payload",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?);
    io::copy(&mut input, &mut file).with_context(|| format!("reading {label}"))?;
    fcntl_add_seals(
        &file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )?;
    file.rewind()?;
    Ok(file)
}
