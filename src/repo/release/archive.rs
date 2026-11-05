use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::{
    image::Publisher,
    model::Node,
    repo::{canonical_repo, stage_blob},
};

use super::{
    ARCHIVE, Catalog, Mirror, Published, Root, cached_object, check_cache_origin,
    fetch_repository_payloads, lock_repo, open_state, path_present, read_catalog, read_pointer,
};

pub(crate) struct ArchiveCache {
    repo: PathBuf,
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
        Ok(Self { repo, catalog })
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
        let root = Root::open(&publisher.repo)?;
        let _lock = lock_repo(&publisher.repo)?;
        if path_present(&publisher.repo.join(".tuf"))? {
            let state = open_state(&publisher.repo)?;
            check_cache_origin(&state, &publisher.name, &publisher.key)?;
        }
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
