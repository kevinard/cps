use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use crate::{
    image::root::Root,
    kdl_format::{
        build::{BuildRecipe, PublisherPin, RECIPE_LIMIT, ToolchainPins, parse},
        manifest_text, parse_manifest,
    },
    model::{
        Action, FileDigest, Manifest, Version, digest, validate_digest, validate_name,
        validate_publisher,
    },
    repo::{MANIFEST_LIMIT, read_key, read_limited},
};

pub(crate) const ATTESTATION_LIMIT: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_BUILDERS: usize = 16;
const STATEMENT: &str = "https://in-toto.io/Statement/v1";
const LEGACY_PREDICATE: &str = "urn:canopa:cps:build-provenance:v1";
const PINNED_PREDICATE: &str = "urn:canopa:cps:build-provenance:v2";
const PUBLISHED_PREDICATE: &str = "urn:canopa:cps:build-provenance:v3";
const PUBLISHED_MANIFEST: &str = "published-manifest.kdl";
const MEDIA_TYPE: &str = "application/vnd.in-toto+json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildTrust {
    minimum_builders: usize,
    keys: BTreeSet<[u8; 32]>,
}

impl BuildTrust {
    pub fn load(minimum_builders: usize, paths: &[PathBuf]) -> Result<Self> {
        ensure!(
            paths.len() <= MAX_BUILDERS,
            "at most {MAX_BUILDERS} builder keys are supported"
        );
        let mut keys = BTreeSet::new();
        for path in paths {
            let key = read_key(path)?
                .try_into()
                .expect("read_key checks the key length");
            ensure!(keys.insert(key), "duplicate builder key {}", path.display());
        }
        let trust = Self {
            minimum_builders,
            keys,
        };
        trust.validate()?;
        Ok(trust)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.keys.len() <= MAX_BUILDERS
                && (1..=self.keys.len()).contains(&self.minimum_builders),
            "build trust requires between one and {MAX_BUILDERS} keys and a threshold within that range"
        );
        Ok(())
    }

    pub fn check_publisher_key(&self, key: &[u8]) -> Result<()> {
        ensure!(
            !self.keys.iter().any(|builder| builder.as_slice() == key),
            "builder and publisher must use distinct keys"
        );
        Ok(())
    }

    pub fn verify(&self, bytes: &[u8], manifest: &Manifest, published: FileDigest) -> Result<()> {
        let (payload, signatures) = decode_envelope(bytes)?;
        let message = pae(&payload);
        let mut valid = 0;
        for key in &self.keys {
            let key = UnparsedPublicKey::new(&ED25519, key);
            if signatures
                .iter()
                .any(|sig| key.verify(&message, sig).is_ok())
            {
                valid += 1;
                if valid == self.minimum_builders {
                    break;
                }
            }
        }
        ensure!(
            valid >= self.minimum_builders,
            "build attestation requires {} trusted builder signatures, found {valid}",
            self.minimum_builders
        );
        let statement = parse_statement(&payload)?;
        ensure!(
            statement.predicate_type == PUBLISHED_PREDICATE,
            "build trust requires version 3 build evidence"
        );
        ensure!(
            statement.predicate.package == manifest.id() && statement.predicate.identical_runs == 2,
            "build attestation does not match the package or the reproducibility requirement"
        );
        manifest.check_architecture(&statement.predicate.architecture)?;
        validate_digest(&statement.predicate.recipe_sha256)?;
        let mut subjects = subjects(statement.subject)?;
        ensure!(
            subjects.remove(PUBLISHED_MANIFEST) == Some(published),
            "build attestation does not cover the exact published manifest"
        );
        ensure!(
            subjects.remove("manifest.kdl").is_some()
                && subjects.len() == manifest.payload_paths().len(),
            "build attestation does not cover the declared package outputs"
        );
        for action in &manifest.action {
            if let Action::File {
                path,
                sha256: Some(hash),
                size: Some(size),
                ..
            } = action
            {
                ensure!(
                    subjects
                        .get(&format!("proto/{path}"))
                        .is_some_and(|file| { &file.sha256 == hash && file.size == *size }),
                    "build attestation does not cover published payload {path}"
                );
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    payload_type: String,
    payload: String,
    signatures: Vec<Signature>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signature {
    #[serde(default)]
    keyid: String,
    sig: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Statement {
    #[serde(rename = "_type")]
    statement_type: String,
    subject: Vec<Subject>,
    predicate_type: String,
    predicate: Predicate,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Subject {
    name: String,
    digest: Sha256,
    annotations: Size,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sha256 {
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Size {
    #[serde(rename = "cps:size")]
    bytes: u64,
}

impl Subject {
    fn new(name: String, file: &FileDigest) -> Self {
        Self {
            name,
            digest: Sha256 {
                sha256: file.sha256.clone(),
            },
            annotations: Size { bytes: file.size },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Predicate {
    package: String,
    recipe_sha256: String,
    architecture: String,
    source_date_epoch: u32,
    sources: BTreeMap<String, FileDigest>,
    toolchain: Toolchain,
    identical_runs: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Toolchain {
    Legacy(LegacyToolchain),
    Pinned(PinnedToolchain),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyToolchain {
    pub publisher: String,
    pub key_sha256: String,
    pub catalog_sha256: String,
    pub package_manifests: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedToolchain {
    pub publishers: Vec<PublisherPin>,
    pub package_manifests: BTreeMap<String, PinnedPackage>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedPackage {
    pub publisher: String,
    pub version: Version,
    pub sha256: String,
}

impl Toolchain {
    fn predicate_type(&self) -> &'static str {
        match self {
            Self::Legacy(_) => LEGACY_PREDICATE,
            Self::Pinned(_) => PINNED_PREDICATE,
        }
    }

    fn verify(&self, recipe: &BuildRecipe) -> Result<()> {
        match (self, &recipe.toolchain) {
            (Self::Legacy(chain), ToolchainPins::Single(catalog)) => {
                ensure!(
                    &chain.catalog_sha256 == catalog,
                    "build attestation does not match the pinned toolchain catalog"
                );
                validate_publisher(&chain.publisher)?;
                validate_digest(&chain.key_sha256)?;
                for (name, hash) in &chain.package_manifests {
                    validate_name(name)?;
                    validate_digest(hash)?;
                }
                ensure!(
                    recipe
                        .packages
                        .iter()
                        .all(|name| chain.package_manifests.contains_key(name)),
                    "build attestation omits a requested toolchain package"
                );
            }
            (Self::Pinned(chain), ToolchainPins::Publishers(pins)) => {
                ensure!(
                    &chain.publishers == pins,
                    "build attestation does not match the pinned toolchain publishers"
                );
                for (name, package) in &chain.package_manifests {
                    validate_name(name)?;
                    validate_digest(&package.sha256)?;
                    ensure!(
                        pins.iter().any(|pin| pin.name == package.publisher),
                        "toolchain package {name} claims an unpinned publisher"
                    );
                }
                for (name, spec) in recipe.requested()? {
                    let package = chain.package_manifests.get(name).with_context(|| {
                        format!("build attestation omits toolchain package {name}")
                    })?;
                    ensure!(
                        spec.publisher
                            .is_none_or(|publisher| package.publisher == publisher)
                            && spec
                                .version
                                .is_none_or(|version| package.version == version),
                        "build attestation claims a different selection for toolchain package {name}"
                    );
                }
            }
            _ => anyhow::bail!(
                "build attestation uses a different kind of toolchain pin than the recipe"
            ),
        }
        Ok(())
    }
}

pub(crate) struct VerifiedBuild {
    payload: Vec<u8>,
    signatures: BTreeMap<String, Vec<u8>>,
    pub artifacts: BTreeMap<String, FileDigest>,
    pub recipe_sha256: String,
    pub published_manifest: Option<FileDigest>,
}

impl VerifiedBuild {
    pub fn add_witness(&mut self, bytes: &[u8], key: &[u8]) -> Result<()> {
        let builder = digest(key);
        ensure!(
            !self.signatures.contains_key(&builder),
            "duplicate build signer"
        );
        let (payload, signature) = verify_envelope(bytes, key)?;
        ensure!(
            payload == self.payload,
            "witness build statement differs from the primary build"
        );
        self.signatures.insert(builder, signature);
        Ok(())
    }

    pub fn builders(&self) -> impl ExactSizeIterator<Item = &str> {
        self.signatures.keys().map(String::as_str)
    }

    pub fn envelope(&self) -> Result<Vec<u8>> {
        envelope(
            &self.payload,
            self.signatures
                .iter()
                .map(|(key, sig)| (key.as_str(), sig.as_slice())),
        )
    }
}

pub(crate) fn sign(
    key: &Ed25519KeyPair,
    manifest: &Manifest,
    manifest_bytes: &[u8],
    recipe: &BuildRecipe,
    recipe_bytes: &[u8],
    toolchain: Toolchain,
    artifacts: &BTreeMap<String, FileDigest>,
) -> Result<Vec<u8>> {
    let mut subject = vec![
        Subject::new("manifest.kdl".to_owned(), &file_digest(manifest_bytes)),
        Subject::new(
            PUBLISHED_MANIFEST.to_owned(),
            &published_manifest_digest(manifest, artifacts)?,
        ),
    ];
    subject.extend(
        artifacts
            .iter()
            .map(|(path, file)| Subject::new(format!("proto/{path}"), file)),
    );
    let statement = Statement {
        statement_type: STATEMENT.to_owned(),
        subject,
        predicate_type: PUBLISHED_PREDICATE.to_owned(),
        predicate: Predicate {
            package: manifest.id(),
            recipe_sha256: digest(recipe_bytes),
            architecture: recipe.arch.clone(),
            source_date_epoch: recipe.epoch,
            sources: recipe.source_digests(),
            toolchain,
            identical_runs: 2,
        },
    };
    let payload = serde_json::to_vec(&statement)?;
    let signature = key.sign(&pae(&payload));
    let builder = digest(key.public_key().as_ref());
    envelope(&payload, [(builder.as_str(), signature.as_ref())])
}

fn envelope<'a>(
    payload: &[u8],
    signatures: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&Envelope {
        payload_type: MEDIA_TYPE.to_owned(),
        payload: STANDARD.encode(payload),
        signatures: signatures
            .into_iter()
            .map(|(key, signature)| Signature {
                keyid: key.to_owned(),
                sig: STANDARD.encode(signature),
            })
            .collect(),
    })?;
    ensure!(
        bytes.len() as u64 <= ATTESTATION_LIMIT,
        "build attestation is too large"
    );
    Ok(bytes)
}

fn pae(payload: &[u8]) -> Vec<u8> {
    let mut prefix = format!(
        "DSSEv1 {} {MEDIA_TYPE} {} ",
        MEDIA_TYPE.len(),
        payload.len()
    )
    .into_bytes();
    prefix.extend_from_slice(payload);
    prefix
}

fn decode(encoded: &str) -> Result<Vec<u8>> {
    for encoding in [STANDARD, URL_SAFE, STANDARD_NO_PAD, URL_SAFE_NO_PAD] {
        if let Ok(bytes) = encoding.decode(encoded) {
            return Ok(bytes);
        }
    }
    anyhow::bail!("invalid DSSE base64")
}

fn file_digest(bytes: &[u8]) -> FileDigest {
    FileDigest {
        sha256: digest(bytes),
        size: bytes.len() as u64,
    }
}

fn published_manifest_digest(
    manifest: &Manifest,
    artifacts: &BTreeMap<String, FileDigest>,
) -> Result<FileDigest> {
    let published = manifest.clone().with_payloads(artifacts)?;
    let bytes = manifest_text(&published);
    ensure!(
        bytes.len() as u64 <= MANIFEST_LIMIT,
        "manifest exceeds {MANIFEST_LIMIT} bytes"
    );
    Ok(file_digest(bytes.as_bytes()))
}

fn decode_envelope(bytes: &[u8]) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let signed: Envelope = serde_json::from_slice(bytes)?;
    ensure!(
        signed.payload_type == MEDIA_TYPE,
        "unexpected attestation payload type"
    );
    ensure!(
        signed.signatures.len() <= MAX_BUILDERS,
        "too many attestation signatures"
    );
    let payload = decode(&signed.payload)?;
    let signatures = signed
        .signatures
        .into_iter()
        // An Ed25519 signature is 64 bytes, at most 88 bytes in base64.
        .filter(|entry| entry.sig.len() <= 88)
        .filter_map(|entry| decode(&entry.sig).ok())
        .collect();
    Ok((payload, signatures))
}

fn verify_envelope(bytes: &[u8], builder_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let (payload, signatures) = decode_envelope(bytes)?;
    let message = pae(&payload);
    let key = UnparsedPublicKey::new(&ED25519, builder_key);
    let signature = signatures
        .into_iter()
        .find(|signature| key.verify(&message, signature).is_ok())
        .context("invalid builder attestation signature")?;
    Ok((payload, signature))
}

pub(crate) fn verify_bundle(
    bundle: &Root,
    builder_key: &[u8],
    reviewed_recipe: &str,
) -> Result<(Manifest, VerifiedBuild)> {
    validate_digest(reviewed_recipe).context("invalid reviewed recipe digest")?;
    let bytes = read_limited(bundle.file("attestation.json")?, ATTESTATION_LIMIT)?;
    let (payload, signature) = verify_envelope(&bytes, builder_key)?;
    let statement = parse_statement(&payload)?;
    let covers_published = statement.predicate_type == PUBLISHED_PREDICATE;
    let predicate = statement.predicate;
    let recipe_bytes = read_limited(bundle.file("build.kdl")?, RECIPE_LIMIT)?;
    ensure!(
        predicate.recipe_sha256 == reviewed_recipe && digest(&recipe_bytes) == reviewed_recipe,
        "build recipe differs from the reviewed digest"
    );
    let recipe = parse(std::str::from_utf8(&recipe_bytes)?)?;
    ensure!(
        predicate.architecture == recipe.arch
            && predicate.source_date_epoch == recipe.epoch
            && predicate.sources == recipe.source_digests()
            && predicate.identical_runs == 2,
        "build attestation does not match the recipe or the reproducibility requirement"
    );
    predicate.toolchain.verify(&recipe)?;

    let manifest_bytes = read_limited(bundle.file("manifest.kdl")?, MANIFEST_LIMIT)?;
    let manifest_digest = file_digest(&manifest_bytes);
    ensure!(
        manifest_digest.sha256 == recipe.manifest_digest,
        "bundle manifest differs from the reviewed recipe"
    );
    let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)?;
    manifest.validate(false)?;
    manifest.check_architecture(&recipe.arch)?;
    ensure!(
        manifest.id() == predicate.package,
        "build attestation names a different package"
    );

    let mut subjects = subjects(statement.subject)?;
    ensure!(
        subjects.remove("manifest.kdl") == Some(manifest_digest),
        "build attestation does not cover the bundle manifest"
    );
    let mut artifacts = BTreeMap::new();
    for path in manifest.payload_paths() {
        let file = subjects
            .remove(&format!("proto/{path}"))
            .with_context(|| format!("build attestation omits output {path}"))?;
        artifacts.insert(path.to_owned(), file);
    }
    let published_manifest = if covers_published {
        let expected = published_manifest_digest(&manifest, &artifacts)?;
        ensure!(
            subjects.remove(PUBLISHED_MANIFEST).as_ref() == Some(&expected),
            "build attestation does not cover the exact published manifest"
        );
        Some(expected)
    } else {
        None
    };
    ensure!(
        subjects.is_empty(),
        "build attestation contains undeclared outputs"
    );
    Ok((
        manifest,
        VerifiedBuild {
            payload,
            signatures: BTreeMap::from([(digest(builder_key), signature)]),
            artifacts,
            recipe_sha256: reviewed_recipe.to_owned(),
            published_manifest,
        },
    ))
}

fn parse_statement(payload: &[u8]) -> Result<Statement> {
    let statement: Statement = serde_json::from_slice(payload)?;
    // CPS producers use one serialization. Reject ambiguous fields in externally signed statements.
    ensure!(
        serde_json::to_vec(&statement)? == payload,
        "noncanonical CPS build statement"
    );
    ensure!(
        statement.statement_type == STATEMENT
            && (statement.predicate_type == PUBLISHED_PREDICATE
                || statement.predicate_type == statement.predicate.toolchain.predicate_type()),
        "unexpected build attestation statement type"
    );
    Ok(statement)
}

fn subjects(input: Vec<Subject>) -> Result<BTreeMap<String, FileDigest>> {
    let mut subjects = BTreeMap::new();
    for subject in input {
        validate_digest(&subject.digest.sha256)?;
        ensure!(
            subjects
                .insert(
                    subject.name,
                    FileDigest {
                        sha256: subject.digest.sha256,
                        size: subject.annotations.bytes,
                    }
                )
                .is_none(),
            "duplicate build attestation subject"
        );
    }
    Ok(subjects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_pins_and_selected_roots_are_bound_to_the_recipe() -> Result<()> {
        let hash = "a".repeat(64);
        let recipe = parse(&format!(
            r#"schema-version 1
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v1" {{
        publisher "seed" catalog="{hash}" key-sha256="{hash}"
        publisher "tools" catalog="{hash}" key-sha256="{hash}"
        package "cps://tools/build/compiler@1.0.0-0"
    }}
    manifest "manifest.kdl" sha256="{hash}"
    script "true"
}}
"#
        ))?;
        let ToolchainPins::Publishers(pins) = &recipe.toolchain else {
            panic!("expected publisher pins");
        };
        let chain = |publishers, publisher: &str, version: &str| {
            Toolchain::Pinned(PinnedToolchain {
                publishers,
                package_manifests: BTreeMap::from([(
                    "build/compiler".to_owned(),
                    PinnedPackage {
                        publisher: publisher.to_owned(),
                        version: version.parse().unwrap(),
                        sha256: hash.clone(),
                    },
                )]),
            })
        };
        chain(pins.clone(), "tools", "1.0.0-0").verify(&recipe)?;
        let mut modified = pins.clone();
        modified.reverse();
        assert!(chain(modified, "tools", "1.0.0-0").verify(&recipe).is_err());
        let mut modified = pins.clone();
        modified[1].key_sha256 = "b".repeat(64);
        assert!(chain(modified, "tools", "1.0.0-0").verify(&recipe).is_err());
        assert!(
            chain(pins.clone(), "other", "1.0.0-0")
                .verify(&recipe)
                .is_err()
        );
        assert!(
            chain(pins.clone(), "seed", "1.0.0-0")
                .verify(&recipe)
                .is_err()
        );
        assert!(
            chain(pins.clone(), "tools", "2.0.0-0")
                .verify(&recipe)
                .is_err()
        );
        Ok(())
    }
}
