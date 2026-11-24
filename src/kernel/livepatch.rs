use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use object::{NativeEndian, Object, ObjectSection, elf, read::elf::NoteIterator};
use rustix::fs::{FlockOperation, flock};
use serde::Serialize;

use crate::{
    image::Image,
    model::{Action, Node, validate_name as validate_package_name},
};

use super::{livepatch_build, pinned_file, pinned_reader};

pub(super) const PACKAGE_DIR: &str = "usr/lib/livepatch";

const LIVEPATCH: &str = "/sys/kernel/livepatch";
const SIGNATURE_ENFORCEMENT: &str = "/sys/module/module/parameters/sig_enforce";

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Build and sign a module and prepare it for signed CPS publication
    Build(livepatch_build::Build),
    /// Apply the matching module from an installed CPS package
    ApplyPackage {
        /// Installed package name
        package: String,
        /// Seconds to wait for the kernel transition
        #[arg(long, default_value_t = 30)]
        timeout: u32,
    },
    /// Show the running kernel and its livepatch transitions
    Status {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Load a kernel-trusted livepatch and wait for the transition
    Apply {
        /// Path to an uncompressed, signed livepatch module
        #[arg(value_name = "MODULE")]
        path: PathBuf,
        /// SHA-256 from a trusted source for this exact module
        #[arg(long)]
        sha256: String,
        /// GNU build ID of the kernel this patch targets, from a trusted source
        #[arg(long)]
        kernel_build_id: String,
        /// Seconds to wait for the kernel transition
        #[arg(long, default_value_t = 30)]
        timeout: u32,
    },
    /// Disable a livepatch and wait for the transition
    Disable {
        /// Name shown by livepatch status
        name: String,
        /// Seconds to wait for the kernel transition
        #[arg(long, default_value_t = 30)]
        timeout: u32,
    },
}

impl Command {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Build(args) => args.run(),
            Self::Status { json } => status(&running_release()?, json),
            Self::Apply {
                path,
                sha256,
                kernel_build_id,
                timeout,
            } => {
                let _lock = lock()?;
                let running = running_build_id()?;
                ensure!(
                    running.eq_ignore_ascii_case(&kernel_build_id),
                    "the livepatch targets kernel build {kernel_build_id}, running build is {running}"
                );
                apply(pinned_file(&path, &sha256)?, None, timeout)
            }
            Self::ApplyPackage { package, timeout } => {
                let _lock = lock()?;
                let running = running_build_id()?;
                let (module, name) =
                    installed_module(&Image::open_query(Path::new("/"))?, &package, &running)?;
                apply(module, Some(&name), timeout)
            }
            Self::Disable { name, timeout } => {
                let release = running_release()?;
                validate_name(&name)?;
                let _lock = lock()?;
                let patch = Path::new(LIVEPATCH).join(&name);
                let enabled = patch.join("enabled");
                match read_flag(&enabled)? {
                    Some(true) => fs::write(&enabled, b"0\n")
                        .with_context(|| format!("disabling livepatch {name}"))?,
                    Some(false) => {}
                    None => anyhow::bail!("no livepatch named {name} is loaded"),
                }
                wait(&patch, false, timeout)?;
                println!("Livepatch {name} disabled on Linux {release}");
                Ok(())
            }
        }
    }
}

fn running_release() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/osrelease")?
        .trim()
        .to_owned())
}

fn running_build_id() -> Result<String> {
    build_id()?.context("the running kernel has no exposed GNU build ID")
}

fn package_module<'a>(
    actions: &'a [Action],
    build_id: &str,
) -> Result<(&'a str, &'a str, &'a str)> {
    let prefix = format!("{PACKAGE_DIR}/{build_id}/");
    let mut matches = actions.iter().filter_map(|action| match action {
        Action::File {
            path,
            sha256: Some(digest),
            preserve: false,
            ..
        } => {
            let name = path.strip_prefix(&prefix)?.strip_suffix(".ko")?;
            Some((path.as_str(), name, digest.as_str()))
        }
        _ => None,
    });
    let module = matches
        .next()
        .with_context(|| format!("package has no livepatch for kernel build {build_id}"))?;
    validate_name(module.1)?;
    ensure!(
        matches.next().is_none(),
        "package has several modules for kernel build {build_id}"
    );
    Ok(module)
}

fn installed_module(image: &Image, package: &str, build_id: &str) -> Result<(File, String)> {
    validate_package_name(package)?;
    image.require_clean()?;
    let installed = image
        .installed_package(package)?
        .with_context(|| format!("package {package} is not installed"))?;
    image.require_catalog_admission(package, &installed)?;
    let (path, name, digest) = package_module(&installed.published.manifest.action, build_id)?;
    let managed = image
        .managed_path(path)?
        .context("livepatch module is not managed by CPS")?;
    ensure!(
        managed.owners.contains(package)
            && !managed.preserve
            && matches!(&managed.node, Node::File { sha256, .. } if sha256 == digest),
        "installed livepatch path differs from package {package}"
    );
    Ok((
        pinned_reader(image.root.file(path)?, digest, path)?,
        name.to_owned(),
    ))
}

fn apply(mut module: File, expected_name: Option<&str>, timeout: u32) -> Result<()> {
    let enforcement = fs::read_to_string(SIGNATURE_ENFORCEMENT)
        .context("checking kernel module signature enforcement")?;
    ensure!(
        enforcement.trim() == "Y",
        "enable kernel module signature enforcement before applying a livepatch"
    );
    let release = running_release()?;
    let name = module_name(&mut module, &release)?;
    ensure!(
        expected_name.is_none_or(|expected| expected == name),
        "module name differs from its packaged path"
    );
    module.rewind()?;
    let patch = Path::new(LIVEPATCH).join(&name);
    ensure!(
        !patch.try_exists()?,
        "livepatch {name} is already registered"
    );
    // SAFETY: the file holds the checked module and the empty C string outlives the call.
    let result =
        unsafe { libc::syscall(libc::SYS_finit_module, module.as_raw_fd(), c"".as_ptr(), 0) };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("loading the signed livepatch module");
    }
    wait(&patch, true, timeout)?;
    println!("Livepatch {name} enabled on Linux {release}");
    Ok(())
}

fn lock() -> Result<File> {
    ensure!(
        Path::new(LIVEPATCH).is_dir(),
        "the running kernel does not expose Linux Livepatch"
    );
    ensure!(
        rustix::process::geteuid().is_root(),
        "changing livepatches requires root"
    );
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/run/cps-livepatch.lock")?;
    flock(&file, FlockOperation::LockExclusive).context("locking livepatch operations")?;
    Ok(file)
}

pub(super) fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid livepatch module name {name:?}"
    );
    Ok(())
}

pub(super) fn module_name(file: &mut File, release: &str) -> Result<String> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let elf = object::File::parse(&*bytes).context("reading the livepatch ELF module")?;
    ensure!(
        elf.format() == object::BinaryFormat::Elf && elf.kind() == object::ObjectKind::Relocatable,
        "expected an ELF kernel module"
    );
    let metadata = elf
        .section_by_name(".modinfo")
        .context("module lacks .modinfo")?
        .data()?;
    let mut info = metadata.split(|&b| b == 0);
    ensure!(
        info.clone().any(|field| field == b"livepatch=Y"),
        "module does not declare itself a livepatch"
    );
    let vermagic = info
        .clone()
        .find_map(|field| field.strip_prefix(b"vermagic="))
        .context("module lacks vermagic")?;
    let target = std::str::from_utf8(vermagic)?
        .split_ascii_whitespace()
        .next();
    ensure!(
        target == Some(release),
        "module targets Linux {}, running Linux is {release}",
        target.unwrap_or("(unknown)")
    );
    let name = info
        .find_map(|field| field.strip_prefix(b"name="))
        .context("module lacks a name")?;
    let name = std::str::from_utf8(name)?;
    validate_name(name)?;
    Ok(name.into())
}

#[derive(Serialize)]
struct Patch {
    name: String,
    enabled: bool,
    transition: bool,
}

fn status(release: &str, json: bool) -> Result<()> {
    let build_id = build_id()?;
    let entries = match fs::read_dir(LIVEPATCH) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("reading the running kernel's livepatches"),
    };
    let supported = entries.is_some();
    let mut patches = Vec::new();
    if let Some(entries) = entries {
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if let (Some(enabled), Some(transition)) = (
                read_flag(&path.join("enabled"))?,
                read_flag(&path.join("transition"))?,
            ) {
                patches.push(Patch {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    enabled,
                    transition,
                });
            }
        }
    }
    patches.sort_by(|a, b| a.name.cmp(&b.name));
    if json {
        println!(
            "{}",
            serde_json::json!({"release": release, "build_id": build_id, "supported": supported, "patches": patches})
        );
    } else {
        println!("Linux {release}");
        if let Some(build_id) = build_id {
            println!("Build ID {build_id}");
        }
        if !supported {
            println!("Livepatch is unavailable");
        } else if patches.is_empty() {
            println!("No livepatches registered");
        }
        for patch in patches {
            let state = match (patch.transition, patch.enabled) {
                (true, true) => "enabling",
                (true, false) => "disabling",
                (false, true) => "enabled",
                (false, false) => "disabled",
            };
            println!("{} {state}", patch.name);
        }
    }
    Ok(())
}

fn build_id() -> Result<Option<String>> {
    let bytes = match fs::read("/sys/kernel/notes") {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading the running kernel's ELF notes"),
    };
    let endian = NativeEndian::default();
    // Linux exports these note records directly, with four-byte alignment.
    let mut notes = NoteIterator::<elf::FileHeader64<NativeEndian>>::new(endian, 4, &bytes)?;
    while let Some(note) = notes.next()? {
        if note.name() == elf::ELF_NOTE_GNU && note.n_type(endian) == elf::NT_GNU_BUILD_ID {
            return Ok(Some(hex::encode(note.desc())));
        }
    }
    Ok(None)
}

fn read_flag(path: &Path) -> Result<Option<bool>> {
    let value = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    match value.trim() {
        "0" => Ok(Some(false)),
        "1" => Ok(Some(true)),
        value => anyhow::bail!("unexpected value {value:?} at {}", path.display()),
    }
}

fn wait(patch: &Path, enabled: bool, timeout: u32) -> Result<()> {
    let start = Instant::now();
    loop {
        if read_flag(&patch.join("transition"))? != Some(true) {
            ensure!(
                read_flag(&patch.join("enabled"))?.unwrap_or(false) == enabled,
                "livepatch changed state unexpectedly"
            );
            return Ok(());
        }
        ensure!(
            start.elapsed() < Duration::from_secs(timeout.into()),
            "livepatch still transitioning after {timeout} seconds. Check `cps kernel livepatch status`"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
