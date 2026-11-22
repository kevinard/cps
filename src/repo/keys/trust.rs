//! The publisher trust root.
//!
//! A trust root names the publisher's offline root keys and online catalog
//! keys, each with a signature threshold, and expires at a fixed time. Version
//! 1 is signed by a threshold of its own root keys. Every later version is
//! signed by a threshold of the previous version's root keys and a threshold of
//! its own, so the root keys alone can rotate or revoke any key, and names the
//! SHA-256 of the version 1 payload: the publisher's lasting identity.
//!
//! A trust history is a run of consecutive versions, each verified as the
//! successor of the one before. A catalog root carries the newest
//! [`CATALOG_VERSIONS`] versions of its repository's history, and its
//! signatures must come from exactly the catalog threshold of the last
//! version's catalog keys. A client pins a history: first the version it was
//! given, then every later version it verifies. A catalog's history must agree
//! with the pin on every version both hold and continue it without a gap, and
//! a current catalog must be signed under the newest pinned version or a later
//! one, so a revoked key cannot sign for a client that has seen the revocation.

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
    model::{digest, validate_digest, validate_publisher},
    repo::{read_limited, write_atomic},
};

/// How many of the newest trust root versions a catalog root carries. A
/// client whose pin is fewer versions behind follows every rotation from the
/// catalog alone; one further behind must pin a newer trust root.
pub(crate) const CATALOG_VERSIONS: usize = 16;
/// The most versions a pinned history keeps; older ones are dropped.
const PIN_VERSIONS: usize = 4096;
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

/// A trust root version that is no longer valid. A catalog signed under it
/// cannot become current. A mirror that serves one may only be behind a
/// publisher that has already signed a newer version, so another mirror can
/// still succeed.
#[derive(Debug)]
pub(crate) struct TrustRootExpired {
    pub(crate) version: u64,
    pub(crate) publisher: String,
    pub(crate) expires: String,
}

impl fmt::Display for TrustRootExpired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "trust root version {} for {} expired at {}; the publisher's root keys must sign a new version",
            self.version, self.publisher, self.expires
        )
    }
}

impl std::error::Error for TrustRootExpired {}

/// The signed content of one trust root version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrustRoot {
    pub(crate) publisher: String,
    pub(crate) version: u64,
    /// The SHA-256 of the version 1 payload, in every later version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) genesis_sha256: Option<String>,
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
        match (self.version, &self.genesis_sha256) {
            (1, None) => {}
            (1, Some(_)) => bail!("trust root version 1 cannot name a genesis"),
            (_, Some(genesis)) => validate_digest(genesis).context("invalid trust root genesis")?,
            (version, None) => bail!("trust root version {version} does not name its version 1"),
        }
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

    /// The SHA-256 of the publisher's version 1 payload.
    fn genesis(&self) -> Result<String> {
        match &self.genesis_sha256 {
            Some(genesis) => Ok(genesis.clone()),
            None => Ok(digest(&self.encode()?)),
        }
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
        if self.expires() <= Timestamp::now() {
            return Err(TrustRootExpired {
                version: self.version,
                publisher: self.publisher.clone(),
                expires: self.expires.clone(),
            }
            .into());
        }
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

    /// Verify this version as the successor of `previous`, or as the first
    /// version of a history. A later version on its own also carries its
    /// predecessor's signatures, which cannot be checked without it: its own
    /// root keys must still reach their threshold, and every signature they
    /// made must be valid.
    fn verify(&self, previous: Option<&TrustRoot>) -> Result<Verified> {
        let (root, payload_sha256) = self.open()?;
        let signers = match previous {
            Some(previous) => {
                ensure!(
                    root.publisher == previous.publisher && root.version == previous.version + 1,
                    "trust root version {} does not follow version {} for {}",
                    root.version,
                    previous.version,
                    previous.publisher
                );
                ensure!(
                    root.genesis_sha256 == Some(previous.genesis()?),
                    "trust root version {} names a different version 1 than version {} for {}",
                    root.version,
                    previous.version,
                    previous.publisher
                );
                let mut authorized = root.root_keys();
                authorized.extend(previous.root_keys());
                self.0.verify(&TRUST_ROOT, &authorized)?
            }
            None if root.version == 1 => self.0.verify(&TRUST_ROOT, &root.root_keys())?,
            None => self.0.verify_known(&TRUST_ROOT, &root.root_keys())?,
        };
        check_root_signatures(&root, previous, &signers)?;
        Ok(Verified {
            signed: self.clone(),
            genesis: root.genesis()?,
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

#[derive(Clone)]
struct Verified {
    signed: SignedTrustRoot,
    root: TrustRoot,
    payload_sha256: String,
    genesis: String,
}

/// Consecutive trust root versions: the newest versions a catalog carries,
/// a client's pin, or a repository's whole history.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct TrustChain(Vec<SignedTrustRoot>);

impl TrustChain {
    pub(crate) fn versions(&self) -> &[SignedTrustRoot] {
        &self.0
    }

    /// The newest `count` versions.
    pub(crate) fn newest(&self, count: usize) -> Self {
        Self(self.0[self.0.len().saturating_sub(count)..].to_vec())
    }

    /// Verify each version as the successor of the one before, and the first
    /// on its own root keys, for this publisher.
    pub(crate) fn verify(&self, publisher: &str) -> Result<VerifiedChain> {
        ensure!(
            !self.0.is_empty(),
            "catalog for {publisher} carries no trust root"
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

    /// Require `next`, a later catalog's history, to agree with this one and
    /// to end at the same version or a later one.
    pub(crate) fn check_successor(&self, next: &Self) -> Result<()> {
        let Some(first) = self.0.first() else {
            return Ok(());
        };
        let publisher = first.open()?.0.publisher;
        (|| {
            let (previous, next) = (self.verify(&publisher)?, next.verify(&publisher)?);
            ensure!(
                next.current().version >= previous.current().version,
                "trust root version {} precedes version {}",
                next.current().version,
                previous.current().version
            );
            previous.join(&next).map(drop)
        })()
        .context("release drops or changes the publisher trust root history")
    }
}

/// A trust history whose signatures all verify.
pub(crate) struct VerifiedChain(Vec<Verified>);

impl VerifiedChain {
    /// The newest version: catalogs carrying this history are signed under it.
    pub(crate) fn current(&self) -> &TrustRoot {
        &self.0.last().expect("verified chains are not empty").root
    }

    fn first(&self) -> u64 {
        self.0[0].root.version
    }

    /// The SHA-256 of the version 1 payload: a stable publisher identity.
    pub(crate) fn genesis(&self) -> &str {
        &self.0[0].genesis
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

    fn chain(&self) -> TrustChain {
        TrustChain(
            self.0
                .iter()
                .map(|version| version.signed.clone())
                .collect(),
        )
    }

    /// Join two histories of one publisher. They must agree on every version
    /// both hold, and the later one must start no later than the version
    /// after the earlier one ends.
    fn join(&self, other: &Self) -> Result<Self> {
        let (low, high) = if self.first() <= other.first() {
            (self, other)
        } else {
            (other, self)
        };
        let publisher = &low.current().publisher;
        ensure!(
            low.genesis() == high.genesis(),
            "trust root histories for {publisher} start from different version 1 trust roots"
        );
        let end = low.current().version;
        ensure!(
            high.first() <= end + 1,
            "trust root history for {publisher} jumps from version {end} to version {}; pin a newer trust root",
            high.first()
        );
        let mut joined = low.0.clone();
        for version in &high.0 {
            let number = version.root.version;
            if number <= end {
                let known = &low.0[usize::try_from(number - low.first())?];
                ensure!(
                    known.payload_sha256 == version.payload_sha256,
                    "trust root histories for {publisher} differ at version {number}"
                );
            } else {
                if number == end + 1 {
                    version
                        .signed
                        .verify(Some(&joined.last().expect("not empty").root))?;
                }
                joined.push(version.clone());
            }
        }
        Ok(Self(joined))
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
    /// A new current catalog: it must be signed under the newest pinned
    /// version or a later one, which must not have expired.
    Current,
}

/// Decode a pinned history in its only accepted byte form: a single
/// version is its trust root file, and a longer history is the canonical
/// JSON array of its versions, oldest first.
fn parse_pin(bytes: &[u8]) -> Result<VerifiedChain> {
    (|| {
        let chain = if bytes.first() == Some(&b'[') {
            let chain: TrustChain = serde_json::from_slice(bytes)?;
            ensure!(
                (2..=PIN_VERSIONS).contains(&chain.0.len()),
                "pinned trust root history holds 2 to {PIN_VERSIONS} versions"
            );
            chain
        } else {
            TrustChain(vec![SignedTrustRoot::decode(bytes)?])
        };
        ensure!(
            encode_pin(&chain)? == bytes,
            "pinned trust root history is not canonical"
        );
        let publisher = chain.0[0].open()?.0.publisher;
        chain.verify(&publisher)
    })()
    .context("reading the pinned trust root")
}

fn encode_pin(chain: &TrustChain) -> Result<Vec<u8>> {
    match chain.0.as_slice() {
        [single] => single.encode(),
        _ => Ok(serde_json::to_vec(chain)?),
    }
}

/// Verify a catalog's trust history against a client's pinned history.
pub(crate) fn verify(
    publisher: &str,
    chain: &TrustChain,
    pinned: &[u8],
    freshness: Freshness,
) -> Result<VerifiedChain> {
    let pin = parse_pin(pinned)?;
    ensure!(
        pin.current().publisher == publisher,
        "pinned trust root belongs to publisher {}, not {publisher}",
        pin.current().publisher
    );
    ensure!(
        chain.0.len() <= CATALOG_VERSIONS,
        "catalog for {publisher} carries more than {CATALOG_VERSIONS} trust root versions"
    );
    let verified = chain.verify(publisher)?;
    let signed_under = verified.current().version;
    let oldest = match freshness {
        Freshness::Pinned => pin.first(),
        Freshness::Current => pin.current().version,
    };
    ensure!(
        signed_under >= oldest,
        "catalog for {publisher} is signed under trust root version {signed_under}, older than the pinned version {oldest}"
    );
    pin.join(&verified).with_context(|| {
        format!("catalog trust history does not continue the pinned trust root for {publisher}")
    })?;
    if freshness == Freshness::Current {
        verified.current().check_unexpired()?;
    }
    Ok(verified)
}

/// The pin after a client verified a catalog's history against it: the
/// pinned history extended through the catalog's newest version.
pub(crate) fn advance(pinned: &[u8], chain: &TrustChain) -> Result<Vec<u8>> {
    let pin = parse_pin(pinned)?;
    let verified = chain.verify(&pin.current().publisher)?;
    let mut joined = pin.join(&verified)?;
    let excess = joined.0.len().saturating_sub(PIN_VERSIONS);
    joined.0.drain(..excess);
    encode_pin(&joined.chain())
}

/// Require `next` to be `previous` or a pin advanced from it: the same
/// publisher's history, agreeing on every version both hold and ending at
/// the same version or a later one.
pub(crate) fn check_advanced(previous: &[u8], next: &[u8]) -> Result<()> {
    if previous == next {
        return Ok(());
    }
    let (previous, next) = (parse_pin(previous)?, parse_pin(next)?);
    ensure!(
        next.current().version > previous.current().version,
        "a pinned trust root can only advance to a later version"
    );
    previous.join(&next).map(drop)
}

/// Every key named by a pinned history.
pub(crate) fn pinned_keys(pinned: &[u8]) -> Result<BTreeSet<PublicKey>> {
    Ok(parse_pin(pinned)?.keys())
}

/// The newest version of a pinned history.
pub(crate) fn pinned_root(pinned: &[u8]) -> Result<TrustRoot> {
    Ok(parse_pin(pinned)?.current().clone())
}

/// The SHA-256 of the version 1 payload of a pinned history's publisher.
pub(crate) fn pinned_genesis(pinned: &[u8]) -> Result<String> {
    Ok(parse_pin(pinned)?.genesis().to_owned())
}

/// Decode a standalone trust root file, such as `trust/2.json`, checked
/// against its own root keys.
pub(crate) fn decode_file(bytes: &[u8]) -> Result<TrustRoot> {
    Ok(SignedTrustRoot::decode(bytes)?.verify(None)?.root)
}

/// A pinned history of one standalone trust root file: the file itself.
pub(crate) fn pin_file(bytes: &[u8]) -> Result<Vec<u8>> {
    SignedTrustRoot::decode(bytes)?.verify(None)?;
    Ok(bytes.to_vec())
}

/// Read a trust root file as a pinned history of that one version.
pub(crate) fn read_pinned(path: &Path) -> Result<Vec<u8>> {
    (|| pin_file(&read_limited(File::open(path)?, TRUST_ROOT_LIMIT)?))()
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

    /// The whole history as a pin: the publisher's own pin for its catalogs.
    pub(crate) fn pin(&self) -> Result<Vec<u8>> {
        encode_pin(&self.chain)
    }

    /// The versions a new catalog carries.
    pub(crate) fn catalog_chain(&self) -> TrustChain {
        self.chain.newest(CATALOG_VERSIONS)
    }
}

/// Membership changes for the next trust root version. Empty key lists and
/// absent thresholds keep the previous version's values.
#[derive(Default)]
pub(crate) struct TrustDraft<'a> {
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
            genesis_sha256: None,
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
                genesis_sha256: Some(previous.genesis()?),
                expires: new_expiry(expires)?,
                root_keys: root_keys.unwrap_or_else(|| previous.root_keys.clone()),
                root_threshold: root_threshold.unwrap_or(previous.root_threshold),
                catalog_keys: catalog_keys.unwrap_or_else(|| previous.catalog_keys.clone()),
                catalog_threshold: catalog_threshold.unwrap_or(previous.catalog_threshold),
            }
        }
    };
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
        pinned_root(&read_pinned(path)?)?
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
    use aws_lc_rs::signature::Ed25519KeyPair;

    /// Keys reloadable from their PKCS#8 documents, with public key files.
    pub(crate) struct Keys {
        dir: tempfile::TempDir,
        documents: Vec<aws_lc_rs::pkcs8::Document>,
    }

    impl Keys {
        pub(crate) fn new(count: usize) -> Result<Self> {
            let dir = tempfile::tempdir()?;
            let mut documents = Vec::new();
            for index in 0..count {
                let document =
                    Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
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
            genesis_sha256: None,
            expires: "2099-01-01T00:00:00Z".to_owned(),
            root_keys: root.iter().map(|index| keys.key(*index)).collect(),
            root_threshold,
            catalog_keys: BTreeSet::from([keys.key(9)]),
            catalog_threshold: 1,
        }
    }

    /// The version after `previous` with these root keys.
    fn next(previous: &TrustRoot, keys: &Keys, root: &[usize], root_threshold: usize) -> TrustRoot {
        TrustRoot {
            version: previous.version + 1,
            genesis_sha256: Some(previous.genesis().unwrap()),
            ..self::root(keys, 1, root, root_threshold)
        }
    }

    /// Versions 1 through `count`, each signed by key 0 alone.
    fn history(keys: &Keys, count: u64) -> Vec<SignedTrustRoot> {
        let mut root = self::root(keys, 1, &[0], 1);
        let mut versions = vec![sign(&root, &keys.signers(&[0]))];
        for _ in 1..count {
            root = next(&root, keys, &[0], 1);
            versions.push(sign(&root, &keys.signers(&[0])));
        }
        versions
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
        let second = next(&first, &keys, &[0, 3, 4], 2);
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
        let mut skipped = next(&first, &keys, &[0, 1, 2], 2);
        skipped.version = 3;
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
        // Signatures may differ, but a different version 2 is a fork.
        let resigned = chain(sign(&second, &keys.signers(&[0, 1, 4])));
        valid.check_successor(&resigned)?;
        let fork = chain(sign(
            &next(&first, &keys, &[0, 4, 5], 2),
            &keys.signers(&[0, 1, 4]),
        ));
        assert!(valid.check_successor(&fork).is_err());
        Ok(())
    }

    #[test]
    fn clients_pin_any_version_and_reject_older_or_different_histories() -> Result<()> {
        let keys = Keys::new(10)?;
        let genesis = root(&keys, 1, &[0], 1);
        let first = sign(&genesis, &keys.signers(&[0]));
        let second = sign(&next(&genesis, &keys, &[1], 1), &keys.signers(&[0, 1]));
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
        let unsigned = sign(&next(&genesis, &keys, &[1], 1), &keys.signers(&[0]));
        assert!(parse_pin(&unsigned.encode()?).is_err());
        // The pinned file must be canonical.
        let mut spaced = first.encode()?;
        spaced.push(b'\n');
        assert!(parse_pin(&spaced).is_err());
        Ok(())
    }

    #[test]
    fn expiry_rejects_new_current_catalogs_but_not_pinned_history() -> Result<()> {
        let keys = Keys::new(10)?;
        let mut genesis = root(&keys, 1, &[0], 1);
        genesis.expires = "2020-01-01T00:00:00Z".to_owned();
        let expired = sign(&genesis, &keys.signers(&[0]));
        let chain = TrustChain(vec![expired.clone()]);
        let pin = expired.encode()?;
        verify("canopa", &chain, &pin, Freshness::Pinned)?;
        let error = verify("canopa", &chain, &pin, Freshness::Current)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("expired at 2020-01-01T00:00:00Z"));
        // A renewed version makes the same pin current again.
        let renewed = sign(&next(&genesis, &keys, &[0], 1), &keys.signers(&[0]));
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

    #[test]
    fn catalogs_carry_recent_versions_that_continue_the_pin() -> Result<()> {
        let keys = Keys::new(10)?;
        let versions = history(&keys, 70);
        let pin = |range: std::ops::RangeInclusive<usize>| {
            encode_pin(&TrustChain(versions[range].to_vec())).unwrap()
        };
        let window = |range: std::ops::RangeInclusive<usize>| TrustChain(versions[range].to_vec());
        // Histories have no version limit, but a catalog carries at most
        // CATALOG_VERSIONS of them.
        let store = TrustChain(versions.clone());
        let carried = store.newest(CATALOG_VERSIONS);
        assert_eq!(carried.versions().len(), CATALOG_VERSIONS);
        verify("canopa", &carried, &store_pin(&store)?, Freshness::Current)?;
        let error = verify("canopa", &store, &pin(0..=0), Freshness::Pinned)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("more than 16 trust root versions"));
        // A pin inside the window, or just before it, follows it.
        verify(
            "canopa",
            &window(54..=69),
            &pin(60..=60),
            Freshness::Current,
        )?;
        verify(
            "canopa",
            &window(54..=69),
            &pin(40..=53),
            Freshness::Current,
        )?;
        // A pin further behind cannot.
        let error = verify("canopa", &window(54..=69), &pin(0..=0), Freshness::Current)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("jumps from version 1 to version 55"));
        // A current catalog may not be signed under an older version than
        // the pin's newest, but a historical one may.
        let error = verify("canopa", &window(0..=9), &pin(0..=19), Freshness::Current)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("older than the pinned version 20"));
        verify("canopa", &window(0..=9), &pin(0..=19), Freshness::Pinned)?;
        let error = verify("canopa", &window(0..=9), &pin(15..=19), Freshness::Pinned)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("older than the pinned version 16"));
        // The catalog must agree with every pinned version it holds.
        let fork = sign(
            &next(
                &TrustRoot::decode(&versions[9].0.validate(&TRUST_ROOT)?)?,
                &keys,
                &[0, 1],
                1,
            ),
            &keys.signers(&[0, 1]),
        );
        let forked = TrustChain([&versions[5..=9], &[fork]].concat());
        assert!(verify("canopa", &forked, &pin(0..=10), Freshness::Pinned).is_err());
        Ok(())
    }

    fn store_pin(chain: &TrustChain) -> Result<Vec<u8>> {
        encode_pin(chain)
    }

    #[test]
    fn pins_advance_along_their_history() -> Result<()> {
        let keys = Keys::new(10)?;
        let versions = history(&keys, 40);
        let first = versions[0].encode()?;
        let advanced = advance(&first, &TrustChain(versions[..=15].to_vec()))?;
        assert_eq!(pinned_root(&advanced)?.version, 16);
        check_advanced(&first, &advanced)?;
        assert!(check_advanced(&advanced, &first).is_err());
        let again = advance(&advanced, &TrustChain(versions[16..=31].to_vec()))?;
        assert_eq!(pinned_root(&again)?.version, 32);
        assert_eq!(pinned_genesis(&again)?, pinned_genesis(&first)?);
        // Advancing to an older catalog keeps the newest version.
        assert_eq!(
            advance(&again, &TrustChain(versions[..=3].to_vec()))?,
            again
        );
        // A pin cannot jump a gap in the history.
        assert!(advance(&first, &TrustChain(versions[24..=39].to_vec())).is_err());
        // Another publisher's history is not an advance.
        let other = sign(&root(&keys, 1, &[1], 1), &keys.signers(&[1]));
        assert!(check_advanced(&first, &other.encode()?).is_err());
        Ok(())
    }

    #[test]
    fn later_versions_name_their_version_one() -> Result<()> {
        let keys = Keys::new(10)?;
        let genesis = root(&keys, 1, &[0], 1);
        let second = next(&genesis, &keys, &[0], 1);
        let mut wrong = second.clone();
        wrong.genesis_sha256 = Some(digest(b"another publisher"));
        let chain = |root: &TrustRoot| {
            TrustChain(vec![
                sign(&genesis, &keys.signers(&[0])),
                sign(root, &keys.signers(&[0])),
            ])
        };
        chain(&second).verify("canopa")?;
        let error = chain(&wrong).verify("canopa").err().unwrap();
        assert!(format!("{error:#}").contains("names a different version 1"));
        let mut unnamed = second.clone();
        unnamed.genesis_sha256 = None;
        assert!(unnamed.encode().is_err());
        let mut named = genesis.clone();
        named.genesis_sha256 = Some(digest(b"x"));
        assert!(named.encode().is_err());
        Ok(())
    }
}
