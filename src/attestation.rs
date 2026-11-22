use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::{
    build::{TEMPLATES, TemplateInput, load_templates, read_expanded_output, read_templates},
    image::root::Root,
    kdl_format::{
        build::{BuildRecipe, PublisherPin, RECIPE_LIMIT, TemplatePin, parse},
        manifest_text,
    },
    model::{Action, FileDigest, Manifest, Version, digest, validate_digest, validate_name},
    repo::{
        MANIFEST_LIMIT, OtherSigners, PublicKey, Purpose, SigningKey, read_key, read_limited,
        trust_root_keys, verify_signatures,
    },
};

pub(crate) const ATTESTATION_LIMIT: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_BUILDERS: usize = 16;
const STATEMENT: &str = "https://in-toto.io/Statement/v1";
const PUBLISHED_PREDICATE: &str = "urn:canopa:cps:published-build-provenance:v1";

/// The subject that names an output's exact published manifest.
fn manifest_subject(name: &str) -> String {
    format!("manifest/{name}")
}
const MEDIA_TYPE: &str = "application/vnd.in-toto+json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildTrust {
    minimum_builders: usize,
    keys: BTreeSet<[u8; 32]>,
}

impl BuildTrust {
    pub(crate) fn load(minimum_builders: usize, paths: &[PathBuf]) -> Result<Self> {
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

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.keys.len() <= MAX_BUILDERS
                && (1..=self.keys.len()).contains(&self.minimum_builders),
            "build trust requires between one and {MAX_BUILDERS} keys and a threshold within that range"
        );
        Ok(())
    }

    pub(crate) fn check_publisher_key(&self, key: &[u8]) -> Result<()> {
        ensure!(
            !self.keys.iter().any(|builder| builder.as_slice() == key),
            "builder and publisher must use distinct keys"
        );
        Ok(())
    }

    /// Require every builder key to differ from every key of a pinned trust root.
    pub(crate) fn check_publisher_trust(&self, trust_root: &[u8]) -> Result<()> {
        for key in trust_root_keys(trust_root)? {
            self.check_publisher_key(&key)?;
        }
        Ok(())
    }

    pub(crate) fn verify(
        &self,
        bytes: &[u8],
        manifest: &Manifest,
        published: FileDigest,
    ) -> Result<()> {
        let (payload, signatures) = decode_envelope(bytes)?;
        let valid = verify_signatures(
            "build attestation",
            &pae(&payload),
            signatures.iter().map(|(id, sig)| (id.as_str(), sig)),
            &self.keys,
            OtherSigners::Ignore,
        )?
        .len();
        ensure!(
            valid >= self.minimum_builders,
            "build attestation requires {} trusted builder signatures, found {valid}",
            self.minimum_builders
        );
        let statement = parse_statement(&payload)?;
        let id = manifest.id();
        ensure!(
            statement.predicate.packages.contains(&id) && statement.predicate.identical_runs == 2,
            "build attestation does not match the package or the reproducibility requirement"
        );
        manifest.check_architecture(&statement.predicate.architecture)?;
        validate_digest(&statement.predicate.recipe_sha256)?;
        // One statement covers every output of the build. Check this package's
        // manifest and payloads; the other outputs have their own catalog entries.
        let subjects = subjects(statement.subject)?;
        ensure!(
            subjects.get(&manifest_subject(&manifest.name)) == Some(&published),
            "build attestation does not cover the exact published manifest"
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

/// A DSSE signature: `keyid` is the SHA-256 of the raw Ed25519 public key in
/// lowercase hexadecimal and `sig` the standard padded base64 of the 64-byte
/// signature.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signature {
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
    fn file(&self) -> FileDigest {
        FileDigest {
            sha256: self.digest.sha256.clone(),
            size: self.annotations.bytes,
        }
    }

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
    /// The IDs of every package that the build produced, in recipe order.
    packages: Vec<String>,
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
                    && spec.accepts(&package.version),
                "build attestation claims a different selection for toolchain package {name}"
            );
        }
        Ok(())
    }
}

pub(crate) struct VerifiedBuild {
    payload: Vec<u8>,
    signatures: BTreeMap<String, Vec<u8>>,
    /// The payloads of every output.
    pub artifacts: BTreeMap<String, FileDigest>,
    pub recipe_sha256: String,
    pub recipe: BuildRecipe,
    pub recipe_document: Vec<u8>,
    /// The recipe's pinned templates and their exact bytes.
    pub templates: Vec<TemplateInput>,
    pub toolchain: Toolchain,
    /// The exact published manifest of each output, by package ID.
    pub published: BTreeMap<String, FileDigest>,
}

impl VerifiedBuild {
    pub(crate) fn add_witness(&mut self, bytes: &[u8], key: &[u8]) -> Result<()> {
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

    pub(crate) fn builders(&self) -> impl ExactSizeIterator<Item = &str> {
        self.signatures.keys().map(String::as_str)
    }

    pub(crate) fn envelope(&self) -> Result<Vec<u8>> {
        envelope(
            &self.payload,
            self.signatures
                .iter()
                .map(|(key, sig)| (key.as_str(), sig.as_slice())),
        )
    }
}

/// Sign one statement that covers every output of a build: each output's exact
/// published manifest and every payload, with the recipe, sources, and toolchain.
/// The builder key is held in a file or by an external signer.
pub(crate) fn sign(
    key: &SigningKey,
    manifests: &[Manifest],
    recipe: &BuildRecipe,
    recipe_bytes: &[u8],
    toolchain: Toolchain,
    artifacts: &BTreeMap<String, FileDigest>,
    proto: &Root,
) -> Result<Vec<u8>> {
    let mut subject = manifests
        .iter()
        .map(|manifest| {
            Ok(Subject::new(
                manifest_subject(&manifest.name),
                &published_manifest_digest(manifest, artifacts, proto)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
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
            packages: manifests.iter().map(Manifest::id).collect(),
            recipe_sha256: digest(recipe_bytes),
            architecture: recipe.arch.clone(),
            source_date_epoch: recipe.epoch,
            sources: recipe.source_digests(),
            toolchain,
            identical_runs: 2,
        },
    };
    let payload = serde_json::to_vec(&statement)?;
    let signature = key.sign(Purpose::Attestation, &pae(&payload))?;
    envelope(&payload, [(key.key_id().as_str(), signature.as_slice())])
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

/// Decode standard padded base64 in its one spelling.
fn decode(encoded: &str) -> Result<Vec<u8>> {
    let bytes = STANDARD.decode(encoded).context("invalid DSSE base64")?;
    ensure!(
        STANDARD.encode(&bytes) == encoded,
        "DSSE base64 is not canonical"
    );
    Ok(bytes)
}

fn file_digest(bytes: &[u8]) -> FileDigest {
    FileDigest {
        sha256: digest(bytes),
        size: bytes.len() as u64,
    }
}

/// The digest of an output's exact published manifest. It includes the ELF
/// declarations generated from the payload bytes in `proto`; publication derives
/// them again from the stored payloads and requires this digest.
fn published_manifest_digest(
    manifest: &Manifest,
    artifacts: &BTreeMap<String, FileDigest>,
    proto: &Root,
) -> Result<FileDigest> {
    // Generation parses payload bytes, so check each one against the digest
    // the statement claims before parsing it.
    let published =
        crate::generate::elf::published_manifest(manifest.clone(), artifacts, |path, file| {
            let mut input = proto.file(path)?;
            ensure!(
                crate::model::digest_reader(&mut input)? == file.sha256,
                "attested payload digest mismatch at {path}"
            );
            std::io::Seek::rewind(&mut input)?;
            Ok(input)
        })?;
    let bytes = manifest_text(&published);
    ensure!(
        bytes.len() as u64 <= MANIFEST_LIMIT,
        "manifest exceeds {MANIFEST_LIMIT} bytes"
    );
    Ok(file_digest(bytes.as_bytes()))
}

/// The signatures of a DSSE envelope by key ID, in key ID order.
type Signatures = Vec<(String, [u8; 64])>;

/// Decode an attestation envelope in its one accepted byte form: canonical
/// JSON, standard padded base64, and 1 to MAX_BUILDERS signatures sorted by
/// unique key ID, each exactly 64 bytes. Nothing is skipped or reinterpreted.
fn decode_envelope(bytes: &[u8]) -> Result<(Vec<u8>, Signatures)> {
    let signed: Envelope = serde_json::from_slice(bytes).context("invalid DSSE envelope")?;
    ensure!(
        serde_json::to_vec(&signed)? == bytes,
        "DSSE envelope is not canonical"
    );
    ensure!(
        signed.payload_type == MEDIA_TYPE,
        "unexpected attestation payload type"
    );
    ensure!(
        (1..=MAX_BUILDERS).contains(&signed.signatures.len()),
        "an attestation carries 1 to {MAX_BUILDERS} signatures"
    );
    let payload = decode(&signed.payload)?;
    let mut signatures = Signatures::new();
    for entry in signed.signatures {
        validate_digest(&entry.keyid).context("invalid attestation signature key ID")?;
        ensure!(
            signatures
                .last()
                .is_none_or(|(last, _)| *last < entry.keyid),
            "attestation signatures must be sorted by unique key ID"
        );
        let signature = decode(&entry.sig)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("an attestation signature is 64 bytes"))?;
        signatures.push((entry.keyid, signature));
    }
    Ok((payload, signatures))
}

/// Verify the signature of one builder key and return the payload and that
/// signature.
fn verify_envelope(bytes: &[u8], builder_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let (payload, signatures) = decode_envelope(bytes)?;
    let key: PublicKey = builder_key
        .try_into()
        .context("a builder key is a raw 32-byte Ed25519 key")?;
    let id = digest(&key);
    let (_, signature) = signatures
        .iter()
        .find(|(signer, _)| *signer == id)
        .with_context(|| {
            format!("invalid builder attestation signature: none is by builder key {id}")
        })?;
    verify_signatures(
        "builder attestation",
        &pae(&payload),
        [(id.as_str(), signature)],
        &BTreeSet::from([key]),
        OtherSigners::Reject,
    )?;
    Ok((payload, signature.to_vec()))
}

/// Verify a build bundle against the builder key and the reviewed recipe digest.
/// Returns the expanded manifest of each output, in recipe order.
pub(crate) fn verify_bundle(
    bundle: &Root,
    builder_key: &[u8],
    reviewed_recipe: &str,
) -> Result<(Vec<Manifest>, VerifiedBuild)> {
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
    let templates = read_templates(&bundle.subdir(TEMPLATES)?, &recipe)
        .context("bundle templates differ from the reviewed recipe")?;
    let manifests = expanded_outputs(bundle, &recipe, &templates, &predicate.packages)?;
    let (artifacts, published) =
        claimed_artifacts(&manifests, statement.subject, &bundle.subdir("proto")?)?;
    Ok((
        manifests,
        VerifiedBuild {
            payload,
            signatures: BTreeMap::from([(digest(builder_key), signature)]),
            artifacts,
            recipe_sha256: reviewed_recipe.to_owned(),
            recipe,
            recipe_document: recipe_bytes,
            templates,
            toolchain: predicate.toolchain,
            published,
        },
    ))
}

/// Read each output's expanded manifest from a bundle and check that its template
/// produces it and that the statement names exactly these packages.
fn expanded_outputs(
    bundle: &Root,
    recipe: &BuildRecipe,
    templates: &[TemplateInput],
    packages: &[String],
) -> Result<Vec<Manifest>> {
    let manifests = templates
        .iter()
        .map(|input| read_expanded_output(bundle, recipe, &input.template))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        manifests
            .iter()
            .map(Manifest::id)
            .eq(packages.iter().cloned()),
        "build attestation names different packages than the recipe"
    );
    Ok(manifests)
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

/// Match the statement's subjects to exactly the payloads and published
/// manifests of every output.
fn claimed_artifacts(
    manifests: &[Manifest],
    claimed: Vec<Subject>,
    proto: &Root,
) -> Result<(BTreeMap<String, FileDigest>, BTreeMap<String, FileDigest>)> {
    let mut subjects = subjects(claimed)?;
    let mut artifacts = BTreeMap::new();
    for path in manifests.iter().flat_map(Manifest::payload_paths) {
        if !artifacts.contains_key(path) {
            let file = subjects
                .remove(&format!("proto/{path}"))
                .with_context(|| format!("build attestation omits output {path}"))?;
            artifacts.insert(path.to_owned(), file);
        }
    }
    let mut published = BTreeMap::new();
    for manifest in manifests {
        let digest = published_manifest_digest(manifest, &artifacts, proto)?;
        ensure!(
            subjects.remove(&manifest_subject(&manifest.name)).as_ref() == Some(&digest),
            "build attestation does not cover the exact published manifest of {}",
            manifest.id()
        );
        published.insert(manifest.id(), digest);
    }
    ensure!(
        subjects.is_empty(),
        "build attestation contains undeclared outputs"
    );
    Ok((artifacts, published))
}

/// An admitted build statement, checked against archived or rebuilt inputs.
pub(crate) struct RebuildEvidence {
    pub recipe: BuildRecipe,
    pub templates: Vec<TemplateInput>,
    payload: Vec<u8>,
    signatures: Signatures,
    packages: Vec<String>,
    subjects: Vec<Subject>,
}

impl RebuildEvidence {
    /// The IDs of every package that the admitted build produced.
    pub(crate) fn packages(&self) -> &[String] {
        &self.packages
    }

    /// The digest of the published manifest that the admitted build names for a package.
    pub(crate) fn published_manifest(&self, id: &str) -> Result<&str> {
        let name = &crate::model::PackageSpec::parse(id)?.name;
        self.subjects
            .iter()
            .find(|subject| subject.name == manifest_subject(name))
            .map(|subject| subject.digest.sha256.as_str())
            .with_context(|| format!("admitted build does not cover {id}"))
    }

    /// The payload digests that the admitted build names, by output path.
    pub(crate) fn admitted_artifacts(&self) -> BTreeMap<String, FileDigest> {
        self.subjects
            .iter()
            .filter_map(|subject| {
                let path = subject.name.strip_prefix("proto/")?;
                Some((path.to_owned(), subject.file()))
            })
            .collect()
    }

    /// Check a rebuilt bundle's expanded manifests against the admitted build,
    /// and return the admitted payload digests of every output.
    pub(crate) fn check_outputs(&self, bundle: &Root) -> Result<BTreeMap<String, FileDigest>> {
        let manifests = expanded_outputs(bundle, &self.recipe, &self.templates, &self.packages)?;
        let claimed = self
            .subjects
            .iter()
            .map(|subject| Subject::new(subject.name.clone(), &subject.file()))
            .collect();
        Ok(claimed_artifacts(&manifests, claimed, &bundle.subdir("proto")?)?.0)
    }

    pub(crate) fn check_witness(&self, envelope: &[u8], key: &[u8]) -> Result<()> {
        let public: PublicKey = key
            .try_into()
            .context("a builder key is a raw 32-byte Ed25519 key")?;
        let signed = verify_signatures(
            "admitted build attestation",
            &pae(&self.payload),
            self.signatures.iter().map(|(id, sig)| (id.as_str(), sig)),
            &BTreeSet::from([public]),
            OtherSigners::Ignore,
        )?;
        ensure!(
            signed.is_empty(),
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

/// Check that a recipe and its templates are the inputs of an admitted build
/// that produced `id`.
pub(crate) fn verify_rebuild_inputs(
    envelope: &[u8],
    id: &str,
    recipe_bytes: &[u8],
    templates: impl FnMut(&TemplatePin) -> Result<Vec<u8>>,
) -> Result<RebuildEvidence> {
    let (payload, signatures) = decode_envelope(envelope)?;
    let statement = parse_statement(&payload)?;
    let predicate = statement.predicate;
    ensure!(
        predicate.packages.iter().any(|package| package == id),
        "admitted build does not produce {id}"
    );
    ensure!(
        predicate.recipe_sha256 == digest(recipe_bytes),
        "rebuild recipe differs from the admitted build"
    );
    let recipe = parse(std::str::from_utf8(recipe_bytes)?)?;
    check_recipe(&predicate, &recipe)?;
    let templates = load_templates(&recipe, templates)?;
    ensure!(
        templates
            .iter()
            .map(|input| input.template.manifest.id())
            .eq(predicate.packages.iter().cloned()),
        "rebuild templates name different packages than the admitted build"
    );
    Ok(RebuildEvidence {
        recipe,
        templates,
        payload,
        signatures,
        packages: predicate.packages,
        subjects: statement.subject,
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
    use crate::kdl_format::parse_manifest;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};

    #[test]
    fn attestation_envelopes_have_one_spelling() -> Result<()> {
        let mut ids = [digest(b"a"), digest(b"b")];
        ids.sort();
        let [a, b] = ids;
        let (sa, sb) = ([1; 64], [2; 64]);
        let good = envelope(b"{}", [(a.as_str(), sa.as_slice()), (b.as_str(), &sb)])?;
        let (payload, signatures) = decode_envelope(&good)?;
        assert_eq!(payload, b"{}");
        assert_eq!(signatures, [(a.clone(), sa), (b.clone(), sb)]);

        let text = String::from_utf8(good)?;
        let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0xfb; 64]);
        for bad in [
            // Unsorted or repeated key IDs, and key IDs that are not digests.
            String::from_utf8(envelope(
                b"{}",
                [(b.as_str(), sb.as_slice()), (a.as_str(), &sa)],
            )?)?,
            String::from_utf8(envelope(
                b"{}",
                [(a.as_str(), sa.as_slice()), (a.as_str(), &sb)],
            )?)?,
            String::from_utf8(envelope(b"{}", [("builder", sa.as_slice())])?)?,
            // Signatures that are not exactly 64 bytes, or not standard base64.
            String::from_utf8(envelope(b"{}", [(a.as_str(), [1; 63].as_slice())])?)?,
            text.replace(&STANDARD.encode(sa), &url_safe),
            text.replace(
                &STANDARD.encode(sa),
                STANDARD.encode(sa).trim_end_matches('='),
            ),
            // No signatures, a missing key ID, or other JSON spellings.
            String::from_utf8(envelope(b"{}", [])?)?,
            text.replacen(&format!("\"keyid\":\"{a}\","), "", 1),
            text.replacen(':', ": ", 1),
            format!("{text}\n"),
        ] {
            assert!(decode_envelope(bad.as_bytes()).is_err(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn rejects_older_predicates_even_with_a_valid_builder_signature() -> Result<()> {
        let hash = digest(b"recipe input");
        let recipe_bytes = format!(
            r#"schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v2" {{
        publisher "seed" catalog-sha256="{hash}" key-sha256="{hash}"
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
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let proto = tempfile::tempdir()?;
        let bytes = sign(
            &Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap().into(),
            &[manifest],
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
            &Root::open(proto.path())?,
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
            let id = digest(key.public_key().as_ref());
            let bytes = envelope(&payload, [(id.as_str(), signature.as_ref())])?;
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
        publisher "seed" catalog-sha256="{hash}" key-sha256="{hash}"
        publisher "tools" catalog-sha256="{hash}" key-sha256="{hash}"
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
