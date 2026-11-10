use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Seek,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};

use crate::{
    image::{Image, Installed, Managed, Settings, root::Root},
    model::{Node, digest, digest_reader},
};

const LOADER_ENTRY_PLUGIN: &str = "/usr/lib/kernel/install.d/90-loaderentry.install";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Kernel {
    pub release: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub initrd: bool,
}

impl Kernel {
    pub fn validate(&self) -> Result<()> {
        let release = &self.release;
        ensure!(
            release.len() <= 128
                && release.starts_with(|c: char| c.is_ascii_alphanumeric())
                && release
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c)),
            "invalid kernel release {release:?}"
        );
        Ok(())
    }

    pub fn image(&self) -> String {
        format!("usr/lib/modules/{}/vmlinuz", self.release)
    }

    /// Where the kernel's initramfs is, whether packaged or generated.
    pub fn initrd_path(&self) -> String {
        format!("usr/lib/modules/{}/initrd", self.release)
    }

    /// The packaged initramfs, if the kernel package ships one.
    pub fn initrd(&self) -> Option<String> {
        self.initrd.then(|| self.initrd_path())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifacts {
    pub kernel: Kernel,
    pub(super) linux_sha256: String,
    pub(super) initrd_sha256: Option<String>,
    /// An installed trigger generates the initramfs. Its digest is known
    /// only after the trigger runs, so the prepared artifacts record it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) generated_initrd: bool,
}

/// The digest of a generated initramfs, recorded in its prepared artifacts.
const GENERATED_INITRD_DIGEST: &str = "initrd.sha256";

pub(crate) fn selected(
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
) -> Result<Option<Artifacts>> {
    let mut selected = None;
    let outputs = crate::triggers::active_outputs(installed, paths);
    for (name, package) in installed {
        let Some(kernel) = &package.published.manifest.kernel else {
            continue;
        };
        kernel.validate()?;
        let Some(linux_sha256) = owned_digest(paths, name, &kernel.image()) else {
            // A declaration follows its kernel file's architecture and facet.
            continue;
        };
        let initrd_sha256 = kernel
            .initrd()
            .map(|path| {
                owned_digest(paths, name, &path).with_context(|| {
                    format!(
                        "selected kernel {} requires its initrd {path}",
                        kernel.release
                    )
                })
            })
            .transpose()?;
        ensure!(
            selected.is_none(),
            "multiple kernels selected for the boot environment"
        );
        // A packaged initramfs conflicts with any trigger output that covers it.
        let generated_initrd =
            initrd_sha256.is_none() && crate::triggers::generated(&outputs, &kernel.initrd_path());
        selected = Some(Artifacts {
            kernel: kernel.clone(),
            linux_sha256,
            initrd_sha256,
            generated_initrd,
        });
    }
    Ok(selected)
}

pub(crate) fn transition(
    previous: Option<Artifacts>,
    next: Option<Artifacts>,
) -> Result<Option<Artifacts>> {
    ensure!(
        previous.is_none() || next.is_some(),
        "transaction would remove the image's selected kernel"
    );
    Ok(next.filter(|selected| Some(selected) != previous.as_ref()))
}

fn owned_digest(paths: &BTreeMap<String, Managed>, name: &str, path: &str) -> Option<String> {
    let managed = paths.get(path)?;
    match &managed.node {
        Node::File { sha256, .. } if managed.owners.contains(name) => Some(sha256.clone()),
        _ => None,
    }
}

impl Artifacts {
    fn directory(&self) -> Result<String> {
        Ok(format!("kernel-{}", digest(&serde_json::to_vec(self)?)))
    }

    pub fn open_prepared(&self, state: &Root) -> Result<(File, Option<File>)> {
        let dir = self.directory()?;
        let linux = checked_file(state, &format!("{dir}/linux"), &self.linux_sha256)
            .with_context(|| format!("opening prepared kernel {}", self.kernel.release))?;
        let initrd = self
            .prepared_initrd_sha256(state)?
            .map(|hash| checked_file(state, &format!("{dir}/initrd"), &hash))
            .transpose()
            .with_context(|| format!("opening prepared initrd for {}", self.kernel.release))?;
        Ok((linux, initrd))
    }

    /// The digest of the prepared initramfs, packaged or generated.
    pub fn prepared_initrd_sha256(&self, state: &Root) -> Result<Option<String>> {
        if !self.generated_initrd {
            return Ok(self.initrd_sha256.clone());
        }
        let path = format!("{}/{GENERATED_INITRD_DIGEST}", self.directory()?);
        let recorded = String::from_utf8(state.read_file(&path).with_context(|| {
            format!(
                "opening the generated initramfs digest for {}",
                self.kernel.release
            )
        })?)?;
        crate::model::validate_digest(&recorded)?;
        Ok(Some(recorded))
    }

    pub fn prepare(&self, image: &Image, path: &Path, settings: &Settings) -> Result<()> {
        let root = &image.root;
        self.kernel.validate()?;
        let directory = self.directory()?;
        let state_path = path.join(crate::image::STATE_DIRECTORY);
        let state_dir = image.state_dir.as_file();
        let final_path = state_path.join(&directory);
        let exists = match fs::symlink_metadata(&final_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        // A generated initramfs changes whenever its trigger runs, so its
        // prepared artifacts are rebuilt from the image's current output.
        let generated = self
            .generated_initrd
            .then(|| -> Result<String> {
                let path = self.kernel.initrd_path();
                let hash = digest_reader(
                    root.file(&path)
                        .with_context(|| format!("opening the generated initramfs at {path}"))?,
                )?;
                Ok(hash)
            })
            .transpose()?;
        if exists
            && generated.as_ref().is_none_or(|hash| {
                self.prepared_initrd_sha256(&image.state_dir)
                    .ok()
                    .flatten()
                    .as_ref()
                    == Some(hash)
            })
        {
            self.open_prepared(&image.state_dir)?;
            state_dir.sync_all()?;
            return Ok(());
        }

        let image = self.kernel.image();
        checked_file(root, &image, &self.linux_sha256)?;
        let initrd = self
            .kernel
            .initrd()
            .or(generated.as_ref().map(|_| self.kernel.initrd_path()));
        let initrd_sha256 = self.initrd_sha256.clone().or(generated);
        if let Some(path) = &initrd {
            checked_file(
                root,
                path,
                initrd_sha256.as_deref().context("missing initrd digest")?,
            )?;
        }

        // kernel-install add has no alternate-root mode. Give its trusted
        // BLS plugin only a private boot tree and an explicit private config.
        let scratch = tempfile::Builder::new()
            .prefix(".kernel-install-")
            .tempdir_in(state_path)?;
        let boot = scratch.path().join("boot");
        let config = scratch.path().join("conf");
        fs::create_dir(&boot)?;
        fs::create_dir(&config)?;
        fs::write(
            config.join("install.conf"),
            b"layout=bls\ninitrd_generator=none\nuki_generator=none\n",
        )?;
        fs::write(config.join("cmdline"), b"")?;
        ensure!(
            Path::new(LOADER_ENTRY_PLUGIN).is_file(),
            "missing systemd BLS plugin {LOADER_ENTRY_PLUGIN}"
        );
        let mut command = Command::new("kernel-install");
        command
            .env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("LC_ALL", "C")
            .env("TMPDIR", scratch.path())
            .env("BOOT_ROOT", &boot)
            .env("BOOT_MNT", &boot)
            .env("MACHINE_ID", settings.id.replace('-', ""))
            .env("KERNEL_INSTALL_CONF_ROOT", &config)
            .env("KERNEL_INSTALL_PLUGINS", LOADER_ENTRY_PLUGIN)
            .env("KERNEL_INSTALL_READ_MACHINE_INFO", "0")
            .stdin(Stdio::null())
            .args([
                "--entry-token=literal:cps",
                "--entry-type=type1",
                "--make-entry-directory=yes",
                "add",
            ])
            .arg(&self.kernel.release)
            .arg(path.join(image));
        if let Some(initrd) = initrd {
            command.arg(path.join(initrd));
        }
        let status = command.status().context("running kernel-install")?;
        ensure!(status.success(), "kernel-install failed with {status}");

        let prepared_path = boot.join("cps").join(&self.kernel.release);
        let prepared = Root::open(&prepared_path)?;
        checked_file(&prepared, "linux", &self.linux_sha256)?.sync_all()?;
        if let Some(hash) = &initrd_sha256 {
            checked_file(&prepared, "initrd", hash)?.sync_all()?;
        }
        if self.generated_initrd {
            let hash = initrd_sha256.as_deref().context("missing initrd digest")?;
            let mut record = File::create_new(prepared_path.join(GENERATED_INITRD_DIGEST))?;
            std::io::Write::write_all(&mut record, hash.as_bytes())?;
            record.sync_all()?;
        }
        let prepared_parent = File::open(boot.join("cps"))?;
        File::open(&prepared_path)?.sync_all()?;
        renameat_with(
            &prepared_parent,
            self.kernel.release.as_str(),
            state_dir,
            directory.as_str(),
            if exists {
                RenameFlags::EXCHANGE
            } else {
                RenameFlags::NOREPLACE
            },
        )
        .context("publishing prepared kernel artifacts")?;
        state_dir.sync_all()?;
        // The scratch tree now holds the replaced artifacts, if any.
        Ok(())
    }
}

fn checked_file(root: &Root, path: &str, expected: &str) -> Result<File> {
    let mut file = root.file(path).with_context(|| format!("opening {path}"))?;
    ensure!(
        digest_reader(&mut file)? == expected,
        "kernel artifact digest mismatch at {path}"
    );
    file.rewind()?;
    Ok(file)
}
