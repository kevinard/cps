use std::{fs, io, num::NonZeroU64, path::Path};

use anyhow::{Context, Result, ensure};
use rustix::fs::{RenameFlags, renameat_with};
use tempfile::TempDir;
use tough::{
    editor::RepositoryEditor,
    key_source::{KeySource, LocalKeySource},
    schema::{PathHashPrefix, PathSet, Root, Signed},
};

use super::{
    ARCHIVE, PUBLISHER_KEY_TARGET, PUBLISHER_NAME_TARGET, RETAINED, ROLE_LIMIT, ROOT_LIMIT,
};
use crate::{model::digest, repo::read_limited};

const PREFIXES: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// Sign a flat CPS export using stable TUF roles for content-addressed targets.
pub struct SignExport<'a> {
    pub root: &'a Path,
    pub export: &'a Path,
    pub output: &'a Path,
    pub targets_key: &'a Path,
    pub snapshot_key: &'a Path,
    pub timestamp_key: &'a Path,
    pub version: NonZeroU64,
    pub release_expires: &'a str,
    pub timestamp_expires: &'a str,
}

pub fn sign_export(request: SignExport<'_>) -> Result<()> {
    let parent = request
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let output_name = request
        .output
        .file_name()
        .context("missing mirror output name")?;
    let root: Signed<Root> =
        serde_json::from_slice(&read_limited(fs::File::open(request.root)?, ROOT_LIMIT)?)?;
    ensure!(
        root.signed.consistent_snapshot,
        "use a consistent-snapshot TUF root"
    );

    let stage = TempDir::new_in(parent)?;
    let targets = stage.path().join("targets");
    let metadata = stage.path().join("metadata");
    fs::create_dir(&targets)?;
    let keys: Vec<Box<dyn KeySource>> = [
        request.targets_key,
        request.snapshot_key,
        request.timestamp_key,
    ]
    .into_iter()
    .map(|path| Box::new(LocalKeySource { path: path.into() }) as Box<dyn KeySource>)
    .collect();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut editor = RepositoryEditor::new(request.root).await?;
            let expires = request
                .release_expires
                .parse()
                .context("invalid release expiry")?;
            let version = request.version;
            editor
                .targets_version(version)?
                .targets_expires(expires)?
                .snapshot_version(version)
                .snapshot_expires(expires)
                .timestamp_version(version)
                .timestamp_expires(
                    request
                        .timestamp_expires
                        .parse()
                        .context("invalid timestamp expiry")?,
                );
            let mut shards: [_; PREFIXES.len()] = std::array::from_fn(|_| Vec::new());
            for entry in fs::read_dir(request.export)? {
                let entry = entry?;
                ensure!(
                    entry.file_type()?.is_file(),
                    "export contains a non-file entry"
                );
                let (name, target) = RepositoryEditor::build_target(entry.path()).await?;
                ensure!(
                    name.resolved()
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte)),
                    "invalid CPS mirror target name {:?}",
                    name.resolved()
                );
                let target_path = targets.join(format!(
                    "{}.{}",
                    hex::encode(target.hashes.sha256.as_ref()),
                    name.resolved()
                ));
                if let Err(error) = fs::hard_link(entry.path(), &target_path) {
                    if error.kind() != io::ErrorKind::CrossesDevices {
                        return Err(error.into());
                    }
                    fs::copy(entry.path(), &target_path)?;
                }
                if matches!(
                    name.resolved(),
                    "current" | ARCHIVE | RETAINED | PUBLISHER_NAME_TARGET | PUBLISHER_KEY_TARGET
                ) {
                    editor.add_target(name, target)?;
                } else {
                    let hash = digest(name.resolved().as_bytes());
                    let prefix = usize::from(u8::from_str_radix(&hash[..1], 16)?);
                    shards[prefix].push((name, target));
                }
            }
            // Keep every role, including empty roles. A newer TUF snapshot
            // cannot drop roles from the previous trusted snapshot.
            for prefix in PREFIXES {
                editor
                    .delegate_role(
                        &format!("objects-{prefix}"),
                        &keys[..1],
                        PathSet::PathHashPrefixes(vec![PathHashPrefix::new(prefix.to_string())?]),
                        true,
                        NonZeroU64::MIN,
                        expires,
                        version,
                    )
                    .await?;
            }
            editor.sign_targets_editor(&keys).await?;
            for (prefix, targets) in PREFIXES.into_iter().zip(shards) {
                if targets.is_empty() {
                    continue;
                }
                editor.change_delegated_targets(&format!("objects-{prefix}"))?;
                editor.targets_version(version)?.targets_expires(expires)?;
                for (name, target) in targets {
                    editor.add_target(name, target)?;
                }
                editor.sign_targets_editor(&keys[..1]).await?;
            }
            editor.sign(&keys).await?.write(&metadata).await?;
            anyhow::Ok(())
        })?;

    for entry in fs::read_dir(metadata)? {
        let entry = entry?;
        ensure!(
            entry.metadata()?.len() <= ROLE_LIMIT,
            "TUF metadata exceeds the client size limit at {}",
            entry.path().display()
        );
    }
    let directory = fs::File::open(parent)?;
    renameat_with(
        &directory,
        stage
            .path()
            .file_name()
            .context("missing mirror staging name")?,
        &directory,
        output_name,
        RenameFlags::NOREPLACE,
    )?;
    directory.sync_all()?;
    Ok(())
}
