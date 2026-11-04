use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use cps::query::{PackageRow, Scope, list_packages, package_manifest};
use cps::{
    Activation, BuildOptions, BuildWitness, Plan, Request, SourceLayout, activate_boot_environment,
    add_publisher, apply, build_with_worker, create_configured_image, create_image, fetch_plan,
    fetch_sources, generate_manifest, import_sources, list_boot_environments, list_obsoletions,
    list_publishers, list_withdrawals, plan, prefer_publisher, publish, publish_release,
    publish_witnessed_bundle, read_attestation, read_worker_config, release, remove_publisher,
    rollback_boot_environment, rotate_publisher_key, stage_boot_environment, verify,
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
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
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
        /// Cache the catalog, manifests, and build evidence without downloading payloads
        #[arg(long)]
        index_only: bool,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Install one or more packages, optionally specifying publisher and version
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Reconcile an image to the requested packages and their dependencies
    ExactInstall {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Remove installed packages without contacting the repository
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Restore damaged paths from the installed package versions
    Repair { packages: Vec<String> },
    /// Update the installed package set
    Update,
}

#[derive(Subcommand)]
enum BootCommand {
    /// List registered boot environments and the configured default
    List {
        #[arg(long)]
        esp: PathBuf,
    },
    /// Select a known-good environment for the next boot
    Rollback {
        #[arg(long)]
        esp: PathBuf,
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
    /// Publish a systemd-boot entry for a verified environment
    Activate {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        esp: PathBuf,
        #[arg(long)]
        root_partuuid: String,
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
                SourceCommand::Fetch { cache, recipe } => fetch_sources(&recipe, &cache)?,
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
            sources,
            source_cache,
            output,
            check_reproducible,
            worker_config,
            builder_signing_key,
            recipe,
        } => {
            let options = BuildOptions {
                repo: &repo,
                publisher: &publisher,
                trust_key: &trust_key,
                sources: source_cache
                    .as_deref()
                    .or(sources.as_deref())
                    .context("missing build sources")?,
                output: &output,
                check_reproducible,
                builder_signing_key: builder_signing_key.as_deref(),
            };
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
            let id = build_with_worker(&recipe, options, layout, &worker)?;
            println!("Built {id} at {}", output.display());
        }
        Command::Manifest { command } => match command {
            ManifestCommand::Generate {
                proto,
                publisher,
                version,
                uid,
                gid,
                name,
            } => {
                let text = generate_manifest(&proto, &publisher, &name, &version, uid, gid)?;
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
            } => {
                let sync = if index_only {
                    release::sync_index
                } else {
                    release::sync
                };
                println!(
                    "Synchronized {}",
                    sync(&repo, &root, &metadata, &targets, &publisher, &trust_key)?
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
                name,
            } => {
                add_publisher(&root, &name, &repo, &trust_key)?;
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
        Command::Plan {
            root,
            output,
            request,
        } => {
            let request = match request {
                PlanCommand::Install { packages } => Request::Install { packages },
                PlanCommand::ExactInstall { packages } => Request::ExactInstall { packages },
                PlanCommand::Remove { packages } => Request::Remove { packages },
                PlanCommand::Repair { packages } => Request::Repair { packages },
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
            BootCommand::List { esp } => {
                let environments = list_boot_environments(&esp)?;
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
            BootCommand::Rollback { esp, id } => {
                let id = rollback_boot_environment(&esp, id.as_deref())?;
                println!("Selected {id} for the next boot");
            }
            BootCommand::Stage { source, root, plan } => {
                let id = stage_boot_environment(&source, &root, &load_plan(&plan)?)?;
                println!("Staged {id} at {}", root.display());
            }
            BootCommand::Activate {
                root,
                esp,
                root_partuuid,
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
                let id = activate_boot_environment(&root, &esp, &activation)?;
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
