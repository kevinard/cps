use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use clap::Args;
use object::{Object, ReadCache};
use rustix::fs::{CWD, FlockOperation, RenameFlags, flock, renameat_with};

use super::livepatch::{PACKAGE_DIR, module_name, validate_name};
use crate::{
    kdl_format::{manifest_text, parse_manifest},
    model::{Version, validate_name as validate_package_name, validate_publisher},
};

#[derive(Args)]
pub(crate) struct Build {
    /// Clean, configured, in-tree Linux build used to reproduce the target
    #[arg(long)]
    kernel_tree: PathBuf,
    /// ELF vmlinux archived from the exact kernel release being patched
    #[arg(long)]
    vmlinux: PathBuf,
    /// Module name, using ASCII letters, digits and underscores
    #[arg(long)]
    module: String,
    /// Kernel-trusted module signing private key, kept outside the build tree
    #[arg(long)]
    signing_key: PathBuf,
    /// Certificate corresponding to the module signing key
    #[arg(long)]
    signing_cert: PathBuf,
    /// Name of the CPS publisher
    #[arg(long)]
    publisher: String,
    /// Name of the CPS package
    #[arg(long)]
    package: String,
    /// CPS package version
    #[arg(long)]
    version: String,
    /// New directory for manifest.kdl and the package prototype tree
    #[arg(short, long)]
    output: PathBuf,
    /// Number of parallel kernel build jobs
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..))]
    jobs: Option<u32>,
    /// Reviewed cumulative kernel source patches, in application order
    #[arg(required = true, value_name = "PATCH")]
    patches: Vec<PathBuf>,
}

impl Build {
    pub(super) fn run(self) -> Result<()> {
        validate_name(&self.module)?;
        validate_package_name(&self.package)?;
        validate_publisher(&self.publisher)?;
        let version: Version = self.version.parse()?;
        let tree = self
            .kernel_tree
            .canonicalize()
            .context("opening the kernel tree")?;
        let tree_lock = File::open(&tree)?;
        flock(&tree_lock, FlockOperation::NonBlockingLockExclusive)
            .context("locking the kernel tree for the livepatch build")?;
        let key = self
            .signing_key
            .canonicalize()
            .context("opening the signing key")?;
        let cert = self
            .signing_cert
            .canonicalize()
            .context("opening the signing certificate")?;
        ensure!(
            !key.starts_with(&tree),
            "keep the signing key outside the kernel build tree"
        );
        let (build_id, arch) = kernel_identity(&self.vmlinux)?;
        ensure!(
            kernel_identity(&tree.join("vmlinux"))? == (build_id.clone(), arch),
            "the build tree's vmlinux differs from the deployed kernel build ID or architecture"
        );
        let release = fs::read_to_string(tree.join("include/config/kernel.release"))
            .context("reading the kernel build release")?;
        let patches = self
            .patches
            .iter()
            .map(|p| p.canonicalize())
            .collect::<std::io::Result<Vec<_>>>()?;
        let output_parent = self
            .output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let output = output_parent.canonicalize()?.join(
            self.output
                .file_name()
                .context("missing output directory name")?,
        );
        ensure!(
            !output.starts_with(&tree),
            "put the package output outside the kernel build tree"
        );
        ensure!(
            output
                .symlink_metadata()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "output already exists or is inaccessible"
        );
        let stage = tempfile::Builder::new()
            .prefix(".cps-livepatch-")
            .tempdir_in(output.parent().unwrap())?;
        let path = format!("{PACKAGE_DIR}/{build_id}/{}.ko", self.module);
        let module = stage.path().join("proto").join(&path);
        fs::create_dir_all(module.parent().unwrap())?;
        let mut build = Command::new(tree.join("scripts/livepatch/klp-build"));
        build.current_dir(&tree).arg("--output").arg(&module);
        if let Some(jobs) = self.jobs {
            build.arg("--jobs").arg(jobs.to_string());
        }
        run(
            build.args(patches),
            "building the cumulative Linux livepatch with klp-build",
        )?;
        ensure!(
            module_name(&mut File::open(&module)?, release.trim())? == self.module,
            "klp-build produced a module with a different name"
        );
        run(
            Command::new(tree.join("scripts/sign-file"))
                .current_dir(&tree)
                .arg("sha256")
                .arg(key)
                .arg(cert)
                .arg(&module),
            "signing the livepatch module with scripts/sign-file",
        )?;
        let mut signed = File::open(&module)?;
        const SIGNATURE_MARKER: &[u8] = b"~Module signature appended~\n";
        let mut marker = [0; SIGNATURE_MARKER.len()];
        signed.seek(SeekFrom::End(-(marker.len() as i64)))?;
        signed.read_exact(&mut marker)?;
        ensure!(
            marker == SIGNATURE_MARKER,
            "scripts/sign-file did not append a Linux module signature"
        );
        let manifest = parse_manifest(&format!(
            "schema-version 1 kind=\"package-manifest\"\npackage {:?} publisher={:?} version={:?} architecture={arch:?} {{\n    file {path:?} mode=\"0644\"\n}}\n",
            self.package,
            self.publisher,
            version.to_string(),
        ))?;
        manifest.validate(false)?;
        fs::write(stage.path().join("manifest.kdl"), manifest_text(&manifest))?;
        renameat_with(CWD, stage.path(), CWD, &output, RenameFlags::NOREPLACE)
            .with_context(|| format!("installing package output at {}", output.display()))?;
        println!(
            "Prepared {} for kernel build {build_id} in {}",
            self.package,
            output.display()
        );
        Ok(())
    }
}

fn kernel_identity(path: &Path) -> Result<(String, &'static str)> {
    let cache =
        ReadCache::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let elf = object::File::parse(&cache).context("reading vmlinux ELF headers")?;
    ensure!(
        elf.format() == object::BinaryFormat::Elf && elf.kind() == object::ObjectKind::Executable,
        "expected an ELF vmlinux executable"
    );
    let arch = match elf.architecture() {
        object::Architecture::X86_64 => "x86_64",
        arch => anyhow::bail!("livepatch building does not support {arch:?}"),
    };
    let id = elf
        .build_id()?
        .filter(|id| !id.is_empty())
        .context("vmlinux has no GNU build ID")?;
    Ok((hex::encode(id), arch))
}

fn run(command: &mut Command, action: &str) -> Result<()> {
    let status = command.status().with_context(|| action.to_owned())?;
    ensure!(status.success(), "{action} failed with {status}");
    Ok(())
}
