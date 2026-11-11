//! Facet and variant selection of package actions.
//!
//! A facet is optional content inside one package. An image chooses facets with
//! hierarchical rules (`doc.man`, `locale.*`, `*`) and may override them for single
//! packages. A variant is a mutually exclusive image property, such as `arch`, that
//! selects between alternative content. Neither facets nor variants gate dependencies.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque, btree_map::Entry},
    fmt,
};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::{Action, Dependency, Manifest, name_component, validate_name};

/// The built-in variant that every image sets.
pub(crate) const ARCH_VARIANT: &str = "arch";

/// Facet rules that apply when neither a package override nor an image rule matches.
/// Every other facet defaults to enabled.
pub(crate) const DEFAULT_FACETS: [(&str, bool); 2] = [("debug.*", false), ("isa.*", false)];

static NO_VARIANTS: BTreeMap<String, String> = BTreeMap::new();
static NO_PACKAGE_FACETS: PackageFacets = BTreeMap::new();

/// Per-package facet rules: package name or `prefix/*` glob to facet pattern to rule.
pub type PackageFacets = BTreeMap<String, BTreeMap<String, PackageFacet>>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFacet {
    pub value: bool,
    /// Also apply the rule to the installed dependency closure of the package.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_dependencies: bool,
}

fn dotted(name: &str) -> bool {
    name_component(name) && name.split('.').all(|part| !part.is_empty())
}

/// Validate an exact facet name as used by an action.
pub(crate) fn validate_facet(name: &str) -> Result<()> {
    ensure!(dotted(name), "invalid facet {name:?}");
    Ok(())
}

/// Validate an image facet rule: an exact name, `prefix.*`, or `*`.
pub(crate) fn validate_facet_pattern(pattern: &str) -> Result<()> {
    let name = if pattern == "*" {
        return Ok(());
    } else {
        pattern.strip_suffix(".*").unwrap_or(pattern)
    };
    ensure!(dotted(name), "invalid facet pattern {pattern:?}");
    Ok(())
}

/// Validate a package override target: an exact package name or `prefix/*`.
pub(crate) fn validate_package_pattern(pattern: &str) -> Result<()> {
    let name = pattern.strip_suffix("/*").unwrap_or(pattern);
    validate_name(name).map_err(|_| anyhow::anyhow!("invalid package pattern {pattern:?}"))
}

pub(crate) fn validate_variant(name: &str, value: &str) -> Result<()> {
    ensure!(dotted(name), "invalid variant name {name:?}");
    ensure!(
        name_component(value),
        "invalid value {value:?} for variant {name}"
    );
    Ok(())
}

/// Return how specifically `pattern` names `facet`, or `None` when it does not match.
/// An exact name beats any wildcard, and a longer wildcard prefix beats a shorter one.
fn facet_specificity(pattern: &str, facet: &str) -> Option<usize> {
    if pattern == facet {
        return Some(usize::MAX);
    }
    if pattern == "*" {
        return Some(0);
    }
    let prefix = pattern.strip_suffix(".*")?;
    facet
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('.'))
        .then_some(prefix.len() + 1)
}

/// Return whether a package name or `prefix/*` glob names `package`.
pub(crate) fn package_pattern_matches(pattern: &str, package: &str) -> bool {
    package_specificity(pattern, package).is_some()
}

/// Return how specifically `pattern` names `package`, or `None` when it does not match.
fn package_specificity(pattern: &str, package: &str) -> Option<usize> {
    if pattern == package {
        return Some(usize::MAX);
    }
    let prefix = pattern.strip_suffix("/*")?;
    package
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
        .then_some(prefix.len())
}

/// Return whether an allowed facet pattern covers every facet that `pattern` can match.
pub(crate) fn facet_pattern_covers(allowed: &str, pattern: &str) -> bool {
    allowed == "*"
        || allowed == pattern
        || (allowed.ends_with(".*") && facet_specificity(allowed, pattern).is_some())
}

fn best<'r, V: Copy + 'r>(
    rules: impl IntoIterator<Item = (&'r str, V)>,
    facet: &str,
) -> Option<(&'r str, V)> {
    rules
        .into_iter()
        .filter_map(|(pattern, value)| {
            facet_specificity(pattern, facet).map(|rank| (rank, pattern, value))
        })
        .max_by_key(|(rank, ..)| *rank)
        .map(|(_, pattern, value)| (pattern, value))
}

/// The rule that decided a facet value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum FacetSource {
    /// A per-package override, written `PACKAGE:FACET`.
    Package { rule: String },
    /// A per-package override applied with its dependencies, reached from `root`.
    Dependency { rule: String, root: String },
    /// An image-wide rule.
    Image { rule: String },
    /// A built-in default rule.
    Default { rule: String },
    /// No rule matched, so the facet is enabled.
    Unset,
}

impl fmt::Display for FacetSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Package { rule } | Self::Image { rule } => f.write_str(rule),
            Self::Dependency { rule, root } => write!(f, "{rule} (dependency of {root})"),
            Self::Default { rule } => write!(f, "{rule} (default)"),
            Self::Unset => f.write_str("unset"),
        }
    }
}

/// The image properties that select package actions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Selection<'a> {
    pub arch: &'a str,
    pub variants: &'a BTreeMap<String, String>,
    pub facets: &'a BTreeMap<String, bool>,
    pub packages: &'a PackageFacets,
}

impl<'a> Selection<'a> {
    /// A selection with image-wide facets only, as used by signed release profiles.
    pub(crate) fn image(
        arch: &'a str,
        variants: &'a BTreeMap<String, String>,
        facets: &'a BTreeMap<String, bool>,
    ) -> Self {
        Self {
            arch,
            variants,
            facets,
            packages: &NO_PACKAGE_FACETS,
        }
    }

    /// A selection by architecture and image-wide facets.
    pub(crate) fn arch(arch: &'a str, facets: &'a BTreeMap<String, bool>) -> Self {
        Self::image(arch, &NO_VARIANTS, facets)
    }

    pub(crate) fn variant(&self, name: &str) -> Option<&'a str> {
        if name == ARCH_VARIANT {
            Some(self.arch)
        } else {
            self.variants.get(name).map(String::as_str)
        }
    }

    pub(crate) fn variants_match(&self, action: &Action) -> bool {
        action
            .selectors()
            .variants
            .iter()
            .all(|(name, value)| self.variant(name) == Some(value.as_str()))
    }

    /// Refuse an action that is selected by a variant this image does not set.
    pub(crate) fn check_variants(&self, package: &str, action: &Action) -> Result<()> {
        for name in action.selectors().variants.keys() {
            ensure!(
                self.variant(name).is_some(),
                "{package} selects {} by variant {name}, which this image does not set; choose a value with cps variant set {name}=VALUE",
                action.path()
            );
        }
        Ok(())
    }

    /// The image-wide value of a facet and the rule that decided it.
    pub(crate) fn image_facet(&self, facet: &str) -> (bool, FacetSource) {
        if let Some((rule, &value)) = best(
            self.facets
                .iter()
                .map(|(rule, value)| (rule.as_str(), value)),
            facet,
        ) {
            return (
                value,
                FacetSource::Image {
                    rule: rule.to_owned(),
                },
            );
        }
        if let Some((rule, value)) = best(DEFAULT_FACETS, facet) {
            return (
                value,
                FacetSource::Default {
                    rule: rule.to_owned(),
                },
            );
        }
        (true, FacetSource::Unset)
    }

    /// Per-package rule sets that match `package`, most specific package pattern first.
    fn package_rules(
        &self,
        package: &str,
    ) -> impl Iterator<Item = (&'a str, &'a BTreeMap<String, PackageFacet>)> {
        let mut matches = self
            .packages
            .iter()
            .filter_map(|(pattern, rules)| {
                package_specificity(pattern, package).map(|rank| (rank, pattern.as_str(), rules))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(rank, ..)| std::cmp::Reverse(*rank));
        matches
            .into_iter()
            .map(|(_, pattern, rules)| (pattern, rules))
    }

    fn package_facet(&self, package: &str, facet: &str) -> Option<(bool, FacetSource)> {
        self.package_rules(package).find_map(|(pattern, rules)| {
            best(
                rules
                    .iter()
                    .map(|(rule, entry)| (rule.as_str(), entry.value)),
                facet,
            )
            .map(|(rule, value)| {
                (
                    value,
                    FacetSource::Package {
                        rule: format!("{pattern}:{rule}"),
                    },
                )
            })
        })
    }

    /// The value of a facet for one package, ignoring rules inherited from dependents.
    pub(crate) fn facet(&self, package: &str, facet: &str) -> (bool, FacetSource) {
        self.package_facet(package, facet)
            .unwrap_or_else(|| self.image_facet(facet))
    }

    /// Whether an action applies, ignoring rules inherited from dependents.
    pub(crate) fn applies(&self, package: &str, action: &Action) -> bool {
        self.variants_match(action)
            && action
                .selectors()
                .facet
                .is_none_or(|facet| self.facet(package, facet).0)
    }

    /// Resolve rules that also apply to the installed dependency closure of their packages.
    pub(crate) fn resolve<'m>(
        self,
        installed: impl IntoIterator<Item = (&'m str, &'m Manifest)>,
    ) -> Result<Resolved<'a>> {
        let mut resolved = Resolved {
            selection: self,
            inherited: BTreeMap::new(),
        };
        let closures = self
            .packages
            .iter()
            .filter_map(|(pattern, rules)| {
                let rules = rules
                    .iter()
                    .filter(|(_, rule)| rule.with_dependencies)
                    .map(|(facet, rule)| (facet.as_str(), rule.value))
                    .collect::<Vec<_>>();
                (!rules.is_empty()).then_some((pattern.as_str(), rules))
            })
            .collect::<Vec<_>>();
        if closures.is_empty() {
            return Ok(resolved);
        }
        let installed = installed.into_iter().collect::<BTreeMap<_, _>>();
        let mut providers = BTreeMap::<&str, Vec<&str>>::new();
        for (&name, manifest) in &installed {
            for capability in manifest.provides.keys() {
                providers.entry(capability).or_default().push(name);
            }
        }
        let mut conflicts = Vec::new();
        for (pattern, rules) in closures {
            for &root in installed.keys() {
                if package_specificity(pattern, root).is_none() {
                    continue;
                }
                for dependency in closure(root, &installed, &providers) {
                    let inherited = resolved.inherited.entry(dependency.to_owned()).or_default();
                    for &(facet, value) in &rules {
                        let rule = format!("{pattern}:{facet}");
                        match inherited.entry(facet.to_owned()) {
                            Entry::Vacant(entry) => {
                                entry.insert(Inherited {
                                    value,
                                    rule,
                                    root: root.to_owned(),
                                });
                            }
                            Entry::Occupied(entry) => {
                                let previous = entry.get();
                                if previous.value != value {
                                    conflicts.push((
                                        dependency,
                                        facet,
                                        format!(
                                            "conflicting facet overrides for dependency {dependency}: {} from {} and {rule} from {root}",
                                            previous.rule, previous.root
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        // A dependency's own rules settle a conflict between the rules it inherits.
        for (dependency, facet, message) in conflicts {
            let settled = self
                .package_rules(dependency)
                .any(|(_, rules)| rules.keys().any(|rule| facet_pattern_covers(rule, facet)));
            ensure!(settled, "{message}");
        }
        Ok(resolved)
    }
}

/// Installed packages reachable from `root` through content dependencies, excluding it.
/// `require`, `require-capability`, `require-any`, and `conditional` targets count;
/// recommendations, incorporations, and exclusions do not.
fn closure<'m>(
    root: &'m str,
    installed: &BTreeMap<&'m str, &'m Manifest>,
    providers: &BTreeMap<&str, Vec<&'m str>>,
) -> BTreeSet<&'m str> {
    let mut seen = BTreeSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(name) = queue.pop_front() {
        let mut targets = Vec::new();
        for dependency in &installed[name].dependency {
            match dependency {
                Dependency::Require(target) => targets.push(target.package.as_str()),
                Dependency::RequireAny { alternatives } => {
                    targets.extend(alternatives.iter().map(|target| target.package.as_str()));
                }
                Dependency::Conditional { target, .. } => targets.push(&target.package),
                Dependency::RequireCapability(target) => {
                    targets.extend(
                        providers
                            .get(target.package.as_str())
                            .into_iter()
                            .flatten()
                            .copied(),
                    );
                }
                Dependency::Recommend(_) | Dependency::Incorporate(_) | Dependency::Exclude(_) => {}
            }
        }
        for target in targets {
            if let Some((&name, _)) = installed.get_key_value(target)
                && seen.insert(name)
            {
                queue.push_back(name);
            }
        }
    }
    seen.remove(root);
    seen
}

#[derive(Debug)]
struct Inherited {
    value: bool,
    rule: String,
    root: String,
}

/// A selection with dependency-closure overrides resolved against installed packages.
#[derive(Debug)]
pub(crate) struct Resolved<'a> {
    pub selection: Selection<'a>,
    inherited: BTreeMap<String, BTreeMap<String, Inherited>>,
}

impl Resolved<'_> {
    /// The value of a facet for one installed package and the rule that decided it.
    /// Precedence: the package's own overrides (exact name, then the longest glob),
    /// then overrides inherited from dependents, then image rules, then defaults.
    pub(crate) fn facet(&self, package: &str, facet: &str) -> (bool, FacetSource) {
        if let Some(found) = self.selection.package_facet(package, facet) {
            return found;
        }
        if let Some((_, inherited)) = self.inherited.get(package).and_then(|rules| {
            best(
                rules.iter().map(|(rule, entry)| (rule.as_str(), entry)),
                facet,
            )
        }) {
            return (
                inherited.value,
                FacetSource::Dependency {
                    rule: inherited.rule.clone(),
                    root: inherited.root.clone(),
                },
            );
        }
        self.selection.image_facet(facet)
    }

    pub(crate) fn applies(&self, package: &str, action: &Action) -> bool {
        self.selection.variants_match(action)
            && action
                .selectors()
                .facet
                .is_none_or(|facet| self.facet(package, facet).0)
    }

    /// Package patterns of the rules applied with dependencies that could decide
    /// a facet of `manifest`'s actions, in any selection of packages. The
    /// package's own rules take precedence, so the facets they decide are fixed.
    pub(crate) fn inheritable(&self, package: &str, manifest: &Manifest) -> BTreeSet<&'_ str> {
        let mut patterns = BTreeSet::new();
        for (pattern, rules) in self.selection.packages {
            if patterns.contains(pattern.as_str()) {
                continue;
            }
            let reaches = manifest.action.iter().any(|action| {
                action.selectors().facet.is_some_and(|facet| {
                    self.selection.package_facet(package, facet).is_none()
                        && rules.iter().any(|(rule, entry)| {
                            entry.with_dependencies && facet_specificity(rule, facet).is_some()
                        })
                })
            });
            if reaches {
                patterns.insert(pattern.as_str());
            }
        }
        patterns
    }

    /// Facet patterns of the per-package rules, direct or inherited, that reach `package`.
    pub(crate) fn overrides(&self, package: &str) -> Vec<(String, String)> {
        let mut rules = self
            .selection
            .package_rules(package)
            .flat_map(|(pattern, rules)| {
                rules
                    .keys()
                    .map(move |facet| (facet.clone(), format!("{pattern}:{facet}")))
            })
            .collect::<Vec<_>>();
        rules.extend(
            self.inherited
                .get(package)
                .into_iter()
                .flatten()
                .map(|(facet, inherited)| (facet.clone(), inherited.rule.clone())),
        );
        rules
    }
}

/// The facet and variant policy of an image, as configured or recorded in a lock.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImagePolicy {
    pub facets: BTreeMap<String, bool>,
    pub package_facets: PackageFacets,
    pub variants: BTreeMap<String, String>,
}

impl From<BTreeMap<String, bool>> for ImagePolicy {
    fn from(facets: BTreeMap<String, bool>) -> Self {
        Self {
            facets,
            ..Self::default()
        }
    }
}

impl ImagePolicy {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_policy(&self.facets, &self.package_facets, &self.variants)
    }

    pub(crate) fn selection<'a>(&'a self, arch: &'a str) -> Selection<'a> {
        Selection {
            arch,
            variants: &self.variants,
            facets: &self.facets,
            packages: &self.package_facets,
        }
    }

    /// Add one `[PACKAGE:]FACET` rule, refusing duplicates.
    pub fn insert_facet(&mut self, key: &str, value: bool, with_dependencies: bool) -> Result<()> {
        let inserted = match parse_facet_key(key)? {
            (None, facet) => {
                ensure!(!with_dependencies, "{key} is not a per-package facet rule");
                self.facets.insert(facet.to_owned(), value).is_none()
            }
            (Some(package), facet) => self
                .package_facets
                .entry(package.to_owned())
                .or_default()
                .insert(
                    facet.to_owned(),
                    PackageFacet {
                        value,
                        with_dependencies,
                    },
                )
                .is_none(),
        };
        ensure!(inserted, "duplicate facet rule {key}");
        Ok(())
    }

    /// Set or reset `[PACKAGE:]FACET` rules.
    pub(crate) fn change_facets(
        &mut self,
        changes: &BTreeMap<String, Option<bool>>,
        with_dependencies: bool,
    ) -> Result<()> {
        ensure!(
            !with_dependencies
                || changes
                    .iter()
                    .any(|(key, value)| key.contains(':') && value.is_some()),
            "--with-dependencies requires a PACKAGE:FACET=true|false change"
        );
        for (key, value) in changes {
            match (parse_facet_key(key)?, value) {
                ((None, facet), Some(value)) => {
                    self.facets.insert(facet.to_owned(), *value);
                }
                ((None, facet), None) => {
                    self.facets.remove(facet);
                }
                ((Some(package), facet), Some(value)) => {
                    self.package_facets
                        .entry(package.to_owned())
                        .or_default()
                        .insert(
                            facet.to_owned(),
                            PackageFacet {
                                value: *value,
                                with_dependencies,
                            },
                        );
                }
                ((Some(package), facet), None) => {
                    if let Some(rules) = self.package_facets.get_mut(package) {
                        rules.remove(facet);
                        if rules.is_empty() {
                            self.package_facets.remove(package);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Set or unset non-architecture variants.
    pub(crate) fn change_variants(
        &mut self,
        changes: &BTreeMap<String, Option<String>>,
    ) -> Result<()> {
        for (name, value) in changes {
            ensure!(
                name != ARCH_VARIANT,
                "the arch variant is fixed when an image is created"
            );
            match value {
                Some(value) => {
                    validate_variant(name, value)?;
                    self.variants.insert(name.clone(), value.clone());
                }
                None => {
                    validate_variant(name, "unset")?;
                    self.variants.remove(name);
                }
            }
        }
        Ok(())
    }
}

/// Split a `[PACKAGE:]FACET` rule key.
pub(crate) fn parse_facet_key(key: &str) -> Result<(Option<&str>, &str)> {
    let (package, facet) = match key.split_once(':') {
        Some((package, facet)) => {
            validate_package_pattern(package)?;
            (Some(package), facet)
        }
        None => (None, key),
    };
    validate_facet_pattern(facet)?;
    Ok((package, facet))
}

pub(crate) fn validate_policy(
    facets: &BTreeMap<String, bool>,
    packages: &PackageFacets,
    variants: &BTreeMap<String, String>,
) -> Result<()> {
    for facet in facets.keys() {
        validate_facet_pattern(facet)?;
    }
    for (package, rules) in packages {
        validate_package_pattern(package)?;
        ensure!(!rules.is_empty(), "empty facet overrides for {package}");
        for facet in rules.keys() {
            validate_facet_pattern(facet)?;
        }
    }
    for (name, value) in variants {
        if name == ARCH_VARIANT {
            bail!("the arch variant is set by the image architecture");
        }
        validate_variant(name, value)?;
    }
    Ok(())
}

impl ImagePolicy {
    /// Add one `NAME=VALUE` variant, refusing duplicates and the arch variant.
    pub fn insert_variant(&mut self, entry: &str) -> Result<()> {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("expected NAME=VALUE"))?;
        validate_variant(name, value)?;
        ensure!(
            name != ARCH_VARIANT,
            "set the arch variant with the image architecture"
        );
        ensure!(
            self.variants
                .insert(name.to_owned(), value.to_owned())
                .is_none(),
            "duplicate variant {name}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(entries: &[(&str, bool)]) -> BTreeMap<String, bool> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), *value))
            .collect()
    }

    #[test]
    fn most_specific_facet_rule_wins() {
        let facets = rules(&[
            ("*", false),
            ("locale.*", true),
            ("locale.fr.*", false),
            ("locale.fr.ca", true),
        ]);
        let selection = Selection::arch("x86_64", &facets);
        let value = |facet| selection.image_facet(facet);
        assert_eq!(
            value("locale.fr.ca"),
            (
                true,
                FacetSource::Image {
                    rule: "locale.fr.ca".into()
                }
            )
        );
        assert_eq!(value("locale.fr.fr").1.to_string(), "locale.fr.*");
        assert!(!value("locale.fr.fr").0);
        assert!(value("locale.de").0);
        // `locale.*` matches names below `locale`, not `locale` itself.
        assert_eq!(value("locale").1.to_string(), "*");
        assert_eq!(value("doc.man").1.to_string(), "*");
        // An image-wide rule that matches beats the built-in defaults.
        assert!(!value("debug.cps").0);
        assert_eq!(value("localedata").1.to_string(), "*");
    }

    #[test]
    fn debug_and_multilib_default_off_and_are_overridable() {
        let empty = BTreeMap::new();
        let selection = Selection::arch("x86_64", &empty);
        assert_eq!(
            selection.image_facet("debug.cps"),
            (
                false,
                FacetSource::Default {
                    rule: "debug.*".into()
                }
            )
        );
        assert!(!selection.image_facet("isa.i686").0);
        assert_eq!(selection.image_facet("devel"), (true, FacetSource::Unset));
        // `debug` itself is not below `debug.*`.
        assert!(selection.image_facet("debug").0);
        let enabled = rules(&[("isa.i686", true)]);
        let selection = Selection::arch("x86_64", &enabled);
        assert!(selection.image_facet("isa.i686").0);
        assert!(!selection.image_facet("isa.armv7").0);
    }

    fn manifest(name: &str, requires: &[&str]) -> Manifest {
        Manifest {
            format: super::super::MANIFEST_FORMAT,
            publisher: "canopa".into(),
            name: name.into(),
            version: "1.0.0-0".parse().unwrap(),
            upstream_version: None,
            architecture: None,
            summary: None,
            license: None,
            homepage: None,
            source_url: None,
            dependency: requires
                .iter()
                .map(|package| {
                    Dependency::Require(super::super::DependencyTarget {
                        package: (*package).into(),
                        version: None,
                    })
                })
                .collect(),
            provides: BTreeMap::new(),
            optional_dependencies: Vec::new(),
            action: Vec::new(),
            mediated_links: BTreeMap::new(),
            systemd: Vec::new(),
            triggers: Vec::new(),
            kernel: None,
            requires_reboot: false,
        }
    }

    #[test]
    fn package_overrides_precede_image_rules_and_reach_dependencies() -> Result<()> {
        let mut policy = ImagePolicy::from(rules(&[("devel", false)]));
        policy.insert_facet("library/sqlite:devel", true, true)?;
        policy.insert_facet("library/*:doc.*", false, false)?;
        policy.insert_facet("library/zlib:devel", false, false)?;
        policy.validate()?;
        let manifests = [
            manifest("library/sqlite", &["library/readline", "library/zlib"]),
            manifest("library/readline", &["library/ncurses"]),
            manifest("library/ncurses", &[]),
            manifest("library/zlib", &[]),
            manifest("tools/unrelated", &[]),
        ];
        let resolved = policy
            .selection("x86_64")
            .resolve(manifests.iter().map(|m| (m.name.as_str(), m)))?;
        let facet = |package, facet| resolved.facet(package, facet);
        assert_eq!(
            facet("library/sqlite", "devel").1.to_string(),
            "library/sqlite:devel"
        );
        assert!(facet("library/sqlite", "devel").0);
        assert_eq!(
            facet("library/ncurses", "devel").1.to_string(),
            "library/sqlite:devel (dependency of library/sqlite)"
        );
        assert!(facet("library/ncurses", "devel").0);
        // A package's own override beats one inherited from a dependent.
        assert!(!facet("library/zlib", "devel").0);
        assert!(!facet("tools/unrelated", "devel").0);
        assert_eq!(facet("tools/unrelated", "devel").1.to_string(), "devel");
        // A glob with a matching rule applies when the exact entry has none.
        assert_eq!(
            facet("library/sqlite", "doc.man").1.to_string(),
            "library/*:doc.*"
        );
        assert!(facet("tools/unrelated", "doc.man").0);
        // Without the closure option only the named package changes.
        policy
            .package_facets
            .get_mut("library/sqlite")
            .unwrap()
            .get_mut("devel")
            .unwrap()
            .with_dependencies = false;
        let resolved = policy
            .selection("x86_64")
            .resolve(manifests.iter().map(|m| (m.name.as_str(), m)))?;
        assert!(!resolved.facet("library/ncurses", "devel").0);

        let mut conflicting = ImagePolicy::default();
        conflicting.insert_facet("library/sqlite:devel", true, true)?;
        conflicting.insert_facet("library/readline:devel", false, true)?;
        let error = conflicting
            .selection("x86_64")
            .resolve(manifests.iter().map(|m| (m.name.as_str(), m)))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicting facet overrides for dependency library/ncurses")
        );
        Ok(())
    }

    #[test]
    fn rule_keys_and_variants_are_validated() -> Result<()> {
        for valid in [
            "devel",
            "locale.*",
            "*",
            "library/sqlite:devel",
            "library/*:doc.*",
        ] {
            parse_facet_key(valid)?;
        }
        for invalid in [
            "",
            "Devel",
            "locale.",
            "locale..fr",
            "*.man",
            "lo*",
            "library/*/x:devel",
            "*:devel",
            "library/sqlite:",
            "library/sqlite:devel:x",
        ] {
            assert!(parse_facet_key(invalid).is_err(), "{invalid}");
        }
        assert!(facet_pattern_covers("devel", "devel"));
        assert!(facet_pattern_covers("doc.*", "doc.man"));
        assert!(facet_pattern_covers("doc.*", "doc.man.*"));
        assert!(facet_pattern_covers("*", "devel"));
        assert!(!facet_pattern_covers("doc.man", "doc.*"));
        assert!(!facet_pattern_covers("devel", "*"));

        let mut policy = ImagePolicy::default();
        policy.change_variants(&BTreeMap::from([(
            "container".to_owned(),
            Some("true".to_owned()),
        )]))?;
        assert_eq!(policy.variants["container"], "true");
        assert!(
            policy
                .change_variants(&BTreeMap::from([(
                    "arch".to_owned(),
                    Some("aarch64".to_owned())
                )]))
                .is_err()
        );
        assert!(policy.insert_variant("container=other").is_err());
        assert!(policy.insert_variant("debug=True").is_err());
        assert!(policy.insert_variant("arch=x86_64").is_err());
        policy.insert_variant("debug=false")?;
        assert!(
            policy
                .change_facets(&BTreeMap::from([("devel".to_owned(), Some(true))]), true)
                .is_err()
        );
        Ok(())
    }
}
