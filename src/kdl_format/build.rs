use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use kdl::KdlDocument;
use url::Url;

use crate::model::{FileDigest, validate_digest, validate_name, validate_path, validate_publisher};
use crate::plan::SolverRevision;
use crate::repo::read_limited;

use super::{Fields, document, one, schema};

pub(crate) const RECIPE_LIMIT: u64 = 1024 * 1024;

pub(crate) struct Source {
    pub pin: FileDigest,
    pub url: Option<Url>,
}

pub(crate) struct BuildRecipe {
    pub solver: Option<SolverRevision>,
    pub arch: String,
    pub epoch: u32,
    pub catalog: String,
    pub packages: BTreeSet<String>,
    pub manifest: String,
    pub manifest_digest: String,
    pub sources: BTreeMap<String, Source>,
    pub script: String,
}

impl BuildRecipe {
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
    let fields = Fields::read(toolchain, 1, &["solver"], true)?;
    let catalog = fields.string_arg(0)?;
    let solver = match fields.string("solver")?.as_deref() {
        None => None,
        Some("sat-v1") => Some(SolverRevision::SatV1),
        Some(name) => bail!("unsupported toolchain solver {name:?}"),
    };
    validate_digest(&catalog).context("invalid toolchain catalog")?;
    let mut packages = BTreeSet::new();
    for node in toolchain
        .children()
        .into_iter()
        .flat_map(KdlDocument::nodes)
    {
        ensure!(
            node.name().value() == "package",
            "expected a toolchain package"
        );
        let name = Fields::read(node, 1, &[], false)?.string_arg(0)?;
        validate_name(&name)?;
        ensure!(
            packages.insert(name.clone()),
            "duplicate toolchain package {name}"
        );
    }
    ensure!(
        !packages.is_empty(),
        "build requires at least one toolchain package"
    );

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
    for node in children.nodes() {
        match node.name().value() {
            "toolchain" | "manifest" | "script" => {}
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
    Ok(BuildRecipe {
        solver,
        arch,
        epoch,
        catalog,
        packages,
        manifest,
        manifest_digest,
        sources,
        script,
    })
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
