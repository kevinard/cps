mod elf;

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs::{self, File},
    io::Seek,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{
    boot::kernel::Artifacts,
    image::{
        Image, Managed, Publisher, Settings, State, StateDelta,
        root::{self, DirSyncs, Fingerprint, ReplacementSource, Root},
    },
    model::{Node, copy_payload, digest_reader, validate_digest},
    plan::{Plan, Request, Write, build, config_update_prefix, hardlink_groups, pending_repairs},
    repo::{
        BuildReceipt, Published,
        release::{ArchiveCache, DeltaBase, fetch_payloads},
        stage_blob,
    },
    systemd::{self, Action as SystemdAction},
    triggers::{self, TriggerRun},
    verify::{Checked, Contents, check_transaction_state, manifest_paths_since},
};

const JOURNAL_FORMAT: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    format: u32,
    id: String,
    plan: Plan,
    /// The next state as changes to the committed revision it replaces.
    next: StateDelta,
    /// The digest of `next`.
    state_sha256: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    files_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    live_jobs: Option<systemd::LiveJobs>,
    /// How many of the plan's triggers have run and published their outputs.
    #[serde(default, skip_serializing_if = "is_zero")]
    triggers_done: usize,
    /// Progress of an abort. Once recorded, the transaction only goes back:
    /// resume and abort both finish returning to the committed revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    abort: Option<AbortProgress>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AbortProgress {
    /// Every journaled path is back in its state before the transaction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    files_ready: bool,
    /// Live service jobs that the abort runs around its writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    live_jobs: Option<systemd::LiveJobs>,
    /// How many of the inverse plan's triggers have run and published their outputs.
    #[serde(default, skip_serializing_if = "is_zero")]
    triggers_done: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PendingRecord {
    Live(Box<Journal>),
    Journaled(Box<Journal>),
}

struct PendingWork<'a> {
    id: &'a str,
    writes: &'a [Write],
    next: &'a StateDelta,
    systemd: &'a [SystemdAction],
    triggers: &'a [TriggerRun],
    retired_triggers: &'a [TriggerRun],
    triggers_done: usize,
    kernel_install: Option<&'a Artifacts>,
    files_ready: bool,
}

impl PendingRecord {
    fn is_live(&self) -> bool {
        matches!(self, Self::Live(_))
    }

    fn journal(&self) -> &Journal {
        match self {
            Self::Journaled(journal) | Self::Live(journal) => journal,
        }
    }

    fn journal_mut(&mut self) -> &mut Journal {
        match self {
            Self::Journaled(journal) | Self::Live(journal) => journal,
        }
    }

    fn aborting(&self) -> bool {
        self.journal().abort.is_some()
    }

    fn work(&self) -> PendingWork<'_> {
        let journal = self.journal();
        PendingWork {
            id: &journal.id,
            writes: &journal.plan.writes,
            next: &journal.next,
            systemd: &journal.plan.systemd,
            triggers: &journal.plan.triggers,
            retired_triggers: &journal.plan.retired_triggers,
            triggers_done: journal.triggers_done,
            kernel_install: journal.plan.kernel_install.as_ref(),
            files_ready: journal.files_ready,
        }
    }

    fn mark_files_ready(&mut self) {
        match self {
            Self::Journaled(journal) | Self::Live(journal) => journal.files_ready = true,
        }
    }

    fn mark_triggers_done(&mut self, count: usize) {
        match self {
            Self::Journaled(journal) | Self::Live(journal) => journal.triggers_done = count,
        }
    }

    fn recorded_plan(&self) -> Result<&Plan> {
        let journal = self.journal();
        ensure!(
            journal.format == JOURNAL_FORMAT,
            "unsupported transaction journal format {}",
            journal.format
        );
        ensure!(
            journal.plan.id()? == journal.id,
            "journaled plan digest mismatch"
        );
        ensure!(
            state_digest(&journal.next)? == journal.state_sha256,
            "journaled image state digest mismatch"
        );
        Ok(&journal.plan)
    }
}

fn state_digest(state: &StateDelta) -> Result<String> {
    let mut sha = Sha256::new();
    serde_json::to_writer(&mut sha, state)?;
    Ok(hex::encode(sha.finalize()))
}

#[derive(Debug)]
pub struct TransactionStatus {
    pub revision: u64,
    pub pending: Option<PendingTransaction>,
}

#[derive(Debug)]
pub struct PendingTransaction {
    pub plan_id: String,
    pub target_revision: u64,
    pub files_ready: bool,
    pub plan_summary: String,
    pub live: bool,
    /// An abort has started and must be finished.
    pub aborting: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    Planned,
    PayloadValidated,
    Blocked,
    Applying,
    Aborting,
    Applied,
    Superseded,
    PreparedForBoot,
    StagedForBoot,
    TrialBoot,
    BootedKnownGood,
    InactiveBootEnvironment,
}

#[derive(Debug, Serialize)]
pub struct TransactionProgress {
    pub plan_id: String,
    pub phase: TransactionPhase,
    /// Committed revision, or the recorded transaction revision if already applied.
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_environment: Option<String>,
}

pub fn transaction_status(path: &Path) -> Result<TransactionStatus> {
    let image = Image::open_query(path)?;
    let settings = image.settings()?;
    let pending = image
        .pending::<PendingRecord>()?
        .map(|pending| -> Result<_> {
            let plan_summary = pending.recorded_plan()?.summary();
            let work = pending.work();
            validate_digest(work.id)?;
            ensure!(
                settings.id == work.next.settings.id,
                "pending transaction targets a different image"
            );
            Ok(PendingTransaction {
                plan_id: work.id.to_owned(),
                target_revision: work.next.settings.revision,
                files_ready: work.files_ready,
                plan_summary,
                live: pending.is_live(),
                aborting: pending.aborting(),
            })
        })
        .transpose()?;
    Ok(TransactionStatus {
        revision: settings.revision,
        pending,
    })
}

/// Report progress for the exact saved plan, optionally checking cached payloads now.
pub fn transaction_progress(
    path: &Path,
    plan: &Plan,
    validate: bool,
) -> Result<TransactionProgress> {
    let image = Image::open_query(path)?;
    let settings = image.settings()?;
    let plan_id = plan.id()?;
    let (phase, revision) = if let Some(revision) = image.applied(&plan_id)? {
        (TransactionPhase::Applied, revision)
    } else {
        ensure!(settings.id == plan.image, "plan targets a different image");
        let status = match image.pending::<PendingRecord>()? {
            Some(pending) => {
                pending.recorded_plan()?;
                let work = pending.work();
                validate_digest(work.id)?;
                ensure!(
                    settings.id == work.next.settings.id,
                    "pending transaction targets a different image"
                );
                if work.id == plan_id && pending.aborting() {
                    TransactionPhase::Aborting
                } else if work.id == plan_id {
                    TransactionPhase::Applying
                } else {
                    TransactionPhase::Blocked
                }
            }
            None if check_plan_revision(&settings, plan).is_err() => TransactionPhase::Superseded,
            None if validate => {
                validate_in_image(&image, plan)?;
                TransactionPhase::PayloadValidated
            }
            None => TransactionPhase::Planned,
        };
        (status, settings.revision)
    };
    Ok(TransactionProgress {
        plan_id,
        phase,
        revision,
        boot_environment: None,
    })
}

struct OwnedPayload<'a> {
    node: &'a Node,
    package: &'a Published,
    base: Option<DeltaBase<'a>>,
}

type PublisherPayloads<'a> = (&'a Publisher, BTreeMap<&'a str, OwnedPayload<'a>>);

pub fn apply(path: &Path, plan: &Plan) -> Result<u64> {
    apply_with(path, plan, |_| {})
}

#[derive(Debug)]
pub(crate) struct PublisherFetchError(pub(crate) String);

impl std::fmt::Display for PublisherFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "preparing payloads from {}", self.0)
    }
}

impl std::error::Error for PublisherFetchError {}

pub fn fetch_plan(path: &Path, plan: &Plan) -> Result<usize> {
    fetch_plan_with_archives(path, plan, &[])
}

/// Fetch verified plan payloads and recover exact historical pins from provisioned archives.
pub fn fetch_plan_with_archives(
    path: &Path,
    plan: &Plan,
    archive_caches: &[(String, PathBuf)],
) -> Result<usize> {
    let image = Image::open_query(path)?;
    image.require_clean()?;
    ensure!(
        plan.image == image.settings()?.id,
        "plan targets a different image"
    );
    if image.applied(&plan.id()?)?.is_some() {
        return Ok(0);
    }
    let state = image.state()?;
    check_plan_revision(&state.settings, plan)?;
    let mut archives = BTreeMap::<_, Vec<_>>::new();
    for (name, path) in archive_caches {
        let publisher = state
            .publishers
            .get(name)
            .with_context(|| format!("archive publisher {name} is not configured"))?;
        archives.entry(name.as_str()).or_default().push(
            ArchiveCache::open(path, publisher)
                .with_context(|| format!("opening archive cache {}", path.display()))?,
        );
    }
    for publisher in state.publishers.as_slice() {
        let pin = plan.catalogs.get(&publisher.name).map(String::as_str);
        if let Some(archive) = archives
            .get(publisher.name.as_str())
            .into_iter()
            .flatten()
            .find(|archive| Some(archive.pin()) == pin)
        {
            archive.restore_metadata(publisher).with_context(|| {
                format!("restoring pinned catalog metadata for {}", publisher.name)
            })?;
        }
    }
    let (checked, next, _) = check_plan(&image, state, plan)?;
    if checked.config_update_revision.is_some() {
        return Ok(0);
    }
    let mut count = 0;
    for (publisher, mut blobs) in payload_sources(&checked.writes, &next)? {
        count += blobs.len();
        for archive in archives.get(publisher.name.as_str()).into_iter().flatten() {
            let mut matches = BTreeMap::new();
            let mut packages = BTreeMap::new();
            for (&hash, payload) in &blobs {
                let covered = match packages.entry(payload.package.manifest.name.as_str()) {
                    Entry::Occupied(entry) => *entry.get(),
                    Entry::Vacant(entry) => *entry.insert(archive.covers(payload.package)?),
                };
                if covered {
                    matches.insert(hash, payload.node);
                }
            }
            for hash in matches.keys() {
                blobs.remove(hash);
            }
            archive.restore(publisher, matches)?;
        }
        let blobs = blobs.into_iter().map(|(hash, payload)| {
            let Node::File { size, .. } = payload.node else {
                unreachable!("payloads contain only files");
            };
            (hash, *size, payload.base)
        });
        fetch_payloads(publisher, &image.root, blobs)
            .with_context(|| PublisherFetchError(publisher.name.clone()))?;
    }
    Ok(count)
}

fn check_plan(
    image: &Image,
    state: State,
    plan: &Plan,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    check_plan_revision(&state.settings, plan)?;
    let (checked, next, receipts) = build(
        image,
        state,
        plan.request.clone(),
        Some(plan),
        plan.install_recommends,
    )?;
    ensure!(
        &checked == plan,
        "saved plan differs from the verified plan; generate a new plan"
    );
    Ok((checked, next, receipts))
}

fn check_plan_revision(settings: &Settings, plan: &Plan) -> Result<()> {
    ensure!(
        plan.base_revision == settings.revision,
        "stale plan for revision {}; image is at revision {}",
        plan.base_revision,
        settings.revision
    );
    plan.check_publisher_generation(settings)
}

/// Check a saved plan against cached payloads using disposable image-local staging.
pub fn validate_plan(path: &Path, plan: &Plan) -> Result<()> {
    let image = Image::open_query(path)?;
    validate_in_image(&image, plan)
}

fn validate_in_image(image: &Image, plan: &Plan) -> Result<()> {
    image.require_clean()?;
    let state = image.state()?;
    ensure!(
        plan.image == state.settings.id,
        "plan targets a different image"
    );
    let check_elf = elf::applies(plan, &state);
    let base = state.clone();
    let (checked, next, _) = check_plan(image, state, plan)?;
    let records = image.file_records(None)?;
    let drift = check_drift(&image.root, &checked, &next, &base, &records)?;
    let parent = image.stage()?;
    let scratch = tempfile::tempdir_in(Root::fd_path(parent.as_file()))?;
    let stage = Root::open(scratch.path())?;
    prepare(&image.root, &stage, &checked, &next)?;
    if check_elf {
        let known = elf::Known::new(&records, &drift, &checked);
        elf::check(
            &image.root,
            stage.as_file(),
            &checked,
            &base.paths,
            &next,
            &known,
        )?;
    }
    Ok(())
}

pub(crate) fn validate_cached_payloads(plan: &Plan, state: &State) -> Result<()> {
    let mut unavailable = Vec::new();
    for (publisher, blobs) in payload_sources(&plan.writes, state)? {
        for (hash, payload) in blobs {
            let Node::File { size, .. } = payload.node else {
                unreachable!("payloads contain only files");
            };
            if !crate::repo::release::cached_object(
                &publisher.repo,
                &format!("blobs/{hash}"),
                hash,
                *size,
            )? {
                unavailable.push(format!("{} {hash}", publisher.name));
            }
        }
    }
    ensure!(
        unavailable.is_empty(),
        "missing or damaged bundle payloads:\n{}",
        unavailable.join("\n")
    );
    Ok(())
}

pub(crate) fn apply_with(path: &Path, plan: &Plan, after_write: impl FnMut(&Write)) -> Result<u64> {
    apply_to_image(Image::open_for_apply(path)?, plan, None, after_write)
}

pub(crate) fn apply_live(
    image: Image,
    plan: &Plan,
    stage_packages: &BTreeSet<String>,
) -> Result<u64> {
    apply_to_image(image, plan, Some(stage_packages), |_| {})
}

pub(crate) fn apply_to_image(
    mut image: Image,
    plan: &Plan,
    live: Option<&BTreeSet<String>>,
    after_write: impl FnMut(&Write),
) -> Result<u64> {
    let id = plan.id()?;
    ensure!(
        plan.image == image.settings()?.id,
        "plan targets a different image"
    );
    if let Some(pending) = image.pending::<PendingRecord>()? {
        ensure!(
            pending.work().id == id,
            "a different plan is pending; use transaction status to identify it"
        );
        ensure!(
            !pending.aborting(),
            "this plan is being aborted; finish with transaction abort, then apply it again"
        );
        let state = image.state()?;
        return resume_pending(
            &mut image,
            &state,
            pending,
            live.is_some(),
            None,
            after_write,
        );
    }
    if let Some(revision) = image.applied(&id)? {
        discard_stage(&image);
        return Ok(revision);
    }
    let state = image.state()?;
    let check_elf = elf::applies(plan, &state);
    let base = state.clone();
    let (checked, mut next, receipts) = check_plan(&image, state, plan)?;
    let records = image.file_records(None)?;
    let drift = check_drift(&image.root, &checked, &next, &base, &records)?;
    let stage = image.stage()?;
    let after = if let Some(stage_packages) = live {
        check_live_plan(&checked, stage_packages)?;
        next.settings.boot_revision = Some(base.settings.boot_revision());
        AfterFiles::Live(systemd::LiveServices::plan(&checked, &base, &next).active_jobs()?)
    } else {
        AfterFiles::offline(&image.root, &checked)?
    };
    let staged = prepare(&image.root, &stage, &checked, &next)?;
    let mut learned = Vec::new();
    if check_elf {
        let known = elf::Known::new(&records, &drift, &checked);
        learned = elf::check(
            &image.root,
            stage.as_file(),
            &checked,
            &base.paths,
            &next,
            &known,
        )?;
    }
    let repo_backed = !checked.catalogs.is_empty();
    let delta = StateDelta::between(&base, &next);
    let journal = Box::new(Journal {
        format: JOURNAL_FORMAT,
        id,
        plan: checked,
        state_sha256: state_digest(&delta)?,
        next: delta,
        files_ready: false,
        live_jobs: match &after {
            AfterFiles::Live(jobs) => Some(jobs.clone()),
            _ => None,
        },
        triggers_done: 0,
        abort: None,
    });
    let pending = if live.is_some() {
        PendingRecord::Live(journal)
    } else {
        PendingRecord::Journaled(journal)
    };
    // Fingerprints and ELF metadata learned above join the pending record's
    // SQLite transaction rather than costing syncs of their own.
    let files = crate::image::FileUpdates {
        observed: Some(&drift.observed),
        stale: Some(&drift.stale),
        elf: learned,
    };
    image.record_pending(&pending, &next, &receipts, repo_backed, &files)?;
    let transaction = Transaction {
        base: &base,
        next: &next,
        staged: Some(&staged),
    };
    finish(
        &mut image,
        &stage,
        pending,
        transaction,
        after,
        None,
        after_write,
    )
}

/// Check that paths the plan leaves unchanged still match their manifests.
///
/// Files whose recorded fingerprint still matches are trusted; the rest are
/// hashed. The result carries new fingerprints to record.
fn check_drift(
    root: &Root,
    plan: &Plan,
    next: &State,
    base: &State,
    records: &crate::image::FileRecords,
) -> Result<Checked> {
    if !drift_checked(plan) {
        return Ok(Checked {
            findings: Vec::new(),
            observed: BTreeMap::new(),
            stale: BTreeSet::new(),
        });
    }
    let changed = changed_paths(plan);
    let checked = check_transaction_state(
        root,
        next,
        Contents::Fingerprints(records),
        &base.installed,
        |path| !changed.contains(path),
    )?;
    if let Some(finding) = checked.errors().next() {
        anyhow::bail!(
            "pre-existing drift at {}; repair the image before applying the transaction",
            finding.message
        );
    }
    Ok(checked)
}

/// Recovery plans repair drift, and resolving configuration only touches
/// the selected path, so neither requires a drift-free image.
fn drift_checked(plan: &Plan) -> bool {
    !plan.is_recovery() && !matches!(plan.request, Request::ResolveConfig { .. })
}

fn changed_paths(plan: &Plan) -> BTreeSet<&str> {
    plan.writes
        .iter()
        .filter(|write| write.before != write.after || write.detach)
        .map(|write| write.path.as_str())
        .collect()
}

pub fn resume_transaction(path: &Path) -> Result<Option<u64>> {
    resume_image(Image::open_for_apply(path)?, false, None)
}

/// Restore damaged managed paths from verified local payloads and resume.
pub fn repair_pending_transaction(path: &Path) -> Result<Option<u64>> {
    resume_image(Image::open_for_apply(path)?, false, Some(&BTreeSet::new()))
}

pub(crate) fn resume_live(image: Image) -> Result<Option<u64>> {
    resume_image(image, true, None)
}

pub(crate) fn repair_pending_live(
    image: Image,
    stage_packages: &BTreeSet<String>,
) -> Result<Option<u64>> {
    resume_image(image, true, Some(stage_packages))
}

fn resume_image(
    mut image: Image,
    live: bool,
    repair: Option<&BTreeSet<String>>,
) -> Result<Option<u64>> {
    let state = image.state()?;
    let Some(pending) = image.pending::<PendingRecord>()? else {
        discard_stage(&image);
        return Ok(None);
    };
    if pending.aborting() {
        // An abort that started is the only way out of this transaction.
        return abort_pending(&mut image, &state, pending, live, false, |_| {}).map(Some);
    }
    resume_pending(&mut image, &state, pending, live, repair, |_| {}).map(Some)
}

fn check_live_plan(plan: &Plan, stage_packages: &BTreeSet<String>) -> Result<()> {
    ensure!(
        matches!(
            plan.request,
            Request::Install { .. }
                | Request::Update
                | Request::UpdateProfile
                | Request::Remove { .. }
                | Request::Autoremove
                | Request::Repair { .. }
                | Request::RepairPaths { .. }
                | Request::Revert { .. }
        ),
        "this request cannot be applied live"
    );
    if let Some(reason) = plan.boot_reason(stage_packages) {
        anyhow::bail!("staging required because {reason}");
    }
    Ok(())
}

fn resume_pending(
    image: &mut Image,
    state: &State,
    mut pending: PendingRecord,
    live: bool,
    repair: Option<&BTreeSet<String>>,
    after_write: impl FnMut(&Write),
) -> Result<u64> {
    ensure!(
        pending.is_live() == live,
        "resume the transaction in its original mode"
    );
    let plan = pending.recorded_plan()?;
    let work = pending.work();
    validate_digest(work.id)?;
    let next = work
        .next
        .apply(state)
        .context("rebuilding the pending state from the committed revision")?;
    plan.check_recovery_state(state, &next)?;
    let expected_boot_revision = if live {
        Some(state.settings.boot_revision())
    } else {
        state.settings.boot_revision
    };
    ensure!(
        next.settings.boot_revision == expected_boot_revision,
        "pending state has a different boot revision"
    );
    ensure!(
        manifest_paths_since(&next, Some(&state.installed))? == next.paths,
        "pending ownership index differs from its installed manifests"
    );
    image
        .verify_catalog_archives(&next)
        .context("verifying catalog archives for the pending transaction")?;
    let after = if let PendingRecord::Live(journal) = &pending {
        check_live_plan(plan, repair.unwrap_or(&BTreeSet::new()))?;
        let services = systemd::LiveServices::plan(plan, state, &next);
        if let Some(jobs) = &journal.live_jobs {
            services.check_jobs(jobs)?;
            AfterFiles::Live(jobs.clone())
        } else {
            AfterFiles::Live(services.active_jobs()?)
        }
    } else {
        AfterFiles::offline(&image.root, plan)?
    };
    ensure!(
        !work.files_ready || !matches!(after, AfterFiles::None),
        "pending transaction has an invalid final phase"
    );
    if let (AfterFiles::Live(jobs), PendingRecord::Live(journal)) = (&after, &mut pending)
        && journal.live_jobs.is_none()
    {
        journal.live_jobs = Some(jobs.clone());
        image.update_pending(&pending)?;
    }
    let stage = image.stage()?;
    let transaction = Transaction {
        base: state,
        next: &next,
        staged: None,
    };
    finish(
        image,
        &stage,
        pending,
        transaction,
        after,
        repair,
        after_write,
    )
}

/// Abort the pending transaction of an inactive image.
///
/// Every journaled write runs backwards, so the image returns to the
/// committed revision, which the abort leaves unchanged. Returns that
/// revision, or `None` without a pending transaction. With `fetch`, missing
/// package payloads are downloaded from each publisher's mirror first.
pub fn abort_transaction(path: &Path, fetch: bool) -> Result<Option<u64>> {
    abort_image(Image::open_for_apply(path)?, false, fetch, |_| {})
}

pub(crate) fn abort_live(image: Image, fetch: bool) -> Result<Option<u64>> {
    abort_image(image, true, fetch, |_| {})
}

#[cfg(test)]
pub(crate) fn abort_with(path: &Path, after_write: impl FnMut(&Write)) -> Result<Option<u64>> {
    abort_image(Image::open_for_apply(path)?, false, false, after_write)
}

fn abort_image(
    mut image: Image,
    live: bool,
    fetch: bool,
    after_write: impl FnMut(&Write),
) -> Result<Option<u64>> {
    let state = image.state()?;
    let Some(pending) = image.pending::<PendingRecord>()? else {
        discard_stage(&image);
        return Ok(None);
    };
    abort_pending(&mut image, &state, pending, live, fetch, after_write).map(Some)
}

/// Return every journaled path to its committed state and drop the pending record.
///
/// The abort is recorded in the pending record before it changes anything,
/// and from then on both resume and abort finish it. Its writes are the
/// plan's writes reversed, checked like any other: each path must be in its
/// state after the transaction, before it, or for a transition in one of
/// the recognized intermediate states, so an interrupted abort resumes.
fn abort_pending(
    image: &mut Image,
    base: &State,
    mut pending: PendingRecord,
    live: bool,
    fetch: bool,
    mut after_write: impl FnMut(&Write),
) -> Result<u64> {
    ensure!(
        pending.is_live() == live,
        if pending.is_live() {
            "the pending transaction changed the running system; abort it with cps transaction abort without --root"
        } else {
            "the pending transaction belongs to an inactive image; abort it with cps transaction abort --root"
        }
    );
    let plan = pending.recorded_plan()?.clone();
    let work = pending.work();
    validate_digest(work.id)?;
    let next = work
        .next
        .apply(base)
        .context("rebuilding the pending state from the committed revision")?;
    let inverse = plan.inverse(base, &next)?;
    let stage = image.stage()?;
    let mut payloads = abort_payloads(&image.root, &stage, &inverse, base)?;
    if fetch && !payloads.missing.is_empty() {
        fetch_abort_payloads(&image.root, &payloads.missing)?;
        payloads = abort_payloads(&image.root, &stage, &inverse, base)?;
    }
    payloads.check()?;
    let services = live.then(|| systemd::LiveServices::plan(&inverse, &next, base));
    match (&services, &pending.journal().abort) {
        (_, None) => {
            // Services that the transaction stopped because it removed
            // their units start again once the abort restores those units.
            // They are journaled with the abort before anything changes.
            let live_jobs = services
                .as_ref()
                .map(|services| -> Result<_> {
                    let mut jobs = services.active_jobs()?;
                    if let Some(forward) = &pending.journal().live_jobs {
                        jobs.start_stopped_by(forward);
                    }
                    Ok(jobs)
                })
                .transpose()?;
            pending.journal_mut().abort = Some(AbortProgress {
                files_ready: false,
                live_jobs,
                triggers_done: 0,
            });
            image.update_pending(&pending)?;
        }
        (Some(services), Some(progress)) => services.check_jobs(
            progress
                .live_jobs
                .as_ref()
                .context("live abort has no service jobs")?,
        )?,
        (None, Some(_)) => {}
    }
    payloads.stage(&image.root, &stage)?;
    let progress = pending
        .journal_mut()
        .abort
        .as_mut()
        .expect("recorded abort");
    let mut jobs = progress.live_jobs.clone();
    // ExecStop can need the very files that the abort will remove.
    if let Some(jobs) = &mut jobs
        && jobs.stop_removed_units()?
    {
        progress.live_jobs = Some(jobs.clone());
        image.update_pending(&pending)?;
    }
    let files_ready = pending
        .journal()
        .abort
        .as_ref()
        .is_some_and(|abort| abort.files_ready);
    if !files_ready {
        let generated = triggers::outputs_of(&next, base);
        write_files(
            &image.root,
            &stage,
            &inverse.writes,
            Target {
                paths: &base.paths,
                generated: &generated,
            },
            false,
            None,
            &mut after_write,
        )?;
    }
    systemd::clean_enablement(&image.root, &inverse.writes, &base.paths)?;
    let actions = live
        || !inverse.systemd.is_empty()
        || !inverse.triggers.is_empty()
        || !inverse.retired_triggers.is_empty()
        || inverse.kernel_install.is_some();
    if actions && !files_ready {
        pending
            .journal_mut()
            .abort
            .as_mut()
            .expect("recorded abort")
            .files_ready = true;
        image.update_pending(&pending)?;
    }
    let mut observed = BTreeMap::new();
    if actions {
        let path = if live {
            PathBuf::from("/")
        } else {
            systemd::offline_path(&image.root)?
        };
        if !inverse.systemd.is_empty() {
            systemd::run(&image.root, &path, &inverse.systemd, base, live)?;
        }
        // Generated outputs follow the restored files, exactly as after
        // applying a plan from `next` to `base`. Retiring and pruning are
        // idempotent, and each completed trigger is journaled, so a resumed
        // abort runs only the rest.
        if !inverse.triggers.is_empty() || !inverse.retired_triggers.is_empty() {
            triggers::retire(&image.root, &inverse.retired_triggers, &next, base)?;
            root::test_point("abort-retired");
            let done = pending
                .journal()
                .abort
                .as_ref()
                .map_or(0, |abort| abort.triggers_done);
            for (index, run) in inverse.triggers.iter().enumerate().skip(done) {
                root::test_point(&format!("abort-trigger-{index}"));
                triggers::run(&image.root, &stage, &path, run, base)?;
                pending
                    .journal_mut()
                    .abort
                    .as_mut()
                    .expect("recorded abort")
                    .triggers_done = index + 1;
                image.update_pending(&pending)?;
            }
            triggers::prune(&image.root, &inverse.writes, base)?;
            root::test_point("abort-pruned");
        }
        if let Some(kernel) = &inverse.kernel_install {
            kernel.prepare(image, &path, &base.settings)?;
        }
        // Each journaled path is back in its state before the transaction.
        // That state differs from the committed manifest where the path
        // had drifted before the transaction replaced it, or where a
        // recovery plan was repairing it, and the abort keeps that drift
        // for verify to report rather than rejecting its own result.
        let mut restored = BTreeSet::new();
        for write in &inverse.writes {
            if base.paths.get(&write.path).map(|managed| &managed.node) != write.after.as_ref()
                && image.root.inspect(&write.path)? == write.after
            {
                restored.insert(write.path.as_str());
            }
        }
        let records = image.file_records(None)?;
        let checked = check_transaction_state(
            &image.root,
            base,
            Contents::Fingerprints(&records),
            &base.installed,
            |path| !restored.contains(path),
        )?;
        if let Some(finding) = checked.errors().next() {
            anyhow::bail!(
                "aborting left an inconsistent image: {}; correct it and abort again",
                finding.message
            );
        }
        observed = checked.observed;
        if let Some(jobs) = &jobs {
            jobs.run()?;
        }
    }
    let written = plan
        .writes
        .iter()
        .map(|write| write.path.as_str())
        .collect();
    image.abort_pending(&written, &observed, base)?;
    discard_stage(image);
    Ok(base.settings.revision)
}

/// Where the contents that an abort restores come from.
enum AbortSource<'a> {
    /// Already staged and verified.
    Staged,
    /// A path that the abort already restored, so its contents are verified.
    Image(&'a str),
    /// A publisher's payload cache or repository.
    Cache(&'a Publisher),
}

struct MissingPayload<'a> {
    path: &'a str,
    node: &'a Node,
    /// The committed package that declares these contents, if any.
    owner: Option<(&'a Publisher, String)>,
}

struct AbortPayloads<'a> {
    found: BTreeMap<&'a str, (&'a Node, AbortSource<'a>)>,
    missing: Vec<MissingPayload<'a>>,
}

fn abort_payloads<'a>(
    root: &Root,
    stage: &Root,
    inverse: &'a Plan,
    base: &'a State,
) -> Result<AbortPayloads<'a>> {
    let mut needed = BTreeMap::<&str, (&Node, Vec<&str>)>::new();
    for write in inverse.writes.iter().filter(|write| write.needs_blob()) {
        let Some(node @ Node::File { sha256, .. }) = &write.after else {
            unreachable!("only file replacements require a payload");
        };
        needed
            .entry(sha256.as_str())
            .or_insert((node, Vec::new()))
            .1
            .push(write.path.as_str());
    }
    let mut payloads = AbortPayloads {
        found: BTreeMap::new(),
        missing: Vec::new(),
    };
    'payloads: for (hash, (node, paths)) in needed {
        let Node::File { size, .. } = node else {
            unreachable!("payloads contain only files");
        };
        if checked_payload(stage, node).is_ok() {
            payloads.found.insert(hash, (node, AbortSource::Staged));
            continue;
        }
        for &path in &paths {
            if root.inspect(path)?.as_ref() == Some(node) {
                payloads
                    .found
                    .insert(hash, (node, AbortSource::Image(path)));
                continue 'payloads;
            }
        }
        let owner = paths.iter().find_map(|path| {
            let managed = base
                .paths
                .get(*path)
                .filter(|managed| managed.node == *node)?;
            let package = &base
                .installed
                .get(managed.owners.first()?)?
                .published
                .manifest;
            Some((base.publishers.get(&package.publisher)?, package.id()))
        });
        let candidates = owner
            .iter()
            .map(|(publisher, _)| *publisher)
            .chain(base.publishers.as_slice());
        for publisher in candidates {
            if crate::repo::release::cached_object(
                &publisher.repo,
                &format!("blobs/{hash}"),
                hash,
                *size,
            )? {
                payloads
                    .found
                    .insert(hash, (node, AbortSource::Cache(publisher)));
                continue 'payloads;
            }
        }
        payloads.missing.push(MissingPayload {
            path: paths[0],
            node,
            owner,
        });
    }
    Ok(payloads)
}

impl AbortPayloads<'_> {
    /// Refuse to start, or continue, an abort that cannot restore every path.
    fn check(&self) -> Result<()> {
        if self.missing.is_empty() {
            return Ok(());
        }
        let mut lines = vec![
            "cannot abort the pending transaction; the contents that restore these paths are unavailable:"
                .to_owned(),
        ];
        for missing in &self.missing {
            let Node::File { sha256, size, .. } = missing.node else {
                unreachable!("payloads contain only files");
            };
            lines.push(match &missing.owner {
                Some((_, package)) => format!(
                    "  {}: payload sha256 {sha256}, {size} bytes, from {package}",
                    missing.path
                ),
                None => format!(
                    "  {}: local contents sha256 {sha256} that the transaction replaced",
                    missing.path
                ),
            });
        }
        if self.missing.iter().any(|missing| missing.owner.is_some()) {
            lines.push(
                "Abort again with --fetch to download the package payloads from each publisher's mirror; \
                 the committed manifests pin their digests and sizes."
                    .to_owned(),
            );
        }
        if self.missing.iter().any(|missing| missing.owner.is_none()) {
            lines.push(
                "No package payload holds local contents. Restore those paths from a backup and abort \
                 again, or finish the transaction with resume."
                    .to_owned(),
            );
        }
        anyhow::bail!(lines.join("\n"))
    }

    fn stage(&self, root: &Root, stage: &Root) -> Result<()> {
        let dir = Root::fd_path(stage.as_file());
        for (hash, (node, source)) in &self.found {
            let mut file = NamedTempFile::new_in(&dir)?;
            match source {
                AbortSource::Staged => continue,
                AbortSource::Image(path) => {
                    copy_payload(&root.file(path)?, node, file.as_file_mut())
                        .with_context(|| format!("staging the restored contents of {path}"))?
                }
                AbortSource::Cache(publisher) => {
                    stage_blob(&publisher.repo, node, file.as_file_mut()).with_context(|| {
                        format!("staging payload {hash} from {}", publisher.name)
                    })?
                }
            }
            file.as_file().sync_all()?;
            file.persist(dir.join(hash))?;
        }
        Ok(stage.as_file().sync_all()?)
    }
}

fn fetch_abort_payloads(root: &Root, missing: &[MissingPayload<'_>]) -> Result<()> {
    let mut by_publisher = BTreeMap::<&str, (&Publisher, Vec<_>)>::new();
    for missing in missing {
        let (Some((publisher, _)), Node::File { sha256, size, .. }) =
            (&missing.owner, missing.node)
        else {
            continue;
        };
        by_publisher
            .entry(publisher.name.as_str())
            .or_insert((publisher, Vec::new()))
            .1
            .push((sha256.as_str(), *size, None));
    }
    for (publisher, blobs) in by_publisher.into_values() {
        fetch_payloads(publisher, root, blobs)
            .with_context(|| PublisherFetchError(publisher.name.clone()))?;
    }
    Ok(())
}

/// The committed and next states of a transaction being finished.
struct Transaction<'a> {
    base: &'a State,
    next: &'a State,
    /// Payloads staged and verified by this process, if any.
    staged: Option<&'a StagedPayloads>,
}

enum AfterFiles {
    None,
    Offline(PathBuf),
    Live(systemd::LiveJobs),
}

impl AfterFiles {
    fn offline(root: &Root, plan: &Plan) -> Result<Self> {
        if plan.systemd.is_empty()
            && plan.triggers.is_empty()
            && plan.retired_triggers.is_empty()
            && plan.kernel_install.is_none()
        {
            Ok(Self::None)
        } else {
            Ok(Self::Offline(systemd::offline_path(root)?))
        }
    }
}

fn finish(
    image: &mut Image,
    stage: &Root,
    mut pending: PendingRecord,
    transaction: Transaction<'_>,
    mut after: AfterFiles,
    repair: Option<&BTreeSet<String>>,
    mut after_write: impl FnMut(&Write),
) -> Result<u64> {
    let Transaction { base, next, staged } = transaction;
    // ExecStop can need the very files that this transaction will remove.
    if let (AfterFiles::Live(jobs), PendingRecord::Live(journal)) = (&mut after, &mut pending)
        && jobs.stop_removed_units()?
    {
        journal.live_jobs = Some(jobs.clone());
        image.update_pending(&pending)?;
    }
    let work = pending.work();
    let mut written: BTreeSet<String> =
        work.writes.iter().map(|write| write.path.clone()).collect();
    if !work.files_ready {
        let staged = if repair.is_some() {
            restore_pending_payloads(stage, work.writes, next)?;
            None
        } else {
            staged
        };
        let generated = triggers::outputs_of(base, next);
        write_files(
            &image.root,
            stage,
            work.writes,
            Target {
                paths: &next.paths,
                generated: &generated,
            },
            repair.is_some(),
            staged,
            &mut after_write,
        )?;
    }
    systemd::clean_enablement(&image.root, work.writes, &next.paths)?;
    if !work.files_ready && !matches!(after, AfterFiles::None) {
        pending.mark_files_ready();
        image.update_pending(&pending)?;
    }
    let work = pending.work();
    if let Some(stage_packages) = repair {
        written.extend(restore_pending_paths(
            &image.root,
            stage,
            next,
            pending.is_live(),
            stage_packages,
        )?);
    }
    let mut observed = BTreeMap::new();
    if !matches!(after, AfterFiles::None) {
        let path = match &after {
            AfterFiles::Offline(path) => path.as_path(),
            AfterFiles::Live(_) => Path::new("/"),
            AfterFiles::None => unreachable!(),
        };
        if !work.systemd.is_empty() {
            systemd::run(&image.root, path, work.systemd, next, pending.is_live())?;
        }
        if !work.triggers.is_empty() || !work.retired_triggers.is_empty() {
            // Retiring and pruning are idempotent. Each completed trigger is
            // journaled, so a resumed transaction runs only the rest.
            triggers::retire(&image.root, work.retired_triggers, base, next)?;
            let runs = work.triggers.to_vec();
            for (index, run) in runs.iter().enumerate().skip(work.triggers_done) {
                triggers::run(&image.root, stage, path, run, next)?;
                pending.mark_triggers_done(index + 1);
                image.update_pending(&pending)?;
            }
            triggers::prune(&image.root, pending.work().writes, next)?;
            root::test_point("triggers");
        }
        let work = pending.work();
        if let Some(kernel) = work.kernel_install {
            kernel.prepare(image, path, &next.settings)?;
        }
        let phase = if pending.is_live() { "live" } else { "offline" };
        let repaired = matches!(&pending, PendingRecord::Live(live) if live.plan.is_recovery())
            .then(|| {
                work.writes
                    .iter()
                    .map(|write| write.path.as_str())
                    .collect::<BTreeSet<_>>()
            });
        // Actions can touch any path, for example through tmpfiles rules,
        // and every change to a file sets a new ctime. Trusting matching
        // fingerprints still checks every managed path: directories, links,
        // and ownership exactly, and every file that the transaction wrote
        // or that changed since CPS last hashed it.
        let records = image.file_records(None)?;
        let checked = check_transaction_state(
            &image.root,
            next,
            Contents::Fingerprints(&records),
            &base.installed,
            |path| repaired.as_ref().is_none_or(|paths| paths.contains(path)),
        )?;
        if let Some(finding) = checked.errors().next() {
            anyhow::bail!(
                "{phase} actions left an inconsistent image: {}; try resuming with --repair",
                finding.message
            );
        }
        written.extend(checked.stale);
        observed = checked.observed;
        if let AfterFiles::Live(services) = after {
            services.run()?;
        }
    }
    let revision = next.settings.revision;
    let journal = pending.journal();
    let written = written.iter().map(String::as_str).collect();
    image.commit(
        &journal.id,
        &journal.next,
        next,
        &written,
        &observed,
        &journal.plan.transaction_details(),
    )?;
    discard_stage(image);
    Ok(revision)
}

fn discard_stage(image: &Image) {
    let result = (|| -> Result<()> {
        let stage = image.stage()?;
        clear_stage(&stage)?;
        Ok(stage.as_file().sync_all()?)
    })();
    if let Err(error) = result {
        eprintln!("Warning: could not clear transaction staging files: {error:#}");
    }
}

fn restore_pending_paths(
    root: &Root,
    stage: &Root,
    next: &State,
    live: bool,
    stage_packages: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let writes = pending_repairs(root, next)?;
    if live {
        for write in &writes {
            ensure!(
                !write.boot_sensitive()
                    && next.paths[&write.path].owners.is_disjoint(stage_packages),
                "repair of {} requires a staged recovery root",
                write.path
            );
        }
    }
    restore_pending_payloads(stage, &writes, next)?;
    let generated = triggers::active_outputs(&next.installed, &next.paths);
    let target = Target {
        paths: &next.paths,
        generated: &generated,
    };
    write_files(root, stage, &writes, target, false, None, &mut |_| {})?;
    Ok(writes.into_iter().map(|write| write.path).collect())
}

fn restore_pending_payloads(stage: &Root, writes: &[Write], next: &State) -> Result<()> {
    let dir = Root::fd_path(stage.as_file());
    for (publisher, blobs) in payload_sources(writes, next)? {
        for (hash, payload) in blobs {
            if checked_payload(stage, payload.node).is_ok() {
                continue;
            }
            let mut file = NamedTempFile::new_in(&dir)?;
            stage_blob(&publisher.repo, payload.node, file.as_file_mut()).with_context(|| {
                format!("restoring payload for {}", payload.package.manifest.id())
            })?;
            file.as_file().sync_all()?;
            file.persist(dir.join(hash))?;
        }
    }
    Ok(stage.as_file().sync_all()?)
}

/// Fingerprints of payloads that this process staged and hashed.
type StagedPayloads = BTreeMap<String, Fingerprint>;

/// The state that a transaction's writes lead to.
#[derive(Clone, Copy)]
struct Target<'a> {
    paths: &'a BTreeMap<String, Managed>,
    /// Output patterns of the triggers active before or after the writes.
    /// Generated paths inside a directory that a transition replaces
    /// leave with the old tree, and the triggers' next run regenerates
    /// whatever they still declare.
    generated: &'a [&'a str],
}

fn write_files(
    root: &Root,
    stage: &Root,
    writes: &[Write],
    target: Target<'_>,
    repair: bool,
    staged: Option<&StagedPayloads>,
    after_write: &mut impl FnMut(&Write),
) -> Result<()> {
    // Check every payload before touching the image. A payload this process
    // just staged was hashed while it was copied into the private staging
    // directory, so only its fingerprint is checked. Payloads left by an
    // interrupted run are hashed once here.
    let mut payloads = BTreeMap::new();
    for node in crate::plan::payloads(writes).into_values() {
        let Node::File { sha256, .. } = node else {
            unreachable!("payloads contain only files");
        };
        let fingerprint = match staged.and_then(|staged| staged.get(sha256)) {
            Some(fingerprint) => {
                stage.file_matching(sha256, fingerprint)?;
                *fingerprint
            }
            None => Fingerprint::of(&rustix::fs::fstat(checked_payload(stage, node)?)?),
        };
        payloads.insert(sha256.as_str(), fingerprint);
    }
    let Target { paths, generated } = target;
    let mut writer = Writer {
        root,
        stage,
        paths,
        generated,
        repair,
        payloads,
        syncs: DirSyncs::default(),
        copied_from: BTreeSet::new(),
        checked_target: None,
        groups: if writes.iter().any(|write| write.detach) {
            hardlink_groups(paths)
        } else {
            BTreeMap::new()
        },
        after_write,
    };
    let transitions: Vec<&str> = writes
        .iter()
        .filter(|write| write.transition().is_some())
        .map(|write| write.path.as_str())
        .collect();
    for write in writes {
        // A transition carries every write beneath its path.
        if transitions.iter().any(|directory| write.beneath(directory)) {
            continue;
        }
        if write.transition().is_some() {
            let children = writes
                .iter()
                .filter(|child| child.beneath(&write.path))
                .collect();
            writer.transition(write, children)?;
        } else {
            writer.write(write)?;
        }
    }
    // Every directory entry must be durable before the journal records the
    // files as ready or commits the transaction.
    root.sync_dirs(&mut writer.syncs, Some(stage.as_file()))?;
    debug_assert!(writer.syncs.is_empty());
    Ok(())
}

/// Executes a transaction's writes against the image root.
struct Writer<'a, F> {
    root: &'a Root,
    stage: &'a Root,
    paths: &'a BTreeMap<String, Managed>,
    generated: &'a [&'a str],
    repair: bool,
    payloads: BTreeMap<&'a str, Fingerprint>,
    syncs: DirSyncs,
    // Local configuration copied to a new path must be durable before its
    // source path changes, or a crash could lose it.
    copied_from: BTreeSet<&'a str>,
    checked_target: Option<(&'a str, File)>,
    groups: BTreeMap<&'a str, Vec<&'a str>>,
    after_write: &'a mut F,
}

impl<'a, F: FnMut(&Write)> Writer<'a, F> {
    fn staged_payload(&self, node: &Node) -> Result<File> {
        let Node::File { sha256, .. } = node else {
            anyhow::bail!("expected a file payload");
        };
        let fingerprint = self
            .payloads
            .get(sha256.as_str())
            .with_context(|| format!("missing staged blob {sha256}"))?;
        self.stage.file_matching(sha256, fingerprint)
    }

    /// Open a verified hard link target. A target inside a directory tree
    /// that is still being assembled in staging is opened there.
    fn hardlink_source(&mut self, target: &'a str, tree: Option<(&Root, &str)>) -> Result<File> {
        let managed = self
            .paths
            .get(target)
            .with_context(|| format!("missing hard link target {target}"))?;
        if let Some((tree, directory)) = tree
            && let Some(relative) = target
                .strip_prefix(directory)
                .and_then(|rest| rest.strip_prefix('/'))
        {
            return tree.checked_hardlink_target(relative, &managed.node);
        }
        if self
            .checked_target
            .as_ref()
            .is_none_or(|(name, _)| *name != target)
        {
            self.checked_target = Some((
                target,
                self.root.checked_hardlink_target(target, &managed.node)?,
            ));
        }
        let (_, file) = self.checked_target.as_ref().expect("checked target");
        Ok(file.try_clone()?)
    }

    fn source(
        &mut self,
        write: &'a Write,
        tree: Option<(&Root, &str)>,
    ) -> Result<Option<ReplacementSource<'a>>> {
        Ok(match &write.after {
            Some(node @ Node::File { .. }) => Some(match write.local_config_source() {
                Some((path, before)) => ReplacementSource::LocalConfig { path, before },
                None => ReplacementSource::Staged(self.staged_payload(node)?),
            }),
            Some(Node::Hardlink { target }) => Some(ReplacementSource::Staged(
                self.hardlink_source(target, tree)?,
            )),
            _ => None,
        })
    }

    fn write(&mut self, write: &'a Write) -> Result<()> {
        let root = self.root;
        if write.hardlink_target().is_some() || self.copied_from.contains(write.path.as_str()) {
            // A hard link publishes the target's new inode, so the target's
            // rename must be durable first.
            root.sync_dirs(&mut self.syncs, Some(self.stage.as_file()))?;
        }
        if let Some((source, _)) = write.local_config_source()
            && source != write.path
        {
            self.copied_from.insert(source);
        }
        let link_source = match write.hardlink_target() {
            Some(target) => Some(self.hardlink_source(target, None)?),
            None => None,
        };
        if let Some(source) = &link_source
            && root.shares_inode(&write.path, source)?
        {
            self.syncs.parent_of(&write.path);
            self.syncs.mark_staging();
            return Ok(());
        }
        let observed = root.inspect(&write.path)?;
        // Only restore immutable paths still owned by the pending composition.
        // Removals and edits to preserved or locally moved config remain strict.
        let before = if self.repair
            && observed != write.after
            && let Some(managed) = self.paths.get(&write.path)
            && !managed.preserve
            && write.after.as_ref() == Some(&managed.node)
            && observed.as_ref().is_none_or(|node| {
                node.is_dir() == managed.node.is_dir() && !matches!(node, Node::Special)
            }) {
            &observed
        } else {
            &write.before
        };
        if let (Some(before @ Node::Dir { .. }), Some(after @ Node::Dir { .. })) =
            (before, &write.after)
            && before != after
        {
            if root.repair_directory(&write.path, before, after)? {
                (self.after_write)(write);
            }
            return Ok(());
        }
        let detach = write.detach
            && observed == write.after
            && root.has_other_links(
                &write.path,
                self.groups
                    .get(write.path.as_str())
                    .map_or(&[], Vec::as_slice),
            )?;
        if observed == write.after && !detach {
            if write.before != write.after || write.detach {
                // An interrupted run may have published this path without
                // syncing its directory.
                self.syncs.parent_of(&write.path);
                self.syncs.mark_staging();
            }
            return Ok(());
        }
        // A path holding exactly the contents and metadata of its hard link
        // target can be linked to it without losing anything. An abort finds
        // aliases still linked to the target's old inode in this state once
        // it has restored the target.
        let same_as_target = write
            .hardlink_target()
            .and_then(|target| self.paths.get(target))
            .is_some_and(|target| observed.as_ref() == Some(&target.node));
        ensure!(
            &observed == before || same_as_target,
            "unexpected state at {}; pending transaction retained",
            write.path
        );
        if write.removes_directory() {
            if root.remove_empty_dir(&write.path, &mut self.syncs)? {
                (self.after_write)(write);
            }
            return Ok(());
        }
        match &write.after {
            Some(node) => {
                let payload = match (node, link_source) {
                    (Node::Hardlink { .. }, Some(source)) => {
                        Some(ReplacementSource::Staged(source))
                    }
                    (Node::Hardlink { .. }, None) => anyhow::bail!("missing hard link target"),
                    _ => self.source(write, None)?,
                };
                root.replace(
                    &write.path,
                    node,
                    payload,
                    self.stage.as_file(),
                    write.config_move.is_some(),
                    &mut self.syncs,
                )?;
            }
            None => root.remove_file_deferred(&write.path, &mut self.syncs)?,
        }
        (self.after_write)(write);
        Ok(())
    }

    /// Replace a directory with a file or symlink, or the reverse, together
    /// with every write beneath the path.
    ///
    /// The new object, including a complete new directory tree, is built
    /// under a private staging name and exchanged with the old object, so
    /// the path never goes missing on a filesystem that supports
    /// RENAME_EXCHANGE. Resume and abort recognize every intermediate state:
    ///
    /// - The path is in its old state: anything under the staging names is
    ///   an incomplete replacement, which is rebuilt.
    /// - The path is in its new state: the staging names hold the old
    ///   object, which is removed.
    /// - The path is absent after the fallback for filesystems without
    ///   RENAME_EXCHANGE moved the old object aside: whichever object the
    ///   write wants is renamed into place.
    fn transition(&mut self, write: &'a Write, children: Vec<&'a Write>) -> Result<()> {
        let (root, stage) = (self.root, self.stage);
        let path = write.path.as_str();
        let into_directory = write.transition() == Some(true);
        let id = &crate::model::digest(path.as_bytes())[..32];
        let (swap, old) = (format!("swap-{id}"), format!("old-{id}"));
        let observed = root.inspect(path)?;
        let mut changed = true;
        if observed == write.after {
            changed = false;
        } else if observed == write.before {
            if !into_directory {
                check_replaced_tree(root, write, &children, self.generated)?;
            }
            // Configuration copied out of the old tree and link targets of
            // the new one must be durable before the old object goes.
            root.sync_dirs(&mut self.syncs, Some(stage.as_file()))?;
            self.stage_replacement(write, &children, &swap)?;
            root::test_point("staged");
            root.exchange(path, stage.as_file(), &swap, &old)?;
        } else if observed.is_none() {
            let displaced = stage.inspect(&old)?;
            ensure!(
                displaced.is_some(),
                "unexpected state at {path}; pending transaction retained"
            );
            if displaced == write.before {
                self.stage_replacement(write, &children, &swap)?;
                root.publish_staged(path, stage.as_file(), &swap)?;
            } else if displaced == write.after {
                root.publish_staged(path, stage.as_file(), &old)?;
            } else {
                anyhow::bail!("unexpected state at {path}; pending transaction retained");
            }
        } else {
            anyhow::bail!("unexpected state at {path}; pending transaction retained");
        }
        for name in [&swap, &old] {
            if stage.remove_staged(name)? {
                self.syncs.mark_staging();
            }
        }
        if changed {
            (self.after_write)(write);
        }
        if into_directory {
            // The new tree is in place, so each write beneath it now
            // finds its new state, or an earlier one if the tree was
            // published before an interruption completed it.
            for child in children {
                self.write(child)?;
            }
        }
        Ok(())
    }

    /// Build the replacement object for a transition as the staging entry `name`.
    fn stage_replacement(
        &mut self,
        write: &'a Write,
        children: &[&'a Write],
        name: &str,
    ) -> Result<()> {
        let (root, stage) = (self.root, self.stage);
        let path = write.path.as_str();
        let node = write
            .after
            .as_ref()
            .context("transition has no new object")?;
        if write.transition() == Some(false) {
            let payload = self.source(write, None)?;
            root.stage_object(stage.as_file(), name, path, node, payload)?;
            return Ok(stage.as_file().sync_all()?);
        }
        root.stage_object(stage.as_file(), name, path, node, None)?;
        let tree = stage.subdir(name)?;
        let mut built: Vec<&Write> = Vec::new();
        for &child in children {
            ensure!(
                child.before.is_none(),
                "unexpected journaled state beneath {path}"
            );
            if child.after.is_some() {
                built.push(child);
            }
        }
        // Parents before their entries, and link targets before links.
        built.sort_by_cached_key(|child| {
            (
                child.hardlink_target().is_some(),
                child.path.bytes().filter(|b| *b == b'/').count(),
                child.path.clone(),
            )
        });
        let mut syncs = DirSyncs::default();
        for child in built {
            let relative = &child.path[path.len() + 1..];
            let payload = self.source(child, Some((&tree, path)))?;
            let node = child.after.as_ref().expect("built children have contents");
            root.replace_beneath(&tree, relative, node, payload, stage.as_file(), &mut syncs)?;
        }
        tree.sync_dirs(&mut syncs, None)?;
        tree.as_file().sync_all()?;
        Ok(stage.as_file().sync_all()?)
    }
}

/// Check that a directory about to be replaced holds only the journaled
/// paths beneath it, each still in its state before the transaction, and
/// generated trigger outputs, which leave with the old tree.
fn check_replaced_tree(
    root: &Root,
    write: &Write,
    children: &[&Write],
    generated: &[&str],
) -> Result<()> {
    let expected: BTreeMap<&str, &Write> = children
        .iter()
        .map(|child| (child.path.as_str(), *child))
        .collect();
    root.walk_beneath(&write.path, |path, _| {
        if !expected.contains_key(path) && triggers::generated(generated, path) {
            return Ok(false);
        }
        let child = expected.get(path).with_context(|| {
            format!(
                "unexpected path {path} beneath {}; pending transaction retained",
                write.path
            )
        })?;
        ensure!(
            child.after.is_none() && root.inspect(path)? == child.before,
            "unexpected state at {path}; pending transaction retained"
        );
        Ok(true)
    })
}

fn clear_stage(stage: &Root) -> Result<()> {
    for entry in fs::read_dir(Root::fd_path(stage.as_file()))? {
        let name = entry?.file_name();
        let name = name
            .to_str()
            .with_context(|| format!("non-UTF-8 staging entry {name:?}"))?;
        stage.remove_staged(name)?;
    }
    Ok(())
}

fn prepare(root: &Root, stage: &Root, plan: &Plan, state: &State) -> Result<StagedPayloads> {
    let writes = &plan.writes;
    let dir = Root::fd_path(stage.as_file());
    clear_stage(stage)?;
    for write in writes.iter().filter(|write| write.replaces_file()) {
        if let Some(node) = &write.after
            && (node.capabilities().is_some()
                || matches!(node, Node::File { mode, .. } if mode & 0o6000 != 0))
        {
            let probe = NamedTempFile::new_in(&dir)?;
            Root::set_file_metadata(probe.as_file(), node)
                .with_context(|| format!("checking file metadata support for {}", write.path))?;
        }
    }
    for write in writes {
        if let Some(node @ Node::Dir { mode, .. }) = &write.after
            && mode & 0o2000 != 0
        {
            let probe = tempfile::tempdir_in(&dir)?;
            Root::set_directory_metadata(&File::open(probe.path())?, node).with_context(|| {
                format!("checking directory metadata support for {}", write.path)
            })?;
        }
    }
    // copy_payload hashes each staged copy after writing it, so the staged
    // files are verified here and only fingerprinted afterwards.
    let mut staged = StagedPayloads::new();
    let mut persist = |file: NamedTempFile, hash: &str| -> Result<()> {
        file.as_file().sync_all()?;
        let file = file.persist_noclobber(dir.join(hash))?;
        staged.insert(hash.to_owned(), Fingerprint::of(&rustix::fs::fstat(&file)?));
        Ok(())
    };
    if let Some((path, node @ Node::File { sha256, .. })) = plan.config_payload() {
        let mut file = NamedTempFile::new_in(&dir)?;
        copy_payload(&root.file(&path)?, node, file.as_file_mut())
            .with_context(|| format!("staging vendor configuration {path}"))?;
        persist(file, sha256)?;
    } else {
        for (publisher, blobs) in payload_sources(writes, state)? {
            for (hash, payload) in blobs {
                let mut file = NamedTempFile::new_in(&dir)?;
                stage_blob(&publisher.repo, payload.node, file.as_file_mut())?;
                persist(file, hash)?;
            }
        }
    }
    stage.as_file().sync_all()?;
    Ok(staged)
}

fn payload_sources<'a>(
    writes: &'a [Write],
    state: &'a State,
) -> Result<Vec<PublisherPayloads<'a>>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut by_publisher = BTreeMap::<_, BTreeMap<_, _>>::new();
    let config_updates = config_update_prefix(state.settings.revision);
    for write in writes.iter().filter(|write| write.needs_blob()) {
        let Some(node @ Node::File { sha256, .. }) = &write.after else {
            unreachable!("only file replacements require a payload");
        };
        if !seen.insert(sha256) {
            continue;
        }
        let path = write
            .path
            .strip_prefix(&config_updates)
            .unwrap_or(&write.path);
        let name = state
            .paths
            .get(path)
            .and_then(|managed| managed.owners.first())
            .with_context(|| format!("file has no owning package: {}", write.path))?;
        let owner = state
            .installed
            .get(name)
            .context("file's owning package is absent")?;
        let publisher = owner.published.manifest.publisher.as_str();
        by_publisher.entry(publisher).or_default().insert(
            sha256.as_str(),
            OwnedPayload {
                node,
                package: &owner.published,
                base: match &write.before {
                    Some(Node::File { sha256, size, .. }) if path == write.path => {
                        Some(DeltaBase {
                            path,
                            hash: sha256,
                            length: *size,
                        })
                    }
                    _ => None,
                },
            },
        );
    }
    by_publisher
        .into_iter()
        .map(|(name, blobs)| {
            let publisher = state
                .publishers
                .get(name)
                .with_context(|| format!("publisher {name} is not configured"))?;
            Ok((publisher, blobs))
        })
        .collect()
}

fn checked_payload(stage: &Root, node: &Node) -> Result<File> {
    let Node::File { sha256, size, .. } = node else {
        anyhow::bail!("expected a file payload");
    };
    let mut file = stage
        .file(sha256)
        .with_context(|| format!("opening staged blob {sha256}"))?;
    ensure!(
        file.metadata()?.len() == *size && digest_reader(&mut file)? == *sha256,
        "staged blob {sha256} is corrupt"
    );
    file.rewind()?;
    Ok(file)
}
