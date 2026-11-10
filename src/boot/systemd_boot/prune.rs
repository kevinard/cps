//! Retention of Btrfs boot environments, built on retirement records.

use std::{collections::BTreeSet, fs::File, path::Path};

use anyhow::{Context, Result, ensure};
use rustix::fs::{CWD, FlockOperation, RenameFlags, renameat_with};

use super::{
    Inventory, NO_SELECTION, destroy, destroy_btrfs_boot_environment, inventory, lock_esp,
    policy::{BootEntry, LoaderPolicy},
    rollback,
};
use crate::{
    boot::btrfs_boot::{BtrfsRecord, BtrfsVolume},
    image::root::Root,
};

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
///
/// `staged` names the root that the caller's transaction stages into. A
/// trial booting that root is an interrupted activation that the caller
/// resumes, so it stays while its activation can still complete.
pub fn prune_btrfs_boot_environments(
    esp: &Path,
    volume_path: &Path,
    keep: Option<usize>,
    dry_run: bool,
    staged: Option<&Path>,
) -> Result<BootPruneReport> {
    let volume = BtrfsVolume::open(volume_path)?;
    let planned = plan(esp, &volume, keep, staged)?;
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
        let result = remove(
            kind,
            dry_run,
            |dry_run| exhaust_trial(esp, &volume, &id, dry_run),
            |dry_run, exhausted| {
                if exhausted {
                    destroy::check_exhausted_destruction(esp, volume_path, &id)
                } else {
                    destroy_btrfs_boot_environment(esp, volume_path, &id, dry_run).map(drop)
                }
            },
        );
        match result {
            Ok(()) => report.removed.push((id, kind)),
            Err(error) => report.failed.push((id, format!("{error:#}"))),
        }
    }
    Ok(report)
}

/// Remove one planned environment. An abandoned trial first spends its
/// remaining attempts; a dry run checks its destruction as if it had.
fn remove(
    kind: BootRemoval,
    dry_run: bool,
    exhaust: impl FnOnce(bool) -> Result<()>,
    destroy: impl FnOnce(bool, bool) -> Result<()>,
) -> Result<()> {
    let abandoned = kind == BootRemoval::AbandonedTrial;
    if abandoned {
        exhaust(dry_run)?;
    }
    destroy(dry_run, abandoned && dry_run)
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
        // A verified firmware trial is already known-good and only awaits promotion.
        let booted = match &firmware {
            Some(firmware) => firmware.running_trial(&entries)?,
            None => entries
                .iter()
                .find(|entry| Some(&entry.id) == running.as_ref()),
        };
        cancellable(policy.as_ref(), &entries, booted)
            .map(|trial| (trial.id.clone(), trial.parent.clone()))
    };
    let cancelled = match pending {
        Some((id, parent)) => {
            let parent = parent.context("the trial has no recorded fallback")?;
            rollback(esp, Some(&parent), &volume)
                .with_context(|| format!("selecting the fallback {parent} instead of {id}"))?;
            Some(id)
        }
        None => None,
    };
    let still_running = cancelled.is_some() && cancelled == running;
    let prune = prune_btrfs_boot_environments(esp, volume_path, None, false, None)?;
    Ok(TrialDiscard {
        cancelled,
        running: still_running,
        prune,
    })
}

/// The trial that discard-trial cancels: the one selected for the next boot,
/// or else the running one.
fn cancellable<'a>(
    policy: Option<&LoaderPolicy>,
    entries: &'a [BootEntry],
    booted: Option<&'a BootEntry>,
) -> Option<&'a BootEntry> {
    policy
        .and_then(|policy| policy.selected(entries))
        .filter(|entry| entry.tries.is_some())
        .or(booted.filter(|entry| entry.tries.is_some()))
}

type Planned = (String, BootRemoval, u64);

fn plan(
    esp: &Path,
    volume: &BtrfsVolume,
    keep: Option<usize>,
    staged: Option<&Path>,
) -> Result<Vec<Planned>> {
    let esp = std::fs::canonicalize(esp).context("opening the EFI system partition")?;
    let lock = lock_esp(&esp, FlockOperation::LockShared)?;
    let Inventory {
        policy,
        entries,
        firmware,
    } = inventory(&esp, &lock, false)?;
    let on_volume = |entry: &BootEntry| volume.has_fsid(&entry.btrfs.fsid);
    let subvolume = |entry: &BootEntry| entry.btrfs.subvolid;
    let mut planned: Vec<_> = destroy::pending(&esp)?
        .into_iter()
        .filter(on_volume)
        .map(|entry| {
            let subvolid = subvolume(&entry);
            (entry.id, BootRemoval::Retiring, subvolid)
        })
        .collect();
    let policy = policy.context(NO_SELECTION)?;
    let firmware_trial = match &firmware {
        Some(firmware) => firmware.running_trial(&entries)?,
        None => None,
    };
    let staged_trial = match staged.and_then(|path| staged_record(volume, path)) {
        Some(record) => resumable_trial(&entries, &record, |entry| match &firmware {
            // An unarmed direct UEFI trial reads as exhausted until BootNext
            // selects it; activation arms it again within the same boot.
            Some(firmware) => firmware.can_arm(entry),
            None => Ok(entry.tries.is_some_and(|tries| tries > 0)),
        })?,
        None => None,
    };
    let protected = protected(volume.running()?, firmware_trial, staged_trial);
    let entries: Vec<_> = entries.into_iter().filter(on_volume).collect();
    for (id, kind) in candidates(&entries, &policy, &protected, keep)? {
        let entry = entries.iter().find(|entry| entry.id == id);
        planned.push((id, kind, entry.map_or(0, subvolume)));
    }
    Ok(planned)
}

/// The Btrfs identity of a staged root, if it exists on this volume.
fn staged_record(volume: &BtrfsVolume, path: &Path) -> Option<BtrfsRecord> {
    let path = std::fs::canonicalize(path).ok()?;
    let root = Root::open(&path).ok()?;
    volume.record(&root, &path).ok()
}

/// The trial whose root is `staged` and whose activation can still resume.
fn resumable_trial<'a>(
    entries: &'a [BootEntry],
    staged: &BtrfsRecord,
    can_resume: impl Fn(&BootEntry) -> Result<bool>,
) -> Result<Option<&'a BootEntry>> {
    for entry in entries {
        if entry.tries.is_some() && entry.btrfs.same_subvolume(staged) && can_resume(entry)? {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Environments that prune never removes, with the fallbacks they record:
/// the running root, the running firmware trial and a resumable staged trial.
fn protected(
    running: Option<(String, Option<String>)>,
    firmware_trial: Option<&BootEntry>,
    staged_trial: Option<&BootEntry>,
) -> Vec<String> {
    let mut protected = Vec::new();
    if let Some((id, parent)) = running {
        protected.push(id);
        protected.extend(parent);
    }
    for trial in [firmware_trial, staged_trial].into_iter().flatten() {
        protected.push(trial.id.clone());
        protected.extend(trial.parent.clone());
    }
    protected
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
    let exhausted = entry.path.with_file_name(exhausted_name(id, name)?);
    renameat_with(CWD, &entry.path, CWD, &exhausted, RenameFlags::NOREPLACE)
        .context("exhausting the abandoned trial's boot counter")?;
    // On VFAT the renamed entry's size is durable only with its inode.
    File::open(&exhausted)?.sync_all()?;
    File::open(entry.path.parent().context("missing entries directory")?)?.sync_all()?;
    Ok(())
}

/// The boot-counted filename with no attempts left, keeping the count of
/// attempts already made: `ID+N-M.conf` becomes `ID+0-M.conf`.
fn exhausted_name(id: &str, name: &str) -> Result<String> {
    let counter = name
        .strip_prefix(id)
        .and_then(|name| name.strip_prefix('+'))
        .and_then(|name| name.strip_suffix(".conf"))
        .context("expected a counted boot entry")?;
    Ok(match counter.split_once('-') {
        Some((_, done)) => format!("{id}+0-{done}.conf"),
        None => format!("{id}+0.conf"),
    })
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
            btrfs: btrfs(1000 + revision, &format!("@environments/{revision}/root")),
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

    fn btrfs(subvolid: u64, path: &str) -> BtrfsRecord {
        BtrfsRecord {
            fsid: "11111111-1111-4111-8111-111111111111".into(),
            uuid: format!("22222222-2222-4222-8222-{subvolid:012}"),
            subvolid,
            path: path.into(),
        }
    }

    #[test]
    fn exhausting_a_trial_keeps_its_attempt_count() -> Result<()> {
        let id = id(4);
        assert_eq!(
            exhausted_name(&id, &format!("{id}+2-1.conf"))?,
            format!("{id}+0-1.conf")
        );
        assert_eq!(
            exhausted_name(&id, &format!("{id}+3.conf"))?,
            format!("{id}+0.conf")
        );
        assert_eq!(
            exhausted_name(&id, &format!("{id}+1-12.conf"))?,
            format!("{id}+0-12.conf")
        );
        assert!(exhausted_name(&id, &format!("{id}.conf")).is_err());
        assert!(exhausted_name(&id, &format!("{}+1.conf", self::id(5))).is_err());
        Ok(())
    }

    #[test]
    fn backups_count_as_known_good_environments() -> Result<()> {
        // The running root 5 boots with fallback 3. Live changes left the
        // backups 6 to 9, which record no parent.
        let entries: Vec<_> = [3, 6, 7, 8, 9]
            .into_iter()
            .map(|revision| entry(revision, None, None))
            .chain([entry(5, Some(3), None)])
            .collect();
        let policy = LoaderPolicy::pinned(&id(5));
        let protected = protected(Some((id(5), Some(id(3)))), None, None);
        // The oldest backups retire first, and the running root and its
        // fallback stay even when they are among the oldest.
        assert_eq!(
            removed(&candidates(&entries, &policy, &protected, Some(3))?),
            [(6, BootRemoval::KnownGood)]
        );
        assert_eq!(
            removed(&candidates(&entries, &policy, &protected, Some(1))?),
            [
                (6, BootRemoval::KnownGood),
                (7, BootRemoval::KnownGood),
                (8, BootRemoval::KnownGood)
            ]
        );
        Ok(())
    }

    #[test]
    fn prune_protects_running_firmware_and_resumable_trials_with_their_fallbacks() -> Result<()> {
        let firmware_trial = entry(5, Some(4), Some(1));
        let staged_trial = entry(7, Some(6), Some(3));
        assert!(protected(None, None, None).is_empty());
        assert_eq!(
            protected(Some((id(3), Some(id(2)))), None, None),
            [id(3), id(2)]
        );
        // A bootstrap root has no fallback.
        assert_eq!(protected(Some((id(1), None)), None, None), [id(1)]);
        assert_eq!(
            protected(
                Some((id(3), Some(id(2)))),
                Some(&firmware_trial),
                Some(&staged_trial)
            ),
            [id(3), id(2), id(5), id(4), id(7), id(6)]
        );
        Ok(())
    }

    #[test]
    fn an_interrupted_activation_of_the_staged_root_survives_pruning() -> Result<()> {
        let staged = btrfs(300, "@environments/upgrade-plan/root");
        let entries = |trial_tries| {
            let mut base = entry(2, Some(1), None);
            base.btrfs = btrfs(280, "@environments/upgrade-old/root");
            let mut trial = entry(3, Some(2), Some(trial_tries));
            trial.btrfs = btrfs(300, "@environments/upgrade-plan/root");
            let mut other = entry(4, Some(2), Some(3));
            other.btrfs = btrfs(310, "@environments/upgrade-other/root");
            [base, trial, other]
        };
        let failed = entries(0);
        let entries = entries(3);
        let systemd_boot = |entry: &BootEntry| Ok(entry.tries.is_some_and(|tries| tries > 0));
        let found = resumable_trial(&entries, &staged, systemd_boot)?;
        assert_eq!(found.map(|entry| entry.revision), Some(3));
        // Without that protection, the unselected counted entry is abandoned.
        let pinned = LoaderPolicy::pinned(&id(2));
        assert_eq!(
            removed(&candidates(&entries, &pinned, &[], None)?),
            [
                (3, BootRemoval::AbandonedTrial),
                (4, BootRemoval::AbandonedTrial)
            ]
        );
        let kept = protected(None, None, found);
        assert_eq!(
            removed(&candidates(&entries, &pinned, &kept, None)?),
            [(4, BootRemoval::AbandonedTrial)]
        );
        // An exhausted systemd-boot trial has failed and is not resumed.
        assert!(resumable_trial(&failed, &staged, systemd_boot)?.is_none());
        // A direct UEFI trial reads as exhausted until BootNext arms it; it is
        // resumable while the firmware can still arm it in this boot.
        assert!(resumable_trial(&failed, &staged, |_| Ok(true))?.is_some());
        assert!(resumable_trial(&failed, &staged, |_| Ok(false))?.is_none());
        // Known-good roots are never treated as trials.
        assert!(resumable_trial(&entries[..1], &btrfs(280, "x"), |_| Ok(true))?.is_none());
        Ok(())
    }

    #[test]
    fn dry_run_checks_abandoned_trial_destruction_as_exhausted() -> Result<()> {
        use std::cell::RefCell;
        let calls = RefCell::new(Vec::new());
        let run = |kind, dry_run| {
            calls.borrow_mut().clear();
            remove(
                kind,
                dry_run,
                |dry_run| {
                    calls.borrow_mut().push(format!("exhaust dry={dry_run}"));
                    Ok(())
                },
                |dry_run, exhausted| {
                    calls
                        .borrow_mut()
                        .push(format!("destroy dry={dry_run} exhausted={exhausted}"));
                    Ok(())
                },
            )
        };
        run(BootRemoval::AbandonedTrial, true)?;
        assert_eq!(
            *calls.borrow(),
            ["exhaust dry=true", "destroy dry=true exhausted=true"]
        );
        run(BootRemoval::AbandonedTrial, false)?;
        assert_eq!(
            *calls.borrow(),
            ["exhaust dry=false", "destroy dry=false exhausted=false"]
        );
        run(BootRemoval::FailedTrial, true)?;
        assert_eq!(*calls.borrow(), ["destroy dry=true exhausted=false"]);
        // A failed destroy check fails the dry run.
        let error = remove(
            BootRemoval::AbandonedTrial,
            true,
            |_| Ok(()),
            |_, _| anyhow::bail!("another boot entry references this root"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("references"));
        Ok(())
    }

    #[test]
    fn discard_cancels_the_selected_trial_or_else_the_running_one() {
        let base = entry(2, Some(1), None);
        let running = entry(4, Some(2), Some(0));
        let entries = [entry(2, Some(1), None), entry(3, Some(2), Some(2))];
        let trial = LoaderPolicy::trial(&id(2), &id(3));
        let pinned = LoaderPolicy::pinned(&id(2));
        let revision = |found: Option<&BootEntry>| found.map(|entry| entry.revision);
        // The pending trial wins over the running one.
        assert_eq!(
            revision(cancellable(Some(&trial), &entries, Some(&running))),
            Some(3)
        );
        // A running trial is cancelled when nothing is pending.
        assert_eq!(
            revision(cancellable(Some(&pinned), &entries, Some(&running))),
            Some(4)
        );
        assert_eq!(
            revision(cancellable(None, &entries, Some(&running))),
            Some(4)
        );
        // Known-good selections and a known-good running root are not trials.
        assert_eq!(
            revision(cancellable(Some(&pinned), &entries, Some(&base))),
            None
        );
        assert_eq!(revision(cancellable(Some(&pinned), &entries, None)), None);
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
