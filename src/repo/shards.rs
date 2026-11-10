//! Signed catalog roots, content-addressed catalog shards, and catalog archives.
//!
//! A catalog is a small signed root that commits by SHA-256 to up to 256
//! shards. Each shard holds the package records whose name hashes share a
//! one-byte prefix. A publication rewrites the shards it changes and the root.

use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::{
    CatalogEntry, ReleaseProfile, Retired, Withdrawal, exact_publisher, keys, read_object, rebuild,
};
use crate::model::{PackageSpec, Version, digest, validate_digest, validate_name};

pub(crate) const SHARD_OBJECTS: &str = "catalog-shards";
pub(crate) const SHARD_LIMIT: u64 = 16 * 1024 * 1024;
pub(crate) const CATALOG_ROOT_LIMIT: u64 = 16 * 1024 * 1024;
pub(super) const ROOT_FORMAT: u32 = 1;
const SHARD_FORMAT: u32 = 1;
const ARCHIVE_MAGIC: &[u8] = b"cps catalog archive v1\n";

/// The shard holding records for a package name.
pub(crate) fn shard_of(name: &str) -> String {
    digest(name.as_bytes())[..2].to_owned()
}

/// The shard holding records for an exact package ID.
pub(super) fn shard_of_id(id: &str) -> Result<String> {
    Ok(shard_of(PackageSpec::parse(id)?.name))
}

fn valid_prefix(prefix: &str) -> bool {
    prefix.len() == 2
        && prefix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Sign a root payload. The signatures cover exactly these payload bytes.
pub(super) fn seal(payload: &[u8], signers: &keys::Signers) -> Result<Vec<u8>> {
    keys::seal(&keys::CATALOG, payload, signers)?.encode()
}

/// A decoded root envelope whose signatures are yet to be checked.
pub(super) struct Opened {
    pub(super) payload: Vec<u8>,
    envelope: keys::Envelope,
}

/// Decode a root envelope in its only accepted byte form.
pub(super) fn open(bytes: &[u8]) -> Result<Opened> {
    let envelope = keys::Envelope::decode(&keys::CATALOG, bytes)?;
    Ok(Opened {
        payload: envelope.validate(&keys::CATALOG)?,
        envelope,
    })
}

impl Opened {
    /// Require exactly the catalog threshold of distinct signatures by the
    /// catalog keys of the trust root version the catalog is signed under.
    pub(super) fn verify(&self, trust: &keys::trust::VerifiedChain, hash: &str) -> Result<()> {
        trust.verify_catalog(&self.envelope, hash)
    }
}

/// The signed catalog root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Root {
    pub(super) format: u32,
    pub(super) publisher: String,
    pub(super) sequence: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) shards: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) file_index: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) attestations_required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) toolchains_verified: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) source_archives_required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) profiles_required: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(super) pruned_through: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) renames: BTreeMap<String, String>,
    /// Every trust root version through the one this catalog is signed under.
    pub(super) trust: keys::trust::TrustChain,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) release_profiles: BTreeMap<String, ReleaseProfile>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Root {
    pub(super) fn decode(payload: &[u8]) -> Result<Self> {
        let root: Self = serde_json::from_slice(payload).context("invalid catalog root")?;
        ensure!(root.format == ROOT_FORMAT, "unsupported catalog format");
        ensure!(root.sequence > 0, "invalid publisher catalog sequence");
        ensure!(
            root.pruned_through < root.sequence,
            "catalog prunes history beyond its own sequence"
        );
        for (prefix, hash) in &root.shards {
            ensure!(
                valid_prefix(prefix),
                "invalid catalog shard prefix {prefix}"
            );
            validate_digest(hash).context("invalid catalog shard digest")?;
        }
        Ok(root)
    }
}

/// The package records of one name-hash prefix.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Shard {
    pub(super) format: u32,
    pub(super) publisher: String,
    pub(super) prefix: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) entries: BTreeMap<String, CatalogEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) attestations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) build_inputs: BTreeMap<String, rebuild::Digests>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) withdrawals: BTreeMap<String, Withdrawal>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) retired: BTreeMap<String, Retired>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) obsoletions: BTreeMap<String, Version>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) file_index: Option<String>,
}

impl Shard {
    pub(super) fn new(publisher: &str, prefix: &str) -> Self {
        Self {
            format: SHARD_FORMAT,
            publisher: publisher.to_owned(),
            prefix: prefix.to_owned(),
            ..Self::default()
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.attestations.is_empty()
            && self.build_inputs.is_empty()
            && self.withdrawals.is_empty()
            && self.retired.is_empty()
            && self.obsoletions.is_empty()
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() as u64 <= SHARD_LIMIT,
            "catalog shard {} exceeds {SHARD_LIMIT} bytes",
            self.prefix
        );
        Ok(bytes)
    }

    /// Check what a shard can state about itself, independent of its root.
    fn validate(&self) -> Result<()> {
        ensure!(
            self.format == SHARD_FORMAT,
            "unsupported catalog shard format"
        );
        ensure!(
            valid_prefix(&self.prefix),
            "invalid catalog shard prefix {}",
            self.prefix
        );
        let ids = self
            .entries
            .keys()
            .chain(self.attestations.keys())
            .chain(self.build_inputs.keys())
            .chain(self.withdrawals.keys())
            .chain(self.retired.keys());
        for id in ids {
            ensure!(
                exact_publisher(id)? == self.publisher && shard_of_id(id)? == self.prefix,
                "catalog shard {} contains foreign record {id}",
                self.prefix
            );
        }
        for name in self.obsoletions.keys() {
            validate_name(name)?;
            ensure!(
                shard_of(name) == self.prefix,
                "catalog shard {} contains foreign obsoletion {name}",
                self.prefix
            );
        }
        if let Some(hash) = &self.file_index {
            validate_digest(hash).context("invalid file index digest")?;
        }
        for (id, retired) in &self.retired {
            validate_digest(&retired.manifest)?;
            ensure!(
                !self.entries.contains_key(id) && !self.withdrawals.contains_key(id),
                "retired package {id} is still published"
            );
        }
        Ok(())
    }
}

type ShardCache = Mutex<HashMap<String, Arc<Shard>>>;

/// Decoded shards are immutable by digest, so one process shares them.
static SHARDS: LazyLock<ShardCache> = LazyLock::new(Default::default);

fn cached(hash: &str) -> Option<Arc<Shard>> {
    SHARDS.lock().ok()?.get(hash).cloned()
}

/// Decode shard bytes known to have this digest.
pub(super) fn decode_shard(bytes: &[u8], hash: &str) -> Result<Arc<Shard>> {
    if let Some(shard) = cached(hash) {
        return Ok(shard);
    }
    let shard: Shard =
        serde_json::from_slice(bytes).with_context(|| format!("invalid catalog shard {hash}"))?;
    shard
        .validate()
        .with_context(|| format!("checking catalog shard {hash}"))?;
    let shard = Arc::new(shard);
    if let Ok(mut cache) = SHARDS.lock() {
        cache.insert(hash.to_owned(), shard.clone());
    }
    Ok(shard)
}

pub(super) fn read_shard(repo: &std::path::Path, hash: &str) -> Result<Arc<Shard>> {
    if let Some(shard) = cached(hash) {
        return Ok(shard);
    }
    let bytes = read_object(repo, SHARD_OBJECTS, hash, SHARD_LIMIT)?;
    decode_shard(&bytes, hash)
}

/// Record a shard this process wrote, so later reads need not decode it.
pub(super) fn remember(hash: &str, shard: Arc<Shard>) {
    if let Ok(mut cache) = SHARDS.lock() {
        cache.insert(hash.to_owned(), shard);
    }
}

/// Pack a root envelope and its shards into one self-contained archive.
pub(super) fn pack_archive<'a>(
    root: &'a [u8],
    shards: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Vec<u8>> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
    encoder.write_all(ARCHIVE_MAGIC)?;
    for part in std::iter::once(root).chain(shards) {
        encoder.write_all(&(part.len() as u64).to_le_bytes())?;
        encoder.write_all(part)?;
    }
    Ok(encoder.finish()?)
}

/// The root envelope of a catalog archive, without decoding its shards.
pub(super) fn archive_root(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::new(bytes)?;
    let mut magic = vec![0; ARCHIVE_MAGIC.len()];
    decoder.read_exact(&mut magic)?;
    ensure!(magic == ARCHIVE_MAGIC, "unsupported catalog archive");
    let mut length = [0; 8];
    decoder.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    ensure!(
        length <= CATALOG_ROOT_LIMIT,
        "archived catalog root is too large"
    );
    let mut root = vec![0; usize::try_from(length)?];
    decoder
        .read_exact(&mut root)
        .context("truncated catalog archive")?;
    Ok(root)
}

/// Split a catalog archive into its root envelope and shard bytes.
pub(super) fn unpack_archive(bytes: &[u8], limit: u64) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut decoded = Vec::new();
    zstd::stream::read::Decoder::new(bytes)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut decoded)
        .context("invalid catalog archive")?;
    ensure!(
        decoded.len() as u64 <= limit,
        "catalog archive exceeds {limit} bytes"
    );
    let mut rest = decoded
        .strip_prefix(ARCHIVE_MAGIC)
        .context("unsupported catalog archive")?;
    let mut parts = Vec::new();
    while !rest.is_empty() {
        let Some((length, tail)) = rest.split_first_chunk::<8>() else {
            bail!("truncated catalog archive");
        };
        let length = usize::try_from(u64::from_le_bytes(*length))?;
        ensure!(length <= tail.len(), "truncated catalog archive");
        let (part, tail) = tail.split_at(length);
        parts.push(part.to_vec());
        rest = tail;
    }
    let mut parts = parts.into_iter();
    let root = parts.next().context("empty catalog archive")?;
    Ok((root, parts.collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_roots_require_exactly_the_catalog_threshold() -> Result<()> {
        use crate::repo::keys::{
            Signers,
            tests::key,
            trust::{self, TrustDraft},
        };
        let root_key = key();
        let documents: Vec<_> = (0..3)
            .map(|_| {
                ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                    .unwrap()
            })
            .collect();
        let load = |index: usize| -> crate::repo::keys::SigningKey {
            ring::signature::Ed25519KeyPair::from_pkcs8(documents[index].as_ref())
                .unwrap()
                .into()
        };
        let dir = tempfile::tempdir()?;
        let write = |name: &str, key: &crate::repo::keys::SigningKey| -> Result<_> {
            let path = dir.path().join(name);
            std::fs::write(&path, hex::encode(key.public_key()))?;
            Ok(path)
        };
        let root_public = [write("root", &root_key)?];
        let catalog_public: Vec<_> = (0..3)
            .map(|index| write(&format!("catalog{index}"), &load(index)))
            .collect::<Result<_>>()?;
        let draft = trust::draft(
            None,
            &TrustDraft {
                publisher: Some("canopa"),
                root_keys: &root_public,
                catalog_keys: &catalog_public,
                catalog_threshold: Some(2),
                ..TrustDraft::default()
            },
        )?;
        let signed = trust::combine(
            None,
            &draft.encode()?,
            Vec::new(),
            Some(&Signers::from(vec![root_key])),
        )?;
        let chain: trust::TrustChain = serde_json::from_value(serde_json::json!([signed]))?;
        let verified = chain.verify("canopa")?;
        let payload = br#"{"format":1,"publisher":"canopa","sequence":1}"#;
        let signers = |indexes: &[usize], extra: bool| {
            let mut keys: Vec<_> = indexes.iter().map(|index| load(*index)).collect();
            if extra {
                keys.push(key());
            }
            Signers::from(keys)
        };
        let sealed = seal(payload, &signers(&[0, 1], false))?;
        let opened = open(&sealed)?;
        assert_eq!(opened.payload, payload);
        opened.verify(&verified, "test")?;

        let one = open(&seal(payload, &signers(&[2], false))?)?;
        assert!(one.verify(&verified, "one").is_err());
        let three = open(&seal(payload, &signers(&[0, 1, 2], false))?)?;
        assert!(three.verify(&verified, "three").is_err());
        let error = open(&seal(payload, &signers(&[0], true))?)?
            .verify(&verified, "unauthorized")
            .unwrap_err();
        assert!(format!("{error:#}").contains("unauthorized key"));
        Ok(())
    }

    #[test]
    fn archives_round_trip_and_reject_truncation() -> Result<()> {
        let packed = pack_archive(b"root", [&b"one"[..], b"two"])?;
        let (root, shards) = unpack_archive(&packed, 1024)?;
        assert_eq!(root, b"root");
        assert_eq!(shards, [b"one".to_vec(), b"two".to_vec()]);
        assert!(unpack_archive(&packed, 8).is_err());
        let mut raw = ARCHIVE_MAGIC.to_vec();
        raw.extend_from_slice(&10u64.to_le_bytes());
        raw.extend_from_slice(b"short");
        let truncated = zstd::encode_all(raw.as_slice(), 1)?;
        assert!(unpack_archive(&truncated, 1024).is_err());
        Ok(())
    }
}
