use super::*;
use crate::{
    kdl_format::{manifest_text, parse_manifest},
    triggers::TriggerRun,
};

fn trigger(name: &str, body: &str) -> String {
    format!("    trigger {name:?} {{\n{body}    }}\n")
}

fn handler(f: &Fixture, path: &str) -> Result<String> {
    f.file(path, "#!/bin/sh\n", "0755", "")
}

fn runs(plan: &Plan) -> Vec<String> {
    plan.triggers.iter().map(TriggerRun::label).collect()
}

fn install_plan(root: &Path, packages: &[&str]) -> Result<Plan> {
    plan(
        root,
        Request::Install {
            packages: packages.iter().map(|name| (*name).to_owned()).collect(),
        },
    )
}

#[test]
fn trigger_declarations_round_trip_and_validate() -> Result<()> {
    let manifest = r#"schema-version 1 kind="package-manifest"
package "desktop/glib" publisher="canopa" version="2.86.0-0" {
    dir "usr/share/glib-2.0/schemas" mode="0755" uid=0 gid=0
    file "usr/bin/glib-compile-schemas" mode="0755" uid=0 gid=0
    trigger "schemas" timeout=60 {
        run "usr/bin/glib-compile-schemas" "usr/share/glib-2.0/schemas"
        watch "usr/share/glib-2.0/schemas/*.xml"
        watch "usr/share/glib-2.0/schemas/*.override"
        output "usr/share/glib-2.0/schemas/gschemas.compiled"
        restart "example.service"
    }
}
"#;
    let parsed = parse_manifest(manifest)?;
    let declared = &parsed.triggers[0];
    assert_eq!(
        declared.handler.as_deref(),
        Some("usr/bin/glib-compile-schemas")
    );
    assert_eq!(declared.args, ["usr/share/glib-2.0/schemas"]);
    assert_eq!(declared.timeout, Some(60));
    assert_eq!(parse_manifest(&manifest_text(&parsed))?, parsed);

    let compile = r#"file "usr/bin/glib-compile-schemas" mode="0755""#;
    let run = r#"run "usr/bin/glib-compile-schemas" "usr/share/glib-2.0/schemas""#;
    let output = r#"output "usr/share/glib-2.0/schemas/gschemas.compiled""#;
    let header = r#"trigger "schemas" timeout=60"#;
    let invalid = [
        (
            compile,
            r#"file "usr/bin/glib-compile-schemas" mode="0644""#,
            "must be executable",
        ),
        (
            output,
            r#"output "usr/share/glib-2.0/schemas""#,
            "covers the packaged path",
        ),
        (
            output,
            r#"output "usr/lib/sysimage/cps/cache""#,
            "covers reserved image state",
        ),
        (
            output,
            r#"output "usr/lib/cache""#,
            "outside usr, usr/lib, usr/lib/sysimage",
        ),
        (output, r#"output "var/cache/fontconfig""#, "is under /var"),
        (
            r#"watch "usr/share/glib-2.0/schemas/*.xml""#,
            r#"watch "var/lib/schemas/*.xml""#,
            "under /var",
        ),
        (output, r#"output "cache""#, "below the image root"),
        (
            r#"watch "usr/share/glib-2.0/schemas/*.xml""#,
            r#"watch "usr/share/**x""#,
            "invalid path pattern",
        ),
        (
            r#"restart "example.service""#,
            r#"restart "example.socket""#,
            "only reload or restart services",
        ),
        (
            header,
            r#"trigger "schemas" timeout=0"#,
            "timeout must be between",
        ),
        (header, r#"trigger "Schemas""#, "invalid trigger name"),
        (
            header,
            r#"trigger "schemas" facet="devel""#,
            "triggers cannot carry facet selectors",
        ),
        (
            run,
            r#"run "usr/bin/missing""#,
            "requires an immutable file action",
        ),
        (
            run,
            r#"run "usr/bin/glib-compile-schemas" arch="x86_64""#,
            "triggers cannot carry arch selectors",
        ),
        (run, "", "needs a run handler"),
    ];
    for (from, to, message) in invalid {
        assert!(manifest.contains(from), "{from}");
        let error = parse_manifest(&manifest.replacen(from, to, 1))
            .and_then(|manifest| manifest.validate(false).map(|()| manifest))
            .expect_err(to);
        assert!(format!("{error:#}").contains(message), "{to}: {error:#}");
    }
    let actuator = manifest
        .replace(
            "        run \"usr/bin/glib-compile-schemas\" \"usr/share/glib-2.0/schemas\"\n",
            "",
        )
        .replace(
            "        output \"usr/share/glib-2.0/schemas/gschemas.compiled\"\n",
            "",
        );
    parse_manifest(&actuator)?.validate(false)?;
    Ok(())
}

/// A cache trigger and a trigger that summarizes the cache.
fn cache_fixture() -> Result<Fixture> {
    let f = Fixture::new()?;
    let owner = format!("uid={} gid={}", geteuid().as_raw(), getegid().as_raw());
    f.package(
        "test/cache",
        "1.0.0-0",
        format!("    dir \"usr/share/items\" mode=\"0755\" {owner}\n")
            + &handler(&f, "usr/libexec/cache")?
            + &trigger(
                "index",
                "        run \"usr/libexec/cache\"\n        watch \"usr/share/items/*.item\"\n        output \"usr/share/items/index\"\n",
            ),
    )?;
    f.package(
        "test/summary",
        "1.0.0-0",
        handler(&f, "usr/libexec/summary")?
            + &trigger(
                "summary",
                "        run \"usr/libexec/summary\"\n        watch \"usr/share/items/index\"\n        output \"usr/share/summary/count\"\n",
            ),
    )?;
    f.package(
        "test/item",
        "1.0.0-0",
        f.file("usr/share/items/a.item", "a", "0644", "")?,
    )?;
    f.package(
        "test/other",
        "1.0.0-0",
        f.file("usr/share/other", "other", "0644", "")?,
    )?;
    f.create()?;
    Ok(f)
}

#[test]
fn triggers_run_once_in_data_flow_order() -> Result<()> {
    let f = cache_fixture()?;
    let install = install_plan(&f.root, &["test/summary", "test/cache", "test/item"])?;
    assert_eq!(runs(&install), ["test/cache:index", "test/summary:summary"]);
    assert!(install.retired_triggers.is_empty());
    assert!(
        install
            .summary()
            .contains("Run trigger test/cache:index\nRun trigger test/summary:summary")
    );
    assert!(install.boot_reason(&Default::default()).is_none());
    let saved: Plan = serde_json::from_str(&serde_json::to_string(&install)?)?;
    assert_eq!(saved, install);

    let item = install_plan(&f.root, &["test/item"])?;
    assert!(runs(&item).is_empty(), "no trigger is installed yet");
    let unrelated = install_plan(&f.root, &["test/other"])?;
    assert!(runs(&unrelated).is_empty());
    Ok(())
}

#[test]
fn package_paths_and_overlapping_outputs_conflict() -> Result<()> {
    let f = cache_fixture()?;
    f.package(
        "test/shipped-index",
        "1.0.0-0",
        f.file("usr/share/items/index", "stale", "0644", "")?,
    )?;
    let error = install_plan(&f.root, &["test/cache", "test/shipped-index"]).unwrap_err();
    assert!(
        format!("{error:#}").contains(
            "path conflict for usr/share/items/index between test/shipped-index and trigger test/cache:index"
        ),
        "{error:#}"
    );

    f.package(
        "test/second-cache",
        "1.0.0-0",
        handler(&f, "usr/libexec/second")?
            + &trigger(
                "index",
                "        run \"usr/libexec/second\"\n        watch \"usr/share/items/*\"\n        output \"usr/share/items/ind*\"\n",
            ),
    )?;
    let error = install_plan(&f.root, &["test/cache", "test/second-cache"]).unwrap_err();
    assert!(format!("{error:#}").contains("both generate"), "{error:#}");
    Ok(())
}

#[test]
fn outputs_beneath_symlinks_are_refused_or_skipped() -> Result<()> {
    let f = Fixture::new()?;
    let (uid, gid) = (geteuid().as_raw(), getegid().as_raw());
    f.package(
        "base/merged",
        "1.0.0-0",
        format!("    dir \"usr/lib\" mode=\"0755\" uid={uid} gid={gid}\n")
            + &format!("    symlink \"lib\" target=\"usr/lib\" uid={uid} gid={gid}\n"),
    )?;
    f.package(
        "test/linked",
        "1.0.0-0",
        handler(&f, "usr/libexec/linked")?
            + &trigger(
                "cache",
                "        run \"usr/libexec/linked\"\n        watch \"usr/share/linked/*\"\n        output \"lib/foo/cache\"\n",
            ),
    )?;
    f.package(
        "test/opt",
        "1.0.0-0",
        format!("    dir \"opt\" mode=\"0755\" uid={uid} gid={gid}\n")
            + &handler(&f, "usr/libexec/opt")?
            + &trigger(
                "cache",
                "        run \"usr/libexec/opt\"\n        watch \"usr/share/opt/*\"\n        output \"opt/app/cache\"\n",
            ),
    )?;
    f.create()?;
    apply(&f.root, &install_plan(&f.root, &["base/merged"])?)?;
    // A managed symlink in an output's directories refuses the plan.
    let error = install_plan(&f.root, &["test/linked"]).unwrap_err();
    assert!(
        format!("{error:#}").contains(
            "trigger test/linked:cache output lib/foo/cache lies beneath the symbolic link lib"
        ),
        "{error:#}"
    );

    // An unmanaged symlink fails the handler, and the abort still finishes.
    fs::create_dir(f.root.join("opt"))?;
    std::os::unix::fs::symlink("../usr/lib", f.root.join("opt/app"))?;
    let install = install_plan(&f.root, &["test/opt"])?;
    let error = apply(&f.root, &install).unwrap_err();
    assert!(
        format!("{error:#}").contains("opt/app is not a directory"),
        "{error:#}"
    );
    assert_eq!(crate::abort_transaction(&f.root, false)?, Some(1));
    assert!(crate::transaction_status(&f.root)?.pending.is_none());
    assert!(fs::symlink_metadata(f.root.join("opt/app"))?.is_symlink());
    Ok(())
}

#[test]
fn trigger_cycles_are_rejected() -> Result<()> {
    let f = Fixture::new()?;
    for (name, watch, output) in [
        ("test/left", "usr/share/right/*", "usr/share/left/out"),
        ("test/right", "usr/share/left/*", "usr/share/right/out"),
    ] {
        let script = format!("usr/libexec/{}", name.trim_start_matches("test/"));
        f.package(
            name,
            "1.0.0-0",
            handler(&f, &script)?
                + &trigger(
                    "loop",
                    &format!(
                        "        run {script:?}\n        watch {watch:?}\n        output {output:?}\n"
                    ),
                ),
        )?;
    }
    f.create()?;
    let error = install_plan(&f.root, &["test/left", "test/right"]).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("trigger cycle: test/left:loop -> test/right:loop -> test/left:loop"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn boot_artifact_outputs_require_staging_and_refresh_the_kernel() -> Result<()> {
    let f = Fixture::new()?;
    let release = "6.0.0-test";
    f.package(
        "system/kernel",
        "1.0.0-0",
        f.file(
            &format!("usr/lib/modules/{release}/vmlinuz"),
            "kernel",
            "0644",
            "",
        )? + &format!("    kernel-install {release:?}\n"),
    )?;
    f.package(
        "system/initramfs",
        "1.0.0-0",
        handler(&f, "usr/libexec/mkinitrd")?
            + &trigger(
                "initramfs",
                "        run \"usr/libexec/mkinitrd\"\n        watch \"usr/lib/modules/**\"\n        watch \"etc/crypttab\"\n        output \"usr/lib/modules/*/initrd\"\n",
            ),
    )?;
    f.package(
        "system/packaged-initrd",
        "1.0.0-0",
        f.file(
            &format!("usr/lib/modules/{release}/vmlinuz"),
            "kernel",
            "0644",
            "",
        )? + &f.file(
            &format!("usr/lib/modules/{release}/initrd"),
            "initrd",
            "0644",
            "",
        )? + &format!("    kernel-install {release:?} initrd=#true\n"),
    )?;
    f.create()?;
    let install = install_plan(&f.root, &["system/kernel", "system/initramfs"])?;
    assert_eq!(runs(&install), ["system/initramfs:initramfs"]);
    assert!(install.triggers[0].boot);
    let artifacts = install.kernel_install.as_ref().expect("kernel preparation");
    assert!(artifacts.generated_initrd);
    assert_eq!(
        install.boot_reason(&Default::default()).as_deref(),
        Some("the transaction changes the boot kernel or initramfs")
    );
    // A prepackaged initramfs is a package path, so a generator cannot cover it.
    let error = install_plan(&f.root, &["system/packaged-initrd", "system/initramfs"]).unwrap_err();
    assert!(format!("{error:#}").contains("path conflict for usr/lib/modules/6.0.0-test/initrd"));
    let packaged = install_plan(&f.root, &["system/packaged-initrd"])?;
    assert!(!packaged.kernel_install.unwrap().generated_initrd);
    Ok(())
}

#[test]
fn live_triggers_reload_running_services_after_they_run() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "system/udev",
        "1.0.0-0",
        trigger(
            "rules",
            "        watch \"usr/lib/udev/rules.d/*.rules\"\n        reload \"systemd-udevd.service\"\n",
        ),
    )?;
    f.package(
        "test/rules",
        "1.0.0-0",
        f.file("usr/lib/udev/rules.d/90-test.rules", "", "0644", "")?,
    )?;
    f.create()?;
    // An actuator-only trigger has nothing to run offline.
    let install = install_plan(&f.root, &["system/udev"])?;
    assert_eq!(runs(&install), ["system/udev:rules"]);
    apply(&f.root, &install)?;
    let (rules, services) = crate::plan::plan_system(
        &f.root,
        Request::Install {
            packages: vec!["test/rules".into()],
        },
        crate::PlanOptions::default(),
        crate::plan::UpdateSelection::default(),
    )?;
    assert_eq!(runs(&rules), ["system/udev:rules"]);
    assert!(rules.boot_reason(&Default::default()).is_none());
    assert_eq!(
        services.preview(),
        ["Reload or restart active services systemd-udevd.service after triggers"]
    );
    // Aborting the live change reloads the same services after its triggers.
    let (_, inverse, base, next) = with_states(
        &f.root,
        Request::Install {
            packages: vec!["test/rules".into()],
        },
    )?;
    assert_eq!(runs(&inverse), ["system/udev:rules"]);
    assert_eq!(
        crate::systemd::LiveServices::plan(&inverse, &next, &base).preview(),
        ["Reload or restart active services systemd-udevd.service after triggers"]
    );
    apply(&f.root, &rules)?;
    assert!(verify::verify(&f.root)?.is_empty());
    Ok(())
}

/// Copy host executables and their shared library closure into the
/// prototype tree, so trigger handlers run inside a test image, and return
/// the file actions that ship them.
fn runtime(f: &Fixture, executables: &[&str]) -> Result<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut actions = String::new();
    for executable in executables {
        let output = Command::new("ldd").arg(executable).output()?;
        anyhow::ensure!(output.status.success(), "ldd {executable} failed");
        let libraries = String::from_utf8(output.stdout)?;
        let paths = std::iter::once(*executable).chain(
            libraries
                .lines()
                .flat_map(|line| line.split_whitespace())
                .filter(|word| word.starts_with('/')),
        );
        for path in paths {
            if !seen.insert(path.to_owned()) {
                continue;
            }
            let relative = path.trim_start_matches('/');
            let destination = f.proto.join(relative);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::copy(path, &destination)?;
            actions += &format!(
                "    file {relative:?} mode=\"0755\" uid={} gid={}\n",
                geteuid().as_raw(),
                getegid().as_raw()
            );
        }
    }
    Ok(actions)
}

/// Whether this host can run trigger handlers under Bubblewrap.
fn sandbox_available() -> bool {
    let available = Command::new("/usr/bin/bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        eprintln!("Skipping: trigger handlers need a working /usr/bin/bwrap");
    }
    available
}

const INDEX: &str = "usr/share/items/index";
const INDEX_RUNS: &str = "usr/share/items/runs";
const COUNT: &str = "usr/share/summary/count";
const COUNT_RUNS: &str = "usr/share/summary/runs";
/// While this exists in the image, the summary handler fails.
const SUMMARY_FAIL: &str = "etc/summary-fail";

/// An index trigger that joins every item, and a summary trigger that
/// counts the index. Both handlers record each run in a second output.
fn running_fixture() -> Result<Fixture> {
    let f = Fixture::new()?;
    let owner = format!("uid={} gid={}", geteuid().as_raw(), getegid().as_raw());
    f.package("test/runtime", "1.0.0-0", runtime(&f, &["/usr/bin/bash"])?)?;
    f.package(
        "test/index",
        "1.0.0-0",
        dependency("require", "test/runtime", "1.0.0-0")
            + &format!("    dir \"usr/share/items\" mode=\"0755\" {owner}\n")
            + &f.file(
                "usr/libexec/index",
                "#!/usr/bin/bash\nset -eu\ncd /usr/share/items\nshopt -s nullglob\n: > index\nfor item in *.item; do printf '%s\\n' \"$(<\"$item\")\" >> index; done\necho run >> runs\n",
                "0755",
                "",
            )?
            + &trigger(
                "index",
                &format!(
                    "        run \"usr/libexec/index\"\n        watch \"usr/share/items/*.item\"\n        output {INDEX:?}\n        output {INDEX_RUNS:?}\n"
                ),
            ),
    )?;
    f.package(
        "test/summary",
        "1.0.0-0",
        dependency("require", "test/index", "1.0.0-0")
            + &format!("    dir \"usr/share/summary\" mode=\"0755\" {owner}\n")
            + &f.file(
                "usr/libexec/summary",
                &format!(
                    "#!/usr/bin/bash\nset -eu\n[[ ! -e /{SUMMARY_FAIL} ]] || exit 42\nmapfile -t lines < /{INDEX}\necho \"${{#lines[@]}}\" > /{COUNT}\necho run >> /{COUNT_RUNS}\n"
                ),
                "0755",
                "",
            )?
            + &trigger(
                "summary",
                &format!(
                    "        run \"usr/libexec/summary\"\n        watch {INDEX:?}\n        output {COUNT:?}\n        output {COUNT_RUNS:?}\n"
                ),
            ),
    )?;
    for (name, item) in [("a", "alpha"), ("b", "bravo")] {
        f.package(
            &format!("test/{name}"),
            "1.0.0-0",
            f.file(&format!("usr/share/items/{name}.item"), item, "0644", "")?,
        )?;
    }
    f.create()?;
    Ok(f)
}

fn read(f: &Fixture, path: &str) -> Result<String> {
    Ok(fs::read_to_string(f.root.join(path))?)
}

fn run_count(f: &Fixture, path: &str) -> Result<usize> {
    Ok(read(f, path)?.lines().count())
}

/// Plan a request and the plan that aborts it from the committed revision.
fn with_inverse(root: &Path, request: Request) -> Result<(Plan, Plan)> {
    let (forward, inverse, _, _) = with_states(root, request)?;
    Ok((forward, inverse))
}

/// A plan, its abort, the committed state, and the planned state.
fn with_states(
    root: &Path,
    request: Request,
) -> Result<(Plan, Plan, crate::image::State, crate::image::State)> {
    let (forward, next, _) =
        crate::plan::plan_image(root, request, crate::PlanOptions::default(), None)?;
    let base = crate::image::Image::open_query(root)?.state()?;
    let inverse = forward.inverse(&base, &next)?;
    Ok((forward, inverse, base, next))
}

/// Leave `plan` pending after its triggers ran, then abort it.
fn abort_after_triggers(f: &Fixture, plan: &Plan) -> Result<()> {
    let saved = f._dir.path().join("forward.json");
    fs::write(&saved, serde_json::to_vec(plan)?)?;
    interrupt_worker(&f.root, &saved, &[("CPS_TEST_INTERRUPT_STEP", "triggers")])?;
    let revision = crate::transaction_status(&f.root)?.revision;
    assert_eq!(crate::abort_transaction(&f.root, false)?, Some(revision));
    assert!(crate::transaction_status(&f.root)?.pending.is_none());
    let findings = verify::verify_pristine(&f.root)?;
    assert!(findings.is_empty(), "{findings:?}");
    Ok(())
}

fn install(packages: &[&str]) -> Request {
    Request::Install {
        packages: packages.iter().map(|name| (*name).to_owned()).collect(),
    }
}

#[test]
fn abort_regenerates_trigger_outputs_for_the_restored_files() -> Result<()> {
    if !sandbox_available() {
        return Ok(());
    }
    let f = running_fixture()?;
    apply(&f.root, &install_plan(&f.root, &["test/index", "test/a"])?)?;
    assert_eq!(read(&f, INDEX)?, "alpha\n");

    // A changed watched path runs the trigger again over the restored files.
    let (forward, inverse) = with_inverse(&f.root, install(&["test/b"]))?;
    assert_eq!(runs(&forward), ["test/index:index"]);
    assert_eq!(runs(&inverse), ["test/index:index"]);
    assert!(inverse.retired_triggers.is_empty());
    abort_after_triggers(&f, &forward)?;
    assert_eq!(read(&f, INDEX)?, "alpha\n");
    assert_eq!(run_count(&f, INDEX_RUNS)?, 3);

    // A trigger that the transaction added goes away with its outputs, and
    // triggers whose inputs did not change do not run.
    let (forward, inverse) = with_inverse(&f.root, install(&["test/summary"]))?;
    assert_eq!(runs(&forward), ["test/summary:summary"]);
    assert!(runs(&inverse).is_empty());
    assert_eq!(
        inverse.retired_triggers,
        [TriggerRun {
            package: "test/summary".into(),
            name: "summary".into(),
            boot: false,
        }]
    );
    abort_after_triggers(&f, &forward)?;
    assert!(!f.root.join("usr/share/summary").exists());
    assert_eq!(run_count(&f, INDEX_RUNS)?, 3);

    // A trigger that the transaction removed runs again for its restored package.
    apply(&f.root, &install_plan(&f.root, &["test/summary"])?)?;
    let (forward, inverse) = with_inverse(
        &f.root,
        Request::Remove {
            packages: vec!["test/summary".into()],
        },
    )?;
    assert!(runs(&forward).is_empty());
    assert_eq!(forward.retired_triggers, inverse_runs(&inverse));
    abort_after_triggers(&f, &forward)?;
    assert_eq!(read(&f, COUNT)?, "1\n");
    assert_eq!(run_count(&f, COUNT_RUNS)?, 1);
    assert_eq!(run_count(&f, INDEX_RUNS)?, 3);
    Ok(())
}

fn inverse_runs(inverse: &Plan) -> Vec<TriggerRun> {
    assert!(inverse.retired_triggers.is_empty());
    inverse.triggers.clone()
}

#[test]
fn interrupted_abort_runs_each_remaining_trigger_once() -> Result<()> {
    if !sandbox_available() {
        return Ok(());
    }
    let f = running_fixture()?;
    let base = apply(
        &f.root,
        &install_plan(&f.root, &["test/summary", "test/a"])?,
    )?;
    let forward = install_plan(&f.root, &["test/b"])?;
    let saved = f._dir.path().join("forward.json");
    fs::write(&saved, serde_json::to_vec(&forward)?)?;
    let finish = |resume: bool| -> Result<Option<u64>> {
        if resume {
            crate::resume_transaction(&f.root)
        } else {
            crate::abort_transaction(&f.root, false)
        }
    };
    let pending_after_triggers = || -> Result<(usize, usize)> {
        interrupt_worker(&f.root, &saved, &[("CPS_TEST_INTERRUPT_STEP", "triggers")])?;
        assert_eq!(read(&f, INDEX)?, "alpha\nbravo\n");
        assert_eq!(read(&f, COUNT)?, "2\n");
        Ok((run_count(&f, INDEX_RUNS)?, run_count(&f, COUNT_RUNS)?))
    };
    let assert_aborted = |runs: (usize, usize), step: &str| -> Result<()> {
        assert!(
            crate::transaction_status(&f.root)?.pending.is_none(),
            "{step}"
        );
        assert_eq!(read(&f, INDEX)?, "alpha\n", "{step}");
        assert_eq!(read(&f, COUNT)?, "1\n", "{step}");
        assert_eq!(
            (run_count(&f, INDEX_RUNS)?, run_count(&f, COUNT_RUNS)?),
            (runs.0 + 1, runs.1 + 1),
            "{step}: each trigger runs once per abort"
        );
        let findings = verify::verify_pristine(&f.root)?;
        assert!(findings.is_empty(), "{step}: {findings:?}");
        Ok(())
    };

    // A worker stops the abort before and after its trigger phase and
    // before each trigger. Either command finishes it.
    let steps = [
        "abort-retired",
        "abort-trigger-0",
        "abort-trigger-1",
        "abort-pruned",
    ];
    for (step, resume) in steps
        .iter()
        .flat_map(|step| [(*step, false), (*step, true)])
    {
        let runs = pending_after_triggers()?;
        interrupt_worker(
            &f.root,
            &saved,
            &[("CPS_TEST_INTERRUPT_STEP", step), ("CPS_TEST_ABORT", "1")],
        )?;
        assert!(
            crate::transaction_status(&f.root)?
                .pending
                .is_some_and(|pending| pending.aborting),
            "{step}"
        );
        assert_eq!(finish(resume)?, Some(base), "{step}");
        assert_aborted(runs, step)?;
    }

    // A failed handler keeps the abort pending after the triggers that finished.
    for resume in [false, true] {
        let runs = pending_after_triggers()?;
        fs::create_dir_all(f.root.join("etc"))?;
        fs::write(f.root.join(SUMMARY_FAIL), "")?;
        let error = crate::abort_transaction(&f.root, false).unwrap_err();
        assert!(
            format!("{error:#}").contains("trigger test/summary:summary failed"),
            "{error:#}"
        );
        assert_eq!(read(&f, INDEX)?, "alpha\n");
        assert_eq!(read(&f, COUNT)?, "2\n");
        fs::remove_file(f.root.join(SUMMARY_FAIL))?;
        fs::remove_dir(f.root.join("etc"))?;
        assert_eq!(finish(resume)?, Some(base));
        assert_aborted(runs, "failed handler")?;
    }
    Ok(())
}

/// A package whose handler writes `cache` into `directory`, and the
/// actions that ship it.
fn cache_generator(f: &Fixture, name: &str, directory: &str) -> Result<String> {
    Ok(dependency("require", "test/runtime", "1.0.0-0")
        + &f.file(
            &format!("usr/libexec/{name}"),
            &format!("#!/usr/bin/bash\nset -eu\necho generated > /{directory}/cache\n"),
            "0755",
            "",
        )?
        + &trigger(
            "cache",
            &format!(
                "        run \"usr/libexec/{name}\"\n        watch \"{directory}/*.item\"\n        output \"{directory}/cache\"\n"
            ),
        ))
}

#[test]
fn transitions_carry_generated_outputs_away_with_the_old_tree() -> Result<()> {
    if !sandbox_available() {
        return Ok(());
    }
    let f = Fixture::new()?;
    let owner = format!("uid={} gid={}", geteuid().as_raw(), getegid().as_raw());
    f.package("test/runtime", "1.0.0-0", runtime(&f, &["/usr/bin/bash"])?)?;
    f.package(
        "test/gen",
        "1.0.0-0",
        format!("    dir \"usr/share/gen\" mode=\"0755\" {owner}\n")
            + &f.file("usr/share/gen/a.item", "a", "0644", "")?
            + &cache_generator(&f, "gen", "usr/share/gen")?,
    )?;
    f.package(
        "test/conf",
        "1.0.0-0",
        f.file("usr/share/conf", "plain", "0644", "")?,
    )?;
    f.create()?;
    apply(&f.root, &install_plan(&f.root, &["test/gen", "test/conf"])?)?;
    assert_eq!(read(&f, "usr/share/gen/cache")?, "generated\n");

    // The directory holding generated outputs becomes a symlink, and the
    // release drops the trigger. The outputs leave with the old tree.
    fs::remove_dir_all(f.proto.join("usr/share/gen"))?;
    f.package(
        "test/gen",
        "2.0.0-0",
        f.file("usr/lib/gen/a.item", "a", "0644", "")?
            + &format!("    symlink \"usr/share/gen\" target=\"../lib/gen\" {owner}\n"),
    )?;
    let update = install_plan(&f.root, &["test/gen@2.0.0-0"])?;
    assert_eq!(update.retired_triggers.len(), 1);
    // Aborting it after its triggers brings the tree and its outputs back.
    abort_after_triggers(&f, &update)?;
    assert_eq!(read(&f, "usr/share/gen/cache")?, "generated\n");
    apply(&f.root, &update)?;
    assert!(fs::symlink_metadata(f.root.join("usr/share/gen"))?.is_symlink());
    assert!(!f.root.join("usr/lib/gen/cache").exists());
    assert!(verify::verify_pristine(&f.root)?.is_empty());

    // A file becomes a directory that a new trigger writes into. Aborting
    // turns the directory, with its outputs, back into the file.
    fs::remove_file(f.proto.join("usr/share/conf"))?;
    f.package(
        "test/conf",
        "2.0.0-0",
        f.file("usr/share/conf/a.item", "a", "0644", "")?
            + &cache_generator(&f, "conf", "usr/share/conf")?,
    )?;
    let update = install_plan(&f.root, &["test/conf@2.0.0-0"])?;
    assert_eq!(runs(&update), ["test/conf:cache"]);
    abort_after_triggers(&f, &update)?;
    assert_eq!(read(&f, "usr/share/conf")?, "plain");
    apply(&f.root, &update)?;
    assert_eq!(read(&f, "usr/share/conf/cache")?, "generated\n");
    assert!(verify::verify_pristine(&f.root)?.is_empty());
    Ok(())
}
