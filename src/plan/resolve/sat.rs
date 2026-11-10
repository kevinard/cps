use std::{
    any::Any,
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
};

use anyhow::{Context, Result, bail, ensure};
use resolvo::{
    Candidates, Condition, ConditionId, ConditionalRequirement, Dependencies, DependencyProvider,
    HintDependenciesAvailable, Interner, KnownDependencies, NameId, Problem, Requirement,
    SolvableId, Solver, SolverCache, StringId, UnsolvableOrCancelled, VersionSetId,
    VersionSetUnionId,
    utils::{Pool, VersionSet},
};

use super::Package;
use crate::{
    image::Installed,
    model::{
        Dependency, DependencyTarget, Manifest, Version, VersionConstraint, package_pattern_matches,
    },
    plan::paths::PathConflict,
};

#[derive(Clone, Default, Eq, Hash, PartialEq)]
struct Filter {
    expression: Option<String>,
    constraint: VersionConstraint,
    capability: Option<String>,
    exclude: bool,
}

impl Filter {
    fn matches(&self, manifest: &Manifest) -> bool {
        let matches = match &self.capability {
            Some(name) => manifest.provides_capability(name, &self.constraint),
            None => self.constraint.matches(&manifest.version),
        };
        matches != self.exclude
    }

    fn accepts(&self, package: Package<'_>) -> bool {
        let matches = match &self.capability {
            Some(name) => package.provides_capability(name, &self.constraint),
            None => self.constraint.matches(package.version()),
        };
        matches != self.exclude
    }
}

impl VersionSet for Filter {
    type V = Record;
}

/// What a solvable stands for.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Record {
    /// A candidate package, by its index in the universe.
    Package(usize),
    /// One way to avoid a conflict that depends on other packages, by its
    /// index in the rejections.
    Escape(usize),
}

impl Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Package(index) => write!(f, "package {index}"),
            Self::Escape(index) => write!(f, "escape {index}"),
        }
    }
}

impl Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exclude {
            f.write_str("outside ")?;
        }
        f.write_str(self.expression.as_deref().unwrap_or("any version"))
    }
}

/// Resolution work is bounded by solver steps, not wall-clock time, so the
/// outcome of a plan never depends on the speed of the machine. One step is
/// one cancellation check: a unit propagation round or a provider request.
/// A solve takes about two steps per selected package, and admitting 2048
/// recommendations about a million, so the limits only stop runaway searches.
const REQUIRED_WORK: u64 = 50_000_000;
/// Recommendations share a separate budget. When it runs out, the remaining
/// recommendations are skipped, at the same point on every machine.
const RECOMMENDED_WORK: u64 = 50_000_000;

struct Budget {
    used: Cell<u64>,
    limit: u64,
}

impl Budget {
    fn new(limit: u64) -> Self {
        Self {
            used: Cell::new(0),
            limit,
        }
    }

    fn spend(&self) -> bool {
        let used = self.used.get().saturating_add(1);
        self.used.set(used);
        used <= self.limit
    }

    fn exhausted(&self) -> bool {
        self.used.get() >= self.limit
    }
}

struct Universe<'a> {
    pool: Pool<Filter>,
    candidates: BTreeMap<NameId, Candidates>,
    packages: Vec<(Package<'a>, KnownDependencies)>,
}

impl<'a> Universe<'a> {
    fn new(
        candidates: &BTreeMap<&str, Vec<Package<'a>>>,
        wanted: &BTreeSet<&str>,
        installed: &BTreeMap<String, Installed>,
    ) -> Result<Self> {
        let mut universe = Self {
            pool: Pool::new(),
            candidates: BTreeMap::new(),
            packages: Vec::new(),
        };
        let mut providers = BTreeMap::<_, BTreeSet<_>>::new();
        for (&name, versions) in candidates {
            for package in versions {
                for capability in package.provides().keys() {
                    providers
                        .entry(capability.as_str())
                        .or_default()
                        .insert(name);
                }
            }
        }
        let retained = installed
            .iter()
            .filter(|(name, _)| wanted.contains(name.as_str()))
            .map(|(name, entry)| (name.as_str(), &entry.published.manifest.version))
            .collect();
        for (&name, versions) in candidates {
            let name = universe.pool.intern_package_name(name);
            let mut solvables = Vec::with_capacity(versions.len());
            for &package in versions {
                let mut dependencies = KnownDependencies::default();
                for dependency in package.dependencies() {
                    match dependency {
                        Dependency::Require(target) => dependencies
                            .requirements
                            .push(universe.target(target, false)?.into()),
                        Dependency::RequireCapability(target) => {
                            let requirement = universe.require_capability(
                                target,
                                providers.get(target.package.as_str()),
                                wanted,
                                installed,
                            )?;
                            dependencies.requirements.push(requirement.into());
                        }
                        Dependency::Recommend(_) => {}
                        Dependency::Incorporate(target) => dependencies
                            .constrains
                            .push(universe.target(target, false)?),
                        Dependency::Exclude(target) => {
                            dependencies.constrains.push(universe.target(target, true)?)
                        }
                        Dependency::RequireAny { alternatives } => {
                            let union = universe.alternatives(alternatives, &retained)?;
                            dependencies.requirements.push(union.into());
                        }
                        Dependency::Conditional { target, predicate } => {
                            let predicate = universe.target(predicate, false)?;
                            let condition = universe
                                .pool
                                .intern_condition(Condition::Requirement(predicate));
                            dependencies.requirements.push(ConditionalRequirement {
                                condition: Some(condition),
                                requirement: universe.target(target, false)?.into(),
                            });
                        }
                    }
                }
                let record = Record::Package(universe.packages.len());
                solvables.push(universe.pool.intern_solvable(name, record));
                universe.packages.push((package, dependencies));
            }
            universe.candidates.insert(
                name,
                Candidates {
                    candidates: solvables,
                    hint_dependencies_available: HintDependenciesAvailable::All,
                    ..Candidates::default()
                },
            );
        }
        Ok(universe)
    }

    fn record(&self, id: SolvableId) -> Record {
        self.pool.resolve_solvable(id).record
    }

    /// The package of a solvable, or `None` for an escape.
    fn candidate(&self, id: SolvableId) -> Option<Package<'a>> {
        match self.record(id) {
            Record::Package(index) => Some(self.packages[index].0),
            Record::Escape(_) => None,
        }
    }

    fn package(&self, id: SolvableId) -> Package<'a> {
        self.candidate(id).expect("an escape is not a package")
    }

    /// The selected packages that a conflict between `targets` depends on,
    /// besides the targets themselves, when rules applied with dependencies
    /// from packages matching `patterns` can change their actions.
    ///
    /// A rule reaches a package through the content dependencies that
    /// `Resolved` follows, so the packages on a dependency path, in any
    /// version, from a matching name to a target can pass a rule on. Those
    /// that are selected must stay as they are. Those that are not must stay
    /// out, so the selected packages that could pull one in also stay, along
    /// with the alternatives and capability providers they chose. Every other
    /// selection is still possible. Returns `None` when no rule can reach the
    /// targets, so they conflict whatever else is selected.
    fn contingency(
        &self,
        patterns: &BTreeSet<String>,
        targets: [&str; 2],
        selected: &BTreeMap<&str, SolvableId>,
    ) -> Option<BTreeSet<SolvableId>> {
        let mut providers = BTreeMap::<&str, BTreeSet<&str>>::new();
        for (package, _) in &self.packages {
            for capability in package.provides().keys() {
                providers
                    .entry(capability.as_str())
                    .or_default()
                    .insert(package.name());
            }
        }
        let providers_of = |capability: &str| {
            providers
                .get(capability)
                .into_iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>()
        };
        // Content dependencies, and the predicates that can pull in a
        // conditional target.
        let mut edges = BTreeMap::<&str, BTreeSet<&str>>::new();
        let mut pulls = BTreeMap::<&str, BTreeSet<&str>>::new();
        for (package, _) in &self.packages {
            let name = package.name();
            edges.entry(name).or_default();
            for dependency in package.dependencies() {
                let next: Vec<&str> = match dependency {
                    Dependency::Require(target) => vec![&target.package],
                    Dependency::Conditional { target, predicate } => {
                        pulls
                            .entry(&predicate.package)
                            .or_default()
                            .insert(&target.package);
                        vec![&target.package]
                    }
                    Dependency::RequireAny { alternatives } => alternatives
                        .iter()
                        .map(|target| target.package.as_str())
                        .collect(),
                    Dependency::RequireCapability(target) => providers_of(&target.package),
                    Dependency::Recommend(_)
                    | Dependency::Incorporate(_)
                    | Dependency::Exclude(_) => continue,
                };
                edges.entry(name).or_default().extend(next);
            }
        }
        let reverse = |graphs: &[&BTreeMap<&'a str, BTreeSet<&'a str>>]| {
            let mut reversed = BTreeMap::<&str, BTreeSet<&str>>::new();
            for graph in graphs {
                for (&from, targets) in *graph {
                    for &to in targets {
                        reversed.entry(to).or_default().insert(from);
                    }
                }
            }
            reversed
        };
        let walk = |start: Vec<&'a str>, graph: &BTreeMap<&'a str, BTreeSet<&'a str>>| {
            let mut seen = BTreeSet::new();
            let mut pending = start;
            while let Some(name) = pending.pop() {
                if seen.insert(name) {
                    pending.extend(graph.get(name).into_iter().flatten().copied());
                }
            }
            seen
        };
        let roots = edges
            .keys()
            .copied()
            .filter(|name| {
                patterns
                    .iter()
                    .any(|pattern| package_pattern_matches(pattern, name))
            })
            .collect();
        let from_roots = walk(roots, &edges);
        let targets = targets.map(|target| {
            edges
                .get_key_value(target)
                .map(|(&name, _)| name)
                .expect("conflict owners are candidates")
        });
        let mut passing = walk(targets.to_vec(), &reverse(&[&edges]));
        passing.retain(|name| from_roots.contains(name) && !targets.contains(name));
        if passing.is_empty() {
            return None;
        }
        let (present, absent): (Vec<_>, Vec<_>) = passing
            .into_iter()
            .partition(|name| selected.contains_key(name));
        let mut members: BTreeSet<_> = present.iter().map(|name| selected[name]).collect();
        let pulling = walk(absent, &reverse(&[&edges, &pulls]));
        for name in pulling {
            let Some(&id) = selected.get(name) else {
                continue;
            };
            members.insert(id);
            for dependency in self.package(id).dependencies() {
                let chosen = match dependency {
                    Dependency::RequireAny { alternatives } => alternatives
                        .iter()
                        .map(|target| target.package.as_str())
                        .collect(),
                    Dependency::RequireCapability(target) => providers_of(&target.package),
                    _ => continue,
                };
                members.extend(chosen.into_iter().filter_map(|name| selected.get(name)));
            }
        }
        Some(members)
    }

    fn exact(&self, id: SolvableId) -> ConditionalRequirement {
        let version = self.package(id).version();
        let name = self.pool.resolve_solvable(id).name;
        self.pool
            .intern_version_set(
                name,
                Filter {
                    expression: Some(format!("={version}")),
                    constraint: VersionConstraint::exact(version.clone()),
                    ..Filter::default()
                },
            )
            .into()
    }

    fn target(&self, target: &DependencyTarget, exclude: bool) -> Result<VersionSetId> {
        let name = self.pool.intern_package_name(target.package.as_str());
        let filter = Filter {
            expression: target.version.clone(),
            constraint: VersionConstraint::parse(target.version.as_deref())?,
            capability: None,
            exclude,
        };
        Ok(self.pool.intern_version_set(name, filter))
    }

    fn require_capability(
        &self,
        target: &DependencyTarget,
        providers: Option<&BTreeSet<&str>>,
        wanted: &BTreeSet<&str>,
        installed: &BTreeMap<String, Installed>,
    ) -> Result<Requirement> {
        let filter = Filter {
            expression: Some(format!("providing {target}")),
            constraint: VersionConstraint::parse(target.version.as_deref())?,
            capability: Some(target.package.clone()),
            exclude: false,
        };
        let Some(providers) = providers else {
            // An unsatisfied requirement lets the solver try another version of the consumer.
            let missing = format!("capability {}", target.package);
            let name = self.pool.intern_package_name(missing);
            return Ok(self.pool.intern_version_set(name, filter).into());
        };
        // Capability providers have no declared order. Prefer a provider the
        // transaction wants, then a compatible installed one, then the name.
        let mut choices: Vec<_> = providers
            .iter()
            .map(|&name| {
                let compatible = installed
                    .get(name)
                    .is_some_and(|entry| filter.matches(&entry.published.manifest));
                (!wanted.contains(name), !compatible, name)
            })
            .collect();
        choices.sort_unstable();
        let mut sets = choices.into_iter().map(|(_, _, name)| {
            let name = self.pool.intern_package_name(name);
            self.pool.intern_version_set(name, filter.clone())
        });
        let first = sets.next().expect("a capability has at least one provider");
        Ok(self.pool.intern_version_set_union(first, sets).into())
    }

    /// Order `require-any` alternatives for the solver, which tries a union's
    /// sets in order. A compatible installed provider that the transaction
    /// retains comes first; the rest keep the order the author declared.
    fn alternatives(
        &self,
        targets: &[DependencyTarget],
        retained: &BTreeMap<&str, &Version>,
    ) -> Result<VersionSetUnionId> {
        let mut choices = Vec::with_capacity(targets.len());
        for target in targets {
            let set = self.target(target, false)?;
            let matches_installed = retained
                .get(target.package.as_str())
                .is_some_and(|&version| {
                    self.pool
                        .resolve_version_set(set)
                        .constraint
                        .matches(version)
                });
            choices.push((!matches_installed, set));
        }
        // A stable sort keeps the declared order within each group.
        choices.sort_by_key(|&(not_installed, _)| not_installed);
        let mut sets = choices.into_iter().map(|(_, set)| set);
        let first = sets.next().context("require-any has no alternatives")?;
        Ok(self.pool.intern_version_set_union(first, sets))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        kdl_format::parse_manifest,
        model::digest,
        repo::{PackageInfo, Published},
    };

    fn published(name: &str, version: &str, body: &str) -> Result<Published> {
        let document = format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"{name}\" publisher=\"canopa\" version=\"{version}\" {{\n{body}\n}}\n"
        );
        Ok(serde_json::from_value(serde_json::json!({
            "sha256": digest(document.as_bytes()),
            "manifest": parse_manifest(&document)?,
            "document": document,
        }))?)
    }

    fn selected_names(
        packages: &[Published],
        wanted: &[&str],
        installed: &BTreeMap<String, Installed>,
    ) -> Result<BTreeSet<String>> {
        let mut candidates = BTreeMap::<_, Vec<&dyn PackageInfo>>::new();
        for package in packages {
            candidates
                .entry(package.manifest.name.as_str())
                .or_default()
                .push(package);
        }
        solve(
            &candidates,
            &wanted.iter().copied().collect(),
            installed,
            None,
            |selected| Ok(selected.keys().map(|name| name.to_string()).collect()),
        )
    }

    #[test]
    fn require_any_prefers_retained_providers_then_declared_order() -> Result<()> {
        let app = |first: &str, second: &str| {
            published(
                "desktop/app",
                "1.0.0-0",
                &format!(
                    "require-any {{\npackage \"{first}\"\npackage \"{second}\" version=\">= 1.0.0-0\"\n}}"
                ),
            )
        };
        let alpha = published("backend/alpha", "1.0.0-0", "")?;
        let zeta = published("backend/zeta", "1.0.0", "")?;
        let none = BTreeMap::new();
        for (first, second) in [
            ("backend/zeta", "backend/alpha"),
            ("backend/alpha", "backend/zeta"),
        ] {
            let packages = [app(first, second)?, alpha.clone(), zeta.clone()];
            let selected = selected_names(&packages, &["desktop/app"], &none)?;
            assert!(selected.contains(first), "{selected:?}");
            assert!(!selected.contains(second), "{selected:?}");
        }

        let installed = BTreeMap::from([(
            "backend/alpha".to_owned(),
            Installed {
                published: alpha.clone(),
                explicit: true,
            },
        )]);
        let packages = [app("backend/zeta", "backend/alpha")?, alpha, zeta];
        let selected = selected_names(&packages, &["desktop/app", "backend/alpha"], &installed)?;
        assert!(!selected.contains("backend/zeta"), "{selected:?}");
        Ok(())
    }

    #[test]
    fn capability_providers_prefer_wanted_then_name() -> Result<()> {
        let packages = [
            published(
                "mail/client",
                "1.0.0-0",
                "require-capability \"cap/mail-transport\"",
            )?,
            published("mail/zeta", "1.0.0-0", "provides \"cap/mail-transport\"")?,
            published("mail/alpha", "1.0.0-0", "provides \"cap/mail-transport\"")?,
        ];
        let none = BTreeMap::new();
        let selected = selected_names(&packages, &["mail/client"], &none)?;
        assert!(selected.contains("mail/alpha") && !selected.contains("mail/zeta"));
        let selected = selected_names(&packages, &["mail/client", "mail/zeta"], &none)?;
        assert!(!selected.contains("mail/alpha"), "{selected:?}");
        Ok(())
    }

    #[test]
    fn resolves_a_large_cycle() -> Result<()> {
        const COUNT: usize = 4096;
        let mut packages = Vec::<crate::repo::Published>::with_capacity(COUNT);
        for index in 0..COUNT {
            let next = (index + 1) % COUNT;
            let document = format!(
                "schema-version 1 kind=\"package-manifest\"\npackage \"system/chain-{index:04}\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n\
                 require \"system/chain-{next:04}\"\n}}\n"
            );
            packages.push(serde_json::from_value(serde_json::json!({
                "sha256": digest(document.as_bytes()),
                "manifest": parse_manifest(&document)?,
                "document": document,
            }))?);
        }
        let candidates = packages
            .iter()
            .map(|package| (package.manifest.name.as_str(), vec![package as Package<'_>]))
            .collect();
        let selected = solve(
            &candidates,
            &BTreeSet::from(["system/chain-0000"]),
            &BTreeMap::new(),
            None,
            Ok,
        )?;
        assert_eq!(selected.len(), COUNT);
        Ok(())
    }

    #[test]
    fn admits_thousands_of_recommendations_with_a_few_solves() -> Result<()> {
        const COUNT: usize = 2048;
        let package = |name: &str, body: &str| -> Result<crate::repo::Published> {
            let document = format!(
                "schema-version 1 kind=\"package-manifest\"\npackage \"{name}\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{body}}}\n"
            );
            Ok(serde_json::from_value(serde_json::json!({
                "sha256": digest(document.as_bytes()),
                "manifest": parse_manifest(&document)?,
                "document": document,
            }))?)
        };
        let mut body = String::new();
        let mut packages = Vec::new();
        for index in 0..COUNT {
            let name = format!("extras/plugin-{index:04}");
            body.push_str(&format!("    recommends \"{name}\"\n"));
            // Every other recommendation conflicts with the required runtime.
            let extra = if index % 64 == 0 {
                "    exclude \"library/runtime\"\n"
            } else {
                ""
            };
            packages.push(package(&name, extra)?);
        }
        packages.push(package("library/runtime", "")?);
        body.push_str("    require \"library/runtime\"\n");
        packages.push(package("apps/desktop", &body)?);
        let candidates = packages
            .iter()
            .map(|package| (package.manifest.name.as_str(), vec![package as Package<'_>]))
            .collect();
        let solves = Cell::new(0);
        let selected = solve(
            &candidates,
            &BTreeSet::from(["apps/desktop"]),
            &BTreeMap::new(),
            Some(&BTreeMap::new()),
            |selected| {
                solves.set(solves.get() + 1);
                Ok(selected
                    .keys()
                    .map(|name| name.to_string())
                    .collect::<BTreeSet<_>>())
            },
        )?;
        assert_eq!(selected.len(), 2 + COUNT - COUNT / 64);
        assert!(!selected.contains("extras/plugin-0000"));
        assert!(selected.contains("extras/plugin-0001"));
        // Binary splitting isolates each conflict in a logarithmic number of
        // solves, rather than one solve per recommendation.
        assert!(solves.get() < COUNT / 2, "{} solves", solves.get());
        Ok(())
    }
}

/// Conflicts found by composing earlier selections, as clauses for later solves.
#[derive(Default)]
struct Rejections {
    excluded: BTreeMap<SolvableId, StringId>,
    constraints: BTreeMap<SolvableId, Vec<VersionSetId>>,
    pairs: BTreeSet<(SolvableId, SolvableId)>,
    /// Requirements on escapes, which hold for the first owner of a conflict.
    requirements: BTreeMap<SolvableId, Vec<ConditionalRequirement>>,
    /// The candidates of each escape name, one per member of its conflict.
    escapes: BTreeMap<NameId, Candidates>,
    /// A label and the dependencies of each escape candidate.
    records: Vec<(String, KnownDependencies)>,
    combinations: BTreeSet<Vec<SolvableId>>,
}

impl Rejections {
    fn record(
        &mut self,
        universe: &Universe<'_>,
        selected: &BTreeMap<&str, SolvableId>,
        conflict: &PathConflict,
    ) -> Result<()> {
        let [owner, other] = &conflict.owners;
        let a = *selected
            .get(owner.as_str())
            .context("missing file owner in solver selection")?;
        let b = *selected
            .get(other.as_str())
            .context("missing file owner in solver selection")?;
        if !conflict.inheritable.is_empty()
            && let Some(members) =
                universe.contingency(&conflict.inheritable, [owner, other], selected)
        {
            return self.combination(universe, a, b, members, conflict);
        }
        if a == b {
            let reason = universe.pool.intern_string(conflict.to_string());
            ensure!(
                self.excluded.insert(a, reason).is_none(),
                "solver selected an excluded candidate"
            );
        } else {
            ensure!(
                self.pairs.insert((a.min(b), a.max(b))),
                "solver selected a rejected candidate pair"
            );
            let exclusion = without(universe, b, Some(conflict));
            self.constraints.entry(a).or_default().push(exclusion);
        }
        Ok(())
    }

    /// Rule out `a` and `b` together with the selected packages that the
    /// conflict depends on. Inherited rules can enable or disable the
    /// conflicting actions, so a selection that replaces or drops any of these
    /// packages remains possible. The clause `¬a ∨ ¬b ∨ ¬m…` is encoded as a
    /// requirement of `a` on an escape name whose candidates each rule out
    /// one other member.
    fn combination(
        &mut self,
        universe: &Universe<'_>,
        a: SolvableId,
        b: SolvableId,
        members: impl IntoIterator<Item = SolvableId>,
        conflict: &PathConflict,
    ) -> Result<()> {
        let mut others = BTreeSet::from([b]);
        others.extend(members);
        others.remove(&a);
        let mut key = Vec::from_iter(others.iter().copied());
        key.push(a);
        key.sort_unstable();
        ensure!(
            self.combinations.insert(key),
            "solver selected a rejected combination of candidates"
        );
        let short = |id: SolvableId| {
            let package = universe.package(id);
            format!("{}@{}", package.name(), package.version())
        };
        let [first, second] = &conflict.owners;
        let context = others
            .iter()
            .filter(|&&id| {
                let name = universe.package(id).name();
                name != first && name != second
            })
            .map(|&id| short(id))
            .collect::<Vec<_>>();
        let mut label = if context.is_empty() {
            conflict.to_string()
        } else {
            format!("{conflict} with {}", context.join(", "))
        };
        // Each clause needs its own name, because a name selects one candidate.
        let base = label.clone();
        let mut count = 1;
        while universe.pool.lookup_package_name(&label).is_some() {
            count += 1;
            label = format!("{base} ({count})");
        }
        let name = universe.pool.intern_package_name(label.as_str());
        let mut candidates = Vec::with_capacity(others.len());
        for id in others {
            let mut dependencies = KnownDependencies::default();
            dependencies.constrains.push(without(universe, id, None));
            let record = Record::Escape(self.records.len());
            self.records
                .push((format!("without {}", short(id)), dependencies));
            candidates.push(universe.pool.intern_solvable(name, record));
        }
        self.escapes.insert(
            name,
            Candidates {
                candidates,
                hint_dependencies_available: HintDependenciesAvailable::All,
                ..Candidates::default()
            },
        );
        let escape = universe.pool.intern_version_set(
            name,
            Filter {
                expression: Some("avoided".into()),
                ..Filter::default()
            },
        );
        self.requirements
            .entry(a)
            .or_default()
            .push(Requirement::from(escape).into());
        Ok(())
    }
}

/// A constraint that rules out the version of `id`.
fn without(universe: &Universe<'_>, id: SolvableId, reason: Option<&PathConflict>) -> VersionSetId {
    let version = universe.package(id).version();
    let expression = match reason {
        Some(conflict) => format!("={version} ({conflict})"),
        None => format!("={version}"),
    };
    let filter = Filter {
        expression: Some(expression),
        constraint: VersionConstraint::exact(version.clone()),
        exclude: true,
        capability: None,
    };
    let name = universe.pool.resolve_solvable(id).name;
    universe.pool.intern_version_set(name, filter)
}

struct Provider<'u, 'a> {
    universe: &'u Universe<'a>,
    rejections: &'u Rejections,
    budget: &'u Budget,
}

impl Interner for Provider<'_, '_> {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, id: SolvableId) -> impl Display + '_ {
        match self.universe.record(id) {
            Record::Package(_) => self.universe.package(id).version().to_string(),
            Record::Escape(index) => self.rejections.records[index].0.clone(),
        }
    }

    fn display_name(&self, id: NameId) -> impl Display + '_ {
        self.universe.pool.resolve_package_name(id)
    }

    fn display_version_set(&self, id: VersionSetId) -> impl Display + '_ {
        self.universe.pool.resolve_version_set(id)
    }

    fn display_string(&self, id: StringId) -> impl Display + '_ {
        self.universe.pool.resolve_string(id)
    }

    fn version_set_name(&self, id: VersionSetId) -> NameId {
        self.universe.pool.resolve_version_set_package_name(id)
    }

    fn solvable_name(&self, id: SolvableId) -> NameId {
        self.universe.pool.resolve_solvable(id).name
    }

    fn version_sets_in_union(&self, id: VersionSetUnionId) -> impl Iterator<Item = VersionSetId> {
        self.universe.pool.resolve_version_set_union(id)
    }

    fn resolve_condition(&self, id: ConditionId) -> Condition {
        self.universe.pool.resolve_condition(id).clone()
    }
}

impl DependencyProvider for Provider<'_, '_> {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let filter = self.universe.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|&id| {
                // An escape name has no versions, so every filter on it is "any version".
                let accepted = self
                    .universe
                    .candidate(id)
                    .is_none_or(|package| filter.accepts(package));
                accepted != inverse
            })
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        if let Some(escapes) = self.rejections.escapes.get(&name) {
            return Some(escapes.clone());
        }
        let mut candidates = self.universe.candidates.get(&name)?.clone();
        candidates
            .excluded
            .extend(candidates.candidates.iter().filter_map(|id| {
                self.rejections
                    .excluded
                    .get(id)
                    .map(|&reason| (*id, reason))
            }));
        Some(candidates)
    }

    async fn sort_candidates(&self, _cache: &SolverCache<Self>, candidates: &mut [SolvableId]) {
        // The common candidate policy assigned indices in preference order.
        candidates.sort_by_key(|&id| self.universe.pool.resolve_solvable(id).record);
    }

    async fn get_dependencies(&self, id: SolvableId) -> Dependencies {
        let index = match self.universe.record(id) {
            Record::Package(index) => index,
            Record::Escape(index) => {
                return Dependencies::Known(self.rejections.records[index].1.clone());
            }
        };
        let mut dependencies = self.universe.packages[index].1.clone();
        if let Some(constraints) = self.rejections.constraints.get(&id) {
            dependencies.constrains.extend(constraints);
        }
        if let Some(requirements) = self.rejections.requirements.get(&id) {
            dependencies.requirements.extend(requirements);
        }
        Dependencies::Known(dependencies)
    }

    fn should_cancel_with_value(&self) -> Option<Box<dyn Any>> {
        (!self.budget.spend()).then(|| Box::new(()) as Box<dyn Any>)
    }
}

pub(super) fn solve<'a, T>(
    candidates: &BTreeMap<&str, Vec<Package<'a>>>,
    wanted: &BTreeSet<&str>,
    installed: &BTreeMap<String, Installed>,
    recommendation_exclusions: Option<&BTreeMap<String, String>>,
    compose: impl Fn(BTreeMap<&'a str, Package<'a>>) -> Result<T>,
) -> Result<T> {
    let universe = Universe::new(candidates, wanted, installed)?;
    let mut requirements: Vec<_> = wanted
        .iter()
        .map(|&name| {
            let name = universe.pool.intern_package_name(name);
            universe
                .pool
                .intern_version_set(name, Filter::default())
                .into()
        })
        .collect();
    let mut rejections = Rejections::default();
    let required = Budget::new(REQUIRED_WORK);
    let (mut selected, mut composed) = select(
        &universe,
        &requirements,
        &mut rejections,
        &required,
        false,
        &compose,
    )?
    .expect("required solve reports failure as an error");
    let Some(excluded) = recommendation_exclusions else {
        return Ok(composed);
    };

    // Lock the required solution, then admit recommendations in rounds.
    // Recommendations can add a closure, never replace a locked selection.
    let mut search = Recommendations {
        universe: &universe,
        rejections,
        budget: Budget::new(RECOMMENDED_WORK),
        compose: &compose,
    };
    requirements.clear();
    let mut locked = BTreeSet::new();
    let mut attempted = BTreeSet::new();
    loop {
        let mut pending = Vec::new();
        for (&name, &id) in &selected {
            if !locked.insert(name) {
                continue;
            }
            requirements.push(universe.exact(id));
            if excluded.contains_key(name) {
                continue;
            }
            let old = installed.get(name).map(|entry| &entry.published.manifest);
            for dependency in universe.package(id).dependencies() {
                let Dependency::Recommend(target) = dependency else {
                    continue;
                };
                if old.is_some_and(|manifest| manifest.dependency.contains(dependency)) {
                    // Respect prior omissions, but keep a matching installed
                    // recommendation when recalculating the image closure.
                    let Some(entry) = installed.get(&target.package) else {
                        continue;
                    };
                    if !target.matches(&entry.published.manifest.version)? {
                        continue;
                    }
                }
                pending.push(target);
            }
        }
        pending.retain(|target| {
            let name = target.package.as_str();
            !selected.contains_key(name)
                && candidates.contains_key(name)
                && attempted.insert((name, target.version.as_deref()))
        });
        let mut accepted = None;
        search.admit(&mut requirements, &pending, &mut accepted)?;
        let Some(next) = accepted else {
            return Ok(composed);
        };
        (selected, composed) = next;
    }
}

type Selection<'a> = BTreeMap<&'a str, SolvableId>;

struct Recommendations<'u, 'a, C> {
    universe: &'u Universe<'a>,
    rejections: Rejections,
    budget: Budget,
    compose: &'u C,
}

impl<'a, T, C: Fn(BTreeMap<&'a str, Package<'a>>) -> Result<T>> Recommendations<'_, 'a, C> {
    /// Admit a batch of recommendations with one solve when the whole batch
    /// fits, and split a failing batch in order, so earlier recommendations
    /// win conflicts. Accepted targets stay in `requirements`.
    fn admit(
        &mut self,
        requirements: &mut Vec<ConditionalRequirement>,
        batch: &[&DependencyTarget],
        accepted: &mut Option<(Selection<'a>, T)>,
    ) -> Result<()> {
        if batch.is_empty() || self.budget.exhausted() {
            return Ok(());
        }
        let base = requirements.len();
        for target in batch {
            requirements.push(self.universe.target(target, false)?.into());
        }
        if let Some(next) = select(
            self.universe,
            requirements,
            &mut self.rejections,
            &self.budget,
            true,
            self.compose,
        )? {
            *accepted = Some(next);
            return Ok(());
        }
        requirements.truncate(base);
        if let [_] = batch {
            return Ok(());
        }
        let (first, second) = batch.split_at(batch.len() / 2);
        self.admit(requirements, first, accepted)?;
        self.admit(requirements, second, accepted)
    }
}

fn select<'a, T>(
    universe: &Universe<'a>,
    requirements: &[ConditionalRequirement],
    rejections: &mut Rejections,
    budget: &Budget,
    optional: bool,
    compose: &impl Fn(BTreeMap<&'a str, Package<'a>>) -> Result<T>,
) -> Result<Option<(Selection<'a>, T)>> {
    loop {
        let ids = {
            let mut solver = Solver::new(Provider {
                universe,
                rejections,
                budget,
            });
            match solver.solve(Problem::new().requirements(requirements.to_vec())) {
                Ok(ids) => ids,
                Err(UnsolvableOrCancelled::Unsolvable(_) | UnsolvableOrCancelled::Cancelled(_))
                    if optional =>
                {
                    return Ok(None);
                }
                Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
                    bail!(
                        "cannot satisfy package constraints\n{}",
                        conflict.display_user_friendly(&solver)
                    );
                }
                Err(UnsolvableOrCancelled::Cancelled(_)) => {
                    bail!(
                        "package resolution exceeded its limit of {} solver steps",
                        budget.limit
                    );
                }
            }
        };
        let selected = ids
            .into_iter()
            .filter_map(|id| Some((universe.candidate(id)?.name(), id)))
            .collect::<BTreeMap<_, _>>();
        let manifests = selected
            .iter()
            .map(|(&name, &id)| (name, universe.package(id)))
            .collect();
        match compose(manifests) {
            Ok(composed) => return Ok(Some((selected, composed))),
            Err(error) => {
                let Some(conflict) = error.downcast_ref::<PathConflict>() else {
                    if optional {
                        return Ok(None);
                    }
                    return Err(error);
                };
                // A path conflict rules out these exact candidates while preserving the rest
                // of the version search. Most transactions need just one solve.
                rejections.record(universe, &selected, conflict)?;
            }
        }
    }
}
