use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use cps::query::{PackageRow, Scope, list_facets, list_packages, package_manifest};
use cps::{
    Activation, BuildOptions, BuildSettings, BuildWitness, ConfigChoice, Plan, Request,
    RootDeployment, SourceLayout, activate_boot_environment, activate_boot_environment_on_disk,
    activate_deployed_boot_environment, activate_device_boot_environment,
    add_publisher_with_build_trust, apply, audit_publisher_builds, build_with_toolchain_config,
    build_with_worker, create_configured_image, create_image, deploy_and_activate_boot_environment,
    deploy_boot_environment, export_boot_environment, export_boot_environment_receipt, fetch_plan,
    fetch_sources, fetch_sources_with_local, generate_manifest_for_architecture, import_sources,
    list_boot_environments, list_boot_environments_on_disk, list_config_updates, list_obsoletions,
    list_publishers, list_withdrawals, plan, prefer_publisher, publish, publish_release,
    publish_witnessed_bundle, read_attestation, read_worker_config, release, remove_publisher,
    resume_transaction, rollback_boot_environment, rollback_boot_environment_on_disk,
    rotate_publisher_key, set_publisher_build_trust, set_publisher_repositories,
    stage_boot_environment, transaction_details, transaction_history, transaction_status, verify,
    verify_pristine, withdraw_package,
};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prepare pinned package sources for offline builds
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Build a package with verified inputs in an offline sandbox
    Build {
        #[arg(long, required_unless_present = "toolchain_config")]
        repo: Option<PathBuf>,
        #[arg(long, required_unless_present = "toolchain_config")]
        publisher: Option<String>,
        #[arg(long, required_unless_present = "toolchain_config")]
        trust_key: Option<PathBuf>,
        /// Worker-local repository and trust key locations for the pinned publishers
        #[arg(long, conflicts_with_all = ["repo", "publisher", "trust_key"])]
        toolchain_config: Option<PathBuf>,
        #[arg(long, required_unless_present = "source_cache")]
        sources: Option<PathBuf>,
        #[arg(long, conflicts_with = "sources")]
        source_cache: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        check_reproducible: bool,
        #[arg(long)]
        worker_config: Option<PathBuf>,
        #[arg(long, requires = "check_reproducible")]
        builder_signing_key: Option<PathBuf>,
        recipe: PathBuf,
    },
    /// Prepare a package manifest for review
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
    /// Publish one manifest and its payloads to a local repository
    Publish {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        proto: PathBuf,
        #[arg(long)]
        signing_key: PathBuf,
        manifest: PathBuf,
    },
    /// Publish a reproducible build bundle after verifying its worker signature and reviewed recipe
    PublishBundle {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        signing_key: PathBuf,
        #[arg(long)]
        builder_key: PathBuf,
        #[arg(long)]
        recipe_sha256: String,
        #[arg(long, value_name = "KEY=ATTESTATION", value_parser = parse_witness)]
        witness: Vec<BuildWitness>,
        bundle: PathBuf,
    },
    /// Export or synchronize a signed repository release
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Initialize an empty directory as an image
    ImageCreate {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, conflicts_with_all = ["repo", "publisher", "trust_key", "arch", "facet"])]
        config: Option<PathBuf>,
        #[arg(long, required_unless_present = "config")]
        repo: Option<PathBuf>,
        #[arg(long, required_unless_present = "config")]
        publisher: Option<String>,
        #[arg(long, required_unless_present = "config")]
        trust_key: Option<PathBuf>,
        #[arg(long)]
        arch: Option<String>,
        #[arg(long, value_name = "NAME=BOOL")]
        facet: Vec<String>,
    },
    /// Manage an image's ordered set of trusted publishers
    Publisher {
        #[command(subcommand)]
        command: PublisherCommand,
    },
    /// Review pending changes to preserved configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// List the image's configured facet overrides
    Facet {
        #[arg(long)]
        root: PathBuf,
    },
    /// List installed packages or versions in the configured signed catalogs
    List {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        available: bool,
        #[arg(long, requires = "available")]
        all_versions: bool,
    },
    /// Search package names in the signed catalog or installed image
    Search {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        installed: bool,
        term: String,
    },
    /// Show an installed or available package's manifest in KDL
    Info {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        available: bool,
        #[arg(value_name = "NAME[@VERSION]")]
        package: String,
    },
    /// Preview and save a package transaction
    Plan {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[command(subcommand)]
        request: PlanCommand,
    },
    /// Fetch the verified payloads required by a saved plan
    Fetch {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        plan: PathBuf,
    },
    /// Apply or resume a saved plan
    Apply {
        #[arg(long)]
        root: PathBuf,
        plan: PathBuf,
    },
    /// Inspect or resume an interrupted image transaction
    Transaction {
        #[command(subcommand)]
        command: TransactionCommand,
    },
    /// Check an image against its installed manifests, offline
    Verify {
        #[arg(long)]
        root: PathBuf,
        /// Require an inactive root with pristine package paths and no unmanaged entries
        #[arg(long)]
        pristine: bool,
    },
    /// Manage systemd-boot environments
    BootEnv {
        #[command(subcommand)]
        command: BootCommand,
    },
}

#[derive(Subcommand)]
enum SourceCommand {
    /// Download missing sources over HTTPS and verify their pins
    Fetch {
        #[arg(long)]
        cache: PathBuf,
        /// Prefer files from this directory before using recipe URLs
        #[arg(long)]
        from: Option<PathBuf>,
        recipe: PathBuf,
    },
    /// Verify and cache sources from a local directory
    Import {
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        from: PathBuf,
        recipe: PathBuf,
    },
}

#[derive(Subcommand)]
enum ManifestCommand {
    /// Generate KDL from a staged prototype tree
    Generate {
        #[arg(long)]
        proto: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        architecture: Option<String>,
        #[arg(long, default_value_t = 0)]
        uid: u32,
        #[arg(long, default_value_t = 0)]
        gid: u32,
        name: String,
    },
}

#[derive(Subcommand)]
enum PublisherCommand {
    /// Show configured publishers in search order
    List {
        #[arg(long)]
        root: PathBuf,
    },
    /// Verify and append a publisher to the image's search order
    Add {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        trust_key: PathBuf,
        /// Require builder signatures from this KDL policy
        #[arg(long)]
        build_trust: Option<PathBuf>,
        name: String,
    },
    /// Replace one or more local repository paths as a single change
    SetRepository {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, required = true, value_name = "PUBLISHER=PATH", value_parser = parse_repository)]
        repo: Vec<(String, PathBuf)>,
    },
    /// Replace a publisher's locally configured builder keys and threshold
    SetBuildTrust {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        policy: PathBuf,
        /// Change admission policy without auditing existing packages
        #[arg(long)]
        future_only: bool,
        name: String,
    },
    /// Check installed packages against the configured builder policy
    AuditBuilds {
        #[arg(long)]
        root: PathBuf,
        name: String,
    },
    /// Search this publisher first when selecting new packages
    Prefer {
        #[arg(long)]
        root: PathBuf,
        name: String,
    },
    /// Remove a publisher that has no installed packages
    Remove {
        #[arg(long)]
        root: PathBuf,
        name: String,
    },
}

fn parse_witness(value: &str) -> std::result::Result<BuildWitness, String> {
    let (key, attestation) = value.split_once('=').ok_or("expected KEY=ATTESTATION")?;
    if key.is_empty() || attestation.is_empty() {
        return Err("witness key and attestation paths are required".to_owned());
    }
    Ok(BuildWitness {
        key: key.into(),
        attestation: attestation.into(),
    })
}

fn parse_repository(value: &str) -> std::result::Result<(String, PathBuf), String> {
    let (publisher, path) = value.split_once('=').ok_or("expected PUBLISHER=PATH")?;
    if publisher.is_empty() || path.is_empty() {
        return Err("publisher and repository path are required".to_owned());
    }
    Ok((publisher.to_owned(), path.into()))
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Authorize a new publisher signing key in a successor catalog
    RotateKey {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        signing_key: PathBuf,
        #[arg(long)]
        new_signing_key: PathBuf,
    },
    /// List signed package obsoletion cutoffs
    Obsoletions {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
    },
    /// Commit a set of reviewed build bundles and withdrawals as one signed catalog
    PublishRelease {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        signing_key: PathBuf,
        release: PathBuf,
    },
    /// Print the signed build evidence for a full package ID
    Attestation {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        package_id: String,
    },
    /// Withdraw an exact published version from future package selections
    Withdraw {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        signing_key: PathBuf,
        package_id: String,
    },
    /// List withdrawn package IDs and their original manifest digests
    Withdrawals {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
    },
    /// Export signed catalogs and their objects for TUF signing
    Export {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// Include a historical catalog and its objects in this mirror release
        #[arg(long = "retain-catalog")]
        retained: Vec<String>,
    },
    /// Verify and cache a release from a TUF mirror
    Sync {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        metadata: String,
        #[arg(long)]
        targets: String,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        /// Check the signed catalog against this installed image before promotion
        #[arg(long)]
        image: Option<PathBuf>,
        /// Cache the catalog, manifests, and build evidence without downloading payloads
        #[arg(long)]
        index_only: bool,
    },
    /// Verify a signed release profile and its pinned companion catalogs from TUF mirrors
    SyncProfile {
        /// Image configuration with mirror settings for every publisher
        #[arg(long)]
        config: PathBuf,
        /// Check the signed catalogs against this installed image before promotion
        #[arg(long)]
        image: Option<PathBuf>,
        /// Cache the catalogs, manifests, and build evidence without downloading payloads
        #[arg(long)]
        index_only: bool,
    },
    /// Preview or remove unused payload files from a TUF mirror cache
    PruneCache {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        /// Also retain payloads from this historical catalog
        #[arg(long = "keep-catalog")]
        catalogs: Vec<String>,
        /// Keep payloads for this image's installed packages and current profile
        #[arg(long = "keep-image")]
        images: Vec<PathBuf>,
        /// Keep payloads used by this saved plan
        #[arg(long = "keep-plan")]
        plans: Vec<PathBuf>,
        /// Remove the listed files
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// List pending vendor configuration copies
    List {
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Subcommand)]
enum TransactionCommand {
    /// Show whether the image has an interrupted transaction
    Status {
        #[arg(long)]
        root: PathBuf,
    },
    /// Finish a journaled transaction using the files already staged in the image
    Resume {
        #[arg(long)]
        root: PathBuf,
    },
    /// List completed transactions, most recent first
    History {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show the recorded details of a completed plan as JSON
    Show {
        #[arg(long)]
        root: PathBuf,
        #[arg(value_name = "PLAN_SHA256")]
        plan: String,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Install one or more packages, optionally specifying publisher and version
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Remove dependencies no longer needed by explicitly installed packages
    Autoremove,
    /// Change facet selection for installed package actions
    ChangeFacet {
        #[arg(required = true, value_name = "NAME=true|false|default")]
        facets: Vec<String>,
    },
    /// Reconcile an image to the requested packages and their dependencies
    ExactInstall {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Install the configured signed release profile into an image with no packages
    InstallProfile,
    /// Remove installed packages without contacting the repository
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Restore damaged paths from the installed package versions
    Repair { packages: Vec<String> },
    /// Review a pending configuration update
    ResolveConfig {
        /// Path relative to the image root
        path: String,
        #[arg(long, value_enum)]
        take: ConfigChoice,
    },
    /// Update the installed package set
    Update,
}

#[derive(Subcommand)]
enum BootCommand {
    /// List registered boot environments and the configured default
    List {
        #[arg(long, required_unless_present = "disk", conflicts_with = "disk")]
        esp: Option<PathBuf>,
        #[arg(long, requires = "esp_partuuid")]
        disk: Option<PathBuf>,
        #[arg(long, requires = "disk")]
        esp_partuuid: Option<String>,
    },
    /// Select a known-good environment for the next boot
    Rollback {
        #[arg(long, required_unless_present = "disk", conflicts_with = "disk")]
        esp: Option<PathBuf>,
        #[arg(long, requires_all = ["esp_partuuid", "output_disk"])]
        disk: Option<PathBuf>,
        #[arg(long, requires = "disk")]
        esp_partuuid: Option<String>,
        /// Publish the rollback in a new disk file
        #[arg(long, requires = "disk")]
        output_disk: Option<PathBuf>,
        /// Environment ID. Defaults to the previous known-good environment.
        id: Option<String>,
    },
    /// Clone the base image and apply a saved plan in a new directory
    Stage {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        root: PathBuf,
        plan: PathBuf,
    },
    /// Export an inactive root to a new ext4 partition image
    Export {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_name = "MIB")]
        size_mib: u64,
        /// Write the KDL export receipt to standard output
        #[arg(long)]
        receipt: bool,
    },
    /// Deploy an exported root to a partition in a new offline GPT disk image
    Deploy {
        #[arg(long)]
        disk: PathBuf,
        #[arg(long)]
        root_image: PathBuf,
        #[arg(long)]
        root_partuuid: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify or write an inactive GPT root partition and publish its boot entry
    ActivateDevice {
        #[arg(long)]
        root: PathBuf,
        /// Mount point of the writable EFI System Partition
        #[arg(long)]
        esp: PathBuf,
        /// Inactive Linux filesystem partition on the same disk
        #[arg(long)]
        device: PathBuf,
        #[arg(long)]
        root_partuuid: String,
        /// KDL receipt produced by boot-env export --receipt
        #[arg(long)]
        receipt: PathBuf,
        /// Write this export before activation. Without it, verify the existing bytes.
        #[arg(long)]
        root_image: Option<PathBuf>,
        /// Retire this environment before writing its inactive root partition
        #[arg(long, requires = "root_image")]
        replace: Option<String>,
        #[arg(long, default_value = "boot/vmlinuz")]
        kernel: String,
        #[arg(long)]
        initrd: Option<String>,
        #[arg(long, default_value = "")]
        options: String,
        #[arg(long, default_value_t = 2)]
        tries: u32,
        /// Bootstrap the first entry using an already known-good base image
        #[arg(long, conflicts_with = "replace")]
        known_good: bool,
    },
    /// Publish a systemd-boot entry for a verified environment
    Activate {
        #[arg(long)]
        root: PathBuf,
        #[arg(
            long,
            required_unless_present = "output_disk",
            conflicts_with = "output_disk"
        )]
        esp: Option<PathBuf>,
        #[arg(long)]
        root_partuuid: String,
        /// Offline GPT disk holding the deployed root
        #[arg(long, requires = "receipt")]
        disk: Option<PathBuf>,
        /// KDL receipt produced by boot-env export --receipt
        #[arg(long, requires = "disk")]
        receipt: Option<PathBuf>,
        /// GPT PARTUUID of the EFI System Partition on the disk
        #[arg(long, requires = "output_disk")]
        esp_partuuid: Option<String>,
        /// Publish the activation in a new disk file
        #[arg(long, requires_all = ["disk", "receipt", "esp_partuuid"])]
        output_disk: Option<PathBuf>,
        /// Deploy this root payload in the same transaction as activation
        #[arg(long, requires = "output_disk")]
        root_image: Option<PathBuf>,
        /// Recycle the root slot of this inactive boot environment
        #[arg(long, requires = "root_image")]
        replace: Option<String>,
        #[arg(long, default_value = "boot/vmlinuz")]
        kernel: String,
        #[arg(long)]
        initrd: Option<String>,
        #[arg(long, default_value = "")]
        options: String,
        #[arg(long, default_value_t = 2)]
        tries: u32,
        /// Bootstrap the first entry using an already known-good base image
        #[arg(long)]
        known_good: bool,
    },
}

fn load_plan(path: &Path) -> Result<Plan> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("reading plan {}", path.display()))
}

fn parse_facets(entries: Vec<String>) -> Result<BTreeMap<String, bool>> {
    let mut facets = BTreeMap::new();
    for entry in entries {
        let (name, value) = entry
            .split_once('=')
            .context("expected --facet NAME=BOOL")?;
        ensure!(
            facets.insert(name.to_owned(), value.parse()?).is_none(),
            "duplicate facet {name}"
        );
    }
    Ok(facets)
}

fn parse_facet_changes(entries: Vec<String>) -> Result<BTreeMap<String, Option<bool>>> {
    let mut facets = BTreeMap::new();
    for entry in entries {
        let (name, value) = entry
            .split_once('=')
            .context("expected NAME=true|false|default")?;
        let value = if value == "default" {
            None
        } else {
            Some(
                value
                    .parse()
                    .context("facet value must be true, false, or default")?,
            )
        };
        ensure!(
            facets.insert(name.to_owned(), value).is_none(),
            "duplicate facet {name}"
        );
    }
    Ok(facets)
}

fn save(path: &Path, plan: &Plan) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, plan)?;
    writeln!(file)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn print_packages(rows: impl IntoIterator<Item = PackageRow>) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "PACKAGE\tSTATE")?;
    for row in rows {
        writeln!(out, "{}\t{}", row.id, row.status)?;
    }
    Ok(())
}

fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Source { command } => {
            let count = match command {
                SourceCommand::Fetch {
                    cache,
                    from,
                    recipe,
                } => match from {
                    Some(from) => fetch_sources_with_local(&recipe, &cache, &from)?,
                    None => fetch_sources(&recipe, &cache)?,
                },
                SourceCommand::Import {
                    cache,
                    from,
                    recipe,
                } => import_sources(&recipe, &cache, &from)?,
            };
            println!("Verified {count} pinned source(s) in the cache");
        }
        Command::Build {
            repo,
            publisher,
            trust_key,
            toolchain_config,
            sources,
            source_cache,
            output,
            check_reproducible,
            worker_config,
            builder_signing_key,
            recipe,
        } => {
            let input = source_cache
                .as_deref()
                .or(sources.as_deref())
                .context("missing build sources")?;
            let layout = if source_cache.is_some() {
                SourceLayout::Cache
            } else {
                SourceLayout::Directory
            };
            let worker = worker_config
                .as_deref()
                .map(read_worker_config)
                .transpose()?
                .unwrap_or_default();
            let id = if let Some(config) = toolchain_config {
                build_with_toolchain_config(
                    &recipe,
                    &config,
                    BuildSettings {
                        sources: input,
                        output: &output,
                        check_reproducible,
                        builder_signing_key: builder_signing_key.as_deref(),
                    },
                    layout,
                    &worker,
                )?
            } else {
                build_with_worker(
                    &recipe,
                    BuildOptions {
                        repo: repo.as_deref().context("missing toolchain repository")?,
                        publisher: publisher
                            .as_deref()
                            .context("missing toolchain publisher")?,
                        trust_key: trust_key
                            .as_deref()
                            .context("missing toolchain trust key")?,
                        sources: input,
                        output: &output,
                        check_reproducible,
                        builder_signing_key: builder_signing_key.as_deref(),
                    },
                    layout,
                    &worker,
                )?
            };
            println!("Built {id} at {}", output.display());
        }
        Command::Manifest { command } => match command {
            ManifestCommand::Generate {
                proto,
                publisher,
                version,
                architecture,
                uid,
                gid,
                name,
            } => {
                let text = generate_manifest_for_architecture(
                    &proto,
                    &publisher,
                    &name,
                    &version,
                    uid,
                    gid,
                    architecture.as_deref(),
                )?;
                std::io::stdout().lock().write_all(text.as_bytes())?;
            }
        },
        Command::Publish {
            repo,
            proto,
            signing_key,
            manifest,
        } => {
            println!(
                "Published {}",
                publish(&repo, &proto, &manifest, &signing_key)?
            );
        }
        Command::PublishBundle {
            repo,
            signing_key,
            builder_key,
            recipe_sha256,
            witness,
            bundle,
        } => {
            println!(
                "Published {}",
                publish_witnessed_bundle(
                    &repo,
                    &bundle,
                    &signing_key,
                    &builder_key,
                    &recipe_sha256,
                    &witness
                )?
            );
        }
        Command::Repo { command } => match command {
            RepoCommand::RotateKey {
                repo,
                publisher,
                base,
                signing_key,
                new_signing_key,
            } => {
                println!(
                    "Published catalog {}",
                    rotate_publisher_key(&repo, &publisher, &base, &signing_key, &new_signing_key)?
                );
            }
            RepoCommand::Obsoletions {
                repo,
                publisher,
                trust_key,
            } => {
                for id in list_obsoletions(&repo, &publisher, &trust_key)? {
                    println!("{id}");
                }
            }
            RepoCommand::PublishRelease {
                repo,
                signing_key,
                release,
            } => {
                println!(
                    "Published catalog {}",
                    publish_release(&repo, &signing_key, &release)?
                );
            }
            RepoCommand::Attestation {
                repo,
                publisher,
                trust_key,
                package_id,
            } => {
                let bytes = read_attestation(&repo, &publisher, &trust_key, &package_id)?;
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(&bytes)?;
                writeln!(stdout)?;
            }
            RepoCommand::Withdraw {
                repo,
                signing_key,
                package_id,
            } => {
                withdraw_package(&repo, &signing_key, &package_id)?;
                println!("Withdrawn {package_id}");
            }
            RepoCommand::Withdrawals {
                repo,
                publisher,
                trust_key,
            } => {
                for (id, hash) in list_withdrawals(&repo, &publisher, &trust_key)? {
                    println!("{id}\t{hash}");
                }
            }
            RepoCommand::Export {
                repo,
                publisher,
                trust_key,
                output,
                retained,
            } => {
                println!(
                    "Exported {}",
                    release::export_with_retained(
                        &repo, &publisher, &trust_key, &output, &retained
                    )?
                );
            }
            RepoCommand::Sync {
                repo,
                root,
                metadata,
                targets,
                publisher,
                trust_key,
                index_only,
                image,
            } => {
                println!(
                    "Synchronized {}",
                    release::sync_with_options(
                        &repo,
                        &root,
                        &metadata,
                        &targets,
                        &publisher,
                        &trust_key,
                        release::SyncOptions {
                            index_only,
                            image: image.as_deref()
                        }
                    )?
                );
            }
            RepoCommand::SyncProfile {
                config,
                index_only,
                image,
            } => {
                let synchronized = match image {
                    Some(image) => release::sync_profile_for_image(&config, index_only, &image)?,
                    None => release::sync_profile(&config, index_only)?,
                };
                println!("Synchronized {}", synchronized);
            }
            RepoCommand::PruneCache {
                repo,
                publisher,
                trust_key,
                catalogs,
                images,
                plans,
                apply,
            } => {
                let plans = plans
                    .iter()
                    .map(|path| load_plan(path))
                    .collect::<Result<Vec<_>>>()?;
                let report = release::prune_payloads(
                    &repo,
                    &publisher,
                    &trust_key,
                    release::CacheRetention {
                        catalogs: &catalogs,
                        images: &images,
                        plans: &plans,
                    },
                    apply,
                )?;
                for (hash, length) in &report.blobs {
                    println!("{hash}\t{length}");
                }
                let action = if apply { "Removed" } else { "Would remove" };
                println!(
                    "{action} {} payload file(s), {} bytes",
                    report.blobs.len(),
                    report.bytes
                );
            }
        },
        Command::ImageCreate {
            root,
            config,
            repo,
            publisher,
            trust_key,
            arch,
            facet,
        } => {
            let id = match config {
                Some(path) => create_configured_image(&root, &path)?,
                None => create_image(
                    &root,
                    &repo.context("missing repository")?,
                    &publisher.context("missing publisher")?,
                    &trust_key.context("missing trust key")?,
                    arch.as_deref().unwrap_or(std::env::consts::ARCH),
                    parse_facets(facet)?,
                )?,
            };
            println!("Created image {id} at {}", root.display());
        }
        Command::Publisher { command } => match command {
            PublisherCommand::List { root } => {
                println!("PUBLISHER\tREPOSITORY");
                for publisher in list_publishers(&root)? {
                    println!("{}\t{}", publisher.name, publisher.repository.display());
                }
            }
            PublisherCommand::Add {
                root,
                repo,
                trust_key,
                build_trust,
                name,
            } => {
                add_publisher_with_build_trust(
                    &root,
                    &name,
                    &repo,
                    &trust_key,
                    build_trust.as_deref(),
                )?;
            }
            PublisherCommand::SetRepository { root, repo } => {
                set_publisher_repositories(&root, &repo)?;
            }
            PublisherCommand::SetBuildTrust {
                root,
                policy,
                future_only,
                name,
            } => {
                set_publisher_build_trust(&root, &name, &policy, future_only)?;
                if future_only {
                    println!("Configured build trust for future packages from {name}");
                } else {
                    println!("Configured build trust for {name}");
                }
            }
            PublisherCommand::AuditBuilds { root, name } => {
                let audit = audit_publisher_builds(&root, &name)?;
                for finding in &audit.failures {
                    eprintln!("{finding}");
                }
                println!(
                    "Verified build evidence for {} installed package(s) from {name}",
                    audit.verified
                );
                ensure!(
                    audit.failures.is_empty(),
                    "{} installed package(s) failed build audit",
                    audit.failures.len()
                );
            }
            PublisherCommand::Prefer { root, name } => prefer_publisher(&root, &name)?,
            PublisherCommand::Remove { root, name } => remove_publisher(&root, &name)?,
        },
        Command::List {
            root,
            available,
            all_versions,
        } => {
            let scope = match (available, all_versions) {
                (true, true) => Scope::AllAvailableVersions,
                (true, false) => Scope::Available,
                _ => Scope::Installed,
            };
            print_packages(list_packages(&root, scope)?)?;
        }
        Command::Search {
            root,
            installed,
            term,
        } => {
            let term = term.trim().to_ascii_lowercase();
            ensure!(!term.is_empty(), "search requires a package name fragment");
            let scope = if installed {
                Scope::Installed
            } else {
                Scope::Available
            };
            let packages = list_packages(&root, scope)?;
            print_packages(packages.into_iter().filter(|row| row.name.contains(&term)))?;
        }
        Command::Info {
            root,
            available,
            package,
        } => {
            let manifest = package_manifest(&root, &package, available)?;
            std::io::stdout().lock().write_all(manifest.as_bytes())?;
        }
        Command::Config {
            command: ConfigCommand::List { root },
        } => {
            let updates = list_config_updates(&root)?;
            if updates.is_empty() {
                println!("No pending configuration updates");
            } else {
                println!("STATUS\tREVISION\tPATH\tVENDOR COPY");
                for update in updates {
                    let status = if update.vendor_available {
                        "pending"
                    } else {
                        "vendor copy unavailable"
                    };
                    println!(
                        "{status}\t{}\t{:?}\t{:?}",
                        update.revision, update.path, update.vendor_copy
                    );
                }
            }
        }
        Command::Facet { root } => {
            println!("FACET\tVALUE");
            for (facet, value) in list_facets(&root)? {
                println!("{facet}\t{value}");
            }
        }
        Command::Plan {
            root,
            output,
            request,
        } => {
            let request = match request {
                PlanCommand::Install { packages } => Request::Install { packages },
                PlanCommand::ExactInstall { packages } => Request::ExactInstall { packages },
                PlanCommand::InstallProfile => Request::InstallProfile,
                PlanCommand::Remove { packages } => Request::Remove { packages },
                PlanCommand::Autoremove => Request::Autoremove,
                PlanCommand::ChangeFacet { facets } => Request::ChangeFacet {
                    facets: parse_facet_changes(facets)?,
                },
                PlanCommand::Repair { packages } => Request::Repair { packages },
                PlanCommand::ResolveConfig { path, take } => Request::ResolveConfig { path, take },
                PlanCommand::Update => Request::Update,
            };
            let plan = plan(&root, request)?;
            save(&output, &plan).with_context(|| format!("saving {}", output.display()))?;
            println!("{}", plan.summary());
        }
        Command::Apply { root, plan } => {
            let saved = load_plan(&plan)?;
            println!("Image revision {}", apply(&root, &saved)?);
        }
        Command::Transaction { command } => match command {
            TransactionCommand::Status { root } => {
                let status = transaction_status(&root)?;
                if let Some(pending) = status.pending {
                    if let Some(summary) = &pending.plan_summary {
                        println!("{summary}");
                    } else {
                        println!("Image revision {}", status.revision);
                    }
                    println!(
                        "Pending plan {} targets revision {}",
                        pending.plan_id, pending.target_revision
                    );
                    println!(
                        "{}",
                        if pending.files_ready {
                            "Offline actions pending"
                        } else {
                            "File writes pending"
                        }
                    );
                    if pending.plan_summary.is_some() {
                        println!("Recover with transaction resume");
                    } else {
                        println!("Recover with apply and the original saved plan");
                    }
                } else {
                    println!("Image revision {}", status.revision);
                    println!("No pending transaction");
                }
            }
            TransactionCommand::Resume { root } => {
                if let Some(revision) = resume_transaction(&root)? {
                    println!("Image revision {revision}");
                } else {
                    println!("No pending transaction");
                }
            }
            TransactionCommand::History { root, limit } => {
                let history = transaction_history(&root, limit)?;
                println!("REVISION\tPLAN\tOPERATION");
                for entry in history {
                    let operation = entry
                        .details
                        .as_ref()
                        .map_or("legacy", |details| details.request.operation());
                    println!("{}\t{}\t{operation}", entry.revision, entry.plan_id);
                }
            }
            TransactionCommand::Show { root, plan } => {
                let entry = transaction_details(&root, &plan)?;
                println!("{}", serde_json::to_string_pretty(&entry)?);
            }
        },
        Command::Fetch { root, plan } => {
            let saved = load_plan(&plan)?;
            let count = fetch_plan(&root, &saved)?;
            println!("Verified {count} plan payloads");
        }
        Command::Verify { root, pristine } => {
            let findings = if pristine {
                verify_pristine(&root)?
            } else {
                verify(&root)?
            };
            for finding in &findings {
                if finding.is_error {
                    eprintln!("error: {}", finding.message);
                } else {
                    println!("note: {}", finding.message);
                }
            }
            if findings.iter().any(|finding| finding.is_error) {
                return Ok(ExitCode::FAILURE);
            }
            println!("Image verified");
        }
        Command::BootEnv { command } => match command {
            BootCommand::List {
                esp,
                disk,
                esp_partuuid,
            } => {
                let environments = match (esp.as_deref(), disk.as_deref(), esp_partuuid.as_deref())
                {
                    (Some(esp), None, None) => list_boot_environments(esp)?,
                    (None, Some(disk), Some(partuuid)) => {
                        list_boot_environments_on_disk(disk, partuuid)?
                    }
                    _ => anyhow::bail!("specify --esp or both --disk and --esp-partuuid"),
                };
                if environments.is_empty() {
                    println!("No boot environments registered");
                } else {
                    println!("DEFAULT\tSTATUS\tID\tROOT PARTUUID\tTITLE");
                    for environment in environments {
                        let marker = if environment.is_default { "*" } else { "-" };
                        let status = match environment.tries_left {
                            None => "known-good".to_owned(),
                            Some(0) => "exhausted".to_owned(),
                            Some(tries) => format!("trial ({tries} left)"),
                        };
                        println!(
                            "{marker}\t{status}\t{}\t{}\t{}",
                            environment.id, environment.root_partuuid, environment.title
                        );
                    }
                }
            }
            BootCommand::Rollback {
                esp,
                disk,
                esp_partuuid,
                output_disk,
                id,
            } => {
                let id = match (
                    esp.as_deref(),
                    disk.as_deref(),
                    esp_partuuid.as_deref(),
                    output_disk.as_deref(),
                ) {
                    (Some(esp), None, None, None) => rollback_boot_environment(esp, id.as_deref())?,
                    (None, Some(disk), Some(partuuid), Some(output)) => {
                        rollback_boot_environment_on_disk(disk, partuuid, id.as_deref(), output)?
                    }
                    _ => anyhow::bail!("specify --esp or the offline disk and output options"),
                };
                println!("Selected {id} for the next boot");
            }
            BootCommand::Stage { source, root, plan } => {
                let id = stage_boot_environment(&source, &root, &load_plan(&plan)?)?;
                println!("Staged {id} at {}", root.display());
            }
            BootCommand::Export {
                root,
                output,
                size_mib,
                receipt,
            } => {
                if receipt {
                    let receipt = export_boot_environment_receipt(&root, &output, size_mib)?;
                    std::io::stdout().lock().write_all(receipt.as_bytes())?;
                } else {
                    let id = export_boot_environment(&root, &output, size_mib)?;
                    println!("Exported {id} to {}", output.display());
                }
            }
            BootCommand::Deploy {
                disk,
                root_image,
                root_partuuid,
                output,
            } => {
                let hash = deploy_boot_environment(&disk, &root_image, &root_partuuid, &output)?;
                println!(
                    "Deployed root to {root_partuuid} in {} (SHA-256 {hash})",
                    output.display()
                );
            }
            BootCommand::ActivateDevice {
                root,
                esp,
                device,
                root_partuuid,
                receipt,
                root_image,
                replace,
                kernel,
                initrd,
                options,
                tries,
                known_good,
            } => {
                let activation = Activation {
                    root_partuuid,
                    kernel,
                    initrd,
                    options,
                    tries,
                    known_good,
                };
                let id = activate_device_boot_environment(
                    &root,
                    &esp,
                    &activation,
                    &device,
                    &receipt,
                    root_image.as_deref().map(|image| RootDeployment {
                        image,
                        replace: replace.as_deref(),
                    }),
                )?;
                println!("Activated {id}");
            }
            BootCommand::Activate {
                root,
                esp,
                root_partuuid,
                disk,
                receipt,
                esp_partuuid,
                output_disk,
                root_image,
                replace,
                kernel,
                initrd,
                options,
                tries,
                known_good,
            } => {
                let activation = Activation {
                    root_partuuid,
                    kernel,
                    initrd,
                    options,
                    tries,
                    known_good,
                };
                let id = match (
                    esp.as_deref(),
                    disk.as_deref(),
                    receipt.as_deref(),
                    esp_partuuid.as_deref(),
                    output_disk.as_deref(),
                ) {
                    (Some(esp), Some(disk), Some(receipt), None, None) => {
                        activate_deployed_boot_environment(&root, esp, &activation, disk, receipt)?
                    }
                    (Some(esp), None, None, None, None) => {
                        activate_boot_environment(&root, esp, &activation)?
                    }
                    (None, Some(disk), Some(receipt), Some(partuuid), Some(output)) => {
                        if let Some(image) = root_image.as_deref() {
                            deploy_and_activate_boot_environment(
                                &root,
                                &activation,
                                disk,
                                partuuid,
                                receipt,
                                output,
                                RootDeployment {
                                    image,
                                    replace: replace.as_deref(),
                                },
                            )?
                        } else {
                            activate_boot_environment_on_disk(
                                &root,
                                &activation,
                                disk,
                                partuuid,
                                receipt,
                                output,
                            )?
                        }
                    }
                    _ => anyhow::bail!("specify --esp or the offline disk and output options"),
                };
                println!("Activated {id}");
            }
        },
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cps: {error:#}");
            ExitCode::FAILURE
        }
    }
}
