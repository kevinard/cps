use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};

use super::Plan;
use crate::{
    image::State,
    repo::{Published, snapshot},
};

#[derive(Default)]
pub(super) struct Catalogs {
    pub legacy: Option<String>,
    pub pins: BTreeMap<String, String>,
    pub packages: Vec<Published>,
}

pub(super) fn load(state: &State, saved: Option<&Plan>) -> Result<Catalogs> {
    let publishers = state.publishers.as_slice();
    let single = publishers.len() == 1;
    if let Some(saved) = saved {
        ensure!(
            if single {
                saved.catalog.is_some() && saved.catalogs.is_empty()
            } else {
                saved.catalog.is_none() && saved.catalogs.len() == publishers.len()
            },
            "saved plan's catalogs do not match the image's publishers"
        );
    }

    // Installed names keep their source. For other names only the first
    // publisher that offers the package contributes candidates to the solver.
    let mut sources: BTreeMap<_, _> = state
        .installed
        .iter()
        .map(|(name, pkg)| (name.clone(), pkg.published.manifest.publisher.as_str()))
        .collect();
    let mut catalogs = Catalogs::default();
    for publisher in publishers {
        let pin = match saved {
            None => None,
            Some(plan) if single => plan.catalog.as_deref(),
            Some(plan) => Some(
                plan.catalogs
                    .get(&publisher.name)
                    .with_context(|| format!("saved plan has no catalog for {}", publisher.name))?
                    .as_str(),
            ),
        };
        let snapshot = snapshot(&publisher.repo, &publisher.name, &publisher.key, pin)
            .with_context(|| format!("reading signed catalog for {}", publisher.name))?;
        if single {
            catalogs.legacy = Some(snapshot.sha256);
        } else {
            catalogs
                .pins
                .insert(publisher.name.clone(), snapshot.sha256);
        }
        catalogs
            .packages
            .extend(snapshot.packages.into_iter().filter(|pkg| {
                *sources
                    .entry(pkg.manifest.name.clone())
                    .or_insert(&publisher.name)
                    == publisher.name
            }));
    }
    Ok(catalogs)
}
