use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Publisher, Publishers, Settings, State, catalogs};
use crate::{
    attestation::ATTESTATION_LIMIT,
    model::validate_digest,
    repo::{BuildReceipt, CATALOG_LIMIT, Catalog, catalog_bytes, decode_archived_catalog},
};

const RECEIPT_LIMIT: u64 = 128 * 1024;

pub(super) fn exists(db: &Connection) -> Result<bool> {
    table_exists(db, "build_receipts")
}

fn table_exists(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}

pub(super) fn admission_catalog(
    db: &Connection,
    publisher: &Publisher,
) -> Result<Option<(Catalog, String)>> {
    if !table_exists(db, "build_admission_catalogs")? {
        return Ok(None);
    }
    let archived = db
        .query_row(
            "SELECT hash, CASE WHEN length(CAST(catalog AS BLOB)) <= ?2 THEN catalog END
         FROM build_admission_catalogs WHERE publisher=?1",
            (&publisher.name, CATALOG_LIMIT),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
        )
        .optional()?;
    archived
        .map(|(hash, bytes)| -> Result<_> {
            validate_digest(&hash)?;
            let bytes = bytes.context("archived build admission catalog exceeds size limit")?;
            let catalog = decode_archived_catalog(&bytes, &hash, &publisher.name, &publisher.key)?;
            Ok((catalog, hash))
        })
        .transpose()
        .with_context(|| format!("reading archived build admission for {}", publisher.name))
}

pub(super) fn save_audit(
    tx: &Transaction<'_>,
    publishers: &Publishers,
    settings: &Settings,
    receipts: &[BuildReceipt],
) -> Result<()> {
    let Some(first) = receipts.first() else {
        return Ok(());
    };
    let publisher = publishers
        .get(&first.publisher)
        .context("missing build receipt publisher")?;
    let hash = first.catalog();
    ensure!(
        receipts
            .iter()
            .all(|receipt| receipt.publisher == first.publisher && receipt.catalog() == hash),
        "build audit receipts must share a signed catalog"
    );
    let point = settings.catalog_checkpoints.get(&publisher.name);
    if point.is_none_or(|point| point.catalog != hash) {
        let bytes = catalog_bytes(&publisher.repo, hash)?;
        let catalog = decode_archived_catalog(&bytes, hash, &publisher.name, &publisher.key)?;
        if let Some((previous, previous_hash)) = admission_catalog(tx, publisher)?
            && previous_hash != hash
        {
            previous
                .check_successor(&catalog, &previous_hash)
                .context("checking build admission catalog succession")?;
        }
        if let Some(point) = point {
            catalogs::read_checkpoint(tx, publisher, point)?
                .check_successor(&catalog, &point.catalog)
                .context("checking build admission against the applied catalog")?;
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS build_admission_catalogs (
                publisher TEXT PRIMARY KEY, hash TEXT NOT NULL CHECK(length(hash)=64), catalog BLOB NOT NULL
             )",
        )?;
        tx.execute(
            "INSERT INTO build_admission_catalogs VALUES (?1, ?2, ?3)
             ON CONFLICT(publisher) DO UPDATE SET hash=excluded.hash, catalog=excluded.catalog",
            (&publisher.name, hash, bytes),
        )?;
    }
    save(tx, publishers, receipts)
}

pub(super) fn retain_admission(tx: &Transaction<'_>, publishers: &Publishers) -> Result<()> {
    if !table_exists(tx, "build_admission_catalogs")? {
        return Ok(());
    }
    let mut select = tx.prepare("SELECT publisher FROM build_admission_catalogs")?;
    let archived = select
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut delete = tx.prepare("DELETE FROM build_admission_catalogs WHERE publisher=?1")?;
    for name in archived {
        if publishers.get(&name).is_none() {
            delete.execute([name])?;
        }
    }
    Ok(())
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
