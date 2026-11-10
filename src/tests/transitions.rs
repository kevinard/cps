//! Directory and leaf transitions, aborting pending transactions, and the
//! dependency order of writes.

use std::os::unix::fs::MetadataExt;

use super::*;
use crate::{TransactionPhase, abort_transaction, resume_transaction, transaction_progress};

fn ids() -> (u32, u32) {
    (geteuid().as_raw(), getegid().as_raw())
}

fn symlink_action(path: &str, target: &str) -> String {
    let (uid, gid) = ids();
    format!("    symlink {path:?} target={target:?} uid={uid} gid={gid}\n")
}

fn install_all(root: &Path, packages: &[&str]) -> Result<u64> {
    let request = Request::Install {
        packages: packages.iter().map(|name| (*name).to_owned()).collect(),
    };
    apply(root, &plan(root, request)?)
}

fn position(plan: &Plan, path: &str) -> usize {
    plan.writes
        .iter()
        .position(|write| write.path == path)
        .unwrap_or_else(|| panic!("plan does not write {path}"))
}

/// Interrupt a worker applying `plan`, or aborting with `abort`, at a
/// transition step, optionally without RENAME_EXCHANGE.
fn interrupt_step(f: &Fixture, plan: &Plan, step: &str, fallback: bool, abort: bool) -> Result<()> {
    let saved = f._dir.path().join("transition.json");
    fs::write(&saved, serde_json::to_vec(plan)?)?;
    let mut env = vec![("CPS_TEST_INTERRUPT_STEP", step)];
    if fallback {
        env.push(("CPS_TEST_NO_EXCHANGE", "1"));
    }
    if abort {
        env.push(("CPS_TEST_ABORT", "1"));
    }
    interrupt_worker(&f.root, &saved, &env)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn staging_is_empty(root: &Path) -> Result<bool> {
    Ok(fs::read_dir(root.join("var/lib/cps/image/stage"))?
        .next()
        .is_none())
}

/// A `bin` directory that a release replaces with `bin -> usr/bin`.
fn usrmerge() -> Result<(Fixture, Plan)> {
    let f = Fixture::new()?;
    let split = f.file("bin/tool", "tool 1", "0755", "")?
        + &f.file("bin/sub/data", "data 1", "0644", "")?
        + &f.file("usr/bin/other", "other 1", "0755", "")?;
    f.package("base/filesystem", "1.0.0-0", split)?;
    f.create()?;
    assert_eq!(install_all(&f.root, &["base/filesystem"])?, 1);
    fs::remove_dir_all(f.proto.join("bin"))?;
    let merged = f.file("usr/bin/tool", "tool 2", "0755", "")?
        + &f.file("usr/bin/sub/data", "data 2", "0644", "")?
        + &f.file("usr/bin/other", "other 1", "0755", "")?
        + &symlink_action("bin", "usr/bin");
    f.package("base/filesystem", "2.0.0-0", merged)?;
    let update = plan(&f.root, Request::Update)?;
    Ok((f, update))
}

fn assert_split(root: &Path) -> Result<()> {
    assert!(fs::symlink_metadata(root.join("bin"))?.is_dir());
    assert_eq!(fs::read_to_string(root.join("bin/tool"))?, "tool 1");
    assert_eq!(fs::read_to_string(root.join("bin/sub/data"))?, "data 1");
    assert!(!root.join("usr/bin/tool").exists());
    assert!(verify(root)?.is_empty());
    assert!(staging_is_empty(root)?);
    Ok(())
}

fn assert_merged(root: &Path) -> Result<()> {
    assert_eq!(fs::read_link(root.join("bin"))?, Path::new("usr/bin"));
    assert_eq!(fs::read_to_string(root.join("bin/tool"))?, "tool 2");
    assert_eq!(fs::read_to_string(root.join("usr/bin/sub/data"))?, "data 2");
    assert!(verify(root)?.is_empty());
    assert!(staging_is_empty(root)?);
    Ok(())
}

#[test]
fn directory_becomes_a_symlink_and_every_step_resumes_or_aborts() -> Result<()> {
    let (f, update) = usrmerge()?;
    let summary = update.summary();
    assert!(
        summary.contains("Replace bin with a symbolic link to usr/bin"),
        "{summary}"
    );
    assert!(!summary.contains("Prune if empty bin"), "{summary}");
    // The symlink only replaces the directory once its new target is complete.
    assert!(position(&update, "bin") > position(&update, "usr/bin/tool"));
    assert!(position(&update, "bin") > position(&update, "usr/bin/sub/data"));

    // Interrupt each step, with and without RENAME_EXCHANGE, and abort.
    for (step, fallback) in [
        ("staged", false),
        ("exchanged", false),
        ("staged", true),
        ("displaced", true),
        ("published", true),
    ] {
        interrupt_step(&f, &update, step, fallback, false)?;
        let bin = f.root.join("bin");
        match step {
            "staged" => assert!(fs::symlink_metadata(&bin)?.is_dir()),
            "displaced" => assert!(fs::symlink_metadata(&bin).is_err()),
            _ => assert!(is_symlink(&bin)),
        }
        assert_eq!(
            transaction_progress(&f.root, &update, false)?.phase,
            TransactionPhase::Applying
        );
        assert_eq!(abort_transaction(&f.root, false)?, Some(1), "{step}");
        assert_split(&f.root)?;
        assert_eq!(abort_transaction(&f.root, false)?, None);
    }

    // An interrupted abort finishes through either command.
    for (step, fallback, finish_with_resume) in [
        ("staged", false, true),
        ("exchanged", false, false),
        ("displaced", true, true),
        ("published", true, false),
    ] {
        interrupt_step(&f, &update, "exchanged", false, false)?;
        interrupt_step(&f, &update, step, fallback, true)?;
        let status = crate::transaction_status(&f.root)?;
        assert!(
            status
                .pending
                .as_ref()
                .is_some_and(|pending| pending.aborting)
        );
        assert_eq!(
            transaction_progress(&f.root, &update, false)?.phase,
            TransactionPhase::Aborting
        );
        let error = apply(&f.root, &update).unwrap_err();
        assert!(error.to_string().contains("being aborted"), "{error:#}");
        let revision = if finish_with_resume {
            resume_transaction(&f.root)?
        } else {
            abort_transaction(&f.root, false)?
        };
        assert_eq!(revision, Some(1), "{step}");
        assert_split(&f.root)?;
    }

    // Each forward intermediate state also resumes forward, and so does
    // each state of the downgrade, which turns the symlink back into a tree.
    let downgrade = Request::Install {
        packages: vec!["base/filesystem@1.0.0-0".into()],
    };
    for (step, fallback) in [("staged", false), ("exchanged", false), ("displaced", true)] {
        let update = plan(&f.root, Request::Update)?;
        let revision = update.base_revision + 1;
        interrupt_step(&f, &update, step, fallback, false)?;
        assert_eq!(resume_transaction(&f.root)?, Some(revision), "{step}");
        assert_merged(&f.root)?;
        assert_eq!(resume_transaction(&f.root)?, None);
        let restore = plan(&f.root, downgrade.clone())?;
        assert!(restore.summary().contains("Replace bin with a directory"));
        interrupt_step(&f, &restore, step, fallback, false)?;
        assert_eq!(resume_transaction(&f.root)?, Some(revision + 1), "{step}");
        assert_split(&f.root)?;
    }
    Ok(())
}

#[test]
fn symlink_becomes_a_directory_tree_with_links_inside() -> Result<()> {
    let f = Fixture::new()?;
    let linked =
        f.file("usr/lib/ld.so", "loader 1", "0755", "")? + &symlink_action("lib64", "usr/lib");
    f.package("base/loader", "1.0.0-0", linked)?;
    f.create()?;
    install_all(&f.root, &["base/loader"])?;
    assert!(is_symlink(&f.root.join("lib64")));
    let tree = f.file("usr/lib/ld.so", "loader 1", "0755", "")?
        + &f.file("lib64/ld.so", "loader 2", "0755", "")?
        + "    hardlink \"lib64/sub/ld-alias\" target=\"lib64/ld.so\"\n"
        + &symlink_action("lib64/current", "ld.so");
    f.package("base/loader", "2.0.0-0", tree)?;
    let update = plan(&f.root, Request::Update)?;
    assert!(update.summary().contains("Replace lib64 with a directory"));
    let check_tree = |root: &Path| -> Result<()> {
        assert!(fs::symlink_metadata(root.join("lib64"))?.is_dir());
        assert_eq!(fs::read_to_string(root.join("lib64/ld.so"))?, "loader 2");
        assert_eq!(
            fs::metadata(root.join("lib64/ld.so"))?.ino(),
            fs::metadata(root.join("lib64/sub/ld-alias"))?.ino()
        );
        assert_eq!(
            fs::read_link(root.join("lib64/current"))?,
            Path::new("ld.so")
        );
        assert!(verify(root)?.is_empty());
        assert!(staging_is_empty(root)?);
        Ok(())
    };
    for (step, fallback) in [
        ("staged", false),
        ("exchanged", false),
        ("displaced", true),
        ("published", true),
    ] {
        interrupt_step(&f, &update, step, fallback, false)?;
        assert_eq!(abort_transaction(&f.root, false)?, Some(1), "{step}");
        assert!(is_symlink(&f.root.join("lib64")));
        assert!(verify(&f.root)?.is_empty());
        assert!(staging_is_empty(&f.root)?);
    }
    // Abort a completed transition, interrupted inside its own exchange.
    interrupt_step(&f, &update, "published", true, false)?;
    interrupt_step(&f, &update, "exchanged", false, true)?;
    assert!(is_symlink(&f.root.join("lib64")));
    assert_eq!(resume_transaction(&f.root)?, Some(1));
    assert!(is_symlink(&f.root.join("lib64")));
    assert!(verify(&f.root)?.is_empty());

    interrupt_step(&f, &update, "displaced", true, false)?;
    assert_eq!(apply(&f.root, &update)?, 2);
    check_tree(&f.root)?;
    Ok(())
}

#[test]
fn replacing_a_directory_refuses_to_discard_unmanaged_or_local_content() -> Result<()> {
    let f = Fixture::new()?;
    let keep = f.file(
        "etc/keep/main.conf",
        "keep vendor",
        "0644",
        "preserve=#true",
    )? + &f.file("etc/keep/vendor.txt", "vendor", "0644", "")?;
    f.package("app/keep", "1.0.0-0", keep)?;
    let moved = f.file(
        "etc/app/main.conf",
        "app vendor 1",
        "0644",
        "preserve=#true",
    )?;
    f.package("app/config", "1.0.0-0", moved)?;
    f.create()?;
    install_all(&f.root, &["app/keep", "app/config"])?;
    fs::remove_dir_all(f.proto.join("etc"))?;
    f.package(
        "app/keep",
        "2.0.0-0",
        f.file("etc/keep", "single file", "0644", "")?,
    )?;
    // The configuration identity moves next to the directory, which becomes a symlink.
    let identity = "preserve=#true original-name=\"app/config:etc/app/main.conf\"";
    f.package(
        "app/config",
        "2.0.0-0",
        f.file("etc/app.conf", "app vendor 2", "0644", identity)?
            + &f.file("etc/app.d/README", "readme", "0644", "")?
            + &symlink_action("etc/app", "app.d"),
    )?;
    fs::write(f.root.join("etc/keep/main.conf"), "local keep edit")?;
    fs::write(f.root.join("etc/keep/local"), "unmanaged")?;
    fs::create_dir(f.root.join("etc/keep/notes"))?;
    fs::write(f.root.join("etc/keep/notes/today"), "unmanaged")?;
    fs::write(f.root.join("etc/app/main.conf"), "local app edit")?;
    let error = plan(&f.root, Request::Update).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("cannot replace directory etc/keep"),
        "{message}"
    );
    assert!(
        message.contains("etc/keep/main.conf (locally modified configuration)"),
        "{message}"
    );
    assert!(
        message.contains("etc/keep/local (not managed by CPS)"),
        "{message}"
    );
    // An unmanaged directory is listed once, not with everything inside it.
    assert!(
        message.contains("etc/keep/notes (not managed by CPS)"),
        "{message}"
    );
    assert!(!message.contains("etc/keep/notes/today"), "{message}");
    // Locally edited configuration that moves with its identity is no obstacle.
    assert!(!message.contains("etc/app/main.conf"), "{message}");
    assert_eq!(
        fs::read_to_string(f.root.join("etc/keep/local"))?,
        "unmanaged"
    );

    fs::remove_file(f.root.join("etc/keep/local"))?;
    fs::remove_dir_all(f.root.join("etc/keep/notes"))?;
    fs::write(f.root.join("etc/keep/main.conf"), "keep vendor")?;
    let update = plan(&f.root, Request::Update)?;
    let summary = update.summary();
    assert!(
        summary.contains("Move config etc/app/main.conf to etc/app.conf"),
        "{summary}"
    );
    assert!(
        summary.contains("Replace etc/keep with a file"),
        "{summary}"
    );
    interrupt_step(&f, &update, "exchanged", false, false)?;
    assert_eq!(abort_transaction(&f.root, false)?, Some(1));
    assert_eq!(
        fs::read_to_string(f.root.join("etc/app/main.conf"))?,
        "local app edit"
    );
    assert!(!f.root.join("etc/app.conf").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("etc/keep/vendor.txt"))?,
        "vendor"
    );
    let update = plan(&f.root, Request::Update)?;
    assert_eq!(apply(&f.root, &update)?, 2);
    assert_eq!(fs::read_to_string(f.root.join("etc/keep"))?, "single file");
    assert_eq!(
        fs::read_to_string(f.root.join("etc/app.conf"))?,
        "local app edit"
    );
    assert_eq!(fs::read_link(f.root.join("etc/app"))?, Path::new("app.d"));
    let findings = verify(&f.root)?;
    assert!(
        findings.iter().all(|finding| !finding.is_error),
        "{findings:?}"
    );
    Ok(())
}

#[test]
fn replacing_a_file_with_a_directory_keeps_local_configuration() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "app/settings",
        "1.0.0-0",
        f.file("etc/settings", "vendor", "0644", "preserve=#true")?,
    )?;
    f.create()?;
    install_all(&f.root, &["app/settings"])?;
    fs::remove_file(f.proto.join("etc/settings"))?;
    f.package(
        "app/settings",
        "2.0.0-0",
        f.file("etc/settings/main.conf", "vendor 2", "0644", "")?,
    )?;
    fs::write(f.root.join("etc/settings"), "local edit")?;
    let error = plan(&f.root, Request::Update).unwrap_err();
    assert!(
        error.to_string().contains("would discard local config"),
        "{error:#}"
    );
    fs::write(f.root.join("etc/settings"), "vendor")?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(
        fs::read_to_string(f.root.join("etc/settings/main.conf"))?,
        "vendor 2"
    );
    Ok(())
}

/// Two releases of a compositor and its library, with a locally edited
/// configuration, a hard link, and a directory whose mode changes.
fn upgrade_fixture() -> Result<(Fixture, Plan)> {
    const ALIAS: &str = "usr/bin/compositor-alias";
    let f = Fixture::new()?;
    let (uid, gid) = ids();
    f.package(
        "library/libdisplay",
        "1.0.0-0",
        f.file(LIBRARY, "display ABI 1", "0644", "")?,
    )?;
    let release = |version: &str, content: &str, mode: &str| -> Result<String> {
        Ok(dependency("require", "library/libdisplay", version)
            + &f.file(COMPOSITOR, content, "0755", "")?
            + &format!("    hardlink {ALIAS:?} target={COMPOSITOR:?}\n")
            + &f.file(
                CONFIG,
                &format!("vendor config {version}"),
                "0644",
                "preserve=#true",
            )?
            + &format!("    dir \"usr/share/canopa\" mode={mode:?} uid={uid} gid={gid}\n"))
    };
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        release("1.0.0-0", "compositor 1", "0755")?,
    )?;
    f.create()?;
    install_all(&f.root, &["desktop/compositor"])?;
    fs::write(f.root.join(CONFIG), "administrator edit")?;
    f.package(
        "library/libdisplay",
        "2.0.0-0",
        f.file(LIBRARY, "display ABI 2", "0644", "")?,
    )?;
    let actions = release("2.0.0-0", "compositor 2", "0700")?
        + &f.file("usr/share/canopa/new", "new", "0644", "")?;
    f.package("desktop/compositor", "2.0.0-0", actions)?;
    let update = plan(&f.root, Request::Update)?;
    Ok((f, update))
}

fn assert_committed_release(f: &Fixture) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let root = &f.root;
    assert_eq!(fs::read_to_string(root.join(LIBRARY))?, "display ABI 1");
    assert_eq!(fs::read_to_string(root.join(COMPOSITOR))?, "compositor 1");
    assert_eq!(
        fs::metadata(root.join(COMPOSITOR))?.ino(),
        fs::metadata(root.join("usr/bin/compositor-alias"))?.ino()
    );
    assert_eq!(fs::read_to_string(root.join(CONFIG))?, "administrator edit");
    assert_eq!(
        fs::metadata(root.join("usr/share/canopa"))?
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    assert!(!root.join("usr/share/canopa/new").exists());
    assert!(!root.join("var/lib/cps/image/config-updates/2").exists());
    let status = crate::transaction_status(root)?;
    assert_eq!(status.revision, 1);
    assert!(status.pending.is_none());
    let findings = verify(root)?;
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].message.contains("locally modified config"));
    assert!(staging_is_empty(root)?);
    Ok(())
}

#[test]
fn abort_restores_the_committed_revision_from_any_interruption() -> Result<()> {
    let (f, update) = upgrade_fixture()?;
    // Dependencies go first: the new library before the new compositor.
    assert!(position(&update, LIBRARY) < position(&update, COMPOSITOR));
    let paths: Vec<&str> = update
        .writes
        .iter()
        .filter(|write| write.before != write.after)
        .map(|write| write.path.as_str())
        .collect();
    let last = *paths.last().expect("changed paths");
    let saved = f._dir.path().join("update.json");
    for path in &paths {
        interrupt_apply_at(&f.root, &saved, &update, path)?;
        assert_eq!(abort_transaction(&f.root, false)?, Some(1), "{path}");
        assert_committed_release(&f)?;
    }
    // Interrupt the abort itself after each path it restores.
    for path in &paths {
        interrupt_apply_at(&f.root, &saved, &update, last)?;
        let saved = f._dir.path().join("abort.json");
        fs::write(&saved, serde_json::to_vec(&update)?)?;
        let worker = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "tests::install_upgrade_and_resume_offline",
                "--nocapture",
            ])
            .env("CPS_TEST_IMAGE", &f.root)
            .env("CPS_TEST_PLAN", &saved)
            .env("CPS_TEST_AFTER_PATH", path)
            .env("CPS_TEST_ABORT", "1")
            .output()?;
        if worker.status.code() != Some(INTERRUPTED) {
            // Nothing to restore at this path, so the abort finished.
            assert!(worker.status.success() || worker.status.code() == Some(101));
            if crate::transaction_status(&f.root)?.pending.is_none() {
                assert_committed_release(&f)?;
                continue;
            }
        }
        assert!(
            crate::transaction_status(&f.root)?
                .pending
                .is_some_and(|pending| pending.aborting)
        );
        assert_eq!(abort_transaction(&f.root, false)?, Some(1), "{path}");
        assert_committed_release(&f)?;
    }
    assert_eq!(apply(&f.root, &update)?, 2);
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor 2");
    Ok(())
}

#[test]
fn abort_lists_missing_payloads_and_local_contents_before_changing_anything() -> Result<()> {
    let (f, update) = upgrade_fixture()?;
    let saved = f._dir.path().join("update.json");
    interrupt_apply_at(&f.root, &saved, &update, COMPOSITOR)?;
    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    let error = abort_transaction(&f.root, false).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(&format!(
            "{LIBRARY}: payload sha256 {}",
            digest(b"display ABI 1")
        )),
        "{message}"
    );
    assert!(
        message.contains("from cps://canopa/library/libdisplay@1.0.0-0"),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "{COMPOSITOR}: payload sha256 {}",
            digest(b"compositor 1")
        )),
        "{message}"
    );
    assert!(message.contains("--fetch"), "{message}");
    // Nothing changed, and the transaction can still go forward.
    assert!(
        !crate::transaction_status(&f.root)?
            .pending
            .unwrap()
            .aborting
    );
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor 2");
    let error = abort_transaction(&f.root, true).unwrap_err();
    assert!(format!("{error:#}").contains("no TUF mirror"), "{error:#}");
    fs::rename(&offline, &f.repo)?;
    assert_eq!(abort_transaction(&f.root, false)?, Some(1));
    assert_committed_release(&f)?;

    // Restoring vendor configuration discards the local edit, which no
    // payload holds, so only resume can finish that transaction.
    let revert = plan(
        &f.root,
        Request::Revert {
            paths: vec![CONFIG.into()],
        },
    )?;
    interrupt_apply_at(&f.root, &saved, &revert, CONFIG)?;
    let error = abort_transaction(&f.root, false).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(&format!(
            "{CONFIG}: local contents sha256 {} that the transaction replaced",
            digest(b"administrator edit")
        )),
        "{message}"
    );
    assert!(
        message.contains("finish the transaction with resume"),
        "{message}"
    );
    // Putting the contents back from a backup lets the abort finish.
    fs::write(f.root.join(CONFIG), "administrator edit")?;
    assert_eq!(abort_transaction(&f.root, false)?, Some(1));
    assert_committed_release(&f)?;
    Ok(())
}

#[test]
fn writes_follow_dependencies_and_removals_run_in_reverse() -> Result<()> {
    let f = Fixture::new()?;
    // The application sorts first by name but needs the library's capability.
    let app = "    require-capability \"cap/display-abi\"\n".to_owned()
        + &f.file("usr/bin/app", "app", "0755", "")?
        + &f.file("usr/share/app/data", "data", "0644", "")?
        + &f.file("usr/lib/app/plugin.so", "plugin", "0644", "")?;
    f.package("a/app", "1.0.0-0", app)?;
    let library = "    provides \"cap/display-abi\"\n".to_owned()
        + &f.file("usr/lib/libz.so.1", "library", "0755", "")?
        + &f.file("usr/share/z/data", "data", "0644", "")?
        + &f.file("usr/libexec/z-helper", "helper", "0644", "")?;
    f.package("z/lib", "1.0.0-0", library)?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["a/app".into()],
        },
    )?;
    let order = [
        "usr/lib/libz.so.1",
        "usr/share/z/data",
        "usr/libexec/z-helper",
        "usr/lib/app/plugin.so",
        "usr/share/app/data",
        "usr/bin/app",
    ];
    let positions: Vec<_> = order.iter().map(|path| position(&install, path)).collect();
    assert!(positions.is_sorted(), "{:?}", install.writes);
    // Directories exist before anything is written into them.
    assert!(position(&install, "usr/lib") < positions[0]);
    apply(&f.root, &install)?;
    let removal = plan(
        &f.root,
        Request::Remove {
            packages: vec!["a/app".into(), "z/lib".into()],
        },
    )?;
    let positions: Vec<_> = ["usr/bin/app", "usr/lib/app/plugin.so", "usr/lib/libz.so.1"]
        .iter()
        .map(|path| position(&removal, path))
        .collect();
    assert!(positions.is_sorted(), "{:?}", removal.writes);
    assert!(position(&removal, "usr/lib/app") > positions[2]);
    Ok(())
}
