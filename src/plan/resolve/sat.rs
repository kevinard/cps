use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use resolvo::{
    Candidates, Condition, ConditionId, Dependencies, DependencyProvider,
    HintDependenciesAvailable, Interner, KnownDependencies, NameId, Problem, SolvableId, Solver,
    SolverCache, StringId, UnsolvableOrCancelled, VersionSetId, VersionSetUnionId,
    utils::{Pool, VersionSet},
};

use crate::{
    model::{DependencyKind, VersionConstraint},
    plan::paths::PathConflict,
    repo::Published,
};

#[derive(Clone, Default, Eq, Hash, PartialEq)]
struct Filter {
    expression: Option<String>,
    constraint: VersionConstraint,
    exclude: bool,
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

const TIME_LIMIT: Duration = Duration::from_secs(60);

struct Universe<'a> {
    pool: Pool<Filter>,
    candidates: BTreeMap<NameId, Candidates>,
    packages: Vec<(&'a Published, KnownDependencies)>,
}

impl<'a> Universe<'a> {
    fn new(candidates: &BTreeMap<&str, Vec<&'a Published>>) -> Result<Self> {
        let mut universe = Self {
            pool: Pool::new(),
            candidates: BTreeMap::new(),
            packages: Vec::new(),
        };
        for (&name, versions) in candidates {
            let name = universe.pool.intern_package_name(name);
            let mut solvables = Vec::with_capacity(versions.len());
            for &package in versions {
                let mut dependencies = KnownDependencies::default();
                for dependency in &package.manifest.dependency {
                    let name = universe
                        .pool
                        .intern_package_name(dependency.package.as_str());
                    let filter = Filter {
                        expression: dependency.version.clone(),
                        constraint: VersionConstraint::parse(dependency.version.as_deref())?,
                        exclude: dependency.kind == DependencyKind::Exclude,
                    };
                    let set = universe.pool.intern_version_set(name, filter);
                    match dependency.kind {
                        DependencyKind::Require => dependencies.requirements.push(set.into()),
                        DependencyKind::Incorporate | DependencyKind::Exclude => {
                            dependencies.constrains.push(set);
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

    fn package(&self, id: SolvableId) -> &'a Published {
        self.packages[self.pool.resolve_solvable(id).record].0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{kdl_format::parse_manifest, model::digest};

    #[test]
    fn resolves_a_large_cycle() -> Result<()> {
        const COUNT: usize = 4096;
        let mut packages = Vec::<Published>::with_capacity(COUNT);
        for index in 0..COUNT {
            let next = (index + 1) % COUNT;
            let document = format!(
                "schema-version 1\npackage \"system/chain-{index:04}\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n\
                 require \"system/chain-{next:04}\"\n}}\n"
            );
            packages.push(serde_json::from_value(serde_json::json!({
                "sha256": digest(document.as_bytes()),
                "manifest": parse_manifest(&document)?,
            }))?);
        }
        let candidates = packages
            .iter()
            .map(|package| (package.manifest.name.as_str(), vec![package]))
            .collect();
        let selected = solve(&candidates, &BTreeSet::from(["system/chain-0000"]), Ok)?;
        assert_eq!(selected.len(), COUNT);
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
            let version = universe.package(b).manifest.version;
            let exact = format!("={version}");
            let filter = Filter {
                expression: Some(format!("{exact} ({conflict})")),
                constraint: VersionConstraint::parse(Some(&exact))?,
                exclude: true,
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
    deadline: Instant,
}

impl Interner for Provider<'_, '_> {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, id: SolvableId) -> impl Display + '_ {
        self.universe.package(id).manifest.version
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
                let version = self.universe.package(id).manifest.version;
                (filter.constraint.matches(version) != filter.exclude) != inverse
            })
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
        (Instant::now() >= self.deadline).then(|| Box::new(()) as Box<dyn Any>)
    }
}

pub(super) fn solve<'a, T>(
    candidates: &BTreeMap<&str, Vec<&'a Published>>,
    wanted: &BTreeSet<&str>,
    compose: impl Fn(BTreeMap<&'a str, &'a Published>) -> Result<T>,
) -> Result<T> {
    let deadline = Instant::now() + TIME_LIMIT;
    let universe = Universe::new(candidates)?;
    let requirements: Vec<_> = wanted
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
    loop {
        ensure!(
            Instant::now() < deadline,
            "package resolution exceeded {} seconds",
            TIME_LIMIT.as_secs()
        );
        let ids = {
            let mut solver = Solver::new(Provider {
                universe: &universe,
                rejections: &rejections,
                deadline,
            });
            match solver.solve(Problem::new().requirements(requirements.clone())) {
                Ok(ids) => ids,
                Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
                    bail!(
                        "cannot satisfy package constraints\n{}",
                        conflict.display_user_friendly(&solver)
                    );
                }
                Err(UnsolvableOrCancelled::Cancelled(_)) => {
                    bail!(
                        "package resolution exceeded {} seconds",
                        TIME_LIMIT.as_secs()
                    );
                }
            }
        };
        let selected = ids
            .into_iter()
            .map(|id| (universe.package(id).manifest.name.as_str(), id))
            .collect::<BTreeMap<_, _>>();
        let manifests = selected
            .iter()
            .map(|(&name, &id)| (name, universe.package(id)))
            .collect();
        match compose(manifests) {
            Ok(composed) => return Ok(composed),
            Err(error) => {
                let Some(conflict) = error.downcast_ref::<PathConflict>() else {
                    return Err(error);
                };
                // A path conflict rules out these exact candidates while preserving the rest
                // of the version search. Most transactions need just one solve.
                rejections.record(&universe, &selected, conflict)?;
            }
        }
    }
}
