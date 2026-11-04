use std::{
    fs::{self, File},
    io::Seek,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    boot::kernel::Artifacts,
    image::{Image, State, root::Root},
    model::{Node, digest_reader},
    plan::{Plan, Write, build, payloads},
    repo::stage_blob,
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

pub fn apply(path: &Path, plan: &Plan) -> Result<u64> {
    apply_with(path, plan, |_| {})
}

pub(crate) fn apply_with(
    path: &Path,
    plan: &Plan,
    mut after_write: impl FnMut(&Write),
) -> Result<u64> {
    let id = plan.id()?;
    let mut image = Image::open(path, true)?;
    let state = image.state()?;
    ensure!(
        plan.image == state.settings.id,
        "plan targets a different image"
    );
    let stage = image.stage()?;
    let (mut pending, offline) = if let Some(pending) = image.pending::<Pending>()? {
        ensure!(
            pending.id == id,
            "a different plan is pending; rerun apply with that plan"
        );
        let offline = offline_target(
            &image.root,
            !pending.systemd.is_empty() || pending.kernel_install.is_some(),
        )?;
        (pending, offline)
    } else {
        if let Some(revision) = image.applied(&id)? {
            return Ok(revision);
        }
        ensure!(
            plan.base_revision == state.settings.revision,
            "stale plan for revision {}; image is at revision {}",
            plan.base_revision,
            state.settings.revision
        );
        plan.check_publisher_generation(&state.settings)?;
        let (checked, next) = build(
            &image.root,
            state,
            plan.request.clone(),
            Some(plan),
            plan.solver,
        )?;
        ensure!(
            &checked == plan,
            "saved plan differs from the verified plan; generate a new plan"
        );
        let offline = offline_target(
            &image.root,
            !checked.systemd.is_empty() || checked.kernel_install.is_some(),
        )?;
        prepare(&stage, &checked.writes, &next)?;
        let pending = Pending {
            id,
            writes: checked.writes,
            next,
            systemd: checked.systemd,
            kernel_install: checked.kernel_install,
            files_ready: false,
        };
        image.record_pending(&pending)?;
        (pending, offline)
    };

    if !pending.files_ready {
        write_files(&image.root, &stage, &pending.writes, &mut after_write)?;
        if offline.is_some() {
            pending.files_ready = true;
            image.update_pending(&pending)?;
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
    after_write: &mut impl FnMut(&Write),
) -> Result<()> {
    for node in payloads(writes).into_values() {
        checked_payload(root, node)?;
    }
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
        let observed = root.inspect(&write.path)?;
        if observed == write.after {
            if write.before != write.after {
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
                let payload = if matches!(node, Node::File { .. }) {
                    Some(checked_payload(root, node)?)
                } else {
                    None
                };
                root.replace(&write.path, node, payload, stage)?;
            }
            None => root.remove_file(&write.path)?,
        }
        after_write(write);
    }
    Ok(())
}

fn prepare(stage: &File, writes: &[Write], state: &State) -> Result<()> {
    let dir = Root::fd_path(stage);
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    let payloads = payloads(writes);
    let mut sources = std::collections::BTreeMap::new();
    for managed in state.paths.values() {
        if let Node::File { sha256, .. } = &managed.node
            && payloads.contains_key(sha256.as_str())
        {
            let name = managed
                .owners
                .first()
                .context("file has no owning package")?;
            let owner = state
                .installed
                .get(name)
                .context("file's owning package is absent")?;
            sources
                .entry(sha256.as_str())
                .or_insert(&owner.published.manifest.publisher);
        }
    }
    for (hash, node) in payloads {
        let name = sources
            .get(hash)
            .with_context(|| format!("no owning publisher for payload {hash}"))?;
        let publisher = state
            .publishers
            .get(name)
            .with_context(|| format!("publisher {name} is not configured"))?;
        let mut file = NamedTempFile::new_in(&dir)?;
        stage_blob(&publisher.repo, node, file.as_file_mut())?;
        file.as_file().sync_all()?;
        file.persist_noclobber(dir.join(hash))?;
    }
    stage.sync_all()?;
    Ok(())
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
