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
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
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
const SIGNATURE_DOMAIN: &[u8] = b"cps signed catalog root v1\n";
const ARCHIVE_MAGIC: &[u8] = b"cps catalog archive v1\n";
const MAX_SIGNATURES: usize = 16;

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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    signed: String,
    signatures: Vec<EnvelopeSignature>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EnvelopeSignature {
    keyid: String,
    sig: String,
}

/// Sign a root payload. The signature covers exactly these payload bytes.
pub(super) fn seal(payload: &[u8], key: &Ed25519KeyPair) -> Result<Vec<u8>> {
    let mut message = SIGNATURE_DOMAIN.to_vec();
    message.extend_from_slice(payload);
    let envelope = Envelope {
        signed: STANDARD.encode(payload),
        signatures: vec![EnvelopeSignature {
            keyid: digest(key.public_key().as_ref()),
            sig: hex::encode(key.sign(&message).as_ref()),
        }],
    };
    Ok(serde_json::to_vec(&envelope)?)
}

/// A decoded root envelope whose signatures are yet to be checked.
pub(super) struct Opened {
    pub(super) payload: Vec<u8>,
    signatures: Vec<EnvelopeSignature>,
}

/// Decode a root envelope in its only accepted byte form.
pub(super) fn open(bytes: &[u8]) -> Result<Opened> {
    let envelope: Envelope = serde_json::from_slice(bytes).context("invalid catalog envelope")?;
    // One root has one byte encoding, so its SHA-256 identity cannot be
    // changed without the signer: no whitespace, escapes, or hex case.
    ensure!(
        serde_json::to_vec(&envelope)? == bytes,
        "catalog envelope is not canonical"
    );
    let payload = STANDARD
        .decode(&envelope.signed)
        .context("invalid catalog envelope payload")?;
    ensure!(
        STANDARD.encode(&payload) == envelope.signed,
        "catalog envelope payload is not canonical"
    );
    ensure!(
        (1..=MAX_SIGNATURES).contains(&envelope.signatures.len()),
        "catalog envelope requires 1 to {MAX_SIGNATURES} signatures"
    );
    for pair in envelope.signatures.windows(2) {
        ensure!(
            pair[0].keyid < pair[1].keyid,
            "catalog envelope signatures must be sorted by unique key ID"
        );
    }
    for signature in &envelope.signatures {
        validate_digest(&signature.keyid).context("invalid catalog signature key ID")?;
        ensure!(
            signature.sig.len() == 128
                && signature
                    .sig
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "invalid catalog signature encoding"
        );
    }
    Ok(Opened {
        payload,
        signatures: envelope.signatures,
    })
}

impl Opened {
    /// Require every listed signature to verify. Today the only authorized
    /// key is the publisher's current catalog signer.
    pub(super) fn verify(&self, signer: &[u8], hash: &str) -> Result<()> {
        let keyid = digest(signer);
        ensure!(
            self.signatures.len() == 1 && self.signatures[0].keyid == keyid,
            "invalid signature on catalog {hash}: expected exactly the publisher's current key"
        );
        let mut message = SIGNATURE_DOMAIN.to_vec();
        message.extend_from_slice(&self.payload);
        for signature in &self.signatures {
            UnparsedPublicKey::new(&ED25519, signer)
                .verify(&message, &hex::decode(&signature.sig)?)
                .map_err(|_| anyhow::anyhow!("invalid signature on catalog {hash}"))?;
        }
        Ok(())
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) key_rotations: Vec<keys::Rotation>,
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

    pub(super) fn key() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    #[test]
    fn envelopes_have_one_byte_form_and_sign_the_exact_payload() -> Result<()> {
        let key = key();
        let signer = key.public_key().as_ref();
        let payload = br#"{"format":1,"publisher":"canopa","sequence":1}"#;
        let sealed = seal(payload, &key)?;
        let opened = open(&sealed)?;
        assert_eq!(opened.payload, payload);
        opened.verify(signer, "test")?;

        let text = String::from_utf8(sealed.clone())?;
        let mut variants = vec![
            text.replacen(':', ": ", 1),
            format!("{text}\n"),
            text.replacen("\"signed\"", "\"\\u0073igned\"", 1),
        ];
        let sig = &opened.signatures[0].sig;
        variants.push(text.replace(sig.as_str(), &sig.to_uppercase()));
        let unpadded = STANDARD.encode(payload).trim_end_matches('=').to_owned();
        variants.push(text.replace(&STANDARD.encode(payload), &unpadded));
        for variant in variants {
            assert_ne!(variant.as_bytes(), sealed.as_slice());
            assert!(open(variant.as_bytes()).is_err(), "accepted {variant}");
        }

        // The signature covers the payload bytes, not a re-serialization.
        let spaced = br#"{"format": 1,"publisher":"canopa","sequence":1}"#;
        let mut forged: Envelope = serde_json::from_slice(&sealed)?;
        forged.signed = STANDARD.encode(spaced);
        let forged = open(&serde_json::to_vec(&forged)?)?;
        let error = forged.verify(signer, "forged").unwrap_err();
        assert!(format!("{error:#}").contains("invalid signature"));

        // Another key, or an extra signature, is not the publisher's signature.
        let other = super::tests::key();
        assert!(opened.verify(other.public_key().as_ref(), "test").is_err());
        let mut doubled: Envelope = serde_json::from_slice(&sealed)?;
        let mut extra = doubled.signatures[0].clone();
        extra.keyid = digest(other.public_key().as_ref());
        doubled.signatures.push(extra);
        doubled.signatures.sort_by(|a, b| a.keyid.cmp(&b.keyid));
        let doubled = open(&serde_json::to_vec(&doubled)?)?;
        assert!(doubled.verify(signer, "doubled").is_err());
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
