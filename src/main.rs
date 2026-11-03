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
    Activation, Plan, Request, activate_boot_environment, apply, create_image, plan, publish,
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
    /// Initialize an empty directory as an image
    ImageCreate {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_key: PathBuf,
        #[arg(long, default_value = std::env::consts::ARCH)]
        arch: String,
        #[arg(long, value_name = "NAME=BOOL")]
        facet: Vec<String>,
    },
    /// Preview and save an install or update plan
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
    /// Stage and activate systemd-boot environments
    BootEnv {
        #[command(subcommand)]
        command: BootCommand,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Install one or more packages
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Update the installed package set
    Update,
}

#[derive(Subcommand)]
enum BootCommand {
    /// Clone the base image and apply a saved update in a new directory
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
        Command::ImageCreate {
            root,
            repo,
            publisher,
            trust_key,
            arch,
            facet,
        } => {
            let id = create_image(
                &root,
                &repo,
                &publisher,
                &trust_key,
                &arch,
                parse_facets(facet)?,
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
