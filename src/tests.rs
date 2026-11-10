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
    /// A client's copy of the repository's version 1 trust root, created for
    /// "canopa" and recreated for another publisher before its first
    /// publication.
    trust: PathBuf,
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
        let repo = path.join("repo");
        let trust = path.join("trust.json");
        fs::copy(crate::init_trust(&repo, "canopa", &private, None)?, &trust)?;
        Ok(Self {
            trust,
            repo,
            root: path.join("image"),
            recipe: path.join("manifest.kdl"),
            proto,
            private,
            public,
            _dir: dir,
        })
    }

    /// Make the repository's trust root name `publisher` while it has no catalog.
    fn trust_publisher(&self, publisher: &str) -> Result<()> {
        if self.repo.join("current").exists()
            || crate::show_trust(&self.trust)?.contains(&format!("Publisher: {publisher}\n"))
        {
            return Ok(());
        }
        fs::remove_dir_all(self.repo.join("trust"))?;
        fs::copy(
            crate::init_trust(&self.repo, publisher, &self.private, None)?,
            &self.trust,
        )?;
        Ok(())
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
            "schema-version 1 kind=\"package-manifest\"\npackage {name:?} publisher={publisher:?} version={version:?} {{\n{actions}}}\n"
        );
        fs::write(&self.recipe, manifest)?;
        self.trust_publisher(publisher)?;
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
            &self.trust,
            "x86_64",
            BTreeMap::from([("doc.man".to_owned(), false)]),
        )?;
        Ok(())
    }
}

/// Image-wide and per-package facet rules configured on an image, without defaults.
fn local_facets(root: &Path) -> Result<BTreeMap<String, bool>> {
    Ok(crate::query::list_facets(root)?
        .into_iter()
        .filter(|row| row.source == "local")
        .map(|row| (row.rule, row.value))
        .collect())
}

fn dependency(kind: &str, name: &str, version: &str) -> String {
    format!("    {kind} {name:?} version=\"={version}\"\n")
}

#[test]
fn live_install_recovers_offline_and_preserves_boot_identity() -> Result<()> {
    use std::{collections::BTreeSet, panic};

    use crate::{
        apply::{apply_to_image, resume_live},
        image::{Image, root::Root},
    };

    let fixture = Fixture::new()?;
    let path = "usr/bin/live-app";
    fixture.package(
        "apps/live",
        "1.0.0-0",
        fixture.file(path, "new", "0755", "")?,
    )?;
    fixture.create()?;
    let before = Image::open_query(&fixture.root)?.settings()?;
    let install = plan(
        &fixture.root,
        Request::Install {
            packages: vec!["apps/live".into()],
        },
    )?;
    let open =
        || Image::open_for_live_apply(Root::open(&fixture.root)?, &before.id, before.revision);
    let interrupted = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        apply_to_image(open().unwrap(), &install, Some(&BTreeSet::new()), |write| {
            if write.path == path {
                panic!("interrupted");
            }
        })
    }));
    assert!(interrupted.is_err());
    assert_eq!(fs::read_to_string(fixture.root.join(path))?, "new");
    assert!(
        crate::transaction_status(&fixture.root)?
            .pending
            .unwrap()
            .live
    );
    assert!(crate::resume_transaction(&fixture.root).is_err());
    fs::rename(&fixture.repo, fixture.repo.with_extension("offline"))?;
    assert_eq!(resume_live(open()?)?, Some(before.revision + 1));
    assert_eq!(resume_live(open()?)?, None);
    let after = Image::open_query(&fixture.root)?.settings()?;
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(after.boot_revision(), before.revision);
    assert_eq!(after.as_boot_parent().image_revision(), after.revision);
    assert!(verify(&fixture.root)?.is_empty());
    Ok(())
}

#[test]
fn resume_with_repair_restores_damaged_pending_writes() -> Result<()> {
    use std::panic;

    let fixture = Fixture::new()?;
    let path = "usr/share/updated";
    fixture.package(
        "test/app",
        "1.0.0-0",
        fixture.file(path, "old", "0644", "")?,
    )?;
    fixture.create()?;
    let install = plan(
        &fixture.root,
        Request::Install {
            packages: vec!["test/app".into()],
        },
    )?;
    apply(&fixture.root, &install)?;
    fixture.package(
        "test/app",
        "2.0.0-0",
        fixture.file(path, "new", "0644", "")?,
    )?;
    let update = plan(&fixture.root, Request::Update)?;
    let interrupted = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        apply_with(&fixture.root, &update, |write| {
            if write.path == path {
                panic!("interrupted after publishing the file");
            }
        })
        .unwrap();
    }));
    assert!(interrupted.is_err());
    assert_eq!(fs::read_to_string(fixture.root.join(path))?, "new");
    fs::write(fixture.root.join(path), "damaged")?;
    assert!(crate::resume_transaction(&fixture.root).is_err());
    assert!(
        !crate::transaction_status(&fixture.root)?
            .pending
            .unwrap()
            .files_ready
    );

    // The new payload may need to be restored from the local repository as well.
    let image = crate::image::Image::open_query(&fixture.root)?;
    let stage = image.stage()?;
    fs::remove_file(crate::image::root::Root::fd_path(stage.as_file()).join(digest(b"new")))?;
    drop(image);

    assert_eq!(crate::repair_pending_transaction(&fixture.root)?, Some(2));
    assert_eq!(fs::read_to_string(fixture.root.join(path))?, "new");
    assert!(verify(&fixture.root)?.is_empty());
    Ok(())
}

#[test]
fn live_resume_repairs_unrelated_drift_in_the_pending_image() -> Result<()> {
    use std::{collections::BTreeSet, os::unix::fs::MetadataExt, panic};

    use crate::{
        apply::{apply_to_image, repair_pending_live, resume_live},
        image::{Image, root::Root},
    };

    let fixture = Fixture::new()?;
    let unchanged = "usr/share/unchanged";
    let alias = "usr/share/unchanged-alias";
    let config = "etc/local.conf";
    fixture.package(
        "test/base",
        "1.0.0-0",
        fixture.file(unchanged, "original", "0644", "")?
            + &format!("    hardlink {alias:?} target={unchanged:?}\n")
            + &fixture.file(config, "default", "0644", " preserve=#true")?,
    )?;
    let installed = "usr/bin/new-app";
    fixture.package(
        "apps/new",
        "1.0.0-0",
        fixture.file(installed, "new", "0755", "")?,
    )?;
    fixture.create()?;
    let install = |name: &str| {
        plan(
            &fixture.root,
            Request::Install {
                packages: vec![name.into()],
            },
        )
    };
    apply(&fixture.root, &install("test/base")?)?;
    fs::write(fixture.root.join(config), "local edit")?;
    let before = Image::open_query(&fixture.root)?.settings()?;
    let open =
        || Image::open_for_live_apply(Root::open(&fixture.root)?, &before.id, before.revision);
    let install = install("apps/new")?;
    let stage_packages = BTreeSet::new();
    let interrupted = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        apply_to_image(open().unwrap(), &install, Some(&stage_packages), |write| {
            if write.path == installed {
                panic!("interrupted after publishing the new file");
            }
        })
    }));
    assert!(interrupted.is_err());
    fs::write(fixture.root.join(unchanged), "damaged")?;
    let error = resume_live(open()?).unwrap_err();
    assert!(error.to_string().contains(unchanged));
    assert!(
        crate::transaction_status(&fixture.root)?
            .pending
            .unwrap()
            .files_ready
    );

    assert_eq!(repair_pending_live(open()?, &stage_packages)?, Some(2));
    assert_eq!(fs::read_to_string(fixture.root.join(installed))?, "new");
    assert_eq!(fs::read_to_string(fixture.root.join(config))?, "local edit");
    assert_eq!(fs::read_to_string(fixture.root.join(alias))?, "original");
    assert_eq!(
        fs::metadata(fixture.root.join(unchanged))?.ino(),
        fs::metadata(fixture.root.join(alias))?.ino()
    );
    assert_eq!(repair_pending_live(open()?, &stage_packages)?, None);
    Ok(())
}

#[test]
fn system_plan_previews_live_service_effects() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = "usr/lib/systemd/system/example.service";
    let install = Request::Install {
        packages: vec!["service/example".into()],
    };
    fixture.package(
        "service/example",
        "1.0.0-0",
        fixture.file(path, "[Service]\nExecStart=/usr/bin/example\n", "0644", "")?,
    )?;
    fixture.create()?;
    let initial = plan(&fixture.root, install)?;
    apply(&fixture.root, &initial)?;
    let (remove, services) = crate::plan::plan_system(
        &fixture.root,
        Request::Remove {
            packages: vec!["service/example".into()],
        },
        crate::PlanOptions::default(),
        crate::plan::UpdateSelection::default(),
    )?;
    assert_eq!(
        services.preview(),
        [
            "Stop active services matching example.service",
            "Reload the systemd manager"
        ],
    );
    assert!(remove.has_changes() && fixture.root.join(path).exists());
    Ok(())
}

#[test]
fn live_remove_recovers_offline_and_allows_autoremove() -> Result<()> {
    use std::{collections::BTreeSet, panic};

    use crate::{
        apply::{apply_live, apply_to_image, resume_live},
        image::{Image, root::Root},
    };

    let fixture = Fixture::new()?;
    let app = "usr/bin/live-app";
    let library = "usr/lib/live-library";
    fixture.package(
        "library/live",
        "1.0.0-0",
        fixture.file(library, "library", "0644", "")?,
    )?;
    fixture.package(
        "apps/live",
        "1.0.0-0",
        fixture.file(app, "app", "0755", "")? + &dependency("require", "library/live", "1.0.0-0"),
    )?;
    fixture.create()?;
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::Install {
                packages: vec!["apps/live".into()],
            },
        )?,
    )?;
    let before = Image::open_query(&fixture.root)?.settings()?;
    let open = || {
        Image::open_for_live_apply(
            Root::open(&fixture.root)?,
            &before.id,
            before.boot_revision(),
        )
    };
    fs::rename(&fixture.repo, fixture.repo.with_extension("offline"))?;
    let remove = plan(
        &fixture.root,
        Request::Remove {
            packages: vec!["apps/live".into()],
        },
    )?;
    let policy = BTreeSet::new();
    let live = open()?;
    let interrupted = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        apply_to_image(live, &remove, Some(&policy), |write| {
            if write.path == app {
                panic!("interrupted live removal");
            }
        })
    }));
    assert!(interrupted.is_err());
    assert!(!fixture.root.join(app).exists());
    assert!(fixture.root.join(library).exists());
    assert_eq!(resume_live(open()?)?, Some(before.revision + 1));

    let cleanup = plan(&fixture.root, Request::Autoremove)?;
    assert_eq!(apply_live(open()?, &cleanup, &policy)?, before.revision + 2);
    assert!(!fixture.root.join(library).exists());
    let after = Image::open_query(&fixture.root)?.settings()?;
    assert_eq!(after.boot_revision(), before.boot_revision());
    assert!(verify(&fixture.root)?.is_empty());
    Ok(())
}

#[test]
fn package_manifest_requires_staging_for_dependency_changes() -> Result<()> {
    use std::collections::BTreeSet;

    use crate::{
        apply::apply_live,
        image::{Image, root::Root},
        kdl_format::parse_manifest,
        query::package_manifest,
        system::Change,
    };

    let fixture = Fixture::new()?;
    let runtime = "system/runtime";
    let path = "usr/lib/libexample.so.1";
    let file = fixture.file(path, "version one", "0644", "")?;
    let manifest = format!(
        "schema-version 1 kind=\"package-manifest\"\npackage {runtime:?} publisher=\"canopa\" version=\"1.0.0-0\" requires-reboot=#true {{\n{file}}}\n"
    );
    assert!(parse_manifest(&manifest.replace("#true", "\"true\"")).is_err());
    fs::write(&fixture.recipe, manifest)?;
    publish(
        &fixture.repo,
        &fixture.proto,
        &fixture.recipe,
        &fixture.private,
    )?;
    fixture.package(
        "apps/example",
        "1.0.0-0",
        format!("    require {runtime:?}\n"),
    )?;
    fixture.package("apps/independent", "1.0.0-0", String::new())?;
    fixture.create()?;

    let verified = package_manifest(&fixture.root, runtime, true)?;
    assert!(parse_manifest(&verified)?.requires_reboot);
    let install = |name: &str| {
        plan(
            &fixture.root,
            Request::Install {
                packages: vec![name.into()],
            },
        )
    };
    let policy = BTreeSet::new();
    let reason = "system/runtime declares requires-reboot in its manifest";
    let initial = install("apps/example")?;
    let automatic = Change::from_plan(initial.clone(), &policy);
    assert_eq!(automatic.boot_reason(), Some(reason));
    assert_eq!(automatic.with_staging().boot_reason(), Some(reason));

    let settings = Image::open_query(&fixture.root)?.settings()?;
    let live =
        Image::open_for_live_apply(Root::open(&fixture.root)?, &settings.id, settings.revision)?;
    let error = apply_live(live, &initial, &policy).unwrap_err();
    assert!(format!("{error:#}").contains(reason));
    assert!(!fixture.root.join(path).exists());
    assert!(crate::transaction_status(&fixture.root)?.pending.is_none());

    apply(&fixture.root, &initial)?;
    let empty = Change::from_plan(install("apps/example")?, &policy).with_staging();
    assert!(empty.is_empty());
    assert!(empty.boot_reason().is_none());
    let ordinary = Change::from_plan(install("apps/independent")?, &policy);
    assert!(ordinary.boot_reason().is_none());
    assert_eq!(
        ordinary.with_staging().boot_reason(),
        Some("--stage was requested")
    );
    let remove = plan(
        &fixture.root,
        Request::Remove {
            packages: vec!["apps/example".into(), runtime.into()],
        },
    )?;
    assert_eq!(remove.boot_reason(&policy).as_deref(), Some(reason));

    fixture.package(
        runtime,
        "2.0.0-0",
        fixture.file(path, "version two", "0644", "")?,
    )?;
    let upgrade = plan(&fixture.root, Request::Update)?;
    assert_eq!(upgrade.boot_reason(&policy).as_deref(), Some(reason));
    apply(&fixture.root, &upgrade)?;
    assert!(
        plan(&fixture.root, Request::Update)?
            .boot_reason(&policy)
            .is_none()
    );
    Ok(())
}

#[test]
fn manifest_capabilities_require_a_protected_executable() -> Result<()> {
    use crate::kdl_format::{manifest_text, parse_manifest};
    let package = |metadata| {
        parse_manifest(&format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"test/caps\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n    file \"bin/probe\" capabilities=\"cap_net_raw=ep\" {metadata}\n}}\n"
        ))
    };
    for metadata in [
        "uid=1000 gid=0 mode=\"0755\"",
        "uid=0 gid=0 mode=\"0644\"",
        "uid=0 gid=0 mode=\"0775\"",
        "uid=0 gid=0 mode=\"0757\"",
        "uid=0 gid=0 mode=\"0755\" preserve=#true",
    ] {
        assert!(package(metadata)?.validate(false).is_err());
    }
    let manifest = package("uid=0 gid=0 mode=\"0755\"")?;
    manifest.validate(false)?;
    assert_eq!(parse_manifest(&manifest_text(&manifest))?, manifest);
    Ok(())
}

#[test]
fn hardlinks_follow_target_updates_and_resume_offline() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    const ALIAS: &str = "bin/compositor";
    const OTHER: &str = "usr/bin/compositor";
    let f = Fixture::new()?;
    let package = |version, bytes| {
        f.package(
            "desktop/compositor",
            version,
            f.file(COMPOSITOR, bytes, "0755", "")?
                + &format!(
                    "    hardlink {ALIAS:?} target={COMPOSITOR:?}\n    hardlink {OTHER:?} target={COMPOSITOR:?}\n"
                ),
        )
    };
    let check_links = |root: &Path, bytes: &str| -> Result<u64> {
        let canonical = fs::metadata(root.join(COMPOSITOR))?;
        assert_eq!(canonical.nlink(), 3);
        for path in [ALIAS, OTHER] {
            let alias = fs::metadata(root.join(path))?;
            assert_eq!(
                (alias.dev(), alias.ino()),
                (canonical.dev(), canonical.ino())
            );
            assert_eq!(fs::read_to_string(root.join(path))?, bytes);
        }
        assert!(crate::verify_pristine(root)?.is_empty());
        Ok(canonical.ino())
    };
    package("1.0.0-0", "version one")?;
    f.create()?;
    let plan_for = |request| plan(&f.root, request);
    let install = plan_for(Request::Install {
        packages: vec!["desktop/compositor".into()],
    })?;
    assert!(install.summary().contains("11 payload bytes required"));
    assert_eq!(apply(&f.root, &install)?, 1);
    let old_inode = check_links(&f.root, "version one")?;
    let copy = f.root.with_extension("clone");
    crate::stage_boot_environment(&f.root, &copy, &plan_for(Request::Update)?)?;
    assert_ne!(old_inode, check_links(&copy, "version one")?);

    let unowned = f.root.with_extension("unowned");
    fs::hard_link(f.root.join(COMPOSITOR), &unowned)?;
    assert!(verify(&f.root)?.is_empty());
    assert!(
        crate::verify_pristine(&f.root)?.iter().any(|finding| {
            finding.is_error && finding.message.contains("unexpected hard link")
        })
    );
    fs::remove_file(unowned)?;

    package("2.0.0-0", "version two")?;
    let upgrade = plan_for(Request::Update)?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("upgrade.json"),
        &upgrade,
        ALIAS,
    )?;
    assert_eq!(fs::read_to_string(f.root.join(ALIAS))?, "version two");
    assert_eq!(fs::read_to_string(f.root.join(OTHER))?, "version one");
    assert!(verify(&f.root).is_err());
    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    assert_eq!(apply(&f.root, &upgrade)?, 2);
    assert_ne!(old_inode, check_links(&f.root, "version two")?);

    fs::remove_file(f.root.join(ALIAS))?;
    fs::copy(f.root.join(COMPOSITOR), f.root.join(ALIAS))?;
    assert!(
        verify(&f.root)?.iter().any(|finding| {
            finding.is_error && finding.message.contains("expected a hard link")
        })
    );
    let repair = || {
        plan_for(Request::Repair {
            packages: vec!["desktop/compositor".into()],
        })
    };
    let fix = repair()?;
    assert!(fix.summary().contains("0 payload bytes required"));
    assert_eq!(apply(&f.root, &fix)?, 3);
    check_links(&f.root, "version two")?;

    fs::write(f.root.join(COMPOSITOR), "corrupt")?;
    let fix = repair()?;
    fs::rename(&offline, &f.repo)?;
    assert_eq!(apply(&f.root, &fix)?, 4);
    check_links(&f.root, "version two")?;

    // Splitting a group requires fresh inodes even when all payload bytes and
    // metadata stay the same. Replay after the new alias is already linked.
    const NEW: &str = "bin/new-compositor";
    f.package(
        "desktop/compositor",
        "3.0.0-0",
        f.file(COMPOSITOR, "version two", "0755", "")?
            + &f.file(ALIAS, "version two", "0755", "")?
            + &f.file(OTHER, "version two", "0755", "")?
            + &format!("    hardlink {NEW:?} target={ALIAS:?}\n"),
    )?;
    let split = plan_for(Request::Update)?;
    assert!(split.summary().contains(&format!("Detach {ALIAS}")));
    interrupt_apply_at(&f.root, &f._dir.path().join("split.json"), &split, NEW)?;
    let after = fs::metadata(f.root.join(ALIAS))?.ino();
    assert_eq!(apply(&f.root, &split)?, 5);
    assert_eq!(after, fs::metadata(f.root.join(ALIAS))?.ino());
    assert_eq!(after, fs::metadata(f.root.join(NEW))?.ino());
    let canonical = fs::metadata(f.root.join(COMPOSITOR))?;
    let other = fs::metadata(f.root.join(OTHER))?;
    assert_ne!(canonical.ino(), other.ino());
    assert_ne!(canonical.ino(), after);
    assert_ne!(other.ino(), after);
    assert!(crate::verify_pristine(&f.root)?.is_empty());

    let remove = plan_for(Request::Remove {
        packages: vec!["desktop/compositor".into()],
    })?;
    assert_eq!(apply(&f.root, &remove)?, 6);
    assert!(crate::verify_pristine(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn hardlink_targets_must_be_immutable_files_owned_by_the_same_package() -> Result<()> {
    use crate::kdl_format::parse_manifest;

    let parse = |actions: &str| {
        parse_manifest(&format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"test/hardlinks\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{actions}\n}}\n"
        ))
    };
    let file = "file \"usr/bin/tool\" mode=\"0755\" uid=0 gid=0";
    let link = "hardlink \"bin/tool\" target=\"usr/bin/tool\"";
    for actions in [
        link.to_owned(),
        format!("{file} preserve=#true\n{link}"),
        format!("{file} facet=\"doc.man\"\n{link}"),
        format!("{file} arch=\"aarch64\"\n{link} arch=\"x86_64\""),
        format!("symlink \"usr/bin/tool\" target=\"external\" uid=0 gid=0\n{link}"),
        format!("hardlink \"usr/bin/tool\" target=\"bin/tool\"\n{link}"),
        "hardlink \"bin/tool\" target=\"../tool\"".to_owned(),
        "hardlink \"bin/tool\" target=\"bin/tool\"".to_owned(),
    ] {
        assert!(parse(&actions)?.validate(false).is_err(), "{actions}");
    }
    assert!(parse(&format!("{file}\n{link} uid=0")).is_err());
    parse(&format!("{file}\n{link} arch=\"x86_64\""))?.validate(false)?;
    parse(&format!(
        "{file} facet=\"doc.man\"\n{link} facet=\"doc.man\""
    ))?
    .validate(false)?;
    Ok(())
}

#[test]
fn adding_a_hardlink_checks_the_unchanged_target_before_writing() -> Result<()> {
    use std::os::unix::fs::symlink;

    let f = Fixture::new()?;
    let canonical = f.file("usr/bin/tool", "tool", "0755", "")?;
    f.package("test/tool", "1.0.0-0", canonical.clone())?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["test/tool".into()],
        },
    )?;
    apply(&f.root, &install)?;
    f.package(
        "test/tool",
        "2.0.0-0",
        canonical + "    hardlink \"bin/tool\" target=\"usr/bin/tool\"\n",
    )?;
    let upgrade = plan(&f.root, Request::Update)?;
    let outside = f.root.with_extension("external");
    fs::write(&outside, "outside")?;
    fs::remove_file(f.root.join("usr/bin/tool"))?;
    symlink(&outside, f.root.join("usr/bin/tool"))?;
    let error = apply(&f.root, &upgrade).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("hard link target usr/bin/tool is not a regular file")
    );
    assert!(!f.root.join("bin/tool").exists());
    assert_eq!(fs::read_to_string(outside)?, "outside");
    assert!(verify(&f.root)?.iter().any(|finding| finding.is_error));
    Ok(())
}

#[test]
fn generated_manifest_is_deterministic_and_installs_the_prototype() -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

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
        let alias = proto.join("usr/bin/z-tool");
        let (first, second) = if reverse {
            (&alias, &payload)
        } else {
            (&payload, &alias)
        };
        fs::write(first, "tool payload")?;
        fs::set_permissions(first, fs::Permissions::from_mode(0o755))?;
        fs::hard_link(first, second)?;
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
    assert_eq!(parsed.action.len(), 8);
    assert!(parsed.action.iter().any(|action| matches!(
        action,
        crate::model::Action::Hardlink { path, target, .. }
            if path == "usr/bin/z-tool" && target == "usr/bin/tool #1"
    )));

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
    assert!(crate::verify_pristine(&f.root)?.is_empty());
    assert_eq!(
        fs::metadata(f.root.join("usr/bin/tool #1"))?.ino(),
        fs::metadata(f.root.join("usr/bin/z-tool"))?.ino()
    );
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
    let hardlink = f.proto.with_extension("outside-link");
    fs::hard_link(&file, &hardlink)?;
    assert!(
        generate(&f.proto)
            .unwrap_err()
            .to_string()
            .contains("extends outside the prototype")
    );
    fs::remove_file(hardlink)?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o1755))?;
    assert!(format!("{:#}", generate(&f.proto).unwrap_err()).contains("sticky bits"));
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
            "schema-version 1 kind=\"image-config\"\npublisher \"canopa\" {{\n    repository \"repo\"\n    trust-root \"trust.json\"\n}}\n\
             publisher \"extras\" {{\n    repository {:?}\n    trust-root {:?}\n}}\n",
            extra.repo, extra_key,
        )
    };
    fs::write(&config, definition(&primary.trust))?;
    let wrong_key_root = primary.root.with_extension("wrong-key");
    crate::create_configured_image(&wrong_key_root, &config)?;
    let install = |name: &str| Request::Install {
        packages: vec![name.to_owned()],
    };
    let error = format!(
        "{:#}",
        plan(&wrong_key_root, install("desktop/plugin")).unwrap_err()
    );
    assert!(
        error.contains("signed catalog for extras")
            && error.contains("pinned trust root belongs to publisher canopa"),
        "{error}"
    );

    fs::write(&config, definition(&extra.trust))?;
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
fn respelled_releases_satisfy_constraints_but_are_not_upgrades() -> Result<()> {
    let f = Fixture::new()?;
    let name = "desktop/compositor";
    f.package(name, "02.0.0", f.file(COMPOSITOR, "installed", "0755", "")?)?;
    f.package(
        "desktop/plugin",
        "1.0.0-0",
        format!("    require {name:?} version=\">= 2.0.0, <2.0.1\"\n"),
    )?;
    f.create()?;
    let install = |name: &str| Request::Install {
        packages: vec![name.into()],
    };
    apply(&f.root, &plan(&f.root, install("desktop/plugin"))?)?;
    f.package(
        name,
        "2.0.0-0",
        f.file(COMPOSITOR, "respelled", "0755", "")?,
    )?;
    let update = plan(&f.root, Request::Update)?;
    assert!(!update.summary().contains(name), "{}", update.summary());
    apply(&f.root, &update)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "installed");
    // The exact spelling still selects the other package ID deliberately.
    apply(
        &f.root,
        &plan(&f.root, install("desktop/compositor@2.0.0-0"))?,
    )?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "respelled");
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn boot_environment_keeps_its_own_image_state() -> Result<()> {
    let f = Fixture::new()?;
    let name = "desktop/compositor";
    f.package(name, "1.0.0-0", f.file(COMPOSITOR, "previous", "0755", "")?)?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec![name.into()],
        },
    )?;
    apply(&f.root, &install)?;
    let database = "var/lib/cps/image/state.sqlite";
    let base_state = fs::read(f.root.join(database))?;

    f.package(name, "2.0.0-0", f.file(COMPOSITOR, "trial", "0755", "")?)?;
    let staged = f.root.with_extension("staged");
    let update = plan(&f.root, Request::Update)?;
    crate::stage_boot_environment(&f.root, &staged, &update)?;
    assert_eq!(fs::read(f.root.join(database))?, base_state);
    assert_ne!(fs::read(staged.join(database))?, base_state);
    for (root, payload) in [(&f.root, "previous"), (&staged, "trial")] {
        assert!(!root.join(".cps").exists());
        assert_eq!(fs::read_to_string(root.join(COMPOSITOR))?, payload);
        assert!(crate::verify_pristine(root)?.is_empty());
    }

    f.package(
        "test/state-collision",
        "1.0.0-0",
        f.file(database, "unexpected", "0644", "")?,
    )?;
    let error = plan(
        &f.root,
        Request::Install {
            packages: vec!["test/state-collision".into()],
        },
    )
    .expect_err("packages must not own the image state directory");
    assert!(format!("{error:#}").contains("reserved image state path"));
    assert_eq!(fs::read(f.root.join(database))?, base_state);
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
    assert!(add_publisher(&base.root, "extras", &extra.repo, &base.trust).is_err());
    assert_eq!(settings(&base.root)?, before);
    add_publisher(&base.root, "extras", &extra.repo, &extra.trust)?;
    let order = || -> Result<Vec<String>> {
        Ok(list_publishers(&base.root)?
            .into_iter()
            .map(|publisher| publisher.name)
            .collect())
    };
    assert_eq!(order()?, ["canopa", "extras"]);
    assert!(add_publisher(&base.root, "extras", &extra.repo, &extra.trust).is_err());
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
fn builder_policy_changes_are_atomic_and_wait_for_transaction_recovery() -> Result<()> {
    use crate::{audit_publisher_builds, image::Image, set_publisher_build_trust};

    let f = Fixture::new()?;
    let worker = Fixture::new()?;
    let policy = f._dir.path().join("trust.kdl");
    fs::write(
        &policy,
        format!(
            "schema-version 1 kind=\"build-trust\"\nbuild-trust {{ builder-key {:?}; }}\n",
            worker.public
        ),
    )?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "original", "0755", "")?,
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("install.json"),
        &install,
        COMPOSITOR,
    )?;
    let error = set_publisher_build_trust(&f.root, "canopa", &policy, true).unwrap_err();
    assert!(error.to_string().contains("incomplete transaction"));
    apply(&f.root, &install)?;

    let settings = || Image::open(&f.root, false)?.settings();
    let before = settings()?;
    let saved = plan(&f.root, Request::Update)?;
    let error = set_publisher_build_trust(&f.root, "canopa", &policy, false).unwrap_err();
    assert!(error.to_string().contains("no build attestation"));
    assert_eq!(settings()?, before);

    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    set_publisher_build_trust(&f.root, "canopa", &policy, true)?;
    let after = settings()?;
    assert_eq!(after.publisher_generation, before.publisher_generation + 1);
    assert_eq!(after.id, before.id);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.boot_parent, before.boot_parent);
    assert_eq!(after.format, before.format);
    assert_eq!(after.catalog_checkpoints, before.catalog_checkpoints);
    let error = apply(&f.root, &saved).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("publisher configuration changed")
    );
    set_publisher_build_trust(&f.root, "canopa", &policy, true)?;
    assert_eq!(settings()?, after);

    fs::rename(&offline, &f.repo)?;
    let audit = audit_publisher_builds(&f.root, "canopa")?;
    assert_eq!(audit.verified, 0);
    assert_eq!(audit.failures.len(), 1);
    assert!(audit.failures[0].contains("no build attestation"));
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "unsigned by builders", "0755", "")?,
    )?;
    let error = plan(&f.root, Request::Update).unwrap_err();
    assert!(error.to_string().contains("no build attestation"));
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "original");
    Ok(())
}

#[test]
fn build_receipts_survive_offline_transaction_recovery_and_follow_installed_packages() -> Result<()>
{
    use crate::{
        attestation::{PinnedPackage, Toolchain, sign},
        audit_publisher_builds,
        kdl_format::{build, parse_manifest},
        model::FileDigest,
        publish_bundle, set_publisher_build_trust,
    };

    let f = Fixture::new()?;
    let worker = Fixture::new()?;
    let bundle = f._dir.path();
    let bytes = "guarded executable";
    let action = f.file(COMPOSITOR, bytes, "0755", "")?;
    let manifest = format!(
        "schema-version 1 kind=\"package-manifest\"\npackage \"desktop/compositor\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{action}}}\n"
    );
    fs::write(&f.recipe, &manifest)?;
    let pin = digest(b"test toolchain catalog");
    let key_hash = digest(b"test toolchain key");
    let recipe = format!(
        r#"schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v2" {{ publisher "seed" catalog="{pin}" key-sha256="{key_hash}"; package "build/shell"; }}
    manifest "manifest.kdl" sha256="{}"
    script "true"
}}
"#,
        digest(manifest.as_bytes())
    );
    fs::write(bundle.join("build.kdl"), &recipe)?;
    let recipe_model = build::parse(&recipe)?;
    let toolchain = Toolchain {
        publishers: recipe_model.toolchain.clone(),
        package_manifests: BTreeMap::from([(
            "build/shell".into(),
            PinnedPackage {
                publisher: "seed".into(),
                version: "1.0.0-0".parse()?,
                sha256: digest(b"test shell"),
            },
        )]),
    };
    crate::build::write_explicit_outputs(bundle, &[("manifest.kdl", manifest.as_bytes())])?;
    let evidence = sign(
        &crate::repo::SigningKey::open(&worker.private)?,
        &[parse_manifest(&manifest)?],
        &recipe_model,
        recipe.as_bytes(),
        toolchain,
        &BTreeMap::from([(
            COMPOSITOR.into(),
            FileDigest {
                sha256: digest(bytes.as_bytes()),
                size: bytes.len() as u64,
            },
        )]),
        &crate::image::root::Root::open(&f.proto)?,
    )?;
    fs::write(bundle.join("attestation.json"), evidence)?;
    publish_bundle(
        &f.repo,
        bundle,
        &f.private,
        &worker.public,
        &digest(recipe.as_bytes()),
    )?;

    f.create()?;
    let policy = bundle.join("trust.kdl");
    fs::write(
        &policy,
        format!(
            "schema-version 1 kind=\"build-trust\"\nbuild-trust {{ builder-key {:?}; }}\n",
            worker.public
        ),
    )?;
    set_publisher_build_trust(&f.root, "canopa", &policy, false)?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    interrupt_apply_at(&f.root, &bundle.join("pending.json"), &install, COMPOSITOR)?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(apply(&f.root, &install)?, 1);
    let audit = audit_publisher_builds(&f.root, "canopa")?;
    assert_eq!(audit.verified, 1);
    assert!(audit.failures.is_empty());

    {
        let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
        assert_eq!(
            db.execute("UPDATE build_receipts SET envelope=x'00'", [])?,
            1
        );
    }
    let audit = audit_publisher_builds(&f.root, "canopa")?;
    assert_eq!(audit.verified, 0);
    assert_eq!(audit.failures.len(), 1);
    assert!(audit.failures[0].contains("archived build attestation digest mismatch"));
    let remove = plan(
        &f.root,
        Request::Remove {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    assert_eq!(apply(&f.root, &remove)?, 2);
    let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
    let count: u32 = db.query_row("SELECT count(*) FROM build_receipts", [], |row| row.get(0))?;
    assert_eq!(count, 0);
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
fn rejects_old_plans_and_applies_current_plans_with_their_pinned_catalog() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "desktop/app",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    f.create()?;
    let saved = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/app".into()],
        },
    )?;
    let current = serde_json::to_value(&saved)?;
    assert_eq!(current["format"], 1);
    let mut old = current.clone();
    old.as_object_mut().unwrap().remove("install_recommends");
    assert!(serde_json::from_value::<Plan>(old.clone()).is_err());
    let catalogs = old.as_object_mut().unwrap().remove("catalogs").unwrap();
    old["catalog"] = catalogs["canopa"].clone();
    old["solver"] = "sat-v2".into();
    assert!(serde_json::from_value::<Plan>(old).is_err());
    let mut development = current.clone();
    development["format"] = 2.into();
    let development: Plan = serde_json::from_value(development)?;
    assert!(
        format!("{:#}", apply(&f.root, &development).unwrap_err())
            .contains("unsupported plan format")
    );

    f.package(
        "desktop/app",
        "2.0.0-0",
        f.file(COMPOSITOR, "new", "0755", "")?,
    )?;
    apply(&f.root, &serde_json::from_value(current)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    Ok(())
}

#[test]
fn marks_installed_package_requests_offline_without_touching_files() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    use crate::{
        MarkChoice,
        query::{ProfileRole, explain_package, list_package_intent},
    };

    let f = Fixture::new()?;
    let runtime = "library/runtime";
    let tool = "desktop/tool";
    f.package(runtime, "1.0.0-0", f.file(LIBRARY, "runtime", "0644", "")?)?;
    f.package(tool, "1.0.0-0", dependency("require", runtime, "1.0.0-0"))?;
    f.create()?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec![tool.into()],
            },
        )?,
    )?;
    let file = f.root.join(LIBRARY);
    let inode = file.metadata()?.ino();
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;

    let explanation = explain_package(&f.root, runtime)?;
    assert!(!explanation.intent.requested);
    assert_eq!(explanation.intent.profile, ProfileRole::None);
    assert_eq!(explanation.profile_name, None);
    assert_eq!(
        explanation.dependents,
        ["cps://canopa/desktop/tool@1.0.0-0"]
    );

    let mark = |choice, package: &str| {
        plan(
            &f.root,
            Request::Mark {
                choice,
                packages: vec![package.into()],
            },
        )
    };
    let stale = mark(MarkChoice::Automatic, tool)?;
    let error = mark(MarkChoice::Explicit, "library/runtime@2.0.0-0").unwrap_err();
    assert!(format!("{error:#}").contains("does not match the requested version"));
    let promoted = mark(MarkChoice::Explicit, "cps://canopa/library/runtime@1.0.0-0")?;
    assert!(promoted.summary().contains("as locally requested"));
    assert!(promoted.writes.is_empty() && promoted.systemd.is_empty());
    assert!(promoted.kernel_install.is_none() && promoted.catalogs.is_empty());
    assert_eq!(crate::fetch_plan(&f.root, &promoted)?, 0);
    assert_eq!(apply(&f.root, &promoted)?, 2);
    assert_eq!(apply(&f.root, &promoted)?, 2);
    let promoted_reason = explain_package(&f.root, "cps://canopa/library/runtime@1.0.0-0")?;
    assert!(promoted_reason.intent.requested);
    assert_eq!(promoted_reason.dependents, explanation.dependents);
    assert!(format!("{:#}", apply(&f.root, &stale).unwrap_err()).contains("stale plan"));
    let history = crate::transaction_details(&f.root, &promoted.id()?)?.details;
    assert_eq!(history.intent_changes, promoted.intent_changes);
    assert!(history.package_changes.is_empty());
    assert_eq!(history.planned_path_changes, 0);

    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Remove {
                packages: vec![tool.into()],
            },
        )?,
    )?;
    let rows = list_package_intent(&f.root)?;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].requested);
    assert_eq!(rows[0].profile, ProfileRole::None);
    let cleanup = plan(&f.root, Request::Autoremove)?;
    assert!(cleanup.writes.is_empty());
    let demoted = mark(MarkChoice::Automatic, runtime)?;
    assert!(demoted.summary().contains("as automatic"));
    apply(&f.root, &demoted)?;
    assert_eq!(file.metadata()?.ino(), inode);
    assert_eq!(fs::read_to_string(&file)?, "runtime");
    assert!(!list_package_intent(&f.root)?[0].requested);
    let explanation = explain_package(&f.root, "library/runtime@1.0.0-0")?;
    assert!(!explanation.intent.requested);
    assert!(explanation.dependents.is_empty());
    apply(&f.root, &plan(&f.root, Request::Autoremove)?)?;
    assert!(!file.exists());
    Ok(())
}

#[test]
fn autoremove_preserves_installed_roots_and_resumes_offline() -> Result<()> {
    use crate::query::{Scope, Status, list_packages};

    let f = Fixture::new()?;
    for name in [
        "library/backup",
        "library/primary",
        "desktop/theme",
        "desktop/optional-plugin",
    ] {
        f.package(name, "1.0.0-0", String::new())?;
    }
    f.package(
        "desktop/plugin",
        "1.0.0-0",
        f.file(PLUGIN, "active plugin", "0644", "")?,
    )?;
    f.package(
        "library/left",
        "1.0.0-0",
        f.file(LIBRARY, "unused library", "0644", "")? + "    require \"library/right\"\n",
    )?;
    f.package(
        "library/right",
        "1.0.0-0",
        "    require \"library/left\"\n".into(),
    )?;
    f.package(
        "desktop/setup-primary",
        "1.0.0-0",
        "    require \"library/primary\"\n    require \"library/left\"\n".into(),
    )?;
    f.package(
        "desktop/setup-backup",
        "1.0.0-0",
        "    require \"library/backup\"\n".into(),
    )?;
    f.package(
        "desktop/session",
        "1.0.0-0",
        // Both providers stay installed, so the declared order keeps the backup.
        r#"    require-any {
        package "library/backup"
        package "library/primary"
    }
    conditional "desktop/plugin" when="library/backup"
    conditional "desktop/optional-plugin" when="library/primary"
"#
        .into(),
    )?;
    f.create()?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: [
                    "desktop/session",
                    "desktop/setup-primary",
                    "desktop/setup-backup",
                    "desktop/theme",
                ]
                .map(str::to_owned)
                .to_vec(),
            },
        )?,
    )?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Remove {
                packages: vec![
                    "desktop/setup-primary".into(),
                    "desktop/setup-backup".into(),
                ],
            },
        )?,
    )?;
    assert_eq!(list_packages(&f.root, Scope::Installed)?.len(), 8);
    let cleanup = plan(&f.root, Request::Autoremove)?;
    assert!(cleanup.summary().contains("Remove library/left"));
    assert!(cleanup.summary().contains("Remove library/right"));
    assert!(cleanup.summary().contains("Remove library/primary"));
    assert!(cleanup.summary().contains("Remove desktop/optional-plugin"));
    let stage_packages = ["library/left".to_owned()].into_iter().collect();
    assert_eq!(
        cleanup.boot_reason(&stage_packages).as_deref(),
        Some("library/left is configured for staged changes")
    );
    let staged = f._dir.path().join("staged-cleanup");
    crate::stage_boot_environment(&f.root, &staged, &cleanup)?;
    assert!(!staged.join(LIBRARY).exists());
    assert!(f.root.join(LIBRARY).exists());
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("cleanup.json"),
        &cleanup,
        LIBRARY,
    )?;
    apply(&f.root, &cleanup)?;
    assert_eq!(fs::read_to_string(f.root.join(PLUGIN))?, "active plugin");
    assert!(!f.root.join(LIBRARY).exists());
    let installed: Vec<_> = list_packages(&f.root, Scope::Installed)?
        .into_iter()
        .map(|row| (row.name, row.status))
        .collect();
    assert_eq!(
        installed,
        [
            ("desktop/plugin".to_owned(), Status::Dependency),
            ("desktop/session".to_owned(), Status::Explicit),
            ("desktop/theme".to_owned(), Status::Explicit),
            ("library/backup".to_owned(), Status::Dependency),
        ]
    );
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    assert!(
        !plan(&f.root, Request::Autoremove)?
            .summary()
            .contains("Remove ")
    );
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
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec!["desktop/app".into()],
            },
        )?,
    )?;
    assert!(
        !f.root.join(ALPHA).exists(),
        "an installed backend already satisfies the dependency"
    );

    let fresh = f._dir.path().join("fresh");
    create_image(
        &fresh,
        &f.repo,
        "canopa",
        &f.trust,
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
fn virtual_packages_resolve_real_providers_and_preserve_dependencies_offline() -> Result<()> {
    use crate::query::{direct_dependencies, explain_package, reverse_dependencies};

    let f = Fixture::new()?;
    let install = |root: &Path, names: &[&str]| {
        plan(
            root,
            Request::Install {
                packages: names.iter().map(|name| name.to_string()).collect(),
            },
        )
    };
    let remove_zeta = || Request::Remove {
        packages: vec!["backend/zeta".into()],
    };
    let provider = "    provides \"cap/mail-transport\" version=\"2.0.0-0\"\n";
    f.package("backend/alpha", "1.0.0-0", provider.into())?;
    f.package(
        "backend/alpha",
        "2.0.0-0",
        "    provides \"cap/mail-transport\" version=\"1.0.0-0\"\n".into(),
    )?;
    f.package("backend/zeta", "1.0.0-0", provider.into())?;
    f.package(
        "backend/unversioned",
        "9.0.0-0",
        "    provides \"cap/mail-transport\"\n".into(),
    )?;
    f.package("cap/mail-transport", "9.0.0-0", String::new())?;
    f.package(
        "apps/mail",
        "1.0.0-0",
        "    require-capability \"cap/mail-transport\" version=\">=2.0.0-0\"\n".into(),
    )?;
    f.package(
        "apps/optional-mail",
        "1.0.0-0",
        "    require-capability \"cap/mail-transport\"\n".into(),
    )?;
    f.create()?;
    let unversioned = install(&f.root, &["backend/unversioned", "cap/mail-transport"])?;
    apply(&f.root, &unversioned)?;
    let optional = install(&f.root, &["apps/optional-mail"])?;
    apply(&f.root, &optional)?;
    assert_eq!(
        direct_dependencies(&f.root, "apps/optional-mail")?,
        ["cps://canopa/backend/unversioned@9.0.0-0"]
    );

    let fresh = f._dir.path().join("fresh");
    create_image(
        &fresh,
        &f.repo,
        "canopa",
        &f.trust,
        "x86_64",
        BTreeMap::new(),
    )?;
    let resolved = install(&fresh, &["apps/mail"])?;
    apply(&fresh, &resolved)?;
    assert_eq!(
        direct_dependencies(&fresh, "apps/mail")?,
        ["cps://canopa/backend/alpha@1.0.0-0"]
    );
    apply(&fresh, &install(&fresh, &["backend/zeta"])?)?;
    let cleanup = plan(&fresh, Request::Autoremove)?;
    assert!(cleanup.summary().contains("Remove backend/alpha"));
    apply(&fresh, &cleanup)?;

    apply(&f.root, &install(&f.root, &["backend/zeta"])?)?;
    let saved = install(&f.root, &["apps/mail"])?;
    apply(
        &f.root,
        &serde_json::from_str(&serde_json::to_string(&saved)?)?,
    )?;
    assert_eq!(
        direct_dependencies(&f.root, "apps/mail")?,
        ["cps://canopa/backend/zeta@1.0.0-0"]
    );
    assert_eq!(
        reverse_dependencies(&f.root, "backend/zeta")?,
        [
            "cps://canopa/apps/mail@1.0.0-0",
            "cps://canopa/apps/optional-mail@1.0.0-0"
        ]
    );
    assert_eq!(
        explain_package(&f.root, "backend/zeta")?.dependents,
        reverse_dependencies(&f.root, "backend/zeta")?
    );
    assert!(reverse_dependencies(&f.root, "cap/mail-transport")?.is_empty());

    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    let error = format!("{:#}", plan(&f.root, remove_zeta()).unwrap_err());
    assert!(
        error.contains("missing a provider for capability cap/mail-transport"),
        "{error}"
    );
    let mark = plan(
        &f.root,
        Request::Mark {
            choice: crate::MarkChoice::Automatic,
            packages: vec!["backend/zeta".into()],
        },
    )?;
    apply(&f.root, &mark)?;
    let cleanup = plan(&f.root, Request::Autoremove)?;
    assert!(!cleanup.summary().contains("Remove backend/zeta"));
    assert!(verify(&f.root)?.is_empty());

    fs::rename(&offline, &f.repo)?;
    apply(&f.root, &install(&f.root, &["backend/alpha@1.0.0-0"])?)?;
    fs::rename(&f.repo, &offline)?;
    apply(&f.root, &plan(&f.root, remove_zeta())?)?;
    assert_eq!(
        direct_dependencies(&f.root, "apps/mail")?,
        ["cps://canopa/backend/alpha@1.0.0-0"]
    );
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn virtual_package_requirement_without_providers_can_backtrack() -> Result<()> {
    let f = Fixture::new()?;
    f.package("cap/display", "1.0.0-0", String::new())?;
    f.package(
        "apps/display",
        "1.0.0-0",
        "    require \"cap/display\"\n".into(),
    )?;
    f.package(
        "apps/display",
        "2.0.0-0",
        "    require-capability \"cap/display\"\n".into(),
    )?;
    f.create()?;
    let install = |name: &str| {
        plan(
            &f.root,
            Request::Install {
                packages: vec![name.into()],
            },
        )
    };
    let error = format!("{:#}", install("apps/display@2.0.0-0").unwrap_err());
    assert!(error.contains("cap/display"), "{error}");
    apply(&f.root, &install("apps/display")?)?;
    assert_eq!(
        crate::query::direct_dependencies(&f.root, "apps/display@1.0.0-0")?,
        ["cps://canopa/cap/display@1.0.0-0"]
    );
    Ok(())
}

#[test]
fn recommendations_extend_required_plans_and_keep_installed_dependencies_offline() -> Result<()> {
    use crate::query::{Scope, Status, explain_package, list_packages};

    let f = Fixture::new()?;
    f.package("library/core", "1.0.0-0", String::new())?;
    f.package(
        "library/core",
        "2.0.0-0",
        f.file(LIBRARY, "core", "0644", "")?,
    )?;
    f.package(
        "extras/blocked",
        "1.0.0-0",
        dependency("require", "library/core", "1.0.0-0"),
    )?;
    f.package(
        "extras/collision",
        "1.0.0-0",
        f.file(LIBRARY, "collision", "0644", "")?,
    )?;
    f.package("extras/syntax", "1.0.0-0", String::new())?;
    f.package(
        "extras/help",
        "1.0.0-0",
        "    recommends \"extras/syntax\"\n".into(),
    )?;
    f.package(
        "apps/editor",
        "1.0.0-0",
        r#"    require "library/core"
    recommends "extras/help" version=">=1.0.0-0"
    recommends "extras/blocked"
    recommends "extras/collision"
    recommends "extras/missing"
"#
        .into(),
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/editor".into()],
        },
    )?;
    apply(
        &f.root,
        &serde_json::from_str(&serde_json::to_string(&install)?)?,
    )?;
    assert_eq!(
        list_packages(&f.root, Scope::Installed)?
            .into_iter()
            .map(|row| (row.name, row.status))
            .collect::<Vec<_>>(),
        [
            ("apps/editor".into(), Status::Explicit),
            ("extras/help".into(), Status::Dependency),
            ("extras/syntax".into(), Status::Dependency),
            ("library/core".into(), Status::Dependency),
        ]
    );
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "core");

    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    let help = explain_package(&f.root, "extras/help")?;
    assert!(help.dependents.is_empty());
    assert_eq!(help.recommenders, ["cps://canopa/apps/editor@1.0.0-0"]);
    assert!(verify(&f.root)?.is_empty());
    let cleanup = plan(&f.root, Request::Autoremove)?;
    assert!(cleanup.package_changes().is_empty());

    let removal = plan(
        &f.root,
        Request::Remove {
            packages: vec!["extras/help".into()],
        },
    )?;
    apply(&f.root, &removal)?;
    let cleanup = plan(&f.root, Request::Autoremove)?;
    assert!(cleanup.summary().contains("Remove extras/syntax"));
    apply(&f.root, &cleanup)?;
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn recommendations_can_be_skipped_and_upgrades_consider_new_recommendations() -> Result<()> {
    let f = Fixture::new()?;
    f.package("extras/old", "1.0.0-0", String::new())?;
    f.package("extras/new", "1.0.0-0", String::new())?;
    f.package("apps/other", "1.0.0-0", String::new())?;
    let original = "    recommends \"extras/old\"\n";
    f.package("apps/editor", "1.0.0-0", original.into())?;
    f.create()?;
    let install = |name: &str| Request::Install {
        packages: vec![name.into()],
    };
    let minimal = crate::plan_with_recommends(&f.root, install("apps/editor"), false)?;
    apply(
        &f.root,
        &serde_json::from_str(&serde_json::to_string(&minimal)?)?,
    )?;
    let other = plan(&f.root, install("apps/other"))?;
    assert!(!other.summary().contains("extras/old"));
    apply(&f.root, &other)?;

    f.package(
        "apps/editor",
        "2.0.0-0",
        original.to_owned() + "    recommends \"extras/new\"\n",
    )?;
    let update = plan(&f.root, Request::Update)?;
    assert!(update.summary().contains("Install extras/new"));
    assert!(!update.summary().contains("extras/old"));
    apply(&f.root, &update)?;
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
fn queries_signed_package_descriptions_and_keeps_optional_dependencies_advisory() -> Result<()> {
    use crate::{
        kdl_format::{manifest_text, parse_manifest},
        query::{Scope, list_packages, package_manifest},
    };

    let f = Fixture::new()?;
    let summary = "A lightweight text editor";
    let recipe = format!(
        r#"schema-version 1 kind="package-manifest"
package "apps/editor" publisher="canopa" version="1.0.0-0" summary="{summary}" {{
    optional "apps/editor-spellcheck" version=">=1.0.0-0" description="Check spelling while typing"
}}
"#,
    );
    let manifest = parse_manifest(&recipe)?;
    manifest.validate(false)?;
    assert_eq!(parse_manifest(&manifest_text(&manifest))?, manifest);
    for invalid in [
        recipe.replace(summary, "  "),
        recipe.replace(summary, r"first\nsecond"),
        recipe.replace(">=1.0.0-0", "unknown"),
        recipe.replace("Check spelling while typing", ""),
    ] {
        assert!(parse_manifest(&invalid)?.validate(false).is_err());
    }
    fs::write(&f.recipe, recipe)?;
    publish(&f.repo, &f.proto, &f.recipe, &f.private)?;
    f.create()?;
    fs::rename(f.repo.join("manifests"), f.repo.join("saved-manifests"))?;
    let available = list_packages(&f.root, Scope::Available)?;
    assert_eq!(available[0].summary.as_deref(), Some(summary));
    fs::rename(f.repo.join("saved-manifests"), f.repo.join("manifests"))?;
    let available = package_manifest(&f.root, "apps/editor", true)?;
    assert_eq!(parse_manifest(&available)?, manifest);

    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/editor".into()],
        },
    )?;
    assert_eq!(install.package_changes().len(), 1);
    apply(&f.root, &install)?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    let installed = list_packages(&f.root, Scope::Installed)?;
    assert_eq!(installed[0].summary.as_deref(), Some(summary));
    assert_eq!(package_manifest(&f.root, "apps/editor", false)?, available);
    Ok(())
}

#[test]
fn searches_files_in_verified_available_manifests() -> Result<()> {
    use crate::{
        add_publisher,
        query::{FileMatch, search_files},
        repo::{read_trust_root, snapshot},
    };

    let f = Fixture::new()?;
    let extras = Fixture::new()?;
    let backup = "usr/libexec/canopa-compositor";
    let old = "usr/bin/old-compositor";
    let foreign = "usr/sbin/canopa-compositor";
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(old, "old", "0755", "")?,
    )?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "current", "0755", "arch=\"x86_64\"")?
            + &f.file(backup, "backup", "0755", "")?
            + &f.file(foreign, "foreign", "0755", "arch=\"aarch64\"")?
            + &f.file(MANPAGE, "docs", "0644", "facet=\"doc.man\"")?,
    )?;
    extras.package_as(
        "extras",
        "desktop/compositor",
        "1.0.0-0",
        extras.file(COMPOSITOR, "alternative", "0755", "")?,
    )?;
    f.package(
        "tools/unrelated",
        "1.0.0-0",
        f.file("usr/bin/unrelated", "unrelated", "0755", "")?,
    )?;
    f.create()?;
    add_publisher(&f.root, "extras", &extras.repo, &extras.trust)?;
    fs::remove_dir_all(f.repo.join("blobs"))?;
    fs::remove_dir_all(extras.repo.join("blobs"))?;

    let primary = "cps://canopa/desktop/compositor@2.0.0-0";
    let secondary = "cps://extras/desktop/compositor@1.0.0-0";
    let found = |package: &str, path: &str| FileMatch {
        package: package.to_owned(),
        path: path.to_owned(),
    };
    assert_eq!(
        search_files(&f.root, "canopa-compositor")?,
        [
            found(primary, COMPOSITOR),
            found(primary, backup),
            found(secondary, COMPOSITOR)
        ]
    );
    for path in [COMPOSITOR.to_owned(), format!("/{COMPOSITOR}")] {
        assert_eq!(
            search_files(&f.root, &path)?,
            [found(primary, COMPOSITOR), found(secondary, COMPOSITOR)]
        );
    }
    for term in [old, foreign, MANPAGE, "compositor", "Canopa-Compositor"] {
        assert!(search_files(&f.root, term)?.is_empty(), "matched {term}");
    }
    assert!(search_files(&f.root, "../usr/bin/canopa-compositor").is_err());

    let index_catalog = crate::enable_file_index(&f.repo, "canopa", &f.private)?;
    assert_eq!(
        crate::enable_file_index(&f.repo, "canopa", &f.private)?,
        index_catalog
    );
    let index_dir = f.repo.join("file-indexes");
    let bytes = fs::read(fs::read_dir(&index_dir)?.next().unwrap()?.path())?;
    let mut document: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(document["format"], 1);
    let unversioned = document["entries"].to_string();
    document["format"] = 2.into();
    for invalid in [unversioned, document.to_string()] {
        let hash = digest(invalid.as_bytes());
        fs::write(index_dir.join(&hash), invalid)?;
        assert!(crate::repo::read_file_index(&f.repo, &hash).is_err());
    }
    let catalog = snapshot(&f.repo, "canopa", &read_trust_root(&f.trust)?, None)?;
    let unrelated = catalog
        .packages
        .iter()
        .find(|package| package.manifest.name == "tools/unrelated")
        .unwrap();
    fs::remove_file(f.repo.join("manifests").join(&unrelated.sha256))?;
    assert_eq!(
        search_files(&f.root, COMPOSITOR)?,
        [found(primary, COMPOSITOR), found(secondary, COMPOSITOR)]
    );
    let package = catalog
        .packages
        .iter()
        .find(|pkg| pkg.manifest.id() == primary)
        .unwrap();
    fs::write(f.repo.join("manifests").join(&package.sha256), b"tampered")?;
    assert!(search_files(&f.root, COMPOSITOR).is_err());
    Ok(())
}

#[test]
fn queries_installed_path_ownership_offline() -> Result<()> {
    use crate::query::{package_contents, path_owners};

    let f = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let shared = format!("    dir \"usr/share\" mode=\"0755\" uid={uid} gid={gid}\n");
    let link = "usr/lib/libdisplay-current.so";
    f.package(
        "library/libdisplay",
        "1.0.0-0",
        shared.clone() + &f.file(LIBRARY, "ABI 1", "0644", "")?,
    )?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        shared
            + &dependency("require", "library/libdisplay", "1.0.0-0")
            + &f.file(COMPOSITOR, "installed", "0755", "")?
            + &f.file(MANPAGE, "documentation", "0644", "facet=\"doc.man\"")?
            + &f.file("usr/bin/other-arch", "foreign", "0755", "arch=\"aarch64\"")?
            + &format!("    symlink {link:?} target=\"libdisplay.so\" uid={uid} gid={gid}\n"),
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    apply(&f.root, &install)?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    fs::remove_file(f.root.join(COMPOSITOR))?;

    let compositor = "cps://canopa/desktop/compositor@1.0.0-0";
    let library = "cps://canopa/library/libdisplay@1.0.0-0";
    assert_eq!(path_owners(&f.root, "/usr/share")?, [compositor, library]);
    assert_eq!(path_owners(&f.root, COMPOSITOR)?, [compositor]);
    assert_eq!(path_owners(&f.root, link)?, [compositor]);
    assert_eq!(path_owners(&f.root, LIBRARY)?, [library]);
    for path in ["usr/lib", MANPAGE, "usr/bin/other-arch"] {
        let error = path_owners(&f.root, path).unwrap_err();
        assert!(error.to_string().contains("no installed package owns"));
    }
    assert!(path_owners(&f.root, "../usr/share").is_err());
    for spec in [
        "desktop/compositor",
        "desktop/compositor@1.0.0-0",
        compositor,
    ] {
        assert_eq!(
            package_contents(&f.root, spec)?,
            [COMPOSITOR, link, "usr/share"]
        );
    }
    assert!(package_contents(&f.root, "desktop/compositor@2.0.0-0").is_err());
    assert!(package_contents(&f.root, "cps://other/desktop/compositor").is_err());
    Ok(())
}

#[test]
fn queries_direct_dependencies_and_dependents_offline() -> Result<()> {
    use crate::query::{direct_dependencies, explain_package, reverse_dependencies};

    let f = Fixture::new()?;
    let packages = [
        ("library/display", ""),
        ("library/backup", ""),
        ("runtime/desktop", ""),
        (
            "desktop/session",
            r#"    conditional "library/display" version="=1.0.0-0" when="runtime/desktop" when-version=">=1.0.0-0"
"#,
        ),
        (
            "desktop/direct",
            r#"    require "library/display" version=">=1.0.0-0, <2.0.0-0"
    conditional "library/display" when="runtime/desktop"
"#,
        ),
        (
            "desktop/alternate",
            r#"    require-any {
        package "library/display" version=">=2.0.0-0"
        package "library/display" version="=1.0.0-0"
        package "library/backup"
    }
"#,
        ),
        (
            "desktop/versioned",
            r#"    require-any {
        package "library/display" version=">=2.0.0-0"
        package "library/backup"
    }
    conditional "library/display" when="runtime/desktop" when-version=">=2.0.0-0"
    conditional "library/display" when="runtime/missing"
"#,
        ),
        (
            "desktop/policy",
            r#"    incorporate "library/display" version="=1.0.0-0"
    exclude "library/display" version=">=2.0.0-0"
"#,
        ),
        ("desktop/transitive", "    require \"desktop/direct\"\n"),
    ];
    for (name, dependencies) in packages {
        f.package(name, "1.0.0-0", dependencies.into())?;
    }
    f.create()?;
    let install = Request::Install {
        packages: packages.map(|(name, _)| name.to_owned()).to_vec(),
    };
    apply(&f.root, &plan(&f.root, install)?)?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;

    let backup = "cps://canopa/library/backup@1.0.0-0";
    let display = "cps://canopa/library/display@1.0.0-0";
    for spec in [
        "desktop/alternate",
        "desktop/alternate@1.0.0-0",
        "cps://canopa/desktop/alternate",
        "cps://canopa/desktop/alternate@1.0.0-0",
    ] {
        assert_eq!(direct_dependencies(&f.root, spec)?, [backup, display]);
    }
    assert_eq!(direct_dependencies(&f.root, "desktop/direct")?, [display]);
    assert_eq!(direct_dependencies(&f.root, "desktop/session")?, [display]);
    assert_eq!(direct_dependencies(&f.root, "desktop/versioned")?, [backup]);
    assert_eq!(
        direct_dependencies(&f.root, "desktop/transitive")?,
        ["cps://canopa/desktop/direct@1.0.0-0"]
    );
    assert!(direct_dependencies(&f.root, "desktop/policy")?.is_empty());
    assert!(direct_dependencies(&f.root, "runtime/desktop")?.is_empty());

    for spec in ["library/display", "library/display@1.0.0-0", display] {
        let expected = [
            "cps://canopa/desktop/alternate@1.0.0-0",
            "cps://canopa/desktop/direct@1.0.0-0",
            "cps://canopa/desktop/session@1.0.0-0",
        ];
        assert_eq!(reverse_dependencies(&f.root, spec)?, expected);
        let explanation = explain_package(&f.root, spec)?;
        assert_eq!(explanation.intent.id, display);
        assert!(explanation.intent.requested);
        assert_eq!(explanation.dependents, expected);
    }
    assert_eq!(
        reverse_dependencies(&f.root, "library/backup")?,
        [
            "cps://canopa/desktop/alternate@1.0.0-0",
            "cps://canopa/desktop/versioned@1.0.0-0",
        ]
    );
    assert!(reverse_dependencies(&f.root, "runtime/desktop")?.is_empty());
    for spec in [
        "runtime/missing",
        "library/display@2.0.0-0",
        "cps://other/library/display",
    ] {
        let expected = format!("package is not installed: {spec}");
        assert_eq!(
            direct_dependencies(&f.root, spec).unwrap_err().to_string(),
            expected
        );
        assert_eq!(
            reverse_dependencies(&f.root, spec).unwrap_err().to_string(),
            expected
        );
        assert_eq!(
            explain_package(&f.root, spec).unwrap_err().to_string(),
            expected
        );
    }
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Mark {
                choice: crate::MarkChoice::Automatic,
                packages: vec!["library/display".into()],
            },
        )?,
    )?;
    let path = explain_package(&f.root, display)?.path.unwrap();
    assert_eq!(path.root.id, "cps://canopa/desktop/alternate@1.0.0-0");
    assert_eq!(path.steps[0].provider, display);
    assert_eq!(path.steps[0].matching_providers, 2);
    assert!(path.steps[0].requirement.contains("alternatives"));

    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Mark {
                choice: crate::MarkChoice::Automatic,
                packages: vec![
                    "desktop/alternate".into(),
                    "desktop/direct".into(),
                    "desktop/session".into(),
                ],
            },
        )?,
    )?;
    let path = explain_package(&f.root, display)?.path.unwrap();
    assert_eq!(path.root.id, "cps://canopa/desktop/transitive@1.0.0-0");
    assert_eq!(path.steps.len(), 2);
    assert_eq!(
        path.steps[0].provider,
        "cps://canopa/desktop/direct@1.0.0-0"
    );
    assert_eq!(path.steps[1].provider, display);
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
        f.file(
            COMPOSITOR,
            "requires an unset variant",
            "0755",
            "variant.container=\"true\"",
        )?,
    )?;
    let mut held_back = Vec::new();
    let inspected = crate::plan::inspect_update(
        &f.root,
        crate::plan::UpdateDiagnostics {
            held_back: &mut held_back,
            explain: Some("desktop/app"),
            advisories: None,
        },
        &Default::default(),
    )?;
    assert!(!inspected.has_changes());
    assert_eq!(held_back.len(), 1);
    assert_eq!(held_back[0].available.to_string(), "4.0.0-0");
    assert!(
        held_back[0].reason.contains(COMPOSITOR),
        "{}",
        held_back[0].reason
    );
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
fn changes_facets_without_resolving_new_packages_and_recovers_offline() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    const ALIAS: &str = "usr/share/man/man1/compositor.1";
    let f = Fixture::new()?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "compositor 1", "0755", "")?
            + &f.file(MANPAGE, "manpage 1", "0644", "facet=\"doc.man\"")?
            + &f.file(
                CONFIG,
                "vendor settings",
                "0644",
                "preserve=#true facet=\"doc.man\"",
            )?
            + &format!("    hardlink {ALIAS:?} target={MANPAGE:?} facet=\"doc.man\"\n"),
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    apply(&f.root, &install)?;
    let before = crate::image::Image::open_query(&f.root)?.state()?;
    assert!(!f.root.join(MANPAGE).exists());
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "compositor 2", "0755", "")?,
    )?;
    let set = |value| Request::ChangeFacet {
        facets: BTreeMap::from([("doc.man".into(), value)]),
        with_dependencies: false,
    };
    let enable = plan(&f.root, set(None))?;
    assert!(
        enable
            .summary()
            .contains("Reset facet doc.man to its default")
    );
    assert!(enable.summary().contains(&format!("Add {MANPAGE}")));
    assert!(enable.catalogs.is_empty());
    let staged = f._dir.path().join("staged");
    crate::stage_boot_environment(&f.root, &staged, &enable)?;
    assert!(!f.root.join(MANPAGE).exists());
    assert_eq!(fs::read_to_string(staged.join(MANPAGE))?, "manpage 1");
    assert!(local_facets(&staged)?.is_empty());
    assert!(crate::verify_pristine(&staged)?.is_empty());

    interrupt_apply_at(&f.root, &f._dir.path().join("facet.json"), &enable, MANPAGE)?;
    assert!(!f.root.join(ALIAS).exists());
    assert!(crate::query::list_facets(&f.root).is_err());
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(apply(&f.root, &enable)?, before.settings.revision + 1);
    assert_eq!(
        fs::metadata(f.root.join(MANPAGE))?.ino(),
        fs::metadata(f.root.join(ALIAS))?.ino()
    );
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor 1");
    let after = crate::image::Image::open_query(&f.root)?.state()?;
    assert_eq!(after.installed, before.installed);
    assert_eq!(
        after.settings.catalog_checkpoints,
        before.settings.catalog_checkpoints
    );
    assert!(after.settings.facets.is_empty());
    assert!(crate::verify_pristine(&f.root)?.is_empty());

    fs::write(f.root.join(CONFIG), "administrator settings")?;
    let disable = plan(&f.root, set(Some(false)))?;
    assert!(
        disable
            .summary()
            .contains(&format!("Preserve local config {CONFIG}"))
    );
    apply(&f.root, &disable)?;
    assert!(!f.root.join(MANPAGE).exists() && !f.root.join(ALIAS).exists());
    assert_eq!(
        fs::read_to_string(f.root.join(CONFIG))?,
        "administrator settings"
    );
    assert_eq!(
        local_facets(&f.root)?,
        BTreeMap::from([("doc.man".into(), false)])
    );
    assert!(verify(&f.root)?.is_empty());
    fs::write(f.root.join(CONFIG), "replacement file")?;
    let error = plan(&f.root, set(None)).unwrap_err();
    assert!(error.to_string().contains("disabled configuration changed"));
    fs::write(f.root.join(CONFIG), "administrator settings")?;
    let inode = fs::metadata(f.root.join(CONFIG))?.ino();
    fs::rename(f.repo.with_extension("offline"), &f.repo)?;
    apply(&f.root, &plan(&f.root, set(None))?)?;
    assert_eq!(fs::metadata(f.root.join(CONFIG))?.ino(), inode);
    assert_eq!(
        fs::read_to_string(f.root.join(CONFIG))?,
        "administrator settings"
    );
    let state = crate::image::Image::open_query(&f.root)?.state()?;
    assert!(state.paths.contains_key(CONFIG));
    assert!(state.settings.dormant_configs.is_empty());
    assert_eq!(state.settings.format, before.settings.format);
    Ok(())
}

#[test]
fn reinstall_reclaims_the_same_package_config_after_removal() -> Result<()> {
    let f = Fixture::new()?;
    let name = "test/config";
    let install = Request::Install {
        packages: vec![name.into()],
    };
    f.package(
        name,
        "1.0.0-0",
        f.file(CONFIG, "first default", "0644", "preserve=#true")?,
    )?;
    f.create()?;
    apply(&f.root, &plan(&f.root, install.clone())?)?;
    fs::write(f.root.join(CONFIG), "local settings")?;
    let remove = Request::Remove {
        packages: vec![name.into()],
    };
    apply(&f.root, &plan(&f.root, remove)?)?;
    assert_eq!(fs::read_to_string(f.root.join(CONFIG))?, "local settings");
    assert!(verify(&f.root)?.is_empty());

    let other = "test/another-config";
    f.package(
        other,
        "1.0.0-0",
        f.file(CONFIG, "other default", "0644", "preserve=#true")?,
    )?;
    let error = plan(
        &f.root,
        Request::Install {
            packages: vec![other.into()],
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("unmanaged path"));

    fs::write(f.root.join(CONFIG), "edited while uninstalled")?;
    f.package(
        name,
        "2.0.0-0",
        f.file(CONFIG, "new default", "0600", "preserve=#true")?,
    )?;
    apply(&f.root, &plan(&f.root, install)?)?;
    assert_eq!(
        fs::read_to_string(f.root.join(CONFIG))?,
        "edited while uninstalled"
    );
    let proposals = crate::list_config_updates(&f.root)?;
    assert_eq!(proposals.len(), 1);
    assert_eq!(
        fs::read_to_string(f.root.join(&proposals[0].vendor_copy))?,
        "new default"
    );
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

#[test]
fn restores_disabled_config_proposals_and_moves_local_edits_after_upgrade() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const NEW_PATH: &str = "etc/canopa/preferences.conf";
    const ABSENT: &str = "etc/canopa/deleted.conf";
    let f = Fixture::new()?;
    let recipe = |path, contents, mode| -> Result<String> {
        Ok(f.file(path, contents, mode, "preserve=#true facet=\"optional\" original-name=\"desktop/compositor:etc/canopa/compositor.conf\"")?
            + &f.file(ABSENT, "deleted by administrator", "0644", "preserve=#true facet=\"optional\"")?)
    };
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        recipe(CONFIG, "first default", "0644")?,
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    apply(&f.root, &install)?;
    fs::write(f.root.join(CONFIG), "local preferences")?;
    fs::remove_file(f.root.join(ABSENT))?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        recipe(CONFIG, "second default", "0644")?,
    )?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    let previous = crate::list_config_updates(&f.root)?;
    assert_eq!(previous.len(), 1);
    let toggle = |value| Request::ChangeFacet {
        facets: BTreeMap::from([("optional".into(), value)]),
        with_dependencies: false,
    };
    apply(&f.root, &plan(&f.root, toggle(Some(false)))?)?;
    assert!(crate::list_config_updates(&f.root)?.is_empty());
    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    apply(&f.root, &plan(&f.root, toggle(None))?)?;
    let restored = crate::list_config_updates(&f.root)?;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].revision, previous[0].revision);
    assert!(restored[0].vendor_available);
    assert!(!f.root.join(ABSENT).exists());
    apply(&f.root, &plan(&f.root, toggle(Some(false)))?)?;
    fs::rename(&offline, &f.repo)?;

    f.package(
        "desktop/compositor",
        "3.0.0-0",
        recipe(NEW_PATH, "third default", "0600")?,
    )?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert!(f.root.join(CONFIG).is_file());
    assert!(!f.root.join(NEW_PATH).exists());
    let enable = plan(&f.root, toggle(None))?;
    assert!(
        enable
            .summary()
            .contains(&format!("Move config {CONFIG} to {NEW_PATH}"))
    );
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("resume-config.json"),
        &enable,
        NEW_PATH,
    )?;
    fs::rename(&f.repo, &offline)?;
    apply(&f.root, &enable)?;
    assert!(!f.root.join(CONFIG).exists());
    assert!(!f.root.join(ABSENT).exists());
    assert_eq!(
        fs::read_to_string(f.root.join(NEW_PATH))?,
        "local preferences"
    );
    assert_eq!(
        fs::metadata(f.root.join(NEW_PATH))?.permissions().mode() & 0o777,
        0o600
    );
    let proposals = crate::list_config_updates(&f.root)?;
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].path, NEW_PATH);
    assert_eq!(
        fs::read_to_string(f.root.join(&proposals[0].vendor_copy))?,
        "third default"
    );
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    apply(&f.root, &plan(&f.root, toggle(Some(false)))?)?;
    let remove = plan(
        &f.root,
        Request::Remove {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    apply(&f.root, &remove)?;
    let state = crate::image::Image::open_query(&f.root)?.state()?;
    assert!(state.settings.dormant_configs.is_empty());
    assert!(state.installed.is_empty());
    assert_eq!(
        fs::read_to_string(f.root.join(NEW_PATH))?,
        "local preferences"
    );
    Ok(())
}

#[test]
fn rejects_development_journals_and_verifies_completed_transactions() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "compositor", "0755", "")?,
    )?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("plan.json"),
        &install,
        COMPOSITOR,
    )?;
    let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
    let journal: String =
        db.query_row("SELECT data FROM pending WHERE id=1", [], |row| row.get(0))?;
    let journal: serde_json::Value = serde_json::from_str(&journal)?;
    let record = &journal["journaled"];
    assert_eq!(record["format"], 1);
    let legacy = serde_json::json!({"archived": {
        "id": record["id"], "writes": record["plan"]["writes"], "next": record["next"]
    }});
    let mut unversioned = journal.clone();
    unversioned["journaled"]
        .as_object_mut()
        .unwrap()
        .remove("format");
    let mut wrong_version = journal.clone();
    wrong_version["journaled"]["format"] = 2.into();
    for rejected in [legacy, unversioned, wrong_version] {
        let data = rejected.to_string();
        db.execute("UPDATE pending SET data=?1 WHERE id=1", [&data])?;
        assert!(crate::transaction_status(&f.root).is_err());
        assert!(crate::transaction_progress(&f.root, &install, false).is_err());
        assert!(crate::resume_transaction(&f.root).is_err());
        assert!(apply(&f.root, &install).is_err());
        let saved: String =
            db.query_row("SELECT data FROM pending WHERE id=1", [], |row| row.get(0))?;
        assert_eq!(saved, data);
        assert_eq!(
            crate::image::Image::open_query(&f.root)?
                .settings()?
                .revision,
            0
        );
    }
    db.execute(
        "UPDATE pending SET data=?1 WHERE id=1",
        [journal.to_string()],
    )?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(apply(&f.root, &install)?, 1);
    assert!(verify(&f.root)?.is_empty());
    let install_id = install.id()?;
    assert_eq!(
        crate::transaction_details(&f.root, &install_id)?
            .details
            .request,
        install.request,
    );
    let remove = plan(
        &f.root,
        Request::Remove {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    apply(&f.root, &remove)?;
    let entries = crate::transaction_history(&f.root, 20)?;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].plan_id, remove.id()?);
    assert_eq!(entries[1].plan_id, install_id);
    assert_eq!(crate::transaction_history(&f.root, 1)?.len(), 1);
    assert!(crate::transaction_history(&f.root, 0).is_err());
    assert!(crate::transaction_details(&f.root, &digest(b"unknown")).is_err());
    let data: String = db.query_row(
        "SELECT data FROM transaction_details WHERE plan=?1",
        [&install_id],
        |row| row.get(0),
    )?;
    let unversioned = digest(format!("{install_id}\n1\n{data}").as_bytes());
    db.execute(
        "UPDATE transaction_details SET sha256=?1 WHERE plan=?2",
        (&unversioned, &install_id),
    )?;
    assert!(crate::transaction_details(&f.root, &install_id).is_err());
    db.execute(
        "UPDATE transaction_details SET data='{}' WHERE plan=?1",
        [remove.id()?],
    )?;
    let error = crate::transaction_details(&f.root, &remove.id()?).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("transaction details digest mismatch")
    );
    db.execute(
        "DELETE FROM transaction_details WHERE plan=?1",
        [&install_id],
    )?;
    assert!(crate::transaction_details(&f.root, &install_id).is_err());
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
    let other = plan(&f.root, Request::Repair { packages: vec![] })?;
    interrupt_apply(&f.root, &saved, &upgrade)?;
    let interrupted = crate::transaction_progress(&f.root, &upgrade, false)?;
    assert_eq!(interrupted.phase, crate::TransactionPhase::Applying);
    assert_eq!(interrupted.plan_id, upgrade.id()?);
    assert_eq!(
        crate::transaction_progress(&f.root, &other, false)?.phase,
        crate::TransactionPhase::Blocked
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
        fs::read_to_string(
            f.root
                .join(format!("var/lib/cps/image/config-updates/3/{CONFIG}"))
        )?,
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

#[test]
fn reconciles_preserved_config_metadata_atomically() -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use crate::{fetch_plan, list_config_updates};

    const LOCAL: &str = "administrator setting";
    const ATTRIBUTE: &str = "user.cps-test";
    let f = Fixture::new()?;
    let package = |version: &str, mode: &str| {
        f.package(
            "desktop/settings",
            version,
            f.file(CONFIG, "vendor setting", mode, "preserve=#true")?,
        )
    };
    package("1.0.0-0", "0644")?;
    f.create()?;
    let initial = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/settings".into()],
        },
    )?;
    apply(&f.root, &initial)?;
    let config = f.root.join(CONFIG);
    fs::write(&config, LOCAL)?;
    rustix::fs::setxattr(
        &config,
        ATTRIBUTE,
        b"local annotation",
        rustix::fs::XattrFlags::empty(),
    )?;
    let alias = f._dir.path().join("outside-image");
    fs::hard_link(&config, &alias)?;
    let mode =
        |path: &Path| -> Result<u32> { Ok(fs::metadata(path)?.permissions().mode() & 0o777) };
    let check_local_config = || -> Result<()> {
        assert_eq!(mode(&config)?, 0o600);
        assert_eq!(fs::read_to_string(&config)?, LOCAL);
        let mut attribute = [0; 32];
        let len = rustix::fs::getxattr(&config, ATTRIBUTE, &mut attribute[..])?;
        assert_eq!(&attribute[..len], b"local annotation");
        Ok(())
    };

    package("2.0.0-0", "0600")?;
    let upgrade = plan(&f.root, Request::Update)?;
    assert!(
        upgrade
            .summary()
            .contains(&format!("Set package metadata on local config {CONFIG}"))
    );
    assert_eq!(fetch_plan(&f.root, &upgrade)?, 1);
    fs::write(&config, "changed after planning")?;
    assert!(
        apply(&f.root, &upgrade)
            .unwrap_err()
            .to_string()
            .contains("saved plan differs")
    );
    assert_eq!(mode(&alias)?, 0o644);
    fs::write(&config, LOCAL)?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("upgrade.json"),
        &upgrade,
        CONFIG,
    )?;
    check_local_config()?;
    assert_ne!(fs::metadata(&config)?.ino(), fs::metadata(&alias)?.ino());
    assert_eq!(mode(&alias)?, 0o644);
    assert_eq!(fs::read_to_string(&alias)?, LOCAL);
    assert!(
        !f.root
            .join("var/lib/cps/image/stage")
            .join(digest(LOCAL.as_bytes()))
            .exists()
    );

    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(apply(&f.root, &upgrade)?, 2);
    assert_eq!(apply(&f.root, &upgrade)?, 2);
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    let updates = list_config_updates(&f.root)?;
    assert_eq!(updates.len(), 1);
    assert!(updates[0].vendor_available);
    assert_eq!(mode(&f.root.join(&updates[0].vendor_copy))?, 0o600);

    // Images upgraded before this fix can restore the installed mode offline.
    fs::set_permissions(&config, fs::Permissions::from_mode(0o640))?;
    assert!(verify(&f.root)?.iter().any(|finding| finding.is_error));
    let repair = plan(
        &f.root,
        Request::Repair {
            packages: vec!["desktop/settings".into()],
        },
    )?;
    assert_eq!(fetch_plan(&f.root, &repair)?, 0);
    assert_eq!(apply(&f.root, &repair)?, 3);
    check_local_config()?;
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    assert!(list_config_updates(&f.root)?[0].vendor_available);
    Ok(())
}

#[test]
fn reviews_configuration_offline_in_a_staged_environment() -> Result<()> {
    use crate::{ConfigChoice, fetch_plan, list_config_updates, stage_boot_environment};

    const OPTIONAL: &str = "etc/canopa/optional.conf";
    const RESTORE: &str = "etc/canopa/restore.conf";
    let f = Fixture::new()?;
    let package = |version: &str, defaults: &str| {
        let mut actions = String::new();
        for path in [CONFIG, OPTIONAL, RESTORE] {
            actions += &f.file(path, defaults, "0644", "preserve=#true")?;
        }
        f.package("desktop/settings", version, actions)
    };
    let review = |path: &str, take| Request::ResolveConfig {
        path: path.to_owned(),
        take,
    };
    package("1.0.0-0", "vendor version one")?;
    f.create()?;
    let initial = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/settings".into()],
        },
    )?;
    apply(&f.root, &initial)?;
    fs::write(f.root.join(CONFIG), "administrator edit")?;
    fs::remove_file(f.root.join(OPTIONAL))?;
    fs::remove_file(f.root.join(RESTORE))?;
    package("2.0.0-0", "vendor version two")?;
    let staged = f._dir.path().join("staged");
    let upgrade = plan(&f.root, Request::Update)?;
    stage_boot_environment(&f.root, &staged, &upgrade)?;
    let proposals = list_config_updates(&staged)?;
    assert_eq!(proposals.len(), 3);
    assert!(
        proposals
            .iter()
            .all(|update| update.revision == 2 && update.vendor_available)
    );
    assert!(list_config_updates(&f.root)?.is_empty());
    let original = crate::image::Image::open_query(&staged)?.settings()?;
    let vendor_copy = staged.join(&proposals[0].vendor_copy);
    assert_eq!(proposals[0].path, CONFIG);

    let accept = plan(&staged, review(CONFIG, ConfigChoice::Vendor))?;
    fs::write(&vendor_copy, "tampered")?;
    assert!(!list_config_updates(&staged)?[0].vendor_available);
    let local_input = f._dir.path().join("local-input");
    let vendor_input = f._dir.path().join("vendor-input");
    let diff_inputs = || {
        crate::config_diff_inputs(
            &staged,
            CONFIG,
            &mut fs::File::create(&local_input)?,
            &mut fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&vendor_input)?,
        )
    };
    assert!(diff_inputs().is_err());
    assert!(
        apply(&staged, &accept)
            .unwrap_err()
            .to_string()
            .contains("differs from its installed manifest")
    );
    fs::write(&vendor_copy, "vendor version two")?;
    diff_inputs()?;
    assert_eq!(fs::read_to_string(&local_input)?, "administrator edit");
    assert_eq!(fs::read_to_string(&vendor_input)?, "vendor version two");
    fs::write(staged.join(CONFIG), "edited after review")?;
    assert!(
        apply(&staged, &accept)
            .unwrap_err()
            .to_string()
            .contains("saved plan differs")
    );
    fs::write(staged.join(CONFIG), "administrator edit")?;

    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    assert_eq!(fetch_plan(&staged, &accept)?, 0);
    let saved = f._dir.path().join("accept.json");
    interrupt_apply_at(&staged, &saved, &accept, CONFIG)?;
    assert!(
        list_config_updates(&staged)
            .unwrap_err()
            .to_string()
            .contains("incomplete transaction")
    );
    fs::remove_file(vendor_copy)?;
    fs::remove_file(saved)?;
    let status = crate::transaction_status(&staged)?;
    assert_eq!(status.revision, 2);
    assert_eq!(status.pending.unwrap().target_revision, 2);
    let before = crate::transaction_history(&staged, 10)?;
    assert_eq!(before.len(), 2);
    assert_eq!(before[0].plan_id, upgrade.id()?);
    assert_eq!(crate::resume_transaction(&staged)?, Some(2));
    assert_eq!(apply(&staged, &accept)?, 2);
    assert_eq!(
        fs::read_to_string(staged.join(CONFIG))?,
        "vendor version two"
    );

    let proposals = list_config_updates(&staged)?;
    assert_eq!(proposals.len(), 2);
    assert_eq!(proposals[0].path, OPTIONAL);
    fs::remove_file(staged.join(&proposals[0].vendor_copy))?;
    let keep = plan(&staged, review(OPTIONAL, ConfigChoice::Local))?;
    assert_eq!(apply(&staged, &keep)?, 2);
    assert!(!staged.join(OPTIONAL).exists());
    let restore = plan(&staged, review(RESTORE, ConfigChoice::Vendor))?;
    assert_eq!(apply(&staged, &restore)?, 2);
    assert_eq!(
        fs::read_to_string(staged.join(RESTORE))?,
        "vendor version two"
    );
    assert!(list_config_updates(&staged)?.is_empty());
    let history = crate::transaction_history(&staged, 10)?;
    assert_eq!(
        history
            .iter()
            .map(|entry| (entry.revision, entry.plan_id.clone()))
            .collect::<Vec<_>>(),
        [
            (2, restore.id()?),
            (2, keep.id()?),
            (2, accept.id()?),
            (2, upgrade.id()?),
            (1, initial.id()?)
        ]
    );
    let restored = &history[0].details;
    assert_eq!(restored.base_image, original.id);
    assert_eq!(restored.base_revision, 2);
    assert_eq!(restored.request, review(RESTORE, ConfigChoice::Vendor));
    let upgraded = &history[3].details;
    assert_eq!(upgraded.base_image, upgrade.image);
    assert_eq!(
        upgraded.catalogs["canopa"],
        upgrade.catalogs["canopa"].as_str()
    );
    assert_eq!(upgraded.package_changes[0].from.as_deref(), Some("1.0.0-0"));
    assert_eq!(upgraded.package_changes[0].to.as_deref(), Some("2.0.0-0"));
    assert_eq!(crate::transaction_history(&f.root, 10)?.len(), 1);
    let resolved = crate::image::Image::open_query(&staged)?.settings()?;
    assert_eq!(
        (resolved.id, resolved.revision, resolved.boot_parent),
        (original.id, original.revision, original.boot_parent)
    );
    assert_eq!(
        fs::read_to_string(f.root.join(CONFIG))?,
        "administrator edit"
    );
    assert!(!f.root.join(RESTORE).exists());

    fs::rename(offline, &f.repo)?;
    package("3.0.0-0", "vendor version three")?;
    let next = plan(&staged, Request::Update)?;
    apply(&staged, &next)?;
    let updates = list_config_updates(&staged)?;
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].path, OPTIONAL);
    assert_eq!(updates[0].revision, 3);
    assert_eq!(
        fs::read_to_string(staged.join(CONFIG))?,
        "vendor version three"
    );
    assert_eq!(apply(&staged, &keep)?, 2);
    assert_eq!(list_config_updates(&staged)?[0].revision, 3);
    let remove = plan(
        &staged,
        Request::Remove {
            packages: vec!["desktop/settings".into()],
        },
    )?;
    apply(&staged, &remove)?;
    assert!(list_config_updates(&staged)?.is_empty());
    assert!(staged.join(&updates[0].vendor_copy).is_file());
    Ok(())
}

#[test]
fn relocates_preserved_configuration_across_releases_and_resumes_offline() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    use crate::{list_config_updates, stage_boot_environment};

    const OLD: [&str; 3] = [CONFIG, "etc/canopa/optional", "etc/canopa/pristine"];
    const MIDDLE: [&str; 3] = ["etc/mid/main", "etc/mid/optional", "etc/mid/pristine"];
    const NEW: [&str; 3] = [
        "etc/new/settings/main",
        "etc/new/settings/optional",
        "etc/new/settings/pristine",
    ];
    let f = Fixture::new()?;
    let package = |name, version, paths: &[&str], defaults, mode, moved| -> Result<()> {
        let mut actions = String::new();
        for (path, original) in paths.iter().zip(OLD) {
            let properties = if moved {
                format!(r#"preserve=#true original-name="desktop/settings:{original}""#)
            } else {
                "preserve=#true".to_owned()
            };
            actions += &f.file(path, defaults, mode, &properties)?;
        }
        f.package(name, version, actions)
    };

    package("desktop/settings", "1.0.0-0", &OLD, "v1", "0644", false)?;
    f.create()?;
    let initial = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/settings".into()],
        },
    )?;
    apply(&f.root, &initial)?;
    fs::write(f.root.join(OLD[0]), "administrator settings")?;
    fs::remove_file(f.root.join(OLD[1]))?;
    let flags = rustix::fs::XattrFlags::empty();
    rustix::fs::setxattr(f.root.join(OLD[0]), "user.cps-test", b"annotation", flags)?;

    package("desktop/settings", "2.0.0-0", &OLD, "v2", "0644", false)?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(list_config_updates(&f.root)?.len(), 2);
    package("desktop/settings", "3.0.0-0", &MIDDLE, "v3", "0644", true)?;
    let intermediate = f._dir.path().join("intermediate");
    stage_boot_environment(&f.root, &intermediate, &plan(&f.root, Request::Update)?)?;

    package("desktop/preferences", "4.0.0-0", &NEW, "v4", "0600", true)?;
    let request = Request::ExactInstall {
        packages: vec!["desktop/preferences".into()],
    };
    let check_result = |root: &Path| -> Result<()> {
        assert!(
            OLD.iter()
                .chain(&MIDDLE)
                .all(|path| !root.join(path).exists())
        );
        assert_eq!(
            fs::read_to_string(root.join(NEW[0]))?,
            "administrator settings"
        );
        assert_eq!(
            fs::metadata(root.join(NEW[0]))?.permissions().mode() & 0o777,
            0o600
        );
        let mut attribute = [0; 16];
        let len = rustix::fs::getxattr(root.join(NEW[0]), "user.cps-test", &mut attribute[..])?;
        assert_eq!(&attribute[..len], b"annotation");
        assert!(!root.join(NEW[1]).exists());
        assert_eq!(fs::read_to_string(root.join(NEW[2]))?, "v4");
        let proposals = list_config_updates(root)?;
        assert_eq!(
            proposals
                .iter()
                .map(|update| update.path.as_str())
                .collect::<Vec<_>>(),
            NEW[..2]
        );
        for update in proposals {
            assert!(update.vendor_available);
            assert_eq!(fs::read_to_string(root.join(update.vendor_copy))?, "v4");
        }
        assert!(verify(root)?.iter().all(|finding| !finding.is_error));
        Ok(())
    };

    let moved_twice = f._dir.path().join("moved-twice");
    stage_boot_environment(
        &intermediate,
        &moved_twice,
        &plan(&intermediate, request.clone())?,
    )?;
    check_result(&moved_twice)?;
    assert_eq!(
        fs::read_to_string(intermediate.join(MIDDLE[0]))?,
        "administrator settings"
    );

    fs::create_dir_all(f.root.join(NEW[0]).parent().unwrap())?;
    fs::write(f.root.join(NEW[0]), "unmanaged settings")?;
    assert!(
        plan(&f.root, request.clone())
            .unwrap_err()
            .to_string()
            .contains("unmanaged path")
    );
    fs::remove_file(f.root.join(NEW[0]))?;
    let direct = plan(&f.root, request)?;
    assert!(
        direct
            .summary()
            .contains(&format!("Move config {} to {}", OLD[0], NEW[0]))
    );
    let staged = f._dir.path().join("skipped-version");
    stage_boot_environment(&f.root, &staged, &direct)?;
    check_result(&staged)?;
    assert_eq!(
        fs::read_to_string(f.root.join(OLD[0]))?,
        "administrator settings"
    );
    assert_eq!(fs::read_to_string(f.root.join(OLD[2]))?, "v2");
    assert_eq!(list_config_updates(&f.root)?[0].path, OLD[0]);

    interrupt_apply_at(&f.root, &f._dir.path().join("move.json"), &direct, NEW[0])?;
    assert_eq!(
        fs::read_to_string(f.root.join(OLD[0]))?,
        "administrator settings"
    );
    assert_eq!(
        fs::read_to_string(f.root.join(NEW[0]))?,
        "administrator settings"
    );
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(apply(&f.root, &direct)?, 3);
    assert_eq!(apply(&f.root, &direct)?, 3);
    check_result(&f.root)?;
    Ok(())
}

#[test]
fn configuration_move_identities_are_unique_within_a_publisher() -> Result<()> {
    const OTHER: &str = "etc/replacement/preferences.conf";
    const MOVE: &str =
        "preserve=#true original-name=\"desktop/settings:etc/canopa/compositor.conf\"";
    let primary = Fixture::new()?;
    let extra = Fixture::new()?;
    primary.package(
        "desktop/settings",
        "1.0.0-0",
        primary.file(CONFIG, "original defaults", "0644", "preserve=#true")?,
    )?;
    let other = primary.file(OTHER, "alternate defaults", "0644", MOVE)?;
    primary.package("desktop/alternate", "1.0.0-0", other)?;
    extra.package_as(
        "extras",
        "desktop/replacement",
        "1.0.0-0",
        extra.file(OTHER, "foreign defaults", "0644", MOVE)?,
    )?;
    let config = primary._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            "schema-version 1 kind=\"image-config\"\npublisher \"canopa\" {{\n    repository \"repo\"\n    trust-root \"trust.json\"\n}}\n\
             publisher \"extras\" {{\n    repository {:?}\n    trust-root {:?}\n}}\n",
            extra.repo, extra.trust,
        ),
    )?;
    crate::create_configured_image(&primary.root, &config)?;
    let install = |name: &str| Request::Install {
        packages: vec![name.into()],
    };
    apply(
        &primary.root,
        &plan(&primary.root, install("desktop/settings"))?,
    )?;
    fs::write(primary.root.join(CONFIG), "private administrator setting")?;

    let error = plan(&primary.root, install("desktop/alternate")).unwrap_err();
    assert!(
        format!("{error:#}").contains("configuration identity"),
        "{error:#}"
    );
    let foreign = plan(
        &primary.root,
        Request::ExactInstall {
            packages: vec!["desktop/replacement".into()],
        },
    )?;
    assert!(!foreign.summary().contains("Move config"));
    apply(&primary.root, &foreign)?;
    assert_eq!(
        fs::read_to_string(primary.root.join(OTHER))?,
        "foreign defaults"
    );
    assert_eq!(
        fs::read_to_string(primary.root.join(CONFIG))?,
        "private administrator setting"
    );
    Ok(())
}

#[test]
fn rejects_the_old_image_state_layout_and_format() -> Result<()> {
    let f = Fixture::new()?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        "schema-version 1 kind=\"image-config\"\nimage-state \"legacy\"\npublisher \"canopa\" {\n repository \"repo\"\n trust-root \"trust.json\"\n}\n",
    )?;
    f.package("test/pkg", "1.0.0-0", String::new())?;
    assert!(crate::create_configured_image(&f.root, &config).is_err());
    f.create()?;
    let state_dir = f.root.join("var/lib/cps/image");
    assert_eq!(
        crate::image::Image::open_query(&f.root)?.settings()?.format,
        1
    );
    assert!(verify(&f.root)?.is_empty());
    let db = rusqlite::Connection::open(state_dir.join("state.sqlite"))?;
    db.execute("UPDATE image SET data=json_set(data, '$.format', 13)", [])?;
    let error = verify(&f.root).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported image format"));
    db.execute("UPDATE image SET data=json_set(data, '$.format', 1)", [])?;
    db.pragma_update(None, "application_id", 0)?;
    drop(db);
    let Err(error) = crate::list_publishers(&f.root) else {
        panic!("accepted a development image database without an application ID");
    };
    assert!(format!("{error:#}").contains("unsupported image database layout"));
    fs::rename(state_dir, f.root.join(".cps"))?;
    assert!(verify(&f.root).is_err());
    Ok(())
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
    assert!(repair.catalogs.is_empty());
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
    assert!(
        crate::verify_selected(
            &f.root,
            &["cps://canopa/desktop/compositor@1.0.0-0".into()],
            &[],
            false,
        )?
        .iter()
        .all(|finding| !finding.is_error)
    );
    assert!(crate::verify_selected(&f.root, &[], &[format!("/{LIBRARY}")], false)?[0].is_error);
    assert!(crate::verify_selected(&f.root, &[], &[format!("/{LIBRARY}")], true)?[0].is_error);

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

    fs::write(f.root.join(COMPOSITOR), "damaged again")?;
    fs::write(f.root.join(LIBRARY), "other drift")?;
    let repair_path = plan(
        &f.root,
        Request::RepairPaths {
            paths: vec![format!("/{COMPOSITOR}")],
        },
    )?;
    assert_eq!(apply(&f.root, &repair_path)?, 4);
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor 1");
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "other drift");
    let revert = plan(
        &f.root,
        Request::Revert {
            paths: vec![CONFIG.into(), REMOVED_CONFIG.into()],
        },
    )?;
    assert_eq!(apply(&f.root, &revert)?, 5);
    assert_eq!(fs::read_to_string(f.root.join(CONFIG))?, "vendor config");
    assert_eq!(fs::read_to_string(f.root.join(REMOVED_CONFIG))?, "optional");
    assert!(
        plan(
            &f.root,
            Request::Revert {
                paths: vec![COMPOSITOR.into()]
            }
        )
        .is_err()
    );
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
    apply(&f.root, &remove)?;
    assert!(!f.root.join(LIB).exists() && !f.root.join(APP).exists());
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

mod release;

mod facets;
mod freezes;
mod modes;
mod triggers;

#[test]
fn compares_independent_committed_roots_without_the_repository() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "demo/tool",
        "1.0.0-0",
        f.file("usr/bin/tool", "old", "0755", "")?,
    )?;
    f.create()?;
    let install = |root: &Path| -> Result<()> {
        apply(
            root,
            &plan(
                root,
                Request::Install {
                    packages: vec!["demo/tool".into()],
                },
            )?,
        )?;
        Ok(())
    };
    install(&f.root)?;
    f.package(
        "demo/tool",
        "2.0.0-0",
        f.file("usr/bin/tool", "new", "0755", "")?,
    )?;
    let next = f.root.with_file_name("next");
    create_image(
        &next,
        &f.repo,
        "canopa",
        &f.trust,
        "x86_64",
        BTreeMap::new(),
    )?;
    install(&next)?;
    fs::remove_dir_all(&f.repo)?;
    fs::remove_file(next.join("usr/bin/tool"))?;

    let diff = crate::image_diff(&f.root, &next, true)?;
    assert_ne!(diff.before.id, diff.after.id);
    assert_eq!(diff.packages.len(), 1);
    let change = &diff.packages[0];
    assert_eq!(change.name, "demo/tool");
    assert_eq!(
        change.before.as_ref().unwrap().version.to_string(),
        "1.0.0-0"
    );
    assert_eq!(
        change.after.as_ref().unwrap().version.to_string(),
        "2.0.0-0"
    );
    let paths = diff.paths.unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].name, "usr/bin/tool");
    assert!(crate::image_diff(&next, &next, false)?.packages.is_empty());
    Ok(())
}

#[test]
fn builder_keys_must_differ_from_every_key_of_the_pinned_trust_root() -> Result<()> {
    let f = Fixture::new()?;
    let extra = Fixture::new()?;
    f.package("demo/base", "1.0.0-0", String::new())?;
    extra.package_as("extras", "demo/extra", "1.0.0-0", String::new())?;
    f.create()?;
    let policy = f._dir.path().join("build-trust.kdl");
    for (builder, rejected) in [(&extra.public, true), (&f.public, false)] {
        fs::write(
            &policy,
            format!(
                "schema-version 1 kind=\"build-trust\"\nbuild-trust {{\n    builder-key {builder:?}\n}}\n"
            ),
        )?;
        let result = crate::add_publisher_with_build_trust(
            &f.root,
            "extras",
            &extra.repo,
            &extra.trust,
            Some(&policy),
        );
        if rejected {
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains("builder and publisher must use distinct keys"),
                "{error}"
            );
        } else {
            result?;
        }
    }
    Ok(())
}

fn file_records(root: &Path) -> Result<crate::image::FileRecords> {
    crate::image::Image::open_query(root)?.file_records(None)
}

/// Treat fingerprints as settled after 50 ms on this test thread, and wait
/// that long, so a transaction records the files written before the call.
fn settle_fingerprints() {
    crate::image::root::TEST_SETTLED_NS.set(50_000_000);
    std::thread::sleep(std::time::Duration::from_millis(100));
}

fn install(root: &Path, name: &str) -> Result<u64> {
    let saved = plan(
        root,
        Request::Install {
            packages: vec![name.into()],
        },
    )?;
    apply(root, &saved)
}

fn fingerprint_fixture() -> Result<Fixture> {
    let f = Fixture::new()?;
    f.package(
        "test/one",
        "1.0.0-0",
        f.file("usr/share/one", "one", "0644", "")?
            + &f.file("usr/bin/tool", "tool", "0755", "")?,
    )?;
    f.package(
        "test/two",
        "1.0.0-0",
        f.file("usr/share/two", "two", "0644", "")?,
    )?;
    f.create()?;
    install(&f.root, "test/one")?;
    assert!(file_records(&f.root)?.is_empty());
    settle_fingerprints();
    install(&f.root, "test/two")?;
    Ok(f)
}

#[test]
fn transactions_record_fingerprints_that_quick_verification_trusts() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let f = fingerprint_fixture()?;
    let records = file_records(&f.root)?;
    // The drift check hashed the unchanged files and recorded them. The new
    // file changed moments ago, so it is not trusted yet.
    assert!(records.contains_key("usr/share/one") && records.contains_key("usr/bin/tool"));
    assert!(!records.contains_key("usr/share/two"));
    assert_eq!(records["usr/share/one"].observation.sha256, digest(b"one"));
    // The shared-library check cached what it learned about the files.
    assert_eq!(records["usr/bin/tool"].elf.as_deref(), Some("null"));
    assert!(crate::verify_quick(&f.root)?.is_empty());

    // Quick verification believes a matching record instead of hashing.
    let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
    let tamper = |sha256: &str| {
        db.execute(
            "UPDATE files SET sha256=?1 WHERE path='usr/share/one'",
            [sha256],
        )
    };
    tamper(&digest(b"other"))?;
    let findings = crate::verify_quick(&f.root)?;
    assert!(
        findings
            .iter()
            .any(|finding| finding.is_error && finding.message.starts_with("usr/share/one:")),
        "{findings:?}"
    );
    assert!(verify(&f.root)?.is_empty());
    let selected = crate::verify_selected(&f.root, &[], &["/usr/share/one".into()], true)?;
    assert!(selected.iter().any(|finding| finding.is_error));
    tamper(&digest(b"one"))?;

    // Every change to a file invalidates its record, even at the same size.
    fs::write(f.root.join("usr/share/one"), "ONE")?;
    assert!(
        crate::verify_quick(&f.root)?
            .iter()
            .any(|finding| finding.message == "usr/share/one: content, type, or metadata differs")
    );
    fs::write(f.root.join("usr/share/one"), "one")?;
    assert!(crate::verify_quick(&f.root)?.is_empty());
    let tool = f.root.join("usr/bin/tool");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700))?;
    assert!(!crate::verify_quick(&f.root)?.is_empty());
    let remove = plan(
        &f.root,
        Request::Remove {
            packages: vec!["test/two".into()],
        },
    )?;
    let error = apply(&f.root, &remove).unwrap_err();
    assert!(format!("{error:#}").contains("pre-existing drift at usr/bin/tool"));
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755))?;

    // A transaction forgets records of paths it writes or removes and
    // replaces records that no longer match.
    fs::write(f.root.join("usr/share/one"), "one")?;
    settle_fingerprints();
    apply(&f.root, &remove)?;
    let records = file_records(&f.root)?;
    assert!(!records.contains_key("usr/share/two"));
    assert_eq!(records["usr/share/one"].observation.sha256, digest(b"one"));
    assert!(crate::verify_quick(&f.root)?.is_empty() && verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn copied_roots_do_not_trust_the_source_fingerprints() -> Result<()> {
    let f = fingerprint_fixture()?;
    let copy = f._dir.path().join("copy");
    let status = Command::new("cp")
        .arg("-a")
        .arg(&f.root)
        .arg(&copy)
        .status()?;
    assert!(status.success());
    let db = rusqlite::Connection::open(copy.join("var/lib/cps/image/state.sqlite"))?;
    let changed = db.execute(
        "UPDATE files SET sha256=?1 WHERE path='usr/share/one'",
        [digest(b"other")],
    )?;
    assert_eq!(changed, 1);
    drop(db);
    // Copies have new inodes and change times, so CPS hashes them again.
    assert!(crate::verify_quick(&copy)?.is_empty());
    let remove = plan(
        &copy,
        Request::Remove {
            packages: vec!["test/two".into()],
        },
    )?;
    settle_fingerprints();
    apply(&copy, &remove)?;
    let records = file_records(&copy)?;
    assert_eq!(records["usr/share/one"].observation.sha256, digest(b"one"));
    Ok(())
}

#[test]
fn journal_records_only_changes_to_the_committed_state() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "test/one",
        "1.0.0-0",
        f.file("usr/share/one/a", "a", "0644", "")?
            + &f.file("usr/share/one/b", "b", "0644", "")?,
    )?;
    f.package(
        "test/two",
        "1.0.0-0",
        f.file("usr/share/two", "two", "0644", "")?,
    )?;
    f.create()?;
    install(&f.root, "test/one")?;
    let saved = plan(
        &f.root,
        Request::Install {
            packages: vec!["test/two".into()],
        },
    )?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("plan.json"),
        &saved,
        "usr/share/two",
    )?;
    let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
    let original: String =
        db.query_row("SELECT data FROM pending WHERE id=1", [], |row| row.get(0))?;
    let journal: serde_json::Value = serde_json::from_str(&original)?;
    let next = &journal["journaled"]["next"];
    let installed = next["installed"].as_object().unwrap();
    assert_eq!(installed.keys().collect::<Vec<_>>(), ["test/two"]);
    let paths = next["paths"].as_object().unwrap();
    assert!(paths.contains_key("usr/share/two"));
    assert!(!paths.contains_key("usr/share/one/a") && !paths.contains_key("usr/share/one"));
    assert!(next.get("publisher").is_none());

    // A delta that no longer matches its manifests or the committed state
    // is rejected, even with a matching digest.
    let tampered = |change: &dyn Fn(&mut serde_json::Value)| -> Result<String> {
        let mut journal = journal.clone();
        change(&mut journal["journaled"]["next"]);
        let delta: crate::image::StateDelta =
            serde_json::from_value(journal["journaled"]["next"].clone())?;
        journal["journaled"]["state_sha256"] = digest(&serde_json::to_vec(&delta)?).into();
        db.execute(
            "UPDATE pending SET data=?1 WHERE id=1",
            [journal.to_string()],
        )?;
        Ok(format!(
            "{:#}",
            crate::resume_transaction(&f.root).unwrap_err()
        ))
    };
    let error = tampered(&|next| {
        next["paths"]
            .as_object_mut()
            .unwrap()
            .remove("usr/share/two");
    })?;
    assert!(error.contains("pending ownership index differs"), "{error}");
    let error = tampered(&|next| {
        next["paths"]["usr/share/one/a"] = serde_json::Value::Null;
    })?;
    assert!(error.contains("pending ownership index differs"), "{error}");
    let error = tampered(&|next| {
        next["paths"]["usr/share/missing"] = serde_json::Value::Null;
    })?;
    assert!(error.contains("absent from the committed state"), "{error}");
    db.execute("UPDATE pending SET data=?1 WHERE id=1", [&original])?;
    drop(db);

    assert_eq!(crate::resume_transaction(&f.root)?, Some(2));
    assert!(verify(&f.root)?.is_empty());
    let image = crate::image::Image::open_query(&f.root)?;
    let state = image.state()?;
    assert_eq!(crate::verify::manifest_paths(&state)?, state.paths);
    assert_eq!(state.installed.len(), 2);
    Ok(())
}
