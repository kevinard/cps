use std::{fs, num::NonZeroU64, path::Path};

use anyhow::{Context, Result};
use jiff::Timestamp;
use ring::{rand::SystemRandom, signature::Ed25519KeyPair};
use tough::{
    editor::{RepositoryEditor, signed::PathExists},
    key_source::{KeySource, LocalKeySource},
    schema::{Role, RoleType, Root, Signature, Signed, Target},
    sign::{Sign, parse_keypair},
};

pub fn sign_root(path: &Path, private: &Path, predecessor: Option<&Path>) -> Result<()> {
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

pub fn sign_export(
    export: &Path,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
    existing: PathExists,
) -> Result<()> {
    let version = NonZeroU64::new(version).context("release version must be nonzero")?;
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
            signed
                .copy_targets(export, mirror.join("targets"), existing)
                .await?;
            signed.write(mirror.join("metadata")).await?;
            Ok(())
        })
}
