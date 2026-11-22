//! The shapes of the records that CPS persists: saved plans, the pending
//! journal, image state rows, and history. A change to any of them must
//! raise that record's format and keep older records readable, so a
//! mismatch here is a reminder, not a fixture to refresh casually. Run with
//! `CPS_UPDATE_GOLDEN=1` to rewrite the files under `tests/golden/records`
//! after such a change.

use std::collections::BTreeSet;

use anyhow::Context;
use serde_json::Value;

use super::*;

/// Every leaf of a JSON document as `path: type`. Arrays collapse to `[]`,
/// and an object that names its variant in a `kind` or `type` field shows
/// it, so a renamed field or variant changes the shape.
fn shape(value: &Value) -> String {
    fn walk(value: &Value, path: String, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                let tag = ["kind", "type"]
                    .iter()
                    .find_map(|key| Some((key, object.get(*key)?.as_str()?)))
                    .map(|(key, value)| format!("{{{key}={value}}}"))
                    .unwrap_or_default();
                if object.is_empty() {
                    out.insert(format!("{path}{tag}: {{}}"));
                }
                for (key, child) in object {
                    walk(child, format!("{path}{tag}.{key}"), out);
                }
            }
            Value::Array(items) => {
                if items.is_empty() {
                    out.insert(format!("{path}[]: empty"));
                }
                for item in items {
                    walk(item, format!("{path}[]"), out);
                }
            }
            Value::Null => {
                out.insert(format!("{path}: null"));
            }
            Value::Bool(_) => {
                out.insert(format!("{path}: bool"));
            }
            Value::Number(_) => {
                out.insert(format!("{path}: number"));
            }
            Value::String(_) => {
                out.insert(format!("{path}: string"));
            }
        }
    }
    let mut paths = BTreeSet::new();
    walk(value, String::new(), &mut paths);
    paths.into_iter().map(|path| path + "\n").collect()
}

fn check_golden(name: &str, actual: &str) -> Result<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/records")
        .join(name);
    if std::env::var_os("CPS_UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, actual)?;
        return Ok(());
    }
    let expected = fs::read_to_string(&path)
        .with_context(|| format!("reading {}; set CPS_UPDATE_GOLDEN=1", path.display()))?;
    assert!(
        expected == actual,
        "the persisted shape of {name} changed. Raise the record's format and keep \
         older records readable, then set CPS_UPDATE_GOLDEN=1.\n--- expected\n{expected}\n--- actual\n{actual}"
    );
    Ok(())
}

#[test]
fn persisted_records_keep_their_shapes() -> Result<()> {
    let f = Fixture::new()?;
    let mut actions = f.file("usr/bin/tool", "tool", "0755", "")?;
    actions += &f.file("etc/tool.conf", "setting", "0644", "preserve=#true")?;
    actions += "    dir \"usr/share/tool\" mode=\"0755\"\n";
    actions += "    symlink \"usr/bin/tool-link\" target=\"tool\"\n";
    actions += "    hardlink \"usr/bin/tool-alias\" target=\"usr/bin/tool\"\n";
    f.package("test/records", "1.0.0-0", actions)?;
    f.create()?;
    let request = Request::Install {
        packages: vec!["test/records".into()],
    };
    let install = plan(&f.root, request)?;
    check_golden("plan.shape", &shape(&serde_json::to_value(&install)?))?;

    let base = crate::image::Image::open_query(&f.root)?.state()?;
    apply(&f.root, &install)?;
    let next = crate::image::Image::open_query(&f.root)?.state()?;
    check_golden("state.shape", &shape(&serde_json::to_value(&next)?))?;
    check_golden(
        "pending.shape",
        &shape(&crate::apply::sample_pending_record(
            install.clone(),
            &base,
            &next,
        )?),
    )?;
    let history = crate::transaction_history(&f.root, 1)?;
    check_golden("history.shape", &shape(&serde_json::to_value(&history)?))?;

    // Every request a plan or history entry can carry, except a lock, whose
    // document has its own format.
    let requests = [
        Request::Install {
            packages: vec!["a".into()],
        },
        Request::InstallBuildDeps {
            packages: vec!["a".into()],
        },
        Request::ExactInstall {
            packages: vec!["a".into()],
        },
        Request::InstallProfile,
        Request::Mark {
            choice: crate::MarkChoice::Explicit,
            packages: vec!["a".into()],
        },
        Request::Mark {
            choice: crate::MarkChoice::Automatic,
            packages: vec!["a".into()],
        },
        Request::Freeze {
            packages: vec!["a".into()],
            reason: Some("pinned".into()),
        },
        Request::Unfreeze {
            packages: vec!["a".into()],
        },
        Request::Remove {
            packages: vec!["a".into()],
        },
        Request::Autoremove,
        Request::ChangeFacet {
            facets: BTreeMap::from([("doc".into(), Some(true)), ("devel".into(), None)]),
            with_dependencies: true,
        },
        Request::ChangeVariant {
            variants: BTreeMap::from([("gpu".into(), Some("amd".into()))]),
        },
        Request::SetMediator {
            group: "sh".into(),
            provider: Some("system/bash".into()),
        },
        Request::Repair {
            packages: vec!["a".into()],
        },
        Request::RepairPaths {
            paths: vec!["usr/bin/a".into()],
        },
        Request::Revert {
            paths: vec!["etc/a.conf".into()],
        },
        Request::ResolveConfig {
            path: "etc/a.conf".into(),
            take: crate::ConfigChoice::Local,
        },
        Request::ResolveConfig {
            path: "etc/a.conf".into(),
            take: crate::ConfigChoice::Vendor,
        },
        Request::Update,
        Request::UpdateProfile,
    ];
    let mut text = String::new();
    for request in &requests {
        text += &serde_json::to_string(request)?;
        text.push('\n');
    }
    check_golden("requests.json", &text)?;
    Ok(())
}

#[test]
fn records_from_another_version_are_reported_not_fatal() -> Result<()> {
    let f = Fixture::new()?;
    let actions = f.file("usr/bin/tool", "tool", "0755", "")?;
    f.package("test/records", "1.0.0-0", actions)?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["test/records".into()],
        },
    )?;
    apply(&f.root, &install)?;
    let db = rusqlite::Connection::open(f.root.join("usr/lib/sysimage/cps/state.sqlite"))?;

    // Details with a field this version does not know still list the
    // transaction and its operation.
    let (plan_id, revision, data): (String, u64, String) = db.query_row(
        "SELECT h.plan, h.revision, d.data FROM history h JOIN transaction_details d ON d.plan=h.plan",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let mut future: Value = serde_json::from_str(&data)?;
    future["format"] = 2.into();
    future["recorded_by"] = "a later cps".into();
    let future = serde_json::to_string(&future)?;
    db.execute(
        "UPDATE transaction_details SET data=?1, sha256=?2",
        (
            &future,
            crate::image::history::checksum(&plan_id, revision, &future),
        ),
    )?;
    let history = crate::transaction_history(&f.root, 10)?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].operation, "install");
    assert!(history[0].details.is_none());

    // A journal in another format names itself instead of failing to decode.
    db.execute(
        "INSERT INTO pending VALUES (1, ?1)",
        [r#"{"journaled":{"format":2,"id":"abc","plan":{"new":"layout"}}}"#],
    )?;
    let error = crate::transaction_status(&f.root).unwrap_err();
    assert!(
        format!("{error:#}").contains(
            "pending transaction abc was journaled in format 2 and this cps reads format 1"
        ),
        "{error:#}"
    );
    Ok(())
}
