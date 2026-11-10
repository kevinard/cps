//! Signing keys, threshold-signed envelopes, and the publisher trust root.
//!
//! Catalog roots and trust roots share one envelope: canonical JSON holding a
//! base64 payload and signatures sorted by key ID. Each kind of document signs
//! under its own domain prefix, so a signature for one kind never verifies as
//! another.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use crate::model::{digest, validate_digest};

pub(crate) mod signer;
pub(crate) mod trust;
use signer::{CommandSigner, KeySpec, SignRequest};

pub(crate) type PublicKey = [u8; 32];

/// What a signature is for. External signers receive it as `CPS_SIGNER_CONTEXT`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Purpose {
    Catalog,
    TrustRoot,
    Attestation,
}

impl Purpose {
    fn context(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::TrustRoot => "trust-root",
            Self::Attestation => "attestation",
        }
    }
}

/// An Ed25519 signing key, held in a local file or by an external signer.
pub(crate) struct SigningKey {
    public: PublicKey,
    backend: Backend,
}

enum Backend {
    File(Ed25519KeyPair),
    Command(CommandSigner),
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("public", &hex::encode(self.public))
            .finish_non_exhaustive()
    }
}

impl From<Ed25519KeyPair> for SigningKey {
    fn from(key: Ed25519KeyPair) -> Self {
        let public = key
            .public_key()
            .as_ref()
            .try_into()
            .expect("Ed25519 public key length");
        Self {
            public,
            backend: Backend::File(key),
        }
    }
}

impl SigningKey {
    /// Load `PATH`, `file:PATH`, or `command:DESCRIPTOR`.
    pub(crate) fn open(spec: &Path) -> Result<Self> {
        match KeySpec::parse(spec) {
            KeySpec::File(path) => Ok(super::read_signing_key(&path)?.into()),
            KeySpec::Command(descriptor) => {
                let signer = CommandSigner::open(&descriptor)?;
                let public =
                    signer::ed25519_public_key(&signer.public_key()?).with_context(|| {
                        format!("reading the public key for {}", descriptor.display())
                    })?;
                Ok(Self {
                    public,
                    backend: Backend::Command(signer),
                })
            }
        }
    }

    pub(crate) fn public_key(&self) -> &PublicKey {
        &self.public
    }

    /// The key ID: the SHA-256 of the raw public key.
    pub(crate) fn key_id(&self) -> String {
        digest(&self.public)
    }

    /// Sign with the key. An external signature is checked before it is returned.
    pub(crate) fn sign(&self, purpose: Purpose, message: &[u8]) -> Result<[u8; 64]> {
        match &self.backend {
            Backend::File(key) => Ok(key
                .sign(message)
                .as_ref()
                .try_into()
                .expect("Ed25519 signature length")),
            Backend::Command(signer) => {
                let key_id = self.key_id();
                let signature = signer.run(
                    &SignRequest {
                        context: purpose.context(),
                        key_id: &key_id,
                        algorithm: "ed25519",
                    },
                    message,
                )?;
                let signature: [u8; 64] = signature
                    .as_slice()
                    .try_into()
                    .context("external signer returned an invalid Ed25519 signature length")?;
                UnparsedPublicKey::new(&ED25519, self.public)
                    .verify(message, &signature)
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "external signer returned a signature that does not verify with its declared public key"
                        )
                    })?;
                Ok(signature)
            }
        }
    }
}

/// One or more signing key specifications: `PATH`, `file:PATH`, or
/// `command:DESCRIPTOR`.
pub trait KeySpecs {
    fn key_specs(&self) -> Vec<&Path>;
}

impl KeySpecs for Path {
    fn key_specs(&self) -> Vec<&Path> {
        vec![self]
    }
}

impl KeySpecs for PathBuf {
    fn key_specs(&self) -> Vec<&Path> {
        vec![self]
    }
}

impl KeySpecs for [PathBuf] {
    fn key_specs(&self) -> Vec<&Path> {
        self.iter().map(PathBuf::as_path).collect()
    }
}

impl KeySpecs for Vec<PathBuf> {
    fn key_specs(&self) -> Vec<&Path> {
        self.as_slice().key_specs()
    }
}

impl<const N: usize> KeySpecs for [PathBuf; N] {
    fn key_specs(&self) -> Vec<&Path> {
        self.as_slice().key_specs()
    }
}

/// Distinct signing keys that sign one document together.
#[derive(Debug)]
pub(crate) struct Signers(Vec<SigningKey>);

impl Signers {
    pub(crate) fn open(specs: &(impl KeySpecs + ?Sized)) -> Result<Self> {
        let specs = specs.key_specs();
        ensure!(!specs.is_empty(), "at least one signing key is required");
        ensure!(
            specs.len() <= MAX_SIGNATURES,
            "at most {MAX_SIGNATURES} signing keys are supported"
        );
        let mut keys: Vec<SigningKey> = Vec::with_capacity(specs.len());
        for spec in specs {
            let key = SigningKey::open(spec)?;
            ensure!(
                keys.iter().all(|other| other.public != key.public),
                "signing key {} is given more than once",
                spec.display()
            );
            keys.push(key);
        }
        Ok(Self(keys))
    }

    pub(crate) fn public_keys(&self) -> BTreeSet<PublicKey> {
        self.0.iter().map(|key| key.public).collect()
    }

    /// Sign a payload with every key.
    pub(crate) fn sign(&self, domain: &Domain, payload: &[u8]) -> Result<Vec<EnvelopeSignature>> {
        let message = domain.message(payload);
        self.0
            .iter()
            .map(|key| {
                Ok(EnvelopeSignature::new(
                    key.public_key(),
                    &key.sign(domain.purpose, &message)?,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
impl From<Vec<SigningKey>> for Signers {
    fn from(keys: Vec<SigningKey>) -> Self {
        Self(keys)
    }
}

/// The most signatures a catalog envelope may carry, and the most keys
/// in one trust root role.
pub(crate) const MAX_SIGNATURES: usize = 16;

/// What an envelope's signatures cover, and how many it may carry.
pub(crate) struct Domain {
    prefix: &'static [u8],
    pub(crate) what: &'static str,
    purpose: Purpose,
    max_signatures: usize,
}

pub(crate) const CATALOG: Domain = Domain {
    prefix: b"cps signed catalog root v1\n",
    what: "catalog",
    purpose: Purpose::Catalog,
    max_signatures: MAX_SIGNATURES,
};

/// A trust root rotation may carry the old and the new root keys' signatures.
pub(crate) const TRUST_ROOT: Domain = Domain {
    prefix: b"cps publisher trust root v1\n",
    what: "trust root",
    purpose: Purpose::TrustRoot,
    max_signatures: 2 * MAX_SIGNATURES,
};

impl Domain {
    pub(crate) fn message(&self, payload: &[u8]) -> Vec<u8> {
        let mut message = self.prefix.to_vec();
        message.extend_from_slice(payload);
        message
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    signed: String,
    signatures: Vec<EnvelopeSignature>,
}

/// One signature: the SHA-256 of the raw Ed25519 public key and the
/// lowercase hexadecimal signature.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvelopeSignature {
    keyid: String,
    sig: String,
}

impl EnvelopeSignature {
    fn new(key: &PublicKey, signature: &[u8; 64]) -> Self {
        Self {
            keyid: digest(key),
            sig: hex::encode(signature),
        }
    }

    pub(crate) fn key_id(&self) -> &str {
        &self.keyid
    }

    fn validate(&self, domain: &Domain) -> Result<()> {
        validate_digest(&self.keyid)
            .with_context(|| format!("invalid {} signature key ID", domain.what))?;
        ensure!(
            self.sig.len() == 128
                && self
                    .sig
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "invalid {} signature encoding",
            domain.what
        );
        Ok(())
    }

    fn bytes(&self) -> Result<[u8; 64]> {
        let mut signature = [0; 64];
        hex::decode_to_slice(&self.sig, &mut signature)?;
        Ok(signature)
    }

    /// Decode a detached signature file in its one accepted byte form.
    pub(crate) fn decode(domain: &Domain, bytes: &[u8]) -> Result<Self> {
        let signature: Self = serde_json::from_slice(bytes)
            .with_context(|| format!("invalid detached {} signature", domain.what))?;
        ensure!(
            serde_json::to_vec(&signature)? == bytes,
            "detached {} signature is not canonical",
            domain.what
        );
        signature.validate(domain)?;
        Ok(signature)
    }
}

impl Envelope {
    /// Assemble an envelope from a payload and its signatures.
    pub(crate) fn new(
        domain: &Domain,
        payload: &[u8],
        mut signatures: Vec<EnvelopeSignature>,
    ) -> Result<Self> {
        signatures.sort_by(|a, b| a.keyid.cmp(&b.keyid));
        let envelope = Self {
            signed: STANDARD.encode(payload),
            signatures,
        };
        envelope.validate(domain)?;
        Ok(envelope)
    }

    /// Decode an envelope in its only accepted byte form.
    pub(crate) fn decode(domain: &Domain, bytes: &[u8]) -> Result<Self> {
        let envelope: Self = serde_json::from_slice(bytes)
            .with_context(|| format!("invalid {} envelope", domain.what))?;
        // One document has one byte encoding, so its SHA-256 identity cannot
        // be changed without the signers: no whitespace, escapes, or hex case.
        ensure!(
            serde_json::to_vec(&envelope)? == bytes,
            "{} envelope is not canonical",
            domain.what
        );
        envelope.validate(domain)?;
        Ok(envelope)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Check the encoding and return the signed payload bytes.
    pub(crate) fn validate(&self, domain: &Domain) -> Result<Vec<u8>> {
        let payload = STANDARD
            .decode(&self.signed)
            .with_context(|| format!("invalid {} envelope payload", domain.what))?;
        ensure!(
            STANDARD.encode(&payload) == self.signed,
            "{} envelope payload is not canonical",
            domain.what
        );
        ensure!(
            (1..=domain.max_signatures).contains(&self.signatures.len()),
            "{} envelope requires 1 to {} signatures",
            domain.what,
            domain.max_signatures
        );
        for pair in self.signatures.windows(2) {
            ensure!(
                pair[0].keyid < pair[1].keyid,
                "{} envelope signatures must be sorted by unique key ID",
                domain.what
            );
        }
        for signature in &self.signatures {
            signature.validate(domain)?;
        }
        Ok(payload)
    }

    pub(crate) fn signatures(&self) -> &[EnvelopeSignature] {
        &self.signatures
    }

    /// Verify every signature with an authorized key and return the signers.
    /// A signature by any other key, or one that fails, rejects the envelope.
    pub(crate) fn verify(
        &self,
        domain: &Domain,
        authorized: &BTreeSet<PublicKey>,
    ) -> Result<BTreeSet<PublicKey>> {
        let message = domain.message(&self.validate(domain)?);
        let by_id: BTreeMap<String, &PublicKey> =
            authorized.iter().map(|key| (digest(key), key)).collect();
        let mut signers = BTreeSet::new();
        for signature in &self.signatures {
            let key = by_id.get(&signature.keyid).with_context(|| {
                format!(
                    "{} is signed by unauthorized key {}",
                    domain.what, signature.keyid
                )
            })?;
            UnparsedPublicKey::new(&ED25519, key)
                .verify(&message, &signature.bytes()?)
                .map_err(|_| {
                    anyhow::anyhow!(
                        "invalid {} signature by key {}",
                        domain.what,
                        signature.keyid
                    )
                })?;
            signers.insert(**key);
        }
        Ok(signers)
    }
}

impl Envelope {
    /// Verify the signatures by authorized keys and ignore any other key ID.
    pub(crate) fn verify_known(
        &self,
        domain: &Domain,
        authorized: &BTreeSet<PublicKey>,
    ) -> Result<BTreeSet<PublicKey>> {
        let known: BTreeSet<String> = authorized.iter().map(|key| digest(key)).collect();
        let signatures: Vec<_> = self
            .signatures
            .iter()
            .filter(|signature| known.contains(&signature.keyid))
            .cloned()
            .collect();
        if signatures.is_empty() {
            return Ok(BTreeSet::new());
        }
        Self {
            signed: self.signed.clone(),
            signatures,
        }
        .verify(domain, authorized)
    }
}

/// Seal a payload with exactly the signers' signatures.
pub(crate) fn seal(domain: &Domain, payload: &[u8], signers: &Signers) -> Result<Envelope> {
    Envelope::new(domain, payload, signers.sign(domain, payload)?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::rand::SystemRandom;

    pub(crate) fn key() -> SigningKey {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("generate key");
        Ed25519KeyPair::from_pkcs8(document.as_ref())
            .expect("read generated key")
            .into()
    }

    #[test]
    fn external_signers_learn_what_they_sign() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if std::process::Command::new("openssl")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping: openssl is unavailable");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let private = dir.path().join("builder.der");
        std::fs::write(&private, document.as_ref())?;
        let local = SigningKey::from(Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap());
        std::fs::write(dir.path().join("builder.pub"), hex::encode(local.public))?;
        let script = dir.path().join("sign.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\n[ \"$CPS_SIGNER_CONTEXT\" = attestation ] && [ \"$CPS_SIGNER_KEY_ID\" = {} ] || exit 3\nmessage=$(mktemp)\ntrap 'rm -f \"$message\"' EXIT\ncat > \"$message\"\nopenssl pkeyutl -sign -rawin -inkey {} -keyform DER -in \"$message\"\n",
                local.key_id(),
                private.display()
            ),
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let descriptor = dir.path().join("builder.signer.kdl");
        std::fs::write(
            &descriptor,
            format!(
                "schema-version 1 kind=\"signer\"\ncommand \"{}\"\npublic-key \"builder.pub\"\ntimeout-seconds 5\n",
                script.display()
            ),
        )?;
        let external = SigningKey::open(Path::new(&format!("command:{}", descriptor.display())))?;
        assert_eq!(external.public_key(), local.public_key());
        assert_eq!(
            external.sign(Purpose::Attestation, b"statement")?,
            local.sign(Purpose::Attestation, b"statement")?
        );
        // The helper refuses other contexts, and the failure is reported.
        assert!(external.sign(Purpose::Catalog, b"statement").is_err());
        assert!(external.sign(Purpose::TrustRoot, b"statement").is_err());
        Ok(())
    }

    #[test]
    fn envelopes_have_one_byte_form_and_accept_only_authorized_signatures() -> Result<()> {
        let first = key();
        let second = key();
        let payload = br#"{"format":1}"#;
        let signers = Signers::from(vec![key(), key()]);
        let sealed = seal(&CATALOG, payload, &signers)?.encode()?;
        let envelope = Envelope::decode(&CATALOG, &sealed)?;
        assert_eq!(envelope.validate(&CATALOG)?, payload);
        assert_eq!(
            envelope.verify(&CATALOG, &signers.public_keys())?,
            signers.public_keys()
        );
        // A signature by a key outside the authorized set rejects the envelope.
        let partial = BTreeSet::from([*signers.0[0].public_key()]);
        assert!(envelope.verify(&CATALOG, &partial).is_err());
        // Another domain's signature does not verify.
        let other = Envelope::new(&TRUST_ROOT, payload, envelope.signatures.clone())?;
        assert!(other.verify(&TRUST_ROOT, &signers.public_keys()).is_err());

        let text = String::from_utf8(sealed.clone())?;
        let sig = &envelope.signatures[0].sig;
        for variant in [
            text.replacen(':', ": ", 1),
            format!("{text}\n"),
            text.replace(sig.as_str(), &sig.to_uppercase()),
        ] {
            assert!(Envelope::decode(&CATALOG, variant.as_bytes()).is_err());
        }
        let mut unsorted = envelope.clone();
        unsorted.signatures.reverse();
        assert!(Envelope::decode(&CATALOG, &unsorted.encode()?).is_err());
        let detached = serde_json::to_vec(&EnvelopeSignature::new(
            first.public_key(),
            &first.sign(Purpose::Catalog, b"x")?,
        ))?;
        EnvelopeSignature::decode(&CATALOG, &detached)?;
        assert!(Signers::open(&Vec::<PathBuf>::new()).is_err());
        drop(second);
        Ok(())
    }
}
