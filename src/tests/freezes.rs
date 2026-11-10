use super::*;

#[test]
fn package_freezes_constrain_updates_and_survive_offline_recovery() -> Result<()> {
    use crate::{image::Image, list_freezes, resume_transaction};

    let f = Fixture::new()?;
    let runtime = "library/runtime";
    let tool = "desktop/tool";
    f.package(
        runtime,
        "1.0.0-0",
        f.file(LIBRARY, "runtime 1", "0644", "")?,
    )?;
    f.package(
        tool,
        "1.0.0-0",
        f.file(COMPOSITOR, "tool 1", "0755", "")? + &dependency("require", runtime, "1.0.0-0"),
    )?;
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
    let stale = plan(&f.root, Request::Update)?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    let freeze = plan(
        &f.root,
        Request::Freeze {
            packages: vec![runtime.into()],
            reason: Some("keep known-good graphics ABI".into()),
        },
    )?;
    assert!(
        freeze
            .summary()
            .contains("Freeze library/runtime at 1.0.0-0")
    );
    assert!(
        freeze.writes.is_empty() && freeze.systemd.is_empty() && freeze.kernel_install.is_none()
    );
    assert_eq!(crate::fetch_plan(&f.root, &freeze)?, 0);
    let mut tampered = serde_json::to_value(&freeze)?;
    tampered["freeze_changes"] = serde_json::json!([]);
    assert!(apply(&f.root, &serde_json::from_value(tampered)?).is_err());
    assert_eq!(apply(&f.root, &freeze)?, 2);
    assert_eq!(apply(&f.root, &freeze)?, 2);
    assert_eq!(
        Image::open_query(&f.root)?.settings()?.format,
        crate::image::IMAGE_FORMAT
    );
    assert!(format!("{:#}", apply(&f.root, &stale).unwrap_err()).contains("stale plan"));
    let freezes = list_freezes(&f.root)?;
    assert_eq!(
        freezes[runtime].reason.as_deref(),
        Some("keep known-good graphics ABI")
    );
    let details = crate::transaction_details(&f.root, &freeze.id()?)?.details;
    assert_eq!(details.freeze_changes, freeze.freeze_changes);
    assert_eq!(details.planned_path_changes, 0);
    fs::rename(f.repo.with_extension("offline"), &f.repo)?;
    f.package(
        runtime,
        "2.0.0-0",
        f.file(LIBRARY, "runtime 2", "0644", "")?,
    )?;
    f.package(
        tool,
        "2.0.0-0",
        f.file(COMPOSITOR, "tool 2", "0755", "")?
            + &format!("    require {runtime:?} version=\">=1.0.0-0\"\n"),
    )?;
    f.package(
        tool,
        "3.0.0-0",
        f.file(COMPOSITOR, "tool 3", "0755", "")? + &dependency("require", runtime, "2.0.0-0"),
    )?;
    f.package(
        "desktop/new-abi",
        "1.0.0-0",
        dependency("require", runtime, "2.0.0-0"),
    )?;
    let error = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/new-abi".into()],
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("keep known-good graphics ABI"));
    let error = plan(
        &f.root,
        Request::Install {
            packages: vec![format!("{runtime}@2.0.0-0")],
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("is frozen at 1.0.0-0"));
    let policy = crate::query::package_policy(&f.root, &format!("{runtime}@2.0.0-0"), true)?;
    assert_eq!(policy.preferred_publisher.as_deref(), Some("canopa"));
    assert_eq!(policy.freeze.unwrap().version.to_string(), "1.0.0-0");
    assert_eq!(policy.publishers[0].candidates.len(), 2);
    assert!(matches!(
        policy.install_probe,
        Some(crate::query::InstallProbe::Blocked { reason }) if reason.contains("keep known-good graphics ABI")
    ));
    let mut held_back = Vec::new();
    let update = crate::plan::inspect_update(
        &f.root,
        crate::plan::UpdateDiagnostics {
            held_back: &mut held_back,
            explain: None,
            advisories: None,
        },
        &Default::default(),
    )?;
    assert_eq!(update, plan(&f.root, Request::Update)?);
    assert_eq!(held_back.len(), 2);
    assert_eq!(held_back[0].name, tool);
    assert_eq!(held_back[0].available.to_string(), "3.0.0-0");
    assert_eq!(
        held_back[0].selected.as_ref().unwrap().to_string(),
        "2.0.0-0"
    );
    assert_eq!(held_back[1].name, runtime);
    assert_eq!(held_back[1].available.to_string(), "2.0.0-0");
    assert_eq!(
        held_back[1].selected.as_ref().unwrap().to_string(),
        "1.0.0-0"
    );
    assert!(held_back[1].reason.contains("keep known-good graphics ABI"));
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("frozen-update.json"),
        &update,
        COMPOSITOR,
    )?;
    let db = rusqlite::Connection::open(f.root.join("usr/lib/sysimage/cps/state.sqlite"))?;
    let original: String =
        db.query_row("SELECT data FROM pending WHERE id=1", [], |row| row.get(0))?;
    let mut journal: serde_json::Value = serde_json::from_str(&original)?;
    journal["journaled"]["next"]["settings"]["freezes"][runtime]["reason"] = "tampered".into();
    let next: crate::image::StateDelta =
        serde_json::from_value(journal["journaled"]["next"].clone())?;
    journal["journaled"]["state_sha256"] = digest(&serde_json::to_vec(&next)?).into();
    db.execute(
        "UPDATE pending SET data=?1 WHERE id=1",
        [journal.to_string()],
    )?;
    assert!(
        format!("{:#}", resume_transaction(&f.root).unwrap_err())
            .contains("pending package freezes differ")
    );
    db.execute("UPDATE pending SET data=?1 WHERE id=1", [original])?;
    drop(db);
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert_eq!(resume_transaction(&f.root)?, Some(3));
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "tool 2");
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "runtime 1");
    assert_eq!(list_freezes(&f.root)?, freezes);
    assert!(verify(&f.root)?.is_empty());
    fs::rename(f.repo.with_extension("offline"), &f.repo)?;
    let blocked = crate::plan::inspect_update(
        &f.root,
        crate::plan::UpdateDiagnostics {
            held_back: &mut held_back,
            explain: None,
            advisories: None,
        },
        &Default::default(),
    )?;
    assert!(!blocked.has_changes());
    assert_eq!(held_back.len(), 2);
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Unfreeze {
                packages: vec![runtime.into()],
            },
        )?,
    )?;
    assert!(list_freezes(&f.root)?.is_empty());
    let update = crate::plan::inspect_update(
        &f.root,
        crate::plan::UpdateDiagnostics {
            held_back: &mut held_back,
            explain: None,
            advisories: None,
        },
        &Default::default(),
    )?;
    assert!(held_back.is_empty());
    apply(&f.root, &update)?;
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "runtime 2");
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "tool 3");
    Ok(())
}

#[test]
fn signed_profile_freezes_follow_staged_roots_and_require_explicit_unfreezing() -> Result<()> {
    use crate::{
        create_configured_image, image::Image, list_freezes, publish_release,
        stage_boot_environment,
    };
    let f = Fixture::new()?;
    let name = "desktop/base";
    f.package(name, "1.0.0-0", f.file(COMPOSITOR, "base 1", "0755", "")?)?;
    f.package(name, "2.0.0-0", f.file(COMPOSITOR, "base 2", "0755", "")?)?;
    let publish_profile = |version: &str| -> Result<()> {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {{ package "{name}@{version}"; }}
}}
"#
            ),
        )?;
        let definition = f._dir.path().join("release.kdl");
        fs::write(
            &definition,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                fs::read_to_string(f.repo.join("current"))?.trim()
            ),
        )?;
        publish_release(&f.repo, &f.private, &definition)?;
        Ok(())
    };
    publish_profile("1.0.0-0")?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-root {:?}; }}
"#,
            f.repo, f.trust
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    let freeze = |name: String| Request::Freeze {
        packages: vec![name],
        reason: Some("validate desktop compatibility".into()),
    };
    apply(&f.root, &plan(&f.root, freeze(format!("{name}@2.0.0-0")))?)?;
    assert!(
        format!("{:#}", plan(&f.root, Request::InstallProfile).unwrap_err())
            .contains("is frozen at 2.0.0-0")
    );
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Unfreeze {
                packages: vec![name.into()],
            },
        )?,
    )?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;
    apply(&f.root, &plan(&f.root, freeze(name.into()))?)?;
    fs::create_dir(f.root.join("boot"))?;
    fs::write(f.root.join("boot/vmlinuz"), "fixture kernel")?;
    let esp = f._dir.path().join("esp");
    fs::create_dir(&esp)?;
    let mut activation = crate::Activation {
        root_partuuid: "7adff456-af50-498f-b39a-dcf87cb4a7c1".into(),
        kernel: "boot/vmlinuz".into(),
        initrd: None,
        options: String::new(),
        tries: 2,
        known_good: true,
    };
    let base_id = crate::activate_boot_environment(&f.root, &esp, &activation)?;
    let freezes = list_freezes(&f.root)?;
    let mut held_back = Vec::new();
    assert!(
        !crate::plan::inspect_update(
            &f.root,
            crate::plan::UpdateDiagnostics {
                held_back: &mut held_back,
                explain: None,
                advisories: None,
            },
            &Default::default(),
        )?
        .has_changes()
    );
    assert!(
        held_back.is_empty(),
        "versions outside the signed profile are inapplicable"
    );
    publish_profile("2.0.0-0")?;
    let error = crate::plan::inspect_update(
        &f.root,
        crate::plan::UpdateDiagnostics {
            held_back: &mut held_back,
            explain: None,
            advisories: None,
        },
        &Default::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("validate desktop compatibility"));
    assert_eq!(held_back.len(), 1);
    assert_eq!(held_back[0].available.to_string(), "2.0.0-0");
    assert_eq!(held_back[0].selected, None);
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "base 1");
    let keep = plan(&f.root, freeze(name.into()))?;
    let held_root = f._dir.path().join("held-root");
    stage_boot_environment(&f.root, &held_root, &keep)?;
    assert_eq!(list_freezes(&held_root)?, freezes);
    assert!(plan(&held_root, Request::Update).is_err());
    let unfreeze = plan(
        &f.root,
        Request::Unfreeze {
            packages: vec![name.into()],
        },
    )?;
    let next_root = f._dir.path().join("next-root");
    stage_boot_environment(&f.root, &next_root, &unfreeze)?;
    assert!(list_freezes(&next_root)?.is_empty());
    let staged = Image::open_query(&next_root)?.settings()?;
    assert_eq!(
        staged.boot_parent.as_ref().unwrap().id,
        Image::open_query(&f.root)?.settings()?.id
    );
    apply(&next_root, &plan(&next_root, Request::Update)?)?;
    let after = Image::open_query(&next_root)?.settings()?;
    assert_eq!(after.id, staged.id);
    assert_eq!(after.boot_parent, staged.boot_parent);
    assert_eq!(after.revision, staged.revision + 1);
    activation.known_good = false;
    activation.root_partuuid = "b52060cf-81ab-4b8a-ad84-3aa69000ed49".into();
    let next_id = crate::activate_boot_environment(&next_root, &esp, &activation)?;
    assert!(
        crate::list_boot_environments(&esp)?
            .iter()
            .any(|entry| entry.id == next_id && entry.is_default)
    );
    assert_eq!(crate::rollback_boot_environment(&esp, None)?, base_id);
    assert_eq!(fs::read_to_string(next_root.join(COMPOSITOR))?, "base 2");
    assert_eq!(list_freezes(&f.root)?, freezes);
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "base 1");
    assert!(verify(&f.root)?.is_empty() && verify(&next_root)?.is_empty());
    Ok(())
}

#[test]
fn package_freezes_persist_after_removal_and_constrain_reinstallation() -> Result<()> {
    use crate::list_freezes;
    let f = Fixture::new()?;
    let name = "test/tool";
    f.package(name, "1.0.0-0", f.file(COMPOSITOR, "old", "0755", "")?)?;
    f.package(name, "2.0.0-0", f.file(COMPOSITOR, "new", "0755", "")?)?;
    f.create()?;
    let freeze = |packages: Vec<String>, reason| Request::Freeze { packages, reason };
    assert!(plan(&f.root, freeze(vec![name.into()], None)).is_err());
    assert!(
        plan(
            &f.root,
            freeze(vec![format!("{name}@1.0.0-0")], Some(String::new()))
        )
        .is_err()
    );
    assert!(
        plan(
            &f.root,
            freeze(
                vec![format!("{name}@1.0.0-0"), format!("{name}@2.0.0-0")],
                None
            )
        )
        .is_err()
    );
    apply(
        &f.root,
        &plan(&f.root, freeze(vec![format!("{name}@1.0.0-0")], None))?,
    )?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec![name.into()],
            },
        )?,
    )?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    let before = list_freezes(&f.root)?;
    assert!(plan(&f.root, freeze(vec![format!("{name}@2.0.0-0")], None)).is_err());
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::ExactInstall {
                packages: vec![name.into()],
            },
        )?,
    )?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Remove {
                packages: vec![name.into()],
            },
        )?,
    )?;
    assert_eq!(list_freezes(&f.root)?, before);
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec![name.into()],
            },
        )?,
    )?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    assert!(
        plan(
            &f.root,
            Request::Unfreeze {
                packages: vec![name.into(), "test/missing".into()]
            }
        )
        .is_err()
    );
    assert_eq!(list_freezes(&f.root)?, before);
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}
