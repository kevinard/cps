use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;

#[test]
fn special_modes_require_protected_executables_and_meaningful_directory_bits() -> Result<()> {
    use crate::kdl_format::{manifest_text, parse_manifest};
    let package = |action: &str| {
        parse_manifest(&format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"test/modes\" publisher=\"canopa\" version=\"1.0.0-0\" {{ {action}; }}\n"
        ))
    };
    for action in [
        "dir \"tmp\" mode=\"1777\" uid=0 gid=0",
        "dir \"shared\" mode=\"2775\" uid=1000 gid=1000",
        "dir \"shared\" mode=\"3770\" uid=0 gid=42",
        "file \"bin/helper\" mode=\"4755\" uid=0 gid=0",
        "file \"bin/helper\" mode=\"2750\" uid=0 gid=42",
        "file \"bin/helper\" mode=\"6755\" uid=0 gid=0",
    ] {
        let manifest = package(action)?;
        manifest.validate(false)?;
        assert_eq!(parse_manifest(&manifest_text(&manifest))?, manifest);
    }
    for action in [
        "dir \"tmp\" mode=\"4755\" uid=0 gid=0",
        "dir \"tmp\" mode=\"7777\" uid=0 gid=0",
        "dir \"tmp\" mode=\"755\" uid=0 gid=0",
        "dir \"tmp\" mode=\"01777\" uid=0 gid=0",
        "dir \"tmp\" mode=\"1787\" uid=0 gid=0",
        "file \"bin/helper\" mode=\"1755\" uid=0 gid=0",
        "file \"bin/helper\" mode=\"4755\" uid=1000 gid=0",
        "file \"bin/helper\" mode=\"4775\" uid=0 gid=0",
        "file \"bin/helper\" mode=\"4757\" uid=0 gid=0",
        "file \"bin/helper\" mode=\"4644\" uid=0 gid=0",
        "file \"bin/helper\" mode=\"4755\" uid=0 gid=0 preserve=#true",
        "file \"bin/helper\" mode=\"6755\" uid=0 gid=0 capabilities=\"cap_net_raw=ep\"",
    ] {
        assert!(package(action)?.validate(false).is_err(), "{action}");
    }
    Ok(())
}

#[test]
fn generated_manifests_capture_directory_and_protected_setid_modes() -> Result<()> {
    let f = Fixture::new()?;
    fs::create_dir(f.proto.join("tmp"))?;
    fs::set_permissions(f.proto.join("tmp"), fs::Permissions::from_mode(0o1777))?;
    fs::create_dir(f.proto.join("shared"))?;
    fs::set_permissions(f.proto.join("shared"), fs::Permissions::from_mode(0o2775))?;
    fs::write(f.proto.join("helper"), "fixture executable")?;
    fs::set_permissions(f.proto.join("helper"), fs::Permissions::from_mode(0o6755))?;
    fs::hard_link(f.proto.join("helper"), f.proto.join("helper-alias"))?;
    let generate =
        |uid| crate::generate_manifest(&f.proto, "canopa", "test/modes", "1.0.0-0", uid, 0);
    let generated = generate(0)?;
    for mode in ["1777", "2775", "6755"] {
        assert!(generated.contains(&format!("mode=\"{mode}\"")));
    }
    assert!(generated.contains("hardlink"));
    assert_eq!(generated, generate(0)?);
    assert!(format!("{:#}", generate(1000).unwrap_err()).contains("root-owned executable"));
    Ok(())
}

#[test]
fn directory_special_modes_survive_updates_offline_recovery_repair_and_staging() -> Result<()> {
    let f = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let package = |version, tmp, shared, content| {
        f.package(
            "system/filesystem",
            version,
            format!(
                "    dir \"tmp\" mode=\"{tmp}\" uid={uid} gid={gid}\n    dir \"var/shared\" mode=\"{shared}\" uid={uid} gid={gid}\n"
            ) + &f.file(COMPOSITOR, content, "0755", "")?,
        )
    };
    let mode = |path: &Path| -> Result<u32> { Ok(fs::metadata(path)?.mode() & 0o7777) };
    package("1.0.0-0", "1777", "2770", "first")?;
    f.create()?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec!["system/filesystem".into()],
            },
        )?,
    )?;
    let tmp = f.root.join("tmp");
    let shared = f.root.join("var/shared");
    assert_eq!(mode(&tmp)?, 0o1777);
    assert_eq!(mode(&shared)?, 0o2770);
    let inode = fs::metadata(&shared)?.ino();
    fs::write(shared.join("local"), "local state")?;
    package("2.0.0-0", "1770", "3770", "second")?;
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o2775))?;
    assert!(plan(&f.root, Request::Update).is_err());
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o2770))?;
    let update = plan(&f.root, Request::Update)?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("update.json"),
        &update,
        "var/shared",
    )?;
    assert_eq!(mode(&shared)?, 0o3770);
    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    assert_eq!(crate::resume_transaction(&f.root)?, Some(2));
    assert!(verify(&f.root)?.is_empty());
    assert_eq!(fs::metadata(&shared)?.ino(), inode);
    assert_eq!(mode(&tmp)?, 0o1770);
    assert_eq!(fs::read_to_string(shared.join("local"))?, "local state");
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o770))?;
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o770))?;
    assert!(!verify(&f.root)?.is_empty());
    let repair = plan(&f.root, Request::Repair { packages: vec![] })?;
    apply(&f.root, &repair)?;
    assert_eq!(mode(&tmp)?, 0o1770);
    assert_eq!(mode(&shared)?, 0o3770);
    assert_eq!(fs::metadata(&shared)?.ino(), inode);
    assert!(verify(&f.root)?.is_empty());
    fs::rename(&offline, &f.repo)?;
    let staged = f._dir.path().join("staged");
    let keep = plan(
        &f.root,
        Request::Install {
            packages: vec!["system/filesystem".into()],
        },
    )?;
    crate::stage_boot_environment(&f.root, &staged, &keep)?;
    assert_eq!(mode(&staged.join("tmp"))?, 0o1770);
    assert_eq!(mode(&staged.join("var/shared"))?, 0o3770);
    assert_eq!(
        fs::read_to_string(staged.join("var/shared/local"))?,
        "local state"
    );
    assert!(verify(&staged)?.is_empty());
    Ok(())
}
