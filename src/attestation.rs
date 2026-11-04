use std::collections::BTreeMap;

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
        build::{BuildRecipe, RECIPE_LIMIT, parse},
        parse_manifest,
    },
    model::{FileDigest, Manifest, digest, validate_digest, validate_name, validate_publisher},
    repo::{MANIFEST_LIMIT, read_limited},
};

pub(crate) const ATTESTATION_LIMIT: u64 = 64 * 1024 * 1024;
const STATEMENT: &str = "https://in-toto.io/Statement/v1";
const PREDICATE: &str = "urn:canopa:cps:build-provenance:v1";
const MEDIA_TYPE: &str = "application/vnd.in-toto+json";

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
#[serde(deny_unknown_fields)]
pub(crate) struct Toolchain {
    pub publisher: String,
    pub key_sha256: String,
    pub catalog_sha256: String,
    pub package_manifests: BTreeMap<String, String>,
}

pub(crate) struct VerifiedBuild {
    pub envelope: Vec<u8>,
    pub artifacts: BTreeMap<String, FileDigest>,
    pub builder_sha256: String,
    pub recipe_sha256: String,
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
    let mut subject = vec![Subject::new(
        "manifest.kdl".to_owned(),
        &file_digest(manifest_bytes),
    )];
    subject.extend(
        artifacts
            .iter()
            .map(|(path, file)| Subject::new(format!("proto/{path}"), file)),
    );
    let statement = Statement {
        statement_type: STATEMENT.to_owned(),
        subject,
        predicate_type: PREDICATE.to_owned(),
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
    envelope(&payload, signature.as_ref(), key.public_key().as_ref())
}

fn envelope(payload: &[u8], signature: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&Envelope {
        payload_type: MEDIA_TYPE.to_owned(),
        payload: STANDARD.encode(payload),
        signatures: vec![Signature {
            keyid: digest(key),
            sig: STANDARD.encode(signature),
        }],
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

pub(crate) fn verify_bundle(
    bundle: &Root,
    builder_key: &[u8],
    reviewed_recipe: &str,
) -> Result<(Manifest, VerifiedBuild)> {
    validate_digest(reviewed_recipe).context("invalid reviewed recipe digest")?;
    let bytes = read_limited(bundle.file("attestation.json")?, ATTESTATION_LIMIT)?;
    let signed: Envelope = serde_json::from_slice(&bytes)?;
    ensure!(
        signed.payload_type == MEDIA_TYPE,
        "unexpected attestation payload type"
    );
    let payload = decode(&signed.payload)?;
    let message = pae(&payload);
    let key = UnparsedPublicKey::new(&ED25519, builder_key);
    let signature = signed
        .signatures
        .iter()
        .find_map(|entry| {
            let signature = decode(&entry.sig).ok()?;
            key.verify(&message, &signature).ok()?;
            Some(signature)
        })
        .context("invalid builder attestation signature")?;
    let statement: Statement = serde_json::from_slice(&payload)?;
    // CPS producers use one serialization. Reject ambiguous fields in externally signed statements.
    ensure!(
        serde_json::to_vec(&statement)? == payload,
        "noncanonical CPS build statement"
    );
    ensure!(
        statement.statement_type == STATEMENT && statement.predicate_type == PREDICATE,
        "unexpected build attestation statement type"
    );
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
            && predicate.toolchain.catalog_sha256 == recipe.catalog
            && predicate.sources == recipe.source_digests()
            && predicate.identical_runs == 2,
        "build attestation does not match the recipe or the reproducibility requirement"
    );
    validate_publisher(&predicate.toolchain.publisher)?;
    validate_digest(&predicate.toolchain.key_sha256)?;
    for (name, hash) in &predicate.toolchain.package_manifests {
        validate_name(name)?;
        validate_digest(hash)?;
    }
    ensure!(
        recipe
            .packages
            .iter()
            .all(|name| predicate.toolchain.package_manifests.contains_key(name)),
        "build attestation omits a requested toolchain package"
    );

    let manifest_bytes = read_limited(bundle.file("manifest.kdl")?, MANIFEST_LIMIT)?;
    let manifest_digest = file_digest(&manifest_bytes);
    ensure!(
        manifest_digest.sha256 == recipe.manifest_digest,
        "bundle manifest differs from the reviewed recipe"
    );
    let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)?;
    manifest.validate(false)?;
    ensure!(
        manifest.id() == predicate.package,
        "build attestation names a different package"
    );

    let mut subjects = BTreeMap::new();
    for subject in statement.subject {
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
    ensure!(
        subjects.is_empty(),
        "build attestation contains undeclared outputs"
    );
    Ok((
        manifest,
        VerifiedBuild {
            envelope: envelope(&payload, &signature, builder_key)?,
            artifacts,
            builder_sha256: digest(builder_key),
            recipe_sha256: reviewed_recipe.to_owned(),
        },
    ))
}
