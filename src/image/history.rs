use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use rusqlite::Transaction;
use serde::{Deserialize, Serialize};

use super::{Image, Settings};
use crate::{
    model::{digest, is_uuid, validate_digest},
    plan::{FreezeChange, IntentChange, PackageChange, Request},
};

/// The format of the transaction details that this CPS records.
pub(crate) const DETAILS_FORMAT: u32 = 1;

#[derive(Debug, Serialize)]
pub(crate) struct HistoryEntry {
    pub plan_id: String,
    pub revision: u64,
    /// The operation, such as `install`.
    pub operation: String,
    /// `None` when another CPS version recorded details that this one
    /// cannot read. The entry still lists the transaction.
    pub details: Option<TransactionDetails>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransactionDetails {
    pub format: u32,
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

pub(crate) fn transaction_history(root: &Path, limit: u32) -> Result<Vec<HistoryEntry>> {
    ensure!(limit > 0, "history limit must be positive");
    read(root, None, limit)
}

pub(crate) fn transaction_details(root: &Path, plan_id: &str) -> Result<HistoryEntry> {
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
    let data = serde_json::to_string(details)?;
    let hash = checksum(plan, revision, &data);
    tx.execute(
        "INSERT INTO transaction_details VALUES (?1, ?2, ?3)",
        (plan, data, hash),
    )?;
    Ok(())
}

pub(crate) fn checksum(plan: &str, revision: u64, data: &str) -> String {
    digest(format!("cps transaction details v1\n{plan}\n{revision}\n{data}").as_bytes())
}

/// The fields of transaction details that every format keeps, so the
/// history stays readable whatever else a later CPS records.
#[derive(Deserialize)]
struct DetailsCore {
    base_image: String,
    base_revision: u64,
    request: RequestKind,
}

#[derive(Deserialize)]
struct RequestKind {
    kind: String,
}

/// Committed details, checked against their digest and revision.
struct Checked {
    base_image: String,
    operation: String,
    details: Option<TransactionDetails>,
}

fn checked_details(plan: &str, revision: u64, data: &str, hash: &str) -> Result<Checked> {
    ensure!(
        checksum(plan, revision, data) == hash,
        "transaction details digest mismatch for {plan}"
    );
    let core: DetailsCore = serde_json::from_str(data)
        .with_context(|| format!("reading the transaction details of {plan}"))?;
    // Resolving a configuration update records the revision it started at.
    let expected = if core.request.kind == "resolve_config" {
        Some(core.base_revision)
    } else {
        core.base_revision.checked_add(1)
    };
    ensure!(
        expected == Some(revision) && is_uuid(&core.base_image),
        "transaction details do not match the committed revision"
    );
    let details = serde_json::from_str::<TransactionDetails>(data)
        .ok()
        .filter(|details| details.format == DETAILS_FORMAT);
    Ok(Checked {
        base_image: core.base_image,
        operation: details.as_ref().map_or(core.request.kind, |details| {
            details.request.operation().to_owned()
        }),
        details,
    })
}

impl Image {
    pub(crate) fn rollout_identity(&self, settings: &Settings) -> Result<String> {
        // Boot environments inherit history but receive new image IDs.
        let mut statement = self.db.prepare(
            "SELECT h.plan, h.revision, d.data, d.sha256 FROM history h
             LEFT JOIN transaction_details d ON d.plan=h.plan ORDER BY h.rowid LIMIT 1",
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
        Ok(settings.id.clone())
    }
}

fn read(root: &Path, plan: Option<&str>, limit: u32) -> Result<Vec<HistoryEntry>> {
    let image = Image::open_query(root)?;
    let settings = image.settings()?;
    let mut statement = image.db.prepare(
        "SELECT h.plan, h.revision, d.data, d.sha256 FROM history h
         LEFT JOIN transaction_details d ON d.plan=h.plan
         WHERE (?1 IS NULL OR h.plan=?1) ORDER BY h.revision DESC, h.rowid DESC LIMIT ?2",
    )?;
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
        let data: String = row.get(2)?;
        let hash: String = row.get(3)?;
        let checked = checked_details(&plan_id, revision, &data, &hash)?;
        history.push(HistoryEntry {
            plan_id,
            revision,
            operation: checked.operation,
            details: checked.details,
        });
    }
    Ok(history)
}
