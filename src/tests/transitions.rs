//! Directory and leaf transitions, aborting pending transactions, and the
//! dependency order of writes.

use super::*;

fn position(plan: &Plan, path: &str) -> usize {
    plan.writes
        .iter()
        .position(|write| write.path == path)
        .unwrap_or_else(|| panic!("plan does not write {path}"))
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
