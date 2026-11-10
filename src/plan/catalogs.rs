use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};

use super::{Plan, Renames, Retention, resolve};
use crate::{
    attestation::BuildTrust,
    image::{Image, Installed, Publisher, State, lock::ImageLock},
    model::{PackageSpec, Version},
    repo::{
        BuildEvidence, BuildReceipt, Candidate, CatalogCheckpoint, PackageInfo, Published,
        ReleaseProfile, candidates,
    },
};

#[derive(Default)]
pub(super) struct Catalogs {
    pub pins: BTreeMap<String, String>,
    pub candidates: CandidateSet<Candidate>,
    pub renames: Renames,
    pub profile: Option<ReleaseProfile>,
    pub checkpoints: BTreeMap<String, CatalogCheckpoint>,
    builds: BTreeMap<String, SelectedBuilds>,
}

struct SelectedBuilds {
    evidence: BuildEvidence,
    profile_trust: Option<BuildTrust>,
}

impl SelectedBuilds {
    fn verify_retained(
        &self,
        image: &Image,
        publisher: &Publisher,
        package: &Published,
    ) -> Result<Option<BuildReceipt>> {
        let Some(trust) = &self.profile_trust else {
            return Ok(None);
        };
        self.evidence.check_keys(trust)?;
        if let Some((receipt, envelope)) = image.archived_build(&package.sha256)?
            && self.evidence.check_receipt(package, &receipt).is_ok()
            && receipt
                .verify(&envelope, package, &publisher.key, trust)
                .is_ok()
        {
            return Ok(None);
        }
        self.evidence
            .verify(&publisher.repo, [trust], package)
            .map(Some)
            .with_context(|| {
                format!(
                    "checking retained {} against signed build trust",
                    package.manifest.id()
                )
            })
    }
}

impl Catalogs {
    pub(super) fn verify_builds(
        &self,
        image: &Image,
        state: &State,
        installed: &BTreeMap<String, Installed>,
    ) -> Result<Vec<BuildReceipt>> {
        let mut receipts = Vec::new();
        for (name, package) in installed {
            let publisher = state
                .publishers
                .get(&package.published.manifest.publisher)
                .context("missing publisher for selected package")?;
            let Some(builds) = self.builds.get(&publisher.name) else {
                continue;
            };
            let retained = state
                .installed
                .get(name)
                .is_some_and(|previous| previous.published.sha256 == package.published.sha256);
            let receipt = if retained {
                builds.verify_retained(image, publisher, &package.published)?
            } else {
                builds.evidence.verify_policies(
                    &publisher.repo,
                    &package.published,
                    builds.profile_trust.as_ref(),
                    publisher.build_trust.as_ref(),
                )?
            };
            receipts.extend(receipt);
        }
        Ok(receipts)
    }
}

pub(crate) struct CandidateSet<P> {
    pub packages: Vec<P>,
    pub obsoletions: BTreeMap<String, Version>,
}

pub(crate) fn claim_source<'a>(
    sources: &mut BTreeMap<String, &'a str>,
    publisher: &'a str,
    name: &str,
) -> bool {
    *sources.entry(name.to_owned()).or_insert(publisher) == publisher
}

impl<P> Default for CandidateSet<P> {
    fn default() -> Self {
        Self {
            packages: Vec::new(),
            obsoletions: BTreeMap::new(),
        }
    }
}

impl<P: PackageInfo> CandidateSet<P> {
    pub(crate) fn replay(&self, profile: &ReleaseProfile) -> Result<BTreeMap<String, Installed>> {
        let mut installed = BTreeMap::new();
        for package in &self.packages {
            let name = package.name();
            if profile.composition.get(name).map(String::as_str) != Some(package.sha256()) {
                continue;
            }
            package.check_architecture(&profile.architecture)?;
            ensure!(
                self.obsoletions
                    .get(name)
                    .is_none_or(|cutoff| package.version().semantic() > cutoff.semantic()),
                "signed profile package {} is obsolete",
                package.id()
            );
            let entry = Installed {
                published: package.load()?,
                explicit: profile.roots.contains_key(name),
            };
            ensure!(
                installed.insert(name.to_owned(), entry).is_none(),
                "duplicate manifest for signed profile package {name}"
            );
        }
        for (name, hash) in &profile.composition {
            ensure!(
                installed.contains_key(name),
                "signed profile package {name} with manifest {hash} is unavailable"
            );
        }
        resolve::check(&installed).context("checking dependencies of the signed composition")?;
        Ok(installed)
    }

    pub(crate) fn update_profile<T>(
        &self,
        before: &BTreeMap<String, Installed>,
        profile: &ReleaseProfile,
        freezes: &BTreeMap<String, super::FrozenPackage>,
        local_roots: Option<&BTreeSet<String>>,
        renames: &BTreeMap<&str, PackageSpec<'_>>,
        install_recommends: bool,
        compose: impl Fn(&BTreeMap<String, Installed>) -> Result<T>,
    ) -> Result<(BTreeMap<String, Installed>, T)> {
        let locked = self.replay(profile)?;
        super::freeze::check(freezes, &locked)?;
        let mut requested = renames.clone();
        for (name, package) in &locked {
            let manifest = &package.published.manifest;
            if let Some(previous) = before.get(name) {
                let previous = &previous.published.manifest;
                ensure!(
                    previous.publisher == manifest.publisher,
                    "signed profile would change the publisher of {}",
                    previous.id()
                );
                ensure!(
                    previous.version.semantic() <= manifest.version.semantic(),
                    "signed profile would downgrade {} to {}",
                    previous.id(),
                    manifest.version
                );
            }
            requested.insert(
                name.as_str(),
                PackageSpec {
                    publisher: Some(&manifest.publisher),
                    name,
                    version: Some(manifest.version.clone()),
                },
            );
        }
        let packages = self.packages.iter().filter(|pkg| {
            profile
                .composition
                .get(pkg.name())
                .is_none_or(|hash| hash == pkg.sha256())
        });
        let (mut installed, paths) = resolve::resolve(
            before,
            resolve::Candidates {
                packages,
                obsoletions: &self.obsoletions,
                freezes,
                profile_packages: &profile.composition,
            },
            &requested,
            true,
            local_roots.map_or(Retention::All, Retention::Names),
            install_recommends,
            compose,
        )?;
        for (name, package) in &mut installed {
            package.explicit = profile.roots.contains_key(name)
                || local_roots.map_or_else(
                    || before.get(name).is_some_and(|old| old.explicit),
                    |roots| roots.contains(name),
                );
        }
        Ok((installed, paths))
    }

    pub(crate) fn add_obsoletions<'a>(
        &mut self,
        sources: &mut BTreeMap<String, &'a str>,
        publisher: &'a str,
        obsoletions: &BTreeMap<String, Version>,
    ) {
        for (name, cutoff) in obsoletions {
            if claim_source(sources, publisher, name) {
                self.obsoletions.insert(name.clone(), cutoff.clone());
            }
        }
    }

    pub(crate) fn add_packages<'a>(
        &mut self,
        sources: &mut BTreeMap<String, &'a str>,
        publisher: &'a str,
        architecture: &str,
        packages: impl IntoIterator<Item = P>,
    ) {
        self.packages.extend(packages.into_iter().filter(|pkg| {
            pkg.supports_architecture(architecture) && claim_source(sources, publisher, pkg.name())
        }));
    }
}

pub(super) fn requested<'a>(
    state: &State,
    packages: &'a [String],
) -> Result<BTreeMap<&'a str, PackageSpec<'a>>> {
    ensure!(!packages.is_empty(), "request requires a package name");
    let mut requested = BTreeMap::new();
    for package in packages {
        let spec = PackageSpec::parse(package)
            .with_context(|| format!("invalid requested package {package:?}"))?;
        if let Some(publisher) = spec.publisher {
            ensure!(
                state.publishers.get(publisher).is_some(),
                "publisher {publisher} is not configured in this image"
            );
            if let Some(installed) = state.installed.get(spec.name) {
                let current = &installed.published.manifest.publisher;
                ensure!(
                    current == publisher,
                    "{} is installed from sticky publisher {current}; remove it before selecting {publisher}",
                    spec.name
                );
            }
        }
        let previous = requested.insert(spec.name, spec.clone());
        ensure!(
            previous.is_none_or(|previous| previous == spec),
            "conflicting selectors for {}",
            spec.name
        );
    }
    Ok(requested)
}

pub(super) fn load(
    image: &Image,
    state: &State,
    requested: &BTreeMap<&str, PackageSpec<'_>>,
    saved: Option<&Plan>,
    updating: bool,
    deferred_publishers: &BTreeSet<String>,
    rollout: super::update::Rollout<'_>,
    lock: Option<&ImageLock>,
) -> Result<Catalogs> {
    let publishers = state.publishers.as_slice();
    let omit_catalog = |name: &str| deferred_publishers.contains(name);
    if let Some(saved) = saved {
        ensure!(
            saved.catalogs.len()
                == publishers
                    .iter()
                    .filter(|publisher| !omit_catalog(&publisher.name))
                    .count(),
            "saved plan's catalogs do not match the image's publishers"
        );
    }

    // Installed names keep their source. For other names only the first
    // publisher that offers the package contributes candidates to the solver.
    let mut sources: BTreeMap<_, _> = state
        .installed
        .iter()
        .map(|(name, pkg)| (name.clone(), pkg.published.manifest.publisher.as_str()))
        .collect();
    sources.extend(requested.values().filter_map(|spec| {
        spec.publisher
            .map(|publisher| (spec.name.to_owned(), publisher))
    }));
    let saved_pin = |publisher: &str| -> Result<Option<&str>> {
        match saved {
            None => Ok(None),
            Some(plan) => {
                Ok(Some(plan.catalogs.get(publisher).with_context(|| {
                    format!("saved plan has no catalog for {publisher}")
                })?))
            }
        }
    };
    let selected_pin = |publisher: &str| -> Result<Option<&str>> {
        let saved_pin = saved_pin(publisher)?;
        let Some(lock) = lock else {
            return Ok(saved_pin);
        };
        let pin = lock.catalog(publisher)?;
        ensure!(
            saved_pin.is_none_or(|saved| saved == pin),
            "saved plan catalog for {publisher} differs from the image lock"
        );
        Ok(Some(pin))
    };
    let profile = image.select_profile(
        &state.publishers,
        &state.settings,
        selected_pin(&publishers[0].name)?,
    )?;
    if updating && let Some(profile) = &profile {
        if let Some(advisories) = rollout.advisories {
            *advisories = super::update::matching_advisories(state, &profile.record)?;
        }
        if !rollout.force {
            let primary = profile
                .pin(&publishers[0].name)
                .context("missing primary profile pin")?;
            super::update::check_rollout(image, state, primary, &profile.record)?;
        }
    }
    let mut catalogs = Catalogs::default();
    let mut snapshots = Vec::new();
    for publisher in publishers {
        if omit_catalog(&publisher.name) {
            if let Some(previous) = state.settings.catalog_checkpoints.get(&publisher.name) {
                catalogs
                    .checkpoints
                    .insert(publisher.name.clone(), previous.clone());
            }
            continue;
        }
        let pin = selected_pin(&publisher.name)?;
        let signed = profile
            .as_ref()
            .and_then(|profile| profile.pin(&publisher.name));
        let snapshot = if let Some(required) = signed {
            ensure!(
                pin.is_none_or(|pin| pin == required.sha256),
                "{} catalog for {} differs from the signed release profile",
                if lock.is_some() {
                    "image lock"
                } else {
                    "saved plan"
                },
                publisher.name
            );
            required.candidates(&publisher.repo, &publisher.name, &publisher.key)
        } else {
            candidates(&publisher.repo, &publisher.name, &publisher.key, pin)
        }
        .with_context(|| format!("reading signed catalog for {}", publisher.name))?;
        image.check_catalog(
            &state.publishers,
            &state.settings,
            publisher,
            &snapshot.builds.history,
        )?;
        if !deferred_publishers.contains(&publisher.name) {
            catalogs.renames.add(
                &state.installed,
                &publisher.name,
                &snapshot.rename_targets,
                &snapshot.obsoletions,
            );
        }
        snapshots.push((publisher, signed, snapshot));
    }
    catalogs.renames.check_request(updating)?;
    catalogs.renames.reserve_sources(&mut sources)?;
    for (publisher, signed, snapshot) in snapshots {
        catalogs
            .checkpoints
            .insert(publisher.name.clone(), snapshot.builds.history.clone());
        catalogs
            .pins
            .insert(publisher.name.clone(), snapshot.sha256.clone());
        if deferred_publishers.contains(&publisher.name) {
            continue;
        }
        catalogs
            .candidates
            .add_obsoletions(&mut sources, &publisher.name, &snapshot.obsoletions);
        for spec in requested.values() {
            if let Some(version) = &spec.version
                && sources.get(spec.name) == Some(&publisher.name.as_str())
                && snapshot
                    .obsoletions
                    .get(spec.name)
                    .is_none_or(|cutoff| version.semantic() > cutoff.semantic())
            {
                let id = format!("cps://{}/{}@{version}", publisher.name, spec.name);
                ensure!(!snapshot.is_withdrawn(&id), "{id} was withdrawn");
            }
        }
        catalogs.candidates.add_packages(
            &mut sources,
            &publisher.name,
            &state.settings.arch,
            snapshot.packages,
        );
        let profile_trust = signed.and_then(|pin| pin.build_trust.clone());
        if publisher.build_trust.is_some() || profile_trust.is_some() {
            catalogs.builds.insert(
                publisher.name.clone(),
                SelectedBuilds {
                    evidence: snapshot.builds,
                    profile_trust,
                },
            );
        }
    }
    catalogs.profile = profile.map(|profile| profile.record);
    Ok(catalogs)
}
