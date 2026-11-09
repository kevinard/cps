use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    io::{self, Read, Seek},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::file_caps::FileCapabilities;

mod selection;
mod version;

pub(crate) use selection::{
    ARCH_VARIANT, DEFAULT_FACETS, Resolved, Selection, facet_pattern_covers, validate_facet,
    validate_facet_pattern, validate_policy, validate_variant,
};
pub use selection::{FacetSource, ImagePolicy, PackageFacet, PackageFacets};
pub use version::Version;

pub(crate) const MANIFEST_FORMAT: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileDigest {
    pub sha256: String,
    pub size: u64,
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn digest_reader(mut reader: impl Read) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

pub fn validate_digest(hash: &str) -> Result<()> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid SHA-256 digest {hash:?}"
    );
    Ok(())
}

pub(crate) const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

pub(crate) fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.contains('\0') && path.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "invalid root-relative path {path:?}"
    );
    Ok(())
}

fn name_component(part: &str) -> bool {
    part.starts_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        && part
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        name.split('/').all(name_component),
        "invalid package name {name:?}"
    );
    Ok(())
}

fn validate_capability(name: &str) -> Result<()> {
    let suffix = name
        .strip_prefix("cap/")
        .context("capability names must begin with cap/")?;
    validate_name(suffix).with_context(|| format!("invalid capability name {name:?}"))
}

pub fn validate_publisher(publisher: &str) -> Result<()> {
    ensure!(name_component(publisher), "invalid publisher {publisher:?}");
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PackageSpec<'a> {
    pub publisher: Option<&'a str>,
    pub name: &'a str,
    pub version: Option<Version>,
}

impl<'a> PackageSpec<'a> {
    pub fn parse(text: &'a str) -> Result<Self> {
        let (publisher, spec) = if let Some(spec) = text.strip_prefix("cps://") {
            let (publisher, spec) = spec.split_once('/').context("package ID has no name")?;
            validate_publisher(publisher)?;
            (Some(publisher), spec)
        } else {
            (None, text)
        };
        let (name, version) = if let Some((name, version)) = spec.split_once('@') {
            (name, Some(version.parse()?))
        } else {
            (spec, None)
        };
        validate_name(name)?;
        Ok(Self {
            publisher,
            name,
            version,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Dependency {
    Require(DependencyTarget),
    RequireCapability(DependencyTarget),
    Recommend(DependencyTarget),
    Incorporate(DependencyTarget),
    Exclude(DependencyTarget),
    RequireAny {
        alternatives: Vec<DependencyTarget>,
    },
    Conditional {
        target: DependencyTarget,
        predicate: DependencyTarget,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyTarget {
    pub package: String,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptionalDependency {
    pub target: DependencyTarget,
    pub description: String,
}

impl DependencyTarget {
    pub fn matches(&self, version: impl Borrow<Version>) -> Result<bool> {
        Ok(VersionConstraint::parse(self.version.as_deref())?.matches(version.borrow()))
    }

    pub(crate) fn matches_capability(&self, manifest: &Manifest) -> Result<bool> {
        let constraint = VersionConstraint::parse(self.version.as_deref())?;
        Ok(manifest.provides_capability(&self.package, &constraint))
    }

    fn validate(&self) -> Result<()> {
        validate_name(&self.package)?;
        VersionConstraint::parse(self.version.as_deref())?;
        Ok(())
    }
}

impl fmt::Display for DependencyTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}",
            self.package,
            self.version.as_deref().unwrap_or("at any version")
        )
    }
}

impl Dependency {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Require(target) | Self::Recommend(target) | Self::Exclude(target) => {
                target.validate()
            }
            Self::RequireCapability(target) => {
                validate_capability(&target.package)?;
                VersionConstraint::parse(target.version.as_deref())?;
                Ok(())
            }
            Self::Incorporate(target) => {
                ensure!(
                    target.version.is_some(),
                    "incorporate requires a version constraint"
                );
                target.validate()
            }
            Self::RequireAny { alternatives } => {
                ensure!(
                    alternatives.len() >= 2,
                    "require-any needs at least two alternatives"
                );
                let mut seen = BTreeSet::new();
                for target in alternatives {
                    target.validate()?;
                    ensure!(
                        seen.insert((&target.package, &target.version)),
                        "duplicate require-any alternative {target}"
                    );
                }
                Ok(())
            }
            Self::Conditional { target, predicate } => {
                target.validate()?;
                predicate
                    .validate()
                    .context("invalid conditional predicate")
            }
        }
    }
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) struct VersionConstraint(Vec<(&'static str, Version)>);

impl VersionConstraint {
    /// Match one package ID's exact spelling, unlike the semantic `=` operator.
    pub(crate) fn exact(version: Version) -> Self {
        Self(vec![("==", version)])
    }

    pub(crate) fn parse(constraint: Option<&str>) -> Result<Self> {
        let clauses = constraint
            .into_iter()
            .flat_map(|constraint| constraint.split(','));
        clauses
            .map(|clause| {
                let clause = clause.trim();
                let (operator, bound) = [">=", "<=", "=", ">", "<"]
                    .into_iter()
                    .find_map(|op| clause.strip_prefix(op).map(|value| (op, value)))
                    .with_context(|| format!("invalid constraint {clause:?}"))?;
                Ok((operator, bound.trim_start().parse()?))
            })
            .collect::<Result<Vec<_>>>()
            .map(Self)
    }

    /// Match by release semantics, so `2.0.0` satisfies `=2.0.0-0`.
    pub(crate) fn matches(&self, version: &Version) -> bool {
        self.0.iter().all(|(operator, bound)| {
            if *operator == "==" {
                return version == bound;
            }
            let (version, bound) = (version.semantic(), bound.semantic());
            match *operator {
                ">=" => version >= bound,
                "<=" => version <= bound,
                ">" => version > bound,
                "<" => version < bound,
                _ => version == bound,
            }
        })
    }

    pub(crate) fn matches_provided(&self, version: Option<&Version>) -> bool {
        version.map_or(self.0.is_empty(), |version| self.matches(version))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    File {
        path: String,
        mode: String,
        uid: u32,
        gid: u32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        preserve: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        facet: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        variants: BTreeMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<FileCapabilities>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_name: Option<String>,
    },
    Dir {
        path: String,
        mode: String,
        uid: u32,
        gid: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        facet: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        variants: BTreeMap<String, String>,
    },
    Symlink {
        path: String,
        target: String,
        uid: u32,
        gid: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        facet: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        variants: BTreeMap<String, String>,
    },
    Hardlink {
        path: String,
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        facet: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        variants: BTreeMap<String, String>,
    },
}

/// The facet and variants that select an action.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Selectors<'a> {
    pub facet: Option<&'a str>,
    pub variants: &'a BTreeMap<String, String>,
}

impl<'a> Selectors<'a> {
    pub fn arch(&self) -> Option<&'a str> {
        self.variants.get(ARCH_VARIANT).map(String::as_str)
    }

    /// Whether no image can select both actions, because they require different values
    /// of one variant.
    fn excludes(&self, other: &Selectors<'_>) -> bool {
        self.variants
            .iter()
            .any(|(name, value)| other.variants.get(name).is_some_and(|other| other != value))
    }

    /// Whether every image that selects `self` also selects `other`.
    fn implies(&self, other: &Selectors<'_>) -> bool {
        other.facet.is_none_or(|facet| self.facet == Some(facet))
            && other
                .variants
                .iter()
                .all(|(name, value)| self.variants.get(name) == Some(value))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    File {
        sha256: String,
        size: u64,
        mode: u32,
        uid: u32,
        gid: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<FileCapabilities>,
    },
    Dir {
        mode: u32,
        uid: u32,
        gid: u32,
    },
    Symlink {
        target: String,
        uid: u32,
        gid: u32,
    },
    Hardlink {
        target: String,
    },
    Special,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir { .. })
    }

    pub(crate) fn capabilities(&self) -> Option<&FileCapabilities> {
        match self {
            Self::File { capabilities, .. } => capabilities.as_ref(),
            _ => None,
        }
    }
}

pub(crate) fn copy_payload(source: &File, node: &Node, destination: &mut File) -> Result<()> {
    let Node::File { sha256, size, .. } = node else {
        bail!("expected a file payload");
    };
    ensure!(
        source.metadata()?.len() == *size,
        "payload length mismatch for {sha256}"
    );
    let copied = io::copy(&mut source.take(size.saturating_add(1)), destination)?;
    ensure!(copied == *size, "payload length mismatch for {sha256}");
    destination.rewind()?;
    ensure!(
        digest_reader(destination)? == *sha256,
        "payload digest mismatch for {sha256}"
    );
    Ok(())
}

pub(crate) fn parse_mode(mode: &str) -> Result<u32> {
    ensure!(
        mode.len() == 4 && mode.bytes().all(|b| (b'0'..=b'7').contains(&b)),
        "expected a four-digit octal mode such as \"0755\" or \"1777\""
    );
    Ok(u32::from_str_radix(mode, 8)?)
}

impl Action {
    pub fn path(&self) -> &str {
        match self {
            Self::File { path, .. }
            | Self::Dir { path, .. }
            | Self::Symlink { path, .. }
            | Self::Hardlink { path, .. } => path,
        }
    }

    pub fn selectors(&self) -> Selectors<'_> {
        match self {
            Self::File {
                facet, variants, ..
            }
            | Self::Dir {
                facet, variants, ..
            }
            | Self::Symlink {
                facet, variants, ..
            }
            | Self::Hardlink {
                facet, variants, ..
            } => Selectors {
                facet: facet.as_deref(),
                variants,
            },
        }
    }

    pub fn preserve(&self) -> bool {
        matches!(self, Self::File { preserve: true, .. })
    }

    pub(crate) fn original_name(&self) -> Option<&str> {
        match self {
            Self::File { original_name, .. } => original_name.as_deref(),
            _ => None,
        }
    }

    pub fn node(&self) -> Result<Node> {
        Ok(match self {
            Self::File {
                sha256,
                size,
                mode,
                uid,
                gid,
                capabilities,
                ..
            } => Node::File {
                sha256: sha256.clone().context("missing file digest")?,
                size: size.context("missing file size")?,
                mode: parse_mode(mode)?,
                uid: *uid,
                gid: *gid,
                capabilities: *capabilities,
            },
            Self::Dir { mode, uid, gid, .. } => Node::Dir {
                mode: parse_mode(mode)?,
                uid: *uid,
                gid: *gid,
            },
            Self::Symlink {
                target, uid, gid, ..
            } => Node::Symlink {
                target: target.clone(),
                uid: *uid,
                gid: *gid,
            },
            Self::Hardlink { target, .. } => Node::Hardlink {
                target: target.clone(),
            },
        })
    }

    fn validate(&self, published: bool) -> Result<()> {
        let path = self.path();
        validate_path(path)?;
        ensure!(
            path.split('/').next() != Some(".cps"),
            "reserved image path {path}"
        );
        let selectors = self.selectors();
        if let Some(facet) = selectors.facet {
            selection::validate_facet(facet)?;
        }
        for (name, value) in selectors.variants {
            validate_variant(name, value)?;
        }
        let (uid, gid) = match self {
            Self::File {
                mode,
                uid,
                gid,
                sha256,
                size,
                preserve,
                capabilities,
                original_name,
                ..
            } => {
                let mode = parse_mode(mode)?;
                ensure!(
                    mode & 0o1000 == 0,
                    "sticky bits are only supported on directories"
                );
                if mode & 0o6000 != 0 {
                    ensure!(
                        *uid == 0 && mode & 0o111 != 0 && mode & 0o022 == 0 && !preserve,
                        "setid bits require a root-owned executable with no group or other write access and preserve disabled"
                    );
                    ensure!(
                        capabilities.is_none(),
                        "setid bits cannot be combined with file capabilities"
                    );
                }
                if let Some(original) = original_name {
                    ensure!(*preserve, "original-name requires preserve=#true");
                    let (package, path) = original.split_once(':').context(
                        "original-name must be a package name followed by ':' and a path",
                    )?;
                    validate_name(package)?;
                    validate_path(path)?;
                    ensure!(
                        path.split('/').next() != Some(".cps"),
                        "reserved image path {path}"
                    );
                }
                if let Some(capabilities) = capabilities {
                    capabilities.validate_declaration()?;
                    ensure!(
                        *uid == 0 && mode & 0o111 != 0 && mode & 0o022 == 0 && !preserve,
                        "file capabilities require a root-owned executable with no group or other write access and preserve disabled"
                    );
                }
                if published {
                    validate_digest(sha256.as_deref().context("missing file digest")?)?;
                    ensure!(size.is_some(), "missing file size");
                } else {
                    ensure!(
                        sha256.is_none() && size.is_none(),
                        "publish derives file digest and size"
                    );
                }
                (*uid, *gid)
            }
            Self::Dir { mode, uid, gid, .. } => {
                ensure!(
                    parse_mode(mode)? & 0o4000 == 0,
                    "setuid directories are not supported"
                );
                (*uid, *gid)
            }
            Self::Symlink {
                target, uid, gid, ..
            } => {
                ensure!(
                    !target.is_empty() && !target.contains('\0'),
                    "invalid symlink target"
                );
                (*uid, *gid)
            }
            Self::Hardlink { target, .. } => {
                validate_path(target)?;
                ensure!(target != path, "hard link points to itself");
                return Ok(());
            }
        };
        ensure!(uid != u32::MAX && gid != u32::MAX, "invalid owner");
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediatedLink {
    pub group: String,
    pub priority: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub publisher: String,
    pub name: String,
    pub version: Version,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub dependency: Vec<Dependency>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provides: BTreeMap<String, Option<Version>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_dependencies: Vec<OptionalDependency>,
    #[serde(default)]
    pub action: Vec<Action>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mediated_links: BTreeMap<String, MediatedLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub systemd: Vec<crate::systemd::Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<crate::boot::kernel::Kernel>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requires_reboot: bool,
}

pub(crate) fn validate_summary(summary: &str) -> Result<()> {
    validate_display_text(summary, 256, "package summary")
}

fn validate_display_text(value: &str, max_bytes: usize, label: &str) -> Result<()> {
    ensure!(
        value.len() <= max_bytes
            && !value.trim().is_empty()
            && !value.chars().any(char::is_control),
        "{label} must be nonempty, at most {max_bytes} bytes, and free of control characters"
    );
    Ok(())
}

impl Manifest {
    pub(crate) fn provides_capability(&self, name: &str, constraint: &VersionConstraint) -> bool {
        self.provides
            .get(name)
            .is_some_and(|version| constraint.matches_provided(version.as_ref()))
    }

    pub(crate) fn supports_architecture(&self, architecture: &str) -> bool {
        self.architecture
            .as_deref()
            .is_none_or(|target| target == architecture)
    }

    pub(crate) fn check_architecture(&self, architecture: &str) -> Result<()> {
        if let Some(target) = &self.architecture {
            ensure!(
                target == architecture,
                "{} targets architecture {target}, expected {architecture}",
                self.id()
            );
        }
        Ok(())
    }

    pub(crate) fn with_payloads(
        mut self,
        artifacts: &BTreeMap<String, FileDigest>,
    ) -> Result<Self> {
        for action in &mut self.action {
            if let Action::File {
                path, sha256, size, ..
            } = action
            {
                let file = artifacts
                    .get(path)
                    .with_context(|| format!("missing package payload {path}"))?;
                *sha256 = Some(file.sha256.clone());
                *size = Some(file.size);
            }
        }
        self.validate(true)?;
        Ok(self)
    }

    pub(crate) fn payload_paths(&self) -> BTreeSet<&str> {
        self.action
            .iter()
            .filter_map(|action| match action {
                Action::File { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn id(&self) -> String {
        format!("cps://{}/{}@{}", self.publisher, self.name, self.version)
    }

    pub fn validate(&self, published: bool) -> Result<()> {
        ensure!(
            self.format == MANIFEST_FORMAT,
            "unsupported manifest format {}",
            self.format
        );
        validate_publisher(&self.publisher)?;
        validate_name(&self.name)?;
        if let Some(version) = &self.upstream_version {
            validate_display_text(version, 256, "upstream version")?;
        }
        if let Some(architecture) = &self.architecture {
            validate_publisher(architecture).context("invalid package architecture")?;
        }
        if let Some(summary) = &self.summary {
            validate_summary(summary)?;
        }
        for name in self.provides.keys() {
            validate_capability(name)?;
        }
        for dependency in &self.dependency {
            dependency.validate()?;
        }
        for optional in &self.optional_dependencies {
            optional
                .target
                .validate()
                .with_context(|| format!("optional dependency {}", optional.target.package))?;
            validate_display_text(
                &optional.description,
                1024,
                "optional dependency description",
            )?;
        }
        let mut actions = BTreeMap::<&str, Vec<&Action>>::new();
        for action in &self.action {
            action
                .validate(published)
                .with_context(|| format!("action {}", action.path()))?;
            if let (Some(architecture), Some(target)) =
                (&self.architecture, action.selectors().arch())
            {
                ensure!(
                    architecture == target,
                    "action {} targets {target}, but its package targets {architecture}",
                    action.path()
                );
            }
            let alternatives = actions.entry(action.path()).or_default();
            // One path may carry alternatives only for different values of one variant,
            // so that no image can select two of them.
            for other in alternatives.iter() {
                ensure!(
                    action.selectors().excludes(&other.selectors()),
                    "duplicate action for {}: actions at one path must select different values of a variant",
                    action.path()
                );
            }
            alternatives.push(action);
        }
        for action in &self.action {
            if let Action::Hardlink { path, target, .. } = action {
                let selectors = action.selectors();
                ensure!(
                    actions
                        .get(target.as_str())
                        .is_some_and(|targets| targets.iter().any(|target| matches!(
                            target,
                            Action::File {
                                preserve: false,
                                ..
                            }
                        ) && selectors
                            .implies(&target.selectors()))),
                    "hard link {path} requires an immutable file action for {target} in the same package with compatible selectors"
                );
            }
        }
        let mut systemd = BTreeSet::new();
        let mut priorities = BTreeMap::new();
        for (path, link) in &self.mediated_links {
            validate_publisher(&link.group).context("invalid mediator group")?;
            let mut actions = self
                .action
                .iter()
                .filter(|action| action.path() == path)
                .peekable();
            ensure!(
                actions.peek().is_some()
                    && actions.all(|action| matches!(action, Action::Symlink { .. })),
                "mediation requires a symlink action for {path}"
            );
            let priority = priorities.entry(&link.group).or_insert(link.priority);
            ensure!(
                *priority == link.priority,
                "a package must use one priority for mediator {}",
                link.group
            );
        }
        for effect in &self.systemd {
            effect.validate()?;
            ensure!(
                systemd.insert(effect),
                "duplicate {} action for {}",
                effect.kind(),
                effect.name()
            );
            self.require_immutable_file(&effect.path(), effect.kind())?;
        }
        if let Some(kernel) = &self.kernel {
            kernel.validate()?;
            self.require_immutable_file(&kernel.image(), "kernel-install")?;
            if let Some(path) = kernel.initrd() {
                self.require_immutable_file(&path, "kernel-install")?;
            }
        }
        Ok(())
    }

    fn require_immutable_file(&self, path: &str, operation: &str) -> Result<()> {
        let mut inputs = self
            .action
            .iter()
            .filter(|action| action.path() == path)
            .peekable();
        ensure!(
            inputs.peek().is_some()
                && inputs.all(|action| matches!(
                    action,
                    Action::File {
                        preserve: false,
                        ..
                    }
                )),
            "{operation} requires an immutable file action for {path} in the same package"
        );
        Ok(())
    }
}
