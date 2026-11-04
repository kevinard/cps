use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction};

use super::{Image, Publisher, Publishers, Settings, State, builds};
use crate::repo::{CATALOG_LIMIT, CatalogCheckpoint, catalog_bytes, check_catalog_successor};

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

pub(super) fn read(db: &Connection, publisher: &str) -> Result<Vec<u8>> {
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
            legacy_profile
                .map(|_| builds::read_profile_catalog(&self.db))
                .transpose()?
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
