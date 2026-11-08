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
        Image, Managed, Publisher, Settings, State,
        root::{ReplacementSource, Root},
    },
    model::{Node, copy_payload, digest_reader, validate_digest},
    plan::{
        Plan, Request, Write, build, config_update_prefix, hardlink_groups, payloads,
        pending_repairs,
    },
    repo::{
        BuildReceipt, Published,
        release::{ArchiveCache, DeltaBase, fetch_payloads},
        stage_blob,
    },
    systemd::{self, Action as SystemdAction},
    verify::{manifest_paths, verify_state, verify_unchanged},
};

#[derive(Deserialize, Serialize)]
struct Journal {
    id: String,
    plan: Plan,
    next: State,
    state_sha256: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    files_ready: bool,
}

#[derive(Deserialize, Serialize)]
struct Pending {
    id: String,
    writes: Vec<Write>,
    next: State,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    systemd: Vec<SystemdAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kernel_install: Option<Artifacts>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    files_ready: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum PendingRecord {
    Live { live: Box<Journal> },
    Journaled { journaled: Box<Journal> },
    Dormant { dormant: Pending },
    Archived { archived: Pending },
    Profiled { profiled: Pending },
    Checkpointed { checkpointed: Pending },
    Legacy(Pending),
}

struct PendingWork<'a> {
    id: &'a str,
    writes: &'a [Write],
    next: &'a State,
    systemd: &'a [SystemdAction],
    kernel_install: Option<&'a Artifacts>,
    files_ready: bool,
}

impl PendingRecord {
    fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }

    fn work(&self) -> PendingWork<'_> {
        match self {
            Self::Journaled { journaled } | Self::Live { live: journaled } => PendingWork {
                id: &journaled.id,
                writes: &journaled.plan.writes,
                next: &journaled.next,
                systemd: &journaled.plan.systemd,
                kernel_install: journaled.plan.kernel_install.as_ref(),
                files_ready: journaled.files_ready,
            },
            Self::Dormant { dormant: pending }
            | Self::Archived { archived: pending }
            | Self::Profiled { profiled: pending }
            | Self::Checkpointed {
                checkpointed: pending,
            }
            | Self::Legacy(pending) => PendingWork {
                id: &pending.id,
                writes: &pending.writes,
                next: &pending.next,
                systemd: &pending.systemd,
                kernel_install: pending.kernel_install.as_ref(),
                files_ready: pending.files_ready,
            },
        }
    }

    fn mark_files_ready(&mut self) {
        *match self {
            Self::Journaled { journaled } | Self::Live { live: journaled } => {
                &mut journaled.files_ready
            }
            Self::Dormant { dormant: pending }
            | Self::Archived { archived: pending }
            | Self::Profiled { profiled: pending }
            | Self::Checkpointed {
                checkpointed: pending,
            }
            | Self::Legacy(pending) => &mut pending.files_ready,
        } = true;
    }

    fn recorded_plan(&self) -> Result<Option<&Plan>> {
        let journaled = match self {
            Self::Journaled { journaled } | Self::Live { live: journaled } => journaled,
            _ => return Ok(None),
        };
        ensure!(
            journaled.plan.id()? == journaled.id,
            "journaled plan digest mismatch"
        );
        ensure!(
            state_digest(&journaled.next)? == journaled.state_sha256,
            "journaled image state digest mismatch"
        );
        Ok(Some(&journaled.plan))
    }
}

fn state_digest(state: &State) -> Result<String> {
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
    pub plan_summary: Option<String>,
    pub live: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    Planned,
    PayloadValidated,
    Blocked,
    Applying,
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
            let plan_summary = pending.recorded_plan()?.map(Plan::summary);
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
                if work.id == plan_id {
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
    let state = image.state()?;
    ensure!(
        plan.image == state.settings.id,
        "plan targets a different image"
    );
    if image.applied(&plan.id()?)?.is_some() {
        return Ok(0);
    }
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
        let pin = if state.publishers.as_slice().len() == 1 {
            plan.catalog.as_deref()
        } else {
            plan.catalogs.get(&publisher.name).map(String::as_str)
        };
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
            .with_context(|| format!("preparing payloads from {}", publisher.name))?;
    }
    Ok(count)
}

fn check_plan(
    image: &Image,
    state: State,
    plan: &Plan,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    check_plan_revision(&state.settings, plan)?;
    let (checked, next, receipts) =
        build(image, state, plan.request.clone(), Some(plan), plan.solver)?;
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
    let previous = elf::applies(plan, &state).then(|| state.paths.clone());
    let (checked, next, _) = check_plan(image, state, plan)?;
    check_drift(&image.root, &checked, &next)?;
    let parent = image.stage()?;
    let scratch = tempfile::tempdir_in(Root::fd_path(parent.as_file()))?;
    let stage = Root::open(scratch.path())?;
    prepare(&image.root, &stage, &checked, &next)?;
    if let Some(previous) = &previous {
        elf::check(&image.root, stage.as_file(), &checked, previous, &next)?;
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
    let state = image.state()?;
    ensure!(
        plan.image == state.settings.id,
        "plan targets a different image"
    );
    if let Some(pending) = image.pending::<PendingRecord>()? {
        ensure!(
            pending.work().id == id,
            "a different plan is pending; use transaction status to identify it"
        );
        return resume_pending(
            &mut image,
            &state,
            pending,
            Some(plan),
            live.is_some(),
            None,
            after_write,
        );
    }
    if let Some(revision) = image.applied(&id)? {
        discard_stage(&image);
        return Ok(revision);
    }
    let check_elf = elf::applies(plan, &state);
    let previous_paths = (check_elf && live.is_none()).then(|| state.paths.clone());
    let current = live.map(|_| state.clone());
    let (checked, mut next, receipts) = check_plan(&image, state, plan)?;
    check_drift(&image.root, &checked, &next)?;
    let stage = image.stage()?;
    let after = if let (Some(stage_packages), Some(current)) = (live, &current) {
        check_live_plan(&checked, stage_packages)?;
        next.settings.boot_revision = Some(current.settings.boot_revision());
        next.settings.format = next.publishers.image_format(&next.settings);
        AfterFiles::Live(systemd::LiveServices::plan(&checked, current, &next))
    } else {
        AfterFiles::offline(&image.root, &checked)?
    };
    prepare(&image.root, &stage, &checked, &next)?;
    if check_elf {
        let previous = previous_paths
            .as_ref()
            .or_else(|| current.as_ref().map(|state| &state.paths))
            .expect("ELF checking retained the previous paths");
        elf::check(&image.root, stage.as_file(), &checked, previous, &next)?;
    }
    let repo_backed = checked.catalog.is_some() || !checked.catalogs.is_empty();
    let journal = Box::new(Journal {
        id,
        plan: checked,
        state_sha256: state_digest(&next)?,
        next,
        files_ready: false,
    });
    let pending = if live.is_some() {
        PendingRecord::Live { live: journal }
    } else {
        PendingRecord::Journaled { journaled: journal }
    };
    image.record_pending(&pending, pending.work().next, &receipts, repo_backed)?;
    finish(&mut image, &stage, pending, None, after, None, after_write)
}

fn check_drift(root: &Root, plan: &Plan, next: &State) -> Result<()> {
    if plan.is_recovery() || matches!(plan.request, Request::ResolveConfig { .. }) {
        return Ok(());
    }
    let changed = plan
        .writes
        .iter()
        .filter(|write| write.before != write.after || write.detach)
        .map(|write| write.path.as_str())
        .collect();
    for finding in verify_unchanged(root, next, &changed)? {
        ensure!(
            !finding.is_error,
            "pre-existing drift at {}; repair the image before applying the transaction",
            finding.message
        );
    }
    Ok(())
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
    resume_pending(&mut image, &state, pending, None, live, repair, |_| {}).map(Some)
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
    pending: PendingRecord,
    saved: Option<&Plan>,
    live: bool,
    repair: Option<&BTreeSet<String>>,
    after_write: impl FnMut(&Write),
) -> Result<u64> {
    ensure!(
        pending.is_live() == live,
        "resume the transaction in its original mode"
    );
    let work = pending.work();
    let recorded = pending.recorded_plan()?;
    let plan = recorded
        .or(saved)
        .context("this older transaction requires its saved plan; rerun apply with that plan")?;
    validate_digest(work.id)?;
    plan.check_recovery_state(state, work.next)?;
    let expected_boot_revision = if live {
        Some(state.settings.boot_revision())
    } else {
        state.settings.boot_revision
    };
    ensure!(
        work.next.settings.boot_revision == expected_boot_revision,
        "pending state has a different boot revision"
    );
    ensure!(
        recorded.is_some()
            || (work.writes == plan.writes
                && work.systemd == plan.systemd
                && work.kernel_install == plan.kernel_install.as_ref()),
        "pending actions differ from the verified plan"
    );
    ensure!(
        manifest_paths(work.next)? == work.next.paths,
        "pending ownership index differs from its installed manifests"
    );
    image
        .verify_catalog_archives(work.next)
        .context("verifying catalog archives for the pending transaction")?;
    let after = if live {
        check_live_plan(plan, &BTreeSet::new())?;
        AfterFiles::Live(systemd::LiveServices::plan(plan, state, work.next))
    } else {
        AfterFiles::offline(&image.root, plan)?
    };
    ensure!(
        !work.files_ready || !matches!(after, AfterFiles::None),
        "pending transaction has an invalid final phase"
    );
    let stage = image.stage()?;
    finish(image, &stage, pending, saved, after, repair, after_write)
}

enum AfterFiles {
    None,
    Offline(PathBuf),
    Live(systemd::LiveServices),
}

impl AfterFiles {
    fn offline(root: &Root, plan: &Plan) -> Result<Self> {
        if plan.systemd.is_empty() && plan.kernel_install.is_none() {
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
    saved: Option<&Plan>,
    after: AfterFiles,
    repair: Option<&BTreeSet<String>>,
    mut after_write: impl FnMut(&Write),
) -> Result<u64> {
    let work = pending.work();
    if !work.files_ready {
        write_files(
            &image.root,
            stage,
            work.writes,
            &work.next.paths,
            &mut after_write,
        )?;
        if !matches!(after, AfterFiles::None) {
            pending.mark_files_ready();
            image.update_pending(&pending)?;
        }
    }
    let work = pending.work();
    if let Some(stage_packages) = repair {
        restore_pending_paths(
            &image.root,
            stage,
            work.next,
            pending.is_live(),
            stage_packages,
        )?;
    }
    if !matches!(after, AfterFiles::None) {
        let path = match &after {
            AfterFiles::Offline(path) => path.as_path(),
            AfterFiles::Live(_) => Path::new("/"),
            AfterFiles::None => unreachable!(),
        };
        if !work.systemd.is_empty() {
            systemd::run(
                &image.root,
                path,
                work.systemd,
                work.next,
                pending.is_live(),
            )?;
        }
        if let Some(kernel) = work.kernel_install {
            kernel.prepare(image, path, &work.next.settings)?;
        }
        let phase = if pending.is_live() { "live" } else { "offline" };
        let repaired = matches!(&pending, PendingRecord::Live { live } if live.plan.is_recovery())
            .then(|| {
                work.writes
                    .iter()
                    .map(|write| write.path.as_str())
                    .collect()
            });
        for finding in verify_state(&image.root, work.next, repaired.as_ref())? {
            ensure!(
                !finding.is_error,
                "{phase} actions left an inconsistent image: {}; try resuming with --repair",
                finding.message
            );
        }
        if let AfterFiles::Live(services) = after {
            services.run()?;
        }
    }
    let revision = work.next.settings.revision;
    let plan = match &pending {
        PendingRecord::Journaled { journaled } | PendingRecord::Live { live: journaled } => {
            &journaled.plan
        }
        _ => saved.context("missing verified plan for the pending transaction")?,
    };
    image.commit(work.id, work.next, &plan.transaction_details(work.next))?;
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
) -> Result<()> {
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
    let dir = Root::fd_path(stage.as_file());
    for (publisher, blobs) in payload_sources(&writes, next)? {
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
    stage.as_file().sync_all()?;
    write_files(root, stage, &writes, &next.paths, &mut |_| {})
}

fn write_files(
    root: &Root,
    stage: &Root,
    writes: &[Write],
    paths: &BTreeMap<String, Managed>,
    after_write: &mut impl FnMut(&Write),
) -> Result<()> {
    for node in payloads(writes).into_values() {
        checked_payload(stage, node)?;
    }
    let mut checked_target: Option<(&str, File)> = None;
    let groups = if writes.iter().any(|write| write.detach) {
        hardlink_groups(paths)
    } else {
        BTreeMap::new()
    };
    for write in writes {
        if let (Some(before @ Node::Dir { .. }), Some(after @ Node::Dir { .. })) =
            (&write.before, &write.after)
            && before != after
        {
            if root.repair_directory(&write.path, before, after)? {
                after_write(write);
            }
            continue;
        }
        let link_source = if let Some(target) = write.hardlink_target() {
            if checked_target
                .as_ref()
                .is_none_or(|(name, _)| *name != target)
            {
                let managed = paths
                    .get(target)
                    .with_context(|| format!("missing hard link target {target}"))?;
                checked_target =
                    Some((target, root.checked_hardlink_target(target, &managed.node)?));
            }
            checked_target.as_ref().map(|(_, file)| file)
        } else {
            None
        };
        if let Some(source) = link_source
            && root.shares_inode(&write.path, source)?
        {
            root.sync_parent(&write.path)?;
            stage.as_file().sync_all()?;
            continue;
        }
        let observed = root.inspect(&write.path)?;
        let detach = write.detach
            && observed == write.after
            && root.has_other_links(
                &write.path,
                groups.get(write.path.as_str()).map_or(&[], Vec::as_slice),
            )?;
        if observed == write.after && !detach {
            if write.before != write.after || write.detach {
                root.sync_parent(&write.path)?;
                stage.as_file().sync_all()?;
            }
            continue;
        }
        ensure!(
            observed == write.before,
            "unexpected state at {}; pending transaction retained",
            write.path
        );
        if write.removes_directory() {
            if root.remove_empty_dir(&write.path)? {
                after_write(write);
            }
            continue;
        }
        match &write.after {
            Some(node) => {
                let payload = match node {
                    Node::File { .. } => Some(match write.local_config_source() {
                        Some((path, before)) => ReplacementSource::LocalConfig { path, before },
                        None => ReplacementSource::Staged(checked_payload(stage, node)?),
                    }),
                    Node::Hardlink { .. } => Some(ReplacementSource::Staged(
                        link_source
                            .context("missing hard link target")?
                            .try_clone()?,
                    )),
                    _ => None,
                };
                root.replace(
                    &write.path,
                    node,
                    payload,
                    stage.as_file(),
                    write.config_move.is_some(),
                )?;
            }
            None => root.remove_file(&write.path)?,
        }
        after_write(write);
    }
    Ok(())
}

fn clear_stage(stage: &Root) -> Result<()> {
    for entry in fs::read_dir(Root::fd_path(stage.as_file()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn prepare(root: &Root, stage: &Root, plan: &Plan, state: &State) -> Result<()> {
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
    if let Some((path, node @ Node::File { sha256, .. })) =
        plan.config_payload(state.settings.state_layout)
    {
        let mut file = NamedTempFile::new_in(&dir)?;
        copy_payload(&root.file(&path)?, node, file.as_file_mut())
            .with_context(|| format!("staging vendor configuration {path}"))?;
        file.as_file().sync_all()?;
        file.persist_noclobber(dir.join(sha256))?;
    } else {
        for (publisher, blobs) in payload_sources(writes, state)? {
            for (hash, payload) in blobs {
                let mut file = NamedTempFile::new_in(&dir)?;
                stage_blob(&publisher.repo, payload.node, file.as_file_mut())?;
                file.as_file().sync_all()?;
                file.persist_noclobber(dir.join(hash))?;
            }
        }
    }
    stage.as_file().sync_all()?;
    Ok(())
}

fn payload_sources<'a>(
    writes: &'a [Write],
    state: &'a State,
) -> Result<Vec<PublisherPayloads<'a>>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut by_publisher = BTreeMap::<_, BTreeMap<_, _>>::new();
    let config_updates = config_update_prefix(state.settings.state_layout, state.settings.revision);
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
