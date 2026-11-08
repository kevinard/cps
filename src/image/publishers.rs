use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::ErrorKind,
    iter,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{RenameFlags, renameat_with, syncfs};

use super::{Image, Publisher, Publishers, Settings, State, builds, catalogs};
use crate::{
    attestation::BuildTrust,
    boot::{is_registered_btrfs, with_running_environment},
    git_install::GitSource,
    kdl_format::{read_build_trust, read_image_definition},
    model::validate_publisher,
    repo::{
        BuildAudit, BuildReceipt, CatalogCheckpoint, CatalogPin, Published, ReleaseProfile,
        audit_installed_builds, canonical_repo, catalog_index, read_selected_catalog,
        release::sync_url_publisher, release_profile,
    },
};

pub(crate) fn register_git_publisher(
    image: &mut Image,
    source: GitSource,
    repo: &Path,
    key: &Path,
    registered: bool,
) -> Result<()> {
    image.require_clean()?;
    let configured = image.publishers()?;
    let name = source.publisher();
    let mut publisher = Publisher::from_config(&name, repo, key)?;
    publisher.git = Some(source);
    if let Some(existing) = configured.get(&name) {
        ensure!(
            existing == &publisher,
            "Git publisher {name} is configured differently"
        );
        return Ok(());
    }
    check_new_publisher(image, &configured, &name)?;
    catalog_index(&publisher.repo, &name, &publisher.key, None)?;
    let mut publishers = configured.as_slice().to_vec();
    publishers.push(publisher);
    save(
        image,
        configured.with_publishers(publishers)?,
        &[],
        registered,
    )
}

pub struct PublisherInfo {
    pub name: String,
    pub repository: PathBuf,
}

pub(crate) struct SelectedProfile {
    primary: CatalogPin,
    pub record: ReleaseProfile,
}

impl SelectedProfile {
    pub(crate) fn pin(&self, publisher: &str) -> Option<&CatalogPin> {
        iter::once(&self.primary)
            .chain(&self.record.companions)
            .find(|pin| pin.publisher == publisher)
    }
}

impl Image {
    pub(crate) fn select_profile(
        &self,
        publishers: &Publishers,
        settings: &Settings,
        primary_pin: Option<&str>,
    ) -> Result<Option<SelectedProfile>> {
        let primary = &publishers.as_slice()[0];
        let checkpoint = settings.catalog_checkpoints.get(&primary.name);
        let selected = publishers.select_profile(
            &settings.arch,
            &settings.facets,
            settings.bootstrap_catalog.as_deref().or(primary_pin),
            checkpoint,
        )?;
        if let Some(selected) = &selected {
            self.check_catalog_transition(settings, primary, &selected.primary.sha256)?;
        }
        Ok(selected)
    }

    pub(crate) fn release_pins(
        &self,
        publishers: &Publishers,
        settings: &Settings,
    ) -> Result<BTreeMap<String, CatalogPin>> {
        let Some(selected) = self.select_profile(publishers, settings, None)? else {
            return Ok(BTreeMap::new());
        };
        Ok(iter::once(selected.primary)
            .chain(selected.record.companions)
            .map(|pin| (pin.publisher.clone(), pin))
            .collect())
    }
}

impl Publishers {
    pub(crate) fn select_profile(
        &self,
        arch: &str,
        facets: &BTreeMap<String, bool>,
        primary_pin: Option<&str>,
        previous: Option<&CatalogCheckpoint>,
    ) -> Result<Option<SelectedProfile>> {
        let Some(name) = self.release_profile() else {
            return Ok(None);
        };
        let (primary, companions) = self
            .signed_publishers()
            .split_first()
            .context("missing primary publisher")?;
        let (checkpoint, profile) = release_profile(
            &primary.repo,
            &primary.name,
            &primary.key,
            primary_pin,
            name,
        )?;
        if let Some(previous) = previous {
            previous.check(&checkpoint).with_context(|| {
                format!("checking installed catalog checkpoint for {}", primary.name)
            })?;
        }
        profile.check_image(
            name,
            arch,
            facets,
            companions.iter().map(|publisher| publisher.name.as_str()),
        )?;
        Ok(Some(SelectedProfile {
            primary: CatalogPin::primary(primary.name.clone(), checkpoint.catalog),
            record: profile,
        }))
    }
}

pub fn list_publishers(root: &Path) -> Result<Vec<PublisherInfo>> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    Ok(image
        .publishers()?
        .as_slice()
        .iter()
        .map(|publisher| PublisherInfo {
            name: publisher.name.clone(),
            repository: publisher.repo.clone(),
        })
        .collect())
}

pub fn add_publisher(root: &Path, name: &str, repo: &Path, key: &Path) -> Result<()> {
    add_publisher_with_build_trust(root, name, repo, key, None)
}

/// Verify a URL mirror before admitting its publisher to the image.
pub fn add_publisher_from_url(
    root: &Path,
    name: &str,
    url: &str,
    trusted_root: &Path,
    key: &Path,
    cache: &Path,
    policy: Option<&Path>,
) -> Result<PathBuf> {
    validate_publisher(name)?;
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let configured = image.publishers()?;
    check_new_publisher(&image, &configured, name)?;

    let leaf = cache
        .file_name()
        .context("expected a mirror cache directory")?;
    let parent = cache
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent)?;
    let cache = parent.join(leaf);
    for publisher in configured.as_slice() {
        ensure!(
            !cache.starts_with(&publisher.repo) && !publisher.repo.starts_with(&cache),
            "mirror cache overlaps the configured repository for {}",
            publisher.name
        );
    }
    drop(image);

    match cache.symlink_metadata() {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && cache.join("current").is_file(),
                "use a new directory or an already synchronized mirror cache"
            );
            sync_url_publisher(&cache, url, trusted_root, name, key)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let scratch = tempfile::Builder::new()
                .prefix(".cps-mirror-")
                .tempdir_in(&parent)?;
            sync_url_publisher(scratch.path(), url, trusted_root, name, key)?;
            let directory = File::open(&parent)?;
            renameat_with(
                &directory,
                scratch
                    .path()
                    .file_name()
                    .context("missing temporary cache name")?,
                &directory,
                leaf,
                RenameFlags::NOREPLACE,
            )
            .context("publishing the verified mirror cache")?;
            syncfs(&directory)?;
        }
        Err(error) => return Err(error.into()),
    }
    add_publisher_with_build_trust(root, name, &cache, key, policy)?;
    Ok(cache)
}

pub fn add_publisher_with_build_trust(
    root: &Path,
    name: &str,
    repo: &Path,
    key: &Path,
    policy: Option<&Path>,
) -> Result<()> {
    edit_publisher_order(root, |image, configured, registered| {
        check_new_publisher(image, &configured, name)?;
        let mut publisher = Publisher::from_config(name, repo, key)?;
        if let Some(path) = policy {
            let trust = read_build_trust(path)?.load(&publisher.key)?;
            audit_installed_builds(
                &publisher.repo,
                name,
                &publisher.key,
                None,
                |_| Ok(()),
                &trust,
                [],
            )?;
            publisher.build_trust = Some(trust);
        } else {
            catalog_index(&publisher.repo, name, &publisher.key, None)
                .with_context(|| format!("verifying signed catalog for {name}"))?;
        }
        let mut publishers = configured.as_slice().to_vec();
        publishers.push(publisher);
        save(
            image,
            configured.with_publishers(publishers)?,
            &[],
            registered,
        )
    })
}

fn check_new_publisher(image: &Image, configured: &Publishers, name: &str) -> Result<()> {
    ensure!(
        configured.get(name).is_none(),
        "publisher {name} is already configured"
    );
    if configured.release_profile().is_some() {
        let state = image.state()?;
        ensure!(
            !state.settings.profile_packages.is_empty(),
            "install the signed profile composition before adding a local publisher"
        );
        image.verify_catalog_archives(&state)?;
    }
    Ok(())
}

/// Move one or more configured publishers to authenticated replacement caches.
pub fn set_publisher_repositories(root: &Path, replacements: &[(String, PathBuf)]) -> Result<()> {
    ensure!(
        !replacements.is_empty(),
        "specify at least one repository to change"
    );
    let mut image = Image::open(root, true)?;
    image.require_clean()?;
    let state = image.state()?;
    let mut publishers = state.publishers.as_slice().to_vec();
    let mut specified = BTreeSet::new();
    let mut changed = BTreeSet::new();
    for (name, repo) in replacements {
        ensure!(
            specified.insert(name),
            "duplicate repository for publisher {name}"
        );
        let index = position(&publishers, name)?;
        let publisher = &mut publishers[index];
        let repo = canonical_repo(repo)?;
        if publisher.repo != repo {
            publisher.repo = repo;
            changed.insert(name);
        }
    }
    if changed.is_empty() {
        return Ok(());
    }
    let publishers = state.publishers.with_publishers(publishers)?;
    let profile = publishers.select_profile(
        &state.settings.arch,
        &state.settings.facets,
        state.settings.bootstrap_catalog.as_deref(),
        None,
    )?;
    for publisher in publishers.as_slice() {
        if profile.is_none() && !changed.contains(&publisher.name) {
            continue;
        }
        let pin = profile
            .as_ref()
            .and_then(|profile| profile.pin(&publisher.name));
        let check = || {
            let (catalog, hash) =
                read_selected_catalog(&publisher.repo, &publisher.name, &publisher.key, pin)?;
            image.check_repository_candidate(
                &state,
                publisher,
                &catalog,
                &catalog.checkpoint(hash),
            )?;
            let policies = publisher
                .build_trust
                .iter()
                .chain(pin.and_then(|pin| pin.build_trust.as_ref()));
            for trust in policies {
                for key in catalog.publisher_keys(&publisher.key)? {
                    trust.check_publisher_key(&key)?;
                }
            }
            for (name, package) in &state.installed {
                if package.published.manifest.publisher == publisher.name {
                    package.verify(name, &publishers)?;
                    catalog
                        .check_installed(&package.published)
                        .with_context(|| format!("checking installed package {name}"))?;
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        check().with_context(|| format!("checking replacement catalog for {}", publisher.name))?;
    }
    save(&mut image, publishers, &[], false)
}

/// Replace the locally provisioned builder policy. Audit installed packages
/// first unless the administrator explicitly scopes the change to future plans.
pub fn set_publisher_build_trust(
    root: &Path,
    name: &str,
    policy: &Path,
    future_only: bool,
) -> Result<()> {
    let mut image = Image::open(root, true)?;
    image.require_clean()?;
    let state = image.state()?;
    let mut publishers = state.publishers.as_slice().to_vec();
    let publisher = &mut publishers[position(state.publishers.as_slice(), name)?];
    let trust = read_build_trust(policy)?.load(&publisher.key)?;
    let receipts = if future_only {
        Vec::new()
    } else {
        let profile = catalogs::read_profile(&image.db, &state)?;
        let audit = audit(&image, &state, publisher, profile.as_ref(), &trust, true)?;
        ensure!(
            audit.failures.is_empty(),
            "installed build audit failed for {name}\n{}",
            audit.failures.join("\n")
        );
        audit.receipts
    };
    if publisher.build_trust.as_ref() != Some(&trust) {
        publisher.build_trust = Some(trust);
        save(
            &mut image,
            state.publishers.with_publishers(publishers)?,
            &receipts,
            false,
        )?;
    } else if !receipts.is_empty() {
        let tx = image.db.transaction()?;
        builds::save_audit(&tx, &state.publishers, &state.settings, &receipts)?;
        tx.commit()?;
    }
    Ok(())
}

pub fn audit_publisher_builds(root: &Path, name: &str) -> Result<BuildAudit> {
    let image = Image::open_query(root)?;
    image.require_clean()?;
    let state = image.state()?;
    let publisher = state
        .publishers
        .get(name)
        .with_context(|| format!("publisher {name} is not configured"))?;
    let profile = catalogs::read_profile(&image.db, &state)?;
    if let Some(signed) = profile
        .as_ref()
        .and_then(|profile| profile.companions.iter().find(|pin| pin.publisher == name))
        .and_then(|pin| pin.build_trust.as_ref())
    {
        let mut policies = vec![signed];
        if let Some(local) = publisher
            .build_trust
            .as_ref()
            .filter(|local| *local != signed)
        {
            policies.push(local);
        }
        let (mut audit, missing) =
            audit_archived(&image, &state, publisher, profile.as_ref(), &policies)?;
        audit.failures.extend(missing.into_iter().map(|package| {
            format!(
                "missing archived build evidence for {}",
                package.manifest.id()
            )
        }));
        return Ok(audit);
    }
    let trust = publisher.build_trust.as_ref().with_context(|| {
        format!("publisher {name} has no local or archived signed build trust policy")
    })?;
    audit(&image, &state, publisher, profile.as_ref(), trust, false)
}

fn audit(
    image: &Image,
    state: &State,
    publisher: &Publisher,
    profile: Option<&ReleaseProfile>,
    trust: &BuildTrust,
    check_current: bool,
) -> Result<BuildAudit> {
    let (mut audit, missing) = audit_archived(image, state, publisher, profile, &[trust])?;
    if check_current || !missing.is_empty() {
        let repository_audit = || {
            let pins = image.release_pins(&state.publishers, &state.settings)?;
            audit_installed_builds(
                &publisher.repo,
                &publisher.name,
                &publisher.key,
                pins.get(&publisher.name),
                |next| image.check_catalog(&state.publishers, &state.settings, publisher, next),
                trust,
                missing.iter().copied(),
            )
        };
        match repository_audit() {
            Ok(found) => {
                audit.verified += found.verified;
                audit.failures.extend(found.failures);
                audit.receipts = found.receipts;
            }
            Err(error) if check_current => return Err(error),
            Err(error) => audit.failures.extend(missing.iter().map(|package| {
                format!(
                    "checking repository build evidence for {}: {error:#}",
                    package.manifest.id()
                )
            })),
        }
    }
    Ok(audit)
}

fn audit_archived<'a>(
    image: &Image,
    state: &'a State,
    publisher: &Publisher,
    profile: Option<&ReleaseProfile>,
    policies: &[&BuildTrust],
) -> Result<(BuildAudit, Vec<&'a Published>)> {
    let mut audit = BuildAudit::default();
    let mut installed = state
        .installed
        .values()
        .map(|package| &package.published)
        .filter(|package| package.manifest.publisher == publisher.name)
        .peekable();
    let mut missing = Vec::new();
    if installed.peek().is_none() {
        return Ok((audit, missing));
    }
    let (applied, applied_hash) = catalogs::read_applied(&image.db, state, publisher, profile)?
        .with_context(|| {
            format!(
                "missing catalog checkpoint for installed publisher {}",
                publisher.name
            )
        })?;
    let archive = builds::exists(&image.db)?;
    let mut supplemental = None;
    for package in installed {
        if !archive {
            missing.push(package);
            continue;
        }
        let result = match builds::read(&image.db, &package.sha256) {
            Ok(None) => {
                missing.push(package);
                continue;
            }
            Ok(Some((receipt, bytes))) => (|| {
                let (catalog, expected) = if let Some(hash) = applied.check_installed(package)? {
                    (&applied, hash)
                } else {
                    if supplemental.is_none() {
                        let found = builds::admission_catalog(&image.db, publisher)?;
                        if let Some((catalog, hash)) = &found
                            && hash != applied_hash
                        {
                            applied.check_successor(catalog, applied_hash).context(
                                "checking archived build admission against the applied catalog",
                            )?;
                        }
                        supplemental = Some(found);
                    }
                    let Some((catalog, _)) = supplemental.as_ref().and_then(Option::as_ref) else {
                        return Ok(false);
                    };
                    let hash = catalog
                        .check_installed(package)?
                        .context("no signed catalog admission for archived build attestation")?;
                    (catalog, hash)
                };
                receipt.check_attestation(expected)?;
                let publisher_keys = catalog.publisher_keys(&publisher.key)?;
                policies.iter().try_for_each(|trust| {
                    for key in &publisher_keys {
                        trust.check_publisher_key(key)?;
                    }
                    receipt.verify(&bytes, package, &publisher.key, trust)
                })?;
                Ok(true)
            })(),
            Err(error) => Err(error),
        };
        match result {
            Ok(true) => audit.verified += 1,
            Ok(false) => missing.push(package),
            Err(error) => audit.failures.push(format!(
                "checking archived build evidence for {}: {error:#}",
                package.manifest.id()
            )),
        }
    }
    Ok((audit, missing))
}

pub fn prefer_publisher(root: &Path, name: &str) -> Result<()> {
    edit_publisher_order(root, |image, configured, registered| {
        let mut publishers = configured.as_slice().to_vec();
        let position = position(&publishers, name)?;
        let first = configured.signed_publishers().len();
        ensure!(
            position >= first,
            "the signed release profile fixes this image's publishers and their order"
        );
        if position == first {
            return Ok(());
        }
        publishers[first..=position].rotate_right(1);
        save(
            image,
            configured.with_publishers(publishers)?,
            &[],
            registered,
        )
    })
}

pub fn remove_publisher(root: &Path, name: &str) -> Result<()> {
    edit_publisher_order(root, |image, configured, registered| {
        let mut publishers = configured.as_slice().to_vec();
        let position = position(&publishers, name)?;
        ensure!(
            position >= configured.signed_publishers().len(),
            "the signed release profile fixes this image's publishers and their order"
        );
        ensure!(
            publishers.len() > 1,
            "cannot remove the image's last publisher"
        );
        for (package_name, package) in image.installed()? {
            package.verify(&package_name, &configured)?;
            ensure!(
                package.published.manifest.publisher != name,
                "cannot remove publisher {name} while {package_name} is installed"
            );
        }
        publishers.remove(position);
        save(
            image,
            configured.with_publishers(publishers)?,
            &[],
            registered,
        )
    })
}

fn position(publishers: &[Publisher], name: &str) -> Result<usize> {
    validate_publisher(name)?;
    publishers
        .iter()
        .position(|publisher| publisher.name == name)
        .with_context(|| format!("publisher {name} is not configured"))
}

fn edit_publisher_order(
    path: &Path,
    edit: impl FnOnce(&mut Image, Publishers, bool) -> Result<()>,
) -> Result<()> {
    let mut image = Image::open(path, true)?;
    image.require_clean()?;
    let publishers = image.publishers()?;
    if publishers.release_profile().is_none() || !is_registered_btrfs(&image.root)? {
        return edit(&mut image, publishers, false);
    }

    let target = image.root.as_file().metadata()?;
    // Acquire the ESP before reopening the image, as activation does.
    drop(image);
    let boot = read_image_definition(Path::new(crate::system::CONFIG))?
        .boot_btrfs
        .context("configure boot-btrfs to edit publishers on the installed image")?;
    with_running_environment(
        &boot.volume,
        &boot.esp,
        boot.activation.root_partuuid.as_deref(),
        |root, id, revision| {
            let running = root.as_file().metadata()?;
            ensure!(
                (target.dev(), target.ino()) == (running.dev(), running.ino()),
                "boot the selected image before changing its local publishers"
            );
            let mut image = Image::open_for_live_apply(root, id, revision)?;
            image.require_clean()?;
            let publishers = image.publishers()?;
            edit(&mut image, publishers, true)
        },
    )
}

fn save(
    image: &mut Image,
    publishers: Publishers,
    receipts: &[BuildReceipt],
    registered: bool,
) -> Result<()> {
    let mut settings = image.settings()?;
    if registered {
        // A snapshot taken before this edit must fail the boot parent revision check.
        settings.boot_revision = Some(settings.boot_revision());
        settings.revision = settings.next_revision()?;
    }
    settings
        .catalog_checkpoints
        .retain(|name, _| publishers.get(name).is_some());
    settings
        .dormant_configs
        .retain(|_, config| publishers.get(&config.publisher).is_some());
    settings.publisher_generation = settings
        .publisher_generation
        .checked_add(1)
        .context("publisher generation exhausted")?;
    let tx = image.db.transaction()?;
    builds::save_audit(&tx, &publishers, &settings, receipts)?;
    builds::retain_admission(&tx, &publishers)?;
    catalogs::retain(&tx, &settings)?;
    tx.execute(
        "UPDATE publishers SET data=?1 WHERE id=1",
        [serde_json::to_string(&publishers)?],
    )?;
    tx.execute(
        "UPDATE image SET data=?1 WHERE id=1",
        [serde_json::to_string(&settings)?],
    )?;
    tx.commit()?;
    Ok(())
}
