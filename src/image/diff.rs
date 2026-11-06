use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result};
use serde::Serialize;

use super::{Image, Managed};
use crate::{
    model::Version,
    query::{ProfileRole, intent_row, verified_intent},
};

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct ImagePackage {
    pub publisher: String,
    pub version: Version,
    pub manifest_sha256: String,
    pub requested: bool,
    pub profile_role: ProfileRole,
}

#[derive(Debug, Serialize)]
pub struct ImageMetadata {
    pub id: String,
    pub revision: u64,
    pub architecture: String,
    pub profile: Option<String>,
    pub facets: BTreeMap<String, bool>,
}

#[derive(Debug, Serialize)]
pub struct ImageChange<T> {
    pub name: String,
    pub before: Option<T>,
    pub after: Option<T>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ImagePath(Managed);

#[derive(Debug, Serialize)]
pub struct ImageDiff {
    pub before: ImageMetadata,
    pub after: ImageMetadata,
    pub packages: Vec<ImageChange<ImagePackage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<ImageChange<ImagePath>>>,
}

struct Snapshot {
    metadata: ImageMetadata,
    packages: BTreeMap<String, ImagePackage>,
    paths: BTreeMap<String, ImagePath>,
}

impl Snapshot {
    fn read(image: &Image) -> Result<Self> {
        let (state, intent) = verified_intent(image)?;
        let packages = state
            .installed
            .iter()
            .map(|(name, package)| {
                let row = intent_row(&state, &intent, package);
                let published = &package.published;
                (
                    name.clone(),
                    ImagePackage {
                        publisher: published.manifest.publisher.clone(),
                        version: published.manifest.version,
                        manifest_sha256: published.sha256.clone(),
                        requested: row.requested,
                        profile_role: row.profile,
                    },
                )
            })
            .collect();
        Ok(Self {
            metadata: ImageMetadata {
                id: state.settings.id,
                revision: state.settings.revision,
                architecture: state.settings.arch,
                profile: state.publishers.release_profile().map(str::to_owned),
                facets: state.settings.facets,
            },
            packages,
            paths: state
                .paths
                .into_iter()
                .map(|(path, managed)| (path, ImagePath(managed)))
                .collect(),
        })
    }

    fn diff(self, after: Self, paths: bool) -> ImageDiff {
        ImageDiff {
            before: self.metadata,
            after: after.metadata,
            packages: changes(self.packages, after.packages),
            paths: paths.then(|| changes(self.paths, after.paths)),
        }
    }
}

fn changes<T: PartialEq>(
    mut before: BTreeMap<String, T>,
    mut after: BTreeMap<String, T>,
) -> Vec<ImageChange<T>> {
    let names: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    names
        .into_iter()
        .filter_map(|name| {
            let before = before.remove(&name);
            let after = after.remove(&name);
            (before != after).then_some(ImageChange {
                name,
                before,
                after,
            })
        })
        .collect()
}

/// Compare the committed package state of two roots without reading payloads.
pub fn image_diff(before: &Path, after: &Path, paths: bool) -> Result<ImageDiff> {
    let read = |root: &Path| -> Result<Snapshot> { Snapshot::read(&Image::open_query(root)?) };
    let before = read(before).with_context(|| format!("reading {}", before.display()))?;
    let after = read(after).with_context(|| format!("reading {}", after.display()))?;
    Ok(before.diff(after, paths))
}

pub(crate) fn diff_open_images(before: &Image, after: &Image, paths: bool) -> Result<ImageDiff> {
    Ok(Snapshot::read(before)?.diff(Snapshot::read(after)?, paths))
}
