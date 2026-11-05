use std::{
    collections::BTreeSet,
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, unlinkat};
use serde::{Deserialize, Serialize};

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
const PROFILE_PINS: &str = "profile-pins";
const PROFILE_PINS_LIMIT: u64 = 16 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfilePins {
    primary_cache: PathBuf,
    previous: Option<ProfileSelection>,
    next: ProfileSelection,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileSelection {
    primary_catalog: String,
    companion_catalogs: BTreeSet<String>,
}

impl ProfilePins {
    fn selections(&self) -> impl Iterator<Item = &ProfileSelection> {
        self.previous.iter().chain(std::iter::once(&self.next))
    }

    fn read(directory: &Root, name: &str) -> Result<Self> {
        let bytes = read_limited(directory.file(name)?, PROFILE_PINS_LIMIT)?;
        let pins: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            pins.primary_cache.is_absolute(),
            "invalid primary cache path"
        );
        for selection in pins.selections() {
            validate_digest(&selection.primary_catalog)?;
            ensure!(
                (1..=2).contains(&selection.companion_catalogs.len()),
                "invalid number of companion profile pins"
            );
            for catalog in &selection.companion_catalogs {
                validate_digest(catalog)?;
            }
        }
        Ok(pins)
    }
}

/// Record both visible and staged selections before any profile pointer changes.
pub(super) fn record_profile_pins<'a>(
    repo: &Path,
    binding: &str,
    primary_cache: &Path,
    previous: Option<(&str, Option<&str>)>,
    next: &str,
    next_pins: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let state = repo.join(".tuf");
    let path = state.join(PROFILE_PINS);
    if !path_present(&path)? {
        fs::create_dir(&path)?;
        File::open(&state)?.sync_all()?;
    }
    let directory = Root::open(&state)?.subdir(PROFILE_PINS)?;
    let binding = binding.trim_end();
    validate_digest(binding)?;
    let existing = path_present(&path.join(binding))?
        .then(|| ProfilePins::read(&directory, binding))
        .transpose()?;
    if let Some(existing) = &existing {
        ensure!(
            existing.primary_cache == primary_cache,
            "profile retention names a different primary cache"
        );
    }
    let previous = if let Some((primary_catalog, companion)) = previous {
        let saved = existing.as_ref().and_then(|pins| {
            pins.selections()
                .filter(|selection| selection.primary_catalog == primary_catalog)
                .last()
        });
        let companion_catalogs = if let Some(saved) = saved {
            ensure!(
                companion.is_none_or(|pin| saved.companion_catalogs.contains(pin)),
                "profile retention differs from its visible catalog"
            );
            Some(saved.companion_catalogs.clone())
        } else {
            companion.map(|pin| BTreeSet::from([pin.to_owned()]))
        };
        companion_catalogs.map(|companion_catalogs| ProfileSelection {
            primary_catalog: primary_catalog.to_owned(),
            companion_catalogs,
        })
    } else {
        None
    };
    let pins = ProfilePins {
        primary_cache: primary_cache.to_owned(),
        previous,
        next: ProfileSelection {
            primary_catalog: next.to_owned(),
            companion_catalogs: next_pins.map(str::to_owned).collect(),
        },
    };
    let bytes = serde_json::to_vec(&pins)?;
    ensure!(
        bytes.len() as u64 <= PROFILE_PINS_LIMIT,
        "profile retention exceeds the size limit"
    );
    write_atomic(&path.join(binding), &bytes, false)
}

fn keep_profile_pins(state: &Root, catalogs: &mut BTreeSet<String>) -> Result<()> {
    if !path_present(&Root::fd_path(state.as_file()).join(PROFILE_PINS))? {
        return Ok(());
    }
    let directory = state.subdir(PROFILE_PINS)?;
    for entry in fs::read_dir(Root::fd_path(directory.as_file()))? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if validate_digest(&name).is_err() {
            continue;
        }
        let pins = ProfilePins::read(&directory, &name)
            .with_context(|| format!("reading profile retention {name}"))?;
        // Profile sync holds this companion's lock until the primary pointer
        // changes. An inaccessible primary conservatively keeps both sets.
        let visible = read_current(&pins.primary_cache).ok().filter(|hash| {
            pins.selections()
                .any(|selection| &selection.primary_catalog == hash)
        });
        for selection in pins.selections() {
            if visible
                .as_ref()
                .is_none_or(|hash| &selection.primary_catalog == hash)
            {
                catalogs.extend(selection.companion_catalogs.iter().cloned());
            }
        }
    }
    Ok(())
}

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
    keep_profile_pins(&state, &mut catalogs)?;
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
        let pins = image.release_pins(&state.publishers, &state.settings)?;
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
        for (id, entry) in catalog.entries {
            let package = read_package(&repo, publisher, &id, &entry)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::digest;

    #[test]
    fn shared_companion_keeps_each_primary_selection() -> Result<()> {
        let work = tempfile::tempdir()?;
        let companion = work.path().join("companion");
        let state = companion.join(".tuf");
        fs::create_dir_all(&state)?;
        let primary = work.path().join("primary");
        let other = work.path().join("other");
        fs::create_dir(&primary)?;
        fs::create_dir(&other)?;
        let old_primary = digest(b"old primary");
        let new_primary = digest(b"new primary");
        let other_primary = digest(b"other primary");
        let old = digest(b"old pin");
        let new = digest(b"new pin");
        let bootstrap = digest(b"bootstrap pin");
        fs::write(primary.join("current"), format!("{old_primary}\n"))?;
        fs::write(other.join("current"), format!("{other_primary}\n"))?;
        record_profile_pins(
            &companion,
            &digest(b"primary binding"),
            &primary,
            Some((&old_primary, Some(&old))),
            &new_primary,
            std::iter::once(new.as_str()),
        )?;
        record_profile_pins(
            &companion,
            &digest(b"other binding"),
            &other,
            None,
            &other_primary,
            std::iter::once(bootstrap.as_str()),
        )?;
        let retained = || -> Result<BTreeSet<String>> {
            let mut catalogs = BTreeSet::new();
            keep_profile_pins(&Root::open(&state)?, &mut catalogs)?;
            Ok(catalogs)
        };
        assert_eq!(
            retained()?,
            BTreeSet::from([old.clone(), bootstrap.clone()])
        );
        fs::write(primary.join("current"), format!("{new_primary}\n"))?;
        assert_eq!(
            retained()?,
            BTreeSet::from([new.clone(), bootstrap.clone()])
        );
        fs::rename(&primary, primary.with_extension("offline"))?;
        assert_eq!(retained()?, BTreeSet::from([old, new, bootstrap]));
        Ok(())
    }
}
