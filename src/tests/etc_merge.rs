//! The /etc merge into a staged environment, as run at shutdown.

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use super::*;
use crate::{
    boot::etc_merge::{self, Step},
    image::{Image, root::Root},
    list_config_updates,
};

const PRESERVED: &str = "etc/canopa/preserved.conf";
const STABLE: &str = "etc/canopa/stable.conf";
const VENDOR: &str = "etc/canopa/vendor.conf";
const REMOVED: &str = "etc/canopa/removed.conf";
const STASH: &str = "usr/lib/sysimage/cps/config-updates/etc-merge/2";
const PASSWD: &str = "root:x:0:0::/root:/bin/sh\nbin:x:1:1::/:/usr/bin/nologin\n";

fn package(f: &Fixture, version: &str, vendor: &str) -> Result<()> {
    let actions = f.file(PRESERVED, vendor, "0644", "preserve=#true")?
        + &f.file(STABLE, "stable", "0644", "preserve=#true")?
        + &f.file(VENDOR, vendor, "0644", "")?
        + &f.file(REMOVED, "removed", "0644", "")?;
    f.package("system/settings", version, actions)
}

/// Stage `copies` trials of an upgrade, then change the running root.
fn prepare(copies: usize) -> Result<(Fixture, Vec<PathBuf>)> {
    let f = Fixture::new()?;
    package(&f, "1.0.0-0", "vendor one")?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["system/settings".into()],
        },
    )?;
    apply(&f.root, &install)?;
    let etc = f.root.join("etc");
    fs::write(etc.join("passwd"), PASSWD)?;
    fs::write(etc.join("shadow"), "root:!:1::::::\nbin:!:1::::::\n")?;
    fs::write(etc.join("hostname"), "canopa\n")?;
    fs::create_dir(etc.join("local"))?;
    fs::write(etc.join("local/old"), "old")?;
    fs::write(etc.join("local/mode"), "mode")?;
    fs::create_dir(etc.join("gone"))?;
    fs::write(etc.join("gone/file"), "gone")?;
    symlink("../usr/share/zoneinfo/UTC", etc.join("localtime"))?;

    package(&f, "2.0.0-0", "vendor two")?;
    let update = plan(&f.root, Request::Update)?;
    let mut trials = Vec::new();
    for copy in 0..copies {
        let trial = f._dir.path().join(format!("trial-{copy}"));
        crate::boot::stage_directory_copy(&f.root, &trial, &update)?;
        // Changes besides the package files, as a trigger would make them.
        fs::write(
            trial.join("etc/passwd"),
            format!("{PASSWD}systemd-network:x:192:192::/:/usr/bin/nologin\n"),
        )?;
        fs::write(trial.join("etc/hostname"), "staged\n")?;
        trials.push(trial);
    }

    // The administrator keeps working on the running system until shutdown.
    fs::write(f.root.join(PRESERVED), "local edit")?;
    fs::write(f.root.join(STABLE), "local stable")?;
    fs::write(f.root.join(VENDOR), "drifted")?;
    fs::remove_file(f.root.join(REMOVED))?;
    fs::write(
        etc.join("passwd"),
        "root:x:0:0::/root:/bin/zsh\nbin:x:1:1::/:/usr/bin/nologin\nalice:x:1000:1000::/home/alice:/bin/sh\n",
    )?;
    fs::write(
        etc.join("shadow"),
        "root:$6$new:2::::::\nbin:!:1::::::\nalice:$6$alice:2::::::\n",
    )?;
    fs::write(etc.join("hostname"), "workstation\n")?;
    fs::write(etc.join("local/new"), "new")?;
    fs::remove_file(etc.join("local/old"))?;
    fs::set_permissions(etc.join("local/mode"), fs::Permissions::from_mode(0o600))?;
    fs::remove_file(etc.join("localtime"))?;
    symlink("../usr/share/zoneinfo/Europe/Paris", etc.join("localtime"))?;
    fs::remove_dir_all(etc.join("gone"))?;
    fs::create_dir(etc.join("added"))?;
    fs::write(etc.join("added/one"), "linked")?;
    fs::hard_link(etc.join("added/one"), etc.join("added/two"))?;
    Ok((f, trials))
}

fn merge(
    f: &Fixture,
    trial: &Path,
    step: impl FnMut(Step) -> Result<()>,
) -> Result<Option<crate::EtcMergeReport>> {
    let image = Image::open_exclusive_root(Root::open(trial)?)?;
    etc_merge::finalize_with(&image, &Root::open(&f.root)?, step)
}

/// Each path below `/etc` with its type, mode and contents.
fn tree(root: &Path) -> Result<BTreeMap<String, String>> {
    fn visit(path: &Path, name: &str, out: &mut BTreeMap<String, String>) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let mode = metadata.mode() & 0o7777;
        let value = if metadata.is_dir() {
            for child in fs::read_dir(path)? {
                let child = child?;
                let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
                visit(&child.path(), &child_name, out)?;
            }
            format!("dir {mode:o}")
        } else if metadata.file_type().is_symlink() {
            format!("link {}", fs::read_link(path)?.display())
        } else {
            format!("file {mode:o} {}", fs::read_to_string(path)?)
        };
        out.insert(name.to_owned(), value);
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(&root.join("etc"), "etc", &mut out)?;
    Ok(out)
}

fn read(root: &Path, path: &str) -> Result<String> {
    Ok(fs::read_to_string(root.join(path))?)
}

fn check_merged(f: &Fixture, trial: &Path) -> Result<()> {
    assert_eq!(read(trial, PRESERVED)?, "local edit");
    assert_eq!(
        read(
            trial,
            &format!("usr/lib/sysimage/cps/config-updates/2/{PRESERVED}")
        )?,
        "vendor two"
    );
    assert_eq!(read(trial, STABLE)?, "local stable");
    assert_eq!(read(trial, VENDOR)?, "vendor two");
    assert_eq!(read(trial, &format!("{STASH}/{VENDOR}"))?, "drifted");
    assert_eq!(read(trial, REMOVED)?, "removed");
    assert_eq!(
        read(trial, "etc/passwd")?,
        "root:x:0:0::/root:/bin/zsh\nbin:x:1:1::/:/usr/bin/nologin\n\
         systemd-network:x:192:192::/:/usr/bin/nologin\nalice:x:1000:1000::/home/alice:/bin/sh\n"
    );
    assert_eq!(read(trial, "etc/shadow")?, read(&f.root, "etc/shadow")?);
    assert_eq!(read(trial, "etc/hostname")?, "staged\n");
    assert_eq!(
        read(trial, &format!("{STASH}/etc/hostname"))?,
        "workstation\n"
    );
    assert_eq!(read(trial, "etc/local/new")?, "new");
    assert!(!trial.join("etc/local/old").exists());
    assert_eq!(
        fs::metadata(trial.join("etc/local/mode"))?.mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::read_link(trial.join("etc/localtime"))?,
        Path::new("../usr/share/zoneinfo/Europe/Paris")
    );
    assert!(!trial.join("etc/gone").exists());
    let (one, two) = (
        fs::metadata(trial.join("etc/added/one"))?,
        fs::metadata(trial.join("etc/added/two"))?,
    );
    assert_eq!((one.ino(), one.nlink()), (two.ino(), 2));
    // The package state accepts the result: only the preserved edits differ.
    for finding in verify(trial)? {
        assert!(!finding.is_error, "{}", finding.message);
    }
    let updates = list_config_updates(trial)?;
    assert_eq!(updates.len(), 1);
    assert_eq!(
        (updates[0].path.as_str(), updates[0].vendor_available),
        (PRESERVED, true)
    );
    let saved: Vec<_> = crate::saved_etc_versions(trial)?
        .into_iter()
        .map(|(revision, path, _)| (revision, path))
        .collect();
    assert_eq!(
        saved,
        [(2, VENDOR.to_owned()), (2, "etc/hostname".to_owned())]
    );
    assert!(!trial.join("usr/lib/sysimage/cps/etc-merge").exists());
    Ok(())
}

#[test]
fn shutdown_merge_carries_local_changes_into_the_staged_root() -> Result<()> {
    let (f, trials) = prepare(1)?;
    let trial = &trials[0];
    let base = trial.join("usr/lib/sysimage/cps").join(etc_merge::BASE);
    assert_eq!(fs::metadata(&base)?.mode() & 0o777, 0o600);
    let staged = tree(trial)?;

    let report = merge(&f, trial, |_| Ok(()))?.expect("the staged root has a merge base");
    check_merged(&f, trial)?;
    assert_eq!(report.revision, 2);
    assert_eq!(
        report.applied,
        [
            "etc/added",
            "etc/added/one",
            "etc/added/two",
            STABLE,
            "etc/gone",
            "etc/gone/file",
            "etc/local/mode",
            "etc/local/new",
            "etc/local/old",
            "etc/localtime",
            "etc/shadow",
        ]
    );
    assert_eq!(report.merged, ["etc/passwd"]);
    assert_eq!(report.config_updates, [PRESERVED]);
    let conflicts: Vec<_> = report
        .conflicts
        .iter()
        .map(|conflict| (conflict.path.as_str(), conflict.saved.is_some()))
        .collect();
    assert_eq!(
        conflicts,
        [(REMOVED, false), (VENDOR, true), ("etc/hostname", true)]
    );
    assert_eq!(crate::last_etc_merge(trial)?, Some(report));
    assert_ne!(tree(trial)?, staged);

    // A second run finds nothing new, and later edits merge on their own.
    let merged = tree(trial)?;
    assert!(merge(&f, trial, |_| Ok(()))?.unwrap().is_empty());
    assert_eq!(tree(trial)?, merged);
    fs::write(f.root.join("etc/local/new"), "newer")?;
    let report = merge(&f, trial, |_| Ok(()))?.unwrap();
    assert_eq!(report.applied, ["etc/local/new"]);
    assert!(report.conflicts.is_empty());
    assert_eq!(read(trial, "etc/local/new")?, "newer");

    // A later conflict is saved beside the first run's, which stays.
    fs::write(f.root.join("etc/hostname"), "renamed")?;
    let report = merge(&f, trial, |_| Ok(()))?.unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(
        read(trial, &format!("{STASH}/etc/hostname"))?,
        "workstation\n"
    );
    assert_eq!(read(trial, &format!("{STASH}-2/etc/hostname"))?, "renamed");
    let saved: Vec<_> = crate::saved_etc_versions(trial)?
        .into_iter()
        .map(|(revision, path, _)| (revision, path))
        .collect();
    assert_eq!(
        saved,
        [
            (2, VENDOR.to_owned()),
            (2, "etc/hostname".to_owned()),
            (2, "etc/hostname".to_owned())
        ]
    );

    // The next transaction in the staged root works with the merged state.
    let review = plan(
        trial,
        Request::ResolveConfig {
            path: PRESERVED.into(),
            take: crate::ConfigChoice::Vendor,
        },
    )?;
    apply(trial, &review)?;
    assert_eq!(read(trial, PRESERVED)?, "vendor two");
    assert!(list_config_updates(trial)?.is_empty());
    for finding in verify(trial)? {
        assert!(!finding.is_error, "{}", finding.message);
    }
    Ok(())
}

#[test]
fn interrupted_shutdown_merge_leaves_the_staged_etc_or_the_merged_one() -> Result<()> {
    let steps = [
        Step::Built,
        Step::Recorded,
        Step::Stashed,
        Step::Exchanged,
        Step::Rebased,
    ];
    let (f, trials) = prepare(steps.len() + 1)?;
    let staged = tree(&trials[0])?;
    merge(&f, &trials[0], |_| Ok(()))?;
    let merged = tree(&trials[0])?;
    for (step, trial) in steps.into_iter().zip(&trials[1..]) {
        let error = merge(&f, trial, |reached| {
            ensure_not(reached == step, "simulated power loss")
        })
        .expect_err("the merge stops at the injected step");
        assert!(format!("{error:#}").contains("simulated power loss"));
        // The exchange is the only step that changes what the trial boots.
        let expected = if matches!(step, Step::Exchanged | Step::Rebased) {
            &merged
        } else {
            &staged
        };
        assert_eq!(&tree(trial)?, expected, "after {step:?}");
        for finding in verify(trial)? {
            assert!(!finding.is_error, "after {step:?}: {}", finding.message);
        }
        // A later run completes the same merge.
        merge(&f, trial, |_| Ok(()))?;
        assert_eq!(tree(trial)?, merged, "after resuming from {step:?}");
        check_merged(&f, trial)?;
    }
    Ok(())
}

#[test]
fn shutdown_merge_needs_a_base_and_a_running_etc() -> Result<()> {
    let (f, trials) = prepare(1)?;
    let trial = &trials[0];
    let staged = tree(trial)?;
    // Without the running /etc, the merge must not read every path as removed.
    let empty = tempfile::tempdir()?;
    let image = Image::open_exclusive_root(Root::open(trial)?)?;
    let error = etc_merge::finalize(&image, &Root::open(empty.path())?).unwrap_err();
    assert!(format!("{error:#}").contains("no /etc"), "{error:#}");
    drop(image);
    assert_eq!(tree(trial)?, staged);
    fs::remove_file(trial.join("usr/lib/sysimage/cps").join(etc_merge::BASE))?;
    assert!(merge(&f, trial, |_| Ok(()))?.is_none());
    assert_eq!(tree(trial)?, staged);
    Ok(())
}

fn ensure_not(condition: bool, message: &str) -> Result<()> {
    if condition {
        anyhow::bail!("{message}");
    }
    Ok(())
}
