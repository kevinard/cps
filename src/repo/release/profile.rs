use std::{fs, path::Path};

use anyhow::{Context, Result, ensure};

use crate::{kdl_format::read_image_definition, repo::release_profile};

use super::{Mirror, SyncSource, digest, lock_repo, stage_release};

/// Materialize a signed profile's catalog set before advancing its primary pointer.
pub fn sync_profile(config: &Path, index_only: bool) -> Result<String> {
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
    let primary_hash = stage_release(primary_repo, primary, full, Some(&binding), None)
        .with_context(|| format!("staging primary publisher {}", primary.publisher))?;
    let (_, profile) = release_profile(
        primary_repo,
        primary.publisher,
        &primary.key,
        Some(&primary_hash),
        name,
    )?;
    profile.check_image(
        name,
        &definition.arch,
        &definition.facets,
        companions.iter().map(|source| source.publisher),
    )?;
    let mut staged = Vec::with_capacity(companions.len());
    for ((source, repo), pin) in companions
        .iter()
        .zip(companion_repos)
        .zip(&profile.companions)
    {
        let hash = stage_release(repo, source, full, None, Some(pin)).with_context(|| {
            format!(
                "staging companion publisher {} for profile {name}",
                source.publisher
            )
        })?;
        staged.push((source, repo, hash));
    }
    // Profile readers use the primary pointer as the visibility boundary.
    // Earlier catalogs and objects remain addressable by their original digests.
    for (source, repo, hash) in staged {
        source.promote(repo, &hash, None)?;
    }
    primary.promote(primary_repo, &primary_hash, Some(&binding))?;
    Ok(primary_hash)
}
