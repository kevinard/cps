use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File},
    io::{self, IsTerminal, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use cps::query::{
    PackageRow, Scope, direct_dependencies, explain_package, list_facets, list_package_intent,
    list_packages, package_contents, package_manifest, path_owners, reverse_dependencies,
    search_files,
};
use cps::system::{self, System, UpdateStatus};
use cps::{
    Activation, BuildSettings, BuildToolchain, BuildWitness, ConfigChoice, GitSnapshotRef,
    MarkChoice, PackageChange, PhasedUpdateDeferred, Plan, PlanOptions, Request, RootDeployment,
    SourceLayout, activate_boot_environment, activate_boot_environment_on_disk,
    activate_deployed_boot_environment, activate_device_boot_environment, add_publisher_from_url,
    add_publisher_with_build_trust, apply, archive_build_sources, audit_publisher_builds,
    build_with_recipe_options, capture_boot_environment, create_image,
    deploy_and_activate_boot_environment, deploy_boot_environment, destroy_btrfs_boot_environment,
    export_boot_environment, export_boot_environment_receipt, export_rebuild_inputs,
    fetch_build_sources, fetch_plan_with_archives, fetch_sources, fetch_sources_with_local,
    fetch_toolchain, generate_manifest_for_architecture, import_sources, list_boot_environments,
    list_boot_environments_on_disk, list_config_updates, list_freezes, list_obsoletions,
    list_publishers, list_withdrawals, plan_with_options, prefer_publisher,
    prepare_boot_environment, prepare_git_snapshot_ref, publish_witnessed_bundle_with_sources,
    read_attestation, read_worker_config, release, remove_publisher, repair_pending_transaction,
    resume_transaction, rollback_boot_environment, rollback_boot_environment_on_disk,
    rollback_btrfs_boot_environment, set_publisher_build_trust, set_publisher_repositories,
    stage_boot_environment, stage_btrfs_boot_environment, suggest_dependencies,
    transaction_details, transaction_history, transaction_status, verify, verify_pristine,
    verify_rebuild, withdraw_package,
};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install packages on the running system
    Install {
        #[command(flatten)]
        options: SystemOptions,
        /// Stage the transaction in a new Btrfs trial boot environment
        #[arg(long)]
        stage: bool,
        /// Build and install the package described by cps.kdl in this Git repository
        #[arg(long, requires = "git_ref", conflicts_with = "packages")]
        git: Option<String>,
        /// Pin the install to a full Git commit ID
        #[arg(long, requires = "git", group = "git_ref")]
        commit: Option<String>,
        /// Follow this branch when running cps upgrade
        #[arg(long, requires = "git", group = "git_ref")]
        track_branch: Option<String>,
        /// Require Git signatures from the signers in ssh:PATH or openpgp:PATH
        #[arg(long, requires = "git", value_name = "FORMAT:PATH")]
        allowed_signers: Option<String>,
        /// Which Git object must carry the allowed signature
        #[arg(
            long,
            requires = "allowed_signers",
            value_enum,
            default_value = "commit"
        )]
        require_signed: GitSignedArg,
        #[arg(required_unless_present = "git", value_name = "PACKAGE")]
        packages: Vec<String>,
    },
    /// Upgrade the system and stage changes that require a reboot
    Upgrade {
        #[command(flatten)]
        options: SystemOptions,
        /// Stage the transaction in a new Btrfs trial boot environment
        #[arg(long)]
        stage: bool,
        /// Upgrade even if this image is outside the signed profile rollout
        #[arg(long)]
        force_phased_update: bool,
        /// Use only the distribution publishers for this upgrade
        #[arg(long)]
        base_only: bool,
    },
    /// Check for upgrades and report any blockers or reboot requirement
    CheckUpdates {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
        /// Report the result as JSON
        #[arg(long)]
        json: bool,
        /// Use the currently cached catalogs
        #[arg(long)]
        no_refresh: bool,
        /// Check updates from distribution publishers only
        #[arg(long)]
        base_only: bool,
        /// Probe the exact newer version of this held-back package
        #[arg(long, value_name = "PACKAGE")]
        explain: Option<String>,
    },
    /// Remove installed packages and stage changes that require a reboot
    Remove {
        #[command(flatten)]
        options: SystemOptions,
        /// Stage the transaction in a new Btrfs trial boot environment
        #[arg(long)]
        stage: bool,
        #[arg(required = true, value_name = "PACKAGE")]
        packages: Vec<String>,
    },
    /// Remove unused dependencies and stage changes that require a reboot
    Autoremove {
        #[command(flatten)]
        options: SystemOptions,
        /// Stage the transaction in a new Btrfs trial boot environment
        #[arg(long)]
        stage: bool,
    },
    /// Restore damaged managed files on the running system
    Repair {
        #[command(flatten)]
        options: SystemOptions,
        /// Prepare a verified recovery root for separate activation
        #[arg(long)]
        prepare_only: bool,
        #[arg(long = "path", value_name = "PATH", conflicts_with = "packages")]
        paths: Vec<String>,
        packages: Vec<String>,
    },
    /// Restore vendor defaults for preserved configuration on the running system
    Revert {
        #[command(flatten)]
        options: SystemOptions,
        /// Prepare a verified recovery root for separate activation
        #[arg(long)]
        prepare_only: bool,
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Show an interrupted change or a system waiting for reboot
    Status {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
        /// Report progress for a saved plan as JSON
        #[arg(long, value_name = "PLAN")]
        plan: Option<PathBuf>,
        /// Validate an unapplied plan against cached payloads
        #[arg(long, requires = "plan")]
        validate: bool,
    },
    /// Finish an interrupted live package transaction
    Resume {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
        /// Restore damaged managed paths from verified local payloads before retrying final actions
        #[arg(long)]
        repair: bool,
    },
    /// Prepare pinned package sources for offline builds
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Build a package with verified inputs in an offline sandbox
    Build {
        #[command(flatten)]
        inputs: BuildInputs,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        check_reproducible: bool,
        #[arg(long, requires = "check_reproducible")]
        builder_signing_key: Option<PathBuf>,
        recipe: PathBuf,
    },
    /// Run a shell or command with a recipe's verified toolchain and sources
    Develop {
        #[command(flatten)]
        inputs: BuildInputs,
        /// Host directory to mount read-write at /build/workspace
        #[arg(long)]
        workspace: Option<PathBuf>,
        recipe: PathBuf,
        /// Run this command instead of starting an interactive shell
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Prepare a package manifest for review
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
    /// Publish manifests and their payloads to a local repository in one signed catalog update
    Publish {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        proto: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        #[arg(required = true)]
        manifests: Vec<PathBuf>,
    },
    /// Publish a reproducible build bundle after verifying its worker signature and reviewed recipe
    PublishBundle {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        #[arg(long)]
        builder_key: PathBuf,
        #[arg(long)]
        recipe_sha256: String,
        #[arg(long, value_name = "KEY=ATTESTATION", value_parser = parse_witness)]
        witness: Vec<BuildWitness>,
        /// Cache of verified source files to include in the publication
        #[arg(long)]
        source_cache: Option<PathBuf>,
        bundle: PathBuf,
    },
    /// Export or synchronize a signed repository release
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Compare committed package state between two roots, offline
    ImageDiff {
        before: PathBuf,
        after: PathBuf,
        /// Include managed file actions and ownership
        #[arg(long)]
        paths: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print a portable lock for an image's committed package composition
    ImageLock {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Export the lock and its verified provisioning objects to a new directory
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// Initialize an empty directory as an image
    ImageCreate {
        #[arg(long)]
        root: PathBuf,
        #[arg(
            long,
            conflicts_with_all = ["repo", "publisher", "trust_root", "arch", "facet", "variant"]
        )]
        config: Option<PathBuf>,
        /// Initialize with the publisher layout and bootstrap catalog of this lock
        #[arg(long, requires = "config")]
        lock: Option<PathBuf>,
        /// Initialize from a portable image bundle using locally configured trust
        #[arg(long, requires = "config", conflicts_with = "lock")]
        bundle: Option<PathBuf>,
        /// Bootstrap the configured release profile from this exact signed catalog
        #[arg(long, requires = "config", conflicts_with_all = ["lock", "bundle"])]
        bootstrap_catalog: Option<String>,
        /// Use this primary repository when bootstrapping a pinned release profile
        #[arg(long, requires = "bootstrap_catalog")]
        primary_repo: Option<PathBuf>,
        #[arg(long, required_unless_present = "config")]
        repo: Option<PathBuf>,
        #[arg(long, required_unless_present = "config")]
        publisher: Option<String>,
        #[arg(long, required_unless_present = "config")]
        trust_root: Option<PathBuf>,
        #[arg(long)]
        arch: Option<String>,
        /// Set an image-wide facet rule (FACET, PREFIX.*, or *) or a per-package rule
        /// (PACKAGE:FACET, where PACKAGE may end in /*)
        #[arg(long, value_name = "[PACKAGE:]FACET=BOOL")]
        facet: Vec<String>,
        /// Apply the per-package --facet rules to each package's installed dependencies too
        #[arg(long, requires = "facet")]
        with_dependencies: bool,
        /// Set a variant other than arch, such as container=true
        #[arg(long, value_name = "NAME=VALUE")]
        variant: Vec<String>,
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
    /// List facet rules, or show which rule decides each named facet
    Facet {
        #[arg(long)]
        root: PathBuf,
        /// Resolve facets for this installed package, including per-package rules;
        /// without FACET arguments, show every facet its actions use
        #[arg(long)]
        package: Option<String>,
        /// Show the effective value of these facets instead of listing rules
        #[arg(value_name = "FACET")]
        facets: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the image's variant values, including arch
    Variant {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Show preferred and active providers for mediated links
    Mediator {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List persistent package version freezes, offline
    Freeze {
        #[arg(long)]
        root: PathBuf,
    },
    /// List installed packages or versions in the configured signed catalogs
    List {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(long)]
        available: bool,
        #[arg(long, requires = "available")]
        all_versions: bool,
        /// Show local requests and roles in the installed release profile
        #[arg(long, conflicts_with = "available")]
        intent: bool,
    },
    /// Search for packages by name, summary, or a file they ship
    Search {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(long)]
        installed: bool,
        /// Match an exact path or basename in the signed catalogs
        #[arg(long, conflicts_with = "installed")]
        file: bool,
        term: String,
    },
    /// Show an installed or available package's manifest in KDL
    Info {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(long)]
        available: bool,
        #[arg(value_name = "NAME[@VERSION]")]
        package: String,
    },
    /// Show package candidates, publisher order, and image constraints
    Policy {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(long)]
        json: bool,
        /// Try planning an install of this selector without changing the image
        #[arg(long)]
        probe: bool,
        package: String,
    },
    /// Show installed package IDs that own an exact image path, offline
    Owner {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Absolute or root-relative path within the image
        path: String,
    },
    /// List the paths owned by an installed package, offline
    Contents {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// List the installed packages that satisfy a package's direct requirements, offline
    Deps {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// List the installed packages that directly depend on a package, offline
    Rdeps {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// Explain why a package is installed, offline
    Why {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// Preview and save a package transaction
    Plan {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// Skip recommended packages when resolving the plan
        #[arg(long, global = true)]
        no_install_recommends: bool,
        #[command(subcommand)]
        request: PlanCommand,
    },
    /// Fetch the verified payloads required by a saved plan
    Fetch {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        /// Use a synced historical archive for pinned metadata and matching payloads
        #[arg(long = "archive-cache", value_name = "PUBLISHER=PATH", value_parser = parse_repository)]
        archive_caches: Vec<(String, PathBuf)>,
    },
    /// Validate a saved plan against cached payloads before applying it
    Validate {
        #[arg(long)]
        root: PathBuf,
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
        #[arg(long, conflicts_with_all = ["packages", "paths"])]
        pristine: bool,
        /// Limit file verification to the installed package
        #[arg(long = "package", value_name = "PACKAGE")]
        packages: Vec<String>,
        /// Limit file verification to this managed path
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// Audit the installed managed ELF interpreter and shared-library closure
        #[arg(long, conflicts_with_all = ["packages", "paths"])]
        runtime: bool,
        #[arg(long = "library-dir", requires = "runtime", value_name = "PATH")]
        library_dirs: Vec<String>,
    },
    /// Stage, activate, and manage boot environments
    BootEnv {
        #[command(subcommand)]
        command: BootCommand,
    },
}

#[derive(Args)]
struct BuildInputs {
    #[arg(long, required_unless_present = "toolchain_config")]
    repo: Option<PathBuf>,
    #[arg(long, required_unless_present = "toolchain_config")]
    publisher: Option<String>,
    #[arg(long, required_unless_present = "toolchain_config")]
    trust_root: Option<PathBuf>,
    /// Worker-local repository and trust key locations for the pinned publishers
    #[arg(long, conflicts_with_all = ["repo", "publisher", "trust_root"])]
    toolchain_config: Option<PathBuf>,
    #[arg(long, required_unless_present = "source_cache")]
    sources: Option<PathBuf>,
    #[arg(long, conflicts_with = "sources")]
    source_cache: Option<PathBuf>,
    #[arg(long)]
    worker_config: Option<PathBuf>,
    /// Override a build option declared in the recipe
    #[arg(long = "option", value_name = "NAME=VALUE")]
    recipe_options: Vec<String>,
}

struct PreparedBuildInputs<'a> {
    sources: (&'a Path, SourceLayout),
    toolchain: BuildToolchain<'a>,
    worker: cps::BuildWorker,
    overrides: BTreeMap<String, String>,
}

impl BuildInputs {
    fn prepare(&self) -> Result<PreparedBuildInputs<'_>> {
        let input = self
            .source_cache
            .as_deref()
            .or(self.sources.as_deref())
            .context("missing build sources")?;
        let layout = if self.source_cache.is_some() {
            SourceLayout::Cache
        } else {
            SourceLayout::Directory
        };
        let worker = self
            .worker_config
            .as_deref()
            .map(read_worker_config)
            .transpose()?
            .unwrap_or_default();
        let toolchain = if let Some(config) = self.toolchain_config.as_deref() {
            BuildToolchain::Config(config)
        } else {
            BuildToolchain::Single {
                repo: self
                    .repo
                    .as_deref()
                    .context("missing toolchain repository")?,
                publisher: self
                    .publisher
                    .as_deref()
                    .context("missing toolchain publisher")?,
                trust_key: self
                    .trust_root
                    .as_deref()
                    .context("missing toolchain trust root")?,
            }
        };
        let mut overrides = BTreeMap::new();
        for option in &self.recipe_options {
            let (name, value) = option
                .split_once('=')
                .context("expected --option NAME=VALUE")?;
            ensure!(
                overrides
                    .insert(name.to_owned(), value.to_owned())
                    .is_none(),
                "duplicate --option {name}"
            );
        }
        Ok(PreparedBuildInputs {
            sources: (input, layout),
            toolchain,
            worker,
            overrides,
        })
    }
}

#[derive(Args)]
struct SystemOptions {
    /// Distribution configuration for the running system
    #[arg(long, default_value = system::CONFIG)]
    config: PathBuf,
    /// Confirm the displayed transaction without prompting
    #[arg(short = 'y', long, conflicts_with = "dry_run")]
    yes: bool,
    /// Preview the transaction without applying it
    #[arg(long)]
    dry_run: bool,
    /// Use the currently cached catalog
    #[arg(long)]
    no_refresh: bool,
    /// Skip recommended packages when resolving the transaction
    #[arg(long)]
    no_install_recommends: bool,
    /// Save the exact transaction plan for progress reporting
    #[arg(long, value_name = "PATH")]
    save_plan: Option<PathBuf>,
}

#[derive(Subcommand)]
enum SourceCommand {
    /// Pin a Git branch, tag, or commit and prepare a versioned build recipe
    #[command(group(clap::ArgGroup::new("git_ref").required(true).args(["branch", "tag", "commit"])))]
    GitSnapshot {
        /// Local Git repository path or remote Git URL
        #[arg(long)]
        git_repo: OsString,
        /// Branch name in the Git repository
        #[arg(long)]
        branch: Option<String>,
        /// Tag name in the Git repository
        #[arg(long)]
        tag: Option<String>,
        /// Full SHA-1 or SHA-256 commit ID
        #[arg(long)]
        commit: Option<String>,
        /// Local CPS repository used to allocate snapshot versions
        #[arg(long)]
        repo: PathBuf,
        /// Ed25519 DER public key for the package publisher
        #[arg(long)]
        trust_root: PathBuf,
        /// New directory for the pinned recipe, manifest, and archive
        #[arg(long)]
        output: PathBuf,
        /// Build recipe template with a pinned package manifest
        recipe: PathBuf,
    },
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
    /// Suggest runtime dependencies from ELF and shebangs using pinned local catalogs
    Dependencies {
        #[arg(long)]
        proto: PathBuf,
        /// Image configuration with target architecture, facets, and local publisher paths
        #[arg(long)]
        config: PathBuf,
        /// Exact signed catalog digest for each configured publisher
        #[arg(long = "catalog", required = true, value_name = "PUBLISHER=SHA256")]
        catalogs: Vec<String>,
        /// Default ELF library directory in search order
        #[arg(long = "library-dir", required = true, value_name = "PATH")]
        library_dirs: Vec<String>,
        /// Target PATH directory for env shebangs, in search order
        #[arg(long = "path-dir", default_values = ["/usr/bin", "/bin"], value_name = "PATH")]
        path_dirs: Vec<String>,
        /// Fail when a detected external provider lacks a matching direct require
        #[arg(long)]
        check: bool,
        manifest: PathBuf,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum GitSignedArg {
    /// The installed commit itself
    Commit,
    /// An annotated tag that points at the installed commit
    Tag,
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
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Existing local repository
        #[arg(long, conflicts_with = "url", requires = "trust_root")]
        repo: Option<PathBuf>,
        /// Base URL of a TUF mirror containing metadata/ and targets/
        #[arg(long, requires_all = ["tuf_root", "trust_root"])]
        url: Option<String>,
        /// Trusted TUF root metadata for a mirror
        #[arg(long, conflicts_with = "repo")]
        tuf_root: Option<PathBuf>,
        /// Cache directory for a URL mirror
        #[arg(long, conflicts_with = "repo")]
        cache: Option<PathBuf>,
        /// Pinned CPS publisher trust root for --repo or --url
        #[arg(long)]
        trust_root: Option<PathBuf>,
        /// Require builder signatures from this KDL policy
        #[arg(long)]
        build_trust: Option<PathBuf>,
        /// With an HTTPS mirror URL: require exactly these root key SHA-256
        /// fingerprints instead of asking (repeat for each root key)
        #[arg(long, value_name = "SHA256", conflicts_with_all = ["repo", "url"])]
        expect_root_key: Vec<String>,
        /// Publisher name with --repo or --url, or an HTTPS mirror URL
        #[arg(value_name = "NAME_OR_URL")]
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

/// Membership of the next trust root version. Omitted keys and thresholds
/// keep the previous version's values.
#[derive(Args)]
struct TrustMembership {
    /// Publisher name; required for the first version
    #[arg(long)]
    publisher: Option<String>,
    /// Root key: a public key file or command:DESCRIPTOR (repeat; replaces the root keys)
    #[arg(long, value_name = "PUBLIC_KEY")]
    root_key: Vec<PathBuf>,
    #[arg(long)]
    root_threshold: Option<usize>,
    /// Catalog key: a public key file or command:DESCRIPTOR (repeat; replaces the catalog keys)
    #[arg(long, value_name = "PUBLIC_KEY")]
    catalog_key: Vec<PathBuf>,
    #[arg(long)]
    catalog_threshold: Option<usize>,
    /// RFC 3339 UTC expiry, such as 2027-01-01T00:00:00Z (default: one year from now)
    #[arg(long)]
    expires: Option<String>,
}

impl TrustMembership {
    fn draft(&self) -> cps::TrustDraft<'_> {
        cps::TrustDraft {
            publisher: self.publisher.as_deref(),
            root_keys: &self.root_key,
            root_threshold: self.root_threshold,
            catalog_keys: &self.catalog_key,
            catalog_threshold: self.catalog_threshold,
            expires: self.expires.as_deref(),
        }
    }
}

#[derive(Subcommand)]
enum TrustCommand {
    /// Create a repository's first trust root with one key as root and catalog key
    Init {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        /// The only root and catalog key: PATH, file:PATH, or command:DESCRIPTOR
        #[arg(long, value_name = "KEY")]
        key: PathBuf,
        /// RFC 3339 UTC expiry, such as 2027-01-01T00:00:00Z (default: one year from now)
        #[arg(long)]
        expires: Option<String>,
    },
    /// Sign and store the next trust root version with local or external root keys
    Rotate {
        #[arg(long)]
        repo: PathBuf,
        #[command(flatten)]
        membership: TrustMembership,
        /// Root key that signs: PATH, file:PATH, or command:DESCRIPTOR (repeat)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
    },
    /// Write the unsigned next trust root version for detached signing
    Draft {
        #[arg(long)]
        repo: PathBuf,
        #[command(flatten)]
        membership: TrustMembership,
        #[arg(long)]
        output: PathBuf,
    },
    /// Sign a trust root draft and write detached signatures
    Sign {
        #[arg(long)]
        draft: PathBuf,
        /// Root key that signs: PATH, file:PATH, or command:DESCRIPTOR (repeat)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Combine a draft with its signatures and store it as the next version
    Commit {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        draft: PathBuf,
        /// Detached signatures from `cps repo trust sign` (repeat)
        #[arg(long)]
        signature: Vec<PathBuf>,
        /// Root key that also signs: PATH, file:PATH, or command:DESCRIPTOR (repeat)
        #[arg(long, value_name = "KEY")]
        key: Vec<PathBuf>,
    },
    /// Show a trust root file, or the newest version in a repository
    Show {
        /// A trust root file or a repository directory
        path: PathBuf,
    },
}

fn run_trust(command: TrustCommand) -> Result<()> {
    match command {
        TrustCommand::Init {
            repo,
            publisher,
            key,
            expires,
        } => {
            let path = cps::init_trust(&repo, &publisher, &key, expires.as_deref())?;
            println!("Created {}", path.display());
            print!("{}", cps::show_trust(&path)?);
        }
        TrustCommand::Rotate {
            repo,
            membership,
            key,
        } => {
            let path = cps::rotate_trust(&repo, &membership.draft(), &key)?;
            println!("Created {}", path.display());
            print!("{}", cps::show_trust(&path)?);
        }
        TrustCommand::Draft {
            repo,
            membership,
            output,
        } => {
            let draft = cps::draft_trust(&repo, &membership.draft())?;
            write_new_file(&output, &draft)?;
            println!("Wrote {}", output.display());
        }
        TrustCommand::Sign { draft, key, output } => {
            write_new_file(&output, &cps::sign_trust(&draft, &key)?)?;
            println!("Wrote {}", output.display());
        }
        TrustCommand::Commit {
            repo,
            draft,
            signature,
            key,
        } => {
            let path = cps::commit_trust(&repo, &draft, &signature, &key)?;
            println!("Created {}", path.display());
            print!("{}", cps::show_trust(&path)?);
        }
        TrustCommand::Show { path } => print!("{}", cps::show_trust(&path)?),
    }
    Ok(())
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Widen one profile's rollout on the exact current catalog
    WidenRollout {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        #[arg(long)]
        base: String,
        #[arg(long)]
        profile: String,
        #[arg(long)]
        percentage: u8,
    },
    /// Sign an exported mirror with sharded TUF targets metadata
    SignExport {
        #[arg(long)]
        root: PathBuf,
        /// Targets role key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the threshold)
        #[arg(long, value_name = "KEY", required = true)]
        targets_key: Vec<PathBuf>,
        /// Snapshot role key: PATH, file:PATH, or command:DESCRIPTOR (repeatable)
        #[arg(long, value_name = "KEY", required = true)]
        snapshot_key: Vec<PathBuf>,
        /// Timestamp role key: PATH, file:PATH, or command:DESCRIPTOR (repeatable)
        #[arg(long, value_name = "KEY", required = true)]
        timestamp_key: Vec<PathBuf>,
        /// Key for the objects-* delegations (repeatable; default: the first targets key)
        #[arg(long, value_name = "KEY")]
        objects_key: Vec<PathBuf>,
        /// Signatures required on each objects-* delegation
        #[arg(long, default_value = "1")]
        objects_threshold: std::num::NonZeroU64,
        #[arg(long)]
        version: std::num::NonZeroU64,
        #[arg(long)]
        release_expires: String,
        #[arg(long)]
        timestamp_expires: String,
        #[arg(long)]
        output: PathBuf,
        export: PathBuf,
    },
    /// Re-sign the timestamp, and optionally the snapshot, of a published mirror
    RefreshMetadata {
        #[arg(long)]
        root: PathBuf,
        /// The published versioned snapshot metadata, such as 7.snapshot.json
        #[arg(long)]
        snapshot: PathBuf,
        /// Timestamp role key: PATH, file:PATH, or command:DESCRIPTOR (repeatable)
        #[arg(long, value_name = "KEY", required = true)]
        timestamp_key: Vec<PathBuf>,
        /// Also re-sign the snapshot with this key (repeatable)
        #[arg(long, value_name = "KEY", requires = "snapshot_expires")]
        snapshot_key: Vec<PathBuf>,
        #[arg(long, requires = "snapshot_key")]
        snapshot_expires: Option<String>,
        #[arg(long)]
        version: std::num::NonZeroU64,
        #[arg(long)]
        timestamp_expires: String,
        /// New directory for timestamp.json and any re-signed snapshot
        #[arg(long)]
        output: PathBuf,
    },
    /// Replace old withdrawal records with signed tombstones up to a catalog sequence
    PruneWithdrawals {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        /// Exact current catalog to build on
        #[arg(long)]
        base: String,
        /// Prune records withdrawn at or before this catalog sequence
        #[arg(long, value_name = "SEQUENCE")]
        through: u64,
    },
    /// Enable a catalog-signed index for available package file lookups
    IndexFiles {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
    },
    /// Publish the exact current catalog again under the newest trust root
    Resign {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        /// Exact current catalog to build on
        #[arg(long)]
        base: String,
    },
    /// Create, rotate, sign, and inspect the publisher trust root
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
    /// List signed package obsoletion cutoffs
    Obsoletions {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
    },
    /// Commit a set of reviewed build bundles and withdrawals as one signed catalog
    PublishRelease {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        /// Cache of verified source files to include in the publication
        #[arg(long)]
        source_cache: Option<PathBuf>,
        /// Store the signed candidate without advancing the current catalog
        #[arg(long, conflicts_with = "expect_catalog")]
        prepare: bool,
        /// Publish only if the candidate equals this previously tested catalog
        #[arg(long, value_name = "SHA256")]
        expect_catalog: Option<String>,
        release: PathBuf,
    },
    /// Print the signed build evidence for a full package ID
    Attestation {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
        package_id: String,
    },
    /// Extract the recipe, reviewed manifest, and attestation for an exact package
    RebuildInputs {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
        /// Select a historical signed catalog by SHA-256
        #[arg(long)]
        catalog: Option<String>,
        /// New directory for the authenticated inputs
        #[arg(long)]
        output: PathBuf,
        package_id: String,
    },
    /// Compare a local build bundle with the attestation admitted by a signed catalog
    VerifyRebuild {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
        /// Select a historical signed catalog by SHA-256
        #[arg(long)]
        catalog: Option<String>,
        #[arg(long)]
        bundle: PathBuf,
        /// Verify the bundle's matching statement under a separate builder key
        #[arg(long)]
        witness_key: Option<PathBuf>,
        package_id: String,
    },
    /// Attach verified source files to a published build
    ArchiveSources {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        #[arg(long)]
        source_cache: PathBuf,
        package_id: String,
    },
    /// Fetch archived sources into a build source cache
    FetchSources {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
        #[arg(long)]
        source_cache: PathBuf,
        /// Select a historical signed catalog by SHA-256
        #[arg(long)]
        catalog: Option<String>,
        package_id: String,
    },
    /// Fetch payloads for the toolchain pinned by a build recipe
    FetchToolchain {
        #[arg(long)]
        toolchain_config: PathBuf,
        recipe: PathBuf,
    },
    /// Withdraw an exact published version from future package selections
    Withdraw {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        signing_key: Vec<PathBuf>,
        package_id: String,
    },
    /// List withdrawn package IDs and their original manifest digests
    Withdrawals {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
    },
    /// Export signed catalogs and their objects for TUF signing
    Export {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// Include a historical catalog and its objects in this mirror release
        #[arg(long = "retain-catalog")]
        retained: Vec<String>,
        /// Export byte-level payload patches against an earlier catalog
        #[arg(long = "delta-from", value_name = "SHA256")]
        delta_from: Vec<String>,
        /// Export an exact catalog to a separate archive mirror
        #[arg(long, value_name = "SHA256", conflicts_with_all = ["retained", "delta_from"])]
        archive_catalog: Option<String>,
        /// Include archived build sources as TUF targets
        #[arg(long)]
        include_sources: bool,
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
        trust_root: PathBuf,
        /// Check the signed catalog against this installed image before promotion
        #[arg(long)]
        image: Option<PathBuf>,
        /// Cache the catalog, manifests, and build evidence without downloading payloads
        #[arg(long)]
        index_only: bool,
        /// Require this catalog to be current or retained in the signed mirror release
        #[arg(long = "require-catalog", value_name = "SHA256")]
        required_catalogs: Vec<String>,
        /// Sync an exact catalog from a separate archive mirror into a dedicated cache
        #[arg(long, value_name = "SHA256", conflicts_with_all = ["image", "required_catalogs"])]
        archive_catalog: Option<String>,
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
        /// Fetch the signed profile's selected payloads before promoting it
        #[arg(long, conflicts_with = "index_only")]
        profile_payloads: bool,
    },
    /// Preview or remove unused payload files from a TUF mirror cache
    PruneCache {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        publisher: String,
        #[arg(long)]
        trust_root: PathBuf,
        /// Also retain payloads from this historical catalog
        #[arg(long = "keep-catalog")]
        catalogs: Vec<String>,
        /// Keep payloads for this image's installed packages and current profile
        #[arg(long = "keep-image")]
        images: Vec<PathBuf>,
        /// Keep payloads used by this saved plan
        #[arg(long = "keep-plan")]
        plans: Vec<PathBuf>,
        /// Keep the locked packages and payloads for this publisher
        #[arg(long = "keep-lock")]
        locks: Vec<PathBuf>,
        /// Retain the roots registered in this installed system's boot configuration
        #[arg(long)]
        system_config: Option<PathBuf>,
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
    /// Compare a preserved configuration file with its verified pending vendor copy
    Diff {
        #[arg(long)]
        root: PathBuf,
        path: String,
    },
}

#[derive(Subcommand)]
enum TransactionCommand {
    /// Show whether the image has an interrupted transaction
    Status {
        #[arg(long)]
        root: PathBuf,
    },
    /// Report progress for a saved plan as JSON
    Progress {
        #[arg(long)]
        root: PathBuf,
        /// Validate an unapplied plan against cached payloads
        #[arg(long)]
        validate: bool,
        plan: PathBuf,
    },
    /// Finish a journaled transaction using the files already staged in the image
    Resume {
        #[arg(long)]
        root: PathBuf,
        /// Restore damaged managed paths from verified local payloads before retrying final actions
        #[arg(long)]
        repair: bool,
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
    /// Select a provider for a mediated link group or restore the default
    SetMediator {
        group: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Freeze installed or explicitly named versions until they are unfrozen
    Freeze {
        #[arg(required = true, value_name = "NAME[@VERSION]")]
        packages: Vec<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Lift persistent package version constraints
    Unfreeze {
        #[arg(required = true, value_name = "NAME")]
        packages: Vec<String>,
    },
    /// Install one or more packages, optionally specifying publisher and version
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Remove dependencies no longer needed by explicitly installed packages
    Autoremove,
    /// Change facet rules for installed package actions, offline
    ChangeFacet {
        /// FACET, PREFIX.*, or * for the whole image; PACKAGE:FACET or PREFIX/*:FACET for
        /// packages; `default` removes the rule
        #[arg(required = true, value_name = "[PACKAGE:]FACET=true|false|default")]
        facets: Vec<String>,
        /// Also apply the PACKAGE:FACET rules to each package's installed dependencies
        #[arg(long)]
        with_dependencies: bool,
    },
    /// Change variants other than arch for installed package actions, offline
    ChangeVariant {
        #[arg(value_name = "NAME=VALUE")]
        variants: Vec<String>,
        /// Unset a variant
        #[arg(long, value_name = "NAME")]
        unset: Vec<String>,
    },
    /// Reconcile an image to the requested packages and their dependencies
    ExactInstall {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Install the configured signed release profile into an image with no packages
    InstallProfile,
    /// Install an exact locked composition into a newly initialized image
    InstallLock { lock: PathBuf },
    /// Change local request marks without installing or removing packages
    Mark {
        #[arg(value_enum)]
        choice: MarkChoice,
        #[arg(required = true, value_name = "PACKAGE")]
        packages: Vec<String>,
    },
    /// Remove installed packages without contacting the repository
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Restore damaged paths from the installed package versions
    Repair {
        #[arg(long = "path", value_name = "PATH", conflicts_with = "packages")]
        paths: Vec<String>,
        packages: Vec<String>,
    },
    /// Restore installed vendor defaults for the specified preserved files
    Revert {
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Review a pending configuration update
    ResolveConfig {
        /// Path relative to the image root
        path: String,
        #[arg(long, value_enum)]
        take: ConfigChoice,
    },
    /// Update the installed package set
    Update {
        /// Plan the upgrade even if this image is outside the signed profile rollout
        #[arg(long)]
        force_phased_update: bool,
    },
}

#[derive(Subcommand)]
enum BootCommand {
    /// Retire an inactive Btrfs root and its boot artifacts, or resume its cleanup
    DestroyBtrfs {
        #[arg(long)]
        esp: PathBuf,
        #[arg(long)]
        btrfs_volume: PathBuf,
        #[arg(long)]
        dry_run: bool,
        id: String,
    },
    /// Remove old environments and failed trials under the boot-btrfs retention policy
    Prune {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
        /// Known-good environments to keep, overriding keep-environments
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=99))]
        keep: Option<u64>,
        /// Check and list the removals without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Cancel a pending or running trial and remove failed trials
    DiscardTrial {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
    },
    /// Stage a saved plan in a writable Btrfs snapshot of a read-only baseline
    StageBtrfs {
        #[arg(long)]
        source: PathBuf,
        /// New directory containing the staged root/ subvolume
        #[arg(long)]
        output: PathBuf,
        /// Start the staged root with an empty directory for this nested subvolume
        #[arg(long, value_name = "PATH")]
        exclude_subvolume: Vec<String>,
        plan: PathBuf,
    },
    /// Capture a running CPS Btrfs root as a directory with a read-only root/
    Capture {
        #[arg(long)]
        source: PathBuf,
        /// New path in a separate subvolume on the same Btrfs filesystem
        #[arg(long)]
        output: PathBuf,
        /// Capture an empty directory for this nested subvolume
        #[arg(long, value_name = "PATH")]
        exclude_subvolume: Vec<String>,
    },
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
        /// Mounted Btrfs top-level volume for checking a subvolume root
        #[arg(long, requires = "esp", conflicts_with = "disk")]
        btrfs_volume: Option<PathBuf>,
        /// Compare the configured default with the rollback root without changing boot selection
        #[arg(long, requires = "btrfs_volume")]
        dry_run: bool,
        /// Include managed file actions and ownership in the preview
        #[arg(long, requires = "dry_run")]
        paths: bool,
        #[arg(long, requires = "dry_run")]
        json: bool,
        /// Environment ID. Defaults to the previous known-good environment.
        id: Option<String>,
    },
    /// Publish a boot entry for a root on a mounted Btrfs volume
    ActivateBtrfs {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        btrfs_volume: PathBuf,
        #[arg(long)]
        esp: PathBuf,
        /// Use a direct GPT root partition instead of the mounted Btrfs filesystem UUID
        #[arg(long)]
        root_partuuid: Option<String>,
        #[arg(long, default_value = "boot/vmlinuz")]
        kernel: String,
        #[arg(long)]
        initrd: Option<String>,
        /// Build a snapshot-specific UKI and sign it with the host's sbctl key
        #[arg(long)]
        uki: bool,
        /// Sign a UKI and boot it directly from UEFI, with a one-shot trial
        #[arg(long)]
        uefi: bool,
        #[arg(long, default_value = "")]
        options: String,
        /// Trial attempts (default 2, or 1 with --uefi)
        #[arg(long)]
        tries: Option<u32>,
        #[arg(long)]
        known_good: bool,
    },
    /// Verify and promote the running direct UEFI trial
    BlessUefi {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
    },
    /// Clone the base image and apply a saved plan in a new directory
    Stage {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        root: PathBuf,
        plan: PathBuf,
    },
    /// Stage and export a saved plan as one directory for explicit activation
    Prepare {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_name = "MIB")]
        size_mib: u64,
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

fn print_boot_prune(
    report: &system::BootPruneReport,
    dry_run: bool,
    report_empty: bool,
) -> ExitCode {
    for (id, kind) in &report.removed {
        let action = if dry_run { "Would remove" } else { "Removed" };
        println!("{action} {} {id}", system::removal_label(*kind));
    }
    for (id, error) in &report.failed {
        eprintln!("Could not remove {id}: {error}");
    }
    if report.removed.is_empty() && report.failed.is_empty() && report_empty {
        println!("Nothing to prune");
    }
    if report.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn load_plan(path: &Path) -> Result<Plan> {
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("reading plan {}", path.display()))
}

fn parse_policy(
    facets: Vec<String>,
    with_dependencies: bool,
    variants: Vec<String>,
) -> Result<cps::ImagePolicy> {
    let mut policy = cps::ImagePolicy::default();
    for entry in facets {
        let (name, value) = entry
            .split_once('=')
            .context("expected --facet [PACKAGE:]FACET=BOOL")?;
        let value = value.parse().context("facet value must be true or false")?;
        policy.insert_facet(name, value, with_dependencies && name.contains(':'))?;
    }
    ensure!(
        !with_dependencies || !policy.package_facets.is_empty(),
        "--with-dependencies requires a --facet PACKAGE:FACET=BOOL rule"
    );
    for entry in variants {
        policy.insert_variant(&entry)?;
    }
    Ok(policy)
}

fn parse_variant_changes(
    entries: Vec<String>,
    unset: Vec<String>,
) -> Result<BTreeMap<String, Option<String>>> {
    let mut variants = BTreeMap::new();
    let changes = entries
        .iter()
        .map(|entry| {
            let (name, value) = entry.split_once('=').context("expected NAME=VALUE")?;
            anyhow::Ok((name.to_owned(), Some(value.to_owned())))
        })
        .chain(unset.into_iter().map(|name| Ok((name, None))));
    for change in changes {
        let (name, value) = change?;
        ensure!(
            variants.insert(name.clone(), value).is_none(),
            "duplicate variant {name}"
        );
    }
    ensure!(!variants.is_empty(), "specify at least one variant change");
    Ok(variants)
}

fn parse_facet_changes(entries: Vec<String>) -> Result<BTreeMap<String, Option<bool>>> {
    let mut facets = BTreeMap::new();
    for entry in entries {
        let (name, value) = entry
            .split_once('=')
            .context("expected [PACKAGE:]FACET=true|false|default")?;
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

fn print_packages(rows: impl IntoIterator<Item = PackageRow>, show_summary: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if show_summary {
        writeln!(out, "PACKAGE\tSTATE\tSUMMARY")?;
        for row in rows {
            writeln!(
                out,
                "{}\t{}\t{}",
                row.id,
                row.status,
                row.summary.as_deref().unwrap_or("")
            )?;
        }
    } else {
        writeln!(out, "PACKAGE\tSTATE")?;
        for row in rows {
            writeln!(out, "{}\t{}", row.id, row.status)?;
        }
    }
    Ok(())
}

fn choose_install_source(package: &str, sources: &[cps::query::InstallSource]) -> Result<usize> {
    eprintln!("Choose a publisher for {package}");
    for (index, source) in sources.iter().enumerate() {
        eprintln!("  {}. {} ({})", index + 1, source.publisher, source.version);
    }
    loop {
        eprint!("Publisher [1] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        ensure!(
            io::stdin().read_line(&mut answer)? != 0,
            "publisher selection canceled"
        );
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(0);
        }
        if let Ok(number) = answer.parse::<usize>()
            && (1..=sources.len()).contains(&number)
        {
            return Ok(number - 1);
        }
        eprintln!("Enter a number from 1 to {}.", sources.len());
    }
}

fn print_image_diff(diff: &cps::ImageDiff, json: bool) -> Result<()> {
    if json {
        serde_json::to_writer_pretty(io::stdout().lock(), diff)?;
        println!();
        return Ok(());
    }
    let describe = |entry: &cps::ImagePackage| {
        format!(
            "{}@{} [{}; local={}; profile={}]",
            entry.publisher,
            entry.version,
            entry.manifest_sha256,
            entry.requested,
            entry.profile_role
        )
    };
    println!(
        "{} revision {} → {} revision {}",
        diff.before.id, diff.before.revision, diff.after.id, diff.after.revision
    );
    if diff.before.architecture != diff.after.architecture {
        println!(
            "Architecture {} → {}",
            diff.before.architecture, diff.after.architecture
        );
    }
    if diff.before.profile != diff.after.profile {
        println!(
            "Profile {} → {}",
            diff.before.profile.as_deref().unwrap_or("—"),
            diff.after.profile.as_deref().unwrap_or("—")
        );
    }
    if diff.before.facets != diff.after.facets {
        println!(
            "Facet rules {:?} → {:?}",
            diff.before.facets, diff.after.facets
        );
    }
    if diff.before.package_facets != diff.after.package_facets {
        println!(
            "Per-package facet rules {:?} → {:?}",
            diff.before.package_facets, diff.after.package_facets
        );
    }
    if diff.before.variants != diff.after.variants {
        println!(
            "Variants {:?} → {:?}",
            diff.before.variants, diff.after.variants
        );
    }
    if diff.before.mediators != diff.after.mediators {
        println!(
            "Mediator preferences {:?} → {:?}",
            diff.before.mediators, diff.after.mediators
        );
    }
    if diff.packages.is_empty() {
        println!("No package changes");
    }
    for change in &diff.packages {
        let before = change
            .before
            .as_ref()
            .map(describe)
            .unwrap_or_else(|| "—".into());
        let after = change
            .after
            .as_ref()
            .map(describe)
            .unwrap_or_else(|| "—".into());
        println!("{}: {before} → {after}", change.name);
    }
    if let Some(paths) = &diff.paths {
        for change in paths {
            println!(
                "/{}: {} → {}",
                change.name,
                serde_json::to_string(&change.before)?,
                serde_json::to_string(&change.after)?
            );
        }
    }
    Ok(())
}

fn print_package_changes(changes: &[PackageChange]) {
    for package in changes {
        let name = package.publisher.as_ref().map_or_else(
            || package.name.clone(),
            |publisher| format!("cps://{publisher}/{}", package.name),
        );
        match (&package.from, &package.to) {
            (Some(old), Some(new)) => println!("Update {name} {old} → {new}"),
            (None, Some(new)) => println!("Install {name} {new}"),
            (Some(old), None) => println!("Remove {name} {old}"),
            (None, None) => {}
        }
    }
}

fn change_system(
    options: SystemOptions,
    mut request: Request,
    stage: bool,
    force_phased_update: bool,
    base_only: bool,
    prepare_only: bool,
    git: Option<(&str, GitSnapshotRef<'_>, Option<system::GitSignatures>)>,
) -> Result<()> {
    let recovery = matches!(
        request,
        Request::Repair { .. } | Request::RepairPaths { .. } | Request::Revert { .. }
    );
    let system = System::load(&options.config)?
        .with_install_recommends(!options.no_install_recommends)
        .with_force_phased_update(force_phased_update)
        .with_base_only(base_only);
    let _lock = system.lock()?;
    let refresh = !options.no_refresh;
    let mut git_preview = false;
    let mut approve_git = |change: &system::GitChange| -> Result<bool> {
        let branch = change
            .branch
            .as_ref()
            .map_or(String::new(), |branch| format!(" (tracking {branch})"));
        let revision = change.previous.as_ref().map_or_else(
            || change.commit.clone(),
            |previous| format!("{previous} → {}", change.commit),
        );
        println!(
            "Git {}{branch}\n  {}\n  {revision}",
            change.package, change.url
        );
        if let Some(signature) = &change.signature {
            println!("  {signature}");
        }
        if options.dry_run {
            git_preview = true;
            return Ok(false);
        }
        if options.yes {
            return Ok(true);
        }
        ensure!(
            io::stdin().is_terminal(),
            "use --yes to confirm in a non-interactive session"
        );
        eprint!("Build this Git commit? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    };
    if let Some((url, reference, signatures)) = git {
        let Some(package) =
            system.prepare_git_install(url, reference, signatures, &mut approve_git)?
        else {
            return Ok(());
        };
        request = Request::Install {
            packages: vec![package],
        };
    }
    let git_updates = if refresh && !base_only && matches!(request, Request::Update) {
        system.prepare_git_updates(approve_git)?
    } else {
        Vec::new()
    };
    let result = if !git_updates.is_empty() {
        system.plan_git_upgrade(&git_updates, refresh)
    } else if !options.yes && !options.dry_run && io::stdin().is_terminal() {
        system.plan_with_source_selection(request, refresh, choose_install_source)
    } else {
        system.plan(request, refresh)
    };
    let mut change = match result {
        Ok(change) => change,
        Err(error) => {
            if let Some(deferred) = error.downcast_ref::<PhasedUpdateDeferred>() {
                println!("{deferred}");
                return Ok(());
            }
            return Err(error);
        }
    };
    if stage || prepare_only {
        change = change.with_staging();
    }
    if let Some(path) = &options.save_plan {
        save(path, change.saved_plan())?;
    }
    if !change.saved_plan().deferred_publishers().is_empty() {
        eprintln!(
            "Deferred publishers: {}",
            change
                .saved_plan()
                .deferred_publishers()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if change.is_empty() {
        if !git_preview {
            println!("Nothing to do.");
        }
        return Ok(());
    }
    if recovery {
        println!("{}", change.summary());
    } else {
        print_package_changes(change.packages());
    }
    for effect in change.service_effects() {
        println!("{effect}");
    }
    let prepare_recovery = recovery && change.boot_reason().is_some();
    if prepare_recovery {
        println!("CPS will prepare a verified recovery root for separate activation.");
    } else if let Some(reason) = change.boot_reason() {
        println!("A reboot will be required because {reason}.");
    } else {
        println!("Changes will take effect on the running system.");
    }
    if options.dry_run {
        return Ok(());
    }
    if !options.yes {
        ensure!(
            io::stdin().is_terminal(),
            "use --yes to confirm in a non-interactive session"
        );
        eprint!("Proceed? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Canceled.");
            return Ok(());
        }
    }
    if prepare_recovery {
        let root = system.prepare_recovery(&change)?;
        println!("Prepared recovery root {}", root.display());
    } else {
        match system.apply(&change) {
            Ok(Some(id)) => println!("Prepared {id}. Reboot when ready to use the new system."),
            Ok(None) => println!("Changes are available now."),
            Err(error) if error.downcast_ref::<system::RetryBaseOnly>().is_some() => {
                eprintln!("Warning: {error:#}. Replanning with distribution publishers only.");
                drop(_lock);
                return change_system(
                    options,
                    Request::Update,
                    stage,
                    force_phased_update,
                    true,
                    false,
                    None,
                );
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn check_system_updates(
    config: &Path,
    refresh: bool,
    json: bool,
    base_only: bool,
    explain: Option<&str>,
) -> Result<ExitCode> {
    let report = System::check_updates_with_scope(config, refresh, explain, base_only);
    if json {
        let mut out = io::stdout().lock();
        serde_json::to_writer_pretty(&mut out, &report)?;
        writeln!(out)?;
    } else {
        if report.status == UpdateStatus::UpToDate {
            println!("Up to date.");
        }
        if !report.deferred_publishers.is_empty() {
            println!(
                "Deferred publishers: {}",
                report
                    .deferred_publishers
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for freshness in &report.publisher_freshness {
            println!("{freshness}");
        }
        if let Some(deferred) = &report.phased_update {
            println!("{deferred}");
        }
        for advisory in &report.advisories {
            println!(
                "{} affects cps://{}/{} {}. Fixed in {}. {}",
                advisory.id,
                advisory.publisher,
                advisory.package,
                advisory.affected,
                advisory.fixed,
                advisory.summary
            );
            if let Some(action) = &advisory.action {
                println!("Operator action for {}. {action}", advisory.id);
            }
        }
        print_package_changes(&report.changes);
        for update in &report.held_back {
            println!(
                "Held back cps://{}/{} {} → {} ({})",
                update.publisher, update.name, update.installed, update.available, update.reason
            );
        }
        for blocker in &report.blockers {
            eprintln!("cps: {blocker}");
        }
        if let Some(reason) = &report.reboot_reason {
            println!("A reboot will be required because {reason}.");
        } else if report.status == UpdateStatus::UpdatesAvailable {
            println!("Changes can be applied to the running system.");
        }
    }
    Ok(if report.status == UpdateStatus::Blocked {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Install {
            options,
            stage,
            git,
            commit,
            track_branch,
            allowed_signers,
            require_signed,
            packages,
        } => {
            let signatures = allowed_signers
                .map(|signers| {
                    system::GitSignatures::new(
                        &signers,
                        match require_signed {
                            GitSignedArg::Commit => system::GitSignedObject::Commit,
                            GitSignedArg::Tag => system::GitSignedObject::Tag,
                        },
                    )
                })
                .transpose()?;
            let source = match (git.as_deref(), commit.as_deref(), track_branch.as_deref()) {
                (Some(url), Some(commit), None) => {
                    Some((url, GitSnapshotRef::Commit(commit), signatures))
                }
                (Some(url), None, Some(branch)) => {
                    Some((url, GitSnapshotRef::Branch(branch), signatures))
                }
                (None, None, None) => None,
                _ => unreachable!("clap validates Git install arguments"),
            };
            change_system(
                options,
                Request::Install { packages },
                stage,
                false,
                false,
                false,
                source,
            )?;
        }
        Command::Upgrade {
            options,
            stage,
            force_phased_update,
            base_only,
        } => change_system(
            options,
            Request::Update,
            stage,
            force_phased_update,
            base_only,
            false,
            None,
        )?,
        Command::CheckUpdates {
            config,
            json,
            no_refresh,
            base_only,
            explain,
        } => {
            return check_system_updates(&config, !no_refresh, json, base_only, explain.as_deref());
        }
        Command::Remove {
            options,
            stage,
            packages,
        } => change_system(
            options,
            Request::Remove { packages },
            stage,
            false,
            false,
            false,
            None,
        )?,
        Command::Autoremove { options, stage } => change_system(
            options,
            Request::Autoremove,
            stage,
            false,
            false,
            false,
            None,
        )?,
        Command::Repair {
            options,
            prepare_only,
            paths,
            packages,
        } => {
            let request = if paths.is_empty() {
                Request::Repair { packages }
            } else {
                Request::RepairPaths { paths }
            };
            change_system(options, request, false, false, false, prepare_only, None)?;
        }
        Command::Revert {
            options,
            prepare_only,
            paths,
        } => {
            change_system(
                options,
                Request::Revert { paths },
                false,
                false,
                false,
                prepare_only,
                None,
            )?;
        }
        Command::Status {
            config,
            plan,
            validate,
        } => {
            let system = System::load(&config)?;
            if let Some(path) = plan {
                let progress = system.transaction_progress(&load_plan(&path)?, validate)?;
                println!("{}", serde_json::to_string_pretty(&progress)?);
                return Ok(ExitCode::SUCCESS);
            }
            let (running, entries) = system.boot_status()?;
            let status = transaction_status(Path::new("/"))?;
            println!("Running {running}, package revision {}", status.revision);
            if let Some(pending) = status.pending {
                println!(
                    "Transaction {} is incomplete. Run cps resume.",
                    pending.plan_id
                );
            }
            if let Some(next) = entries
                .iter()
                .find(|entry| entry.is_default && entry.id != running)
            {
                println!("Reboot pending into {}.", next.id);
            } else if entries
                .iter()
                .any(|entry| entry.id == running && entry.tries_left.is_some())
            {
                println!("This system is running as a trial.");
            } else {
                println!("No reboot pending.");
            }
            match system.publisher_freshness() {
                Ok(report) => report.iter().for_each(|freshness| println!("{freshness}")),
                Err(error) => eprintln!("cps: cannot report publisher freshness: {error:#}"),
            }
        }
        Command::Resume { config, repair } => {
            let system = System::load(&config)?;
            let _lock = system.lock()?;
            let revision = if repair {
                system.resume_with_repair()?
            } else {
                system.resume()?
            };
            if let Some(revision) = revision {
                println!("Recovered the live transaction at package revision {revision}.");
            } else {
                println!("No pending transaction.");
            }
        }
        Command::Source { command } => match command {
            SourceCommand::GitSnapshot {
                git_repo,
                branch,
                tag,
                commit,
                repo,
                trust_root,
                output,
                recipe,
            } => {
                let source = match (branch.as_deref(), tag.as_deref(), commit.as_deref()) {
                    (Some(branch), None, None) => GitSnapshotRef::Branch(branch),
                    (None, Some(tag), None) => GitSnapshotRef::Tag(tag),
                    (None, None, Some(commit)) => GitSnapshotRef::Commit(commit),
                    _ => unreachable!("clap requires exactly one Git selector"),
                };
                let (package, commit) = prepare_git_snapshot_ref(
                    &recipe,
                    &git_repo,
                    source,
                    &repo,
                    &trust_root,
                    &output,
                )?;
                println!("Prepared {package} from {}", source.provenance(&commit));
            }
            SourceCommand::Fetch {
                cache,
                from,
                recipe,
            } => {
                let count = match from {
                    Some(from) => fetch_sources_with_local(&recipe, &cache, &from)?,
                    None => fetch_sources(&recipe, &cache)?,
                };
                println!("Verified {count} pinned source(s) in the cache");
            }
            SourceCommand::Import {
                cache,
                from,
                recipe,
            } => {
                let count = import_sources(&recipe, &cache, &from)?;
                println!("Verified {count} pinned source(s) in the cache");
            }
        },
        Command::Build {
            inputs,
            output,
            check_reproducible,
            builder_signing_key,
            recipe,
        } => {
            let inputs = inputs.prepare()?;
            let id = build_with_recipe_options(
                &recipe,
                inputs.toolchain,
                BuildSettings {
                    sources: inputs.sources.0,
                    output: &output,
                    check_reproducible,
                    builder_signing_key: builder_signing_key.as_deref(),
                },
                inputs.sources.1,
                &inputs.worker,
                &inputs.overrides,
            )?;
            println!("Built {id} at {}", output.display());
        }
        Command::Develop {
            inputs,
            workspace,
            recipe,
            command,
        } => {
            let inputs = inputs.prepare()?;
            let status = cps::develop(
                &recipe,
                inputs.toolchain,
                inputs.sources,
                workspace.as_deref(),
                &command,
                &inputs.worker,
                &inputs.overrides,
            )?;
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
            return Ok(ExitCode::from(code as u8));
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
            ManifestCommand::Dependencies {
                proto,
                config,
                catalogs,
                library_dirs,
                path_dirs,
                check,
                manifest,
            } => {
                let inspect = if check {
                    cps::check_manifest_dependencies
                } else {
                    suggest_dependencies
                };
                let (suggestions, diagnostics) = inspect(
                    &proto,
                    &manifest,
                    &config,
                    &catalogs,
                    &library_dirs,
                    &path_dirs,
                )?;
                std::io::stdout().lock().write_all(suggestions.as_bytes())?;
                if !diagnostics.is_empty() {
                    for diagnostic in diagnostics {
                        eprintln!("cps: {diagnostic}");
                    }
                    return Ok(ExitCode::FAILURE);
                }
            }
        },
        Command::Publish {
            repo,
            proto,
            signing_key,
            manifests,
        } => {
            for id in cps::publish_batch(&repo, &proto, &manifests, &signing_key)? {
                println!("Published {id}");
            }
        }
        Command::PublishBundle {
            repo,
            signing_key,
            builder_key,
            recipe_sha256,
            witness,
            source_cache,
            bundle,
        } => {
            println!(
                "Published {}",
                publish_witnessed_bundle_with_sources(
                    &repo,
                    &bundle,
                    &signing_key,
                    &builder_key,
                    &recipe_sha256,
                    &witness,
                    source_cache.as_deref(),
                )?
            );
        }
        Command::Repo { command } => match command {
            RepoCommand::WidenRollout {
                repo,
                publisher,
                signing_key,
                base,
                profile,
                percentage,
            } => {
                let hash = cps::widen_rollout(
                    &repo,
                    &publisher,
                    &signing_key,
                    &base,
                    &profile,
                    percentage,
                )?;
                println!("Published catalog {hash}");
            }
            RepoCommand::PruneWithdrawals {
                repo,
                publisher,
                signing_key,
                base,
                through,
            } => {
                let (hash, pruned) =
                    cps::prune_withdrawals(&repo, &publisher, &signing_key, &base, through)?;
                println!("Pruned {pruned} withdrawals; catalog {hash}");
            }
            RepoCommand::IndexFiles {
                repo,
                publisher,
                signing_key,
            } => {
                println!(
                    "Catalog {}",
                    cps::enable_file_index(&repo, &publisher, &signing_key)?
                );
            }
            RepoCommand::Trust { command } => run_trust(command)?,
            RepoCommand::Resign {
                repo,
                publisher,
                signing_key,
                base,
            } => {
                println!(
                    "Published catalog {}",
                    cps::resign_catalog(&repo, &publisher, &signing_key, &base)?
                );
            }
            RepoCommand::Obsoletions {
                repo,
                publisher,
                trust_root,
            } => {
                for id in list_obsoletions(&repo, &publisher, &trust_root)? {
                    println!("{id}");
                }
            }
            RepoCommand::PublishRelease {
                repo,
                signing_key,
                source_cache,
                prepare,
                expect_catalog,
                release,
            } => {
                let mode = if prepare {
                    cps::ReleaseMode::Prepare
                } else if let Some(hash) = &expect_catalog {
                    cps::ReleaseMode::RequireCandidate(hash)
                } else {
                    cps::ReleaseMode::Publish
                };
                let hash = cps::publish_release_with_mode(
                    &repo,
                    &signing_key,
                    &release,
                    source_cache.as_deref(),
                    mode,
                )?;
                println!(
                    "{} catalog {hash}",
                    if prepare { "Prepared" } else { "Published" }
                );
            }
            RepoCommand::Attestation {
                repo,
                publisher,
                trust_root,
                package_id,
            } => {
                let bytes = read_attestation(&repo, &publisher, &trust_root, &package_id)?;
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(&bytes)?;
                writeln!(stdout)?;
            }
            RepoCommand::RebuildInputs {
                repo,
                publisher,
                trust_root,
                catalog,
                output,
                package_id,
            } => {
                let recipe = export_rebuild_inputs(
                    &repo,
                    &publisher,
                    &trust_root,
                    &package_id,
                    catalog.as_deref(),
                    &output,
                )?;
                println!("{}", recipe.display());
            }
            RepoCommand::VerifyRebuild {
                repo,
                publisher,
                trust_root,
                catalog,
                bundle,
                witness_key,
                package_id,
            } => {
                let count = verify_rebuild(
                    &repo,
                    &publisher,
                    &trust_root,
                    &package_id,
                    catalog.as_deref(),
                    &bundle,
                    witness_key.as_deref(),
                )?;
                println!("Verified {count} rebuilt payload(s) for {package_id}");
                if witness_key.is_some() {
                    println!("Verified a matching statement from a separate rebuild signer");
                }
            }
            RepoCommand::ArchiveSources {
                repo,
                signing_key,
                source_cache,
                package_id,
            } => {
                let (catalog, count) =
                    archive_build_sources(&repo, &signing_key, &source_cache, &package_id)?;
                println!("Archived {count} source object(s) for {package_id} in catalog {catalog}");
            }
            RepoCommand::FetchSources {
                repo,
                publisher,
                trust_root,
                source_cache,
                catalog,
                package_id,
            } => {
                let count = fetch_build_sources(
                    &repo,
                    &publisher,
                    &trust_root,
                    &package_id,
                    catalog.as_deref(),
                    &source_cache,
                )?;
                println!("Verified {count} archived source object(s) for {package_id}");
            }
            RepoCommand::FetchToolchain {
                toolchain_config,
                recipe,
            } => {
                let count = fetch_toolchain(&recipe, &toolchain_config)?;
                println!("Verified {count} toolchain payload(s)");
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
                trust_root,
            } => {
                for (id, hash) in list_withdrawals(&repo, &publisher, &trust_root)? {
                    println!("{id}\t{hash}");
                }
            }
            RepoCommand::Export {
                repo,
                publisher,
                trust_root,
                output,
                retained,
                delta_from,
                include_sources,
                archive_catalog,
            } => {
                let hash = if let Some(catalog) = archive_catalog {
                    release::export_archive(
                        &repo,
                        &publisher,
                        &trust_root,
                        &output,
                        &catalog,
                        include_sources,
                    )?
                } else if !delta_from.is_empty() {
                    release::export_with_deltas(
                        &repo,
                        &publisher,
                        &trust_root,
                        &output,
                        &retained,
                        include_sources,
                        &delta_from,
                    )?
                } else if include_sources {
                    release::export_with_sources(
                        &repo,
                        &publisher,
                        &trust_root,
                        &output,
                        &retained,
                    )?
                } else {
                    release::export_with_retained(
                        &repo,
                        &publisher,
                        &trust_root,
                        &output,
                        &retained,
                    )?
                };
                println!("Exported {hash}");
            }
            RepoCommand::SignExport {
                root,
                targets_key,
                snapshot_key,
                timestamp_key,
                objects_key,
                objects_threshold,
                version,
                release_expires,
                timestamp_expires,
                output,
                export,
            } => release::sign_export(release::SignExport {
                root: &root,
                export: &export,
                output: &output,
                keys: release::ReleaseKeys {
                    targets: &targets_key,
                    snapshot: &snapshot_key,
                    timestamp: &timestamp_key,
                    objects: &objects_key,
                    objects_threshold,
                },
                version,
                release_expires: &release_expires,
                timestamp_expires: &timestamp_expires,
            })?,
            RepoCommand::RefreshMetadata {
                root,
                snapshot,
                timestamp_key,
                snapshot_key,
                snapshot_expires,
                version,
                timestamp_expires,
                output,
            } => release::refresh_metadata(release::RefreshMetadata {
                root: &root,
                snapshot: &snapshot,
                output: &output,
                timestamp_keys: &timestamp_key,
                snapshot_keys: &snapshot_key,
                version,
                snapshot_expires: snapshot_expires.as_deref(),
                timestamp_expires: &timestamp_expires,
            })?,
            RepoCommand::Sync {
                repo,
                root,
                metadata,
                targets,
                publisher,
                trust_root,
                index_only,
                image,
                required_catalogs,
                archive_catalog,
            } => {
                let hash = if let Some(catalog) = archive_catalog {
                    release::sync_archive(
                        &repo,
                        &root,
                        &metadata,
                        &targets,
                        &publisher,
                        &trust_root,
                        release::ArchiveSyncOptions {
                            catalog: &catalog,
                            index_only,
                        },
                    )?
                } else {
                    release::sync_with_required_catalogs(
                        &repo,
                        &root,
                        &metadata,
                        &targets,
                        &publisher,
                        &trust_root,
                        release::SyncOptions {
                            index_only,
                            image: image.as_deref(),
                        },
                        &required_catalogs,
                    )?
                };
                println!("Synchronized {hash}");
            }
            RepoCommand::SyncProfile {
                config,
                index_only,
                profile_payloads,
                image,
            } => {
                let payloads = if profile_payloads {
                    release::ProfilePayloads::Composition
                } else if index_only {
                    release::ProfilePayloads::IndexOnly
                } else {
                    release::ProfilePayloads::All
                };
                let synchronized =
                    release::sync_profile_with_payloads(&config, payloads, image.as_deref())?;
                println!("Synchronized {}", synchronized);
            }
            RepoCommand::PruneCache {
                repo,
                publisher,
                trust_root,
                catalogs,
                images,
                plans,
                locks,
                system_config,
                apply,
            } => {
                let plans = plans
                    .iter()
                    .map(|path| load_plan(path))
                    .collect::<Result<Vec<_>>>()?;
                let locks = locks
                    .iter()
                    .map(|path| cps::read_image_lock(path))
                    .collect::<Result<Vec<_>>>()?;
                let keep = release::CacheRetention {
                    catalogs: &catalogs,
                    images: &images,
                    plans: &plans,
                };
                let report = if let Some(config) = system_config {
                    System::load(&config)?.prune_cache_with_locks(
                        &repo,
                        &publisher,
                        &trust_root,
                        keep,
                        &locks,
                        apply,
                    )?
                } else {
                    release::prune_payloads_with_locks(
                        &repo,
                        &publisher,
                        &trust_root,
                        keep,
                        &locks,
                        apply,
                    )?
                };
                for root in &report.registered_roots {
                    print!(
                        "Keep {} package payload(s) for {}",
                        root.package_payloads, root.id
                    );
                    if let Some(catalog) = &root.catalog {
                        print!(" and catalog {catalog}");
                    }
                    println!();
                }
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
        Command::ImageDiff {
            before,
            after,
            paths,
            json,
        } => {
            print_image_diff(&cps::image_diff(&before, &after, paths)?, json)?;
        }
        Command::ImageLock { root, export } => {
            if let Some(output) = export {
                cps::export_image_bundle(&root, &output)?;
                println!("Exported image bundle to {}", output.display());
            } else {
                serde_json::to_writer_pretty(
                    io::stdout().lock(),
                    &cps::capture_image_lock(&root)?,
                )?;
                println!();
            }
        }
        Command::ImageCreate {
            root,
            config,
            lock,
            bundle,
            bootstrap_catalog,
            primary_repo,
            repo,
            publisher,
            trust_root,
            arch,
            facet,
            with_dependencies,
            variant,
        } => {
            let id = match (config, lock, bundle) {
                (Some(config), None, Some(bundle)) => {
                    cps::create_image_from_bundle(&root, &config, &bundle)?
                }
                (Some(config), Some(lock), None) => {
                    cps::create_image_from_lock(&root, &config, &cps::read_image_lock(&lock)?)?
                }
                (Some(path), None, None) => cps::create_configured_image_at(
                    &root,
                    &path,
                    bootstrap_catalog.as_deref(),
                    primary_repo.as_deref(),
                )?,
                (None, None, None) => create_image(
                    &root,
                    &repo.context("missing repository")?,
                    &publisher.context("missing publisher")?,
                    &trust_root.context("missing trust root")?,
                    arch.as_deref().unwrap_or(std::env::consts::ARCH),
                    parse_policy(facet, with_dependencies, variant)?,
                )?,
                _ => anyhow::bail!("select either --lock or --bundle with --config"),
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
                url,
                tuf_root,
                cache,
                trust_root,
                build_trust,
                expect_root_key,
                name,
            } => {
                let credentials;
                let (name, url, tuf_root, trust_root) = if repo.is_none() && url.is_none() {
                    ensure!(
                        trust_root.is_none(),
                        "use --repo or --url with --trust-root"
                    );
                    ensure!(
                        name.contains("://"),
                        "provide an HTTPS mirror URL, or a publisher name with --repo or --url"
                    );
                    ensure!(
                        !expect_root_key.is_empty() || io::stdin().is_terminal(),
                        "publisher confirmation requires a terminal; use --expect-root-key, or --url with explicit trust files, for non-interactive setup"
                    );
                    let trusted_root = tuf_root
                        .as_deref()
                        .unwrap_or(Path::new("/etc/cps/tuf-root.json"));
                    let bootstrap = release::PublisherBootstrap::fetch(&name, trusted_root)?;
                    eprintln!("Publisher trust root verified by TUF.");
                    eprintln!("Mirror: {}", bootstrap.url);
                    eprintln!("TUF root SHA-256: {}", bootstrap.tuf_root_fingerprint());
                    eprint!("{}", bootstrap.describe()?);
                    if expect_root_key.is_empty() {
                        eprint!("Trust this publisher? [y/N] ");
                        io::stderr().flush()?;
                        let mut answer = String::new();
                        io::stdin().read_line(&mut answer)?;
                        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                            println!("Canceled.");
                            return Ok(ExitCode::SUCCESS);
                        }
                    } else {
                        bootstrap.check_root_keys(&expect_root_key)?;
                    }

                    credentials = tempfile::tempdir()?;
                    let root_path = credentials.path().join("root.json");
                    let key_path = credentials.path().join("trust-root.json");
                    fs::write(&root_path, &bootstrap.tuf_root)?;
                    fs::write(&key_path, &bootstrap.trust_root)?;
                    (
                        bootstrap.name,
                        Some(bootstrap.url.into()),
                        Some(root_path),
                        Some(key_path),
                    )
                } else {
                    (name, url, tuf_root, trust_root)
                };
                let trust_root = trust_root.context("missing trust root")?;
                if let Some(url) = url {
                    let (cache, _lock) = if let Some(cache) = cache {
                        (cache, None)
                    } else {
                        ensure!(
                            fs::canonicalize(&root)? == Path::new("/"),
                            "use --cache with an offline image"
                        );
                        let system = System::load(Path::new(system::CONFIG))?;
                        let lock = system.lock()?;
                        (system.publisher_cache(&name)?, Some(lock))
                    };
                    let cache = add_publisher_from_url(
                        &root,
                        &name,
                        &url,
                        &tuf_root.context("missing trusted TUF root")?,
                        &trust_root,
                        &cache,
                        build_trust.as_deref(),
                    )?;
                    println!("Added {name} from {url} (cache {})", cache.display());
                } else {
                    add_publisher_with_build_trust(
                        &root,
                        &name,
                        &repo.context("missing local repository")?,
                        &trust_root,
                        build_trust.as_deref(),
                    )?;
                }
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
            intent,
        } => {
            if intent {
                let mut out = std::io::stdout().lock();
                writeln!(out, "PACKAGE\tLOCAL REQUEST\tPROFILE")?;
                for row in list_package_intent(&root)? {
                    let request = if row.requested { "yes" } else { "-" };
                    writeln!(out, "{}\t{request}\t{}", row.id, row.profile)?;
                }
            } else {
                let scope = match (available, all_versions) {
                    (true, true) => Scope::AllAvailableVersions,
                    (true, false) => Scope::Available,
                    _ => Scope::Installed,
                };
                print_packages(list_packages(&root, scope)?, false)?;
            }
        }
        Command::Search {
            root,
            file: true,
            term,
            ..
        } => {
            let matches = search_files(&root, &term)?;
            ensure!(!matches.is_empty(), "no available package ships {term:?}");
            let mut out = std::io::stdout().lock();
            for matched in matches {
                writeln!(out, "{}\t{}", matched.package, matched.path)?;
            }
        }
        Command::Search {
            root,
            installed,
            file: false,
            term,
        } => {
            let term = term.trim().to_lowercase();
            ensure!(
                !term.is_empty(),
                "search requires a package name or summary fragment"
            );
            let scope = if installed {
                Scope::Installed
            } else {
                Scope::Available
            };
            let packages = list_packages(&root, scope)?;
            print_packages(
                packages.into_iter().filter(|row| {
                    row.name.contains(&term)
                        || row
                            .summary
                            .as_ref()
                            .is_some_and(|summary| summary.to_lowercase().contains(&term))
                }),
                true,
            )?;
        }
        Command::Policy {
            root,
            json,
            probe,
            package,
        } => {
            let policy = cps::query::package_policy(&root, &package, probe)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&policy)?);
            } else {
                println!("Package {}", policy.package);
                if let Some(installed) = &policy.installed {
                    println!("Installed {installed}");
                }
                if let Some(publisher) = &policy.preferred_publisher {
                    println!("Preferred publisher {publisher}");
                }
                if let Some(freeze) = &policy.freeze {
                    println!(
                        "Frozen at {} {}",
                        freeze.version,
                        freeze.reason.as_deref().unwrap_or("")
                    );
                }
                if let Some(profile) = &policy.profile {
                    println!("Release profile {profile}");
                    if let Some(manifest) = &policy.applied_manifest {
                        println!("Applied profile manifest {manifest}");
                    }
                    if let Some(manifest) = &policy.target_manifest {
                        println!("Target profile manifest {manifest}");
                    }
                }
                for publisher in policy.publishers {
                    println!("Publisher {} catalog {}", publisher.name, publisher.catalog);
                    if publisher.requires_build_trust {
                        println!("  Requires build trust");
                    }
                    if let Some(cutoff) = publisher.obsoleted_through {
                        println!("  Obsoleted through {cutoff}");
                    }
                    for candidate in publisher.candidates {
                        println!("  {} {}", candidate.id, candidate.manifest);
                    }
                }
                if let Some(probe) = policy.install_probe {
                    match probe {
                        cps::query::InstallProbe::Solved { plan } => {
                            println!("Install probe solved\n{plan}")
                        }
                        cps::query::InstallProbe::Blocked { reason } => {
                            println!("Install probe blocked\n{reason}")
                        }
                    }
                }
            }
        }
        Command::Info {
            root,
            available,
            package,
        } => {
            let manifest = package_manifest(&root, &package, available)?;
            std::io::stdout().lock().write_all(manifest.as_bytes())?;
        }
        Command::Owner { root, path } => {
            let mut out = std::io::stdout().lock();
            for owner in path_owners(&root, &path)? {
                writeln!(out, "{owner}")?;
            }
        }
        Command::Contents { root, package } => {
            let mut out = std::io::stdout().lock();
            for path in package_contents(&root, &package)? {
                writeln!(out, "{path}")?;
            }
        }
        Command::Deps { root, package } => {
            let mut out = std::io::stdout().lock();
            for dependency in direct_dependencies(&root, &package)? {
                writeln!(out, "{dependency}")?;
            }
        }
        Command::Rdeps { root, package } => {
            let mut out = std::io::stdout().lock();
            for dependent in reverse_dependencies(&root, &package)? {
                writeln!(out, "{dependent}")?;
            }
        }
        Command::Why { root, package } => {
            let explanation = explain_package(&root, &package)?;
            let intent = explanation.intent;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{}", intent.id)?;
            let request = if intent.requested { "yes" } else { "no" };
            writeln!(out, "  Explicit local request\t{request}")?;
            match explanation.profile_name {
                Some(name) => writeln!(out, "  Signed profile\t{name} ({})", intent.profile)?,
                None => writeln!(out, "  Signed profile\t(none)")?,
            }
            writeln!(out, "  Direct dependents")?;
            if explanation.dependents.is_empty() {
                writeln!(out, "    (none)")?;
            }
            for dependent in explanation.dependents {
                writeln!(out, "    {dependent}")?;
            }
            if let Some(path) = explanation.path {
                let origin = match (path.root.requested, path.root.profile) {
                    (true, cps::query::ProfileRole::Root) => {
                        "local request and signed profile root"
                    }
                    (true, _) => "local request",
                    (false, _) => "signed profile root",
                };
                writeln!(out, "  Dependency path from {origin}")?;
                writeln!(out, "    {}", path.root.id)?;
                for step in path.steps {
                    write!(out, "    → {} ({})", step.provider, step.requirement)?;
                    if step.matching_providers > 1 {
                        write!(out, " [{} matching providers]", step.matching_providers)?;
                    }
                    writeln!(out)?;
                }
            }
            if !explanation.recommenders.is_empty() {
                writeln!(out, "  Recommended by")?;
                for recommender in explanation.recommenders {
                    writeln!(out, "    {recommender}")?;
                }
            }
        }
        Command::Config {
            command: ConfigCommand::Diff { root, path },
        } => {
            let dir = tempfile::tempdir()?;
            let local = dir.path().join("local");
            let vendor = dir.path().join("vendor");
            cps::config_diff_inputs(
                &root,
                &path,
                &mut File::create_new(&local)?,
                &mut File::create_new(&vendor)?,
            )?;
            let status = std::process::Command::new("/usr/bin/diff")
                .args(["-u", "--label", "local", "--label", "vendor", "--"])
                .arg(local)
                .arg(vendor)
                .status()
                .context("running /usr/bin/diff")?;
            return match status.code() {
                Some(0) => Ok(ExitCode::SUCCESS),
                Some(1) => Ok(ExitCode::FAILURE),
                _ => anyhow::bail!("configuration diff failed with {status}"),
            };
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
        Command::Facet {
            root,
            package,
            facets,
            json,
        } => {
            if package.is_none() && facets.is_empty() {
                let rows = list_facets(&root)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    println!("RULE\tVALUE\tSOURCE");
                    for row in rows {
                        let scope = if row.with_dependencies {
                            " with-dependencies"
                        } else {
                            ""
                        };
                        println!("{}\t{}\t{}{scope}", row.rule, row.value, row.source);
                    }
                }
            } else {
                let rows = cps::query::facet_values(&root, package.as_deref(), &facets)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    println!("FACET\tVALUE\tRULE");
                    for row in rows {
                        println!("{}\t{}\t{}", row.facet, row.value, row.source);
                    }
                }
            }
        }
        Command::Variant { root, json } => {
            let variants = cps::query::list_variants(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&variants)?);
            } else {
                println!("VARIANT\tVALUE");
                for (name, value) in variants {
                    println!("{name}\t{value}");
                }
            }
        }
        Command::Mediator { root, json } => {
            let rows = cps::query::list_mediators(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("MEDIATOR\tPREFERRED\tSELECTED\tPROVIDERS");
                for row in rows {
                    println!(
                        "{}\t{}\t{}\t{}",
                        row.group,
                        row.preferred.as_deref().unwrap_or("-"),
                        row.selected.as_deref().unwrap_or("-"),
                        row.providers
                            .keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
        Command::Freeze { root } => {
            println!("PACKAGE\tVERSION\tREASON");
            for (name, freeze) in list_freezes(&root)? {
                println!(
                    "{name}\t{}\t{:?}",
                    freeze.version,
                    freeze.reason.as_deref().unwrap_or("")
                );
            }
        }
        Command::Plan {
            root,
            output,
            no_install_recommends,
            request,
        } => {
            let force_phased_update = matches!(
                request,
                PlanCommand::Update {
                    force_phased_update: true
                }
            );
            let request = match request {
                PlanCommand::Freeze { packages, reason } => Request::Freeze { packages, reason },
                PlanCommand::Unfreeze { packages } => Request::Unfreeze { packages },
                PlanCommand::Install { packages } => Request::Install { packages },
                PlanCommand::ExactInstall { packages } => Request::ExactInstall { packages },
                PlanCommand::InstallProfile => Request::InstallProfile,
                PlanCommand::InstallLock { lock } => Request::InstallLock {
                    lock: Box::new(cps::read_image_lock(&lock)?),
                },
                PlanCommand::Mark { choice, packages } => Request::Mark { choice, packages },
                PlanCommand::Remove { packages } => Request::Remove { packages },
                PlanCommand::Autoremove => Request::Autoremove,
                PlanCommand::ChangeFacet {
                    facets,
                    with_dependencies,
                } => Request::ChangeFacet {
                    facets: parse_facet_changes(facets)?,
                    with_dependencies,
                },
                PlanCommand::ChangeVariant { variants, unset } => Request::ChangeVariant {
                    variants: parse_variant_changes(variants, unset)?,
                },
                PlanCommand::SetMediator { group, provider } => {
                    Request::SetMediator { group, provider }
                }
                PlanCommand::Repair { packages, paths } if paths.is_empty() => {
                    Request::Repair { packages }
                }
                PlanCommand::Repair { paths, .. } => Request::RepairPaths { paths },
                PlanCommand::Revert { paths } => Request::Revert { paths },
                PlanCommand::ResolveConfig { path, take } => Request::ResolveConfig { path, take },
                PlanCommand::Update { .. } => Request::Update,
            };
            let plan = plan_with_options(
                &root,
                request,
                PlanOptions {
                    install_recommends: !no_install_recommends,
                    force_phased_update,
                },
            )?;
            save(&output, &plan).with_context(|| format!("saving {}", output.display()))?;
            println!("{}", plan.summary());
        }
        Command::Apply { root, plan } => {
            let saved = load_plan(&plan)?;
            println!("Image revision {}", apply(&root, &saved)?);
        }
        Command::Transaction { command } => match command {
            TransactionCommand::Progress {
                root,
                validate,
                plan,
            } => {
                let progress = cps::transaction_progress(&root, &load_plan(&plan)?, validate)?;
                println!("{}", serde_json::to_string_pretty(&progress)?);
            }
            TransactionCommand::Status { root } => {
                let status = transaction_status(&root)?;
                if let Some(pending) = status.pending {
                    println!("{}", pending.plan_summary);
                    println!(
                        "Pending plan {} targets revision {}",
                        pending.plan_id, pending.target_revision
                    );
                    println!(
                        "{}",
                        if pending.files_ready && pending.live {
                            "Live actions pending"
                        } else if pending.files_ready {
                            "Offline actions pending"
                        } else {
                            "File writes pending"
                        }
                    );
                    if pending.live {
                        println!("Recover with cps resume");
                    } else {
                        println!("Recover with transaction resume");
                    }
                } else {
                    println!("Image revision {}", status.revision);
                    println!("No pending transaction");
                }
            }
            TransactionCommand::Resume { root, repair } => {
                let revision = if repair {
                    repair_pending_transaction(&root)?
                } else {
                    resume_transaction(&root)?
                };
                if let Some(revision) = revision {
                    println!("Image revision {revision}");
                } else {
                    println!("No pending transaction");
                }
            }
            TransactionCommand::History { root, limit } => {
                let history = transaction_history(&root, limit)?;
                println!("REVISION\tPLAN\tOPERATION");
                for entry in history {
                    let operation = entry.details.request.operation();
                    println!("{}\t{}\t{operation}", entry.revision, entry.plan_id);
                }
            }
            TransactionCommand::Show { root, plan } => {
                let entry = transaction_details(&root, &plan)?;
                println!("{}", serde_json::to_string_pretty(&entry)?);
            }
        },
        Command::Fetch {
            root,
            plan,
            archive_caches,
        } => {
            let saved = load_plan(&plan)?;
            let count = fetch_plan_with_archives(&root, &saved, &archive_caches)?;
            println!("Verified {count} plan payloads");
        }
        Command::Validate { root, plan } => {
            cps::validate_plan(&root, &load_plan(&plan)?)?;
            println!("Plan validated against cached payloads");
        }
        Command::Verify {
            root,
            pristine,
            packages,
            paths,
            runtime,
            library_dirs,
        } => {
            let findings = if pristine {
                verify_pristine(&root)?
            } else if !packages.is_empty() || !paths.is_empty() {
                cps::verify_selected(&root, &packages, &paths)?
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
            if runtime {
                cps::verify_elf_runtime(&root, &library_dirs)?;
                println!("Managed ELF runtime verified");
            }
            if packages.is_empty() && paths.is_empty() {
                println!("Image verified");
            } else {
                println!("Selected paths verified");
            }
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
                    let by_filesystem = environments.iter().any(|entry| entry.root_uuid.is_some());
                    let header = if by_filesystem {
                        "ROOT"
                    } else {
                        "ROOT PARTUUID"
                    };
                    println!("DEFAULT\tSTATUS\tID\t{header}\tTITLE");
                    for environment in environments {
                        let marker = if environment.is_default { "*" } else { "-" };
                        let status = if environment.is_retiring {
                            "retiring".to_owned()
                        } else {
                            match environment.tries_left {
                                None => "known-good".to_owned(),
                                Some(0) => "exhausted".to_owned(),
                                Some(tries) => format!("trial ({tries} left)"),
                            }
                        };
                        let root = match environment.root_uuid {
                            Some(uuid) => format!("UUID={uuid}"),
                            None if by_filesystem => {
                                format!("PARTUUID={}", environment.root_partuuid)
                            }
                            None => environment.root_partuuid,
                        };
                        println!(
                            "{marker}\t{status}\t{}\t{root}\t{}",
                            environment.id, environment.title
                        );
                    }
                }
            }
            BootCommand::DestroyBtrfs {
                esp,
                btrfs_volume,
                dry_run,
                id,
            } => {
                let id = destroy_btrfs_boot_environment(&esp, &btrfs_volume, &id, dry_run)?;
                println!(
                    "{} {id}",
                    if dry_run {
                        "Would destroy"
                    } else {
                        "Destroyed"
                    }
                );
            }
            BootCommand::Rollback {
                esp,
                disk,
                esp_partuuid,
                output_disk,
                btrfs_volume,
                dry_run,
                paths,
                json,
                id,
            } => {
                if dry_run {
                    let (target, diff) = cps::preview_btrfs_rollback(
                        esp.as_deref().context("--dry-run requires --esp")?,
                        btrfs_volume
                            .as_deref()
                            .context("--dry-run requires --btrfs-volume")?,
                        id.as_deref(),
                        paths,
                    )?;
                    if json {
                        serde_json::to_writer_pretty(
                            io::stdout().lock(),
                            &serde_json::json!({"target": target, "diff": diff}),
                        )?;
                        println!();
                    } else {
                        println!("Would select {target} for the next boot");
                        print_image_diff(&diff, false)?;
                    }
                    return Ok(ExitCode::SUCCESS);
                }
                let id = match (
                    esp.as_deref(),
                    disk.as_deref(),
                    esp_partuuid.as_deref(),
                    output_disk.as_deref(),
                ) {
                    (Some(esp), None, None, None) => {
                        if let Some(volume) = btrfs_volume.as_deref() {
                            rollback_btrfs_boot_environment(esp, volume, id.as_deref())?
                        } else {
                            rollback_boot_environment(esp, id.as_deref())?
                        }
                    }
                    (None, Some(disk), Some(partuuid), Some(output)) => {
                        rollback_boot_environment_on_disk(disk, partuuid, id.as_deref(), output)?
                    }
                    _ => anyhow::bail!("specify --esp or the offline disk and output options"),
                };
                println!("Selected {id} for the next boot");
            }
            BootCommand::ActivateBtrfs {
                root,
                btrfs_volume,
                esp,
                root_partuuid,
                kernel,
                initrd,
                uki,
                uefi,
                options,
                tries,
                known_good,
            } => {
                let activation = Activation {
                    root_partuuid,
                    kernel,
                    initrd,
                    options,
                    tries: tries.unwrap_or(if uefi { 1 } else { 2 }),
                    known_good,
                };
                let id = if uefi {
                    activation.activate_btrfs_uefi(&root, &btrfs_volume, &esp)?
                } else if uki {
                    activation.activate_btrfs_uki(&root, &btrfs_volume, &esp)?
                } else {
                    activation.activate_btrfs(&root, &btrfs_volume, &esp)?
                };
                println!("Activated {id}");
            }
            BootCommand::BlessUefi { config } => {
                if let Some(id) = System::load(&config)?.bless_uefi()? {
                    println!("Promoted {id} in UEFI BootOrder");
                }
            }
            BootCommand::Prune {
                config,
                keep,
                dry_run,
            } => {
                let system = System::load(&config)?;
                let _lock = system.lock()?;
                let keep = keep.map(usize::try_from).transpose()?;
                let report = system.prune_boot_environments(keep, dry_run)?;
                return Ok(print_boot_prune(&report, dry_run, true));
            }
            BootCommand::DiscardTrial { config } => {
                let system = System::load(&config)?;
                let _lock = system.lock()?;
                let discard = system.discard_trial()?;
                match &discard.cancelled {
                    Some(id) if discard.running => println!(
                        "Cancelled the running trial {id}. Reboot into its fallback; the next \
                         prune or staged change removes it."
                    ),
                    Some(id) => println!("Cancelled trial {id}"),
                    None if discard.prune.removed.is_empty() && discard.prune.failed.is_empty() => {
                        println!("No pending or failed trial")
                    }
                    None => {}
                }
                return Ok(print_boot_prune(&discard.prune, false, false));
            }
            BootCommand::Capture {
                source,
                output,
                exclude_subvolume,
            } => {
                let id = capture_boot_environment(&source, &output, &exclude_subvolume)?;
                println!("Captured {id} at {}", output.display());
            }
            BootCommand::Stage { source, root, plan } => {
                let id = stage_boot_environment(&source, &root, &load_plan(&plan)?)?;
                println!("Staged {id} at {}", root.display());
            }
            BootCommand::StageBtrfs {
                source,
                output,
                exclude_subvolume,
                plan,
            } => {
                let id = stage_btrfs_boot_environment(
                    &source,
                    &output,
                    &load_plan(&plan)?,
                    &exclude_subvolume,
                )?;
                println!("Staged {id} at {}", output.join("root").display());
            }
            BootCommand::Prepare {
                source,
                output,
                size_mib,
                plan,
            } => {
                let id = prepare_boot_environment(&source, &output, &load_plan(&plan)?, size_mib)?;
                println!("Prepared {id} at {}", output.display());
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
