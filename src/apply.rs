use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Seek,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{
    boot::kernel::Artifacts,
    image::{
        Image, Managed, Publisher, State,
        root::{ReplacementSource, Root},
    },
    model::{Node, copy_payload, digest_reader, validate_digest},
    plan::{Plan, Write, build, config_update_prefix, hardlink_groups, payloads},
    repo::{BuildReceipt, release::fetch_payloads, stage_blob},
    systemd::{self, Action as SystemdAction},
    verify::{manifest_paths, verify_state},
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
    fn work(&self) -> PendingWork<'_> {
        match self {
            Self::Journaled { journaled } => PendingWork {
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
            Self::Journaled { journaled } => &mut journaled.files_ready,
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
        let Self::Journaled { journaled } = self else {
            return Ok(None);
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
            })
        })
        .transpose()?;
    Ok(TransactionStatus {
        revision: settings.revision,
        pending,
    })
}

type PublisherPayloads<'a> = (&'a Publisher, BTreeMap<&'a str, &'a Node>);

pub fn apply(path: &Path, plan: &Plan) -> Result<u64> {
    apply_with(path, plan, |_| {})
}

pub fn fetch_plan(path: &Path, plan: &Plan) -> Result<usize> {
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
    let (checked, next, _) = check_plan(&image, state, plan)?;
    if checked.config_update_revision.is_some() {
        return Ok(0);
    }
    let mut count = 0;
    for (publisher, blobs) in payload_sources(&checked.writes, &next)? {
        let blobs = blobs
            .into_iter()
            .map(|(hash, node)| {
                let Node::File { size, .. } = node else {
                    unreachable!("payloads contain only files");
                };
                (hash, *size)
            })
            .collect::<BTreeMap<_, _>>();
        count += blobs.len();
        fetch_payloads(publisher, blobs)
            .with_context(|| format!("preparing payloads from {}", publisher.name))?;
    }
    Ok(count)
}

fn check_plan(
    image: &Image,
    state: State,
    plan: &Plan,
) -> Result<(Plan, State, Vec<BuildReceipt>)> {
    ensure!(
        plan.base_revision == state.settings.revision,
        "stale plan for revision {}; image is at revision {}",
        plan.base_revision,
        state.settings.revision
    );
    plan.check_publisher_generation(&state.settings)?;
    let (checked, next, receipts) =
        build(image, state, plan.request.clone(), Some(plan), plan.solver)?;
    ensure!(
        &checked == plan,
        "saved plan differs from the verified plan; generate a new plan"
    );
    Ok((checked, next, receipts))
}

pub(crate) fn apply_with(path: &Path, plan: &Plan, after_write: impl FnMut(&Write)) -> Result<u64> {
    let id = plan.id()?;
    let mut image = Image::open_for_apply(path)?;
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
        return resume_pending(&mut image, &state, pending, Some(plan), after_write);
    }
    if let Some(revision) = image.applied(&id)? {
        return Ok(revision);
    }
    let (checked, next, receipts) = check_plan(&image, state, plan)?;
    let stage = image.stage()?;
    let offline = offline_target(
        &image.root,
        !checked.systemd.is_empty() || checked.kernel_install.is_some(),
    )?;
    prepare(&image.root, &stage, &checked, &next)?;
    let repo_backed = checked.catalog.is_some() || !checked.catalogs.is_empty();
    let pending = PendingRecord::Journaled {
        journaled: Box::new(Journal {
            id,
            plan: checked,
            state_sha256: state_digest(&next)?,
            next,
            files_ready: false,
        }),
    };
    image.record_pending(&pending, pending.work().next, &receipts, repo_backed)?;
    finish(&mut image, &stage, pending, offline, after_write)
}

pub fn resume_transaction(path: &Path) -> Result<Option<u64>> {
    let mut image = Image::open_for_apply(path)?;
    let state = image.state()?;
    let Some(pending) = image.pending::<PendingRecord>()? else {
        return Ok(None);
    };
    resume_pending(&mut image, &state, pending, None, |_| {}).map(Some)
}

fn resume_pending(
    image: &mut Image,
    state: &State,
    pending: PendingRecord,
    saved: Option<&Plan>,
    after_write: impl FnMut(&Write),
) -> Result<u64> {
    let work = pending.work();
    let recorded = pending.recorded_plan()?;
    let plan = recorded
        .or(saved)
        .context("this older transaction requires its saved plan; rerun apply with that plan")?;
    validate_digest(work.id)?;
    plan.check_recovery_state(state, work.next)?;
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
    let offline = offline_target(
        &image.root,
        !work.systemd.is_empty() || work.kernel_install.is_some(),
    )?;
    ensure!(
        !work.files_ready || offline.is_some(),
        "pending transaction has an invalid offline phase"
    );
    let stage = image.stage()?;
    finish(image, &stage, pending, offline, after_write)
}

fn finish(
    image: &mut Image,
    stage: &File,
    mut pending: PendingRecord,
    offline: Option<std::path::PathBuf>,
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
        if offline.is_some() {
            pending.mark_files_ready();
            image.update_pending(&pending)?;
        }
    }
    let work = pending.work();
    if let Some(path) = offline {
        if !work.systemd.is_empty() {
            systemd::run(&image.root, &path, work.systemd, work.next)?;
        }
        if let Some(kernel) = work.kernel_install {
            kernel.prepare(&image.root, &path, &work.next.settings.id)?;
        }
        for finding in verify_state(&image.root, work.next)? {
            ensure!(
                !finding.is_error,
                "offline actions left an inconsistent image: {}",
                finding.message
            );
        }
    }
    let revision = work.next.settings.revision;
    image.commit(work.id, work.next)?;
    Ok(revision)
}

fn offline_target(root: &Root, needed: bool) -> Result<Option<std::path::PathBuf>> {
    needed.then(|| systemd::offline_path(root)).transpose()
}

fn write_files(
    root: &Root,
    stage: &File,
    writes: &[Write],
    paths: &BTreeMap<String, Managed>,
    after_write: &mut impl FnMut(&Write),
) -> Result<()> {
    for node in payloads(writes).into_values() {
        checked_payload(root, node)?;
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
            stage.sync_all()?;
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
                stage.sync_all()?;
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
                        None => ReplacementSource::Staged(checked_payload(root, node)?),
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
                    stage,
                    write.config_move.is_some(),
                )?;
            }
            None => root.remove_file(&write.path)?,
        }
        after_write(write);
    }
    Ok(())
}

fn prepare(root: &Root, stage: &File, plan: &Plan, state: &State) -> Result<()> {
    let writes = &plan.writes;
    let dir = Root::fd_path(stage);
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    for write in writes.iter().filter(|write| write.replaces_file()) {
        if let Some(node) = &write.after
            && node.capabilities().is_some()
        {
            let probe = NamedTempFile::new_in(&dir)?;
            Root::set_file_metadata(probe.as_file(), node)
                .with_context(|| format!("checking file capability support for {}", write.path))?;
        }
    }
    if let Some((path, node @ Node::File { sha256, .. })) = plan.config_payload() {
        let mut file = NamedTempFile::new_in(&dir)?;
        copy_payload(&root.file(&path)?, node, file.as_file_mut())
            .with_context(|| format!("staging vendor configuration {path}"))?;
        file.as_file().sync_all()?;
        file.persist_noclobber(dir.join(sha256))?;
    } else {
        for (publisher, blobs) in payload_sources(writes, state)? {
            for (hash, node) in blobs {
                let mut file = NamedTempFile::new_in(&dir)?;
                stage_blob(&publisher.repo, node, file.as_file_mut())?;
                file.as_file().sync_all()?;
                file.persist_noclobber(dir.join(hash))?;
            }
        }
    }
    stage.sync_all()?;
    Ok(())
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
        by_publisher
            .entry(publisher)
            .or_default()
            .insert(sha256.as_str(), node);
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

fn checked_payload(root: &Root, node: &Node) -> Result<File> {
    let Node::File { sha256, size, .. } = node else {
        anyhow::bail!("expected a file payload");
    };
    let path = format!(".cps/stage/{sha256}");
    let mut file = root
        .file(&path)
        .with_context(|| format!("opening staged blob {sha256}"))?;
    ensure!(
        file.metadata()?.len() == *size && digest_reader(&mut file)? == *sha256,
        "staged blob {sha256} is corrupt"
    );
    file.rewind()?;
    Ok(file)
}
