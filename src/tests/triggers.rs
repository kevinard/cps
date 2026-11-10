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
            r#"output "var/lib/cps/image/cache""#,
            "covers reserved image state",
        ),
        (
            output,
            r#"output "var/lib/cache""#,
            "outside var and var/lib",
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
    apply(&f.root, &rules)?;
    assert!(verify::verify(&f.root)?.is_empty());
    Ok(())
}
