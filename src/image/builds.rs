use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Publishers, State};
use crate::{
    attestation::ATTESTATION_LIMIT,
    repo::{BuildReceipt, CATALOG_LIMIT, ReleaseProfile, archived_release_profile, catalog_bytes},
};

const RECEIPT_LIMIT: u64 = 128 * 1024;

pub(super) fn save_profile(tx: &Transaction<'_>, state: &State) -> Result<()> {
    let Some(hash) = &state.settings.archived_profile_catalog else {
        return Ok(());
    };
    let primary = &state.publishers.as_slice()[0];
    let bytes =
        catalog_bytes(&primary.repo, hash).context("archiving the signed profile catalog")?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS build_profile (
            id INTEGER PRIMARY KEY CHECK(id=1), catalog BLOB NOT NULL
         )",
    )?;
    tx.execute(
        "INSERT INTO build_profile VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET catalog=excluded.catalog WHERE catalog!=excluded.catalog",
        [bytes],
    )?;
    Ok(())
}

pub(super) fn read_profile_catalog(db: &Connection) -> Result<Vec<u8>> {
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

pub(super) fn read_profile(db: &Connection, state: &State) -> Result<Option<ReleaseProfile>> {
    let Some(hash) = &state.settings.archived_profile_catalog else {
        return Ok(None);
    };
    let name = state
        .publishers
        .release_profile()
        .context("missing image profile")?;
    let (primary, companions) = state
        .publishers
        .as_slice()
        .split_first()
        .context("missing primary publisher")?;
    let bytes = read_profile_catalog(db)?;
    let profile = archived_release_profile(&bytes, hash, &primary.name, &primary.key, name)?;
    profile.check_image(
        name,
        &state.settings.arch,
        &state.settings.facets,
        companions.iter().map(|publisher| publisher.name.as_str()),
    )?;
    Ok(Some(profile))
}

pub(super) fn exists(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='build_receipts')",
        [],
        |row| row.get(0),
    )?)
}

pub(super) fn save(
    tx: &Transaction<'_>,
    publishers: &Publishers,
    receipts: &[BuildReceipt],
) -> Result<()> {
    if receipts.is_empty() {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS build_receipts (
            manifest TEXT PRIMARY KEY,
            receipt TEXT NOT NULL,
            envelope BLOB NOT NULL
         )",
    )?;
    let mut insert = tx.prepare(
        "INSERT INTO build_receipts VALUES (?1, ?2, ?3)
         ON CONFLICT(manifest) DO UPDATE SET receipt=excluded.receipt, envelope=excluded.envelope",
    )?;
    for receipt in receipts {
        let publisher = publishers
            .get(&receipt.publisher)
            .context("missing build receipt publisher")?;
        let bytes = receipt.read_attestation(&publisher.repo)?;
        let document = serde_json::to_string(receipt)?;
        ensure!(
            document.len() as u64 <= RECEIPT_LIMIT,
            "build receipt exceeds size limit"
        );
        insert.execute(params![receipt.manifest, document, bytes])?;
    }
    Ok(())
}

pub(super) fn read(db: &Connection, manifest: &str) -> Result<Option<(BuildReceipt, Vec<u8>)>> {
    let archived = db
        .query_row(
            "SELECT CASE WHEN length(CAST(receipt AS BLOB)) <= ?2 THEN receipt END,
                CASE WHEN length(envelope) <= ?3 THEN envelope END
         FROM build_receipts WHERE manifest=?1",
            params![manifest, RECEIPT_LIMIT, ATTESTATION_LIMIT],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                ))
            },
        )
        .optional()?;
    archived
        .map(|(receipt, envelope)| {
            let receipt = receipt.context("archived build receipt exceeds size limit")?;
            let envelope = envelope.context("archived build attestation exceeds size limit")?;
            Ok((serde_json::from_str(&receipt)?, envelope))
        })
        .transpose()
}

pub(super) fn prune(tx: &Transaction<'_>, state: &State) -> Result<()> {
    if !exists(tx)? {
        return Ok(());
    }
    let installed: BTreeSet<_> = state
        .installed
        .values()
        .map(|package| package.published.sha256.as_str())
        .collect();
    let archived = tx
        .prepare("SELECT manifest FROM build_receipts")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut delete = tx.prepare("DELETE FROM build_receipts WHERE manifest=?1")?;
    for manifest in archived {
        if !installed.contains(manifest.as_str()) {
            delete.execute([manifest])?;
        }
    }
    Ok(())
}
