use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use cps::{
    Activation, BuildOptions, ImageConfig, Plan, Request, activate_boot_environment, apply,
    build_package, create_image, list_boot_environments, plan, publish, publish_bundle,
    read_attestation, read_image_config, release, rollback_boot_environment,
    stage_boot_environment, verify,
};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a package with verified inputs in an offline sandbox
    Build {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        #[arg(long)]
        sources: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        check_reproducible: bool,
        #[arg(long, requires = "check_reproducible")]
        builder_signing_key: Option<PathBuf>,
        recipe: PathBuf,
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
    /// Preview and save a package transaction
    Plan {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[command(subcommand)]
        request: PlanCommand,
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
    },
    /// Manage systemd-boot environments
    BootEnv {
        #[command(subcommand)]
        command: BootCommand,
    },
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Print the signed build evidence for a full package ID in the current catalog
    Attestation {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        package_id: String,
    },
    /// Export the current catalog and its objects for TUF signing
    Export {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        #[arg(long)]
        output: PathBuf,
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
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Install one or more packages
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Reconcile an image to the named packages and their dependencies
    ExactInstall {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Remove installed packages without contacting the repository
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
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

fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Build {
            repo,
            publisher,
            trust_key,
            sources,
            output,
            check_reproducible,
            builder_signing_key,
            recipe,
        } => {
            let id = build_package(
                &recipe,
                BuildOptions {
                    repo: &repo,
                    publisher: &publisher,
                    trust_key: &trust_key,
                    sources: &sources,
                    output: &output,
                    check_reproducible,
                    builder_signing_key: builder_signing_key.as_deref(),
                },
            )?;
            println!("Built {id} at {}", output.display());
        }
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
            bundle,
        } => {
            println!(
                "Published {}",
                publish_bundle(&repo, &bundle, &signing_key, &builder_key, &recipe_sha256)?
            );
        }
        Command::Repo { command } => match command {
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
            RepoCommand::Export {
                repo,
                publisher,
                trust_key,
                output,
            } => {
                println!(
                    "Exported {}",
                    release::export(&repo, &publisher, &trust_key, &output)?
                );
            }
            RepoCommand::Sync {
                repo,
                root,
                metadata,
                targets,
                publisher,
                trust_key,
            } => {
                println!(
                    "Synchronized {}",
                    release::sync(&repo, &root, &metadata, &targets, &publisher, &trust_key)?
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
            let config = match config {
                Some(path) => read_image_config(&path)?,
                None => ImageConfig {
                    repository: repo.context("missing repository")?,
                    publisher: publisher.context("missing publisher")?,
                    trust_key: trust_key.context("missing trust key")?,
                    arch: arch.unwrap_or_else(|| std::env::consts::ARCH.to_owned()),
                    facets: parse_facets(facet)?,
                },
            };
            let id = create_image(
                &root,
                &config.repository,
                &config.publisher,
                &config.trust_key,
                &config.arch,
                config.facets,
            )?;
            println!("Created image {id} at {}", root.display());
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
        Command::Verify { root } => {
            let findings = verify(&root)?;
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
