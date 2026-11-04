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
    kdl_format::{build, parse_manifest},
    model::FileDigest,
    publish_bundle, read_attestation, release as mirror_release,
    repo::{read_key, read_signing_key, snapshot},
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

fn sign_release(
    fixture: &Fixture,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
) -> Result<()> {
    fs::create_dir(mirror)?;
    let export = mirror.join("export");
    mirror_release::export(&fixture.repo, "canopa", &fixture.public, &export)?;
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
