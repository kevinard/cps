use std::{num::NonZeroU64, path::Path};

use jiff::Timestamp;
use tough::{
    editor::{RepositoryEditor, signed::PathExists},
    error::Error as TufError,
    key_source::{KeySource, LocalKeySource},
    schema::{Role, RoleType, Root, Signature, Signed, Target},
    sign::{Sign, parse_keypair},
};
use url::Url;

use super::*;
use crate::{
    attestation::{self, LegacyToolchain, Toolchain},
    create_configured_image, fetch_plan,
    kdl_format::{build, parse_manifest},
    list_obsoletions, list_withdrawals,
    model::FileDigest,
    publish_bundle, publish_release,
    query::{Scope, Status, list_packages, package_manifest},
    read_attestation, release as mirror_release,
    repo::{read_key, read_signing_key, snapshot},
    rotate_publisher_key, withdraw_package,
};

fn attach_evidence(fixture: &Fixture, catalog: &str) -> Result<(String, Vec<u8>)> {
    let builder = Fixture::new()?;
    let bundle = fixture._dir.path().join("bundle");
    fs::create_dir_all(bundle.join("proto/usr/bin"))?;
    let manifest_bytes = fs::read(&fixture.recipe)?;
    let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)?;
    let recipe = format!(
        "schema-version 1\nbuild architecture=\"x86_64\" epoch=1700000000 {{\n\
         toolchain \"{catalog}\" {{\n package \"desktop/compositor\"\n }}\n\
         manifest \"manifest.kdl\" sha256=\"{}\"\n script \"true\"\n}}\n",
        digest(&manifest_bytes),
    );
    let key = read_key(&fixture.public)?;
    let release = snapshot(&fixture.repo, "canopa", &key, Some(catalog))?;
    let payload = fs::read(fixture.proto.join(COMPOSITOR))?;
    let envelope = attestation::sign(
        &read_signing_key(&builder.private)?,
        &manifest,
        &manifest_bytes,
        &build::parse(&recipe)?,
        recipe.as_bytes(),
        Toolchain::Legacy(LegacyToolchain {
            publisher: "canopa".to_owned(),
            key_sha256: digest(&key),
            catalog_sha256: release.sha256,
            package_manifests: BTreeMap::from([(
                "desktop/compositor".to_owned(),
                release.packages[0].sha256.clone(),
            )]),
        }),
        &BTreeMap::from([(
            COMPOSITOR.to_owned(),
            FileDigest {
                sha256: digest(&payload),
                size: payload.len() as u64,
            },
        )]),
    )?;
    fs::write(bundle.join("build.kdl"), &recipe)?;
    fs::write(bundle.join("manifest.kdl"), manifest_bytes)?;
    fs::write(bundle.join("attestation.json"), &envelope)?;
    fs::write(bundle.join("proto").join(COMPOSITOR), payload)?;
    let id = publish_bundle(
        &fixture.repo,
        &bundle,
        &fixture.private,
        &builder.public,
        &digest(recipe.as_bytes()),
    )?;
    Ok((id, envelope))
}

fn sign_root(path: &Path, private: &Path, predecessor: Option<&Path>) -> Result<()> {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("generate TUF key");
    fs::write(private, document.as_ref())?;
    let key = parse_keypair(document.as_ref())?.tuf_key();
    let key_id = key.key_id()?.clone();
    let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("read TUF key");
    let role = serde_json::from_value::<tough::schema::RoleKeys>(serde_json::json!({
        "keyids": [key_id],
        "threshold": 1,
    }))?;
    let root = Root {
        spec_version: "1.0.0".to_owned(),
        consistent_snapshot: true,
        version: NonZeroU64::new(if predecessor.is_some() { 2 } else { 1 }).unwrap(),
        expires: Timestamp::now().checked_add(jiff::Span::new().hours(720))?,
        keys: [(key_id.clone(), key)].into_iter().collect(),
        roles: [
            RoleType::Root,
            RoleType::Timestamp,
            RoleType::Snapshot,
            RoleType::Targets,
        ]
        .into_iter()
        .map(|kind| (kind, role.clone()))
        .collect(),
        _extra: Default::default(),
    };
    let canonical = root.canonical_form()?;
    let signature = pair.sign(&canonical);
    let mut root = Signed {
        signed: root,
        signatures: vec![Signature {
            keyid: key_id,
            sig: signature.as_ref().to_vec().into(),
        }],
    };
    if let Some(predecessor) = predecessor {
        let bytes = fs::read(predecessor)?;
        let key = parse_keypair(&bytes)?.tuf_key();
        let pair = Ed25519KeyPair::from_pkcs8(&bytes).expect("read predecessor TUF key");
        root.signatures.push(Signature {
            keyid: key.key_id()?.clone(),
            sig: pair.sign(&canonical).as_ref().to_vec().into(),
        });
    }
    fs::write(path, serde_json::to_vec(&root)?)?;
    Ok(())
}

#[test]
fn release_batch_checks_its_base_and_keeps_withdrawals_atomic() -> Result<()> {
    let f = Fixture::new()?;
    f.package("test/one", "1.0.0-0", String::new())?;
    f.package("test/two", "1.0.0-0", String::new())?;
    let pointer = f.repo.join("current");
    let base = fs::read_to_string(&pointer)?;
    let release = f._dir.path().join("release.kdl");
    let write = |second: &str| {
        fs::write(
            &release,
            format!(
                "schema-version 1\nrelease \"canopa\" base={:?} {{\n    withdraw \"cps://canopa/test/one@1.0.0-0\"\n    withdraw {second:?}\n}}\n",
                base.trim(),
            ),
        )
    };
    write("cps://canopa/test/missing@1.0.0-0")?;
    let error = publish_release(&f.repo, &f.private, &release).unwrap_err();
    assert!(error.to_string().contains("no published package"));
    assert_eq!(fs::read_to_string(&pointer)?, base);
    assert!(list_withdrawals(&f.repo, "canopa", &f.public)?.is_empty());

    write("cps://canopa/test/one@1.0.0-0")?;
    let error = publish_release(&f.repo, &f.private, &release).unwrap_err();
    assert!(format!("{error:#}").contains("duplicate release action"));

    write("cps://canopa/test/two@1.0.0-0")?;
    let committed = publish_release(&f.repo, &f.private, &release)?;
    assert_eq!(fs::read_to_string(&pointer)?.trim(), committed);
    assert_eq!(list_withdrawals(&f.repo, "canopa", &f.public)?.len(), 2);
    let key = read_key(&f.public)?;
    assert!(snapshot(&f.repo, "canopa", &key, None)?.packages.is_empty());
    assert_eq!(
        snapshot(&f.repo, "canopa", &key, Some(base.trim()))?
            .packages
            .len(),
        2
    );

    let error = publish_release(&f.repo, &f.private, &release).unwrap_err();
    assert!(error.to_string().contains("release base is stale"));
    assert_eq!(fs::read_to_string(&pointer)?.trim(), committed);
    Ok(())
}

#[test]
fn release_profiles_pin_latest_roots_and_use_image_facets() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(MANPAGE, "base documentation", "0644", "facet=\"doc.man\"")?,
    )?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        dependency("require", "library/unavailable", "1.0.0-0"),
    )?;
    f.package(
        "desktop/manual",
        "1.0.0-0",
        f.file(MANPAGE, "shared documentation", "0644", "")?,
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    let old = fs::read_to_string(f.repo.join("current"))?;
    let id = "cps://canopa/test/retire@1.0.0-0";
    let write_policy = |root: &str, docs: bool| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                "schema-version 1\npublisher \"canopa\" {{\n    builder-key \"builder.der\"\n    profile \"desktop\" architecture=\"x86_64\" {{\n        package {root:?}\n        package \"desktop/manual\"\n        facet \"doc.man\" #{docs}\n    }}\n}}\n",
            ),
        )
    };

    write_policy("desktop/compositor", false)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    assert!(format!("{error:#}").contains("library/unavailable"));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("desktop/compositor@1.0.0-0", true)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    assert!(format!("{error:#}").contains(MANPAGE));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("desktop/compositor@1.0.0-0", false)?;
    withdraw_package(&f.repo, &f.private, id)?;
    assert!(list_withdrawals(&f.repo, "canopa", &f.public)?.contains_key(id));
    Ok(())
}

#[test]
fn signed_composition_replays_only_its_manifest_digests() -> Result<()> {
    let f = Fixture::new()?;
    f.package("library/runtime", "1.0.0-0", String::new())?;
    f.package("library/runtime", "2.0.0-0", String::new())?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        "    require \"library/runtime\"\n".to_owned(),
    )?;
    let release = snapshot(&f.repo, "canopa", &read_key(&f.public)?, None)?;
    let version = "1.0.0-0".parse()?;
    let composition = release
        .packages
        .iter()
        .filter(|package| package.manifest.version == version)
        .map(|package| (package.manifest.name.clone(), package.sha256.clone()))
        .collect();
    let profile = crate::repo::ReleaseProfile {
        architecture: "x86_64".to_owned(),
        facets: BTreeMap::new(),
        companions: Vec::new(),
        roots: BTreeMap::from([("desktop/compositor".to_owned(), version)]),
        composition,
        elf_library_dirs: Vec::new(),
        elf_symbol_versions: false,
    };
    let mut candidates = crate::plan::CandidateSet {
        packages: release.packages,
        obsoletions: BTreeMap::new(),
    };
    let installed = candidates.replay(&profile)?;
    let locked = &profile.composition["library/runtime"];
    assert_eq!(&installed["library/runtime"].published.sha256, locked);
    candidates
        .packages
        .retain(|package| &package.sha256 != locked);
    let error = candidates.replay(&profile).unwrap_err();
    assert!(
        format!("{error:#}").contains("signed profile package library/runtime"),
        "{error:#}"
    );
    f.create()?;
    let error = plan(&f.root, Request::InstallProfile).unwrap_err();
    assert!(format!("{error:#}").contains("requires a configured signed release profile"));
    Ok(())
}

#[test]
fn release_upgrade_profiles_check_retained_packages_and_path_transitions() -> Result<()> {
    let f = Fixture::new()?;
    f.package("library/legacy", "1.0.0-0", String::new())?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        dependency("require", "library/legacy", "1.0.0-0"),
    )?;
    f.package(
        "desktop/session",
        "1.0.0-0",
        dependency("require", "library/legacy", "1.0.0-0"),
    )?;
    f.package(
        "test/layout",
        "1.0.0-0",
        "    dir \"opt/layout\" mode=\"0755\" uid=0 gid=0\n".to_owned(),
    )?;
    let baseline = fs::read_to_string(f.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        dependency("exclude", "library/legacy", "1.0.0-0"),
    )?;
    f.package("desktop/session", "2.0.0-0", String::new())?;
    f.package(
        "test/layout",
        "2.0.0-0",
        f.file("opt/layout", "replacement", "0644", "")?,
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    let old = fs::read_to_string(f.repo.join("current"))?;
    let id = "cps://canopa/test/retire@1.0.0-0";
    let write_policy = |root: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                "schema-version 1\npublisher \"canopa\" {{\n    builder-key \"builder.der\"\n    profile \"desktop\" architecture=\"x86_64\" upgrade-from={:?} {{\n        package {root:?}\n    }}\n}}\n",
                baseline.trim(),
            ),
        )
    };
    write_policy("desktop/compositor")?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("update selects cps://canopa/desktop/compositor@1.0.0-0 instead of cps://canopa/desktop/compositor@2.0.0-0"), "{message}");
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("desktop/session")?;
    let error =
        withdraw_package(&f.repo, &f.private, "cps://canopa/library/legacy@1.0.0-0").unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("update retains unavailable package cps://canopa/library/legacy@1.0.0-0"),
        "{message}"
    );
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("test/layout")?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("cannot change between directory and leaf actions at opt/layout"),
        "{message}"
    );
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);
    write_policy("desktop/compositor")?;
    let release = f._dir.path().join("obsolete.kdl");
    fs::write(
        &release,
        format!(
            "schema-version 1\nrelease \"canopa\" base={:?} {{\n    obsolete \"cps://canopa/library/legacy@2.0.0-0\"\n}}\n",
            old.trim(),
        ),
    )?;
    publish_release(&f.repo, &f.private, &release)?;
    assert_eq!(
        list_obsoletions(&f.repo, "canopa", &f.public)?,
        ["cps://canopa/library/legacy@2.0.0-0"]
    );
    Ok(())
}

#[test]
fn upgrade_profile_reuses_signed_baseline_roots_and_companions() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let root_dir = format!("    dir \"opt/pinned-root\" mode=\"0755\" uid={uid} gid={gid}\n");
    let companion_dir =
        format!("    dir \"opt/pinned-companion\" mode=\"0755\" uid={uid} gid={gid}\n");
    let runtime_requirement = "    require \"library/runtime\"\n";
    seed.package_as("seed", "library/runtime", "1.0.0-0", companion_dir.clone())?;
    let old_pin = fs::read_to_string(seed.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        format!("{runtime_requirement}{root_dir}"),
    )?;
    let replacement = f.file("opt/pinned-root", "replacement", "0644", "")?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        format!("{runtime_requirement}{replacement}"),
    )?;
    f.package("desktop/addon", "1.0.0-0", String::new())?;
    f.package("desktop/addon", "2.0.0-0", String::new())?;
    f.package(
        "desktop/runtime-plugin",
        "1.0.0-0",
        dependency("require", "library/runtime", "1.0.0-0"),
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;

    let write_policy = |version: &str, pin: &str, base: &str, override_pin: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" {base} {{
        package "desktop/compositor@{version}"
        publisher "seed" catalog={:?} {override_pin} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
                pin.trim(),
                seed.repo,
                seed.public,
            ),
        )
    };
    let pointer = f.repo.join("current");
    let unsigned_base = fs::read_to_string(&pointer)?;
    write_policy("1.0.0-0", &old_pin, "", "")?;
    withdraw_package(&f.repo, &f.private, "cps://canopa/test/retire@1.0.0-0")?;
    let signed_base = fs::read_to_string(&pointer)?;
    let base = format!("upgrade-from={:?}", signed_base.trim());
    let refresh = f._dir.path().join("refresh.kdl");
    fs::write(
        &refresh,
        format!(
            "schema-version 1\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
            signed_base.trim()
        ),
    )?;
    let rejected = |release: &Path, expected: &str| -> Result<()> {
        let error = publish_release(&f.repo, &f.private, release).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(fs::read_to_string(&pointer)?, signed_base);
        Ok(())
    };
    let policy = f.repo.join("publish-policy.kdl");
    fs::write(
        &policy,
        fs::read_to_string(&policy)?.replace("architecture=\"x86_64\"", "architecture=\"aarch64\""),
    )?;
    rejected(
        &refresh,
        "image configuration differs from the signed baseline",
    )?;
    write_policy("1.0.0-0", &old_pin, "", "")?;
    assert_eq!(
        publish_release(&f.repo, &f.private, &refresh)?,
        signed_base.trim()
    );

    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{
    repository {:?}
    trust-key {:?}
}}
publisher "seed" {{
    repository {:?}
    trust-key {:?}
}}
"#,
            f.repo, f.public, seed.repo, seed.public,
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;
    assert!(f.root.join("opt/pinned-root").is_dir());
    assert!(f.root.join("opt/pinned-companion").is_dir());
    let installed = list_packages(&f.root, Scope::Installed)?;
    assert_eq!(installed.len(), 2);
    assert!(installed.iter().any(|row| {
        row.id == "cps://canopa/desktop/compositor@1.0.0-0" && row.status == Status::Explicit
    }));
    assert!(installed.iter().any(|row| {
        row.id == "cps://seed/library/runtime@1.0.0-0" && row.status == Status::Dependency
    }));
    let error = plan(&f.root, Request::InstallProfile).unwrap_err();
    assert!(format!("{error:#}").contains("requires an image with no installed packages"));
    let same_release = plan(&f.root, Request::Update)?;
    assert_eq!(same_release.request, Request::UpdateProfile);
    assert!(!same_release.summary().contains("Update "));
    let newer_root = f._dir.path().join("locally-newer-image");
    create_configured_image(&newer_root, &config)?;
    let manual = plan(
        &newer_root,
        Request::Install {
            packages: vec!["desktop/compositor@2.0.0-0".to_owned()],
        },
    )?;
    apply(&newer_root, &manual)?;
    let error = plan(&newer_root, Request::Update).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("signed profile would downgrade cps://canopa/desktop/compositor@2.0.0-0"),
        "{error:#}"
    );
    let legacy = {
        let image = crate::image::Image::open(&newer_root, false)?;
        crate::plan::build(
            &image.root,
            image.state()?,
            Request::Update,
            Some(&manual),
            manual.solver,
        )?
        .0
    };
    assert_eq!(legacy.request, Request::Update);
    apply(&newer_root, &legacy)?;
    package_manifest(
        &newer_root,
        "cps://canopa/desktop/compositor@2.0.0-0",
        false,
    )?;

    let conflicting_root = f._dir.path().join("conflicting-image");
    create_configured_image(&conflicting_root, &config)?;
    apply(
        &conflicting_root,
        &plan(&conflicting_root, Request::InstallProfile)?,
    )?;
    let plugin = plan(
        &conflicting_root,
        Request::Install {
            packages: vec!["desktop/runtime-plugin".to_owned()],
        },
    )?;
    apply(&conflicting_root, &plugin)?;
    let addon = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/addon@1.0.0-0".to_owned()],
        },
    )?;
    apply(&f.root, &addon)?;

    write_policy("2.0.0-0", &old_pin, "", "")?;
    rejected(
        &refresh,
        "cannot change between directory and leaf actions at opt/pinned-root",
    )?;
    // The older catalog would select 2.0 in its baseline, so only the
    // mandatory check from the signed profile at 1.0 detects the transition.
    let older = format!("upgrade-from={:?}", unsigned_base.trim());
    write_policy("2.0.0-0", &old_pin, &older, "")?;
    rejected(
        &refresh,
        "cannot change between directory and leaf actions at opt/pinned-root",
    )?;

    // Withdraw the rejected 2.0 layout so the next release can update the companion alone.
    let retire_newer = f._dir.path().join("withdraw-newer.kdl");
    fs::write(
        &retire_newer,
        format!(
            "schema-version 1\nrelease \"canopa\" base={:?} {{\n    withdraw \"cps://canopa/desktop/compositor@2.0.0-0\"\n}}\n",
            signed_base.trim()
        ),
    )?;
    let replacement = seed.file("opt/pinned-companion", "replacement", "0644", "")?;
    seed.package_as("seed", "library/runtime", "2.0.0-0", replacement)?;
    let new_pin = fs::read_to_string(seed.repo.join("current"))?;
    write_policy(
        "1.0.0-0",
        &new_pin,
        &base,
        &format!("upgrade-from={:?}", new_pin.trim()),
    )?;
    rejected(&retire_newer, "differs from the signed baseline pin")?;
    write_policy("1.0.0-0", &new_pin, "", "")?;
    rejected(
        &retire_newer,
        "cannot change between directory and leaf actions at opt/pinned-companion",
    )?;

    seed.package_as("seed", "library/runtime", "3.0.0-0", companion_dir)?;
    let latest_pin = fs::read_to_string(seed.repo.join("current"))?;
    write_policy("1.0.0-0", &latest_pin, "", "")?;
    let next = publish_release(&f.repo, &f.private, &retire_newer)?;
    write_policy("1.0.0-0", &latest_pin, &older, "")?;
    fs::write(
        &refresh,
        format!("schema-version 1\nrelease \"canopa\" base={next:?} refresh-profiles=#true\n"),
    )?;
    let error = publish_release(&f.repo, &f.private, &refresh).unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!("checking upgrade from {}", unsigned_base.trim())),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(&pointer)?.trim(), next);
    let error = plan(&conflicting_root, Request::Update).unwrap_err();
    assert!(
        format!("{error:#}").contains("library/runtime"),
        "{error:#}"
    );
    package_manifest(
        &conflicting_root,
        "cps://seed/library/runtime@1.0.0-0",
        false,
    )?;
    let upgrade = plan(&f.root, Request::Update)?;
    assert_eq!(upgrade.request, Request::UpdateProfile);
    assert_eq!(upgrade.catalogs["seed"], latest_pin.trim());
    let staged = f._dir.path().join("staged");
    crate::stage_boot_environment(&f.root, &staged, &upgrade)?;
    assert!(staged.join("opt/pinned-root").is_dir());
    assert!(staged.join("opt/pinned-companion").is_dir());
    package_manifest(&staged, "cps://seed/library/runtime@3.0.0-0", false)?;
    package_manifest(&f.root, "cps://seed/library/runtime@1.0.0-0", false)?;
    assert!(list_packages(&staged, Scope::Installed)?.iter().any(|row| {
        row.id == "cps://canopa/desktop/addon@2.0.0-0" && row.status == Status::Explicit
    }));
    Ok(())
}

#[test]
fn release_profiles_pin_and_authenticate_sticky_companion_publishers() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    seed.package_as("seed", "library/runtime", "1.0.0-0", String::new())?;
    let seed_baseline = fs::read_to_string(seed.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        dependency("require", "library/runtime", "1.0.0-0"),
    )?;
    let baseline = fs::read_to_string(f.repo.join("current"))?;
    seed.package_as("seed", "library/runtime", "2.0.0-0", String::new())?;
    let seed_current = fs::read_to_string(seed.repo.join("current"))?;
    f.package("library/runtime", "2.0.0-0", String::new())?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        dependency("require", "library/runtime", "2.0.0-0"),
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    let pointer = f.repo.join("current");
    let old = fs::read_to_string(&pointer)?;
    let id = "cps://canopa/test/retire@1.0.0-0";
    let write_policy = |pin: &str, key: &Path| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" upgrade-from={:?} {{
        package "desktop/compositor"
        publisher "seed" catalog={:?} upgrade-from={:?} {{
            repository {:?}
            trust-key {key:?}
        }}
    }}
}}
"#,
                baseline.trim(),
                pin.trim(),
                seed_baseline.trim(),
                seed.repo,
            ),
        )
    };

    write_policy(&seed_current, &f.public)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    assert!(format!("{error:#}").contains("invalid signature on catalog"));
    assert_eq!(fs::read_to_string(&pointer)?, old);

    write_policy(&seed_baseline, &seed.public)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("update selects cps://canopa/desktop/compositor@1.0.0-0 instead of cps://canopa/desktop/compositor@2.0.0-0"), "{message}");
    assert_eq!(fs::read_to_string(&pointer)?, old);

    write_policy(&seed_current, &seed.public)?;
    fs::remove_file(seed.repo.join("current"))?;
    withdraw_package(&f.repo, &f.private, id)?;
    assert!(list_withdrawals(&f.repo, "canopa", &f.public)?.contains_key(id));
    Ok(())
}

#[test]
fn profiled_image_follows_signed_catalog_set_and_recovers_saved_plans() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let payload = "usr/lib/runtime.marker";
    seed.package_as(
        "seed",
        "library/runtime",
        "1.0.0-0",
        seed.file(payload, "old", "0644", "")?,
    )?;
    let old_pin = fs::read_to_string(seed.repo.join("current"))?;
    seed.package_as(
        "seed",
        "library/runtime",
        "2.0.0-0",
        seed.file(payload, "new", "0644", "")?,
    )?;
    let new_pin = fs::read_to_string(seed.repo.join("current"))?;
    // Create a same-sequence fork and extend it without the real second release.
    fs::write(seed.repo.join("current"), &old_pin)?;
    seed.package_as("seed", "library/fork", "1.0.0-0", String::new())?;
    let equal_fork = fs::read_to_string(seed.repo.join("current"))?;
    seed.package_as("seed", "library/fork", "2.0.0-0", String::new())?;
    let newer_fork = fs::read_to_string(seed.repo.join("current"))?;
    fs::write(seed.repo.join("current"), &new_pin)?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        "    require \"library/runtime\"\n".to_owned(),
    )?;
    let pointer = f.repo.join("current");
    let base = fs::read_to_string(&pointer)?;
    let refresh = |base: &str| -> Result<String> {
        let path = f._dir.path().join("refresh.kdl");
        fs::write(
            &path,
            format!(
                "schema-version 1\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim()
            ),
        )?;
        publish_release(&f.repo, &f.private, &path)
    };
    let error = refresh(&base).unwrap_err();
    assert!(
        format!("{error:#}").contains("refresh-profiles requires release composition profiles")
    );
    assert_eq!(fs::read_to_string(&pointer)?, base);
    let config = f._dir.path().join("image.kdl");
    let write_config = |arch: &str| {
        fs::write(
            &config,
            format!(
                r#"schema-version 1
architecture {arch:?}
release-profile "desktop"
publisher "canopa" {{
    repository {:?}
    trust-key {:?}
}}
publisher "seed" {{
    repository {:?}
    trust-key {:?}
}}
"#,
                f.repo, f.public, seed.repo, seed.public
            ),
        )
    };
    let write_policy = |pin: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/compositor"
        publisher "seed" catalog={:?} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
                pin.trim(),
                seed.repo,
                seed.public
            ),
        )
    };
    write_policy(&old_pin)?;
    let first = refresh(&base)?;
    assert_eq!(refresh(&first)?, first);
    write_config("aarch64")?;
    let error = create_configured_image(&f.root, &config).unwrap_err();
    assert!(format!("{error:#}").contains("architecture or facets differ"));
    assert!(!f.root.exists());
    write_config("x86_64")?;
    create_configured_image(&f.root, &config)?;
    let error = crate::prefer_publisher(&f.root, "seed").unwrap_err();
    assert!(format!("{error:#}").contains("fixes this image's publishers"));
    let available = list_packages(&f.root, Scope::Available)?;
    assert!(
        available
            .iter()
            .any(|entry| entry.id == "cps://seed/library/runtime@1.0.0-0")
    );
    assert!(package_manifest(&f.root, "cps://seed/library/runtime@2.0.0-0", true).is_err());
    let saved = plan(&f.root, Request::InstallProfile)?;
    assert_eq!(saved.catalogs["seed"], old_pin.trim());
    let mut altered = saved.clone();
    altered
        .catalogs
        .insert("seed".to_owned(), new_pin.trim().to_owned());
    let error = apply(&f.root, &altered).unwrap_err();
    assert!(format!("{error:#}").contains("differs from the signed release profile"));
    assert!(!f.root.join(payload).exists());

    write_policy(&digest(b"missing companion"))?;
    let error = refresh(&first).unwrap_err();
    assert!(format!("{error:#}").contains("reading companion catalog"));
    assert_eq!(fs::read_to_string(&pointer)?.trim(), first);
    write_policy(&new_pin)?;
    let error = refresh(&base).unwrap_err();
    assert!(format!("{error:#}").contains("release base is stale"));
    let second = refresh(&first)?;
    assert_ne!(first, second);
    for (pin, expected) in [
        (&old_pin, "publisher catalog rollback or fork"),
        (&equal_fork, "publisher catalog rollback or fork"),
        (
            &newer_fork,
            "release drops or changes the published manifest",
        ),
    ] {
        write_policy(pin)?;
        let error = refresh(&second).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(fs::read_to_string(&pointer)?.trim(), second);
    }
    let rotated = Fixture::new()?;
    let rotated_pin = rotate_publisher_key(
        &seed.repo,
        "seed",
        new_pin.trim(),
        &seed.private,
        &rotated.private,
    )?;
    write_policy(&rotated_pin)?;
    let third = refresh(&second)?;
    assert_ne!(second, third);
    fs::remove_file(seed.repo.join("current"))?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read_to_string(f.root.join(payload))?, "old");
    let upgrade = plan(&f.root, Request::Update)?;
    assert_eq!(upgrade.catalogs["seed"], rotated_pin);
    let staged = f._dir.path().join("staged");
    crate::stage_boot_environment(&f.root, &staged, &upgrade)?;
    assert_eq!(fs::read_to_string(staged.join(payload))?, "new");
    assert_eq!(fs::read_to_string(f.root.join(payload))?, "old");
    Ok(())
}

fn sign_release(
    fixture: &Fixture,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
) -> Result<()> {
    sign_publisher_release(fixture, mirror, version, expires, root, tuf_key, "canopa")
}

fn sign_publisher_release(
    fixture: &Fixture,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
    publisher: &str,
) -> Result<()> {
    fs::create_dir(mirror)?;
    let export = mirror.join("export");
    mirror_release::export(&fixture.repo, publisher, &fixture.public, &export)?;
    sign_export(&export, mirror, version, expires, root, tuf_key)
}

fn sign_export(
    export: &Path,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
) -> Result<()> {
    let version = NonZeroU64::new(version).expect("nonzero release version");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut editor = RepositoryEditor::new(root).await?;
            editor
                .targets_expires(expires)?
                .targets_version(version)?
                .snapshot_expires(expires)
                .snapshot_version(version)
                .timestamp_expires(expires)
                .timestamp_version(version);
            for entry in fs::read_dir(export)? {
                let entry = entry?;
                let name = entry.file_name().into_string().expect("ASCII target name");
                editor.add_target(name, Target::from_path(entry.path()).await?)?;
            }
            let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource {
                path: tuf_key.to_path_buf(),
            })];
            let signed = editor.sign(&keys).await?;
            signed.write(mirror.join("metadata")).await?;
            signed
                .copy_targets(export, mirror.join("targets"), PathExists::Fail)
                .await?;
            Ok(())
        })
}

#[test]
fn tuf_retained_catalogs_bootstrap_profiled_images_without_rewinding_current() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let payload = "usr/lib/runtime.marker";
    seed.package_as(
        "seed",
        "library/runtime",
        "1.0.0-0",
        seed.file(payload, "old", "0644", "")?,
    )?;
    let previous = fs::read_to_string(seed.repo.join("current"))?;
    seed.package_as(
        "seed",
        "library/runtime",
        "2.0.0-0",
        seed.file(payload, "new", "0644", "")?,
    )?;
    withdraw_package(
        &seed.repo,
        &seed.private,
        "cps://seed/library/runtime@1.0.0-0",
    )?;
    let current = fs::read_to_string(seed.repo.join("current"))?;

    let work = f._dir.path();
    let mirror = work.join("mirror");
    fs::create_dir(&mirror)?;
    let export = mirror.join("export");
    let invalid = mirror_release::export_with_retained(
        &seed.repo,
        "seed",
        &seed.public,
        &export,
        &[current.trim().to_owned()],
    )
    .unwrap_err();
    assert!(
        invalid
            .to_string()
            .contains("current catalog cannot also be retained")
    );
    assert!(!export.exists());
    mirror_release::export_with_retained(
        &seed.repo,
        "seed",
        &seed.public,
        &export,
        &[previous.trim().to_owned()],
    )?;
    assert_eq!(fs::read_to_string(export.join("retained"))?, previous);
    assert_eq!(fs::read_to_string(export.join("current"))?, current);

    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_export(&export, &mirror, 1, fresh, &anchor, &tuf_key)?;
    let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
    let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
    let seed_cache = work.join("seed-cache");
    let hash = mirror_release::sync_index(
        &seed_cache,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "seed",
        &seed.public,
    )?;
    assert_eq!(hash, current.trim());
    assert_eq!(
        fs::read_to_string(seed_cache.join(".tuf/catalog"))?,
        current
    );
    let key = read_key(&seed.public)?;
    let now = snapshot(&seed_cache, "seed", &key, None)?;
    assert_eq!(now.packages.len(), 1);
    assert_eq!(
        now.packages[0].manifest.id(),
        "cps://seed/library/runtime@2.0.0-0"
    );
    let blob = format!("blobs/{}", digest(b"old"));
    assert!(!seed_cache.join(&blob).exists());

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        "    require \"library/runtime\"\n".to_owned(),
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    fs::write(
        f.repo.join("publish-policy.kdl"),
        format!(
            r#"schema-version 1
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/compositor"
        publisher "seed" catalog={:?} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
            previous.trim(),
            seed.repo,
            seed.public
        ),
    )?;
    withdraw_package(&f.repo, &f.private, "cps://canopa/test/retire@1.0.0-0")?;
    let first = fs::read_to_string(f.repo.join("current"))?;
    let primary_root = work.join("primary-root.json");
    let primary_key = work.join("primary-tuf-key.der");
    sign_root(&primary_root, &primary_key, None)?;
    let primary_mirror = work.join("primary-mirror");
    sign_release(&f, &primary_mirror, 1, fresh, &primary_root, &primary_key)?;
    let primary_cache = work.join("primary-cache");
    let config = work.join("image.kdl");
    let write_config = |primary: &Path, seed_mirror: &Path| -> Result<()> {
        let endpoint = |mirror: &Path, directory| {
            Url::from_directory_path(mirror.join(directory))
                .unwrap()
                .to_string()
        };
        fs::write(
            &config,
            format!(
                r#"schema-version 1
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{
    repository {:?}
    trust-key {:?}
}}
mirror "canopa" root={primary_root:?} metadata={:?} targets={:?}
publisher "seed" {{
    repository {:?}
    trust-key {:?}
}}
mirror "seed" root={anchor:?} metadata={:?} targets={:?}
"#,
                primary_cache,
                f.public,
                endpoint(primary, "metadata"),
                endpoint(primary, "targets"),
                seed_cache,
                seed.public,
                endpoint(seed_mirror, "metadata"),
                endpoint(seed_mirror, "targets"),
            ),
        )?;
        Ok(())
    };
    write_config(&primary_mirror, &mirror)?;
    assert_eq!(mirror_release::sync_profile(&config, true)?, first.trim());
    create_configured_image(&f.root, &config)?;
    let saved = plan(&f.root, Request::InstallProfile)?;
    assert_eq!(saved.catalogs["seed"], previous.trim());
    fetch_plan(&f.root, &saved)?;
    assert_eq!(fs::read(seed_cache.join(&blob))?, b"old");
    let prune_seed = |images: &[PathBuf]| {
        mirror_release::prune_payloads(
            &seed_cache,
            "seed",
            &seed.public,
            mirror_release::CacheRetention {
                catalogs: &[],
                images,
                plans: &[],
            },
            false,
        )
    };
    assert!(prune_seed(&[])?.blobs.is_empty());

    let full = work.join("full-cache");
    mirror_release::sync(
        &full,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "seed",
        &seed.public,
    )?;
    assert_eq!(fs::read_to_string(full.join("current"))?, current);
    assert_eq!(fs::read(full.join(&blob))?, b"old");

    // The mirror changes its retention list without a new current catalog.
    // The empty profiled image still needs the original companion pin.
    let unretained_mirror = work.join("unretained-mirror");
    sign_publisher_release(
        &seed,
        &unretained_mirror,
        2,
        fresh,
        &anchor,
        &tuf_key,
        "seed",
    )?;
    mirror_release::sync_index(
        &seed_cache,
        &anchor,
        Url::from_directory_path(unretained_mirror.join("metadata"))
            .unwrap()
            .as_str(),
        Url::from_directory_path(unretained_mirror.join("targets"))
            .unwrap()
            .as_str(),
        "seed",
        &seed.public,
    )?;
    assert_eq!(prune_seed(&[])?.blobs, [(digest(b"old"), 3)]);
    assert!(prune_seed(std::slice::from_ref(&f.root))?.blobs.is_empty());

    seed.package_as(
        "seed",
        "library/runtime",
        "3.0.0-0",
        seed.file(payload, "latest", "0644", "")?,
    )?;
    let latest = fs::read_to_string(seed.repo.join("current"))?;
    let policy = f.repo.join("publish-policy.kdl");
    fs::write(
        &policy,
        fs::read_to_string(&policy)?.replace(previous.trim(), latest.trim()),
    )?;
    let refresh = work.join("refresh.kdl");
    fs::write(
        &refresh,
        format!(
            "schema-version 1\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
            first.trim()
        ),
    )?;
    let second = publish_release(&f.repo, &f.private, &refresh)?;
    let next_primary_mirror = work.join("next-primary-mirror");
    sign_release(
        &f,
        &next_primary_mirror,
        2,
        fresh,
        &primary_root,
        &primary_key,
    )?;
    write_config(&next_primary_mirror, &unretained_mirror)?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!(
            "mirror for seed does not advertise required catalog {}",
            latest.trim()
        )),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, first);
    assert_eq!(
        fs::read_to_string(primary_cache.join(".tuf/catalog"))?.trim(),
        second
    );
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, current);
    assert_eq!(plan(&f.root, Request::InstallProfile)?, saved);
    let error = mirror_release::sync_index(
        &primary_cache,
        &primary_root,
        Url::from_directory_path(next_primary_mirror.join("metadata"))
            .unwrap()
            .as_str(),
        Url::from_directory_path(next_primary_mirror.join("targets"))
            .unwrap()
            .as_str(),
        "canopa",
        &f.public,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("managed by repo sync-profile"));

    let next_seed_mirror = work.join("next-seed-mirror");
    sign_publisher_release(
        &seed,
        &next_seed_mirror,
        3,
        fresh,
        &anchor,
        &tuf_key,
        "seed",
    )?;
    write_config(&next_primary_mirror, &next_seed_mirror)?;
    let correct_config = fs::read_to_string(&config)?;
    fs::write(
        &config,
        correct_config.replace(
            seed_cache.to_str().unwrap(),
            work.join("different-seed-cache").to_str().unwrap(),
        ),
    )?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("bound to a different release profile or companion cache set")
    );
    let builder = Fixture::new()?;
    let seed_key = format!("    trust-key {:?}", seed.public);
    fs::write(
        &config,
        correct_config.replace(
            &seed_key,
            &format!(
                "{seed_key}\n    build-trust {{ builder-key {:?}; }}",
                builder.public
            ),
        ),
    )?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("no build attestation for cps://seed/library/runtime@3.0.0-0"),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, first);
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, current);
    assert_eq!(plan(&f.root, Request::InstallProfile)?, saved);
    fs::write(&config, correct_config)?;
    assert_eq!(mirror_release::sync_profile(&config, true)?, second);
    assert_eq!(
        fs::read_to_string(primary_cache.join("current"))?.trim(),
        second
    );
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, latest);
    let fresh_root = work.join("fresh-image");
    create_configured_image(&fresh_root, &config)?;
    let new_plan = plan(&fresh_root, Request::InstallProfile)?;
    assert_eq!(new_plan.catalogs["seed"], latest.trim());
    fetch_plan(&fresh_root, &new_plan)?;

    let broken = crate::repo::sign_profile_with_missing_dependency(
        &f.repo,
        "canopa",
        &f.private,
        "desktop",
        "library/runtime",
    )?;
    let broken_mirror = work.join("broken-primary-mirror");
    sign_release(&f, &broken_mirror, 3, fresh, &primary_root, &primary_key)?;
    write_config(&broken_mirror, &next_seed_mirror)?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}").contains("signed profile package library/runtime with manifest"),
        "{error:#}"
    );
    assert_eq!(
        fs::read_to_string(primary_cache.join("current"))?.trim(),
        second
    );
    assert_eq!(
        fs::read_to_string(primary_cache.join(".tuf/catalog"))?.trim(),
        broken
    );

    fs::rename(&mirror, work.join("offline-mirror"))?;
    fs::rename(&unretained_mirror, work.join("offline-unretained-mirror"))?;
    fs::rename(&next_seed_mirror, work.join("offline-next-seed-mirror"))?;
    fs::rename(
        &next_primary_mirror,
        work.join("offline-next-primary-mirror"),
    )?;
    fs::rename(&broken_mirror, work.join("offline-broken-primary-mirror"))?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read(f.root.join(payload))?, b"old");
    apply(&fresh_root, &new_plan)?;
    assert_eq!(fs::read(fresh_root.join(payload))?, b"latest");
    let update = plan(&f.root, Request::Update)?;
    assert_eq!(update.request, Request::UpdateProfile);
    assert_eq!(update.catalogs["seed"], latest.trim());
    let staged = work.join("staged-profile");
    crate::stage_boot_environment(&f.root, &staged, &update)?;
    assert_eq!(fs::read(staged.join(payload))?, b"latest");
    assert_eq!(fs::read(f.root.join(payload))?, b"old");
    Ok(())
}

#[test]
fn tuf_payload_pruning_preserves_saved_plans_and_offline_repairs() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let sync = |version| -> Result<PathBuf> {
        let mirror = work.join(format!("mirror-{version}"));
        sign_release(&f, &mirror, version, fresh, &anchor, &tuf_key)?;
        mirror_release::sync(
            &cache,
            &anchor,
            Url::from_directory_path(mirror.join("metadata"))
                .unwrap()
                .as_str(),
            Url::from_directory_path(mirror.join("targets"))
                .unwrap()
                .as_str(),
            "canopa",
            &f.public,
        )?;
        Ok(mirror)
    };
    let prune = |catalogs: &[String], images: &[PathBuf], plans: &[Plan], apply| {
        mirror_release::prune_payloads(
            &cache,
            "canopa",
            &f.public,
            mirror_release::CacheRetention {
                catalogs,
                images,
                plans,
            },
            apply,
        )
    };
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "previous", "0644", "")?,
    )?;
    let first = sync(1)?;
    let old_catalog = fs::read_to_string(cache.join("current"))?.trim().to_owned();
    let installed = work.join("installed");
    for root in [&installed, &f.root] {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
    }
    let request = Request::Install {
        packages: vec!["desktop/compositor".to_owned()],
    };
    let saved = plan(&f.root, request.clone())?;
    apply(&installed, &plan(&installed, request)?)?;

    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "latest", "0644", "")?,
    )?;
    withdraw_package(
        &f.repo,
        &f.private,
        "cps://canopa/desktop/compositor@1.0.0-0",
    )?;
    let second = sync(2)?;
    let current = fs::read_to_string(cache.join("current"))?;
    let old_blob = digest(b"previous");
    let new_blob = digest(b"latest");
    let orphan = digest(b"orphan");
    let blob_dir = cache.join("blobs");
    fs::write(blob_dir.join(&orphan), b"orphan")?;
    fs::write(blob_dir.join("local-note"), b"leave this alone")?;
    let unrelated = work.join("unrelated");
    fs::write(&unrelated, b"outside the cache")?;
    let symlink = blob_dir.join(digest(b"unexpected symlink"));
    std::os::unix::fs::symlink(&unrelated, &symlink)?;

    let inventory = cache.join(".tuf/retention").join(current.trim());
    let saved_inventory = fs::read(&inventory)?;
    fs::remove_file(&inventory)?;
    let error =
        prune(&[], &[], &[], true).expect_err("prune accepted a cache without an inventory");
    assert!(format!("{error:#}").contains("cache has no retention inventory"));
    assert!(blob_dir.join(&old_blob).exists() && blob_dir.join(&orphan).exists());
    fs::write(inventory, saved_inventory)?;

    let report = prune(&[], &[], &[], false)?;
    assert_eq!(report.bytes, 14);
    assert!(report.blobs.iter().any(|(hash, _)| hash == &old_blob));
    assert!(blob_dir.join(&old_blob).exists() && blob_dir.join(&orphan).exists());
    for report in [
        prune(std::slice::from_ref(&old_catalog), &[], &[], false)?,
        prune(&[], std::slice::from_ref(&installed), &[], false)?,
        prune(&[], &[], std::slice::from_ref(&saved), true)?,
    ] {
        assert_eq!(report.blobs, [(orphan.clone(), 6)]);
    }
    assert!(!blob_dir.join(&orphan).exists());
    assert!(blob_dir.join(&old_blob).exists() && blob_dir.join(&new_blob).exists());
    assert!(symlink.symlink_metadata()?.file_type().is_symlink());
    assert_eq!(fs::read(&unrelated)?, b"outside the cache");
    assert!(blob_dir.join("local-note").exists());

    fs::rename(first, work.join("offline-first"))?;
    fs::rename(second, work.join("offline-second"))?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read(f.root.join(COMPOSITOR))?, b"previous");
    fs::write(installed.join(COMPOSITOR), b"damaged")?;
    let repair = plan(
        &installed,
        Request::Repair {
            packages: Vec::new(),
        },
    )?;
    apply(&installed, &repair)?;
    assert_eq!(fs::read(installed.join(COMPOSITOR))?, b"previous");
    assert_eq!(prune(&[], &[], &[], true)?.blobs, [(old_blob.clone(), 8)]);
    assert!(!blob_dir.join(old_blob).exists());
    assert!(blob_dir.join(new_blob).exists());
    assert_eq!(fs::read_to_string(cache.join("current"))?, current);
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, current);
    assert_eq!(
        snapshot(&cache, "canopa", &read_key(&f.public)?, Some(&old_catalog))?
            .packages
            .len(),
        1
    );
    Ok(())
}

#[test]
fn publisher_rotation_survives_partial_sync_and_preserves_pinned_plans() -> Result<()> {
    let f = Fixture::new()?;
    let second_key = Fixture::new()?;
    let third_key = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_root(&anchor, &tuf_key, None)?;
    let cache = work.join("cache");
    let sign = |version| -> Result<PathBuf> {
        let mirror = work.join(format!("mirror-{version}"));
        sign_release(&f, &mirror, version, fresh, &anchor, &tuf_key)?;
        Ok(mirror)
    };
    let refresh = |cache: &Path, mirror: &Path| {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let create = |root: &Path| -> Result<()> {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
        Ok(())
    };
    let install = |root: &Path| {
        plan(
            root,
            Request::Install {
                packages: vec!["desktop/compositor".to_owned()],
            },
        )
    };

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    let initial = refresh(&cache, &sign(1)?)?;
    create(&f.root)?;
    apply(&f.root, &install(&f.root)?)?;
    let historical = work.join("historical");
    create(&historical)?;
    let saved = install(&historical)?;

    let rotated =
        rotate_publisher_key(&f.repo, "canopa", &initial, &f.private, &second_key.private)?;
    let current = f.repo.join("current");
    assert_eq!(fs::read_to_string(&current)?.trim(), rotated);
    let error = rotate_publisher_key(
        &f.repo,
        "canopa",
        &initial,
        &second_key.private,
        &third_key.private,
    )
    .unwrap_err();
    assert!(error.to_string().contains("rotation base is stale"));
    let error = publish(&f.repo, &f.proto, &f.recipe, &f.private).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("signing key is not the current publisher key")
    );

    let actions = f.file(COMPOSITOR, "new", "0755", "")?;
    fs::write(
        &f.recipe,
        format!(
            "schema-version 1\npackage \"desktop/compositor\" publisher=\"canopa\" version=\"2.0.0-0\" {{\n{actions}}}\n"
        ),
    )?;
    publish(&f.repo, &f.proto, &f.recipe, &second_key.private)?;
    let second = fs::read_to_string(&current)?;
    let mirror = sign(2)?;
    let blob = digest(b"new");
    fs::write(
        mirror.join("targets").join(format!("{blob}.blobs-{blob}")),
        b"bad",
    )?;
    let error = refresh(&cache, &mirror).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"));
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, second);

    // Simulate the retired signer creating a higher-sequence fork and getting
    // it wrapped in fresh TUF metadata. The publisher checkpoint must win.
    fs::write(&current, format!("{initial}\n"))?;
    for version in ["1.0.0-0", "2.0.0-0", "3.0.0-0"] {
        f.package("test/fork", version, String::new())?;
    }
    let fork = sign(3)?;
    fs::write(&current, &second)?;
    let error = refresh(&cache, &fork).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("drops or changes the publisher key history")
    );
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, second);

    let latest = rotate_publisher_key(
        &f.repo,
        "canopa",
        second.trim(),
        &second_key.private,
        &third_key.private,
    )?;
    let error = rotate_publisher_key(&f.repo, "canopa", &latest, &third_key.private, &f.private)
        .unwrap_err();
    assert!(error.to_string().contains("publisher key was already used"));
    assert_eq!(fs::read_to_string(&current)?.trim(), latest);
    let newest = sign(4)?;
    assert_eq!(refresh(&cache, &newest)?, latest);
    assert_eq!(refresh(&work.join("new-cache"), &newest)?, latest);
    let old_id = "cps://canopa/desktop/compositor@1.0.0-0";
    assert!(withdraw_package(&f.repo, &second_key.private, old_id).is_err());
    withdraw_package(&f.repo, &third_key.private, old_id)?;
    refresh(&cache, &sign(5)?)?;

    apply(&historical, &saved)?;
    assert_eq!(fs::read_to_string(historical.join(COMPOSITOR))?, "old");
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "new");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

#[test]
fn tuf_obsoletion_removes_only_when_dependencies_allow_and_preserves_history() -> Result<()> {
    use crate::query::{Scope, list_packages, package_manifest};

    const NAME: &str = "library/legacy";
    const OLD: &str = "cps://canopa/library/legacy@1.0.0-0";
    const CUTOFF: &str = "cps://canopa/library/legacy@2.0.0-0";
    let f = Fixture::new()?;
    let extras = Fixture::new()?;
    let work = f._dir.path();
    let cache = work.join("cache");
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let sync = |version| {
        let mirror = work.join(format!("mirror-{version}"));
        sign_release(&f, &mirror, version, fresh, &anchor, &tuf_key)?;
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let create = |root: &Path| -> Result<()> {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
        crate::add_publisher(root, "extras", &extras.repo, &extras.public)
    };
    let install = |root: &Path, package: &str| {
        plan(
            root,
            Request::Install {
                packages: vec![package.to_owned()],
            },
        )
    };
    let config = f.file(CONFIG, "vendor", "0644", "preserve=#true")?;
    f.package(
        NAME,
        "1.0.0-0",
        f.file(LIBRARY, "legacy", "0644", "")? + &config,
    )?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "original", "0755", "")? + &dependency("require", NAME, "1.0.0-0"),
    )?;
    extras.package_as("extras", NAME, "9.0.0-0", String::new())?;
    let base = sync(1)?;
    create(&f.root)?;
    apply(&f.root, &install(&f.root, "desktop/compositor")?)?;
    fs::write(f.root.join(CONFIG), "local settings")?;
    let historical = work.join("historical");
    create(&historical)?;
    let saved = install(&historical, OLD)?;
    let skipped = work.join("skipped");
    create(&skipped)?;
    apply(&skipped, &install(&skipped, OLD)?)?;

    let release = work.join("retire.kdl");
    let retire = |id: &str| {
        fs::write(
            &release,
            format!(
                "schema-version 1\nrelease \"canopa\" base={base:?} {{\n    obsolete {id:?}\n}}\n"
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)
    };
    assert!(
        retire(OLD)
            .unwrap_err()
            .to_string()
            .contains("must be newer")
    );
    let retired = retire(CUTOFF)?;
    assert_eq!(sync(2)?, retired);
    assert_eq!(list_obsoletions(&cache, "canopa", &f.public)?, [CUTOFF]);
    assert!(list_withdrawals(&cache, "canopa", &f.public)?.contains_key(OLD));
    let error = plan(&f.root, Request::Update).unwrap_err();
    assert!(format!("{error:#}").contains(NAME));
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "legacy");
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "original");

    let fresh_root = work.join("fresh");
    create(&fresh_root)?;
    let error = install(&fresh_root, NAME).unwrap_err();
    assert!(error.to_string().contains("is obsolete through 2.0.0-0"));
    assert!(
        install(&fresh_root, OLD)
            .unwrap_err()
            .to_string()
            .contains("is obsolete")
    );
    assert!(
        package_manifest(&fresh_root, NAME, true)
            .unwrap_err()
            .to_string()
            .contains("is obsolete")
    );
    assert!(
        list_packages(&fresh_root, Scope::Available)?
            .iter()
            .all(|row| row.name != NAME)
    );
    let qualified = "cps://extras/library/legacy@9.0.0-0";
    apply(&fresh_root, &install(&fresh_root, qualified)?)?;
    apply(&fresh_root, &plan(&fresh_root, Request::Update)?)?;
    assert!(
        list_packages(&fresh_root, Scope::Installed)?
            .iter()
            .any(|row| row.id == qualified)
    );

    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "current", "0755", "")?,
    )?;
    sync(3)?;
    let update = plan(&f.root, Request::Update)?;
    assert!(
        update
            .summary()
            .contains("Remove cps://canopa/library/legacy")
    );
    apply(&f.root, &update)?;
    assert!(!f.root.join(LIBRARY).exists());
    assert_eq!(fs::read_to_string(f.root.join(CONFIG))?, "local settings");
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "current");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));

    apply(&historical, &saved)?;
    assert_eq!(fs::read_to_string(historical.join(LIBRARY))?, "legacy");
    apply(&historical, &plan(&historical, Request::Update)?)?;
    assert!(list_packages(&historical, Scope::Installed)?.is_empty());
    assert!(!historical.join(LIBRARY).exists());
    assert!(
        f.package(NAME, "2.0.0-0", String::new())
            .unwrap_err()
            .to_string()
            .contains("obsolete")
    );
    f.package(NAME, "3.0.0-0", f.file(LIBRARY, "revived", "0644", "")?)?;
    sync(4)?;
    apply(&skipped, &plan(&skipped, Request::Update)?)?;
    assert_eq!(fs::read_to_string(skipped.join(LIBRARY))?, "revived");
    assert_eq!(list_obsoletions(&cache, "canopa", &f.public)?, [CUTOFF]);
    Ok(())
}

#[test]
fn tuf_cache_rejects_corruption_replay_and_expiry() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let first = work.join("first");
    let second = work.join("second");
    let expired = work.join("expired");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let refresh = |mirror: &Path| {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    sign_release(&f, &first, 1, fresh, &anchor, &tuf_key)?;
    let initial = refresh(&first)?;
    create_image(
        &f.root,
        &cache,
        "canopa",
        &f.public,
        "x86_64",
        BTreeMap::new(),
    )?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".to_owned()],
        },
    )?;

    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "new", "0755", "")?,
    )?;
    let (id, envelope) = attach_evidence(&f, &initial)?;
    sign_release(&f, &second, 2, fresh, &anchor, &tuf_key)?;
    let blob = digest(b"new");
    let target = second.join("targets").join(format!("{blob}.blobs-{blob}"));
    fs::write(&target, b"bad")?;
    let corrupt = refresh(&second).unwrap_err();
    assert!(
        format!("{corrupt:#}").contains("Hash mismatch"),
        "{corrupt:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);
    assert!(!cache.join("blobs").join(&blob).exists());

    let replay = refresh(&first).unwrap_err();
    assert!(
        matches!(
            replay.downcast_ref::<TufError>(),
            Some(TufError::OlderMetadata {
                role: RoleType::Timestamp,
                current_version: 2,
                new_version: 1,
                ..
            })
        ),
        "{replay:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);

    fs::write(&target, b"new")?;
    let hash = digest(&envelope);
    let evidence = second
        .join("targets")
        .join(format!("{hash}.attestations-{hash}"));
    fs::write(&evidence, vec![b'!'; envelope.len()])?;
    let corrupt = refresh(&second).unwrap_err();
    assert!(
        format!("{corrupt:#}").contains("attestations/"),
        "{corrupt:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);
    fs::write(&evidence, &envelope)?;
    let upgraded = refresh(&second)?;
    assert_ne!(upgraded, initial);
    assert_eq!(refresh(&second)?, upgraded);

    // A failed repair leaves the cache untouched. Correct targets can replace
    // locally corrupted content even when the release pointer stays the same.
    let cached_blob = cache.join("blobs").join(&blob);
    fs::write(&cached_blob, b"bad")?;
    fs::write(&target, b"old")?;
    let corrupt = refresh(&second).unwrap_err();
    assert!(format!("{corrupt:#}").contains("Hash mismatch"));
    assert_eq!(fs::read(&cached_blob)?, b"bad");
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), upgraded);
    fs::write(&target, b"new")?;
    let cached_catalog = cache.join("catalogs").join(&upgraded);
    fs::write(&cached_catalog, b"truncated catalog")?;
    assert_eq!(refresh(&second)?, upgraded);
    assert_eq!(fs::read(&cached_blob)?, b"new");
    assert_eq!(digest(&fs::read(&cached_catalog)?), upgraded);
    assert_eq!(
        read_attestation(&cache, "canopa", &f.public, &id)?,
        envelope
    );

    // The earlier plan still pins a verified catalog after the cache advances.
    apply(&f.root, &install)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "new");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));

    let past = Timestamp::now().checked_sub(jiff::Span::new().hours(24))?;
    sign_release(&f, &expired, 3, past, &anchor, &tuf_key)?;
    let freeze = refresh(&expired).unwrap_err();
    assert!(
        matches!(
            freeze.downcast_ref::<TufError>(),
            Some(TufError::ExpiredMetadata { .. })
        ),
        "{freeze:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), upgraded);
    assert!(publish(&cache, &f.proto, &f.recipe, &f.private).is_err());

    let rotated = work.join("rotated");
    let next_root = work.join("next-root.json");
    let next_key = work.join("next-key.der");
    sign_root(&next_root, &next_key, Some(&tuf_key))?;
    sign_release(&f, &rotated, 3, fresh, &next_root, &next_key)?;
    let timestamp = rotated.join("metadata/timestamp.json");
    let timestamp_bytes = fs::read(&timestamp)?;
    fs::remove_file(&timestamp)?;
    assert!(refresh(&rotated).is_err());
    assert!(refresh(&second).is_err()); // Old timestamp key was revoked.
    fs::remove_file(rotated.join("metadata/2.root.json"))?;
    fs::write(&timestamp, timestamp_bytes)?;
    assert_eq!(refresh(&rotated)?, upgraded); // The client retained the rotated root.

    fs::remove_file(cache.join(".tuf/origin"))?;
    assert!(refresh(&rotated).is_err());
    Ok(())
}

#[test]
fn tuf_withdrawal_keeps_cached_recovery_and_rejects_resurrection() -> Result<()> {
    use crate::query::{Scope, list_packages, package_manifest};

    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let refresh = |cache: &Path, mirror: &Path| {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let create = |root: &Path, cache: &Path| {
        create_image(root, cache, "canopa", &f.public, "x86_64", BTreeMap::new())
    };
    let install = |root: &Path, package: &str| {
        plan(
            root,
            Request::Install {
                packages: vec![package.to_owned()],
            },
        )
    };
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "previous", "0755", "")?,
    )?;
    let previous = fs::read_to_string(f.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "withdrawn", "0755", "")?,
    )?;
    let (id, envelope) = attach_evidence(&f, previous.trim())?;
    let original = work.join("original");
    let resurrected = work.join("resurrected");
    sign_release(&f, &original, 1, fresh, &anchor, &tuf_key)?;
    sign_release(&f, &resurrected, 3, fresh, &anchor, &tuf_key)?;
    let original_hash = refresh(&cache, &original)?;
    let key = read_key(&f.public)?;
    let published = snapshot(&cache, "canopa", &key, Some(&original_hash))?;
    let retired = published
        .packages
        .iter()
        .find(|pkg| pkg.manifest.id() == id)
        .unwrap();
    create(&f.root, &cache)?;
    let saved = install(&f.root, &id)?;

    for invalid in [
        "desktop/compositor@2.0.0-0",
        "cps://extras/desktop/compositor@2.0.0-0",
        "cps://canopa/desktop/compositor@3.0.0-0",
    ] {
        assert!(withdraw_package(&f.repo, &f.private, invalid).is_err());
    }
    assert!(withdraw_package(&f.repo, &Fixture::new()?.private, &id).is_err());
    assert!(withdraw_package(&cache, &f.private, &id).is_err());
    assert_eq!(
        fs::read_to_string(f.repo.join("current"))?.trim(),
        original_hash
    );

    withdraw_package(&f.repo, &f.private, &id)?;
    let withdrawn_pointer = fs::read_to_string(f.repo.join("current"))?;
    withdraw_package(&f.repo, &f.private, &id)?;
    let error = publish(&f.repo, &f.proto, &f.recipe, &f.private).unwrap_err();
    assert!(error.to_string().contains("was withdrawn"));
    assert_eq!(
        fs::read_to_string(f.repo.join("current"))?,
        withdrawn_pointer
    );
    assert_eq!(
        read_attestation(&f.repo, "canopa", &f.public, &id)?,
        envelope
    );

    let withdrawn = work.join("withdrawn");
    sign_release(&f, &withdrawn, 2, fresh, &anchor, &tuf_key)?;
    let withdrawn_hash = refresh(&cache, &withdrawn)?;
    assert_eq!(withdrawn_hash, withdrawn_pointer.trim());
    assert_eq!(refresh(&cache, &withdrawn)?, withdrawn_hash);
    assert_eq!(
        list_withdrawals(&cache, "canopa", &f.public)?,
        BTreeMap::from([(id.clone(), retired.sha256.clone())])
    );
    assert_eq!(
        read_attestation(&cache, "canopa", &f.public, &id)?,
        envelope
    );

    let new_cache = work.join("new-cache");
    assert_eq!(refresh(&new_cache, &withdrawn)?, withdrawn_hash);
    assert!(!new_cache.join("manifests").join(&retired.sha256).exists());
    assert!(!new_cache.join("blobs").join(digest(b"withdrawn")).exists());
    assert!(
        !new_cache
            .join("attestations")
            .join(digest(&envelope))
            .exists()
    );
    let new_root = work.join("new-root");
    create(&new_root, &new_cache)?;
    assert_eq!(
        list_packages(&new_root, Scope::Available)?[0].id,
        "cps://canopa/desktop/compositor@1.0.0-0"
    );
    assert!(package_manifest(&new_root, &id, true).is_err());
    let error = install(&new_root, &id).unwrap_err();
    assert!(error.to_string().contains("was withdrawn"));
    apply(&new_root, &install(&new_root, "desktop/compositor")?)?;
    assert_eq!(fs::read_to_string(new_root.join(COMPOSITOR))?, "previous");

    apply(&f.root, &saved)?;
    let error = install(&f.root, "desktop/compositor@2.0.0-0").unwrap_err();
    assert!(error.to_string().contains("was withdrawn"));
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "withdrawn");
    fs::write(f.root.join(COMPOSITOR), "damaged")?;
    let repair = plan(&f.root, Request::Repair { packages: vec![] })?;
    apply(&f.root, &repair)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "withdrawn");

    let error = refresh(&cache, &resurrected).unwrap_err();
    assert!(error.to_string().contains("catalog rollback or fork"));
    assert_eq!(
        fs::read_to_string(cache.join("current"))?.trim(),
        withdrawn_hash
    );

    f.package(
        "desktop/compositor",
        "3.0.0-0",
        f.file(COMPOSITOR, "replacement", "0755", "")?,
    )?;
    let replacement = work.join("replacement");
    sign_release(&f, &replacement, 4, fresh, &anchor, &tuf_key)?;
    refresh(&cache, &replacement)?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "replacement");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

#[test]
fn tuf_cache_remembers_a_newer_publisher_catalog_after_a_failed_download() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let refresh = |name: &str| {
        let mirror = work.join(name);
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let sign =
        |name: &str, version| sign_release(&f, &work.join(name), version, fresh, &anchor, &tuf_key);

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    let current = f.repo.join("current");
    let first = fs::read_to_string(&current)?;
    sign("first", 1)?;
    sign("replay", 3)?; // Fresh TUF metadata, original CPS publisher catalog.

    f.package("test/fork", "1.0.0-0", String::new())?;
    sign("fork", 4)?; // A different publisher-signed catalog at sequence 2.
    fs::write(&current, &first)?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "new", "0755", "")?,
    )?;
    let second = fs::read_to_string(&current)?;
    publish(&f.repo, &f.proto, &f.recipe, &f.private)?;
    assert_eq!(fs::read_to_string(&current)?, second);
    sign("second", 2)?;

    assert_eq!(refresh("first")?, first.trim());
    let checkpoint = cache.join(".tuf/catalog");
    fs::remove_file(&checkpoint)?; // Upgrade an existing cache without a publisher checkpoint.
    let blob = digest(b"new");
    fs::write(
        work.join("second/targets")
            .join(format!("{blob}.blobs-{blob}")),
        b"bad",
    )?;
    let corrupt = refresh("second").unwrap_err();
    assert!(format!("{corrupt:#}").contains("Hash mismatch"));
    assert_eq!(fs::read_to_string(&checkpoint)?, second);
    assert_eq!(fs::read_to_string(cache.join("current"))?, first);

    for rejected in ["replay", "fork"] {
        let error = refresh(rejected).unwrap_err();
        assert!(error.to_string().contains("catalog rollback or fork"));
        assert_eq!(fs::read_to_string(&checkpoint)?, second);
        assert_eq!(fs::read_to_string(cache.join("current"))?, first);
    }

    sign("restored", 5)?;
    assert_eq!(refresh("restored")?, second.trim());
    assert_eq!(refresh("restored")?, second.trim());
    assert_eq!(fs::read_to_string(cache.join("current"))?, second);
    Ok(())
}

#[test]
fn tuf_index_and_plan_fetch_support_offline_apply_and_repair() -> Result<()> {
    let f = Fixture::new()?;
    let extras = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let mirror = work.join("mirror");
    let cache = work.join("cache");
    let extra_anchor = work.join("extras-root.json");
    let extra_tuf_key = work.join("extras-tuf-key.der");
    sign_root(&extra_anchor, &extra_tuf_key, None)?;
    let extra_mirror = work.join("extras-mirror");
    let extra_cache = work.join("extras-cache");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;

    extras.package_as(
        "extras",
        "library/libdisplay",
        "1.0.0-0",
        extras.file(LIBRARY, "library", "0644", "")?,
    )?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "program", "0755", "")?
            + &dependency("require", "library/libdisplay", "1.0.0-0"),
    )?;
    f.package(
        "desktop/unused",
        "1.0.0-0",
        f.file(PLUGIN, "unused", "0644", "")?,
    )?;
    sign_release(&f, &mirror, 1, fresh, &anchor, &tuf_key)?;
    sign_publisher_release(
        &extras,
        &extra_mirror,
        1,
        fresh,
        &extra_anchor,
        &extra_tuf_key,
        "extras",
    )?;
    let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
    let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
    let blob = |content: &[u8]| cache.join("blobs").join(digest(content));
    let target = |content: &[u8]| {
        let hash = digest(content);
        mirror.join("targets").join(format!("{hash}.blobs-{hash}"))
    };
    fs::remove_file(target(b"unused"))?;
    let hash = mirror_release::sync_index(
        &cache,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "canopa",
        &f.public,
    )?;
    let extra_metadata = Url::from_directory_path(extra_mirror.join("metadata")).unwrap();
    let extra_targets = Url::from_directory_path(extra_mirror.join("targets")).unwrap();
    mirror_release::sync_index(
        &extra_cache,
        &extra_anchor,
        extra_metadata.as_str(),
        extra_targets.as_str(),
        "extras",
        &extras.public,
    )?;
    assert_eq!(fs::read_dir(cache.join("blobs"))?.count(), 0);
    assert_eq!(fs::read_dir(extra_cache.join("blobs"))?.count(), 0);
    create_image(
        &f.root,
        &cache,
        "canopa",
        &f.public,
        "x86_64",
        BTreeMap::new(),
    )?;
    crate::add_publisher(&f.root, "extras", &extra_cache, &extras.public)?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".to_owned()],
        },
    )?;
    assert!(apply(&f.root, &install).is_err());
    assert!(!f.root.join(COMPOSITOR).exists());

    let mut tampered = install.clone();
    tampered.writes.first_mut().unwrap().path = "usr/bin/other".to_owned();
    let error = fetch_plan(&f.root, &tampered).unwrap_err();
    assert!(error.to_string().contains("differs from the verified plan"));
    assert_eq!(fs::read_dir(cache.join("blobs"))?.count(), 0);

    fs::write(target(b"program"), b"corrupt")?;
    let error = fetch_plan(&f.root, &install).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"));
    assert!(!blob(b"program").exists());
    assert!(!f.root.join(COMPOSITOR).exists());
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), hash);
    fs::write(target(b"program"), b"program")?;
    assert_eq!(fetch_plan(&f.root, &install)?, 2);
    assert_eq!(fs::read_dir(cache.join("blobs"))?.count(), 1);
    assert_eq!(fs::read_dir(extra_cache.join("blobs"))?.count(), 1);
    assert!(!blob(b"unused").exists());

    let offline = work.join("offline");
    let extra_offline = work.join("extras-offline");
    fs::rename(&mirror, &offline)?;
    fs::rename(&extra_mirror, &extra_offline)?;
    assert_eq!(fetch_plan(&f.root, &install)?, 2);
    apply(&f.root, &install)?;
    assert_eq!(fs::read(f.root.join(COMPOSITOR))?, b"program");
    assert_eq!(fs::read(f.root.join(LIBRARY))?, b"library");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));

    fs::rename(&extra_offline, &extra_mirror)?;
    fs::remove_file(extra_cache.join("blobs").join(digest(b"library")))?;
    fs::write(f.root.join(LIBRARY), b"damaged")?;
    let repair = plan(&f.root, Request::Repair { packages: vec![] })?;
    assert_eq!(fetch_plan(&f.root, &repair)?, 1);
    fs::rename(&extra_mirror, &extra_offline)?;
    apply(&f.root, &repair)?;
    assert_eq!(fs::read(f.root.join(LIBRARY))?, b"library");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}
