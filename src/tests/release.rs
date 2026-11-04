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
    fetch_plan,
    kdl_format::{build, parse_manifest},
    list_withdrawals,
    model::FileDigest,
    publish_bundle, publish_release, read_attestation, release as mirror_release,
    repo::{read_key, read_signing_key, snapshot},
    withdraw_package,
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
            for entry in fs::read_dir(&export)? {
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
                .copy_targets(&export, mirror.join("targets"), PathExists::Fail)
                .await?;
            Ok(())
        })
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
