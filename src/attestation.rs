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
        build::{BuildRecipe, PublisherPin, RECIPE_LIMIT, parse},
        manifest_text, parse_manifest,
    },
    model::{Action, FileDigest, Manifest, Version, digest, validate_digest, validate_name},
    repo::{MANIFEST_LIMIT, read_key, read_limited},
};

pub(crate) const ATTESTATION_LIMIT: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_BUILDERS: usize = 16;
const STATEMENT: &str = "https://in-toto.io/Statement/v1";
const PUBLISHED_PREDICATE: &str = "urn:canopa:cps:published-build-provenance:v1";
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

#[derive(Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Toolchain {
    pub publishers: Vec<PublisherPin>,
    pub package_manifests: BTreeMap<String, PinnedPackage>,
}

#[derive(Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedPackage {
    pub publisher: String,
    pub version: Version,
    pub sha256: String,
}

impl Toolchain {
    fn verify(&self, recipe: &BuildRecipe) -> Result<()> {
        let pins = &recipe.toolchain;
        ensure!(
            &self.publishers == pins,
            "build attestation does not match the pinned toolchain publishers"
        );
        for (name, package) in &self.package_manifests {
            validate_name(name)?;
            validate_digest(&package.sha256)?;
            ensure!(
                pins.iter().any(|pin| pin.name == package.publisher),
                "toolchain package {name} claims an unpinned publisher"
            );
        }
        for (name, spec) in recipe.requested()? {
            let package = self
                .package_manifests
                .get(name)
                .with_context(|| format!("build attestation omits toolchain package {name}"))?;
            ensure!(
                spec.publisher
                    .is_none_or(|publisher| package.publisher == publisher)
                    && spec
                        .version
                        .is_none_or(|version| package.version == version),
                "build attestation claims a different selection for toolchain package {name}"
            );
        }
        Ok(())
    }
}

pub(crate) struct VerifiedBuild {
    payload: Vec<u8>,
    signatures: BTreeMap<String, Vec<u8>>,
    pub artifacts: BTreeMap<String, FileDigest>,
    pub recipe_sha256: String,
    pub recipe: BuildRecipe,
    pub recipe_document: Vec<u8>,
    pub manifest_document: Vec<u8>,
    pub toolchain: Toolchain,
    pub published_manifest: FileDigest,
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

// The proto tree supplies the payload bytes for generated ELF declarations.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign(
    key: &Ed25519KeyPair,
    manifest: &Manifest,
    manifest_bytes: &[u8],
    recipe: &BuildRecipe,
    recipe_bytes: &[u8],
    toolchain: Toolchain,
    artifacts: &BTreeMap<String, FileDigest>,
    proto: &Root,
) -> Result<Vec<u8>> {
    let mut subject = vec![
        Subject::new("manifest.kdl".to_owned(), &file_digest(manifest_bytes)),
        Subject::new(
            PUBLISHED_MANIFEST.to_owned(),
            &published_manifest_digest(manifest, artifacts, proto)?,
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
    proto: &Root,
) -> Result<FileDigest> {
    let published =
        crate::generate::elf::published_manifest(manifest.clone(), artifacts, |path, _| {
            proto.file(path)
        })?;
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
    let predicate = statement.predicate;
    let recipe_bytes = read_limited(bundle.file("build.kdl")?, RECIPE_LIMIT)?;
    ensure!(
        predicate.recipe_sha256 == reviewed_recipe && digest(&recipe_bytes) == reviewed_recipe,
        "build recipe differs from the reviewed digest"
    );
    let recipe = parse(std::str::from_utf8(&recipe_bytes)?)?;
    check_recipe(&predicate, &recipe)?;

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

    let (artifacts, published_manifest) =
        claimed_artifacts(&manifest, manifest_digest, statement.subject)?;
    Ok((
        manifest,
        VerifiedBuild {
            payload,
            signatures: BTreeMap::from([(digest(builder_key), signature)]),
            artifacts,
            recipe_sha256: reviewed_recipe.to_owned(),
            recipe,
            recipe_document: recipe_bytes,
            manifest_document: manifest_bytes,
            toolchain: predicate.toolchain,
            published_manifest,
        },
    ))
}

fn check_recipe(predicate: &Predicate, recipe: &BuildRecipe) -> Result<()> {
    ensure!(
        predicate.architecture == recipe.arch
            && predicate.source_date_epoch == recipe.epoch
            && predicate.sources == recipe.source_digests()
            && predicate.identical_runs == 2,
        "build attestation does not match the recipe or the reproducibility requirement"
    );
    predicate.toolchain.verify(recipe)
}

fn claimed_artifacts(
    manifest: &Manifest,
    manifest_digest: FileDigest,
    claimed: Vec<Subject>,
) -> Result<(BTreeMap<String, FileDigest>, FileDigest)> {
    let mut subjects = subjects(claimed)?;
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
    // The published manifest carries declarations generated from the
    // payload bytes. Publication derives it again from the stored payloads
    // and requires this digest, and the signed catalog binds it afterwards.
    let published_manifest = subjects
        .remove(PUBLISHED_MANIFEST)
        .context("build attestation does not cover the exact published manifest")?;
    ensure!(
        subjects.is_empty(),
        "build attestation contains undeclared outputs"
    );
    Ok((artifacts, published_manifest))
}

pub(crate) struct RebuildEvidence {
    pub recipe: BuildRecipe,
    pub artifacts: BTreeMap<String, FileDigest>,
    payload: Vec<u8>,
    signatures: Vec<Vec<u8>>,
}

impl RebuildEvidence {
    pub fn check_witness(&self, envelope: &[u8], key: &[u8]) -> Result<()> {
        let message = pae(&self.payload);
        let public = UnparsedPublicKey::new(&ED25519, key);
        ensure!(
            !self
                .signatures
                .iter()
                .any(|sig| public.verify(&message, sig).is_ok()),
            "rebuild witness already signed the admitted build"
        );
        let (payload, _) = verify_envelope(envelope, key)?;
        ensure!(
            payload == self.payload,
            "rebuild witness statement differs from the admitted build"
        );
        Ok(())
    }
}

pub(crate) fn verify_rebuild_inputs(
    envelope: &[u8],
    id: &str,
    recipe_bytes: &[u8],
    manifest_bytes: &[u8],
    published_hash: &str,
) -> Result<RebuildEvidence> {
    let (payload, signatures) = decode_envelope(envelope)?;
    let statement = parse_statement(&payload)?;
    ensure!(
        statement.predicate.package == id,
        "admitted build names a different package"
    );
    ensure!(
        statement.predicate.recipe_sha256 == digest(recipe_bytes),
        "rebuild recipe differs from the admitted build"
    );
    let recipe = parse(std::str::from_utf8(recipe_bytes)?)?;
    check_recipe(&statement.predicate, &recipe)?;
    let manifest_digest = file_digest(manifest_bytes);
    ensure!(
        manifest_digest.sha256 == recipe.manifest_digest,
        "rebuild manifest differs from the reviewed recipe"
    );
    let manifest = parse_manifest(std::str::from_utf8(manifest_bytes)?)?;
    manifest.validate(false)?;
    manifest.check_architecture(&recipe.arch)?;
    ensure!(
        manifest.id() == id,
        "rebuild manifest names a different package"
    );
    let (artifacts, published) = claimed_artifacts(&manifest, manifest_digest, statement.subject)?;
    ensure!(
        published.sha256 == published_hash,
        "admitted build does not match the catalog manifest for {id}"
    );
    Ok(RebuildEvidence {
        recipe,
        artifacts,
        payload,
        signatures,
    })
}

fn parse_statement(payload: &[u8]) -> Result<Statement> {
    let statement: Statement = serde_json::from_slice(payload)?;
    // CPS producers use one serialization. Reject ambiguous fields in externally signed statements.
    ensure!(
        serde_json::to_vec(&statement)? == payload,
        "noncanonical CPS build statement"
    );
    ensure!(
        statement.statement_type == STATEMENT && statement.predicate_type == PUBLISHED_PREDICATE,
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
    fn rejects_older_predicates_even_with_a_valid_builder_signature() -> Result<()> {
        let hash = digest(b"recipe input");
        let recipe_bytes = format!(
            r#"schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v2" {{
        publisher "seed" catalog="{hash}" key-sha256="{hash}"
        package "build/compiler"
    }}
    manifest "manifest.kdl" sha256="{hash}"
    script "true"
}}
"#
        );
        let manifest_bytes = b"schema-version 1 kind=\"package-manifest\"\npackage \"test/compiled\" publisher=\"canopa\" version=\"1.0.0-0\"\n";
        let recipe = parse(&recipe_bytes)?;
        let manifest = parse_manifest(std::str::from_utf8(manifest_bytes)?)?;
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let bytes = sign(
            &key,
            &manifest,
            manifest_bytes,
            &recipe,
            recipe_bytes.as_bytes(),
            Toolchain {
                publishers: recipe.toolchain.clone(),
                package_manifests: BTreeMap::from([(
                    "build/compiler".into(),
                    PinnedPackage {
                        publisher: "seed".into(),
                        version: "1.0.0-0".parse()?,
                        sha256: hash,
                    },
                )]),
            },
            &BTreeMap::new(),
            &Root::open(&std::env::temp_dir())?,
        )?;
        let (payload, _) = verify_envelope(&bytes, key.public_key().as_ref())?;
        let mut statement = parse_statement(&payload)?;
        assert_eq!(
            statement.predicate_type,
            "urn:canopa:cps:published-build-provenance:v1"
        );
        for old in [
            "urn:canopa:cps:build-provenance:v1",
            "urn:canopa:cps:build-provenance:v2",
            "urn:canopa:cps:build-provenance:v3",
        ] {
            statement.predicate_type = old.into();
            let payload = serde_json::to_vec(&statement)?;
            let signature = key.sign(&pae(&payload));
            let bytes = envelope(&payload, [("builder", signature.as_ref())])?;
            let (payload, _) = verify_envelope(&bytes, key.public_key().as_ref())?;
            assert!(parse_statement(&payload).is_err());
        }
        Ok(())
    }

    #[test]
    fn publisher_pins_and_selected_roots_are_bound_to_the_recipe() -> Result<()> {
        let hash = "a".repeat(64);
        let recipe = parse(&format!(
            r#"schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v2" {{
        publisher "seed" catalog="{hash}" key-sha256="{hash}"
        publisher "tools" catalog="{hash}" key-sha256="{hash}"
        package "cps://tools/build/compiler@1.0.0-0"
    }}
    manifest "manifest.kdl" sha256="{hash}"
    script "true"
}}
"#
        ))?;
        let pins = &recipe.toolchain;
        let chain = |publishers, publisher: &str, version: &str| Toolchain {
            publishers,
            package_manifests: BTreeMap::from([(
                "build/compiler".to_owned(),
                PinnedPackage {
                    publisher: publisher.to_owned(),
                    version: version.parse().unwrap(),
                    sha256: hash.clone(),
                },
            )]),
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
