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
    attestation::{self, Toolchain},
    create_configured_image, fetch_plan,
    kdl_format::{build, parse_manifest},
    list_obsoletions, list_withdrawals,
    model::FileDigest,
    publish_bundle, publish_release,
    query::{Scope, list_packages, package_manifest},
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
        Toolchain {
            publisher: "canopa".to_owned(),
            key_sha256: digest(&key),
            catalog_sha256: release.sha256,
            package_manifests: BTreeMap::from([(
                "desktop/compositor".to_owned(),
                release.packages[0].sha256.clone(),
            )]),
        },
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
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        "    require \"library/runtime\"\n".to_owned(),
    )?;
    f.package("test/first", "1.0.0-0", String::new())?;
    f.package("test/second", "1.0.0-0", String::new())?;
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
    let release = |pin: &str, retired: &str| -> Result<()> {
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
        )?;
        withdraw_package(&f.repo, &f.private, retired)
    };
    release(&old_pin, "cps://canopa/test/first@1.0.0-0")?;
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
    let saved = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".to_owned()],
        },
    )?;
    assert_eq!(saved.catalogs["seed"], old_pin.trim());
    let mut altered = saved.clone();
    altered
        .catalogs
        .insert("seed".to_owned(), new_pin.trim().to_owned());
    let error = apply(&f.root, &altered).unwrap_err();
    assert!(format!("{error:#}").contains("differs from the signed release profile"));
    assert!(!f.root.join(payload).exists());

    release(&new_pin, "cps://canopa/test/second@1.0.0-0")?;
    fs::remove_file(seed.repo.join("current"))?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read_to_string(f.root.join(payload))?, "old");
    let upgrade = plan(&f.root, Request::Update)?;
    assert_eq!(upgrade.catalogs["seed"], new_pin.trim());
    apply(&f.root, &upgrade)?;
    assert_eq!(fs::read_to_string(f.root.join(payload))?, "new");
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
        dependency("require", "library/runtime", "1.0.0-0"),
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
    let config = work.join("image.kdl");
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
            f.repo, f.public, seed_cache, seed.public
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    let saved = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".to_owned()],
        },
    )?;
    assert_eq!(saved.catalogs["seed"], previous.trim());
    fetch_plan(&f.root, &saved)?;
    assert_eq!(fs::read(seed_cache.join(&blob))?, b"old");

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
    fs::rename(&mirror, work.join("offline-mirror"))?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read(f.root.join(payload))?, b"old");
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, current);
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
