use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Result;
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use rustix::process::{getegid, geteuid};

use crate::{
    Plan, Request, apply,
    apply::apply_with,
    create_image,
    model::{Node, digest},
    plan, publish, verify,
};

const CONFIG: &str = "etc/canopa/compositor.conf";
const COMPOSITOR: &str = "usr/bin/canopa-compositor";
const PLUGIN: &str = "usr/lib/canopa/plugin.so";
const LIBRARY: &str = "usr/lib/libdisplay.so";
const MANPAGE: &str = "usr/share/man/man1/canopa-compositor.1";
const INTERRUPTED: i32 = 86;

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    root: PathBuf,
    proto: PathBuf,
    recipe: PathBuf,
    private: PathBuf,
    public: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let path = dir.path();
        let proto = path.join("proto");
        fs::create_dir(&proto)?;
        let private = path.join("private.der");
        let public = path.join("public.der");
        let random = SystemRandom::new();
        let key = Ed25519KeyPair::generate_pkcs8(&random).expect("generate key");
        fs::write(&private, key.as_ref())?;
        let key = Ed25519KeyPair::from_pkcs8(key.as_ref()).expect("read generated key");
        let mut spki = hex::decode("302a300506032b6570032100")?;
        spki.extend_from_slice(key.public_key().as_ref());
        fs::write(&public, spki)?;
        Ok(Self {
            repo: path.join("repo"),
            root: path.join("image"),
            recipe: path.join("manifest.kdl"),
            proto,
            private,
            public,
            _dir: dir,
        })
    }

    fn package(&self, name: &str, version: &str, actions: String) -> Result<()> {
        let manifest = format!(
            "schema-version 1\npackage {name:?} publisher=\"canopa\" version={version:?} {{\n{actions}}}\n"
        );
        fs::write(&self.recipe, manifest)?;
        publish(&self.repo, &self.proto, &self.recipe, &self.private)?;
        Ok(())
    }

    fn file(&self, path: &str, bytes: &str, mode: &str, extra: &str) -> Result<String> {
        let output = self.proto.join(path);
        fs::create_dir_all(output.parent().unwrap())?;
        fs::write(output, bytes)?;
        let uid = geteuid().as_raw();
        let gid = getegid().as_raw();
        Ok(format!(
            "    file {path:?} mode={mode:?} uid={uid} gid={gid} {extra}\n"
        ))
    }

    fn create(&self) -> Result<()> {
        create_image(
            &self.root,
            &self.repo,
            "canopa",
            &self.public,
            "x86_64",
            BTreeMap::from([("doc.man".to_owned(), false)]),
        )?;
        Ok(())
    }
}

fn dependency(kind: &str, name: &str, version: &str) -> String {
    format!("    {kind} {name:?} version=\"={version}\"\n")
}

#[test]
fn solver_honors_exclusions_and_explains_impossible_requests() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "library/libdisplay",
        "1.0.0-0",
        f.file(LIBRARY, "ABI 1", "0644", "")?,
    )?;
    f.package(
        "library/libdisplay",
        "2.0.0-0",
        f.file(LIBRARY, "ABI 2", "0644", "")?,
    )?;
    f.package(
        "desktop/app",
        "1.0.0-0",
        "    require \"library/libdisplay\"\n".into(),
    )?;
    f.package(
        "policy/desktop",
        "1.0.0-0",
        concat!(
            "    exclude \"library/libdisplay\" version=\">=2.0.0-0\"\n",
            "    exclude \"desktop/experimental\"\n",
        )
        .into(),
    )?;
    f.package("desktop/experimental", "1.0.0-0", String::new())?;
    f.package(
        "desktop/plugin",
        "1.0.0-0",
        dependency("require", "library/libdisplay", "2.0.0-0"),
    )?;
    f.create()?;
    let install = |names: &[&str]| Request::Install {
        packages: names.iter().map(|name| name.to_string()).collect(),
    };
    let selected = plan(&f.root, install(&["desktop/app", "policy/desktop"]))?;
    apply(&f.root, &selected)?;
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "ABI 1");

    for name in ["desktop/plugin", "desktop/experimental"] {
        let error = plan(&f.root, install(&[name])).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("cannot satisfy package constraints"),
            "{message}"
        );
        assert!(
            message.contains(name) && message.contains("policy/desktop"),
            "{message}"
        );
    }

    f.package("policy/desktop", "2.0.0-0", String::new())?;
    let update = plan(&f.root, Request::Update)?;
    apply(&f.root, &update)?;
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "ABI 2");
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn applies_legacy_plan_with_its_pinned_catalog() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "desktop/app",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    f.create()?;
    let legacy = {
        let image = crate::image::Image::open(&f.root, false)?;
        let request = Request::Install {
            packages: vec!["desktop/app".into()],
        };
        crate::plan::build(&image.root, image.state()?, request, None, None)?.0
    };
    let saved = serde_json::to_vec(&legacy)?;
    assert!(
        serde_json::from_slice::<serde_json::Value>(&saved)?
            .get("solver")
            .is_none()
    );
    f.package(
        "desktop/app",
        "2.0.0-0",
        f.file(COMPOSITOR, "new", "0755", "")?,
    )?;
    apply(&f.root, &serde_json::from_slice(&saved)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    Ok(())
}

#[test]
fn requires_one_compatible_backend_and_checks_removal_offline() -> Result<()> {
    let f = Fixture::new()?;
    const ALPHA: &str = "usr/lib/cps/alpha";
    const ZETA: &str = "usr/lib/cps/zeta";
    f.package(
        "backend/alpha",
        "1.0.0-0",
        f.file(ALPHA, "alpha v1", "0644", "")?,
    )?;
    f.package(
        "backend/zeta",
        "2.0.0-0",
        f.file(ZETA, "zeta v2", "0644", "")?,
    )?;
    f.package(
        "desktop/app",
        "1.0.0-0",
        concat!(
            "    require-any {\n",
            "        package \"backend/alpha\" version=\">=2.0.0-0\"\n",
            "        package \"backend/zeta\" version=\"=2.0.0-0\"\n",
            "        package \"backend/missing\"\n",
            "    }\n",
        )
        .into(),
    )?;
    f.package(
        "policy/desktop",
        "1.0.0-0",
        "    exclude \"backend/alpha\"\n".into(),
    )?;
    f.create()?;
    let install = |names: &[&str]| Request::Install {
        packages: names.iter().map(|name| name.to_string()).collect(),
    };
    let remove_zeta = || Request::Remove {
        packages: vec!["backend/zeta".into()],
    };
    apply(&f.root, &plan(&f.root, install(&["backend/zeta"]))?)?;
    f.package(
        "backend/alpha",
        "2.0.0-0",
        f.file(ALPHA, "alpha v2", "0644", "")?,
    )?;
    apply(&f.root, &plan(&f.root, install(&["desktop/app"]))?)?;
    assert!(
        !f.root.join(ALPHA).exists(),
        "an installed backend already satisfies the dependency"
    );

    let fresh = f._dir.path().join("fresh");
    create_image(
        &fresh,
        &f.repo,
        "canopa",
        &f.public,
        "x86_64",
        BTreeMap::new(),
    )?;
    apply(
        &fresh,
        &plan(&fresh, install(&["desktop/app", "policy/desktop"]))?,
    )?;
    assert!(fresh.join(ZETA).is_file());
    assert!(!fresh.join(ALPHA).exists());

    let offline = f._dir.path().join("offline-repo");
    fs::rename(&f.repo, &offline)?;
    let error = format!("{:#}", plan(&f.root, remove_zeta()).unwrap_err());
    assert!(
        error.contains("requires one of") && error.contains("backend/zeta"),
        "{error}"
    );
    fs::rename(&offline, &f.repo)?;
    apply(&f.root, &plan(&f.root, install(&["backend/alpha"]))?)?;
    fs::rename(&f.repo, &offline)?;
    apply(&f.root, &plan(&f.root, remove_zeta())?)?;
    assert!(!f.root.join(ZETA).exists());
    assert_eq!(fs::read_to_string(f.root.join(ALPHA))?, "alpha v2");
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn conditional_dependency_tracks_the_selected_runtime_version() -> Result<()> {
    let f = Fixture::new()?;
    const RUNTIME: &str = "usr/lib/cps/runtime";
    f.package(
        "desktop/app",
        "1.0.0-0",
        concat!(
            "    conditional \"desktop/plugin\" version=\"=2.0.0-0\" ",
            "when=\"runtime/desktop\" when-version=\">=2.0.0-0\"\n",
        )
        .into(),
    )?;
    f.package(
        "runtime/desktop",
        "1.0.0-0",
        f.file(RUNTIME, "runtime v1", "0644", "")?,
    )?;
    f.package(
        "desktop/plugin",
        "1.0.0-0",
        f.file(PLUGIN, "plugin v1", "0644", "")?,
    )?;
    f.create()?;
    let install = |name: &str| Request::Install {
        packages: vec![name.into()],
    };
    apply(&f.root, &plan(&f.root, install("desktop/app"))?)?;
    assert!(!f.root.join(RUNTIME).exists());
    assert!(!f.root.join(PLUGIN).exists());

    apply(&f.root, &plan(&f.root, install("runtime/desktop"))?)?;
    assert_eq!(fs::read_to_string(f.root.join(RUNTIME))?, "runtime v1");
    assert!(!f.root.join(PLUGIN).exists());
    f.package(
        "runtime/desktop",
        "2.0.0-0",
        f.file(RUNTIME, "runtime v2", "0644", "")?,
    )?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(RUNTIME))?, "runtime v1");

    f.package(
        "desktop/plugin",
        "2.0.0-0",
        f.file(PLUGIN, "plugin v2", "0644", "")?,
    )?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(RUNTIME))?, "runtime v2");
    assert_eq!(fs::read_to_string(f.root.join(PLUGIN))?, "plugin v2");

    fs::rename(&f.repo, f._dir.path().join("offline-repo"))?;
    let remove = |names: &[&str]| Request::Remove {
        packages: names.iter().map(|name| name.to_string()).collect(),
    };
    let error = format!(
        "{:#}",
        plan(&f.root, remove(&["desktop/plugin"])).unwrap_err()
    );
    assert!(
        error.contains("requires desktop/plugin =2.0.0-0 when runtime/desktop >=2.0.0-0"),
        "{error}"
    );
    apply(
        &f.root,
        &plan(&f.root, remove(&["desktop/plugin", "runtime/desktop"]))?,
    )?;
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn planner_retries_versions_with_incompatible_file_actions() -> Result<()> {
    let f = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    f.package(
        "base/paths",
        "1.0.0-0",
        format!(
            "{}    symlink \"opt/cps\" target=\"elsewhere\" uid={uid} gid={gid}\n",
            f.file(LIBRARY, "owned by base", "0644", "")?,
        ),
    )?;
    f.package(
        "desktop/app",
        "1.0.0-0",
        f.file(COMPOSITOR, "compatible", "0755", "")?,
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["base/paths".into(), "desktop/app".into()],
        },
    )?;
    apply(&f.root, &install)?;

    f.package(
        "desktop/app",
        "2.0.0-0",
        f.file(LIBRARY, "colliding leaf", "0644", "")?,
    )?;
    f.package(
        "desktop/app",
        "3.0.0-0",
        f.file("opt/cps/child", "under a symlink", "0644", "")?,
    )?;
    f.package(
        "desktop/app",
        "4.0.0-0",
        format!(
            "{}{}",
            f.file(COMPOSITOR, "overlapping actions", "0755", "")?,
            f.file(COMPOSITOR, "overlapping actions", "0755", "arch=\"x86_64\"")?,
        ),
    )?;
    let update = plan(&f.root, Request::Update)?;
    apply(&f.root, &update)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compatible");
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "owned by base");
    assert_eq!(
        fs::read_link(f.root.join("opt/cps"))?,
        Path::new("elsewhere")
    );

    f.package(
        "desktop/blocker",
        "1.0.0-0",
        f.file(LIBRARY, "blocked", "0644", "")?,
    )?;
    let error = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/blocker".into()],
        },
    )
    .unwrap_err();
    let message = format!("{error:#}");
    for detail in [
        "cannot satisfy package constraints",
        LIBRARY,
        "base/paths",
        "desktop/blocker",
    ] {
        assert!(message.contains(detail), "missing {detail} in {message}");
    }

    f.package(
        "desktop/app",
        "5.0.0-0",
        f.file(COMPOSITOR, "next compatible", "0755", "")?,
    )?;
    let update = plan(&f.root, Request::Update)?;
    apply(&f.root, &update)?;
    assert_eq!(
        fs::read_to_string(f.root.join(COMPOSITOR))?,
        "next compatible"
    );
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn install_upgrade_and_resume_offline() -> Result<()> {
    if let (Some(root), Some(saved)) = (
        std::env::var_os("CPS_TEST_IMAGE"),
        std::env::var_os("CPS_TEST_PLAN"),
    ) {
        return interrupt_apply_worker(Path::new(&root), Path::new(&saved));
    }

    let f = Fixture::new()?;
    f.package(
        "library/libdisplay",
        "1.0.0-0",
        f.file(LIBRARY, "display ABI 1", "0644", "")?,
    )?;
    let mut compositor = dependency("require", "library/libdisplay", "1.0.0-0");
    compositor += &f.file(COMPOSITOR, "compositor 1", "0755", "")?;
    compositor += &f.file(CONFIG, "vendor config 1", "0644", "preserve=#true")?;
    compositor += &f.file(MANPAGE, "manpage 1", "0644", "facet=\"doc.man\"")?;
    f.package("desktop/compositor", "1.0.0-0", compositor)?;
    let mut plugin = dependency("require", "desktop/compositor", "1.0.0-0");
    plugin += &f.file(PLUGIN, "plugin 1", "0644", "")?;
    f.package("desktop/plugin", "1.0.0-0", plugin)?;
    f.package(
        "consolidation/desktop",
        "1.0.0-0",
        dependency("incorporate", "desktop/compositor", "1.0.0-0")
            + &dependency("incorporate", "desktop/plugin", "1.0.0-0"),
    )?;
    fs::remove_file(f.repo.join("blobs").join(digest(b"manpage 1")))?;

    f.create()?;
    let initial = plan(
        &f.root,
        Request::Install {
            packages: vec!["consolidation/desktop".into(), "desktop/compositor".into()],
        },
    )?;
    assert_eq!(apply(&f.root, &initial)?, 1);
    assert!(
        !f.root.join(PLUGIN).exists(),
        "incorporate must leave an absent package absent"
    );
    let install_plugin = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/plugin".into()],
        },
    )?;
    assert_eq!(apply(&f.root, &install_plugin)?, 2);
    assert!(verify(&f.root)?.is_empty());
    assert!(!f.root.join(MANPAGE).exists());
    fs::write(f.root.join(CONFIG), "administrator edit")?;

    f.package(
        "library/libdisplay",
        "2.0.0-0",
        f.file(LIBRARY, "display ABI 2", "0644", "")?,
    )?;
    f.package(
        "desktop/settings",
        "2.0.0-0",
        f.file(CONFIG, "vendor config 2", "0644", "preserve=#true")?,
    )?;
    let mut compositor = dependency("require", "library/libdisplay", "2.0.0-0");
    compositor += &dependency("require", "desktop/settings", "2.0.0-0");
    compositor += &f.file(COMPOSITOR, "compositor 2", "0755", "")?;
    compositor += &f.file(MANPAGE, "manpage 2", "0644", "facet=\"doc.man\"")?;
    f.package("desktop/compositor", "2.0.0-0", compositor)?;
    let mut plugin = dependency("require", "desktop/compositor", "2.0.0-0");
    plugin += &f.file(PLUGIN, "plugin 2", "0644", "")?;
    f.package("desktop/plugin", "2.0.0-0", plugin)?;
    f.package(
        "consolidation/desktop",
        "2.0.0-0",
        dependency("incorporate", "desktop/compositor", "2.0.0-0")
            + &dependency("incorporate", "desktop/plugin", "2.0.0-0"),
    )?;
    fs::remove_file(f.repo.join("blobs").join(digest(b"manpage 2")))?;

    let upgrade = plan(&f.root, Request::Update)?;
    let summary = upgrade.summary();
    assert!(summary.contains("Update desktop/plugin 1.0.0-0 -> 2.0.0-0"));
    assert!(summary.contains(&format!("Transfer {CONFIG}")));
    assert!(summary.contains(&format!("Preserve local config {CONFIG}")));
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor 1");
    fs::write(f.root.join(CONFIG), "edited again after plan")?;
    assert!(
        apply(&f.root, &upgrade)
            .unwrap_err()
            .to_string()
            .contains("differs")
    );
    fs::write(f.root.join(CONFIG), "administrator edit")?;

    // Move the repository's current catalog on before apply. The saved plan
    // still targets the same verified set from the earlier catalog.
    f.package(
        "desktop/compositor",
        "3.0.0-0",
        f.file(COMPOSITOR, "later publication", "0755", "")?,
    )?;
    let saved = f._dir.path().join("upgrade.json");
    fs::write(&saved, serde_json::to_vec(&upgrade)?)?;
    let worker = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tests::install_upgrade_and_resume_offline",
            "--nocapture",
        ])
        .env("CPS_TEST_IMAGE", &f.root)
        .env("CPS_TEST_PLAN", &saved)
        .output()?;
    assert_eq!(
        worker.status.code(),
        Some(INTERRUPTED),
        "{}",
        String::from_utf8_lossy(&worker.stderr)
    );
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor 2");
    assert_eq!(fs::read_to_string(f.root.join(PLUGIN))?, "plugin 1");
    assert!(
        verify(&f.root)
            .unwrap_err()
            .to_string()
            .contains("incomplete transaction")
    );
    assert!(
        plan(&f.root, Request::Update)
            .unwrap_err()
            .to_string()
            .contains("incomplete transaction")
    );

    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(apply(&f.root, &upgrade)?, 3);
    assert_eq!(apply(&f.root, &upgrade)?, 3);
    assert_eq!(fs::read_to_string(f.root.join(PLUGIN))?, "plugin 2");
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "display ABI 2");
    assert_eq!(
        fs::read_to_string(f.root.join(CONFIG))?,
        "administrator edit"
    );
    assert_eq!(
        fs::read_to_string(f.root.join(format!(".cps/config-updates/3/{CONFIG}")))?,
        "vendor config 2"
    );
    assert!(!f.root.join(MANPAGE).exists());
    let findings = verify(&f.root)?;
    assert_eq!(findings.len(), 1);
    assert!(!findings[0].is_error);
    assert!(findings[0].message.contains("locally modified config"));

    fs::write(f.root.join(PLUGIN), "unexpected change")?;
    assert!(
        verify(&f.root)?
            .iter()
            .any(|finding| finding.is_error && finding.message.contains(PLUGIN))
    );
    Ok(())
}

fn interrupt_apply_worker(root: &Path, saved: &Path) -> Result<()> {
    let plan: Plan = serde_json::from_slice(&fs::read(saved)?)?;
    apply_with(root, &plan, |write| {
        if write.path == COMPOSITOR && matches!(write.before, Some(Node::File { .. })) {
            std::process::exit(INTERRUPTED);
        }
    })?;
    panic!("fixture did not replace the compositor");
}

#[test]
fn exact_install_and_remove_offline() -> Result<()> {
    const LIB: &str = "usr/lib/libexample.so";
    const APP: &str = "usr/bin/example";
    const EXTRA: &str = "usr/bin/extra";
    const EDITED: &str = "etc/extra.conf";
    let f = Fixture::new()?;
    f.package(
        "library/example",
        "1.0.0-0",
        f.file(LIB, "library", "0644", "")?,
    )?;
    f.package(
        "app/example",
        "1.0.0-0",
        dependency("require", "library/example", "1.0.0-0") + &f.file(APP, "app", "0755", "")?,
    )?;
    f.package(
        "app/extra",
        "1.0.0-0",
        f.file(EXTRA, "extra", "0755", "")?
            + &f.file(EDITED, "vendor", "0644", "preserve=#true")?,
    )?;
    f.create()?;
    let initial = plan(
        &f.root,
        Request::Install {
            packages: vec![
                "app/example".into(),
                "library/example".into(),
                "app/extra".into(),
            ],
        },
    )?;
    apply(&f.root, &initial)?;
    fs::write(f.root.join(EDITED), "local config")?;
    let exact = plan(
        &f.root,
        Request::ExactInstall {
            packages: vec!["app/example".into()],
        },
    )?;
    assert!(exact.summary().contains("Remove app/extra 1.0.0-0"));
    assert!(
        exact
            .summary()
            .contains("Preserve local config etc/extra.conf")
    );
    apply(&f.root, &exact)?;
    assert_eq!(fs::read_to_string(f.root.join(EDITED))?, "local config");
    assert!(!f.root.join(EXTRA).exists());
    assert!(f.root.join(LIB).is_file());
    let image = crate::image::Image::open(&f.root, false)?;
    let installed = image.state()?.installed;
    assert!(installed["app/example"].explicit);
    assert!(!installed["library/example"].explicit);
    drop(image);

    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    let error = plan(
        &f.root,
        Request::Remove {
            packages: vec!["library/example".into()],
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("is missing required package library/example"));
    let remove = plan(
        &f.root,
        Request::Remove {
            packages: vec!["app/example".into(), "library/example".into()],
        },
    )?;
    assert!(remove.catalog.is_none());
    apply(&f.root, &remove)?;
    assert!(!f.root.join(LIB).exists() && !f.root.join(APP).exists());
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

mod release;
