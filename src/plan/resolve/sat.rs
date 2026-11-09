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
    model::{Dependency, DependencyTarget, Manifest, Version, VersionConstraint},
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
    type V = usize;
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
const REQUIRED_WORK: u64 = 4_000_000;
/// Recommendations share a separate budget. When it runs out, the remaining
/// recommendations are skipped, at the same point on every machine.
const RECOMMENDED_WORK: u64 = 4_000_000;

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
                solvables.push(universe.pool.intern_solvable(name, universe.packages.len()));
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

    fn package(&self, id: SolvableId) -> Package<'a> {
        self.packages[self.pool.resolve_solvable(id).record].0
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
            choices.push((
                !matches_installed,
                target.package.as_str(),
                target.version.as_deref(),
                set,
            ));
        }
        choices.sort_unstable();
        let mut sets = choices.into_iter().map(|(_, _, _, set)| set);
        let first = sets.next().context("require-any has no alternatives")?;
        Ok(self.pool.intern_version_set_union(first, sets))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{kdl_format::parse_manifest, model::digest};

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

#[derive(Default)]
struct Rejections {
    excluded: BTreeMap<SolvableId, StringId>,
    constraints: BTreeMap<SolvableId, Vec<VersionSetId>>,
    pairs: BTreeSet<(SolvableId, SolvableId)>,
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
            let version = universe.package(b).version();
            let exact = format!("={version}");
            let filter = Filter {
                expression: Some(format!("{exact} ({conflict})")),
                constraint: VersionConstraint::parse(Some(&exact))?,
                exclude: true,
                capability: None,
            };
            let name = universe.pool.resolve_solvable(b).name;
            let exclusion = universe.pool.intern_version_set(name, filter);
            self.constraints.entry(a).or_default().push(exclusion);
        }
        Ok(())
    }
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
        self.universe.package(id).version()
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
            .filter(|&id| filter.accepts(self.universe.package(id)) != inverse)
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
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
        let mut dependencies = self.universe.packages
            [self.universe.pool.resolve_solvable(id).record]
            .1
            .clone();
        if let Some(constraints) = self.rejections.constraints.get(&id) {
            dependencies.constrains.extend(constraints);
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
            .map(|id| (universe.package(id).name(), id))
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
