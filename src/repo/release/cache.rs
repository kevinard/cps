use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, unlinkat};
use serde::{Deserialize, Serialize};

use crate::{
    ImageLock,
    image::{Image, Publisher},
    model::Version,
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
const PROFILE_PINS_FORMAT: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfilePins {
    format: u32,
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
            pins.format == PROFILE_PINS_FORMAT,
            "unsupported profile retention format {}",
            pins.format
        );
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
        format: PROFILE_PINS_FORMAT,
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

#[derive(Default)]
pub struct CacheRetention<'a> {
    pub catalogs: &'a [String],
    pub images: &'a [PathBuf],
    pub plans: &'a [Plan],
}

#[derive(Debug)]
pub struct PruneReport {
    pub blobs: Vec<(String, u64)>,
    pub bytes: u64,
    pub registered_roots: Vec<RetainedRoot>,
}

#[derive(Debug)]
pub struct RetainedRoot {
    pub id: String,
    pub package_payloads: usize,
    pub catalog: Option<String>,
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
    prune_payloads_with_locks(repo, publisher, trust_key, keep, &[], apply)
}

/// Also keep payloads from the exact selected packages in saved image locks.
pub fn prune_payloads_with_locks(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    keep: CacheRetention<'_>,
    locks: &[ImageLock],
    apply: bool,
) -> Result<PruneReport> {
    prune_with_registered_roots(repo, publisher, trust_key, keep, &[], locks, apply)
}

pub(crate) fn prune_with_registered_roots(
    repo: &Path,
    publisher: &str,
    trust_key: &Path,
    keep: CacheRetention<'_>,
    registered: &[(String, Image)],
    locks: &[ImageLock],
    apply: bool,
) -> Result<PruneReport> {
    prune(
        repo,
        publisher,
        &read_key(trust_key)?,
        registered,
        PrunePolicy {
            keep,
            locks,
            apply,
            versions: None,
        },
    )
}

pub(crate) fn prune_system_payloads(
    publisher: &Publisher,
    registered: &[(String, Image)],
    versions: usize,
) -> Result<PruneReport> {
    ensure!(
        !registered.is_empty(),
        "automatic pruning requires registered boot roots"
    );
    prune(
        &publisher.repo,
        &publisher.name,
        &publisher.key,
        registered,
        PrunePolicy {
            keep: CacheRetention::default(),
            locks: &[],
            apply: true,
            versions: Some(versions),
        },
    )
}

struct PrunePolicy<'a> {
    keep: CacheRetention<'a>,
    locks: &'a [ImageLock],
    apply: bool,
    versions: Option<usize>,
}

fn prune(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    registered: &[(String, Image)],
    policy: PrunePolicy<'_>,
) -> Result<PruneReport> {
    let PrunePolicy {
        keep,
        locks,
        apply,
        versions,
    } = policy;
    validate_publisher(publisher)?;
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
    check_cache_origin(&state, publisher, key)?;
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
    if versions.is_none() {
        keep_profile_pins(&state, &mut catalogs)?;
    }
    for hash in keep.catalogs {
        validate_digest(hash)?;
        catalogs.insert(hash.clone());
    }
    let mut live = BTreeSet::new();
    let mut registered_roots = Vec::new();
    let mut architectures = BTreeSet::new();
    let explicit = images
        .iter()
        .zip(keep.images)
        .map(|(image, path)| (image, path.display().to_string(), false));
    let automatic = registered
        .iter()
        .map(|(id, image)| (image, id.clone(), true));
    for (image, label, automatic) in explicit.chain(automatic) {
        let state = image.state()?;
        if versions.is_some() {
            ensure!(
                !state
                    .publishers
                    .as_slice()
                    .iter()
                    .any(|configured| { configured.repo == repo && configured.name != publisher }),
                "kept image {label} uses this cache for a different publisher"
            );
        }
        let configured = state.publishers.get(publisher);
        if automatic && configured.is_none_or(|configured| configured.repo != repo) {
            continue;
        }
        ensure!(
            configured.is_some_and(|configured| configured.repo == repo && configured.key == key),
            "kept image {label} does not use this cache and key for {publisher}"
        );
        image
            .require_clean()
            .with_context(|| format!("checking kept image {label}"))?;
        image.verify_catalog_archives(&state)?;
        let pins = image.release_pins(&state.publishers, &state.settings)?;
        let pin = pins.get(publisher);
        if let Some(pin) = pin {
            pin.read(&repo, publisher, key)?;
            catalogs.insert(pin.sha256.clone());
        }
        let mut payloads = BTreeSet::new();
        for (name, installed) in &state.installed {
            installed.verify(name, &state.publishers)?;
            if installed.published.manifest.publisher == publisher {
                payloads.extend(
                    release_blobs([&installed.published])?
                        .into_keys()
                        .map(str::to_owned),
                );
            }
        }
        if automatic {
            architectures.insert(state.settings.arch);
            registered_roots.push(RetainedRoot {
                id: label,
                package_payloads: payloads.len(),
                catalog: pin.map(|pin| pin.sha256.clone()),
            });
        }
        live.extend(payloads);
    }
    ensure!(
        registered.is_empty() || !registered_roots.is_empty(),
        "no registered boot root uses this cache for {publisher}"
    );
    for plan in keep.plans {
        plan.id()?;
        for hash in payloads(&plan.writes).into_keys() {
            validate_digest(hash)?;
            live.insert(hash.to_owned());
        }
    }
    for lock in locks {
        let hash = lock.retained_catalog(publisher, key)?;
        let catalog = read_catalog(&repo, hash, publisher, key)
            .with_context(|| format!("reading locked catalog {hash}"))?;
        for (id, manifest) in lock.pinned_packages(publisher) {
            let entry = catalog
                .entries
                .get(&id)
                .with_context(|| format!("locked package {id} is absent from catalog {hash}"))?;
            ensure!(
                entry.manifest_digest() == manifest,
                "locked manifest differs for {id}"
            );
            let package = read_package(&repo, publisher, &id, entry)?;
            live.extend(release_blobs([&package])?.into_keys().map(str::to_owned));
        }
    }
    let directory = Root::open(&repo)?.dir("blobs")?;
    let mut cached = BTreeMap::new();
    for entry in fs::read_dir(Root::fd_path(&directory))? {
        let entry = entry?;
        let Ok(hash) = entry.file_name().into_string() else {
            continue;
        };
        if validate_digest(&hash).is_ok() && entry.file_type()?.is_file() {
            cached.insert(hash, entry.metadata()?.len());
        }
    }
    if let Some(versions) = versions {
        if versions > 0 && !cached.is_empty() {
            keep_cached_versions(
                &repo,
                publisher,
                key,
                &cached,
                versions,
                &architectures,
                &mut live,
            )?;
        }
    } else {
        for hash in catalogs {
            let catalog = read_catalog(&repo, &hash, publisher, key)
                .with_context(|| format!("reading kept catalog {hash}"))?;
            for (id, entry) in catalog.entries {
                let package = read_package(&repo, publisher, &id, &entry)?;
                live.extend(release_blobs([&package])?.into_keys().map(str::to_owned));
            }
        }
    }
    let mut report = PruneReport {
        blobs: Vec::new(),
        bytes: 0,
        registered_roots,
    };
    for (hash, length) in cached {
        if live.contains(&hash) {
            continue;
        }
        report.bytes = report
            .bytes
            .checked_add(length)
            .context("payload size total overflow")?;
        report.blobs.push((hash, length));
    }
    if apply {
        for (hash, _) in &report.blobs {
            unlinkat(&directory, hash.as_str(), AtFlags::empty())
                .with_context(|| format!("removing cached payload {hash}"))?;
        }
        directory.sync_all()?;
    }
    Ok(report)
}

fn keep_cached_versions(
    repo: &Path,
    publisher: &str,
    key: &[u8],
    cached: &BTreeMap<String, u64>,
    versions: usize,
    architectures: &BTreeSet<String>,
    live: &mut BTreeSet<String>,
) -> Result<()> {
    let root = Root::open(repo)?;
    let catalogs = root.dir("catalogs")?;
    let mut seen = BTreeSet::new();
    let mut packages = BTreeMap::<_, BTreeMap<Version, BTreeSet<String>>>::new();
    for catalog in fs::read_dir(Root::fd_path(&catalogs))? {
        let catalog = catalog?;
        let Ok(hash) = catalog.file_name().into_string() else {
            continue;
        };
        if validate_digest(&hash).is_err() || !catalog.file_type()?.is_file() {
            continue;
        }
        let catalog = read_catalog(repo, &hash, publisher, key)
            .with_context(|| format!("reading cached catalog {hash}"))?;
        for (id, entry) in catalog.entries {
            let targets = architectures
                .iter()
                .filter(|arch| entry.supports_architecture(arch));
            let digest = entry.manifest_digest();
            // A cached predecessor can supply lineage without its manifests.
            if targets.clone().next().is_none()
                || !seen.insert(digest.to_owned())
                || !path_present(&repo.join("manifests").join(digest))?
            {
                continue;
            }
            let package = read_package(repo, publisher, &id, &entry)?;
            let present: BTreeSet<_> = release_blobs([&package])?
                .into_iter()
                .filter(|(hash, length)| cached.get(*hash) == Some(length))
                .map(|(hash, _)| hash.to_owned())
                .collect();
            if present.is_empty() {
                continue;
            }
            let manifest = package.manifest;
            for arch in targets {
                let retained = packages.entry((manifest.name.clone(), arch)).or_default();
                retained
                    .entry(manifest.version.clone())
                    .or_default()
                    .extend(present.iter().cloned());
                if retained.len() > versions {
                    retained.pop_first();
                }
            }
        }
    }
    live.extend(
        packages
            .into_values()
            .flat_map(BTreeMap::into_values)
            .flatten(),
    );
    Ok(())
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
        let binding = digest(b"primary binding");
        fs::write(primary.join("current"), format!("{old_primary}\n"))?;
        fs::write(other.join("current"), format!("{other_primary}\n"))?;
        record_profile_pins(
            &companion,
            &binding,
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
        let path = state.join(PROFILE_PINS).join(&binding);
        let bytes = fs::read(&path)?;
        let mut invalid: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(invalid["format"], 1);
        invalid.as_object_mut().unwrap().remove("format");
        fs::write(&path, serde_json::to_vec(&invalid)?)?;
        assert!(retained().is_err());
        invalid["format"] = 2.into();
        fs::write(&path, serde_json::to_vec(&invalid)?)?;
        assert!(retained().is_err());
        fs::write(&path, bytes)?;
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
