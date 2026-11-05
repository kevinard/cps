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
    image::{Installed, Managed, root::Root},
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

    pub fn initrd(&self) -> Option<String> {
        self.initrd
            .then(|| format!("usr/lib/modules/{}/initrd", self.release))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifacts {
    pub kernel: Kernel,
    pub(super) linux_sha256: String,
    pub(super) initrd_sha256: Option<String>,
}

pub(crate) fn selected(
    installed: &BTreeMap<String, Installed>,
    paths: &BTreeMap<String, Managed>,
) -> Result<Option<Artifacts>> {
    let mut selected = None;
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
        selected = Some(Artifacts {
            kernel: kernel.clone(),
            linux_sha256,
            initrd_sha256,
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

    pub fn open_prepared(&self, root: &Root) -> Result<(File, Option<File>)> {
        let dir = self.directory()?;
        let linux = checked_file(root, &format!(".cps/{dir}/linux"), &self.linux_sha256)
            .with_context(|| format!("opening prepared kernel {}", self.kernel.release))?;
        let initrd = self
            .initrd_sha256
            .as_ref()
            .map(|hash| checked_file(root, &format!(".cps/{dir}/initrd"), hash))
            .transpose()
            .with_context(|| format!("opening prepared initrd for {}", self.kernel.release))?;
        Ok((linux, initrd))
    }

    pub fn prepare(&self, root: &Root, path: &Path, image_id: &str) -> Result<()> {
        self.kernel.validate()?;
        let directory = self.directory()?;
        let state_path = path.join(".cps");
        let state_dir = root.dir(".cps")?;
        let final_path = state_path.join(&directory);
        match fs::symlink_metadata(&final_path) {
            Ok(_) => {
                self.open_prepared(root)?;
                state_dir.sync_all()?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let image = self.kernel.image();
        checked_file(root, &image, &self.linux_sha256)?;
        let initrd = self.kernel.initrd();
        if let Some(path) = &initrd {
            checked_file(
                root,
                path,
                self.initrd_sha256
                    .as_deref()
                    .context("missing initrd digest")?,
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
            .env("MACHINE_ID", image_id.replace('-', ""))
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
        if let Some(hash) = &self.initrd_sha256 {
            checked_file(&prepared, "initrd", hash)?.sync_all()?;
        }
        let prepared_parent = File::open(boot.join("cps"))?;
        File::open(&prepared_path)?.sync_all()?;
        renameat_with(
            &prepared_parent,
            self.kernel.release.as_str(),
            &state_dir,
            directory,
            RenameFlags::NOREPLACE,
        )
        .context("publishing prepared kernel artifacts")?;
        state_dir.sync_all()?;
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
