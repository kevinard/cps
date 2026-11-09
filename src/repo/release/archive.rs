use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::{
    image::Publisher,
    model::Node,
    repo::{canonical_repo, catalog_objects, read_object, stage_blob},
};

use super::{
    ARCHIVE, ATTESTATION_LIMIT, CATALOG_ROOT_LIMIT, Catalog, MANIFEST_LIMIT, Mirror, Published,
    Root, cached_object, check_cache_origin, collect_rebuild_objects, fetch_repository_payloads,
    lock_repo, open_state, path_present, prepare_objects, read_catalog, read_pointer, rebuild,
    write_atomic,
};

pub(crate) struct ArchiveCache {
    repo: PathBuf,
    pin: String,
    catalog: Catalog,
}

impl ArchiveCache {
    pub(crate) fn open(path: &Path, publisher: &Publisher) -> Result<Self> {
        let repo = canonical_repo(path)?;
        let directory = open_state(&repo)?;
        let pin = read_pointer(&repo.join(".tuf"), ARCHIVE)
            .context("archive cache requires repo sync --archive-catalog")?;
        check_cache_origin(&directory, &publisher.name, &publisher.key)?;
        Mirror::read(&directory)?;
        directory.dir("trusted")?;
        let catalog = read_catalog(&repo, &pin, &publisher.name, &publisher.key)?;
        Ok(Self { repo, pin, catalog })
    }

    pub(crate) fn pin(&self) -> &str {
        &self.pin
    }

    /// Restore the objects needed to replay a plan at this exact archive pin.
    pub(crate) fn restore_metadata(&self, publisher: &Publisher) -> Result<()> {
        if self.repo == publisher.repo {
            return Ok(());
        }
        let (_lock, _) = lock_destination(publisher)?;
        let destination = &publisher.repo;
        prepare_objects(destination)?;
        let manifests = self
            .catalog
            .entries
            .values()
            .map(|entry| entry.manifest_digest())
            .collect::<BTreeSet<_>>();
        for hash in manifests {
            self.copy_object(destination, "manifests", hash, MANIFEST_LIMIT)?;
        }
        for (kind, hash, limit) in catalog_objects(&self.repo, &self.pin)? {
            self.copy_object(destination, kind, &hash, limit)?;
        }
        let mut attestations = self.catalog.attestations.values().cloned().collect();
        let mut inputs = BTreeMap::new();
        collect_rebuild_objects(&self.catalog, &mut inputs, &mut attestations);
        for hash in attestations {
            self.copy_object(destination, "attestations", &hash, ATTESTATION_LIMIT)?;
        }
        for (hash, limit) in inputs {
            self.copy_object(destination, rebuild::OBJECTS, &hash, limit)?;
        }
        // Publish the exact catalog after its metadata objects are durable.
        self.copy_object(destination, "catalogs", &self.pin, CATALOG_ROOT_LIMIT)
    }

    fn copy_object(&self, destination: &Path, kind: &str, hash: &str, limit: u64) -> Result<()> {
        if read_object(destination, kind, hash, limit).is_err() {
            let bytes = read_object(&self.repo, kind, hash, limit)?;
            write_atomic(&destination.join(kind).join(hash), &bytes, false)?;
        }
        Ok(())
    }

    pub(crate) fn covers(&self, package: &Published) -> Result<bool> {
        if !self.catalog.entries.contains_key(&package.manifest.id()) {
            return Ok(false);
        }
        self.catalog.check_installed(package).with_context(|| {
            format!(
                "checking {} against archive {}",
                package.manifest.id(),
                self.repo.display()
            )
        })?;
        Ok(true)
    }

    pub(crate) fn restore(
        &self,
        publisher: &Publisher,
        blobs: BTreeMap<&str, &Node>,
    ) -> Result<()> {
        let mut missing = BTreeMap::new();
        for (hash, node) in blobs {
            let Node::File { size, .. } = node else {
                unreachable!("payloads contain only files");
            };
            if !cached_object(&publisher.repo, &format!("blobs/{hash}"), hash, *size)? {
                missing.insert(hash, (node, *size));
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        fetch_repository_payloads(
            &self.repo,
            &publisher.name,
            &publisher.key,
            missing
                .iter()
                .map(|(&hash, &(_, size))| (hash, size))
                .collect(),
        )
        .with_context(|| format!("fetching from archive {}", self.repo.display()))?;
        if self.repo == publisher.repo {
            return Ok(());
        }

        // The archive download releases its lock before we lock the destination.
        let (_lock, root) = lock_destination(publisher)?;
        let destination = root.dir("blobs")?;
        let directory = Root::fd_path(&destination);
        for (hash, (node, size)) in missing {
            if !cached_object(&publisher.repo, &format!("blobs/{hash}"), hash, size)? {
                let mut temp = NamedTempFile::new_in(&directory)?;
                stage_blob(&self.repo, node, temp.as_file_mut())?;
                temp.as_file().sync_all()?;
                temp.persist(directory.join(hash))?;
            }
        }
        destination.sync_all()?;
        Ok(())
    }
}

fn lock_destination(publisher: &Publisher) -> Result<(File, Root)> {
    let root = Root::open(&publisher.repo)?;
    let lock = lock_repo(&publisher.repo)?;
    if path_present(&publisher.repo.join(".tuf"))? {
        let state = open_state(&publisher.repo)?;
        check_cache_origin(&state, &publisher.name, &publisher.key)?;
    }
    Ok((lock, root))
}
