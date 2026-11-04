use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    io::{self, Read, Seek},
    str::FromStr,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::file_caps::FileCapabilities;

pub const FORMAT: u32 = 1;

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

pub fn validate_publisher(publisher: &str) -> Result<()> {
    ensure!(name_component(publisher), "invalid publisher {publisher:?}");
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Version([u32; 4]);

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (upstream, revision) = value
            .split_once('-')
            .context("expected major.minor.patch-revision")?;
        let parts: Vec<_> = upstream.split('.').chain([revision]).collect();
        ensure!(parts.len() == 4, "expected major.minor.patch-revision");
        let mut numbers = [0; 4];
        for (part, number) in parts.into_iter().zip(&mut numbers) {
            *number = part.parse().context("invalid numeric version component")?;
            ensure!(part == number.to_string(), "noncanonical version {value:?}");
        }
        Ok(Self(numbers))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch, revision] = self.0;
        write!(f, "{major}.{minor}.{patch}-{revision}")
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Dependency {
    Require(DependencyTarget),
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

impl DependencyTarget {
    pub fn matches(&self, version: Version) -> Result<bool> {
        Ok(VersionConstraint::parse(self.version.as_deref())?.matches(version))
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
            Self::Require(target) | Self::Exclude(target) => target.validate(),
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
                Ok((operator, bound.parse()?))
            })
            .collect::<Result<Vec<_>>>()
            .map(Self)
    }

    pub(crate) fn matches(&self, version: Version) -> bool {
        self.0.iter().all(|&(operator, bound)| match operator {
            ">=" => version >= bound,
            "<=" => version <= bound,
            ">" => version > bound,
            "<" => version < bound,
            _ => version == bound,
        })
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
        #[serde(skip_serializing_if = "Option::is_none")]
        arch: Option<String>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        arch: Option<String>,
    },
    Symlink {
        path: String,
        target: String,
        uid: u32,
        gid: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        facet: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        arch: Option<String>,
    },
    Hardlink {
        path: String,
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        facet: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        arch: Option<String>,
    },
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

fn parse_mode(mode: &str) -> Result<u32> {
    ensure!(
        mode.len() == 4
            && mode.starts_with('0')
            && mode.bytes().all(|b| (b'0'..=b'7').contains(&b)),
        "expected a mode such as \"0755\""
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

    pub fn selectors(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::File { facet, arch, .. }
            | Self::Dir { facet, arch, .. }
            | Self::Symlink { facet, arch, .. }
            | Self::Hardlink { facet, arch, .. } => (facet.as_deref(), arch.as_deref()),
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
        let (facet, arch) = self.selectors();
        for selector in [facet, arch].into_iter().flatten() {
            ensure!(
                name_component(selector),
                "invalid action selector {selector:?}"
            );
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
                parse_mode(mode)?;
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
pub struct Manifest {
    pub format: u32,
    pub publisher: String,
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub dependency: Vec<Dependency>,
    #[serde(default)]
    pub action: Vec<Action>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub systemd: Vec<crate::systemd::Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<crate::boot::kernel::Kernel>,
}

impl Manifest {
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
            self.format == FORMAT,
            "unsupported manifest format {}",
            self.format
        );
        validate_publisher(&self.publisher)?;
        validate_name(&self.name)?;
        for dependency in &self.dependency {
            dependency.validate()?;
        }
        let mut actions = BTreeMap::new();
        for action in &self.action {
            action
                .validate(published)
                .with_context(|| format!("action {}", action.path()))?;
            if actions
                .insert((action.path(), action.selectors()), action)
                .is_some()
            {
                bail!("duplicate action for {}", action.path());
            }
        }
        for action in &self.action {
            if let Action::Hardlink { path, target, .. } = action {
                let (facet, arch) = action.selectors();
                ensure!(
                    [None, facet]
                        .into_iter()
                        .any(|facet| [None, arch].into_iter().any(|arch| matches!(
                            actions.get(&(target.as_str(), (facet, arch))),
                            Some(Action::File {
                                preserve: false,
                                ..
                            })
                        ))),
                    "hard link {path} requires an immutable file action for {target} in the same package with compatible selectors"
                );
            }
        }
        let mut systemd = BTreeSet::new();
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
