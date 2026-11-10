use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use tough::{Repository, RepositoryLoader};
use url::Url;

use super::{
    ENDPOINTS_LIMIT, Mirror, MirrorTransport, PUBLISHER_NAME_TARGET, TRUST_ROOT_TARGET,
    checked_target, download_target, mirror_base, read_tuf_root,
};
use crate::{
    model::{digest, validate_publisher},
    repo::{
        keys::trust::{self, Pinned, TRUST_ROOT_LIMIT},
        read_limited,
    },
    transport::https_client,
};

const PUBLISHER_NAME_LIMIT: u64 = 2 * 1024 * 1024;

pub struct PublisherBootstrap {
    pub name: String,
    pub url: Url,
    pub tuf_root: Vec<u8>,
    /// The publisher's newest trust root, as served by the mirror.
    pub trust_root: Vec<u8>,
}

impl PublisherBootstrap {
    /// Discover the publisher and its trust root using a locally trusted TUF root.
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
        let (name, trust_root) = runtime.block_on(async {
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
            trust_root,
        })
    }

    /// The SHA-256 of the trusted TUF root document.
    pub fn tuf_root_fingerprint(&self) -> String {
        digest(&self.tuf_root)
    }

    /// The publisher, version, expiry, and key fingerprints of the trust root.
    pub fn describe(&self) -> Result<String> {
        Ok(Pinned::parse(&self.trust_root)?.root().to_string())
    }

    /// Require the trust root to name exactly these root key fingerprints, so
    /// provisioning does not depend on trusting the first mirror response.
    pub fn check_root_keys(&self, expected: &[String]) -> Result<()> {
        trust::check_expected_root_keys(Pinned::parse(&self.trust_root)?.root(), expected)
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
    let trust_root =
        read_target(verified, scratch, TRUST_ROOT_TARGET, TRUST_ROOT_LIMIT, None).await?;
    let pinned = Pinned::parse(&trust_root).context("checking the TUF publisher trust root")?;
    ensure!(
        pinned.root().publisher == name,
        "TUF publisher trust root belongs to {}, not {name}",
        pinned.root().publisher
    );
    Ok((name, trust_root))
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
        let publisher = work.path().join("publisher");
        fs::create_dir(&publisher)?;
        let key = publisher.join("key.der");
        fs::write(
            &key,
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .unwrap()
                .as_ref(),
        )?;
        let trust_path = crate::repo::init_trust(&publisher, "extras", &key, None)?;
        let key = fs::read(trust_path)?;
        fs::write(export.join(PUBLISHER_NAME_TARGET), "extras")?;
        fs::write(export.join(TRUST_ROOT_TARGET), &key)?;
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

        let bootstrap = PublisherBootstrap {
            name: identity.0.clone(),
            url: url.clone(),
            tuf_root: anchor.clone(),
            trust_root: identity.1.clone(),
        };
        let root_key = Pinned::parse(&key)?
            .root()
            .root_keys
            .iter()
            .map(|key| trust::fingerprint(&key.0))
            .collect::<Vec<_>>();
        bootstrap.check_root_keys(&root_key)?;
        assert!(bootstrap.check_root_keys(&[digest(b"other key")]).is_err());
        assert!(bootstrap.describe()?.contains("Publisher: extras"));

        let key_target =
            mirror
                .join("targets")
                .join(format!("{}.{}", digest(&key), TRUST_ROOT_TARGET));
        let mut tampered = key;
        let last = tampered.len() - 3;
        tampered[last] ^= 1;
        fs::write(&key_target, tampered)?;
        assert!(
            runtime
                .block_on(verified_identity(&verified, scratch.path()))
                .is_err()
        );

        fs::remove_file(export.join(TRUST_ROOT_TARGET))?;
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
