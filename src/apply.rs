use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Seek,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    boot::kernel::Artifacts,
    image::{
        Image, Managed, Publisher, State,
        root::{ReplacementSource, Root},
    },
    model::{Node, copy_payload, digest_reader},
    plan::{Plan, Write, build, config_update_prefix, hardlink_groups, payloads},
    repo::{BuildReceipt, release::fetch_payloads, stage_blob},
    systemd::{self, Action as SystemdAction},
    verify::verify_state,
};

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
enum PendingRecord<P> {
    Dormant { dormant: P },
    Archived { archived: P },
    Profiled { profiled: P },
    Checkpointed { checkpointed: P },
    Legacy(P),
}

impl Pending {
    fn record(&self) -> PendingRecord<&Self> {
        if !self.next.settings.dormant_configs.is_empty() {
            PendingRecord::Dormant { dormant: self }
        } else if self.next.settings.catalog_archives {
            PendingRecord::Archived { archived: self }
        } else if !self.next.settings.profile_packages.is_empty() {
            PendingRecord::Profiled { profiled: self }
        } else if self.next.settings.catalog_checkpoints.is_empty() {
            PendingRecord::Legacy(self)
        } else {
            // Older clients must fail before resuming a transaction that
            // would commit an image with checkpoints they cannot preserve.
            PendingRecord::Checkpointed { checkpointed: self }
        }
    }
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

pub(crate) fn apply_with(
    path: &Path,
    plan: &Plan,
    mut after_write: impl FnMut(&Write),
) -> Result<u64> {
    let id = plan.id()?;
    let mut image = Image::open_for_apply(path)?;
    let state = image.state()?;
    ensure!(
        plan.image == state.settings.id,
        "plan targets a different image"
    );
    let stage = image.stage()?;
    let (mut pending, offline) = if let Some(record) = image.pending::<PendingRecord<Pending>>()? {
        let pending = match record {
            PendingRecord::Dormant { dormant } => dormant,
            PendingRecord::Archived { archived } => archived,
            PendingRecord::Profiled { profiled } => profiled,
            PendingRecord::Checkpointed { checkpointed } => checkpointed,
            PendingRecord::Legacy(pending) => pending,
        };
        ensure!(
            pending.id == id,
            "a different plan is pending; rerun apply with that plan"
        );
        image
            .verify_catalog_archives(&pending.next)
            .context("verifying catalog archives for the pending transaction")?;
        let offline = offline_target(
            &image.root,
            !pending.systemd.is_empty() || pending.kernel_install.is_some(),
        )?;
        (pending, offline)
    } else {
        if let Some(revision) = image.applied(&id)? {
            return Ok(revision);
        }
        let (checked, next, receipts) = check_plan(&image, state, plan)?;
        let offline = offline_target(
            &image.root,
            !checked.systemd.is_empty() || checked.kernel_install.is_some(),
        )?;
        prepare(&image.root, &stage, &checked, &next)?;
        let repo_backed = checked.catalog.is_some() || !checked.catalogs.is_empty();
        let pending = Pending {
            id,
            writes: checked.writes,
            next,
            systemd: checked.systemd,
            kernel_install: checked.kernel_install,
            files_ready: false,
        };
        image.record_pending(&pending.record(), &pending.next, &receipts, repo_backed)?;
        (pending, offline)
    };

    if !pending.files_ready {
        write_files(
            &image.root,
            &stage,
            &pending.writes,
            &pending.next.paths,
            &mut after_write,
        )?;
        if offline.is_some() {
            pending.files_ready = true;
            image.update_pending(&pending.record())?;
        }
    }
    if let Some(path) = offline {
        if !pending.systemd.is_empty() {
            systemd::run(&image.root, &path, &pending.systemd, &pending.next)?;
        }
        if let Some(kernel) = &pending.kernel_install {
            kernel.prepare(&image.root, &path, &pending.next.settings.id)?;
        }
        for finding in verify_state(&image.root, &pending.next)? {
            ensure!(
                !finding.is_error,
                "offline actions left an inconsistent image: {}",
                finding.message
            );
        }
    }
    let revision = pending.next.settings.revision;
    image.commit(&pending.id, &pending.next)?;
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
