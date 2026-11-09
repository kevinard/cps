use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use tough::{Repository, RepositoryLoader};
use url::Url;

use super::{
    ENDPOINTS_LIMIT, Mirror, MirrorTransport, PUBLISHER_KEY_TARGET, PUBLISHER_NAME_TARGET,
    checked_target, download_target, mirror_base, read_tuf_root,
};
use crate::{
    model::{digest, validate_publisher},
    repo::{PUBLIC_KEY_DER_LEN, parse_public_key, read_limited},
    transport::https_client,
};

const PUBLISHER_NAME_LIMIT: u64 = 2 * 1024 * 1024;

pub struct PublisherBootstrap {
    pub name: String,
    pub url: Url,
    pub tuf_root: Vec<u8>,
    pub trust_key: Vec<u8>,
}

impl PublisherBootstrap {
    /// Discover the publisher and public key using a locally trusted TUF root.
    pub fn fetch(base: &str, trusted_root: &Path) -> Result<Self> {
        let url = mirror_base(base)?;
        ensure!(
            url.scheme() == "https",
            "publisher discovery requires HTTPS"
        );
        let mirror = Mirror::from_base_url(&url)?;
        ensure!(
            mirror.document().len() as u64 <= ENDPOINTS_LIMIT,
            "mirror endpoints exceed the size limit"
        );
        let tuf_root = read_tuf_root(trusted_root)?;
        let scratch = tempfile::tempdir()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (name, trust_key) = runtime.block_on(async {
            let timeout = Duration::from_secs(30);
            let client = https_client().timeout(timeout).build()?;
            let verified = RepositoryLoader::new(&tuf_root, mirror.metadata, mirror.targets)
                .transport(MirrorTransport::with_deadline(Some(client), timeout)?)
                .datastore(scratch.path())
                .load()
                .await
                .context("verifying TUF metadata for publisher discovery")?;
            verified_identity(&verified, scratch.path()).await
        })?;
        Ok(Self {
            name,
            url,
            tuf_root,
            trust_key,
        })
    }

    /// SHA-256 fingerprints of the trusted root document and verified DER key, in that order.
    pub fn fingerprints(&self) -> (String, String) {
        (digest(&self.tuf_root), digest(&self.trust_key))
    }
}

async fn verified_identity(verified: &Repository, scratch: &Path) -> Result<(String, Vec<u8>)> {
    checked_target(verified, "current", 65, None, None)
        .context("publisher discovery requires a live mirror")?;
    let name = read_target(
        verified,
        scratch,
        PUBLISHER_NAME_TARGET,
        PUBLISHER_NAME_LIMIT,
        None,
    )
    .await?;
    let name = String::from_utf8(name).context("reading TUF publisher name")?;
    validate_publisher(&name)?;
    let key = read_target(
        verified,
        scratch,
        PUBLISHER_KEY_TARGET,
        PUBLIC_KEY_DER_LEN,
        Some(PUBLIC_KEY_DER_LEN),
    )
    .await?;
    parse_public_key(&key).context("checking TUF publisher key")?;
    Ok((name, key))
}

async fn read_target(
    verified: &Repository,
    scratch: &Path,
    name: &str,
    limit: u64,
    length: Option<u64>,
) -> Result<Vec<u8>> {
    let (target_name, target) = checked_target(verified, name, limit, None, length)
        .context("checking TUF publisher enrollment targets")?;
    let file = download_target(verified, name, &target_name, target, scratch).await?;
    read_limited(file.reopen()?, limit)
}

#[cfg(test)]
#[path = "../../../tests/support/tuf.rs"]
mod tuf;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tough::editor::signed::PathExists;

    #[test]
    fn discovery_verifies_the_signed_publisher_identity() -> Result<()> {
        let work = tempfile::tempdir()?;
        let root = work.path().join("root.json");
        let signing_key = work.path().join("key.der");
        tuf::sign_root(&root, &signing_key, None)?;
        let export = work.path().join("export");
        let mirror = work.path().join("mirror");
        fs::create_dir(&export)?;
        let mut key = hex::decode("302a300506032b6570032100")?;
        key.extend([42; 32]);
        fs::write(export.join(PUBLISHER_NAME_TARGET), "extras")?;
        fs::write(export.join(PUBLISHER_KEY_TARGET), &key)?;
        fs::write(export.join("current"), format!("{}\n", digest(b"catalog")))?;
        let expires = jiff::Timestamp::now().checked_add(jiff::Span::new().hours(1))?;
        tuf::sign_export(
            &export,
            &mirror,
            1,
            expires,
            &root,
            &signing_key,
            PathExists::Skip,
        )?;

        let anchor = read_tuf_root(&root)?;
        let url = Url::from_directory_path(&mirror).expect("local mirror URL");
        let endpoints = Mirror::from_base_url(&url)?;
        let scratch = tempfile::tempdir()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let load = || {
            runtime.block_on(
                RepositoryLoader::new(
                    &anchor,
                    endpoints.metadata.clone(),
                    endpoints.targets.clone(),
                )
                .transport(MirrorTransport::new(None).expect("default transfer limits"))
                .datastore(scratch.path())
                .load(),
            )
        };
        let verified = load()?;
        let identity = runtime.block_on(verified_identity(&verified, scratch.path()))?;
        assert_eq!(identity, ("extras".to_owned(), key.clone()));

        let key_target =
            mirror
                .join("targets")
                .join(format!("{}.{}", digest(&key), PUBLISHER_KEY_TARGET));
        let mut tampered = key;
        tampered[43] ^= 1;
        fs::write(&key_target, tampered)?;
        assert!(
            runtime
                .block_on(verified_identity(&verified, scratch.path()))
                .is_err()
        );

        fs::remove_file(export.join(PUBLISHER_KEY_TARGET))?;
        tuf::sign_export(
            &export,
            &mirror,
            2,
            expires,
            &root,
            &signing_key,
            PathExists::Skip,
        )?;
        let legacy = load()?;
        assert!(
            runtime
                .block_on(verified_identity(&legacy, scratch.path()))
                .is_err()
        );
        Ok(())
    }
}
