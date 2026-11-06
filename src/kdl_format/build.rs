use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlValue};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::model::{
    FileDigest, PackageSpec, validate_digest, validate_name, validate_path, validate_publisher,
};
use crate::plan::SolverRevision;
use crate::repo::read_limited;

use super::{Fields, PublisherConfig, document, one, publisher_config, schema};

pub(crate) const RECIPE_LIMIT: u64 = 1024 * 1024;

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

pub(crate) enum ToolchainPins {
    Single(String),
    Publishers(Vec<PublisherPin>),
}

pub(crate) struct BuildRecipe {
    pub solver: Option<SolverRevision>,
    pub arch: String,
    pub epoch: u32,
    pub toolchain: ToolchainPins,
    pub packages: BTreeSet<String>,
    pub manifest: String,
    pub manifest_digest: String,
    pub sources: BTreeMap<String, Source>,
    pub options: BTreeMap<String, String>,
    pub script: String,
}

impl BuildRecipe {
    pub(crate) fn requested(&self) -> Result<BTreeMap<&str, PackageSpec<'_>>> {
        let mut requested = BTreeMap::new();
        for selector in &self.packages {
            let spec = PackageSpec::parse(selector)?;
            if let ToolchainPins::Publishers(pins) = &self.toolchain
                && let Some(name) = spec.publisher
            {
                ensure!(
                    pins.iter().any(|pin| pin.name == name),
                    "requested toolchain publisher {name} is not pinned"
                );
            }
            ensure!(
                requested.insert(spec.name, spec).is_none(),
                "multiple selectors for toolchain package {}",
                spec.name
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

pub(crate) fn read_toolchain_config(path: &Path) -> Result<BTreeMap<String, PublisherConfig>> {
    let bytes = read_limited(File::open(path)?, RECIPE_LIMIT)?;
    let parent = std::fs::canonicalize(path)?
        .parent()
        .context("missing toolchain configuration directory")?
        .to_owned();
    let doc = document(std::str::from_utf8(&bytes)?)?;
    schema(&doc)?;
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
    schema(&doc)?;
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
    let legacy = toolchain
        .entries()
        .iter()
        .any(|entry| entry.name().is_none());
    let fields = Fields::read(toolchain, usize::from(legacy), &["solver"], true)?;
    let solver = match fields.string("solver")?.as_deref() {
        None => None,
        Some("sat-v1") => Some(SolverRevision::SatV1),
        Some(name) => bail!("unsupported toolchain solver {name:?}"),
    };
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
                if legacy {
                    validate_name(&name)?;
                }
                ensure!(
                    packages.insert(name.clone()),
                    "duplicate toolchain package {name}"
                );
            }
            "publisher" if !legacy => {
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
    let toolchain = if legacy {
        let catalog = fields.string_arg(0)?;
        validate_digest(&catalog).context("invalid toolchain catalog")?;
        ToolchainPins::Single(catalog)
    } else {
        ensure!(
            !pins.is_empty(),
            "build requires a pinned toolchain publisher"
        );
        ensure!(
            solver.is_some(),
            "publisher pins require an explicit toolchain solver"
        );
        ToolchainPins::Publishers(pins)
    };

    let fields = Fields::read(one(children, "manifest")?, 1, &["sha256"], false)?;
    let manifest = fields.string_arg(0)?;
    validate_path(&manifest)?;
    let manifest_digest = fields.required_string("sha256")?;
    validate_digest(&manifest_digest).context("invalid manifest digest")?;

    let script = Fields::read(one(children, "script")?, 1, &[], false)?.string_arg(0)?;
    ensure!(
        !script.trim().is_empty() && !script.contains('\0'),
        "invalid build script"
    );

    let mut sources = BTreeMap::new();
    let mut options = BTreeMap::new();
    for node in children.nodes() {
        match node.name().value() {
            "toolchain" | "manifest" | "script" => {}
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
    let recipe = BuildRecipe {
        solver,
        arch,
        epoch,
        toolchain,
        packages,
        manifest,
        manifest_digest,
        sources,
        options,
        script,
    };
    recipe.requested()?;
    Ok(recipe)
}

fn check_option(name: &str, value: &str) -> Result<()> {
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
    fn effective_recipe_pins_option_overrides_and_preserves_default_bytes() -> Result<()> {
        let work = tempfile::tempdir()?;
        let path = work.path().join("example.build.kdl");
        let recipe = format!(
            r#"schema-version 1
build architecture="x86_64" epoch=1700000000 {{
    toolchain "{digest}" {{ package "devel/base"; }}
    manifest "nested/hello.kdl" sha256="{digest}"
    option "CFLAGS" "-O2" // compiler flags
    script "true"
}}
"#,
            digest = "0".repeat(64),
        );
        std::fs::write(&path, &recipe)?;
        let options = |name: &str, value: &str| BTreeMap::from([(name.into(), value.into())]);
        let (bytes, parsed) = read_with_options(&path, &options("CFLAGS", "-O2"))?;
        assert_eq!(bytes, recipe.as_bytes());
        assert_eq!(parsed.options["CFLAGS"], "-O2");

        let flags = "-O3 -DNAME=\"a b\" -DKEY=x=y\n\\literal";
        let (bytes, effective) = read_with_options(&path, &options("CFLAGS", flags))?;
        assert_eq!(effective.options["CFLAGS"], flags);
        assert_eq!(effective.manifest, "nested/hello.kdl");
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
