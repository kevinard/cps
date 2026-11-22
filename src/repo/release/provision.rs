use std::{
    fs::{self, OpenOptions},
    io::Write,
    num::NonZeroU64,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use anyhow::{Context, Result};
use aws_lc_rs::signature::Ed25519KeyPair;
use tough::{
    schema::{Role, RoleKeys, RoleType, Root, Signature, Signed},
    sign::{Sign, parse_keypair},
};

use crate::{
    model::validate_publisher,
    repo::{
        keys::{Signers, trust},
        lock_repo, sync_objects,
    },
};

pub(crate) fn init_publisher(output: &Path, publisher: &str) -> Result<()> {
    validate_publisher(publisher)?;
    super::sign::publish_stage(output, |stage| {
        fs::set_permissions(stage, fs::Permissions::from_mode(0o700))?;
        let repository = stage.join("repo");
        let _lock = lock_repo(&repository)?;
        let offline = stage.join("offline");
        let online = stage.join("keys");
        let tuf = online.join("tuf");
        fs::create_dir(&offline)?;
        fs::create_dir_all(&tuf)?;

        let root_key = offline.join("publisher-root.der");
        let catalog_key = online.join("catalog.der");
        generate(&root_key)?;
        generate(&catalog_key)?;
        let roots = Signers::open(&root_key)?;
        let catalogs = Signers::open(&catalog_key)?;
        let draft = trust::draft_with(
            None,
            Some(publisher),
            Some(roots.public_keys().into_iter().map(trust::Key).collect()),
            None,
            Some(catalogs.public_keys().into_iter().map(trust::Key).collect()),
            None,
            None,
        )?;
        trust::store(
            &repository,
            &trust::combine(None, &draft.encode()?, Vec::new(), Some(&roots))?,
        )?;

        let mut keys = std::collections::HashMap::new();
        let mut roles = std::collections::HashMap::new();
        let tuf_root_key = offline.join("tuf-root.der");
        let root_document = generate(&tuf_root_key)?;
        for (role, document) in [
            (RoleType::Root, root_document.clone()),
            (RoleType::Targets, generate(&tuf.join("targets.der"))?),
            (RoleType::Snapshot, generate(&tuf.join("snapshot.der"))?),
            (RoleType::Timestamp, generate(&tuf.join("timestamp.der"))?),
        ] {
            let key = parse_keypair(&document)?.tuf_key();
            let id = key.key_id()?;
            roles.insert(
                role,
                RoleKeys {
                    keyids: vec![id.clone()],
                    threshold: NonZeroU64::MIN,
                    _extra: Default::default(),
                },
            );
            keys.insert(id, key);
        }
        generate(&tuf.join("objects.der"))?;
        let root = Root {
            spec_version: "1.0.0".to_owned(),
            consistent_snapshot: true,
            version: NonZeroU64::MIN,
            expires: draft.expires(),
            keys,
            roles,
            _extra: Default::default(),
        };
        let pair = Ed25519KeyPair::from_pkcs8(&root_document)
            .map_err(|error| anyhow::anyhow!("reading generated TUF root key: {error}"))?;
        let signature = Signature {
            keyid: root.roles[&RoleType::Root].keyids[0].clone(),
            sig: pair.sign(&root.canonical_form()?).as_ref().to_vec().into(),
        };
        fs::write(
            stage.join("tuf-root.json"),
            serde_json::to_vec(&Signed {
                signed: root,
                signatures: vec![signature],
            })?,
        )?;
        fs::write(stage.join("publisher"), format!("{publisher}\n"))?;
        sync_objects(stage)
    })
}

fn generate(path: &Path) -> Result<Vec<u8>> {
    let document = crate::repo::generate_signing_key()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&document)?;
    file.sync_all()?;
    Ok(document)
}
