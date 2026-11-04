use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::{
    boot::kernel::Kernel,
    model::{Action, Dependency, DependencyTarget, FORMAT, Manifest, validate_publisher},
    systemd::Action as SystemdAction,
};

pub(crate) mod build;
pub(crate) mod publish_policy;
pub(crate) mod worker;

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
        systemd: Vec::new(),
        kernel: None,
    };
    for node in package.children().into_iter().flat_map(KdlDocument::nodes) {
        let name = node.name().value();
        match name {
            "require" | "incorporate" | "exclude" | "require-any" | "conditional" => manifest
                .dependency
                .push(parse_dependency(node).with_context(|| format!("{name} dependency"))?),
            "file" | "dir" | "symlink" | "hardlink" => manifest
                .action
                .push(parse_action(node).with_context(|| format!("{name} action"))?),
            "sysusers" | "tmpfiles" | "preset" => {
                let value = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                let action = match name {
                    "sysusers" => SystemdAction::Sysusers(value),
                    "tmpfiles" => SystemdAction::Tmpfiles(value),
                    _ => SystemdAction::Preset(value),
                };
                manifest.systemd.push(action);
            }
            "kernel-install" => {
                ensure!(manifest.kernel.is_none(), "duplicate kernel-install node");
                let fields = Fields::read(node, 1, &["initrd"], false)?;
                let initrd = fields
                    .properties
                    .get("initrd")
                    .map(|value| value.as_bool().context("initrd must be #true or #false"))
                    .transpose()?
                    .unwrap_or(false);
                manifest.kernel = Some(Kernel {
                    release: fields.string_arg(0)?,
                    initrd,
                });
            }
            _ => bail!("unknown package node {name}"),
        }
    }
    Ok(manifest)
}

fn parse_target(node: &KdlNode) -> Result<DependencyTarget> {
    let fields = Fields::read(node, 1, &["version"], false)?;
    Ok(DependencyTarget {
        package: fields.string_arg(0)?,
        version: fields.string("version")?,
    })
}

fn parse_dependency(node: &KdlNode) -> Result<Dependency> {
    let dependency = match node.name().value() {
        "require" => Dependency::Require(parse_target(node)?),
        "incorporate" => Dependency::Incorporate(parse_target(node)?),
        "exclude" => Dependency::Exclude(parse_target(node)?),
        "require-any" => {
            Fields::read(node, 0, &[], true)?;
            let alternatives = node
                .children()
                .context("require-any needs a child block")?
                .nodes()
                .iter()
                .map(|child| {
                    ensure!(
                        child.name().value() == "package",
                        "expected a package alternative"
                    );
                    parse_target(child)
                })
                .collect::<Result<_>>()?;
            Dependency::RequireAny { alternatives }
        }
        "conditional" => {
            let fields = Fields::read(node, 1, &["version", "when", "when-version"], false)?;
            Dependency::Conditional {
                target: DependencyTarget {
                    package: fields.string_arg(0)?,
                    version: fields.string("version")?,
                },
                predicate: DependencyTarget {
                    package: fields.required_string("when")?,
                    version: fields.string("when-version")?,
                },
            }
        }
        name => bail!("unknown dependency {name}"),
    };
    Ok(dependency)
}

fn parse_action(node: &KdlNode) -> Result<Action> {
    let name = node.name().value();
    let props: &[&str] = match name {
        "file" => &[
            "mode",
            "uid",
            "gid",
            "arch",
            "facet",
            "preserve",
            "sha256",
            "size",
            "capabilities",
        ],
        "dir" => &["mode", "uid", "gid", "arch", "facet"],
        "hardlink" => &["target", "arch", "facet"],
        _ => &["target", "uid", "gid", "arch", "facet"],
    };
    let fields = Fields::read(node, 1, props, false)?;
    let path = fields.string_arg(0)?;
    let arch = fields.string("arch")?;
    let facet = fields.string("facet")?;
    if name == "hardlink" {
        return Ok(Action::Hardlink {
            path,
            target: fields.required_string("target")?,
            arch,
            facet,
        });
    }
    let uid = fields.owner("uid")?;
    let gid = fields.owner("gid")?;
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
            capabilities: fields
                .string("capabilities")?
                .as_deref()
                .map(crate::file_caps::FileCapabilities::parse)
                .transpose()?,
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

fn target_node(name: &str, target: &DependencyTarget) -> KdlNode {
    let mut node = KdlNode::new(name);
    node.push(target.package.as_str());
    if let Some(version) = &target.version {
        node.insert("version", version.as_str());
    }
    node
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
        children.push(match dependency {
            Dependency::Require(target) => target_node("require", target),
            Dependency::Incorporate(target) => target_node("incorporate", target),
            Dependency::Exclude(target) => target_node("exclude", target),
            Dependency::RequireAny { alternatives } => {
                let mut node = KdlNode::new("require-any");
                node.ensure_children().nodes_mut().extend(
                    alternatives
                        .iter()
                        .map(|target| target_node("package", target)),
                );
                node
            }
            Dependency::Conditional { target, predicate } => {
                let mut node = target_node("conditional", target);
                node.insert("when", predicate.package.as_str());
                if let Some(version) = &predicate.version {
                    node.insert("when-version", version.as_str());
                }
                node
            }
        });
    }
    for action in &manifest.action {
        let mut node = KdlNode::new(match action {
            Action::File { .. } => "file",
            Action::Dir { .. } => "dir",
            Action::Symlink { .. } => "symlink",
            Action::Hardlink { .. } => "hardlink",
        });
        node.push(action.path());
        let owner = match action {
            Action::File {
                uid,
                gid,
                mode,
                preserve,
                sha256,
                size,
                capabilities,
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
                if let Some(capabilities) = capabilities {
                    node.insert("capabilities", capabilities.to_string());
                }
                Some((*uid, *gid))
            }
            Action::Dir { uid, gid, mode, .. } => {
                node.insert("mode", mode.as_str());
                Some((*uid, *gid))
            }
            Action::Symlink {
                uid, gid, target, ..
            } => {
                node.insert("target", target.as_str());
                Some((*uid, *gid))
            }
            Action::Hardlink { target, .. } => {
                node.insert("target", target.as_str());
                None
            }
        };
        if let Some((uid, gid)) = owner {
            node.insert("uid", i128::from(uid));
            node.insert("gid", i128::from(gid));
        }
        let (facet, arch) = action.selectors();
        if let Some(facet) = facet {
            node.insert("facet", facet);
        }
        if let Some(arch) = arch {
            node.insert("arch", arch);
        }
        children.push(node);
    }
    for effect in &manifest.systemd {
        let mut node = KdlNode::new(effect.kind());
        node.push(effect.name());
        children.push(node);
    }
    if let Some(kernel) = &manifest.kernel {
        let mut node = KdlNode::new("kernel-install");
        node.push(kernel.release.as_str());
        if kernel.initrd {
            node.insert("initrd", true);
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
    let ImageDefinition {
        mut publishers,
        arch,
        facets,
    } = read_image_definition(path)?;
    ensure!(
        publishers.len() == 1,
        "use create_configured_image for multiple publishers"
    );
    let publisher = publishers.pop().expect("one publisher");
    Ok(ImageConfig {
        publisher: publisher.name,
        repository: publisher.repository,
        trust_key: publisher.trust_key,
        arch,
        facets,
    })
}

pub(crate) struct PublisherConfig {
    pub name: String,
    pub repository: PathBuf,
    pub trust_key: PathBuf,
}

pub(crate) struct ImageDefinition {
    pub publishers: Vec<PublisherConfig>,
    pub arch: String,
    pub facets: BTreeMap<String, bool>,
}

pub(crate) fn read_image_definition(path: &Path) -> Result<ImageDefinition> {
    let input = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parent = fs::canonicalize(path)?
        .parent()
        .context("missing config directory")?
        .to_owned();
    parse_image_config(&input, &parent).with_context(|| format!("parsing {}", path.display()))
}

fn parse_image_config(text: &str, parent: &Path) -> Result<ImageDefinition> {
    let doc = document(text)?;
    schema(&doc)?;
    let mut config = ImageDefinition {
        publishers: Vec::new(),
        arch: std::env::consts::ARCH.to_owned(),
        facets: BTreeMap::new(),
    };
    let mut architecture = false;
    let mut names = BTreeSet::new();
    for node in doc.nodes() {
        match node.name().value() {
            "schema-version" => {}
            "publisher" => {
                let name = Fields::read(node, 1, &[], true)?.string_arg(0)?;
                validate_publisher(&name)?;
                ensure!(names.insert(name.clone()), "duplicate publisher {name}");
                let children = node
                    .children()
                    .context("publisher requires a configuration block")?;
                let repo =
                    Fields::read(one(children, "repository")?, 1, &[], false)?.string_arg(0)?;
                let key =
                    Fields::read(one(children, "trust-key")?, 1, &[], false)?.string_arg(0)?;
                ensure!(
                    children.nodes().len() == 2,
                    "expected repository and trust-key under publisher"
                );
                config.publishers.push(PublisherConfig {
                    name,
                    repository: parent.join(repo),
                    trust_key: parent.join(key),
                });
            }
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
    ensure!(!config.publishers.is_empty(), "missing publisher node");
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
        assert_eq!(
            parsed.publishers[0].repository,
            Path::new("/tmp/config/../repo")
        );
        assert_eq!(
            parsed.publishers[0].trust_key,
            Path::new("/etc/keys/canopa.der")
        );
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
