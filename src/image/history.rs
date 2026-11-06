use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};

use super::{Image, Settings};
use crate::{
    model::{digest, is_uuid, validate_digest},
    plan::{FreezeChange, IntentChange, PackageChange, Request},
};

#[derive(Debug, Serialize)]
pub struct HistoryEntry {
    pub plan_id: String,
    pub revision: u64,
    pub details: Option<TransactionDetails>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionDetails {
    pub base_image: String,
    pub base_revision: u64,
    pub request: Request,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_phased_update: bool,
    pub package_changes: Vec<PackageChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intent_changes: Vec<IntentChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub freeze_changes: Vec<FreezeChange>,
    pub catalogs: BTreeMap<String, String>,
    pub planned_path_changes: usize,
    pub payload_bytes: u64,
    pub systemd_actions: usize,
    pub kernel_release: Option<String>,
}

pub fn transaction_history(root: &Path, limit: u32) -> Result<Vec<HistoryEntry>> {
    ensure!(limit > 0, "history limit must be positive");
    read(root, None, limit)
}

pub fn transaction_details(root: &Path, plan_id: &str) -> Result<HistoryEntry> {
    validate_digest(plan_id).context("invalid transaction plan digest")?;
    read(root, Some(plan_id), 1)?
        .pop()
        .with_context(|| format!("no completed transaction for plan {plan_id}"))
}

pub(super) fn record(
    tx: &Transaction<'_>,
    plan: &str,
    revision: u64,
    details: &TransactionDetails,
) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS transaction_details (
            plan TEXT PRIMARY KEY, data TEXT NOT NULL, sha256 TEXT NOT NULL
         )",
    )?;
    let data = serde_json::to_string(details)?;
    let hash = checksum(plan, revision, &data);
    tx.execute(
        "INSERT INTO transaction_details VALUES (?1, ?2, ?3)",
        (plan, data, hash),
    )?;
    Ok(())
}

fn checksum(plan: &str, revision: u64, data: &str) -> String {
    // The plan digest has a fixed width. The separators also distinguish the
    // committed revision from the beginning of the JSON record.
    digest(format!("{plan}\n{revision}\n{data}").as_bytes())
}

fn checked_details(
    plan: &str,
    revision: u64,
    data: &str,
    hash: &str,
) -> Result<TransactionDetails> {
    ensure!(
        checksum(plan, revision, data) == hash,
        "transaction details digest mismatch for {plan}"
    );
    let details: TransactionDetails = serde_json::from_str(data)?;
    let expected = if matches!(details.request, Request::ResolveConfig { .. }) {
        Some(details.base_revision)
    } else {
        details.base_revision.checked_add(1)
    };
    ensure!(
        expected == Some(revision) && is_uuid(&details.base_image),
        "transaction details do not match the committed revision"
    );
    Ok(details)
}

impl Image {
    pub(crate) fn rollout_identity(&self, settings: &Settings) -> Result<String> {
        if has_details(&self.db)? {
            // Boot environments inherit history but receive new image IDs.
            let mut statement = self.db.prepare(
                "SELECT h.plan, h.revision, d.data, d.sha256 FROM history h
                 JOIN transaction_details d ON d.plan=h.plan ORDER BY h.rowid LIMIT 1",
            )?;
            if let Some(row) = statement.query([])?.next()? {
                let plan: String = row.get(0)?;
                let revision: u64 = row.get(1)?;
                let data: String = row.get(2)?;
                let hash: String = row.get(3)?;
                validate_digest(&plan)?;
                ensure!(
                    revision <= settings.revision,
                    "invalid rollout history revision"
                );
                return Ok(checked_details(&plan, revision, &data, &hash)?.base_image);
            }
        }
        Ok(settings.id.clone())
    }
}

fn read(root: &Path, plan: Option<&str>, limit: u32) -> Result<Vec<HistoryEntry>> {
    let image = Image::open_query(root)?;
    let settings = image.settings()?;
    let archived = has_details(&image.db)?;
    let mut statement = if archived {
        image.db.prepare(
            "SELECT h.plan, h.revision, d.data, d.sha256 FROM history h
             LEFT JOIN transaction_details d ON d.plan=h.plan
             WHERE (?1 IS NULL OR h.plan=?1) ORDER BY h.revision DESC, h.rowid DESC LIMIT ?2",
        )?
    } else {
        image.db.prepare(
            "SELECT plan, revision, NULL, NULL FROM history
             WHERE (?1 IS NULL OR plan=?1) ORDER BY revision DESC, rowid DESC LIMIT ?2",
        )?
    };
    let mut rows = statement.query((plan, limit))?;
    let mut history = Vec::new();
    while let Some(row) = rows.next()? {
        let plan_id: String = row.get(0)?;
        validate_digest(&plan_id).context("invalid committed plan digest")?;
        let revision: u64 = row.get(1)?;
        ensure!(
            revision <= settings.revision,
            "committed transaction exceeds the image revision"
        );
        let details = row
            .get::<_, Option<String>>(2)?
            .map(|data| -> Result<_> {
                let hash: String = row.get(3)?;
                checked_details(&plan_id, revision, &data, &hash)
            })
            .transpose()?;
        history.push(HistoryEntry {
            plan_id,
            revision,
            details,
        });
    }
    Ok(history)
}

fn has_details(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='transaction_details')",
        [],
        |row| row.get(0),
    )?)
}
