//! The publisher trust root.
//!
//! A trust root names the publisher's offline root keys and online catalog
//! keys, each with a signature threshold, and expires at a fixed time. Version
//! 1 is signed by a threshold of its own root keys. Every later version is
//! signed by a threshold of the previous version's root keys and a threshold of
//! its own, so the root keys alone can rotate or revoke any key. Each catalog
//! root carries the whole version chain it was signed under, and its
//! signatures must come from exactly the catalog threshold of the last
//! version's catalog keys.

use std::{
    collections::BTreeSet,
    fmt,
    fs::{self, File},
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail, ensure};
use jiff::{Span, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{
    Envelope, EnvelopeSignature, MAX_SIGNATURES, PublicKey, Signers, TRUST_ROOT,
    signer::{self, CommandSigner, KeySpec},
};
use crate::{
    model::{digest, validate_publisher},
    repo::{read_limited, write_atomic},
};

/// The most versions a catalog's trust history may hold.
pub(crate) const MAX_VERSIONS: usize = 64;
/// The repository directory holding `1.json`, `2.json`, and so on.
pub(crate) const TRUST_DIR: &str = "trust";
pub(crate) const TRUST_ROOT_LIMIT: u64 = 64 * 1024;
const DEFAULT_LIFETIME_DAYS: i64 = 365;
const KEY_LIMIT: u64 = 16 * 1024;

/// A raw Ed25519 public key, serialized as 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Key(pub(crate) PublicKey);

impl Serialize for Key {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut key = [0; 32];
        if text.len() != 64 || text.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(serde::de::Error::custom("invalid trust root public key"));
        }
        hex::decode_to_slice(&text, &mut key).map_err(serde::de::Error::custom)?;
        Ok(Self(key))
    }
}

/// The signed content of one trust root version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrustRoot {
    pub(crate) publisher: String,
    pub(crate) version: u64,
    pub(crate) expires: String,
    pub(crate) root_keys: BTreeSet<Key>,
    pub(crate) root_threshold: usize,
    pub(crate) catalog_keys: BTreeSet<Key>,
    pub(crate) catalog_threshold: usize,
}

fn parse_expiry(text: &str) -> Result<Timestamp> {
    let expires = Timestamp::from_str(text).context("invalid trust root expiry")?;
    ensure!(
        expires.subsec_nanosecond() == 0 && expires.to_string() == text,
        "trust root expiry must be an RFC 3339 UTC time in whole seconds, like 2027-01-01T00:00:00Z"
    );
    Ok(expires)
}

impl TrustRoot {
    /// Decode a payload in its only accepted byte form.
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let root: Self = serde_json::from_slice(payload).context("invalid trust root")?;
        ensure!(
            serde_json::to_vec(&root)? == payload,
            "trust root payload is not canonical"
        );
        root.validate()?;
        Ok(root)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    fn validate(&self) -> Result<()> {
        validate_publisher(&self.publisher).context("invalid trust root publisher")?;
        ensure!(self.version > 0, "trust root versions start at 1");
        parse_expiry(&self.expires)?;
        for (role, keys, threshold) in [
            ("root", &self.root_keys, self.root_threshold),
            ("catalog", &self.catalog_keys, self.catalog_threshold),
        ] {
            ensure!(
                keys.len() <= MAX_SIGNATURES && (1..=keys.len()).contains(&threshold),
                "trust root {role} role requires 1 to {MAX_SIGNATURES} keys and a threshold within that range"
            );
        }
        Ok(())
    }

    pub(crate) fn expires(&self) -> Timestamp {
        parse_expiry(&self.expires).expect("validated trust root expiry")
    }

    pub(crate) fn root_keys(&self) -> BTreeSet<PublicKey> {
        self.root_keys.iter().map(|key| key.0).collect()
    }

    pub(crate) fn catalog_keys(&self) -> BTreeSet<PublicKey> {
        self.catalog_keys.iter().map(|key| key.0).collect()
    }

    /// Reject a trust root that has expired.
    pub(crate) fn check_unexpired(&self) -> Result<()> {
        ensure!(
            self.expires() > Timestamp::now(),
            "trust root version {} for {} expired at {}; the publisher's root keys must sign a new version",
            self.version,
            self.publisher,
            self.expires
        );
        Ok(())
    }

    fn count(keys: &BTreeSet<Key>, signers: &BTreeSet<PublicKey>) -> usize {
        keys.iter().filter(|key| signers.contains(&key.0)).count()
    }
}

/// A trust root and its signatures, as stored, published, and pinned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct SignedTrustRoot(Envelope);

impl SignedTrustRoot {
    /// Decode a standalone trust root file in its only accepted byte form.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(Self(Envelope::decode(&TRUST_ROOT, bytes)?))
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.0.encode()
    }

    /// The payload and its SHA-256, without checking signatures.
    fn open(&self) -> Result<(TrustRoot, String)> {
        let payload = self.0.validate(&TRUST_ROOT)?;
        Ok((TrustRoot::decode(&payload)?, digest(&payload)))
    }

    /// Verify this version as the successor of `previous`, or as version 1.
    fn verify(&self, previous: Option<&TrustRoot>) -> Result<Verified> {
        let (root, payload_sha256) = self.open()?;
        let mut authorized = root.root_keys();
        if let Some(previous) = previous {
            ensure!(
                root.publisher == previous.publisher && root.version == previous.version + 1,
                "trust root version {} does not follow version {} for {}",
                root.version,
                previous.version,
                previous.publisher
            );
            authorized.extend(previous.root_keys());
        } else {
            ensure!(
                root.version == 1,
                "trust root history must start at version 1"
            );
        }
        let signers = self.0.verify(&TRUST_ROOT, &authorized)?;
        check_root_signatures(&root, previous, &signers)?;
        Ok(Verified {
            root,
            payload_sha256,
        })
    }
}

fn check_root_signatures(
    root: &TrustRoot,
    previous: Option<&TrustRoot>,
    signers: &BTreeSet<PublicKey>,
) -> Result<()> {
    let found = TrustRoot::count(&root.root_keys, signers);
    ensure!(
        found >= root.root_threshold,
        "trust root version {} requires {} signatures from its own root keys, found {found}",
        root.version,
        root.root_threshold
    );
    if let Some(previous) = previous {
        let found = TrustRoot::count(&previous.root_keys, signers);
        ensure!(
            found >= previous.root_threshold,
            "trust root version {} requires {} signatures from the root keys of version {}, found {found}",
            root.version,
            previous.root_threshold,
            previous.version
        );
    }
    Ok(())
}

struct Verified {
    root: TrustRoot,
    payload_sha256: String,
}

/// A client's pinned trust root: any version, checked against its own root keys.
pub(crate) struct Pinned {
    root: TrustRoot,
    payload_sha256: String,
}

impl Pinned {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let signed = SignedTrustRoot::decode(bytes).context("reading the pinned trust root")?;
        let (root, payload_sha256) = signed.open()?;
        // A later version also carries its predecessor's signatures, which a
        // standalone pin cannot check. Its own root keys must still reach
        // their threshold, and every signature they made must be valid.
        let signers = signed.0.verify_known(&TRUST_ROOT, &root.root_keys())?;
        check_root_signatures(&root, None, &signers)?;
        Ok(Self {
            root,
            payload_sha256,
        })
    }

    pub(crate) fn root(&self) -> &TrustRoot {
        &self.root
    }
}

/// Every trust root version from 1 through the one a catalog is signed under.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct TrustChain(Vec<SignedTrustRoot>);

impl TrustChain {
    pub(crate) fn versions(&self) -> &[SignedTrustRoot] {
        &self.0
    }

    /// Verify every version from version 1 for this publisher.
    pub(crate) fn verify(&self, publisher: &str) -> Result<VerifiedChain> {
        ensure!(
            !self.0.is_empty(),
            "catalog for {publisher} carries no trust root"
        );
        ensure!(
            self.0.len() <= MAX_VERSIONS,
            "publisher trust root history exceeds {MAX_VERSIONS} versions"
        );
        let mut versions: Vec<Verified> = Vec::with_capacity(self.0.len());
        for signed in &self.0 {
            let verified = signed.verify(versions.last().map(|previous| &previous.root))?;
            ensure!(
                verified.root.publisher == publisher,
                "trust root belongs to publisher {}, not {publisher}",
                verified.root.publisher
            );
            versions.push(verified);
        }
        Ok(VerifiedChain(versions))
    }

    /// Require `next` to extend this history unchanged.
    pub(crate) fn check_successor(&self, next: &Self) -> Result<()> {
        ensure!(
            next.0.starts_with(&self.0),
            "release drops or changes the publisher trust root history"
        );
        Ok(())
    }
}

/// A trust root history whose signatures all verify.
pub(crate) struct VerifiedChain(Vec<Verified>);

impl VerifiedChain {
    /// The version catalogs carrying this history are signed under.
    pub(crate) fn current(&self) -> &TrustRoot {
        &self.0.last().expect("verified chains are not empty").root
    }

    /// The SHA-256 of the version 1 payload: a stable publisher identity.
    pub(crate) fn genesis(&self) -> &str {
        &self.0[0].payload_sha256
    }

    /// Every root and catalog key of every version.
    pub(crate) fn keys(&self) -> BTreeSet<PublicKey> {
        self.0
            .iter()
            .flat_map(|version| {
                version
                    .root
                    .root_keys
                    .iter()
                    .chain(&version.root.catalog_keys)
            })
            .map(|key| key.0)
            .collect()
    }

    /// Require the history to contain the client's pinned version unchanged.
    pub(crate) fn check_pinned(&self, pinned: &Pinned) -> Result<()> {
        let version = pinned.root.version;
        ensure!(
            pinned.root.publisher == self.current().publisher,
            "pinned trust root belongs to publisher {}",
            pinned.root.publisher
        );
        let index = usize::try_from(version - 1)?;
        let found = self.0.get(index).with_context(|| {
            format!(
                "catalog for {} is signed under trust root version {}, older than the pinned version {version}",
                pinned.root.publisher,
                self.current().version
            )
        })?;
        ensure!(
            found.payload_sha256 == pinned.payload_sha256,
            "catalog trust history differs from the pinned trust root version {version} for {}",
            pinned.root.publisher
        );
        Ok(())
    }

    /// Require exactly the catalog threshold of distinct, authorized signatures.
    pub(crate) fn verify_catalog(&self, envelope: &Envelope, hash: &str) -> Result<()> {
        let current = self.current();
        envelope
            .verify(&super::CATALOG, &current.catalog_keys())
            .with_context(|| format!("invalid signature on catalog {hash}"))?;
        ensure!(
            envelope.signatures().len() == current.catalog_threshold,
            "catalog {hash} requires exactly {} signatures from the catalog keys of trust root version {}, found {}",
            current.catalog_threshold,
            current.version,
            envelope.signatures().len()
        );
        Ok(())
    }
}

/// How a client uses a catalog it verifies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Freshness {
    /// An exact pin or a historical catalog: its trust root may have expired
    /// or been superseded.
    Pinned,
    /// A new current catalog: its trust root must not have expired.
    Current,
}

/// Verify a catalog's trust history against a client's pinned trust root.
pub(crate) fn verify(
    publisher: &str,
    chain: &TrustChain,
    pinned: &[u8],
    freshness: Freshness,
) -> Result<VerifiedChain> {
    let pinned = Pinned::parse(pinned)?;
    ensure!(
        pinned.root.publisher == publisher,
        "pinned trust root belongs to publisher {}, not {publisher}",
        pinned.root.publisher
    );
    let verified = chain.verify(publisher)?;
    verified.check_pinned(&pinned)?;
    if freshness == Freshness::Current {
        verified.current().check_unexpired()?;
    }
    Ok(verified)
}

/// Every key named by a pinned trust root.
pub(crate) fn pinned_keys(pinned: &[u8]) -> Result<BTreeSet<PublicKey>> {
    let pinned = Pinned::parse(pinned)?;
    Ok(pinned
        .root
        .root_keys
        .iter()
        .chain(&pinned.root.catalog_keys)
        .map(|key| key.0)
        .collect())
}

/// Read a pinned trust root file, returning its canonical bytes.
pub(crate) fn read_pinned(path: &Path) -> Result<Vec<u8>> {
    (|| -> Result<_> {
        let bytes = read_limited(File::open(path)?, TRUST_ROOT_LIMIT)?;
        Pinned::parse(&bytes)?;
        Ok(bytes)
    })()
    .with_context(|| format!("reading trust root {}", path.display()))
}

/// The trust root history stored in a publisher repository.
pub(crate) struct Store {
    pub(crate) chain: TrustChain,
    pub(crate) verified: VerifiedChain,
}

impl Store {
    /// Read `trust/1.json` through the newest version, or `None` before
    /// the repository has a trust root.
    pub(crate) fn read(repo: &Path) -> Result<Option<Self>> {
        let directory = repo.join(TRUST_DIR);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut versions = BTreeSet::new();
        for entry in entries {
            let name = entry?.file_name();
            let name = name.to_str().context("invalid trust root file name")?;
            if name.starts_with('.') {
                continue;
            }
            let version = name
                .strip_suffix(".json")
                .and_then(|version| version.parse::<usize>().ok())
                .filter(|version| *version > 0 && format!("{version}.json") == name)
                .with_context(|| format!("unexpected file {name} in the trust root directory"))?;
            versions.insert(version);
        }
        if versions.is_empty() {
            return Ok(None);
        }
        ensure!(
            versions.iter().copied().eq(1..=versions.len()),
            "repository trust root versions are not contiguous from 1"
        );
        let mut chain = Vec::with_capacity(versions.len());
        let mut publisher = None;
        for version in versions {
            let path = directory.join(format!("{version}.json"));
            let bytes = read_limited(File::open(&path)?, TRUST_ROOT_LIMIT)?;
            let signed = SignedTrustRoot::decode(&bytes)
                .with_context(|| format!("reading trust root {}", path.display()))?;
            if publisher.is_none() {
                publisher = Some(signed.open()?.0.publisher);
            }
            chain.push(signed);
        }
        let chain = TrustChain(chain);
        let verified = chain.verify(&publisher.expect("at least one version"))?;
        Ok(Some(Self { chain, verified }))
    }

    pub(crate) fn path(repo: &Path, version: u64) -> PathBuf {
        repo.join(TRUST_DIR).join(format!("{version}.json"))
    }

    /// The version 1 trust root bytes: the publisher's own pin.
    pub(crate) fn genesis_bytes(&self) -> Result<Vec<u8>> {
        self.chain.0[0].encode()
    }
}

/// Membership changes for the next trust root version. Empty key lists and
/// absent thresholds keep the previous version's values.
#[derive(Default)]
pub struct TrustDraft<'a> {
    pub publisher: Option<&'a str>,
    pub root_keys: &'a [PathBuf],
    pub root_threshold: Option<usize>,
    pub catalog_keys: &'a [PathBuf],
    pub catalog_threshold: Option<usize>,
    /// RFC 3339 UTC expiry; one year from now when absent.
    pub expires: Option<&'a str>,
}

/// Read a public key: a DER, PEM, or hexadecimal Ed25519 public key file,
/// or the declared key of a `command:` signer descriptor.
pub(crate) fn read_public_key(spec: &Path) -> Result<PublicKey> {
    (|| match KeySpec::parse(spec) {
        KeySpec::Command(descriptor) => {
            signer::ed25519_public_key(&CommandSigner::open(&descriptor)?.public_key()?)
        }
        KeySpec::File(path) => {
            signer::ed25519_public_key(&read_limited(File::open(path)?, KEY_LIMIT)?)
        }
    })()
    .with_context(|| format!("reading public key {}", spec.display()))
}

fn read_keys(paths: &[PathBuf]) -> Result<Option<BTreeSet<Key>>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut keys = BTreeSet::new();
    for path in paths {
        ensure!(
            keys.insert(Key(read_public_key(path)?)),
            "public key {} is given more than once",
            path.display()
        );
    }
    Ok(Some(keys))
}

fn new_expiry(expires: Option<&str>) -> Result<String> {
    let expires = match expires {
        Some(text) => parse_expiry(text)?,
        None => Timestamp::now()
            .checked_add(Span::new().hours(24 * DEFAULT_LIFETIME_DAYS))?
            .round(jiff::Unit::Second)?,
    };
    ensure!(
        expires > Timestamp::now(),
        "trust root expiry {expires} is in the past"
    );
    Ok(expires.to_string())
}

/// Build the next version's payload after `previous`.
pub(crate) fn draft(previous: Option<&TrustRoot>, change: &TrustDraft<'_>) -> Result<TrustRoot> {
    draft_with(
        previous,
        change.publisher,
        read_keys(change.root_keys)?,
        change.root_threshold,
        read_keys(change.catalog_keys)?,
        change.catalog_threshold,
        change.expires,
    )
}

pub(crate) fn draft_with(
    previous: Option<&TrustRoot>,
    publisher: Option<&str>,
    root_keys: Option<BTreeSet<Key>>,
    root_threshold: Option<usize>,
    catalog_keys: Option<BTreeSet<Key>>,
    catalog_threshold: Option<usize>,
    expires: Option<&str>,
) -> Result<TrustRoot> {
    let root = match previous {
        None => TrustRoot {
            publisher: publisher
                .context("the first trust root requires a publisher")?
                .to_owned(),
            version: 1,
            expires: new_expiry(expires)?,
            root_keys: root_keys.context("the first trust root requires root keys")?,
            root_threshold: root_threshold.unwrap_or(1),
            catalog_keys: catalog_keys.context("the first trust root requires catalog keys")?,
            catalog_threshold: catalog_threshold.unwrap_or(1),
        },
        Some(previous) => {
            if let Some(publisher) = publisher {
                ensure!(
                    publisher == previous.publisher,
                    "repository trust root belongs to publisher {}",
                    previous.publisher
                );
            }
            TrustRoot {
                publisher: previous.publisher.clone(),
                version: previous.version + 1,
                expires: new_expiry(expires)?,
                root_keys: root_keys.unwrap_or_else(|| previous.root_keys.clone()),
                root_threshold: root_threshold.unwrap_or(previous.root_threshold),
                catalog_keys: catalog_keys.unwrap_or_else(|| previous.catalog_keys.clone()),
                catalog_threshold: catalog_threshold.unwrap_or(previous.catalog_threshold),
            }
        }
    };
    ensure!(
        usize::try_from(root.version)? <= MAX_VERSIONS,
        "publisher trust root history is limited to {MAX_VERSIONS} versions"
    );
    root.validate()?;
    Ok(root)
}

/// Combine a payload with detached signatures and signatures from local or
/// external signers, then verify it as the successor of `previous`.
pub(crate) fn combine(
    previous: Option<&TrustRoot>,
    payload: &[u8],
    detached: Vec<EnvelopeSignature>,
    signers: Option<&Signers>,
) -> Result<SignedTrustRoot> {
    let root = TrustRoot::decode(payload)?;
    let mut signatures = detached;
    if let Some(signers) = signers {
        signatures.extend(signers.sign(&TRUST_ROOT, payload)?);
    }
    let mut seen = BTreeSet::new();
    for signature in &signatures {
        ensure!(
            seen.insert(signature.key_id().to_owned()),
            "trust root key {} signed more than once",
            signature.key_id()
        );
    }
    let expected = previous.map_or(1, |previous| previous.version + 1);
    ensure!(
        root.version == expected,
        "draft is for trust root version {}, but the repository's next version is {expected}",
        root.version
    );
    let signed = SignedTrustRoot(Envelope::new(&TRUST_ROOT, payload, signatures)?);
    signed.verify(previous)?.root.check_unexpired()?;
    Ok(signed)
}

/// Sign a draft payload for a later `combine`.
pub(crate) fn sign_detached(payload: &[u8], signers: &Signers) -> Result<Vec<u8>> {
    TrustRoot::decode(payload)?;
    let signatures = signers.sign(&TRUST_ROOT, payload)?;
    let mut bytes = Vec::new();
    for signature in signatures {
        bytes.extend(serde_json::to_vec(&signature)?);
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Read detached signatures, one canonical JSON object per line.
pub(crate) fn read_detached(path: &Path) -> Result<Vec<EnvelopeSignature>> {
    (|| {
        let bytes = read_limited(File::open(path)?, TRUST_ROOT_LIMIT)?;
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| EnvelopeSignature::decode(&TRUST_ROOT, line))
            .collect::<Result<Vec<_>>>()
    })()
    .with_context(|| format!("reading detached signatures {}", path.display()))
}

/// Write the next version into a repository.
pub(crate) fn store(repo: &Path, signed: &SignedTrustRoot) -> Result<PathBuf> {
    let (root, _) = signed.open()?;
    let directory = repo.join(TRUST_DIR);
    fs::create_dir_all(&directory)?;
    let path = Store::path(repo, root.version);
    ensure!(
        !path.try_exists()?,
        "trust root version {} already exists",
        root.version
    );
    write_atomic(&path, &signed.encode()?, true)?;
    Ok(path)
}

/// SHA-256 fingerprint of a raw public key, as used for key IDs.
pub(crate) fn fingerprint(key: &PublicKey) -> String {
    digest(key)
}

impl fmt::Display for TrustRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Publisher: {}", self.publisher)?;
        writeln!(f, "Version: {}", self.version)?;
        writeln!(f, "Expires: {}", self.expires)?;
        writeln!(
            f,
            "Root keys ({} of {} required):",
            self.root_threshold,
            self.root_keys.len()
        )?;
        for key in &self.root_keys {
            writeln!(f, "  {}", fingerprint(&key.0))?;
        }
        writeln!(
            f,
            "Catalog keys ({} of {} required):",
            self.catalog_threshold,
            self.catalog_keys.len()
        )?;
        for key in &self.catalog_keys {
            writeln!(f, "  {}", fingerprint(&key.0))?;
        }
        Ok(())
    }
}

/// Require a fetched trust root to name exactly the expected root keys.
pub(crate) fn check_expected_root_keys(root: &TrustRoot, expected: &[String]) -> Result<()> {
    let actual: BTreeSet<String> = root
        .root_keys
        .iter()
        .map(|key| fingerprint(&key.0))
        .collect();
    let mut wanted = BTreeSet::new();
    for key in expected {
        crate::model::validate_digest(key).context("invalid expected root key fingerprint")?;
        wanted.insert(key.clone());
    }
    if actual != wanted {
        bail!(
            "trust root for {} names root keys {} but {} were expected",
            root.publisher,
            actual.into_iter().collect::<Vec<_>>().join(", "),
            wanted.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

/// Read a draft payload written by `cps repo trust draft`.
pub(crate) fn read_draft(path: &Path) -> Result<Vec<u8>> {
    (|| -> Result<_> {
        let payload = read_limited(File::open(path)?, TRUST_ROOT_LIMIT)?;
        TrustRoot::decode(&payload)?;
        Ok(payload)
    })()
    .with_context(|| format!("reading trust root draft {}", path.display()))
}

/// Describe a trust root file, or the newest version in a repository.
pub(crate) fn describe(path: &Path) -> Result<String> {
    let root = if path.is_dir() {
        let store = Store::read(path)?
            .with_context(|| format!("repository {} has no trust root", path.display()))?;
        store.verified.current().clone()
    } else {
        Pinned::parse(&read_pinned(path)?)?.root
    };
    let mut text = root.to_string();
    if root.expires() <= Timestamp::now() {
        text.push_str("Status: expired\n");
    }
    Ok(text)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::repo::keys::SigningKey;
    use ring::signature::Ed25519KeyPair;

    /// Keys reloadable from their PKCS#8 documents, with public key files.
    pub(crate) struct Keys {
        dir: tempfile::TempDir,
        documents: Vec<ring::pkcs8::Document>,
    }

    impl Keys {
        pub(crate) fn new(count: usize) -> Result<Self> {
            let dir = tempfile::tempdir()?;
            let mut documents = Vec::new();
            for index in 0..count {
                let document =
                    Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
                fs::write(dir.path().join(format!("{index}.der")), document.as_ref())?;
                let key: SigningKey = Ed25519KeyPair::from_pkcs8(document.as_ref())
                    .unwrap()
                    .into();
                fs::write(
                    dir.path().join(format!("{index}.pub")),
                    hex::encode(key.public_key()),
                )?;
                documents.push(document);
            }
            Ok(Self { dir, documents })
        }

        pub(crate) fn private(&self, index: usize) -> PathBuf {
            self.dir.path().join(format!("{index}.der"))
        }

        pub(crate) fn public(&self, index: usize) -> PathBuf {
            self.dir.path().join(format!("{index}.pub"))
        }

        pub(crate) fn signers(&self, indexes: &[usize]) -> Signers {
            Signers::from(
                indexes
                    .iter()
                    .map(|index| {
                        Ed25519KeyPair::from_pkcs8(self.documents[*index].as_ref())
                            .unwrap()
                            .into()
                    })
                    .collect::<Vec<SigningKey>>(),
            )
        }

        pub(crate) fn key(&self, index: usize) -> Key {
            Key(*self.signers(&[index]).0[0].public_key())
        }
    }

    fn root(keys: &Keys, version: u64, root: &[usize], root_threshold: usize) -> TrustRoot {
        TrustRoot {
            publisher: "canopa".to_owned(),
            version,
            expires: "2099-01-01T00:00:00Z".to_owned(),
            root_keys: root.iter().map(|index| keys.key(*index)).collect(),
            root_threshold,
            catalog_keys: BTreeSet::from([keys.key(9)]),
            catalog_threshold: 1,
        }
    }

    /// Sign without the expiry and successor checks of [`combine`].
    pub(crate) fn sign(root: &TrustRoot, signers: &Signers) -> SignedTrustRoot {
        let payload = root.encode().unwrap();
        SignedTrustRoot(super::super::seal(&TRUST_ROOT, &payload, signers).unwrap())
    }

    #[test]
    fn rotations_need_the_old_and_the_new_root_thresholds() -> Result<()> {
        let keys = Keys::new(10)?;
        let first = root(&keys, 1, &[0, 1, 2], 2);
        let genesis = sign(&first, &keys.signers(&[0, 1]));
        // Below its own threshold, version 1 is invalid.
        let weak = sign(&first, &keys.signers(&[0]));
        assert!(TrustChain(vec![weak]).verify("canopa").is_err());
        assert!(TrustChain(vec![genesis.clone()]).verify("other").is_err());

        // Version 2 replaces keys 1 and 2 with 3 and 4.
        let second = root(&keys, 2, &[0, 3, 4], 2);
        let chain = |signed: SignedTrustRoot| TrustChain(vec![genesis.clone(), signed]);
        for (signers, valid) in [
            (&[0, 1, 3, 4][..], true),
            (&[0, 3][..], false), // only one old root key
            (&[1, 2][..], false), // no new root keys
            (&[1, 2, 3][..], false),
            (&[0, 1, 3][..], true),
        ] {
            let result = chain(sign(&second, &keys.signers(signers))).verify("canopa");
            assert_eq!(
                result.is_ok(),
                valid,
                "{signers:?}: {result:?}",
                result = result.err()
            );
        }
        // A catalog key cannot rotate the trust root.
        let error = chain(sign(&second, &keys.signers(&[0, 1, 3, 9])))
            .verify("canopa")
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("unauthorized key"));
        // Versions must be contiguous.
        let skipped = root(&keys, 3, &[0, 1, 2], 2);
        assert!(
            chain(sign(&skipped, &keys.signers(&[0, 1])))
                .verify("canopa")
                .is_err()
        );

        let valid = chain(sign(&second, &keys.signers(&[0, 1, 3])));
        let verified = valid.verify("canopa")?;
        assert_eq!(verified.current().version, 2);
        assert_eq!(verified.keys().len(), 6);
        // A successor must extend the history unchanged.
        TrustChain(vec![genesis.clone()]).check_successor(&valid)?;
        assert!(
            valid
                .check_successor(&TrustChain(vec![genesis.clone()]))
                .is_err()
        );
        let fork = chain(sign(&second, &keys.signers(&[0, 1, 4])));
        assert!(valid.check_successor(&fork).is_err());
        Ok(())
    }

    #[test]
    fn clients_pin_any_version_and_reject_older_or_different_histories() -> Result<()> {
        let keys = Keys::new(10)?;
        let first = sign(&root(&keys, 1, &[0], 1), &keys.signers(&[0]));
        let second = sign(&root(&keys, 2, &[1], 1), &keys.signers(&[0, 1]));
        let chain = TrustChain(vec![first.clone(), second.clone()]);
        // A later version pins on its own root keys; the predecessor's
        // signature is ignored when it is standalone.
        for pin in [&first, &second] {
            verify("canopa", &chain, &pin.encode()?, Freshness::Current)?;
        }
        let error = verify(
            "canopa",
            &TrustChain(vec![first.clone()]),
            &second.encode()?,
            Freshness::Pinned,
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("older than the pinned version 2"));
        // Another history with the same version number is not the pin.
        let other = sign(&root(&keys, 1, &[2], 1), &keys.signers(&[2]));
        assert!(verify("canopa", &chain, &other.encode()?, Freshness::Pinned).is_err());
        assert!(verify("other", &chain, &first.encode()?, Freshness::Pinned).is_err());
        // A pin needs its own root threshold.
        let unsigned = sign(&root(&keys, 2, &[1], 1), &keys.signers(&[0]));
        assert!(Pinned::parse(&unsigned.encode()?).is_err());
        // The pinned file must be canonical.
        let mut spaced = first.encode()?;
        spaced.push(b'\n');
        assert!(Pinned::parse(&spaced).is_err());
        Ok(())
    }

    #[test]
    fn expiry_rejects_new_current_catalogs_but_not_pinned_history() -> Result<()> {
        let keys = Keys::new(10)?;
        let mut expired = root(&keys, 1, &[0], 1);
        expired.expires = "2020-01-01T00:00:00Z".to_owned();
        let expired = sign(&expired, &keys.signers(&[0]));
        let chain = TrustChain(vec![expired.clone()]);
        let pin = expired.encode()?;
        verify("canopa", &chain, &pin, Freshness::Pinned)?;
        let error = verify("canopa", &chain, &pin, Freshness::Current)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("expired at 2020-01-01T00:00:00Z"));
        // A renewed version makes the same pin current again.
        let renewed = sign(&root(&keys, 2, &[0], 1), &keys.signers(&[0]));
        verify(
            "canopa",
            &TrustChain(vec![expired, renewed]),
            &pin,
            Freshness::Current,
        )?;
        for text in [
            "2027-01-01T00:00:00.5Z",
            "2027-01-01T00:00:00+00:00",
            "soon",
        ] {
            assert!(parse_expiry(text).is_err(), "{text}");
        }
        Ok(())
    }

    #[test]
    fn detached_signatures_combine_into_the_next_version() -> Result<()> {
        let keys = Keys::new(10)?;
        let first = root(&keys, 1, &[0, 1], 2);
        let next = draft(
            Some(&first),
            &TrustDraft {
                root_keys: &[keys.public(0), keys.public(2)],
                catalog_keys: &[keys.public(3), keys.public(4)],
                catalog_threshold: Some(2),
                ..TrustDraft::default()
            },
        )?;
        assert_eq!(
            (next.version, next.root_threshold, next.catalog_threshold),
            (2, 2, 2)
        );
        let payload = next.encode()?;
        let dir = tempfile::tempdir()?;
        let mut detached = Vec::new();
        for index in [1, 2] {
            let path = dir.path().join(format!("{index}.sig"));
            fs::write(&path, sign_detached(&payload, &keys.signers(&[index]))?)?;
            detached.extend(read_detached(&path)?);
        }
        // Key 0 belongs to both versions and signs locally.
        let local = keys.signers(&[0]);
        let signed = combine(Some(&first), &payload, detached.clone(), Some(&local))?;
        assert_eq!(signed.0.signatures().len(), 3);
        assert!(combine(Some(&first), &payload, detached[..1].to_vec(), Some(&local)).is_err());
        // The same key may not sign twice.
        let twice = [detached.clone(), detached.clone()].concat();
        assert!(combine(Some(&first), &payload, twice, None).is_err());
        // A draft is for exactly the next version.
        assert!(combine(None, &payload, detached, Some(&local)).is_err());
        // Thresholds outside the key count are rejected.
        assert!(
            draft(
                Some(&first),
                &TrustDraft {
                    catalog_threshold: Some(2),
                    ..TrustDraft::default()
                },
            )
            .is_err()
        );
        let expected: Vec<_> = [0, 2]
            .into_iter()
            .map(|index| fingerprint(&keys.key(index).0))
            .collect();
        check_expected_root_keys(&next, &expected)?;
        assert!(check_expected_root_keys(&next, &expected[..1]).is_err());
        Ok(())
    }
}
