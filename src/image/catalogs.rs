use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction};

use super::{Image, Publisher, Publishers, Settings, State};
use crate::repo::{
    CATALOG_LIMIT, Catalog, CatalogCheckpoint, ReleaseProfile, catalog_bytes,
    check_catalog_successor, decode_archived_catalog, take_profile,
};

pub(super) fn save(tx: &Transaction<'_>, state: &State) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS catalog_archives (
            publisher TEXT PRIMARY KEY, catalog BLOB NOT NULL
         )",
    )?;
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

fn read_legacy(db: &Connection) -> Result<Vec<u8>> {
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT CASE WHEN length(CAST(catalog AS BLOB)) <= ?1 THEN catalog END
             FROM build_profile WHERE id=1",
            [CATALOG_LIMIT],
            |row| row.get(0),
        )
        .context("reading the archived profile catalog")?;
    bytes.context("archived profile catalog exceeds size limit")
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
    if state.settings.catalog_archives {
        let point = point.context("missing archived primary catalog checkpoint")?;
        return Ok(Some((read_checkpoint(db, primary, point)?, &point.catalog)));
    }
    let Some(hash) = &state.settings.archived_profile_catalog else {
        return Ok(None);
    };
    let bytes = read_legacy(db)?;
    let catalog = if let Some(point) = point {
        ensure!(
            point.catalog == *hash,
            "archived primary catalog digest differs from its checkpoint"
        );
        point.read_archive(&bytes, &primary.name, &primary.key)?
    } else {
        decode_archived_catalog(&bytes, hash, &primary.name, &primary.key)?
    };
    Ok(Some((catalog, hash)))
}

fn profile(catalog: &mut Catalog, hash: &str, name: &str, state: &State) -> Result<ReleaseProfile> {
    let profile = take_profile(catalog, hash, name)?;
    profile.check_image(
        name,
        &state.settings.arch,
        &state.settings.facets,
        state.publishers.as_slice()[1..]
            .iter()
            .map(|publisher| publisher.name.as_str()),
    )?;
    Ok(profile)
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
    if !state.settings.catalog_archives {
        return Ok(None);
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
        pin.check_archive(&catalog, &point.catalog, &publisher.key)?;
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
    if !state.settings.catalog_archives {
        return Ok(());
    }
    for package in state.installed.values() {
        let publisher = &package.published.manifest.publisher;
        ensure!(
            state.settings.catalog_checkpoints.contains_key(publisher),
            "missing catalog checkpoint for installed publisher {publisher}"
        );
    }
    for (index, publisher) in publishers.iter().enumerate() {
        let point = state.settings.catalog_checkpoints.get(&publisher.name);
        let pin = signed.as_ref().map(|profile| &profile.companions[index]);
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
            pin.check_archive(&catalog, &point.catalog, &publisher.key)?;
        }
        check_installed(&catalog, publisher, state)?;
    }
    Ok(())
}

pub(super) fn retain(tx: &Transaction<'_>, settings: &Settings) -> Result<()> {
    if !settings.catalog_archives {
        return Ok(());
    }
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
    pub(crate) fn check_repository_candidate(
        &self,
        state: &State,
        publisher: &Publisher,
        next: &Catalog,
        checkpoint: &CatalogCheckpoint,
    ) -> Result<()> {
        let settings = &state.settings;
        settings.check_catalog(&publisher.name, checkpoint)?;
        let point = settings.catalog_checkpoints.get(&publisher.name);
        let legacy = settings
            .archived_profile_catalog
            .as_deref()
            .filter(|_| state.publishers.as_slice()[0].name == publisher.name);
        let Some(previous_hash) = legacy.or(point.map(|point| point.catalog.as_str())) else {
            return Ok(());
        };
        if previous_hash == checkpoint.catalog {
            return Ok(());
        }
        let previous = if let Some(point) = point.filter(|_| settings.catalog_archives) {
            read_checkpoint(&self.db, publisher, point)?
        } else if legacy.is_some() {
            let bytes = read_legacy(&self.db)?;
            decode_archived_catalog(&bytes, previous_hash, &publisher.name, &publisher.key)?
        } else {
            let bytes = catalog_bytes(&publisher.repo, previous_hash).with_context(|| {
                format!(
                    "replacement cache requires the last applied catalog {previous_hash} for {}",
                    publisher.name
                )
            })?;
            point
                .context("missing installed checkpoint")?
                .read_archive(&bytes, &publisher.name, &publisher.key)?
        };
        previous.check_successor(next, previous_hash)
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
        self.check_catalog_transition(settings, publisher, &next.catalog, None)
    }

    pub(super) fn check_catalog_transition(
        &self,
        settings: &Settings,
        publisher: &Publisher,
        next: &str,
        legacy_profile: Option<&str>,
    ) -> Result<()> {
        let checkpoint = settings.catalog_checkpoints.get(&publisher.name);
        let previous = legacy_profile.or(checkpoint.map(|point| point.catalog.as_str()));
        let Some(previous) = previous.filter(|hash| *hash != next) else {
            return Ok(());
        };
        let archived = if settings.catalog_archives {
            Some(read(&self.db, &publisher.name)?)
        } else {
            legacy_profile.map(|_| read_legacy(&self.db)).transpose()?
        };
        check_catalog_successor(
            &publisher.repo,
            &publisher.name,
            &publisher.key,
            previous,
            archived.as_deref(),
            next,
        )
        .with_context(|| format!("checking catalog successor for {}", publisher.name))
    }
}
