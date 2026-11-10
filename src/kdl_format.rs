use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::{
    attestation::BuildTrust,
    boot::kernel::Kernel,
    model::{
        ARCH_VARIANT, Action, Dependency, DependencyTarget, ImagePolicy, MANIFEST_FORMAT, Manifest,
        NIL_UUID, OptionalDependency, is_uuid, validate_digest, validate_name, validate_publisher,
        validate_variant,
    },
    repo::read_limited,
    systemd::Action as SystemdAction,
};

pub(crate) mod boot_export;
pub(crate) mod build;
pub(crate) mod publish_policy;
pub(crate) mod publish_release;
pub(crate) mod signer;
pub(crate) mod worker;

const SCHEMA_VERSION: u32 = 1;
/// Action properties named `variant.NAME` select by variant.
const VARIANT_PREFIX: &str = "variant.";
const VARIANT_PROPERTY: &str = "variant.*";
const MANIFEST_KIND: &str = "package-manifest";

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
                ensure!(
                    properties.contains(&key)
                        || (key.starts_with(VARIANT_PREFIX)
                            && properties.contains(&VARIANT_PROPERTY)),
                    "unknown {name} property {key}"
                );
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

    /// Read `arch=` and `variant.NAME=` selectors. Values are strings, and `#true` or
    /// `#false` stand for "true" or "false".
    fn variants(&self) -> Result<BTreeMap<String, String>> {
        let mut variants = BTreeMap::new();
        for (&key, value) in &self.properties {
            let name = if key == "arch" {
                ARCH_VARIANT
            } else if let Some(name) = key.strip_prefix(VARIANT_PREFIX) {
                name
            } else {
                continue;
            };
            let value = variant_value(value).with_context(|| format!("invalid {key}"))?;
            ensure!(
                variants.insert(name.to_owned(), value).is_none(),
                "arch and variant.arch select the same variant"
            );
        }
        Ok(variants)
    }

    fn owner(&self, key: &str) -> Result<u32> {
        self.integer(key)?
            .with_context(|| format!("missing {key} property"))
    }
}

/// Whether a facet rule key carries more package scopes than its node declares.
fn name_has_scope(key: &str, package: bool) -> bool {
    key.matches(':').count() > usize::from(package)
}

fn variant_value(value: &KdlValue) -> Result<String> {
    if let Some(value) = value.as_bool() {
        return Ok(value.to_string());
    }
    value
        .as_string()
        .map(str::to_owned)
        .context("variant values must be strings, #true, or #false")
}

fn schema(doc: &KdlDocument, kind: &str) -> Result<()> {
    schema_version(doc, SCHEMA_VERSION, kind)
}

fn schema_version(doc: &KdlDocument, version: u32, kind: &str) -> Result<()> {
    let fields = Fields::read(one(doc, "schema-version")?, 1, &["kind"], false)?;
    ensure!(
        fields.args[0].as_integer() == Some(i128::from(version)),
        "expected schema-version {version} for {kind}"
    );
    ensure!(
        fields.required_string("kind")? == kind,
        "expected schema-version kind={kind:?}"
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
    schema_version(&doc, MANIFEST_FORMAT, MANIFEST_KIND)?;
    let package = one(&doc, "package")?;
    ensure!(
        doc.nodes().len() == 2,
        "expected only schema-version and package nodes"
    );
    let fields = Fields::read(
        package,
        1,
        &[
            "publisher",
            "version",
            "upstream-version",
            "architecture",
            "requires-reboot",
            "summary",
        ],
        true,
    )?;
    let mut manifest = Manifest {
        format: MANIFEST_FORMAT,
        name: fields.string_arg(0)?,
        publisher: fields.required_string("publisher")?,
        version: fields.required_string("version")?.parse()?,
        upstream_version: fields.string("upstream-version")?,
        architecture: fields.string("architecture")?,
        summary: fields.string("summary")?,
        dependency: Vec::new(),
        provides: BTreeMap::new(),
        optional_dependencies: Vec::new(),
        action: Vec::new(),
        mediated_links: BTreeMap::new(),
        systemd: Vec::new(),
        kernel: None,
        requires_reboot: fields
            .properties
            .get("requires-reboot")
            .map(|value| {
                value
                    .as_bool()
                    .context("requires-reboot must be #true or #false")
            })
            .transpose()?
            .unwrap_or(false),
    };
    for node in package.children().into_iter().flat_map(KdlDocument::nodes) {
        let name = node.name().value();
        match name {
            "require" | "require-capability" | "recommends" | "incorporate" | "exclude"
            | "require-any" | "conditional" => manifest
                .dependency
                .push(parse_dependency(node).with_context(|| format!("{name} dependency"))?),
            "provides" => {
                let fields = Fields::read(node, 1, &["version"], false)?;
                let name = fields.string_arg(0)?;
                let version = fields
                    .string("version")?
                    .map(|version| version.parse())
                    .transpose()?;
                ensure!(
                    manifest.provides.insert(name.clone(), version).is_none(),
                    "duplicate provided capability {name}"
                );
            }
            "optional" => {
                reject_selectors(node)?;
                let fields = Fields::read(node, 1, &["version", "description"], false)?;
                manifest.optional_dependencies.push(OptionalDependency {
                    target: DependencyTarget {
                        package: fields.string_arg(0)?,
                        version: fields.string("version")?,
                    },
                    description: fields.required_string("description")?,
                });
            }
            "file" | "dir" | "symlink" | "hardlink" => manifest
                .action
                .push(parse_action(node).with_context(|| format!("{name} action"))?),
            "mediate" => {
                let fields = Fields::read(node, 1, &["group", "priority"], false)?;
                let path = fields.string_arg(0)?;
                let group = fields.required_string("group")?;
                let priority = fields.integer("priority")?.unwrap_or(0);
                ensure!(
                    manifest
                        .mediated_links
                        .insert(path.clone(), crate::model::MediatedLink { group, priority })
                        .is_none(),
                    "duplicate mediation for {path}"
                );
            }
            "sysusers" | "tmpfiles" | "preset" | "hwdb" => {
                let value = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                let action = match name {
                    "sysusers" => SystemdAction::Sysusers(value),
                    "tmpfiles" => SystemdAction::Tmpfiles(value),
                    "hwdb" => SystemdAction::Hwdb(value),
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

/// Refuse facet and variant selectors on dependencies with an explanation.
fn reject_selectors(node: &KdlNode) -> Result<()> {
    let nodes =
        std::iter::once(node).chain(node.children().into_iter().flat_map(KdlDocument::nodes));
    for node in nodes {
        for key in node.entries().iter().filter_map(|entry| entry.name()) {
            let key = key.value();
            ensure!(
                !matches!(key, "facet" | "arch") && !key.starts_with(VARIANT_PREFIX),
                "dependencies cannot carry {key} selectors: facets and variants select files within installed packages and never change which packages are installed"
            );
        }
    }
    Ok(())
}

fn parse_dependency(node: &KdlNode) -> Result<Dependency> {
    reject_selectors(node)?;
    let dependency = match node.name().value() {
        "require" => Dependency::Require(parse_target(node)?),
        "require-capability" => Dependency::RequireCapability(parse_target(node)?),
        "recommends" => Dependency::Recommend(parse_target(node)?),
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
            VARIANT_PROPERTY,
            "preserve",
            "original-name",
            "sha256",
            "size",
            "capabilities",
        ],
        "dir" => &["mode", "uid", "gid", "arch", "facet", VARIANT_PROPERTY],
        "hardlink" => &["target", "arch", "facet", VARIANT_PROPERTY],
        _ => &["target", "uid", "gid", "arch", "facet", VARIANT_PROPERTY],
    };
    let fields = Fields::read(node, 1, props, false)?;
    let path = fields.string_arg(0)?;
    let variants = fields.variants()?;
    let facet = fields.string("facet")?;
    if name == "hardlink" {
        return Ok(Action::Hardlink {
            path,
            target: fields.required_string("target")?,
            variants,
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
            variants,
            facet,
            mode: fields.required_string("mode")?,
            preserve: fields
                .properties
                .get("preserve")
                .map(|value| value.as_bool().context("preserve must be #true or #false"))
                .transpose()?
                .unwrap_or(false),
            original_name: fields.string("original-name")?,
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
            variants,
            facet,
            mode: fields.required_string("mode")?,
        },
        _ => Action::Symlink {
            path,
            uid,
            gid,
            variants,
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

fn schema_header(version: u32, kind: &str) -> KdlNode {
    let mut schema = KdlNode::new("schema-version");
    schema.push(i128::from(version));
    schema.insert("kind", kind);
    schema
}

pub fn manifest_text(manifest: &Manifest) -> String {
    let mut doc = KdlDocument::new();
    doc.nodes_mut()
        .push(schema_header(manifest.format, MANIFEST_KIND));
    let mut package = KdlNode::new("package");
    package.push(manifest.name.as_str());
    package.insert("publisher", manifest.publisher.as_str());
    package.insert("version", manifest.version.to_string());
    if let Some(version) = &manifest.upstream_version {
        package.insert("upstream-version", version.as_str());
    }
    if let Some(architecture) = &manifest.architecture {
        package.insert("architecture", architecture.as_str());
    }
    if manifest.requires_reboot {
        package.insert("requires-reboot", true);
    }
    if let Some(summary) = &manifest.summary {
        package.insert("summary", summary.as_str());
    }
    let children = package.ensure_children().nodes_mut();
    for dependency in &manifest.dependency {
        children.push(match dependency {
            Dependency::Require(target) => target_node("require", target),
            Dependency::RequireCapability(target) => target_node("require-capability", target),
            Dependency::Recommend(target) => target_node("recommends", target),
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
    for (name, version) in &manifest.provides {
        let mut node = KdlNode::new("provides");
        node.push(name.as_str());
        if let Some(version) = version {
            node.insert("version", version.to_string());
        }
        children.push(node);
    }
    for optional in &manifest.optional_dependencies {
        let mut node = target_node("optional", &optional.target);
        node.insert("description", optional.description.as_str());
        children.push(node);
    }
    for (path, link) in &manifest.mediated_links {
        let mut node = KdlNode::new("mediate");
        node.push(path.as_str());
        node.insert("group", link.group.as_str());
        if link.priority != 0 {
            node.insert("priority", i128::from(link.priority));
        }
        children.push(node);
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
                original_name,
                sha256,
                size,
                capabilities,
                ..
            } => {
                node.insert("mode", mode.as_str());
                if *preserve {
                    node.insert("preserve", true);
                }
                if let Some(original_name) = original_name {
                    node.insert("original-name", original_name.as_str());
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
        let selectors = action.selectors();
        if let Some(facet) = selectors.facet {
            node.insert("facet", facet);
        }
        for (name, value) in selectors.variants {
            if name == ARCH_VARIANT {
                node.insert("arch", value.as_str());
            } else {
                node.insert(format!("{VARIANT_PREFIX}{name}").as_str(), value.as_str());
            }
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
    pub policy: ImagePolicy,
}

pub fn read_image_config(path: &Path) -> Result<ImageConfig> {
    let ImageDefinition {
        mut publishers,
        arch,
        policy,
        release_profile,
        image_id,
        ..
    } = read_image_definition(path)?;
    ensure!(
        image_id.is_none(),
        "use create_configured_image for an explicit image-id"
    );
    ensure!(
        publishers.len() == 1 && release_profile.is_none() && publishers[0].build_trust.is_none(),
        "use create_configured_image for multiple publishers, a release profile, or build trust"
    );
    let publisher = publishers.pop().expect("one publisher");
    Ok(ImageConfig {
        publisher: publisher.name,
        repository: publisher.repository,
        trust_key: publisher.trust_key,
        arch,
        policy,
    })
}

pub(crate) struct PublisherConfig {
    pub name: String,
    pub repository: PathBuf,
    pub trust_key: PathBuf,
    pub build_trust: Option<BuildTrustConfig>,
}

pub(crate) struct BuildTrustConfig {
    minimum_builders: usize,
    keys: Vec<PathBuf>,
}

impl BuildTrustConfig {
    pub(crate) fn load(&self, publisher_key: &[u8]) -> Result<BuildTrust> {
        let trust = BuildTrust::load(self.minimum_builders, &self.keys)?;
        trust.check_publisher_trust(publisher_key)?;
        Ok(trust)
    }
}

pub(crate) fn read_build_trust(path: &Path) -> Result<BuildTrustConfig> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("opening build trust policy {}", path.display()))?;
    let parent = path
        .parent()
        .context("missing build trust policy directory")?;
    let bytes = read_limited(File::open(&path)?, 1024 * 1024)?;
    let doc = document(std::str::from_utf8(&bytes)?)?;
    schema(&doc, "build-trust")?;
    ensure!(
        doc.nodes().len() == 2,
        "expected only schema-version and build-trust"
    );
    build_trust_config(one(&doc, "build-trust")?, parent)
        .with_context(|| format!("parsing build trust policy {}", path.display()))
}

fn build_trust_config(node: &KdlNode, parent: &Path) -> Result<BuildTrustConfig> {
    let fields = Fields::read(node, 0, &["minimum-builders"], true)?;
    let keys = node
        .children()
        .context("build-trust requires a builder-key block")?
        .nodes()
        .iter()
        .map(|entry| {
            ensure!(
                entry.name().value() == "builder-key",
                "expected builder-key in build-trust"
            );
            Ok(parent.join(Fields::read(entry, 1, &[], false)?.string_arg(0)?))
        })
        .collect::<Result<_>>()?;
    Ok(BuildTrustConfig {
        minimum_builders: fields.integer("minimum-builders")?.unwrap_or(1),
        keys,
    })
}

fn publisher_config(node: &KdlNode, name: String, parent: &Path) -> Result<PublisherConfig> {
    validate_publisher(&name)?;
    let children = node
        .children()
        .context("publisher requires a configuration block")?;
    let repo = Fields::read(one(children, "repository")?, 1, &[], false)?.string_arg(0)?;
    let key = Fields::read(one(children, "trust-root")?, 1, &[], false)?.string_arg(0)?;
    let build_trust = children
        .nodes()
        .iter()
        .find(|entry| entry.name().value() == "build-trust")
        .map(|node| build_trust_config(node, parent))
        .transpose()?;
    ensure!(
        children.nodes().len() == 2 + usize::from(build_trust.is_some()),
        "expected repository, trust-root, and optional build-trust under publisher"
    );
    Ok(PublisherConfig {
        name,
        repository: parent.join(repo),
        trust_key: parent.join(key),
        build_trust,
    })
}

pub(crate) struct ImageDefinition {
    pub publishers: Vec<PublisherConfig>,
    pub arch: String,
    pub policy: ImagePolicy,
    pub release_profile: Option<String>,
    pub bootstrap_catalog: Option<String>,
    pub image_id: Option<String>,
    pub mirrors: BTreeMap<String, MirrorConfig>,
    pub boot_btrfs: Option<BtrfsConfig>,
    pub stage_packages: BTreeSet<String>,
    pub cache_keep_versions: Option<usize>,
    pub git_toolchain_config: Option<PathBuf>,
    pub refresh_policies: BTreeMap<String, crate::repo::release::RefreshPolicy>,
}

pub(crate) struct BtrfsConfig {
    pub volume: PathBuf,
    pub environments: PathBuf,
    pub esp: PathBuf,
    pub activation: crate::boot::Activation<Option<String>>,
    pub uki: bool,
    pub uefi: bool,
    /// Known-good environments retained beyond the protected ones.
    pub keep_environments: usize,
    pub auto_prune: bool,
    /// Nested subvolumes that staged roots replace with empty directories.
    pub exclude_subvolumes: Vec<String>,
}

pub(crate) struct MirrorConfig {
    pub root: PathBuf,
    pub metadata: String,
    pub targets: String,
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
    schema(&doc, "image-config")?;
    let mut config = ImageDefinition {
        publishers: Vec::new(),
        arch: std::env::consts::ARCH.to_owned(),
        policy: ImagePolicy::default(),
        release_profile: None,
        bootstrap_catalog: None,
        image_id: None,
        mirrors: BTreeMap::new(),
        boot_btrfs: None,
        stage_packages: BTreeSet::new(),
        cache_keep_versions: None,
        git_toolchain_config: None,
        refresh_policies: BTreeMap::new(),
    };
    let mut architecture = false;
    let mut names = BTreeSet::new();
    for node in doc.nodes() {
        match node.name().value() {
            "schema-version" => {}
            "publisher" => {
                let name = Fields::read(node, 1, &[], true)?.string_arg(0)?;
                ensure!(names.insert(name.clone()), "duplicate publisher {name}");
                config
                    .publishers
                    .push(publisher_config(node, name, parent)?);
            }
            "mirror" => {
                let fields = Fields::read(node, 1, &["root", "metadata", "targets"], false)?;
                let name = fields.string_arg(0)?;
                let mirror = MirrorConfig {
                    root: parent.join(fields.required_string("root")?),
                    metadata: fields.required_string("metadata")?,
                    targets: fields.required_string("targets")?,
                };
                ensure!(
                    config.mirrors.insert(name.clone(), mirror).is_none(),
                    "duplicate mirror for publisher {name}"
                );
            }
            "refresh-policy" => {
                let fields =
                    Fields::read(node, 1, &["on-unavailable", "max-staleness-hours"], false)?;
                let name = fields.string_arg(0)?;
                validate_publisher(&name)?;
                let hours = fields.integer::<u64>("max-staleness-hours")?;
                let policy = match fields.required_string("on-unavailable")?.as_str() {
                    "fail" => {
                        ensure!(
                            hours.is_none(),
                            "max-staleness-hours applies only to on-unavailable=\"defer\""
                        );
                        None
                    }
                    "defer" => Some(
                        hours
                            .filter(|hours| {
                                (1..=crate::repo::release::RefreshPolicy::MAX_STALENESS_HOURS)
                                    .contains(hours)
                            })
                            .context("on-unavailable=\"defer\" needs max-staleness-hours between 1 and 8784")?,
                    ),
                    _ => bail!("on-unavailable must be \"fail\" or \"defer\""),
                };
                let policy = crate::repo::release::RefreshPolicy {
                    defer_unavailable_hours: policy,
                };
                ensure!(
                    config
                        .refresh_policies
                        .insert(name.clone(), policy)
                        .is_none(),
                    "duplicate refresh-policy for publisher {name}"
                );
            }
            "boot-btrfs" => {
                ensure!(config.boot_btrfs.is_none(), "duplicate boot-btrfs node");
                let fields = Fields::read(
                    node,
                    0,
                    &[
                        "volume",
                        "environments",
                        "esp",
                        "root-partuuid",
                        "options",
                        "kernel",
                        "initrd",
                        "uki",
                        "uefi",
                        "tries",
                        "keep-environments",
                        "auto-prune",
                    ],
                    true,
                )?;
                let mut exclude_subvolumes = Vec::new();
                for child in node.children().map(|doc| doc.nodes()).unwrap_or_default() {
                    ensure!(
                        child.name().value() == "exclude-subvolume",
                        "unknown boot-btrfs child {}",
                        child.name().value()
                    );
                    let path = Fields::read(child, 1, &[], false)?.string_arg(0)?;
                    ensure!(
                        !exclude_subvolumes.contains(&path),
                        "duplicate exclude-subvolume {path}"
                    );
                    exclude_subvolumes.push(path);
                }
                crate::boot::excluded_subvolumes(&exclude_subvolumes)?;
                let keep_environments = fields.integer("keep-environments")?.unwrap_or(3);
                ensure!(
                    (1..=99).contains(&keep_environments),
                    "keep-environments must be between 1 and 99"
                );
                let uefi = fields
                    .properties
                    .get("uefi")
                    .map(|value| value.as_bool().context("uefi must be #true or #false"))
                    .transpose()?
                    .unwrap_or(false);
                let tries = fields.integer("tries")?.unwrap_or(if uefi { 1 } else { 2 });
                ensure!(
                    (1..=99).contains(&tries),
                    "boot tries must be between 1 and 99"
                );
                ensure!(
                    !uefi || tries == 1,
                    "direct UEFI trials allow exactly one boot attempt"
                );
                config.boot_btrfs = Some(BtrfsConfig {
                    volume: parent.join(fields.required_string("volume")?),
                    environments: parent.join(fields.required_string("environments")?),
                    esp: parent.join(fields.required_string("esp")?),
                    uki: fields
                        .properties
                        .get("uki")
                        .map(|value| value.as_bool().context("uki must be #true or #false"))
                        .transpose()?
                        .unwrap_or(false),
                    uefi,
                    keep_environments,
                    auto_prune: fields
                        .properties
                        .get("auto-prune")
                        .map(|value| {
                            value
                                .as_bool()
                                .context("auto-prune must be #true or #false")
                        })
                        .transpose()?
                        .unwrap_or(true),
                    exclude_subvolumes,
                    activation: crate::boot::Activation {
                        root_partuuid: fields.string("root-partuuid")?,
                        kernel: fields
                            .string("kernel")?
                            .unwrap_or_else(|| "boot/vmlinuz".into()),
                        initrd: fields.string("initrd")?,
                        options: fields.string("options")?.unwrap_or_default(),
                        tries: tries.try_into()?,
                        known_good: false,
                    },
                });
            }
            "git-toolchain-config" => {
                ensure!(
                    config.git_toolchain_config.is_none(),
                    "duplicate git-toolchain-config node"
                );
                let path = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                config.git_toolchain_config = Some(parent.join(path));
            }
            "cache-keep-versions" => {
                ensure!(
                    config.cache_keep_versions.is_none(),
                    "duplicate cache-keep-versions node"
                );
                let fields = Fields::read(node, 1, &[], false)?;
                let versions = fields.args[0]
                    .as_integer()
                    .and_then(|value| usize::try_from(value).ok())
                    .context("cache-keep-versions must be an unsigned integer in range")?;
                config.cache_keep_versions = Some(versions);
            }
            "stage-package" => {
                let name = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                validate_name(&name)?;
                ensure!(
                    config.stage_packages.insert(name.clone()),
                    "duplicate stage-package {name}"
                );
            }
            "architecture" => {
                ensure!(!architecture, "duplicate architecture node");
                architecture = true;
                config.arch = Fields::read(node, 1, &[], false)?.string_arg(0)?;
            }
            "image-id" => {
                ensure!(config.image_id.is_none(), "duplicate image-id node");
                let id = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                ensure!(
                    is_uuid(&id) && id != NIL_UUID,
                    "image-id must be a nonzero UUID"
                );
                config.image_id = Some(id.to_ascii_lowercase());
            }
            "release-profile" => {
                ensure!(
                    config.release_profile.is_none(),
                    "duplicate release-profile node"
                );
                let fields = Fields::read(node, 1, &["bootstrap-catalog"], false)?;
                let name = fields.string_arg(0)?;
                validate_name(&name).context("invalid release profile name")?;
                config.release_profile = Some(name);
                config.bootstrap_catalog = fields.string("bootstrap-catalog")?;
                if let Some(catalog) = &config.bootstrap_catalog {
                    validate_digest(catalog).context("invalid bootstrap catalog")?;
                }
            }
            "facet" => {
                let fields = Fields::read(node, 2, &["package", "with-dependencies"], false)?;
                let name = fields.string_arg(0)?;
                let enabled = fields.args[1]
                    .as_bool()
                    .context("facet must be #true or #false")?;
                let with_dependencies = fields
                    .properties
                    .get("with-dependencies")
                    .map(|value| {
                        value
                            .as_bool()
                            .context("with-dependencies must be #true or #false")
                    })
                    .transpose()?
                    .unwrap_or(false);
                let key = match fields.string("package")? {
                    Some(package) => format!("{package}:{name}"),
                    None if with_dependencies => {
                        bail!("with-dependencies requires a package property")
                    }
                    None => name,
                };
                ensure!(
                    !name_has_scope(&key, fields.properties.contains_key("package")),
                    "write per-package facets as facet NAME VALUE package=PACKAGE"
                );
                config
                    .policy
                    .insert_facet(&key, enabled, with_dependencies)?;
            }
            "variant" => {
                let fields = Fields::read(node, 2, &[], false)?;
                let name = fields.string_arg(0)?;
                let value = variant_value(fields.args[1])?;
                validate_variant(&name, &value)?;
                ensure!(
                    name != ARCH_VARIANT,
                    "set the arch variant with the architecture node"
                );
                ensure!(
                    config.policy.variants.insert(name.clone(), value).is_none(),
                    "duplicate variant {name}"
                );
            }
            name => bail!("unknown image configuration node {name}"),
        }
    }
    ensure!(!config.publishers.is_empty(), "missing publisher node");
    for publisher in config.mirrors.keys() {
        ensure!(
            names.contains(publisher),
            "mirror has no configured publisher {publisher}"
        );
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_resolves_paths_and_checks_duplicates() -> Result<()> {
        let config = r#"
            schema-version 1 kind="image-config"
            publisher "canopa" {
                repository "../repo"
                trust-root "/etc/keys/canopa.trust.json"
            }
            architecture "aarch64"
            cache-keep-versions 2
            facet "doc.man" #false
            image-id "A47593C2-E985-42D2-92B6-44C7057E1E96"
        "#;
        let parsed = parse_image_config(config, Path::new("/tmp/config"))?;
        for invalid in [
            config.replace(" kind=\"image-config\"", ""),
            config.replace("image-config", "toolchain-config"),
        ] {
            assert!(parse_image_config(&invalid, Path::new("/tmp/config")).is_err());
        }
        assert_eq!(
            parsed.publishers[0].repository,
            Path::new("/tmp/config/../repo")
        );
        assert_eq!(
            parsed.publishers[0].trust_key,
            Path::new("/etc/keys/canopa.trust.json")
        );
        assert_eq!(parsed.arch, "aarch64");
        assert_eq!(parsed.cache_keep_versions, Some(2));
        assert!(!parsed.policy.facets["doc.man"]);
        assert_eq!(
            parsed.image_id.as_deref(),
            Some("a47593c2-e985-42d2-92b6-44c7057e1e96")
        );
        assert!(
            parse_image_config(
                &config.replace("A47593C2-E985-42D2-92B6-44C7057E1E96", NIL_UUID),
                Path::new("/tmp")
            )
            .err()
            .unwrap()
            .to_string()
            .contains("image-id must be a nonzero UUID")
        );
        assert!(
            parse_image_config(
                &format!("{config}\nfacet \"doc.man\" #true\n"),
                Path::new("/tmp")
            )
            .is_err()
        );
        for invalid in ["-1", "#true", "2\ncache-keep-versions 1"] {
            assert!(
                parse_image_config(
                    &config.replace(
                        "cache-keep-versions 2",
                        &format!("cache-keep-versions {invalid}")
                    ),
                    Path::new("/tmp"),
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn refresh_policies_default_to_failing_and_bound_deferral() -> Result<()> {
        let header = "schema-version 1 kind=\"image-config\"\npublisher \"canopa\" { repository \"repo\"; trust-root \"trust.json\"; }\n";
        let parsed = parse_image_config(
            &format!(
                "{header}refresh-policy \"extras\" on-unavailable=\"defer\" max-staleness-hours=72\nrefresh-policy \"tools\" on-unavailable=\"fail\"\n"
            ),
            Path::new("/tmp"),
        )?;
        assert_eq!(
            parsed.refresh_policies["extras"].defer_unavailable_hours,
            Some(72)
        );
        assert_eq!(
            parsed.refresh_policies["tools"].defer_unavailable_hours,
            None
        );
        for invalid in [
            "refresh-policy \"extras\" on-unavailable=\"defer\"",
            "refresh-policy \"extras\" on-unavailable=\"defer\" max-staleness-hours=0",
            "refresh-policy \"extras\" on-unavailable=\"defer\" max-staleness-hours=9000",
            "refresh-policy \"extras\" on-unavailable=\"fail\" max-staleness-hours=1",
            "refresh-policy \"extras\" on-unavailable=\"warn\"",
            "refresh-policy \"extras\"",
            "refresh-policy \"Bad Name\" on-unavailable=\"fail\"",
            "refresh-policy \"extras\" on-unavailable=\"fail\"\nrefresh-policy \"extras\" on-unavailable=\"fail\"",
        ] {
            assert!(
                parse_image_config(&format!("{header}{invalid}\n"), Path::new("/tmp")).is_err(),
                "accepted {invalid}"
            );
        }
        Ok(())
    }

    #[test]
    fn boot_btrfs_retention_and_subvolume_exclusions() -> Result<()> {
        let base = r#"
            schema-version 1 kind="image-config"
            publisher "canopa" {
                repository "../repo"
                trust-root "/etc/keys/canopa.trust.json"
            }
        "#;
        let node = r#"boot-btrfs volume="/cps" environments="/cps/@environments" esp="/efi""#;
        let parse = |boot: &str| {
            parse_image_config(
                &format!(
                    "{base}{boot}
"
                ),
                Path::new("/tmp"),
            )
        };
        let defaults = parse(node)?.boot_btrfs.context("missing boot-btrfs")?;
        assert_eq!(defaults.keep_environments, 3);
        assert!(defaults.auto_prune && defaults.exclude_subvolumes.is_empty());
        let custom = parse(&format!(
            "{node} keep-environments=5 auto-prune=#false {{
    exclude-subvolume \"/var/lib/docker\"
}}"
        ))?
        .boot_btrfs
        .context("missing boot-btrfs")?;
        assert_eq!(custom.keep_environments, 5);
        assert!(!custom.auto_prune);
        assert_eq!(custom.exclude_subvolumes, ["/var/lib/docker"]);
        for invalid in [
            format!("{node} keep-environments=0"),
            format!("{node} keep-environments=100"),
            format!("{node} auto-prune=1"),
            format!(
                "{node} {{
    exclude-subvolume \"var/lib/cps\"
}}"
            ),
            format!(
                "{node} {{
    exclude-subvolume \"a\"
    exclude-subvolume \"a\"
}}"
            ),
            format!(
                "{node} {{
    keep \"a\"
}}"
            ),
        ] {
            assert!(parse(&invalid).is_err(), "{invalid}");
        }
        Ok(())
    }

    #[test]
    fn manifest_version_one_requires_its_kind() -> Result<()> {
        let input = "schema-version 1 kind=\"package-manifest\"\npackage \"test/example\" publisher=\"canopa\" version=\"1.0.0-0\"\n";
        let manifest = parse_manifest(input)?;
        assert_eq!(manifest.format, 1);
        assert_eq!(parse_manifest(&manifest_text(&manifest))?, manifest);

        for invalid in [
            input.replace(" kind=\"package-manifest\"", ""),
            input.replace("package-manifest", "build-recipe"),
            input.replace("schema-version 1", "schema-version 2"),
        ] {
            assert!(parse_manifest(&invalid).is_err(), "{invalid}");
        }
        Ok(())
    }

    #[test]
    fn manifest_rejects_duplicate_properties() {
        let input = r#"
            schema-version 1 kind="package-manifest"
            package "test/example" publisher="canopa" version="1.0.0-0" {
                file "usr/bin/example" mode="0755" uid=0 uid=1 gid=0
            }
        "#;
        let error = parse_manifest(input).unwrap_err();
        assert!(format!("{error:#}").contains("duplicate file property uid"));
    }

    #[test]
    fn capability_declarations_round_trip_and_reject_invalid_values() -> Result<()> {
        let package = |body: &str| {
            format!(
                "schema-version 1 kind=\"package-manifest\"\npackage \"test/example\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{body}\n}}"
            )
        };
        let manifest = parse_manifest(&package(
            r#"provides "cap/runtime" version="2.0.0-0"
               provides "cap/helper"
               require-capability "cap/runtime" version=">=1.0.0-0""#,
        ))?;
        manifest.validate(false)?;
        assert_eq!(parse_manifest(&manifest_text(&manifest))?, manifest);

        for body in [
            r#"provides "cap/runtime"
               provides "cap/runtime" version="2.0.0-0""#,
            r#"provides "cap/runtime" version=">=1.0.0-0""#,
            r#"provides "runtime""#,
            r#"require-capability "runtime""#,
        ] {
            assert!(
                parse_manifest(&package(body))
                    .and_then(|manifest| manifest.validate(false))
                    .is_err(),
                "{body}"
            );
        }
        Ok(())
    }
}
