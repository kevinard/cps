use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, unlinkat};

use crate::{
    image::Image,
    plan::{Plan, payloads},
    repo::read_package,
};

use super::{
    RETAINED_LIMIT, Root, StagedRelease, check_cache_origin, lock_repo, open_state, path_present,
    read_catalog, read_current, read_key, read_limited, release_blobs, retained_catalogs,
    validate_digest, validate_publisher, write_atomic,
};

const RETENTION: &str = "retention";

pub struct CacheRetention<'a> {
    pub catalogs: &'a [String],
    pub images: &'a [PathBuf],
    pub plans: &'a [Plan],
}

#[derive(Debug)]
pub struct PruneReport {
    pub blobs: Vec<(String, u64)>,
    pub bytes: u64,
}

pub(super) fn record_retention(state: &Path, release: &StagedRelease) -> Result<()> {
    let directory = state.join(RETENTION);
    if !path_present(&directory)? {
        fs::create_dir(&directory)?;
    }
    Root::open(state)?.dir(RETENTION)?;
    let mut retained = String::new();
    for hash in &release.retained {
        retained.push_str(hash);
        retained.push('\n');
    }
    // A new pointer can use its own inventory without replacing the old one.
    write_atomic(
        &directory.join(&release.current),
        retained.as_bytes(),
        false,
    )
}

/// Preview or remove payloads outside the current, retained and explicitly kept sets.
pub fn prune_payloads(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    keep: CacheRetention<'_>,
    apply: bool,
) -> Result<PruneReport> {
    validate_publisher(publisher)?;
    let key = read_key(trust_key)?;
    let repo = fs::canonicalize(repo).context("opening repository cache")?;
    ensure!(
        path_present(&repo.join(".tuf"))?,
        "payload pruning requires a TUF-managed repository cache"
    );
    // Fetch takes the image lock first. Hold shared image locks through pruning
    // so the installed packages used for offline repair cannot change meanwhile.
    let images = keep
        .images
        .iter()
        .map(|path| {
            Image::open_query(path)
                .with_context(|| format!("opening kept image {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let _lock = lock_repo(&repo)?;
    let state = open_state(&repo)?;
    check_cache_origin(&state, publisher, &key)?;
    let current = read_current(&repo)?;
    let retained = read_limited(
        state.file(&format!("{RETENTION}/{current}"))
            .context("cache has no retention inventory for its current catalog; run repo sync or repo sync-profile")?,
        RETAINED_LIMIT,
    )?;
    let retained = std::str::from_utf8(&retained)?;
    let mut catalogs = BTreeSet::from([current.clone()]);
    catalogs.extend(
        retained_catalogs(retained.lines(), &current)?
            .into_iter()
            .map(str::to_owned),
    );
    for hash in keep.catalogs {
        validate_digest(hash)?;
        catalogs.insert(hash.clone());
    }
    let mut live = BTreeSet::new();
    for (image, path) in images.iter().zip(keep.images) {
        image
            .require_clean()
            .with_context(|| format!("checking kept image {}", path.display()))?;
        let state = image.state()?;
        let configured = state.publishers.get(publisher);
        ensure!(
            configured.is_some_and(|configured| configured.repo == repo && configured.key == key),
            "kept image {} does not use this cache for {publisher}",
            path.display()
        );
        let pins = state.publishers.release_pins(
            &state.settings.arch,
            &state.settings.facets,
            state.settings.bootstrap_catalog.as_deref(),
        )?;
        if let Some(pin) = pins.get(publisher) {
            pin.read(&repo, publisher, &key)?;
            catalogs.insert(pin.sha256.clone());
        }
        for (name, installed) in &state.installed {
            installed.verify(name, &state.publishers)?;
            if installed.published.manifest.publisher == publisher {
                live.extend(
                    release_blobs([&installed.published])?
                        .into_keys()
                        .map(str::to_owned),
                );
            }
        }
    }
    for plan in keep.plans {
        plan.id()?;
        for hash in payloads(&plan.writes).into_keys() {
            validate_digest(hash)?;
            live.insert(hash.to_owned());
        }
    }
    for hash in catalogs {
        let catalog = read_catalog(&repo, &hash, publisher, &key)
            .with_context(|| format!("reading kept catalog {hash}"))?;
        for (id, manifest) in catalog.entries {
            let package = read_package(&repo, publisher, &id, manifest)?;
            live.extend(release_blobs([&package])?.into_keys().map(str::to_owned));
        }
    }
    let directory = Root::open(&repo)?.dir("blobs")?;
    let mut report = PruneReport {
        blobs: Vec::new(),
        bytes: 0,
    };
    for entry in fs::read_dir(Root::fd_path(&directory))? {
        let entry = entry?;
        let Ok(hash) = entry.file_name().into_string() else {
            continue;
        };
        if validate_digest(&hash).is_err() || live.contains(&hash) || !entry.file_type()?.is_file()
        {
            continue;
        }
        let length = entry.metadata()?.len();
        report.bytes = report
            .bytes
            .checked_add(length)
            .context("payload size total overflow")?;
        report.blobs.push((hash, length));
    }
    report.blobs.sort_unstable();
    if apply {
        for (hash, _) in &report.blobs {
            unlinkat(&directory, hash.as_str(), AtFlags::empty())
                .with_context(|| format!("removing cached payload {hash}"))?;
        }
        directory.sync_all()?;
    }
    Ok(report)
}
