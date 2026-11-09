//! Retention of Btrfs boot environments, built on retirement records.

use std::{collections::BTreeSet, fs::File, path::Path};

use anyhow::{Context, Result, ensure};
use rustix::fs::{CWD, FlockOperation, RenameFlags, renameat_with};

use super::{
    Inventory, NO_SELECTION, destroy, destroy_btrfs_boot_environment, inventory, lock_esp,
    policy::{BootEntry, LoaderPolicy},
    rollback,
};
use crate::boot::btrfs_boot::BtrfsVolume;

/// Why prune removes an environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootRemoval {
    /// An interrupted destruction left a retirement record.
    Retiring,
    /// The trial used up its boot attempts or was cancelled.
    FailedTrial,
    /// The trial has attempts left but is no longer selected for any boot.
    AbandonedTrial,
    /// A known-good environment older than the retained ones.
    KnownGood,
}

#[derive(Debug, Default)]
pub struct BootPruneReport {
    /// Environments removed, or that a dry run would remove, oldest first.
    pub removed: Vec<(String, BootRemoval)>,
    /// Environments that could not be removed, with the reason.
    pub failed: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct TrialDiscard {
    /// The pending or running trial whose selection was cancelled.
    pub cancelled: Option<String>,
    /// The cancelled trial is the running system and stays until reboot.
    pub running: bool,
    pub prune: BootPruneReport,
}

/// Remove environments the retention policy no longer needs.
///
/// Failed and abandoned trials are always removed. With `keep`, known-good
/// environments beyond the `keep` most recent are removed as well. The
/// running, selected and default environments and their recorded fallbacks
/// are never candidates, and each removal repeats every destroy-btrfs check.
pub fn prune_btrfs_boot_environments(
    esp: &Path,
    volume_path: &Path,
    keep: Option<usize>,
    dry_run: bool,
) -> Result<BootPruneReport> {
    let volume = BtrfsVolume::open(volume_path)?;
    let planned = plan(esp, &volume, keep)?;
    // The kernel refuses to delete the default subvolume, but only after
    // destroy-btrfs has retired its boot entry. Skip it up front. If the
    // lookup fails, destroy-btrfs still reports the refusal.
    let default = volume.default_subvolume().ok();
    let mut report = BootPruneReport::default();
    for (id, kind, subvolid) in planned {
        if default == Some(subvolid) {
            report.failed.push((
                id,
                format!(
                    "its root is the Btrfs filesystem default subvolume (ID {subvolid}); choose \
                     another default, for example with `btrfs subvolume set-default 5 {}`",
                    volume_path.display()
                ),
            ));
            continue;
        }
        let result = (|| {
            if kind == BootRemoval::AbandonedTrial {
                exhaust_trial(esp, &volume, &id, dry_run)?;
                if dry_run {
                    return Ok(());
                }
            }
            destroy_btrfs_boot_environment(esp, volume_path, &id, dry_run).map(drop)
        })();
        match result {
            Ok(()) => report.removed.push((id, kind)),
            Err(error) => report.failed.push((id, format!("{error:#}"))),
        }
    }
    Ok(report)
}

/// Cancel the pending or running trial, then remove every failed trial that is not running.
pub fn discard_btrfs_trial(esp: &Path, volume_path: &Path) -> Result<TrialDiscard> {
    let volume = BtrfsVolume::open(volume_path)?;
    let running = volume.running()?.map(|(id, _)| id);
    let pending = {
        let esp = std::fs::canonicalize(esp).context("opening the EFI system partition")?;
        let lock = lock_esp(&esp, FlockOperation::LockShared)?;
        let Inventory {
            policy,
            entries,
            firmware,
        } = inventory(&esp, &lock, false)?;
        let selected = policy
            .as_ref()
            .and_then(|policy| policy.selected(&entries))
            .filter(|entry| entry.tries.is_some());
        // A verified firmware trial is already known-good and only awaits promotion.
        let booted = match &firmware {
            Some(firmware) => firmware.running_trial(&entries)?,
            None => entries
                .iter()
                .find(|entry| Some(&entry.id) == running.as_ref()),
        }
        .filter(|entry| entry.tries.is_some());
        selected
            .or(booted)
            .map(|trial| (trial.id.clone(), trial.parent.clone()))
    };
    let cancelled = match pending {
        Some((id, parent)) => {
            let parent = parent.context("the trial has no recorded fallback")?;
            rollback(esp, Some(&parent), None, Some(&volume))
                .with_context(|| format!("selecting the fallback {parent} instead of {id}"))?;
            Some(id)
        }
        None => None,
    };
    let still_running = cancelled.is_some() && cancelled == running;
    let prune = prune_btrfs_boot_environments(esp, volume_path, None, false)?;
    Ok(TrialDiscard {
        cancelled,
        running: still_running,
        prune,
    })
}

type Planned = (String, BootRemoval, u64);

fn plan(esp: &Path, volume: &BtrfsVolume, keep: Option<usize>) -> Result<Vec<Planned>> {
    let esp = std::fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let Inventory {
        policy,
        entries,
        firmware,
    } = inventory(&esp, &lock, false)?;
    let on_volume = |entry: &BootEntry| {
        entry
            .btrfs
            .as_ref()
            .is_some_and(|record| volume.has_fsid(&record.fsid))
    };
    let subvolume = |entry: &BootEntry| entry.btrfs.as_ref().map_or(0, |record| record.subvolid);
    let mut planned: Vec<_> = destroy::pending(&esp)?
        .into_iter()
        .filter(on_volume)
        .map(|entry| {
            let subvolid = subvolume(&entry);
            (entry.id, BootRemoval::Retiring, subvolid)
        })
        .collect();
    let policy = policy.context(NO_SELECTION)?;
    let mut protected = Vec::new();
    if let Some((id, parent)) = volume.running()? {
        protected.push(id);
        protected.extend(parent);
    }
    if let Some(firmware) = &firmware
        && let Some(trial) = firmware.running_trial(&entries)?
    {
        protected.push(trial.id.clone());
        protected.extend(trial.parent.clone());
    }
    let entries: Vec<_> = entries.into_iter().filter(on_volume).collect();
    for (id, kind) in candidates(&entries, &policy, &protected, keep)? {
        let entry = entries.iter().find(|entry| entry.id == id);
        planned.push((id, kind, entry.map_or(0, subvolume)));
    }
    Ok(planned)
}

/// Choose removable environments, oldest first within each kind.
fn candidates(
    entries: &[BootEntry],
    policy: &LoaderPolicy,
    protected: &[String],
    keep: Option<usize>,
) -> Result<Vec<(String, BootRemoval)>> {
    let selected = policy.selected(entries);
    ensure!(
        selected.is_none_or(|entry| entry.tries.is_none()),
        "a trial is pending; boot it, or cancel it with `cps boot-env discard-trial`, before \
         pruning"
    );
    let mut kept: BTreeSet<&str> = protected.iter().map(String::as_str).collect();
    if let LoaderPolicy::Pinned { default, .. } = policy {
        kept.insert(default);
    }
    if let Some(selected) = selected {
        kept.insert(&selected.id);
        kept.extend(selected.parent.as_deref());
    }
    let mut ordered: Vec<_> = entries.iter().collect();
    ordered.sort_by_key(|entry| (entry.revision, entry.id.as_str()));
    let mut removals: Vec<_> = ordered
        .iter()
        .filter(|entry| !kept.contains(entry.id.as_str()))
        .filter_map(|entry| match entry.tries {
            Some(0) => Some((entry.id.clone(), BootRemoval::FailedTrial)),
            Some(_) => Some((entry.id.clone(), BootRemoval::AbandonedTrial)),
            None => None,
        })
        .collect();
    if let Some(keep) = keep {
        let known_good: Vec<_> = ordered
            .iter()
            .filter(|entry| entry.tries.is_none())
            .collect();
        let excess = known_good.len().saturating_sub(keep);
        removals.extend(
            known_good[..excess]
                .iter()
                .filter(|entry| !kept.contains(entry.id.as_str()))
                .map(|entry| (entry.id.clone(), BootRemoval::KnownGood)),
        );
    }
    Ok(removals)
}

/// Spend the remaining attempts of an unselected systemd-boot trial so that
/// destroy-btrfs accepts it. systemd-boot never selects an exhausted entry.
fn exhaust_trial(esp: &Path, volume: &BtrfsVolume, id: &str, dry_run: bool) -> Result<()> {
    let esp = std::fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockExclusive)?;
    let Inventory {
        policy,
        entries,
        firmware,
    } = inventory(&esp, &lock, false)?;
    ensure!(
        firmware.is_none(),
        "unexpected direct UEFI trial with attempts left"
    );
    let policy = policy.context(NO_SELECTION)?;
    let entry = entries
        .iter()
        .find(|entry| entry.id == id)
        .with_context(|| format!("unknown boot environment {id}"))?;
    ensure!(
        entry.tries.is_some_and(|tries| tries > 0),
        "boot environment {id} is not a trial with attempts left"
    );
    ensure!(
        policy
            .selected(&entries)
            .is_none_or(|selected| selected.id != id)
            && !matches!(&policy, LoaderPolicy::Pinned { preferred: Some(preferred), .. } if preferred == id),
        "trial {id} is selected for the next boot; cancel it with `cps boot-env discard-trial`"
    );
    ensure!(
        volume.running()?.is_none_or(|(running, _)| running != id),
        "cannot discard the running trial {id}; boot its fallback first"
    );
    if dry_run {
        return Ok(());
    }
    let name = entry
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid boot entry filename")?;
    let counter = name
        .strip_prefix(id)
        .and_then(|name| name.strip_prefix('+'))
        .and_then(|name| name.strip_suffix(".conf"))
        .context("expected a counted boot entry")?;
    let exhausted = match counter.split_once('-') {
        Some((_, done)) => format!("{id}+0-{done}.conf"),
        None => format!("{id}+0.conf"),
    };
    renameat_with(
        CWD,
        &entry.path,
        CWD,
        entry.path.with_file_name(exhausted),
        RenameFlags::NOREPLACE,
    )
    .context("exhausting the abandoned trial's boot counter")?;
    File::open(entry.path.parent().context("missing entries directory")?)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::systemd_boot::policy::BootRoot;

    fn entry(revision: u64, parent: Option<u64>, tries: Option<u32>) -> BootEntry {
        BootEntry {
            path: format!("{}.conf", id(revision)).into(),
            id: id(revision),
            revision,
            tries,
            description: String::new(),
            title: String::new(),
            root: BootRoot::Partition("7adff456-af50-498f-b39a-dcf87cb4a7c1".into()),
            parent: parent.map(id),
            btrfs: None,
            uki: None,
        }
    }

    fn id(revision: u64) -> String {
        let image = if revision.is_multiple_of(2) { "a" } else { "b" };
        format!(
            "cps-{}-{revision}",
            [
                image.repeat(8),
                image.repeat(4),
                image.repeat(4),
                image.repeat(4),
                image.repeat(12)
            ]
            .join("-")
        )
    }

    fn removed(found: &[(String, BootRemoval)]) -> Vec<(u64, BootRemoval)> {
        found
            .iter()
            .map(|(id, kind)| (id.rsplit_once('-').unwrap().1.parse().unwrap(), *kind))
            .collect()
    }

    #[test]
    fn retention_keeps_recent_selected_running_and_fallback_environments() -> Result<()> {
        let entries: Vec<_> = (1..=6)
            .map(|revision| entry(revision, (revision > 1).then(|| revision - 1), None))
            .collect();
        let policy = LoaderPolicy::pinned(&id(6));
        assert_eq!(
            removed(&candidates(&entries, &policy, &[], Some(3))?),
            [
                (1, BootRemoval::KnownGood),
                (2, BootRemoval::KnownGood),
                (3, BootRemoval::KnownGood)
            ]
        );
        // The running root and its fallback survive even when they are old.
        let running = [id(2), id(1)];
        assert_eq!(
            removed(&candidates(&entries, &policy, &running, Some(1))?),
            [(3, BootRemoval::KnownGood), (4, BootRemoval::KnownGood)]
        );
        // A rolled-back default keeps its fallback, and newer roots can go.
        let rolled_back = LoaderPolicy::pinned(&id(3));
        assert_eq!(
            removed(&candidates(&entries, &rolled_back, &[], Some(1))?),
            [
                (1, BootRemoval::KnownGood),
                (4, BootRemoval::KnownGood),
                (5, BootRemoval::KnownGood)
            ]
        );
        assert!(candidates(&entries, &policy, &[], None)?.is_empty());
        Ok(())
    }

    #[test]
    fn retention_removes_failed_and_abandoned_trials_but_not_a_pending_one() -> Result<()> {
        let mut entries = vec![
            entry(1, None, None),
            entry(2, Some(1), None),
            entry(3, Some(2), Some(0)),
            entry(4, Some(2), Some(2)),
        ];
        let pending = LoaderPolicy::trial(&id(2), &id(4));
        let error = candidates(&entries, &pending, &[], Some(1)).unwrap_err();
        assert!(error.to_string().contains("discard-trial"));
        // After rollback the trial with attempts left is no longer selected.
        let rolled_back = LoaderPolicy::pinned(&id(2));
        assert_eq!(
            removed(&candidates(&entries, &rolled_back, &[], None)?),
            [
                (3, BootRemoval::FailedTrial),
                (4, BootRemoval::AbandonedTrial)
            ]
        );
        // A running trial is never a candidate.
        assert_eq!(
            removed(&candidates(&entries, &rolled_back, &[id(4)], Some(5))?),
            [(3, BootRemoval::FailedTrial)]
        );
        // An exhausted preferred trial falls back to the default and can go.
        // The default's own fallback stays.
        entries[3].tries = Some(0);
        assert_eq!(
            removed(&candidates(&entries, &pending, &[], Some(1))?),
            [(3, BootRemoval::FailedTrial), (4, BootRemoval::FailedTrial)]
        );
        // Automatic selection protects the newest bootable entry.
        assert_eq!(
            removed(&candidates(
                &entries,
                &LoaderPolicy::Automatic,
                &[],
                Some(1)
            )?),
            [(3, BootRemoval::FailedTrial), (4, BootRemoval::FailedTrial)]
        );
        Ok(())
    }
}
