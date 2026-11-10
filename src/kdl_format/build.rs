use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode, KdlValue};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::build::pattern;
use crate::model::{
    Dependency, FileDigest, MANIFEST_FORMAT, Manifest, PackageSpec, Version, parse_mode,
    validate_digest, validate_facet, validate_path, validate_publisher, validate_variant,
};
use crate::repo::read_limited;

use super::{
    Fields, MANIFEST_KIND, PublisherConfig, SCHEMA_VERSION, document, one, publisher_config,
    schema, schema_header,
};

pub(crate) const RECIPE_LIMIT: u64 = 1024 * 1024;
/// The most packages that one build can produce.
pub(crate) const MAX_OUTPUTS: usize = 64;
const RECIPE_KIND: &str = "build-recipe";

pub(crate) fn git_document(text: &str, publisher: &str) -> Result<(Manifest, String)> {
    let doc = document(text)?;
    schema(&doc, "git-package")?;
    ensure!(
        doc.nodes().len() == 3,
        "expected schema-version, package, and build in cps.kdl"
    );
    let mut package = one(&doc, "package")?.clone();
    package.insert("publisher", publisher);
    let mut manifest = KdlDocument::new();
    manifest
        .nodes_mut()
        .extend([schema_header(MANIFEST_FORMAT, MANIFEST_KIND), package]);
    let manifest = super::parse_manifest(&manifest.to_string())?;

    let mut build = one(&doc, "build")?.clone();
    let children = build.ensure_children();
    ensure!(
        children.get("manifest").is_none(),
        "CPS supplies the manifest for cps.kdl"
    );
    children.nodes_mut().push(KdlNode::new("manifest"));
    let mut recipe = KdlDocument::new();
    recipe
        .nodes_mut()
        .extend([schema_header(SCHEMA_VERSION, RECIPE_KIND), build]);
    Ok((manifest, recipe.to_string()))
}

pub(crate) fn git_toolchain_template(template: &str, config: &Path) -> Result<String> {
    let mut recipe = document(template)?;
    let build = recipe.get_mut("build").context("missing build node")?;
    let children = build.ensure_children();
    let toolchain = one(children, "toolchain")?;
    if toolchain.get(0).and_then(KdlValue::as_string) != Some("default") {
        return Ok(template.to_owned());
    }
    Fields::read(toolchain, 1, &[], false)?;

    let default = config.with_extension("default.kdl");
    let bytes = read_limited(
        File::open(&default)
            .with_context(|| format!("opening default Git toolchain {}", default.display()))?,
        RECIPE_LIMIT,
    )?;
    let pins = document(std::str::from_utf8(&bytes)?)?;
    schema(&pins, "git-toolchain")?;
    ensure!(
        pins.nodes().len() == 2,
        "expected schema-version and toolchain in {}",
        default.display()
    );
    *children
        .get_mut("toolchain")
        .context("missing toolchain node")? = one(&pins, "toolchain")?.clone();
    Ok(recipe.to_string())
}

pub(crate) struct Source {
    pub pin: FileDigest,
    pub url: Option<Url>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublisherPin {
    pub name: String,
    pub catalog_sha256: String,
    pub key_sha256: String,
}

/// A package template pinned by the recipe. Each template becomes one output package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TemplatePin {
    pub path: String,
    pub sha256: String,
}

/// The package version that the recipe supplies to templates that omit it.
pub(crate) struct RecipeVersion {
    pub version: Version,
    pub upstream: Option<String>,
}

pub(crate) struct BuildRecipe {
    pub arch: String,
    pub epoch: u32,
    pub toolchain: Vec<PublisherPin>,
    pub packages: BTreeSet<String>,
    pub version: Option<RecipeVersion>,
    pub manifests: Vec<TemplatePin>,
    pub discard: Vec<String>,
    pub strip: bool,
    pub sources: BTreeMap<String, Source>,
    pub options: BTreeMap<String, String>,
    pub script: String,
}

/// How a `collect` rule assigns a facet to the paths it claims.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FacetChoice {
    /// Apply the built-in rules, such as `devel` for `usr/include/**`.
    Default,
    /// Ship the paths without a facet.
    None,
    Facet(String),
}

/// A build-time rule that claims installed paths for one output package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CollectRule {
    pub pattern: String,
    pub mode: Option<String>,
    pub uid: u32,
    pub gid: u32,
    pub preserve: bool,
    pub facet: FacetChoice,
    pub variants: BTreeMap<String, String>,
}

/// A source manifest: explicit package metadata and actions plus `collect` rules
/// that `cps build` expands against the build's destination tree.
#[derive(Clone, Debug)]
pub(crate) struct Template {
    pub manifest: Manifest,
    pub collect: Vec<CollectRule>,
}

/// The token that a template dependency constraint can use for the recipe version.
const VERSION_TOKEN: &str = "{version}";

/// Parse a pinned template in the context of its recipe.
pub(crate) fn parse_template(text: &str, recipe: &BuildRecipe) -> Result<Template> {
    let mut collect = Vec::new();
    let (mut manifest, declared) = super::parse_package(
        text,
        |node| {
            if node.name().value() != "collect" {
                return Ok(false);
            }
            collect.push(collect_rule(node).context("collect rule")?);
            Ok(true)
        },
        recipe.version.as_ref().map(|version| &version.version),
    )?;
    if let Some(version) = &recipe.version {
        ensure!(
            !declared && manifest.upstream_version.is_none(),
            "{} sets its version in the template, but the recipe already declares version {}",
            manifest.name,
            version.version
        );
        manifest.upstream_version.clone_from(&version.upstream);
        let constraints = manifest
            .dependency
            .iter_mut()
            .flat_map(|dependency| match dependency {
                Dependency::Require(target)
                | Dependency::RequireCapability(target)
                | Dependency::Recommend(target)
                | Dependency::Incorporate(target)
                | Dependency::Exclude(target) => vec![target],
                Dependency::RequireAny { alternatives } => alternatives.iter_mut().collect(),
                Dependency::Conditional { target, predicate } => vec![target, predicate],
            })
            .chain(
                manifest
                    .optional_dependencies
                    .iter_mut()
                    .map(|optional| &mut optional.target),
            );
        let replacement = version.version.to_string();
        for target in constraints {
            if let Some(constraint) = &mut target.version {
                *constraint = constraint.replace(VERSION_TOKEN, &replacement);
            }
        }
    }
    manifest.validate(false)?;
    manifest.check_architecture(&recipe.arch)?;
    let mut patterns = BTreeSet::new();
    for rule in &collect {
        ensure!(
            patterns.insert(&rule.pattern),
            "duplicate collect rule {}",
            rule.pattern
        );
    }
    Ok(Template { manifest, collect })
}

fn collect_rule(node: &KdlNode) -> Result<CollectRule> {
    let fields = Fields::read(
        node,
        1,
        &[
            "mode",
            "uid",
            "gid",
            "preserve",
            "facet",
            "arch",
            super::VARIANT_PROPERTY,
        ],
        false,
    )?;
    let pattern = fields.string_arg(0)?;
    pattern::validate(&pattern)?;
    ensure!(
        pattern.split('/').next() != Some(".cps"),
        "reserved image path {pattern}"
    );
    ensure!(
        !crate::model::under_var(&pattern),
        "collect rule {pattern} is under /var: {}",
        crate::model::VAR_ADVICE
    );
    let mode = fields.string("mode")?;
    if let Some(mode) = &mode {
        parse_mode(mode)?;
    }
    let facet = match fields.properties.get("facet") {
        None => FacetChoice::Default,
        Some(value) if value.is_null() => FacetChoice::None,
        Some(value) => {
            let facet = value
                .as_string()
                .context("facet must be a string or #null")?
                .to_owned();
            validate_facet(&facet)?;
            FacetChoice::Facet(facet)
        }
    };
    let variants = fields.variants()?;
    for (name, value) in &variants {
        validate_variant(name, value)?;
    }
    let owner = |key| -> Result<u32> {
        let id = fields.integer(key)?.unwrap_or(0);
        ensure!(id != u32::MAX, "invalid owner");
        Ok(id)
    };
    Ok(CollectRule {
        mode,
        uid: owner("uid")?,
        gid: owner("gid")?,
        preserve: fields
            .properties
            .get("preserve")
            .map(|value| value.as_bool().context("preserve must be #true or #false"))
            .transpose()?
            .unwrap_or(false),
        facet,
        variants,
        pattern,
    })
}

impl BuildRecipe {
    pub(crate) fn requested(&self) -> Result<BTreeMap<&str, PackageSpec<'_>>> {
        let mut requested = BTreeMap::new();
        for selector in &self.packages {
            let spec = PackageSpec::parse(selector)?;
            if let Some(name) = spec.publisher {
                ensure!(
                    self.toolchain.iter().any(|pin| pin.name == name),
                    "requested toolchain publisher {name} is not pinned"
                );
            }
            let name = spec.name;
            ensure!(
                requested.insert(name, spec).is_none(),
                "multiple selectors for toolchain package {name}"
            );
        }
        Ok(requested)
    }

    pub(crate) fn source_digests(&self) -> BTreeMap<String, FileDigest> {
        self.sources
            .iter()
            .map(|(path, source)| (path.clone(), source.pin.clone()))
            .collect()
    }
}

pub(crate) fn read(path: &Path) -> Result<(Vec<u8>, BuildRecipe)> {
    let bytes = read_limited(File::open(path)?, RECIPE_LIMIT)?;
    let recipe = parse(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("parsing build recipe {}", path.display()))?;
    Ok((bytes, recipe))
}

pub(crate) fn read_with_options(
    path: &Path,
    overrides: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, BuildRecipe)> {
    let (bytes, recipe) = read(path)?;
    for (name, value) in overrides {
        ensure!(
            recipe.options.contains_key(name),
            "build option {name:?} is not declared in the recipe"
        );
        check_option(name, value)?;
    }
    if overrides
        .iter()
        .all(|(name, value)| recipe.options.get(name) == Some(value))
    {
        return Ok((bytes, recipe));
    }

    let mut doc = document(std::str::from_utf8(&bytes)?)?;
    let build = doc.get_mut("build").context("missing build node")?;
    for node in build
        .iter_children_mut()
        .filter(|node| node.name().value() == "option")
    {
        let name = node
            .get(0)
            .and_then(KdlValue::as_string)
            .context("missing build option name")?;
        if let Some(value) = overrides
            .get(name)
            .filter(|value| recipe.options.get(name) != Some(value))
        {
            let entry = &mut node.entries_mut()[1];
            let value = KdlValue::String(value.clone());
            if let Some(format) = entry.format_mut() {
                format.value_repr = value.to_string();
            }
            entry.set_value(value);
        }
    }
    let text = doc.to_string();
    ensure!(
        text.len() as u64 <= RECIPE_LIMIT,
        "effective build recipe exceeds the size limit"
    );
    // The sandbox and the attestation both use the effective document.
    let effective = parse(&text).context("reading the effective build recipe")?;
    Ok((text.into_bytes(), effective))
}

pub(crate) fn snapshot_recipe(
    template: &[u8],
    manifest_sha256: &str,
    archive_name: &str,
    archive: &FileDigest,
) -> Result<String> {
    let mut doc = document(std::str::from_utf8(template)?)?;
    let build = doc.get_mut("build").context("missing build node")?;
    let children = build.ensure_children().nodes_mut();
    children.retain(|node| {
        node.name().value() != "source"
            || node.get(0).and_then(KdlValue::as_string) != Some(archive_name)
    });
    let mut manifests = children
        .iter_mut()
        .filter(|node| node.name().value() == "manifest");
    let manifest = manifests.next().context("missing manifest node")?;
    ensure!(
        manifests.next().is_none(),
        "Git snapshots build exactly one package"
    );
    manifest.entries_mut().clear();
    manifest.push("manifest.kdl");
    manifest.insert("sha256", manifest_sha256);
    let mut source = KdlNode::new("source");
    source.push(archive_name);
    source.insert("size", i128::from(archive.size));
    source.insert("sha256", archive.sha256.as_str());
    children.push(source);
    let text = doc.to_string();
    ensure!(
        text.len() as u64 <= RECIPE_LIMIT,
        "snapshot build recipe exceeds the size limit"
    );
    parse(&text).context("validating the snapshot build recipe")?;
    Ok(text)
}

/// Set the package version and upstream version of a template, keeping its
/// other nodes, such as `collect` rules, as written.
pub(crate) fn set_template_version(
    template: &[u8],
    version: &Version,
    upstream: &str,
    recipe: &BuildRecipe,
) -> Result<String> {
    let mut doc = document(std::str::from_utf8(template)?)?;
    let package = doc.get_mut("package").context("missing package node")?;
    package.insert("version", version.to_string());
    package.insert("upstream-version", upstream);
    let text = doc.to_string();
    parse_template(&text, recipe).context("validating the snapshot manifest")?;
    Ok(text)
}

pub(crate) fn read_toolchain_config(path: &Path) -> Result<BTreeMap<String, PublisherConfig>> {
    let bytes = read_limited(File::open(path)?, RECIPE_LIMIT)?;
    let parent = std::fs::canonicalize(path)?
        .parent()
        .context("missing toolchain configuration directory")?
        .to_owned();
    let doc = document(std::str::from_utf8(&bytes)?)?;
    schema(&doc, "toolchain-config")?;
    let mut publishers = BTreeMap::new();
    for node in doc.nodes() {
        match node.name().value() {
            "schema-version" => {}
            "publisher" => {
                let name = Fields::read(node, 1, &[], true)?.string_arg(0)?;
                let config = publisher_config(node, name.clone(), &parent)?;
                ensure!(
                    publishers.insert(name.clone(), config).is_none(),
                    "duplicate toolchain publisher {name}"
                );
            }
            name => bail!("unknown toolchain configuration node {name}"),
        }
    }
    ensure!(!publishers.is_empty(), "missing toolchain publisher");
    Ok(publishers)
}

pub(crate) fn parse(text: &str) -> Result<BuildRecipe> {
    let doc = document(text)?;
    schema(&doc, RECIPE_KIND)?;
    ensure!(
        doc.nodes().len() == 2,
        "expected schema-version and build nodes"
    );
    let build = one(&doc, "build")?;
    let fields = Fields::read(build, 0, &["architecture", "epoch"], true)?;
    let arch = fields.required_string("architecture")?;
    validate_publisher(&arch).context("invalid build architecture")?;
    let epoch = fields.integer("epoch")?.context("missing build epoch")?;
    let children = build.children().context("build requires a child block")?;

    let toolchain = one(children, "toolchain")?;
    let fields = Fields::read(toolchain, 0, &["solver"], true)?;
    ensure!(
        fields.required_string("solver")? == "sat-v2",
        "unsupported toolchain solver, expected sat-v2"
    );
    let mut packages = BTreeSet::new();
    let mut pins = Vec::new();
    let mut publishers = BTreeSet::new();
    for node in toolchain
        .children()
        .into_iter()
        .flat_map(KdlDocument::nodes)
    {
        match node.name().value() {
            "package" => {
                let name = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                ensure!(
                    packages.insert(name.clone()),
                    "duplicate toolchain package {name}"
                );
            }
            "publisher" => {
                let fields = Fields::read(node, 1, &["catalog", "key-sha256"], false)?;
                let name = fields.string_arg(0)?;
                validate_publisher(&name)?;
                ensure!(
                    publishers.insert(name.clone()),
                    "duplicate toolchain publisher {name}"
                );
                let catalog_sha256 = fields.required_string("catalog")?;
                validate_digest(&catalog_sha256).context("invalid toolchain catalog")?;
                let key_sha256 = fields.required_string("key-sha256")?;
                validate_digest(&key_sha256).context("invalid toolchain key digest")?;
                pins.push(PublisherPin {
                    name,
                    catalog_sha256,
                    key_sha256,
                });
            }
            name => bail!("unexpected toolchain node {name}"),
        }
    }
    ensure!(
        !packages.is_empty(),
        "build requires at least one toolchain package"
    );
    ensure!(
        !pins.is_empty(),
        "build requires a pinned toolchain publisher"
    );

    let script = Fields::read(one(children, "script")?, 1, &[], false)?.string_arg(0)?;
    ensure!(
        !script.trim().is_empty() && !script.contains('\0'),
        "invalid build script"
    );

    let mut sources = BTreeMap::new();
    let mut options = BTreeMap::new();
    let mut manifests = Vec::<TemplatePin>::new();
    let mut discard = Vec::new();
    let mut version = None;
    let mut strip = None;
    for node in children.nodes() {
        match node.name().value() {
            "toolchain" | "script" => {}
            "manifest" => {
                let fields = Fields::read(node, 1, &["sha256"], false)?;
                let path = fields.string_arg(0)?;
                validate_path(&path)?;
                let sha256 = fields.required_string("sha256")?;
                validate_digest(&sha256).context("invalid manifest digest")?;
                ensure!(
                    manifests.iter().all(|pin| pin.path != path),
                    "duplicate manifest {path}"
                );
                manifests.push(TemplatePin { path, sha256 });
            }
            "version" => {
                ensure!(version.is_none(), "duplicate version node");
                let fields = Fields::read(node, 1, &["upstream"], false)?;
                let upstream = fields.string("upstream")?;
                version = Some(RecipeVersion {
                    version: fields.string_arg(0)?.parse()?,
                    upstream,
                });
            }
            "discard" => {
                let pattern = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                pattern::validate(&pattern)?;
                ensure!(
                    !discard.contains(&pattern),
                    "duplicate discard pattern {pattern}"
                );
                discard.push(pattern);
            }
            "strip" => {
                ensure!(strip.is_none(), "duplicate strip node");
                let fields = Fields::read(node, 1, &[], false)?;
                strip = Some(
                    fields.args[0]
                        .as_bool()
                        .context("strip must be #true or #false")?,
                );
            }
            "option" => {
                let fields = Fields::read(node, 2, &[], false)?;
                let name = fields.string_arg(0)?;
                let value = fields.string_arg(1)?;
                check_option(&name, &value)?;
                ensure!(
                    options.insert(name.clone(), value).is_none(),
                    "duplicate build option {name}"
                );
            }
            "source" => {
                let fields = Fields::read(node, 1, &["sha256", "size", "url"], false)?;
                let path = fields.string_arg(0)?;
                validate_path(&path)?;
                let sha256 = fields.required_string("sha256")?;
                validate_digest(&sha256).with_context(|| format!("invalid digest for {path}"))?;
                let size = fields.integer("size")?.context("missing source size")?;
                let url = fields
                    .string("url")?
                    .map(|value| source_url(&value))
                    .transpose()?;
                ensure!(
                    sources
                        .insert(
                            path.clone(),
                            Source {
                                pin: FileDigest { sha256, size },
                                url
                            }
                        )
                        .is_none(),
                    "duplicate source {path}"
                );
            }
            name => bail!("unknown build node {name}"),
        }
    }
    let mut sizes = BTreeMap::new();
    for (path, source) in &sources {
        for (index, _) in path.match_indices('/') {
            ensure!(
                !sources.contains_key(&path[..index]),
                "source paths overlap at {}",
                &path[..index]
            );
        }
        let size = sizes.entry(&source.pin.sha256).or_insert(source.pin.size);
        ensure!(
            *size == source.pin.size,
            "conflicting source sizes for {}",
            source.pin.sha256
        );
    }
    ensure!(
        !manifests.is_empty(),
        "build requires at least one manifest node"
    );
    ensure!(
        manifests.len() <= MAX_OUTPUTS,
        "a build can produce at most {MAX_OUTPUTS} packages"
    );
    let recipe = BuildRecipe {
        arch,
        epoch,
        toolchain: pins,
        packages,
        version,
        manifests,
        discard,
        strip: strip.unwrap_or(true),
        sources,
        options,
        script,
    };
    recipe.requested()?;
    Ok(recipe)
}

fn check_option(name: &str, value: &str) -> Result<()> {
    ensure!(
        name != "JOBS",
        "build option JOBS is reserved; recipes read the worker's job count from CPS_BUILD_JOBS"
    );
    ensure!(
        name.len() <= 64
            && name.starts_with(|c: char| c.is_ascii_uppercase())
            && name
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_'),
        "invalid build option name {name:?}; use uppercase ASCII letters, digits, and underscores"
    );
    ensure!(
        !value.contains('\0'),
        "build option {name} contains a NUL character"
    );
    ensure!(
        value.len() <= 16 * 1024,
        "build option {name} exceeds 16 KiB"
    );
    Ok(())
}

fn source_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid source URL")?;
    ensure!(
        url.scheme() == "https"
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "source URL must use HTTPS without credentials or a fragment"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_definition_extracts_a_version_one_manifest() -> Result<()> {
        let (manifest, recipe) = git_document(
            r#"schema-version 1 kind="git-package"
package "test/example" version="1.0.0-0"
build architecture="x86_64" epoch=1700000000 {
    toolchain "default"
    script "true"
}
"#,
            "local",
        )?;
        assert_eq!(manifest.format, 1);
        assert_eq!(manifest.publisher, "local");
        schema(&document(&recipe)?, RECIPE_KIND)?;
        Ok(())
    }

    #[test]
    fn effective_recipe_pins_option_overrides_and_preserves_default_bytes() -> Result<()> {
        let work = tempfile::tempdir()?;
        let path = work.path().join("example.build.kdl");
        let recipe = format!(
            r#"schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v2" {{
        publisher "seed" catalog="{digest}" key-sha256="{digest}"
        package "devel/base"
    }}
    manifest "nested/hello.kdl" sha256="{digest}"
    option "CFLAGS" "-O2" // compiler flags
    script "true"
}}
"#,
            digest = "0".repeat(64),
        );
        assert!(parse(&recipe.replace("sat-v2", "sat-v1")).is_err());
        assert!(parse(&recipe.replace(" solver=\"sat-v2\"", "")).is_err());
        std::fs::write(&path, &recipe)?;
        let options = |name: &str, value: &str| BTreeMap::from([(name.into(), value.into())]);
        let (bytes, parsed) = read_with_options(&path, &options("CFLAGS", "-O2"))?;
        assert_eq!(bytes, recipe.as_bytes());
        assert_eq!(parsed.options["CFLAGS"], "-O2");

        let flags = "-O3 -DNAME=\"a b\" -DKEY=x=y\n\\literal";
        let (bytes, effective) = read_with_options(&path, &options("CFLAGS", flags))?;
        assert_eq!(effective.options["CFLAGS"], flags);
        assert_eq!(effective.manifests[0].path, "nested/hello.kdl");
        assert!(std::str::from_utf8(&bytes)?.contains("// compiler flags"));
        assert_eq!(std::fs::read(&path)?, recipe.as_bytes());
        assert_eq!(
            parse(std::str::from_utf8(&bytes)?)?.options,
            effective.options
        );

        assert!(read_with_options(&path, &options("RUSTFLAGS", "-g")).is_err());
        assert!(read_with_options(&path, &options("CFLAGS", "\0")).is_err());
        assert!(parse(&recipe.replace("CFLAGS", "CFLAGS=PATH")).is_err());
        assert!(parse(&recipe.replace("script", "option \"CFLAGS\" \"-O0\"; script")).is_err());
        Ok(())
    }

    #[test]
    fn source_urls_require_https_without_credentials() {
        assert!(source_url("https://example.org/source.tar.gz").is_ok());
        for invalid in [
            "http://example.org/source",
            "file:///source",
            "https://user@example.org/source",
            "https://example.org/source#fragment",
        ] {
            assert!(source_url(invalid).is_err(), "accepted {invalid}");
        }
    }
}
