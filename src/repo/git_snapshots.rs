use std::{
    fs::{self, File},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::{
    image::root::Root,
    model::{Manifest, PackageSpec, Version, digest, validate_digest},
};

use super::{
    Catalog, lock_file, lock_repo, publication_base, read_catalog, read_limited, read_package,
    write_atomic,
};

const SERIES: &str = "0.0.0-";
const RESERVATIONS: &str = "git-snapshots";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    revision: u64,
    fingerprint: String,
}

fn series_version(revision: u64) -> Result<Version> {
    format!("{SERIES}{revision}").parse()
}

fn package_prefix(manifest: &Manifest) -> String {
    format!("cps://{}/{}@", manifest.publisher, manifest.name)
}

pub(crate) fn lock_snapshot_series(repo: &Path, manifest: &Manifest) -> Result<File> {
    fs::create_dir_all(repo)?;
    publication_base(repo)?;
    let directory = Root::open(repo)?.ensure_dir(RESERVATIONS)?;
    let name = digest(package_prefix(manifest).as_bytes());
    lock_file(&Root::fd_path(directory.as_file()).join(format!("{name}.lock")))
}

pub(crate) fn assign_git_version(
    repo: &Path,
    key: &[u8],
    manifest: &Manifest,
    tag_version: Option<Version>,
    fingerprint: &str,
) -> Result<Version> {
    let _lock = lock_repo(repo)?;
    let catalog = match publication_base(repo)? {
        Some(current) => read_catalog(repo, &current, &manifest.publisher, key)?,
        None => Catalog::empty(&manifest.publisher),
    };
    ensure!(
        !catalog.renames.contains_key(&manifest.name),
        "cannot reserve a snapshot for renamed package {}",
        manifest.name
    );
    let prefix = package_prefix(manifest);
    let cutoff = catalog.obsoletions.get(&manifest.name);
    let tagged_revision = if let Some(version) = tag_version {
        let id = format!("{prefix}{version}");
        if let Some(cutoff) = cutoff {
            ensure!(
                version > *cutoff,
                "Git release {id} is obsolete through {cutoff}"
            );
        }
        ensure!(
            !catalog.withdrawals.contains_key(&id),
            "Git release {id} was withdrawn"
        );
        if let Some(entry) = catalog.entries.get(&id) {
            let published = read_package(repo, &manifest.publisher, &id, entry)?;
            ensure!(
                published.manifest.upstream_version == manifest.upstream_version,
                "Git release {id} is already published from a different source"
            );
            return Ok(version);
        }

        // A numbered tag can overlap a version allocated to an unpublished snapshot.
        let text = version.to_string();
        let revision = text
            .strip_prefix(SERIES)
            .filter(|suffix| !suffix.starts_with('0'))
            .and_then(|suffix| suffix.parse::<u64>().ok());
        let Some(revision) = revision else {
            return Ok(version);
        };
        Some((version, revision))
    } else {
        None
    };

    let directory = Root::open(repo)?.ensure_dir(RESERVATIONS)?;
    let name = digest(prefix.as_bytes());
    let last = match directory.file(&name) {
        Ok(file) => {
            let last: Reservation = serde_json::from_slice(&read_limited(file, 1024)?)?;
            validate_digest(&last.fingerprint)?;
            Some(last)
        }
        Err(err) if err.downcast_ref::<Errno>() == Some(&Errno::NOENT) => None,
        Err(err) => return Err(err),
    };

    if let Some((version, revision)) = tagged_revision {
        if let Some(last) = &last {
            if last.revision == revision && last.fingerprint == fingerprint {
                return Ok(version);
            }
            ensure!(
                revision > last.revision,
                "Git release {prefix}{version} overlaps a reserved snapshot version"
            );
        }
        save_reservation(&directory, &name, revision, fingerprint)?;
        return Ok(version);
    }

    let series_prefix = format!("{prefix}{SERIES}");
    let published = catalog
        .entries
        .range(series_prefix.clone()..)
        .map(|(id, _)| id)
        .take_while(|id| id.starts_with(&series_prefix));
    let withdrawn = catalog
        .withdrawals
        .range(series_prefix.clone()..)
        .map(|(id, _)| id)
        .take_while(|id| id.starts_with(&series_prefix));
    let mut newest = cutoff
        .filter(|version| version.to_string().starts_with(SERIES))
        .cloned();
    for id in published.chain(withdrawn) {
        let version = PackageSpec::parse(id)?.version;
        newest = newest.max(version);
    }
    let last_published = newest
        .as_ref()
        .and_then(|version| {
            version
                .to_string()
                .strip_prefix(SERIES)?
                .split('.')
                .next()?
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(0);

    if let Some(last) = &last
        && last.fingerprint == fingerprint
    {
        let version = series_version(last.revision)?;
        let id = format!("{prefix}{version}");
        if newest.as_ref().is_none_or(|newest| &version >= newest)
            && cutoff.is_none_or(|cutoff| &version > cutoff)
            && !catalog.withdrawals.contains_key(&id)
        {
            let existing = catalog
                .entries
                .get(&id)
                .map(|entry| read_package(repo, &manifest.publisher, &id, entry))
                .transpose()?;
            if existing.is_none_or(|package| {
                package.manifest.upstream_version == manifest.upstream_version
            }) {
                return Ok(version);
            }
        }
    }

    let revision = last_published
        .max(last.map_or(0, |last| last.revision))
        .checked_add(1)
        .context("Git snapshot revision limit reached")?;
    let version = series_version(revision)?;
    if let Some(bound) = newest.as_ref().into_iter().chain(cutoff).max() {
        ensure!(
            &version > bound,
            "snapshot series {SERIES}N cannot advance package {} beyond version {bound}",
            manifest.name
        );
    }
    save_reservation(&directory, &name, revision, fingerprint)?;
    Ok(version)
}

fn save_reservation(directory: &Root, name: &str, revision: u64, fingerprint: &str) -> Result<()> {
    let reservation = Reservation {
        revision,
        fingerprint: fingerprint.to_owned(),
    };
    write_atomic(
        &Root::fd_path(directory.as_file()).join(name),
        &serde_json::to_vec(&reservation)?,
        false,
    )
}
