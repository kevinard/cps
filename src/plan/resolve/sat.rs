use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Display},
};

use anyhow::{Result, bail};
use resolvo::{
    Candidates, Condition, ConditionId, Dependencies, DependencyProvider,
    HintDependenciesAvailable, Interner, KnownDependencies, NameId, Problem, SolvableId, Solver,
    SolverCache, StringId, UnsolvableOrCancelled, VersionSetId, VersionSetUnionId,
    utils::{Pool, VersionSet},
};

use crate::{
    model::{DependencyKind, VersionConstraint},
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

struct Provider<'a> {
    pool: Pool<Filter>,
    candidates: BTreeMap<NameId, Candidates>,
    packages: Vec<(&'a Published, KnownDependencies)>,
}

impl<'a> Provider<'a> {
    fn new(candidates: &BTreeMap<&str, Vec<&'a Published>>) -> Result<Self> {
        let mut provider = Self {
            pool: Pool::new(),
            candidates: BTreeMap::new(),
            packages: Vec::new(),
        };
        for (&name, versions) in candidates {
            let name = provider.pool.intern_package_name(name);
            let mut solvables = Vec::with_capacity(versions.len());
            for &package in versions {
                let mut dependencies = KnownDependencies::default();
                for dependency in &package.manifest.dependency {
                    let name = provider
                        .pool
                        .intern_package_name(dependency.package.as_str());
                    let filter = Filter {
                        expression: dependency.version.clone(),
                        constraint: VersionConstraint::parse(dependency.version.as_deref())?,
                        exclude: dependency.kind == DependencyKind::Exclude,
                    };
                    let set = provider.pool.intern_version_set(name, filter);
                    match dependency.kind {
                        DependencyKind::Require => dependencies.requirements.push(set.into()),
                        DependencyKind::Incorporate | DependencyKind::Exclude => {
                            dependencies.constrains.push(set);
                        }
                    }
                }
                solvables.push(provider.pool.intern_solvable(name, provider.packages.len()));
                provider.packages.push((package, dependencies));
            }
            provider.candidates.insert(
                name,
                Candidates {
                    candidates: solvables,
                    hint_dependencies_available: HintDependenciesAvailable::All,
                    ..Candidates::default()
                },
            );
        }
        Ok(provider)
    }

    fn package(&self, id: SolvableId) -> &'a Published {
        self.packages[self.pool.resolve_solvable(id).record].0
    }
}

impl Interner for Provider<'_> {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, id: SolvableId) -> impl Display + '_ {
        self.package(id).manifest.version
    }

    fn display_name(&self, id: NameId) -> impl Display + '_ {
        self.pool.resolve_package_name(id)
    }

    fn display_version_set(&self, id: VersionSetId) -> impl Display + '_ {
        self.pool.resolve_version_set(id)
    }

    fn display_string(&self, id: StringId) -> impl Display + '_ {
        self.pool.resolve_string(id)
    }

    fn version_set_name(&self, id: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(id)
    }

    fn solvable_name(&self, id: SolvableId) -> NameId {
        self.pool.resolve_solvable(id).name
    }

    fn version_sets_in_union(&self, id: VersionSetUnionId) -> impl Iterator<Item = VersionSetId> {
        self.pool.resolve_version_set_union(id)
    }

    fn resolve_condition(&self, id: ConditionId) -> Condition {
        self.pool.resolve_condition(id).clone()
    }
}

impl DependencyProvider for Provider<'_> {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let filter = self.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|&id| {
                let version = self.package(id).manifest.version;
                (filter.constraint.matches(version) != filter.exclude) != inverse
            })
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        self.candidates.get(&name).cloned()
    }

    async fn sort_candidates(&self, _cache: &SolverCache<Self>, candidates: &mut [SolvableId]) {
        // The common candidate policy assigned indices in preference order.
        candidates.sort_by_key(|&id| self.pool.resolve_solvable(id).record);
    }

    async fn get_dependencies(&self, id: SolvableId) -> Dependencies {
        Dependencies::Known(
            self.packages[self.pool.resolve_solvable(id).record]
                .1
                .clone(),
        )
    }
}

pub(super) fn solve<'a>(
    candidates: &BTreeMap<&str, Vec<&'a Published>>,
    wanted: &BTreeSet<&str>,
) -> Result<BTreeMap<&'a str, &'a Published>> {
    let provider = Provider::new(candidates)?;
    let requirements = wanted
        .iter()
        .map(|&name| {
            let name = provider.pool.intern_package_name(name);
            provider
                .pool
                .intern_version_set(name, Filter::default())
                .into()
        })
        .collect();
    let mut solver = Solver::new(provider);
    let ids = match solver.solve(Problem::new().requirements(requirements)) {
        Ok(ids) => ids,
        Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
            bail!(
                "cannot satisfy package constraints\n{}",
                conflict.display_user_friendly(&solver)
            );
        }
        Err(UnsolvableOrCancelled::Cancelled(_)) => bail!("package resolution was cancelled"),
    };
    Ok(ids
        .into_iter()
        .map(|id| {
            let package = solver.provider().package(id);
            (package.manifest.name.as_str(), package)
        })
        .collect())
}
