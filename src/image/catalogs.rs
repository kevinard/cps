use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction};

use super::{Image, ProfileIntent, Publisher, Publishers, Settings, State};
use crate::repo::{
    CATALOG_LIMIT, Catalog, CatalogCheckpoint, ReleaseProfile, archived_catalog_hash,
    catalog_bytes, read_catalog, take_profile,
};

pub(super) fn save(tx: &Transaction<'_>, state: &State) -> Result<()> {
    let mut insert = tx.prepare(
        "INSERT INTO catalog_archives VALUES (?1, ?2)
         ON CONFLICT(publisher) DO UPDATE SET catalog=excluded.catalog
         WHERE catalog!=excluded.catalog",
    )?;
    for (name, checkpoint) in &state.settings.catalog_checkpoints {
        let publisher = state
            .publishers
            .get(name)
            .context("missing checkpoint publisher")?;
        // An archive names its catalog in its first record, so an unchanged
        // checkpoint costs neither the shards nor a rewrite.
        if read(tx, name).is_ok_and(|bytes| {
            archived_catalog_hash(&bytes).is_ok_and(|hash| hash == checkpoint.catalog)
        }) {
            continue;
        }
        let bytes = catalog_bytes(&publisher.repo, &checkpoint.catalog)
            .with_context(|| format!("archiving the signed catalog for {name}"))?;
        insert.execute((name, bytes))?;
    }
    Ok(())
}

fn read(db: &Connection, publisher: &str) -> Result<Vec<u8>> {
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT CASE WHEN length(CAST(catalog AS BLOB)) <= ?2 THEN catalog END
             FROM catalog_archives WHERE publisher=?1",
            (publisher, CATALOG_LIMIT),
            |row| row.get(0),
        )
        .with_context(|| format!("reading the archived catalog for {publisher}"))?;
    bytes.context("archived catalog exceeds size limit")
}

pub(super) fn read_checkpoint(
    db: &Connection,
    publisher: &Publisher,
    point: &CatalogCheckpoint,
) -> Result<Catalog> {
    let bytes = read(db, &publisher.name)?;
    point
        .read_archive(&bytes, &publisher.name, &publisher.key)
        .with_context(|| format!("verifying the archived catalog for {}", publisher.name))
}

fn read_primary<'a>(db: &Connection, state: &'a State) -> Result<Option<(Catalog, &'a str)>> {
    let primary = &state.publishers.as_slice()[0];
    let point = state.settings.catalog_checkpoints.get(&primary.name);
    point
        .map(|point| Ok((read_checkpoint(db, primary, point)?, point.catalog.as_str())))
        .transpose()
}

fn profile(catalog: &mut Catalog, hash: &str, name: &str, state: &State) -> Result<ReleaseProfile> {
    let profile = take_profile(catalog, hash, name)?;
    profile.check_image(
        name,
        state.settings.selection(),
        state.publishers.signed_publishers()[1..]
            .iter()
            .map(|publisher| publisher.name.as_str()),
    )?;
    Ok(profile)
}

impl super::Image {
    /// The signed release profile archived with the image's applied catalog, offline.
    pub(crate) fn archived_profile(&self, state: &State) -> Result<Option<ReleaseProfile>> {
        read_profile(&self.db, state)
    }
}

pub(super) fn read_profile(db: &Connection, state: &State) -> Result<Option<ReleaseProfile>> {
    let Some(name) = state.publishers.release_profile() else {
        return Ok(None);
    };
    read_primary(db, state)?
        .map(|(mut catalog, hash)| profile(&mut catalog, hash, name, state))
        .transpose()
}

pub(super) fn read_applied<'a>(
    db: &Connection,
    state: &'a State,
    publisher: &Publisher,
    signed: Option<&ReleaseProfile>,
) -> Result<Option<(Catalog, &'a str)>> {
    if state.publishers.release_profile().is_some()
        && state.publishers.as_slice()[0].name == publisher.name
    {
        return read_primary(db, state);
    }
    let point = state.settings.catalog_checkpoints.get(&publisher.name);
    let Some(point) = point else {
        return Ok(None);
    };
    let catalog = read_checkpoint(db, publisher, point)?;
    if let Some(pin) = signed.and_then(|profile| {
        profile
            .companions
            .iter()
            .find(|pin| pin.publisher == publisher.name)
    }) {
        pin.check_archive(&catalog, &point.catalog)?;
    }
    Ok(Some((catalog, &point.catalog)))
}

fn check_installed(catalog: &Catalog, publisher: &Publisher, state: &State) -> Result<()> {
    for package in state.installed.values().map(|package| &package.published) {
        if package.manifest.publisher == publisher.name {
            catalog.check_installed(package).with_context(|| {
                format!(
                    "checking archived catalog admission for {}",
                    package.manifest.id()
                )
            })?;
        }
    }
    Ok(())
}

pub(super) fn verify(db: &Connection, state: &State) -> Result<()> {
    let mut publishers = state.publishers.as_slice();
    let signed = if let Some(name) = state.publishers.release_profile()
        && let Some((mut catalog, hash)) = read_primary(db, state)?
    {
        check_installed(&catalog, &publishers[0], state)?;
        publishers = &publishers[1..];
        Some(profile(&mut catalog, hash, name, state)?)
    } else {
        None
    };
    for package in state.installed.values() {
        let publisher = &package.published.manifest.publisher;
        ensure!(
            state.settings.catalog_checkpoints.contains_key(publisher),
            "missing catalog checkpoint for installed publisher {publisher}"
        );
    }
    for publisher in publishers {
        let point = state.settings.catalog_checkpoints.get(&publisher.name);
        let pin = signed.as_ref().and_then(|profile| {
            profile
                .companions
                .iter()
                .find(|pin| pin.publisher == publisher.name)
        });
        let Some(point) = point else {
            ensure!(
                pin.is_none(),
                "missing checkpoint for signed companion {}",
                publisher.name
            );
            continue;
        };
        let catalog = read_checkpoint(db, publisher, point)?;
        if let Some(pin) = pin {
            pin.check_archive(&catalog, &point.catalog)?;
        }
        check_installed(&catalog, publisher, state)?;
    }
    Ok(())
}

pub(super) fn retain(tx: &Transaction<'_>, settings: &Settings) -> Result<()> {
    let mut select = tx.prepare("SELECT publisher FROM catalog_archives")?;
    let names = select
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut delete = tx.prepare("DELETE FROM catalog_archives WHERE publisher=?1")?;
    for name in names {
        if !settings.catalog_checkpoints.contains_key(&name) {
            delete.execute([name])?;
        }
    }
    Ok(())
}

impl Image {
    pub(crate) fn profile_intent(state: &State) -> ProfileIntent {
        state
            .settings
            .profile_intent
            .clone()
            .unwrap_or_else(|| ProfileIntent {
                roots: BTreeSet::new(),
                local: state
                    .installed
                    .iter()
                    .filter(|(_, package)| package.explicit)
                    .map(|(name, _)| name.clone())
                    .collect(),
            })
    }

    pub(crate) fn check_repository_candidate(
        &self,
        state: &State,
        publisher: &Publisher,
        next: &Catalog,
        checkpoint: &CatalogCheckpoint,
    ) -> Result<()> {
        let settings = &state.settings;
        settings.check_catalog(&publisher.name, checkpoint)?;
        if let Some(point) = settings.catalog_checkpoints.get(&publisher.name)
            && point.catalog != checkpoint.catalog
        {
            read_checkpoint(&self.db, publisher, point)?.check_successor(next, &point.catalog)?;
        }
        Ok(())
    }

    pub(crate) fn check_catalog(
        &self,
        publishers: &Publishers,
        settings: &Settings,
        publisher: &Publisher,
        next: &CatalogCheckpoint,
    ) -> Result<()> {
        // Profile selection already checks the primary, which determines all the pins.
        if publishers.release_profile().is_some() && publishers.as_slice()[0].name == publisher.name
        {
            return Ok(());
        }
        settings.check_catalog(&publisher.name, next)?;
        self.check_catalog_transition(settings, publisher, &next.catalog)
    }

    pub(super) fn check_catalog_transition(
        &self,
        settings: &Settings,
        publisher: &Publisher,
        next: &str,
    ) -> Result<()> {
        let Some(point) = settings
            .catalog_checkpoints
            .get(&publisher.name)
            .filter(|point| point.catalog != next)
        else {
            return Ok(());
        };
        let previous = read_checkpoint(&self.db, publisher, point)?;
        let next = read_catalog(&publisher.repo, next, &publisher.name, &publisher.key)?;
        previous
            .check_successor(&next, &point.catalog)
            .with_context(|| format!("checking catalog successor for {}", publisher.name))
    }
}
