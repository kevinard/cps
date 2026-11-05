use std::{
    collections::BTreeMap,
    fs, iter,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::{
    kdl_format::{PublisherConfig, read_image_definition},
    model::PackageSpec,
    plan::{CandidateSet, claim_source},
    repo::{
        CatalogPin, ReleaseProfile, profiles::profile_paths, read_catalog, read_current,
        read_package, release_profile,
    },
};

use super::{
    ImageGuard, Mirror, SyncSelection, SyncSource, cache::record_profile_pins, digest, lock_repo,
    record_catalog_checkpoint, stage_release,
};

/// Materialize a signed profile's catalog set before advancing its primary pointer.
pub fn sync_profile(config: &Path, index_only: bool) -> Result<String> {
    synchronize_profile(config, index_only, None)
}

pub fn sync_profile_for_image(config: &Path, index_only: bool, root: &Path) -> Result<String> {
    synchronize_profile(config, index_only, Some(ImageGuard::open(root)?))
}

fn synchronize_profile(
    config: &Path,
    index_only: bool,
    guard: Option<ImageGuard>,
) -> Result<String> {
    let definition = read_image_definition(config)?;
    let name = definition
        .release_profile
        .as_deref()
        .context("repo sync-profile requires a release-profile in the image configuration")?;
    let sources = definition
        .publishers
        .iter()
        .map(|publisher| {
            let mirror = definition
                .mirrors
                .get(&publisher.name)
                .with_context(|| format!("missing mirror for publisher {}", publisher.name))?;
            SyncSource::new(
                &mirror.root,
                Mirror::new(&mirror.metadata, &mirror.targets)?,
                &publisher.name,
                &publisher.trust_key,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(guard) = &guard {
        let state = &guard.state;
        ensure!(
            state.publishers.release_profile() == Some(name)
                && state.settings.arch == definition.arch
                && state.settings.facets == definition.facets
                && state
                    .publishers
                    .as_slice()
                    .iter()
                    .map(|publisher| publisher.name.as_str())
                    .eq(sources.iter().map(|source| source.publisher)),
            "sync configuration differs from the installed image profile or publisher order"
        );
        ensure!(
            state
                .settings
                .bootstrap_catalog
                .as_ref()
                .is_none_or(|hash| definition.bootstrap_catalog.as_ref() == Some(hash)),
            "sync configuration differs from the image's pinned bootstrap catalog"
        );
        for source in &sources {
            guard.check_source(source)?;
        }
    }
    let repositories = definition
        .publishers
        .iter()
        .map(|publisher| {
            fs::create_dir_all(&publisher.repository)?;
            fs::canonicalize(&publisher.repository)
                .with_context(|| format!("opening cache for publisher {}", publisher.name))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut lock_order = repositories.iter().collect::<Vec<_>>();
    lock_order.sort_unstable();
    ensure!(
        !lock_order
            .windows(2)
            .any(|paths| paths[1].starts_with(paths[0])),
        "publishers must use distinct, non-nested cache directories"
    );
    let _locks = lock_order
        .into_iter()
        .map(|repo| lock_repo(repo))
        .collect::<Result<Vec<_>>>()?;

    let members = sources
        .iter()
        .zip(&repositories)
        .map(|(source, repo)| (source.publisher, repo))
        .collect::<Vec<_>>();
    let binding = format!("{}\n", digest(&serde_json::to_vec(&(name, members))?));
    let (primary, companions) = sources.split_first().context("missing primary publisher")?;
    let (primary_repo, companion_repos) = repositories
        .split_first()
        .context("missing primary cache")?;
    let full = !index_only;
    let bootstrap_pin = definition
        .bootstrap_catalog
        .as_ref()
        .map(|hash| CatalogPin::primary(primary.publisher.to_owned(), hash.clone()));
    let primary_release = stage_release(
        primary_repo,
        primary,
        full,
        SyncSelection::Current {
            binding: Some(&binding),
            required: bootstrap_pin.as_ref().as_slice(),
            guard: guard.as_ref(),
            defer_checkpoint: guard.is_some(),
        },
    )
    .with_context(|| format!("staging primary publisher {}", primary.publisher))?;
    let current_pin = CatalogPin::primary(
        primary.publisher.to_owned(),
        primary_release.current.clone(),
    );
    let pins = iter::once(current_pin)
        .chain(bootstrap_pin.filter(|pin| pin.sha256 != primary_release.current));
    let mut profiles = Vec::new();
    for pin in pins {
        let (_, profile) = release_profile(
            primary_repo,
            primary.publisher,
            &primary.key,
            Some(&pin.sha256),
            name,
        )?;
        profile.check_image(
            name,
            &definition.arch,
            &definition.facets,
            companions.iter().map(|source| source.publisher),
        )?;
        ensure!(
            definition.bootstrap_catalog.as_ref() != Some(&pin.sha256)
                || !profile.composition.is_empty(),
            "bootstrap catalog has no recorded signed composition for profile {name}"
        );
        profiles.push((pin, profile));
    }
    let mut staged = Vec::with_capacity(companions.len());
    for (index, (source, repo)) in companions.iter().zip(companion_repos).enumerate() {
        let required = profiles
            .iter()
            .map(|(_, profile)| &profile.companions[index])
            .collect::<Vec<_>>();
        let release = stage_release(
            repo,
            source,
            full,
            SyncSelection::Current {
                binding: None,
                required: &required,
                guard: guard.as_ref(),
                defer_checkpoint: guard.is_some(),
            },
        )
        .with_context(|| {
            format!(
                "staging companion publisher {} for profile {name}",
                source.publisher
            )
        })?;
        if let Some(guard) = &guard {
            // The first profile is the staged primary's current profile. A
            // separately retained bootstrap profile may deliberately be older.
            let pin = required[0];
            if pin.sha256 != release.current {
                let catalog = pin.read(repo, source.publisher, &source.key)?;
                guard.check(repo, source, &pin.sha256, &catalog)?;
            }
        }
        staged.push((source, repo, release));
    }
    for (pin, profile) in profiles
        .iter()
        .filter(|(_, profile)| !profile.composition.is_empty())
    {
        check_composition(
            profile,
            pin,
            &definition.publishers,
            &sources,
            &repositories,
        )
        .with_context(|| {
            format!(
                "checking staged composition for release profile {name} at {}",
                pin.sha256
            )
        })?;
    }
    let previous_hash = read_current(primary_repo).ok();
    // The old catalog may be damaged while the replay checkpoint already
    // names a newer one. Reuse its recorded pins when repairing that cache.
    let previous_profile = previous_hash
        .as_ref()
        .and_then(|hash| read_catalog(primary_repo, hash, primary.publisher, &primary.key).ok())
        .and_then(|mut catalog| catalog.release_profiles.remove(name));
    for (index, (source, repo, _)) in staged.iter().enumerate() {
        let previous_pin = previous_profile.as_ref().and_then(|profile| {
            profile
                .companions
                .iter()
                .find(|pin| pin.publisher == source.publisher)
        });
        record_profile_pins(
            repo,
            &binding,
            primary_repo,
            previous_hash
                .as_deref()
                .map(|hash| (hash, previous_pin.map(|pin| pin.sha256.as_str()))),
            &primary_release.current,
            profiles
                .iter()
                .map(|(_, profile)| profile.companions[index].sha256.as_str()),
        )?;
    }
    // Profile readers use the primary pointer as the visibility boundary.
    // Earlier catalogs and objects remain addressable by their original digests.
    if guard.is_some() {
        // A primary that pins an invalid companion must not become a replay
        // checkpoint merely because its own signed catalog was fetched first.
        for (_, repo, release) in &staged {
            record_catalog_checkpoint(repo, &release.current)?;
        }
        record_catalog_checkpoint(primary_repo, &primary_release.current)?;
    }
    for (source, repo, release) in staged {
        source.promote(repo, &release, None)?;
    }
    primary.promote(primary_repo, &primary_release, Some(&binding))?;
    Ok(primary_release.current)
}

fn check_composition(
    profile: &ReleaseProfile,
    primary: &CatalogPin,
    config: &[PublisherConfig],
    sources: &[SyncSource<'_>],
    repositories: &[PathBuf],
) -> Result<()> {
    let mut candidates = CandidateSet::default();
    let mut origins = BTreeMap::new();
    let pins = iter::once(primary).chain(&profile.companions);
    for (((source, repo), config), pin) in sources.iter().zip(repositories).zip(config).zip(pins) {
        let mut catalog = pin.read(repo, source.publisher, &source.key)?;
        let local = config
            .build_trust
            .as_ref()
            .map(|trust| trust.load(&source.key))
            .transpose()
            .with_context(|| format!("loading build trust for {}", source.publisher))?;
        let signed = pin.build_trust.as_ref();
        let evidence = (signed.is_some() || local.is_some())
            .then(|| catalog.take_build_evidence(&pin.sha256, &source.key))
            .transpose()?;
        candidates.add_obsoletions(&mut origins, source.publisher, &catalog.obsoletions);
        // A publisher claims a name even when its versions miss the locked digest.
        for (id, entry) in catalog.entries {
            let spec = PackageSpec::parse(&id)?;
            let Some(expected) = profile.composition.get(spec.name) else {
                continue;
            };
            if entry.supports_architecture(&profile.architecture)
                && claim_source(&mut origins, source.publisher, spec.name)
                && entry.manifest_digest() == expected
            {
                let package = read_package(repo, source.publisher, &id, &entry)?;
                if let Some(evidence) = &evidence {
                    evidence.verify_policies(repo, &package, signed, local.as_ref())?;
                }
                candidates.packages.push(package);
            }
        }
    }
    let installed = candidates.replay(profile)?;
    profile_paths(&profile.architecture, &profile.facets, &installed)?;
    Ok(())
}
