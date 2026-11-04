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
    Plan, Request, apply, apply::apply_with, create_image, model::digest, plan, publish, verify,
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
        self.package_as("canopa", name, version, actions)
    }

    fn package_as(
        &self,
        publisher: &str,
        name: &str,
        version: &str,
        actions: String,
    ) -> Result<()> {
        let manifest = format!(
            "schema-version 1\npackage {name:?} publisher={publisher:?} version={version:?} {{\n{actions}}}\n"
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
fn generated_manifest_is_deterministic_and_installs_the_prototype() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let f = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let generate =
        |proto| crate::generate_manifest(proto, "canopa", "test/proto", "1.0.0-0", uid, gid);
    let prepare = |proto: &Path, reverse: bool| -> Result<()> {
        let directories = if reverse {
            ["var/empty", "usr/bin"]
        } else {
            ["usr/bin", "var/empty"]
        };
        for path in directories {
            let directory = proto.join(path);
            fs::create_dir_all(&directory)?;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o751))?;
        }
        let payload = proto.join("usr/bin/tool #1");
        fs::write(&payload, "tool payload")?;
        fs::set_permissions(&payload, fs::Permissions::from_mode(0o755))?;
        symlink("tool #1", proto.join("usr/bin/link"))?;
        symlink("/outside-the-prototype", proto.join("external"))?;
        Ok(())
    };
    let other = f.proto.with_extension("other");
    prepare(&f.proto, false)?;
    prepare(&other, true)?;
    let manifest = generate(&f.proto)?;
    assert_eq!(manifest, generate(&other)?);
    let parsed = crate::kdl_format::parse_manifest(&manifest)?;
    parsed.validate(false)?;
    assert_eq!(parsed.action.len(), 7);

    fs::write(&f.recipe, manifest)?;
    publish(&f.repo, &f.proto, &f.recipe, &f.private)?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["test/proto".into()],
        },
    )?;
    apply(&f.root, &install)?;
    assert!(verify(&f.root)?.is_empty());
    assert_eq!(
        fs::read_to_string(f.root.join("usr/bin/link"))?,
        "tool payload"
    );
    assert_eq!(
        fs::read_link(f.root.join("external"))?,
        Path::new("/outside-the-prototype")
    );
    assert!(fs::read_dir(f.root.join("var/empty"))?.next().is_none());
    assert_eq!(
        fs::metadata(f.root.join("var/empty"))?.permissions().mode() & 0o777,
        0o751
    );

    let file = f.proto.join("usr/bin/tool #1");
    let hardlink = f.proto.join("hardlink");
    fs::hard_link(&file, &hardlink)?;
    assert!(
        generate(&f.proto)
            .unwrap_err()
            .to_string()
            .contains("hard-linked file")
    );
    fs::remove_file(hardlink)?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o4755))?;
    assert!(
        generate(&f.proto)
            .unwrap_err()
            .to_string()
            .contains("special permission bits")
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
    rustix::fs::setxattr(
        &file,
        "user.cps-test",
        b"value",
        rustix::fs::XattrFlags::empty(),
    )?;
    assert!(
        generate(&f.proto)
            .unwrap_err()
            .to_string()
            .contains("extended attributes")
    );
    Ok(())
}

#[test]
fn pristine_verification_checks_the_complete_offline_root() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let f = Fixture::new()?;
    let actions = f.file(COMPOSITOR, "tool", "0755", "")?
        + &f.file(CONFIG, "vendor", "0644", "preserve=#true")?;
    f.package("test/pristine", "1.0.0-0", actions)?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["test/pristine".into()],
        },
    )?;
    apply(&f.root, &install)?;
    assert!(crate::verify_pristine(&f.root)?.is_empty());

    let config = f.root.join(CONFIG);
    fs::write(&config, "local configuration")?;
    let usr = f.root.join("usr");
    fs::set_permissions(&usr, fs::Permissions::from_mode(0o700))?;
    let unowned = f.root.join("unowned");
    fs::create_dir(&unowned)?;
    symlink("/", unowned.join("outside"))?;
    let tool = f.root.join(COMPOSITOR);
    let alias = f.root.with_extension("outside");
    fs::hard_link(&tool, &alias)?;
    rustix::fs::setxattr(
        &tool,
        "user.cps-test",
        b"value",
        rustix::fs::XattrFlags::empty(),
    )?;

    let normal = verify(&f.root)?;
    assert_eq!(normal.len(), 1);
    assert!(!normal[0].is_error);
    let pristine = crate::verify_pristine(&f.root)?;
    assert_eq!(pristine.len(), 5);
    for detail in [
        CONFIG,
        "usr: content, type, or metadata differs",
        "unmanaged path \"unowned\"",
        "unexpected hard link",
        "unexpected extended attributes",
    ] {
        assert!(
            pristine
                .iter()
                .any(|finding| finding.is_error && finding.message.contains(detail))
        );
    }
    fs::remove_file(alias)?;
    rustix::fs::removexattr(&tool, "user.cps-test")?;
    fs::remove_dir_all(unowned)?;
    fs::set_permissions(&usr, fs::Permissions::from_mode(0o755))?;
    fs::write(&config, "vendor")?;
    assert!(crate::verify_pristine(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn plans_pin_ordered_publishers_and_keep_installed_sources() -> Result<()> {
    use crate::kdl_format::parse_manifest;
    use crate::query::{Scope, list_packages, package_manifest};

    let primary = Fixture::new()?;
    let extra = Fixture::new()?;
    primary.package(
        "desktop/compositor",
        "1.0.0-0",
        primary.file(COMPOSITOR, "base compositor 1", "0755", "")?,
    )?;
    extra.package_as(
        "extras",
        "desktop/compositor",
        "9.0.0-0",
        extra.file(COMPOSITOR, "extra compositor", "0755", "")?,
    )?;
    extra.package_as(
        "extras",
        "desktop/plugin",
        "1.0.0-0",
        extra.file(PLUGIN, "extra plugin 1", "0644", "")?
            + "    require \"desktop/compositor\" version=\">=1.0.0-0\"\n",
    )?;
    extra.package_as(
        "extras",
        "desktop/strict",
        "1.0.0-0",
        dependency("require", "desktop/compositor", "9.0.0-0"),
    )?;
    let config = primary._dir.path().join("image.kdl");
    let definition = |extra_key: &Path| {
        format!(
            "schema-version 1\npublisher \"canopa\" {{\n    repository \"repo\"\n    trust-key \"public.der\"\n}}\n\
             publisher \"extras\" {{\n    repository {:?}\n    trust-key {:?}\n}}\n",
            extra.repo, extra_key,
        )
    };
    fs::write(&config, definition(&primary.public))?;
    let wrong_key_root = primary.root.with_extension("wrong-key");
    crate::create_configured_image(&wrong_key_root, &config)?;
    let install = |name: &str| Request::Install {
        packages: vec![name.to_owned()],
    };
    let error = format!(
        "{:#}",
        plan(&wrong_key_root, install("desktop/plugin")).unwrap_err()
    );
    assert!(error.contains("signed catalog for extras") && error.contains("invalid signature"));

    fs::write(&config, definition(&extra.public))?;
    crate::create_configured_image(&primary.root, &config)?;
    let plan_for = |request| plan(&primary.root, request);
    let qualified_root = primary.root.with_extension("qualified");
    crate::create_configured_image(&qualified_root, &config)?;
    let install_qualified = plan(
        &qualified_root,
        Request::ExactInstall {
            packages: vec![
                "cps://extras/desktop/compositor@9.0.0-0".into(),
                "desktop/strict".into(),
            ],
        },
    )?;
    apply(&qualified_root, &install_qualified)?;
    assert_eq!(
        fs::read_to_string(qualified_root.join(COMPOSITOR))?,
        "extra compositor"
    );
    assert!(
        plan_for(install("desktop/compositor@9.0.0-0"))
            .unwrap_err()
            .to_string()
            .contains("no eligible version")
    );
    let error = format!("{:#}", plan_for(install("desktop/strict")).unwrap_err());
    assert!(error.contains("cannot satisfy package constraints"));
    let all = list_packages(&primary.root, Scope::AllAvailableVersions)?;
    assert!(
        all.iter()
            .any(|row| row.id == "cps://extras/desktop/compositor@9.0.0-0")
    );
    let available = list_packages(&primary.root, Scope::Available)?;
    assert_eq!(
        available
            .iter()
            .find(|row| row.name == "desktop/compositor")
            .unwrap()
            .id,
        "cps://canopa/desktop/compositor@1.0.0-0"
    );
    assert_eq!(
        parse_manifest(&package_manifest(
            &primary.root,
            "cps://extras/desktop/compositor",
            true
        )?)?
        .publisher,
        "extras"
    );
    let saved = plan_for(install("desktop/plugin"))?;
    assert_eq!(saved.catalog, None);
    assert_eq!(
        saved
            .catalogs
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["canopa", "extras"]
    );
    assert!(
        saved
            .summary()
            .contains("Install cps://canopa/desktop/compositor 1.0.0-0")
    );
    assert!(
        saved
            .summary()
            .contains("Install cps://extras/desktop/plugin 1.0.0-0")
    );

    primary.package(
        "desktop/compositor",
        "2.0.0-0",
        primary.file(COMPOSITOR, "base compositor 2", "0755", "")?,
    )?;
    primary.package(
        "desktop/plugin",
        "9.0.0-0",
        primary.file(PLUGIN, "base plugin", "0644", "")?,
    )?;
    extra.package_as(
        "extras",
        "desktop/plugin",
        "2.0.0-0",
        extra.file(PLUGIN, "extra plugin 2", "0644", "")?,
    )?;
    let mut tampered = saved.clone();
    tampered.catalogs.remove("extras");
    assert!(
        apply(&primary.root, &tampered)
            .unwrap_err()
            .to_string()
            .contains("catalogs do not match")
    );
    assert!(!primary.root.join(COMPOSITOR).exists());
    assert_eq!(apply(&primary.root, &saved)?, 1);
    assert!(
        plan_for(install("cps://extras/desktop/compositor"))
            .unwrap_err()
            .to_string()
            .contains("sticky publisher canopa")
    );
    assert_eq!(
        fs::read_to_string(primary.root.join(COMPOSITOR))?,
        "base compositor 1"
    );
    assert_eq!(
        fs::read_to_string(primary.root.join(PLUGIN))?,
        "extra plugin 1"
    );
    assert_eq!(
        parse_manifest(&package_manifest(&primary.root, "desktop/plugin", true)?)?.publisher,
        "extras"
    );
    let available = list_packages(&primary.root, Scope::Available)?;
    assert_eq!(
        available
            .iter()
            .find(|row| row.name == "desktop/plugin")
            .unwrap()
            .id,
        "cps://extras/desktop/plugin@2.0.0-0"
    );

    let update = plan_for(Request::Update)?;
    interrupt_apply(
        &primary.root,
        &primary._dir.path().join("update.json"),
        &update,
    )?;
    assert!(
        crate::prefer_publisher(&primary.root, "extras")
            .unwrap_err()
            .to_string()
            .contains("incomplete transaction")
    );
    let base_offline = primary.repo.with_extension("offline");
    let extra_offline = extra.repo.with_extension("offline");
    fs::rename(&primary.repo, &base_offline)?;
    fs::rename(&extra.repo, &extra_offline)?;
    assert_eq!(apply(&primary.root, &update)?, 2);
    assert_eq!(
        fs::read_to_string(primary.root.join(COMPOSITOR))?,
        "base compositor 2"
    );
    assert_eq!(
        fs::read_to_string(primary.root.join(PLUGIN))?,
        "extra plugin 2"
    );
    assert!(verify(&primary.root)?.is_empty());

    // Offline repair planning uses installed manifests. Apply reads the
    // damaged package's payload from its own repository.
    fs::write(primary.root.join(PLUGIN), "damaged")?;
    let repair = plan_for(Request::Repair {
        packages: vec!["desktop/plugin".into()],
    })?;
    fs::rename(&extra_offline, &extra.repo)?;
    assert_eq!(apply(&primary.root, &repair)?, 3);
    assert_eq!(
        fs::read_to_string(primary.root.join(PLUGIN))?,
        "extra plugin 2"
    );
    assert!(verify(&primary.root)?.is_empty());
    Ok(())
}

#[test]
fn exact_version_requests_respect_dependencies_and_allow_deliberate_downgrades() -> Result<()> {
    let f = Fixture::new()?;
    for (version, bytes) in [("1.0.0-0", "old"), ("2.0.0-0", "new")] {
        f.package(
            "desktop/compositor",
            version,
            f.file(COMPOSITOR, bytes, "0755", "")?,
        )?;
    }
    f.package(
        "desktop/plugin",
        "1.0.0-0",
        dependency("require", "desktop/compositor", "2.0.0-0"),
    )?;
    f.create()?;
    let install = |name: &str| Request::Install {
        packages: vec![name.into()],
    };
    let old = install("desktop/compositor@1.0.0-0");
    let plan_for = |request| plan(&f.root, request);
    apply(&f.root, &plan_for(old.clone())?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    apply(&f.root, &plan_for(install("desktop/plugin"))?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "new");
    let error = format!("{:#}", plan_for(old.clone()).unwrap_err());
    assert!(error.contains("cannot satisfy package constraints"));
    let conflicting = plan_for(Request::Install {
        packages: vec![
            "desktop/compositor@1.0.0-0".into(),
            "desktop/compositor@2.0.0-0".into(),
        ],
    })
    .unwrap_err();
    assert!(conflicting.to_string().contains("conflicting selectors"));
    let remove = plan_for(Request::Remove {
        packages: vec!["desktop/plugin".into()],
    })?;
    apply(&f.root, &remove)?;
    apply(&f.root, &plan_for(old)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn publisher_changes_invalidate_plans_without_changing_boot_lineage() -> Result<()> {
    use crate::{add_publisher, image::Image, list_publishers, prefer_publisher, remove_publisher};

    let base = Fixture::new()?;
    let extra = Fixture::new()?;
    base.package(
        "desktop/compositor",
        "1.0.0-0",
        base.file(COMPOSITOR, "base", "0755", "")?,
    )?;
    extra.package_as(
        "extras",
        "desktop/plugin",
        "1.0.0-0",
        extra.file(PLUGIN, "extra", "0644", "")?,
    )?;
    base.create()?;
    let plan_for = |request| plan(&base.root, request);
    let install = |name: &str| Request::Install {
        packages: vec![name.to_owned()],
    };
    apply(&base.root, &plan_for(install("desktop/compositor"))?)?;
    let settings = |root| Image::open(root, false)?.settings();
    let before = settings(&base.root)?;
    let stale = plan_for(Request::Update)?;
    assert!(add_publisher(&base.root, "extras", &extra.repo, &base.public).is_err());
    assert_eq!(settings(&base.root)?, before);
    add_publisher(&base.root, "extras", &extra.repo, &extra.public)?;
    let order = || -> Result<Vec<String>> {
        Ok(list_publishers(&base.root)?
            .into_iter()
            .map(|publisher| publisher.name)
            .collect())
    };
    assert_eq!(order()?, ["canopa", "extras"]);
    assert!(add_publisher(&base.root, "extras", &extra.repo, &extra.public).is_err());
    assert!(
        apply(&base.root, &stale)
            .unwrap_err()
            .to_string()
            .contains("publisher configuration changed")
    );
    let staged = base.root.with_extension("staged");
    assert!(
        crate::stage_boot_environment(&base.root, &staged, &stale)
            .unwrap_err()
            .to_string()
            .contains("publisher configuration changed")
    );
    assert!(!staged.exists());

    prefer_publisher(&base.root, "extras")?;
    assert_eq!(order()?, ["extras", "canopa"]);
    let fresh = plan_for(install("desktop/plugin"))?;
    prefer_publisher(&base.root, "extras")?;
    let current = settings(&base.root)?;
    assert_eq!(
        current.publisher_generation,
        before.publisher_generation + 2
    );
    assert_eq!(current.id, before.id);
    assert_eq!(current.revision, before.revision);
    crate::stage_boot_environment(&base.root, &staged, &fresh)?;
    assert!(!base.root.join(PLUGIN).exists());
    assert_eq!(fs::read_to_string(staged.join(PLUGIN))?, "extra");
    let staged_settings = settings(&staged)?;
    let parent = staged_settings
        .boot_parent
        .expect("staged image has a parent");
    assert_eq!(parent.id, before.id);
    assert_eq!(parent.revision, before.revision);
    assert!(
        remove_publisher(&staged, "extras")
            .unwrap_err()
            .to_string()
            .contains("desktop/plugin is installed")
    );
    fs::rename(&extra.repo, extra.repo.with_extension("offline"))?;
    remove_publisher(&base.root, "extras")?;
    assert_eq!(order()?, ["canopa"]);
    assert!(
        remove_publisher(&base.root, "canopa")
            .unwrap_err()
            .to_string()
            .contains("last publisher")
    );
    assert!(verify(&base.root)?.is_empty());
    assert!(verify(&staged)?.is_empty());
    Ok(())
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
fn queries_signed_catalog_and_committed_installs_independently() -> Result<()> {
    use crate::query::{Scope, Status, list_packages, package_manifest};

    let f = Fixture::new()?;
    f.package(
        "library/libdisplay",
        "1.0.0-0",
        f.file(LIBRARY, "ABI 1", "0644", "")?,
    )?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        dependency("require", "library/libdisplay", "1.0.0-0")
            + &f.file(COMPOSITOR, "installed", "0755", "")?,
    )?;
    f.create()?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec!["desktop/compositor".into()],
            },
        )?,
    )?;
    for version in ["2.0.0-0", "10.0.0-0"] {
        f.package(
            "desktop/compositor",
            version,
            f.file(COMPOSITOR, version, "0755", "")?,
        )?;
    }

    let rows = list_packages(&f.root, Scope::Installed)?;
    assert_eq!(
        rows.iter()
            .map(|row| (row.id.as_str(), row.status))
            .collect::<Vec<_>>(),
        [
            ("cps://canopa/desktop/compositor@1.0.0-0", Status::Explicit),
            (
                "cps://canopa/library/libdisplay@1.0.0-0",
                Status::Dependency
            ),
        ]
    );
    let latest = list_packages(&f.root, Scope::Available)?;
    assert_eq!(latest[0].id, "cps://canopa/desktop/compositor@10.0.0-0");
    assert_eq!(latest[0].status, Status::Available);
    let all = list_packages(&f.root, Scope::AllAvailableVersions)?;
    assert_eq!(
        all.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        [
            "cps://canopa/desktop/compositor@10.0.0-0",
            "cps://canopa/desktop/compositor@2.0.0-0",
            "cps://canopa/desktop/compositor@1.0.0-0",
            "cps://canopa/library/libdisplay@1.0.0-0",
        ]
    );
    let found = package_manifest(&f.root, "cps://canopa/desktop/compositor@2.0.0-0", true)?;
    assert!(found.contains("version=\"2.0.0-0\"") && found.contains(COMPOSITOR));
    assert!(package_manifest(&f.root, "cps://other/desktop/compositor", true).is_err());
    assert!(package_manifest(&f.root, "desktop/compositor@2.0.0-0", false).is_err());

    let catalog = fs::read_to_string(f.repo.join("current"))?;
    fs::write(f.repo.join("catalogs").join(catalog.trim()), b"{}")?;
    assert!(list_packages(&f.root, Scope::Available).is_err());
    fs::rename(&f.repo, f._dir.path().join("offline-repo"))?;
    assert_eq!(list_packages(&f.root, Scope::Installed)?.len(), 2);
    let found = package_manifest(&f.root, "desktop/compositor", false)?;
    assert!(found.contains("version=\"1.0.0-0\"") && found.contains(COMPOSITOR));
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
    interrupt_apply(&f.root, &saved, &upgrade)?;
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

fn interrupt_apply(root: &Path, saved: &Path, plan: &Plan) -> Result<()> {
    interrupt_apply_at(root, saved, plan, COMPOSITOR)
}

fn interrupt_apply_at(root: &Path, saved: &Path, plan: &Plan, path: &str) -> Result<()> {
    fs::write(saved, serde_json::to_vec(plan)?)?;
    let worker = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tests::install_upgrade_and_resume_offline",
            "--nocapture",
        ])
        .env("CPS_TEST_IMAGE", root)
        .env("CPS_TEST_PLAN", saved)
        .env("CPS_TEST_AFTER_PATH", path)
        .output()?;
    assert_eq!(
        worker.status.code(),
        Some(INTERRUPTED),
        "{}",
        String::from_utf8_lossy(&worker.stderr)
    );
    Ok(())
}

fn interrupt_apply_worker(root: &Path, saved: &Path) -> Result<()> {
    let plan: Plan = serde_json::from_slice(&fs::read(saved)?)?;
    let path = std::env::var("CPS_TEST_AFTER_PATH")?;
    apply_with(root, &plan, |write| {
        if write.path == path {
            std::process::exit(INTERRUPTED);
        }
    })?;
    panic!("fixture did not write {path}");
}

#[test]
fn prunes_only_empty_created_directories_and_recovers_after_parent_removal() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let actions = f.file("opt/example/bin/tool", "tool", "0755", "")?
        + &f.file("opt/shared/vendor-file", "vendor", "0644", "")?
        + &f.file("var/example/cache/vendor-file", "vendor", "0644", "")?
        + &format!(
            "    dir \"opt/example\" mode=\"0755\" uid={uid} gid={gid}\n\
             dir \"var/changed-mode\" mode=\"0755\" uid={uid} gid={gid}\n\
             dir \"var/attributes\" mode=\"0755\" uid={uid} gid={gid}\n"
        );
    f.package("test/prune", "1.0.0-0", actions)?;
    f.create()?;
    fs::create_dir_all(f.root.join("opt/shared"))?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["test/prune".into()],
        },
    )?;
    apply(&f.root, &install)?;
    let changed_mode = f.root.join("var/changed-mode");
    fs::set_permissions(&changed_mode, fs::Permissions::from_mode(0o700))?;
    let attributes = f.root.join("var/attributes");
    rustix::fs::setxattr(
        &attributes,
        "user.cps-test",
        b"kept",
        rustix::fs::XattrFlags::empty(),
    )?;
    let local = f.root.join("var/example/cache/local");
    fs::write(&local, "local data")?;

    let request = Request::Remove {
        packages: vec!["test/prune".into()],
    };
    let legacy = {
        let image = crate::image::Image::open(&f.root, false)?;
        crate::plan::build(&image.root, image.state()?, request.clone(), None, None)?.0
    };
    assert!(!legacy.summary().contains("Prune if empty"));
    let removal = plan(&f.root, request)?;
    assert!(removal.summary().contains("Prune if empty opt/example"));
    assert!(!removal.summary().contains("Prune if empty opt/shared"));
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("remove.json"),
        &removal,
        "opt/example",
    )?;
    assert!(!f.root.join("opt/example").exists());
    assert!(verify(&f.root).is_err());
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(apply(&f.root, &removal)?, 2);
    assert_eq!(apply(&f.root, &removal)?, 2);
    assert!(verify(&f.root)?.is_empty());
    assert!(f.root.join("opt/shared").is_dir());
    assert!(!f.root.join("opt/shared/vendor-file").exists());
    assert_eq!(
        fs::metadata(changed_mode)?.permissions().mode() & 0o777,
        0o700
    );
    assert!(attributes.is_dir());
    assert_eq!(fs::read_to_string(local)?, "local data");
    assert!(!f.root.join("var/example/cache/vendor-file").exists());
    Ok(())
}

#[test]
fn repair_installed_files_and_resume_offline() -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    const REMOVED_CONFIG: &str = "etc/canopa/optional.conf";
    const LINK: &str = "usr/bin/compositor";
    const DIR: &str = "usr/bin";
    let f = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let mut actions = f.file(COMPOSITOR, "compositor 1", "0755", "")?;
    actions += &f.file(PLUGIN, "plugin 1", "0644", "")?;
    actions += &f.file(CONFIG, "vendor config", "0644", "preserve=#true")?;
    actions += &f.file(REMOVED_CONFIG, "optional", "0644", "preserve=#true")?;
    actions += &format!("    symlink {LINK:?} target=\"canopa-compositor\" uid={uid} gid={gid}\n");
    actions += &format!("    dir {DIR:?} mode=\"0755\" uid={uid} gid={gid}\n");
    f.package("desktop/compositor", "1.0.0-0", actions)?;
    f.package(
        "library/libdisplay",
        "1.0.0-0",
        f.file(LIBRARY, "display ABI 1", "0644", "")?,
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into(), "library/libdisplay".into()],
        },
    )?;
    assert_eq!(apply(&f.root, &install)?, 1);

    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "newer release", "0755", "")?,
    )?;
    fs::remove_file(f.repo.join("current"))?;
    let dir = f.root.join(DIR);
    let inode = fs::metadata(&dir)?.ino();
    fs::write(dir.join("local"), "local tool")?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o770))?;
    fs::write(f.root.join(COMPOSITOR), "damaged")?;
    fs::set_permissions(f.root.join(COMPOSITOR), fs::Permissions::from_mode(0o600))?;
    fs::remove_dir_all(f.root.join("usr/lib/canopa"))?;
    fs::write(f.root.join(CONFIG), "administrator edit")?;
    fs::remove_file(f.root.join(REMOVED_CONFIG))?;
    fs::write(f.root.join(LIBRARY), "other package drift")?;
    fs::remove_file(f.root.join(LINK))?;
    symlink("unexpected-target", f.root.join(LINK))?;

    let request = Request::Repair {
        packages: vec!["desktop/compositor".into()],
    };
    let repair = plan(&f.root, request.clone())?;
    assert_eq!(repair.catalog, None);
    let summary = repair.summary();
    for path in [COMPOSITOR, PLUGIN, LINK, "usr/lib/canopa"] {
        assert!(summary.contains(&format!("Repair {path}")));
    }
    for path in [CONFIG, REMOVED_CONFIG] {
        assert!(summary.contains(&format!("Preserve local config {path}")));
    }
    assert!(!summary.contains(LIBRARY));

    let blob = f.repo.join("blobs").join(digest(b"plugin 1"));
    fs::write(&blob, "plugin ?")?;
    assert!(
        apply(&f.root, &repair)
            .unwrap_err()
            .to_string()
            .contains("payload digest mismatch")
    );
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "damaged");
    assert!(!f.root.join("usr/lib/canopa").exists());
    assert_eq!(fs::metadata(&dir)?.mode() & 0o777, 0o770);
    fs::write(&blob, "plugin 1")?;
    interrupt_apply(&f.root, &f._dir.path().join("repair.json"), &repair)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor 1");
    assert!(!f.root.join(PLUGIN).exists());
    assert!(verify(&f.root).is_err());
    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    assert_eq!(apply(&f.root, &repair)?, 2);
    assert_eq!(apply(&f.root, &repair)?, 2);
    let metadata = fs::metadata(&dir)?;
    assert_eq!(metadata.ino(), inode);
    assert_eq!(metadata.mode() & 0o777, 0o755);
    assert_eq!(fs::read_to_string(dir.join("local"))?, "local tool");
    assert_eq!(fs::read_to_string(f.root.join(PLUGIN))?, "plugin 1");
    assert_eq!(
        fs::read_link(f.root.join(LINK))?,
        Path::new("canopa-compositor")
    );
    assert_eq!(
        fs::metadata(f.root.join(COMPOSITOR))?.permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_to_string(f.root.join(CONFIG))?,
        "administrator edit"
    );
    assert!(!f.root.join(REMOVED_CONFIG).exists());
    assert!(plan(&f.root, request.clone())?.writes.is_empty());
    let errors: Vec<_> = verify(&f.root)?
        .into_iter()
        .filter(|finding| finding.is_error)
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains(LIBRARY));

    fs::remove_file(f.root.join(COMPOSITOR))?;
    fs::create_dir(f.root.join(COMPOSITOR))?;
    let unrelated = f.root.join(COMPOSITOR).join("local");
    fs::write(&unrelated, "local data")?;
    assert!(
        plan(&f.root, request)
            .unwrap_err()
            .to_string()
            .contains("path type conflict")
    );
    assert_eq!(fs::read_to_string(unrelated)?, "local data");
    fs::remove_dir_all(f.root.join(COMPOSITOR))?;

    let all = plan(
        &f.root,
        Request::Repair {
            packages: Vec::new(),
        },
    )?;
    fs::rename(&offline, &f.repo)?;
    assert_eq!(apply(&f.root, &all)?, 3);
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
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
