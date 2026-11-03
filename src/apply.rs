use std::{
    fs::{self, File},
    io::{Seek, Write as _},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::{
    image::{Image, State, root::Root},
    model::{Node, digest_reader},
    plan::{Plan, Write, build, payloads},
    repo::read_blob,
};

#[derive(Deserialize, Serialize)]
struct Pending {
    id: String,
    writes: Vec<Write>,
    next: State,
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
    let pending = if let Some(pending) = image.pending::<Pending>()? {
        ensure!(
            pending.id == id,
            "a different plan is pending; rerun apply with that plan"
        );
        pending
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
        let (checked, next) = build(
            &image.root,
            state,
            plan.request.clone(),
            Some(&plan.catalog),
        )?;
        ensure!(
            &checked == plan,
            "saved plan differs from the verified plan; generate a new plan"
        );
        prepare(&stage, &checked.writes, &next.publisher.repo)?;
        let pending = Pending {
            id,
            writes: checked.writes,
            next,
        };
        image.record_pending(&pending)?;
        pending
    };

    for node in payloads(&pending.writes).into_values() {
        checked_payload(&image.root, node)?;
    }
    for write in &pending.writes {
        let observed = image.root.inspect(&write.path)?;
        if observed == write.after {
            if write.before != write.after {
                image.root.sync_parent(&write.path)?;
                stage.sync_all()?;
            }
            continue;
        }
        ensure!(
            observed == write.before,
            "unexpected state at {}; pending transaction retained",
            write.path
        );
        match &write.after {
            Some(node) => {
                let payload = if matches!(node, Node::File { .. }) {
                    Some(checked_payload(&image.root, node)?)
                } else {
                    None
                };
                image.root.replace(&write.path, node, payload, &stage)?;
            }
            None => image.root.remove_file(&write.path)?,
        }
        after_write(write);
    }
    let revision = pending.next.settings.revision;
    image.commit(&pending.id, &pending.next)?;
    Ok(revision)
}

fn prepare(stage: &File, writes: &[Write], repo: &Path) -> Result<()> {
    let dir = Root::fd_path(stage);
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    for (hash, node) in payloads(writes) {
        let mut file = NamedTempFile::new_in(&dir)?;
        file.write_all(&read_blob(repo, node)?)?;
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
