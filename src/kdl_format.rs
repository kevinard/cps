use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::model::{Action, Dependency, DependencyKind, FORMAT, Manifest};

struct Fields<'a> {
    args: Vec<&'a KdlValue>,
    properties: BTreeMap<&'a str, &'a KdlValue>,
}

impl<'a> Fields<'a> {
    fn read(node: &'a KdlNode, count: usize, properties: &[&str], children: bool) -> Result<Self> {
        let name = node.name().value();
        ensure!(
            node.ty().is_none(),
            "type annotations are unsupported on {name}"
        );
        ensure!(
            children || node.children().is_none(),
            "{name} does not take a child block"
        );
        let mut fields = Self {
            args: Vec::new(),
            properties: BTreeMap::new(),
        };
        for entry in node.entries() {
            ensure!(
                entry.ty().is_none(),
                "type annotations are unsupported on {name} values"
            );
            if let Some(key) = entry.name().map(|key| key.value()) {
                ensure!(properties.contains(&key), "unknown {name} property {key}");
                ensure!(
                    fields.properties.insert(key, entry.value()).is_none(),
                    "duplicate {name} property {key}"
                );
            } else {
                fields.args.push(entry.value());
            }
        }
        ensure!(
            fields.args.len() == count,
            "{name} expects {count} positional arguments"
        );
        Ok(fields)
    }

    fn string_arg(&self, index: usize) -> Result<String> {
        self.args[index]
            .as_string()
            .map(str::to_owned)
            .context("expected a string argument")
    }

    fn string(&self, key: &str) -> Result<Option<String>> {
        self.properties
            .get(key)
            .map(|value| {
                value
                    .as_string()
                    .map(str::to_owned)
                    .with_context(|| format!("{key} must be a string"))
            })
            .transpose()
    }

    fn required_string(&self, key: &str) -> Result<String> {
        self.string(key)?
            .with_context(|| format!("missing {key} property"))
    }

    fn integer<T: TryFrom<i128>>(&self, key: &str) -> Result<Option<T>> {
        self.properties
            .get(key)
            .map(|value| {
                value
                    .as_integer()
                    .and_then(|value| T::try_from(value).ok())
                    .with_context(|| format!("{key} must be an unsigned integer in range"))
            })
            .transpose()
    }

    fn owner(&self, key: &str) -> Result<u32> {
        self.integer(key)?
            .with_context(|| format!("missing {key} property"))
    }
}

fn schema(doc: &KdlDocument) -> Result<()> {
    let node = one(doc, "schema-version")?;
    let fields = Fields::read(node, 1, &[], false)?;
    ensure!(
        fields.args[0].as_integer() == Some(i128::from(FORMAT)),
        "expected schema-version {FORMAT}"
    );
    Ok(())
}

fn one<'a>(doc: &'a KdlDocument, name: &str) -> Result<&'a KdlNode> {
    let mut nodes = doc
        .nodes()
        .iter()
        .filter(|node| node.name().value() == name);
    let node = nodes
        .next()
        .with_context(|| format!("missing {name} node"))?;
    ensure!(nodes.next().is_none(), "duplicate {name} node");
    Ok(node)
}

fn document(text: &str) -> Result<KdlDocument> {
    text.parse()
        .map_err(|error: kdl::KdlError| anyhow::anyhow!("invalid KDL: {error:?}"))
}

pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let doc = document(text)?;
    schema(&doc)?;
    let package = one(&doc, "package")?;
    ensure!(
        doc.nodes().len() == 2,
        "expected only schema-version and package nodes"
    );
    let fields = Fields::read(package, 1, &["publisher", "version"], true)?;
    let mut manifest = Manifest {
        format: FORMAT,
        name: fields.string_arg(0)?,
        publisher: fields.required_string("publisher")?,
        version: fields.required_string("version")?.parse()?,
        dependency: Vec::new(),
        action: Vec::new(),
    };
    for node in package.children().into_iter().flat_map(KdlDocument::nodes) {
        let name = node.name().value();
        match name {
            "require" | "incorporate" => {
                let fields = Fields::read(node, 1, &["version"], false).context(name.to_owned())?;
                let kind = if name == "require" {
                    DependencyKind::Require
                } else {
                    DependencyKind::Incorporate
                };
                manifest.dependency.push(Dependency {
                    kind,
                    package: fields.string_arg(0)?,
                    version: fields.string("version")?,
                });
            }
            "file" | "dir" | "symlink" => manifest
                .action
                .push(parse_action(node).with_context(|| format!("{name} action"))?),
            _ => bail!("unknown package node {name}"),
        }
    }
    Ok(manifest)
}

fn parse_action(node: &KdlNode) -> Result<Action> {
    let name = node.name().value();
    let props: &[&str] = match name {
        "file" => &[
            "mode", "uid", "gid", "arch", "facet", "preserve", "sha256", "size",
        ],
        "dir" => &["mode", "uid", "gid", "arch", "facet"],
        _ => &["target", "uid", "gid", "arch", "facet"],
    };
    let fields = Fields::read(node, 1, props, false)?;
    let path = fields.string_arg(0)?;
    let uid = fields.owner("uid")?;
    let gid = fields.owner("gid")?;
    let arch = fields.string("arch")?;
    let facet = fields.string("facet")?;
    Ok(match name {
        "file" => Action::File {
            path,
            uid,
            gid,
            arch,
            facet,
            mode: fields.required_string("mode")?,
            preserve: fields
                .properties
                .get("preserve")
                .map(|value| value.as_bool().context("preserve must be #true or #false"))
                .transpose()?
                .unwrap_or(false),
            sha256: fields.string("sha256")?,
            size: fields.integer("size")?,
        },
        "dir" => Action::Dir {
            path,
            uid,
            gid,
            arch,
            facet,
            mode: fields.required_string("mode")?,
        },
        _ => Action::Symlink {
            path,
            uid,
            gid,
            arch,
            facet,
            target: fields.required_string("target")?,
        },
    })
}

pub fn manifest_text(manifest: &Manifest) -> String {
    let mut doc = KdlDocument::new();
    let mut schema = KdlNode::new("schema-version");
    schema.push(i128::from(manifest.format));
    doc.nodes_mut().push(schema);
    let mut package = KdlNode::new("package");
    package.push(manifest.name.as_str());
    package.insert("publisher", manifest.publisher.as_str());
    package.insert("version", manifest.version.to_string());
    let children = package.ensure_children().nodes_mut();
    for dependency in &manifest.dependency {
        let mut node = KdlNode::new(match dependency.kind {
            DependencyKind::Require => "require",
            DependencyKind::Incorporate => "incorporate",
        });
        node.push(dependency.package.as_str());
        if let Some(version) = &dependency.version {
            node.insert("version", version.as_str());
        }
        children.push(node);
    }
    for action in &manifest.action {
        let mut node = KdlNode::new(match action {
            Action::File { .. } => "file",
            Action::Dir { .. } => "dir",
            Action::Symlink { .. } => "symlink",
        });
        node.push(action.path());
        let (uid, gid) = match action {
            Action::File {
                uid,
                gid,
                mode,
                preserve,
                sha256,
                size,
                ..
            } => {
                node.insert("mode", mode.as_str());
                if *preserve {
                    node.insert("preserve", true);
                }
                if let Some(sha256) = sha256 {
                    node.insert("sha256", sha256.as_str());
                }
                if let Some(size) = size {
                    node.insert("size", i128::from(*size));
                }
                (*uid, *gid)
            }
            Action::Dir { uid, gid, mode, .. } => {
                node.insert("mode", mode.as_str());
                (*uid, *gid)
            }
            Action::Symlink {
                uid, gid, target, ..
            } => {
                node.insert("target", target.as_str());
                (*uid, *gid)
            }
        };
        node.insert("uid", i128::from(uid));
        node.insert("gid", i128::from(gid));
        let (facet, arch) = action.selectors();
        if let Some(facet) = facet {
            node.insert("facet", facet);
        }
        if let Some(arch) = arch {
            node.insert("arch", arch);
        }
        children.push(node);
    }
    doc.nodes_mut().push(package);
    doc.autoformat();
    doc.to_string()
}

pub struct ImageConfig {
    pub publisher: String,
    pub repository: PathBuf,
    pub trust_key: PathBuf,
    pub arch: String,
    pub facets: BTreeMap<String, bool>,
}

pub fn read_image_config(path: &Path) -> Result<ImageConfig> {
    let input = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parent = fs::canonicalize(path)?
        .parent()
        .context("missing config directory")?
        .to_owned();
    parse_image_config(&input, &parent).with_context(|| format!("parsing {}", path.display()))
}

fn parse_image_config(text: &str, parent: &Path) -> Result<ImageConfig> {
    let doc = document(text)?;
    schema(&doc)?;
    let publisher = one(&doc, "publisher")?;
    let publisher_name = Fields::read(publisher, 1, &[], true)?.string_arg(0)?;
    let publisher = publisher
        .children()
        .context("publisher requires a configuration block")?;
    let repository = Fields::read(one(publisher, "repository")?, 1, &[], false)?.string_arg(0)?;
    let trust_key = Fields::read(one(publisher, "trust-key")?, 1, &[], false)?.string_arg(0)?;
    ensure!(
        publisher.nodes().len() == 2,
        "expected repository and trust-key under publisher"
    );
    let mut config = ImageConfig {
        publisher: publisher_name,
        repository: parent.join(repository),
        trust_key: parent.join(trust_key),
        arch: std::env::consts::ARCH.to_owned(),
        facets: BTreeMap::new(),
    };
    let mut architecture = false;
    for node in doc.nodes() {
        match node.name().value() {
            "schema-version" | "publisher" => {}
            "architecture" => {
                ensure!(!architecture, "duplicate architecture node");
                architecture = true;
                config.arch = Fields::read(node, 1, &[], false)?.string_arg(0)?;
            }
            "facet" => {
                let fields = Fields::read(node, 2, &[], false)?;
                let name = fields.string_arg(0)?;
                let enabled = fields.args[1]
                    .as_bool()
                    .context("facet must be #true or #false")?;
                ensure!(
                    config.facets.insert(name.clone(), enabled).is_none(),
                    "duplicate facet {name}"
                );
            }
            name => bail!("unknown image configuration node {name}"),
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_resolves_paths_and_checks_duplicates() -> Result<()> {
        let config = r#"
            schema-version 1
            publisher "canopa" {
                repository "../repo"
                trust-key "/etc/keys/canopa.der"
            }
            architecture "aarch64"
            facet "doc.man" #false
        "#;
        let parsed = parse_image_config(config, Path::new("/tmp/config"))?;
        assert_eq!(parsed.repository, Path::new("/tmp/config/../repo"));
        assert_eq!(parsed.trust_key, Path::new("/etc/keys/canopa.der"));
        assert_eq!(parsed.arch, "aarch64");
        assert!(!parsed.facets["doc.man"]);
        assert!(
            parse_image_config(
                &format!("{config}\nfacet \"doc.man\" #true\n"),
                Path::new("/tmp")
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn manifest_rejects_duplicate_properties() {
        let input = r#"
            schema-version 1
            package "test/example" publisher="canopa" version="1.0.0-0" {
                file "usr/bin/example" mode="0755" uid=0 uid=1 gid=0
            }
        "#;
        let error = parse_manifest(input).unwrap_err();
        assert!(format!("{error:#}").contains("duplicate file property uid"));
    }
}
