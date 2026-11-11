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
    Activation, BootImage, BuildSettings, BuildToolchain, BuildWitness, ConfigChoice,
    GitSnapshotRef, MarkChoice, PackageChange, PhasedUpdateDeferred, Plan, PlanOptions, Request,
    SourceLayout, abort_transaction, add_publisher_from_url, add_publisher_with_build_trust, apply,
    archive_build_sources, audit_publisher_builds, build_with_recipe_options,
    capture_boot_environment, create_boot_image, create_image, destroy_btrfs_boot_environment,
    export_rebuild_inputs, fetch_build_sources, fetch_plan_with_archives, fetch_sources,
    fetch_sources_with_local, fetch_toolchain, generate_manifest_for_architecture, import_sources,
    list_boot_environments, list_config_updates, list_freezes, list_obsoletions, list_publishers,
    list_withdrawals, pinned_publisher, plan_with_options, prefer_publisher,
    prepare_git_snapshot_ref, publish_witnessed_bundle_with_sources, read_attestation,
    read_worker_config, release, remove_publisher, repair_pending_transaction,
    repository_publisher, resume_transaction, rollback_btrfs_boot_environment,
    set_publisher_build_trust, set_publisher_repositories, stage_btrfs_boot_environment,
    suggest_dependencies, transaction_details, transaction_history, transaction_status, verify,
    verify_pristine, verify_rebuild, withdraw_package,
};
use serde::Serialize;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// An image to read: the running system unless `--root` names another.
#[derive(Args)]
struct Image {
    /// Image root (default: the running system)
    #[arg(long, value_name = "IMAGE", default_value = "/")]
    root: PathBuf,
}

/// The running system, or an offline image named by `--root`.
#[derive(Args)]
struct Target {
    /// Operate on this offline image instead of the running system
    #[arg(long, value_name = "IMAGE")]
    root: Option<PathBuf>,
    /// Distribution configuration of the running system
    #[arg(long, default_value = system::CONFIG, conflicts_with = "root")]
    config: PathBuf,
}

/// Options shared by every command that plans and applies a transaction.
#[derive(Args)]
struct ChangeOptions {
    #[command(flatten)]
    target: Target,
    /// Confirm the displayed transaction without prompting
    #[arg(short = 'y', long, conflicts_with = "dry_run")]
    yes: bool,
    /// Preview the transaction without applying it
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Save the exact transaction plan, for `cps status --plan` or a later `cps apply`
    #[arg(long, value_name = "PLAN")]
    save_plan: Option<PathBuf>,
    /// Skip recommended packages when resolving the transaction
    #[arg(long)]
    no_recommends: bool,
    /// Use the currently cached catalogs (images always plan from their cache)
    #[arg(long, conflicts_with = "root")]
    no_refresh: bool,
    /// Stage the change in a new Btrfs trial boot environment, or for repair and revert a
    /// verified recovery root, instead of changing the running root
    #[arg(long, conflicts_with = "root")]
    stage: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Install packages
    Install {
        #[command(flatten)]
        change: ChangeOptions,
        /// Reconcile the image to exactly these packages and their dependencies
        #[arg(long, conflicts_with_all = ["profile", "lock", "git"])]
        exact: bool,
        /// Install the configured signed release profile into an image with no packages
        #[arg(long, conflicts_with_all = ["packages", "lock", "git"])]
        profile: bool,
        /// Install the exact composition of this image lock into a newly created image
        #[arg(long, value_name = "LOCK", conflicts_with_all = ["packages", "git"])]
        lock: Option<PathBuf>,
        /// Build and install the package described by cps.kdl in this Git repository
        #[arg(long, requires = "git_ref", conflicts_with_all = ["packages", "root"])]
        git: Option<String>,
        /// Pin the Git install to a full commit ID
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
        /// Packages to install, optionally with a publisher and version
        #[arg(
            required_unless_present_any = ["git", "profile", "lock"],
            value_name = "PACKAGE"
        )]
        packages: Vec<String>,
    },
    /// Remove installed packages
    Remove {
        #[command(flatten)]
        change: ChangeOptions,
        #[arg(required = true, value_name = "PACKAGE")]
        packages: Vec<String>,
    },
    /// Remove dependencies no longer needed by explicitly installed packages
    Autoremove {
        #[command(flatten)]
        change: ChangeOptions,
    },
    /// Upgrade the installed packages, or report available upgrades with --check
    Upgrade {
        #[command(flatten)]
        change: ChangeOptions,
        /// Report upgrades, blockers, and any reboot requirement without changing anything
        #[arg(
            long,
            conflicts_with_all = ["root", "yes", "dry_run", "save_plan", "stage", "no_recommends"]
        )]
        check: bool,
        /// With --check, probe the exact newer version of this held-back package
        #[arg(long, value_name = "PACKAGE", requires = "check")]
        explain: Option<String>,
        /// With --check, report the result as JSON
        #[arg(long, requires = "check")]
        json: bool,
        /// Upgrade even if this image is outside the signed profile rollout
        #[arg(long, conflicts_with = "check")]
        force_rollout: bool,
        /// Use only the distribution publishers
        #[arg(long, conflicts_with = "root")]
        base_only: bool,
    },
    /// Restore damaged managed files
    Repair {
        #[command(flatten)]
        change: ChangeOptions,
        /// Repair only this managed path, absolute or relative to the image root (repeat for each path)
        #[arg(long = "path", value_name = "PATH", conflicts_with = "packages")]
        paths: Vec<String>,
        /// Repair the paths of these installed packages (default: every package)
        #[arg(value_name = "PACKAGE")]
        packages: Vec<String>,
    },
    /// Restore vendor defaults for preserved configuration files
    Revert {
        #[command(flatten)]
        change: ChangeOptions,
        /// Preserved configuration file to restore, absolute or relative to the image root
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Freeze installed or explicitly named versions until they are unfrozen
    Freeze {
        #[command(flatten)]
        change: ChangeOptions,
        /// Why the versions are frozen, shown by cps list --frozen
        #[arg(long)]
        reason: Option<String>,
        #[arg(required = true, value_name = "NAME[@VERSION]")]
        packages: Vec<String>,
    },
    /// Lift package version freezes
    Unfreeze {
        #[command(flatten)]
        change: ChangeOptions,
        #[arg(required = true, value_name = "NAME")]
        packages: Vec<String>,
    },
    /// Change local request marks without installing or removing packages
    Mark {
        #[command(flatten)]
        change: ChangeOptions,
        #[arg(value_enum)]
        choice: MarkChoice,
        #[arg(required = true, value_name = "PACKAGE")]
        packages: Vec<String>,
    },
    /// List or change facet rules
    Facet {
        #[command(subcommand)]
        command: FacetCommand,
    },
    /// List or change variants other than arch
    Variant {
        #[command(subcommand)]
        command: VariantCommand,
    },
    /// List or select providers for mediated links
    Mediator {
        #[command(subcommand)]
        command: MediatorCommand,
    },
    /// Review and resolve pending changes to preserved configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Fetch the verified payloads required by a saved plan
    Fetch {
        /// Image the plan targets
        #[arg(long, value_name = "IMAGE")]
        root: PathBuf,
        /// Use a synced historical archive for pinned metadata and matching payloads
        #[arg(long = "archive-cache", value_name = "PUBLISHER=PATH", value_parser = parse_repository)]
        archive_caches: Vec<(String, PathBuf)>,
        plan: PathBuf,
    },
    /// Apply or resume a saved plan
    Apply {
        /// Image the plan targets
        #[arg(long, value_name = "IMAGE")]
        root: PathBuf,
        /// Validate the plan against cached payloads without applying it
        #[arg(short = 'n', long)]
        dry_run: bool,
        plan: PathBuf,
    },
    /// Show an interrupted transaction, a pending reboot, or a saved plan's progress
    Status {
        #[command(flatten)]
        target: Target,
        /// Report progress for this saved plan as JSON
        #[arg(long, value_name = "PLAN")]
        plan: Option<PathBuf>,
        /// Validate an unapplied plan against cached payloads
        #[arg(long, requires = "plan")]
        validate: bool,
    },
    /// Finish an interrupted transaction
    Resume {
        #[command(flatten)]
        target: Target,
        /// Restore damaged managed paths from verified local payloads before retrying final actions
        #[arg(long)]
        repair: bool,
    },
    /// Undo an interrupted transaction and keep the committed revision
    Abort {
        #[command(flatten)]
        target: Target,
        /// Download missing package payloads from each publisher's mirror first
        #[arg(long)]
        fetch: bool,
    },
    /// List completed transactions, most recent first, or show one as JSON
    History {
        #[command(flatten)]
        image: Image,
        /// Show at most this many transactions
        #[arg(long, default_value_t = 20, conflicts_with = "plan")]
        limit: u32,
        #[arg(long, conflicts_with = "plan")]
        json: bool,
        /// Show the recorded details of this completed plan
        #[arg(value_name = "PLAN_SHA256")]
        plan: Option<String>,
    },
    /// List installed packages, available versions, local intent, or freezes
    List {
        #[command(flatten)]
        image: Image,
        /// List the packages in the configured signed catalogs
        #[arg(long)]
        available: bool,
        /// With --available, list every version instead of the newest
        #[arg(long, requires = "available")]
        all_versions: bool,
        /// Show local requests and roles in the installed release profile
        #[arg(long, conflicts_with_all = ["available", "frozen"])]
        intent: bool,
        /// List persistent package version freezes
        #[arg(long, conflicts_with = "available")]
        frozen: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search for packages by name, summary, or a file they ship
    Search {
        #[command(flatten)]
        image: Image,
        /// Search installed packages instead of the signed catalogs
        #[arg(long)]
        installed: bool,
        /// Match an exact path or basename in the signed catalogs
        #[arg(long, conflicts_with = "installed")]
        file: bool,
        #[arg(long)]
        json: bool,
        term: String,
    },
    /// Show an installed or available package's manifest in KDL
    Info {
        #[command(flatten)]
        image: Image,
        /// Show the manifest from the signed catalogs
        #[arg(long)]
        available: bool,
        #[arg(value_name = "NAME[@VERSION]")]
        package: String,
    },
    /// Show package candidates, publisher order, and image constraints
    Policy {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        json: bool,
        /// Try planning an install of this selector without changing the image
        #[arg(long)]
        probe: bool,
        package: String,
    },
    /// Show installed package IDs that own an exact image path
    Owner {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        json: bool,
        /// Absolute or root-relative path within the image
        path: String,
    },
    /// List the paths owned by an installed package
    Contents {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        json: bool,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// List the installed packages that satisfy a package's direct requirements
    Deps {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        json: bool,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// List the installed packages that directly depend on a package
    Rdeps {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        json: bool,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// Explain why a package is installed
    Why {
        #[command(flatten)]
        image: Image,
        #[arg(value_name = "PACKAGE")]
        package: String,
    },
    /// Check an image against its installed manifests, offline
    Verify {
        #[command(flatten)]
        image: Image,
        /// Require an inactive root with pristine package paths and no unmanaged entries
        #[arg(long, conflicts_with_all = ["packages", "paths"])]
        pristine: bool,
        /// Limit file verification to the installed package
        #[arg(long = "package", value_name = "PACKAGE")]
        packages: Vec<String>,
        /// Limit file verification to this managed path
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// Trust recorded file fingerprints that still match instead of hashing every file
        #[arg(long, conflicts_with = "pristine")]
        quick: bool,
        /// Audit the installed managed ELF interpreter and shared-library closure
        #[arg(long, conflicts_with_all = ["packages", "paths"])]
        runtime: bool,
        /// Extra library directory for --runtime
        #[arg(long = "library-dir", requires = "runtime", value_name = "PATH")]
        library_dirs: Vec<String>,
    },
    /// Manage an image's ordered set of trusted publishers
    Publisher {
        #[command(subcommand)]
        command: PublisherCommand,
    },
    /// Create, compare, lock, and write images
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Stage, activate, and manage boot environments
    Boot {
        #[command(subcommand)]
        command: BootCommand,
    },
    /// Prepare pinned package sources for offline builds
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Maintain build recipes and plan builds
    Recipe {
        #[command(subcommand)]
        command: RecipeCommand,
    },
    /// Prepare a package manifest for review
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
    /// Build a package with verified inputs in an offline sandbox
    Build {
        #[command(flatten)]
        inputs: BuildInputs,
        /// New directory for the build bundle: recipe, templates, manifests, and proto tree
        #[arg(long)]
        output: PathBuf,
        /// Build twice in fresh sandboxes and require identical outputs
        #[arg(long)]
        check_reproducible: bool,
        /// Sign a build attestation with this key: PATH, file:PATH, or command:DESCRIPTOR (requires --check-reproducible)
        #[arg(long, value_name = "KEY", requires = "check_reproducible")]
        key: Option<PathBuf>,
        /// Build recipe to run
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
    /// Publish manifests, build bundles, or releases to a local repository
    Publish {
        #[command(subcommand)]
        command: PublishCommand,
    },
    /// Manage a publisher repository, its trust root, and its TUF mirrors
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
}

#[derive(Subcommand)]
enum FacetCommand {
    /// List facet rules, or show which rule decides each named facet
    List {
        #[command(flatten)]
        image: Image,
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
    /// Change facet rules for installed package actions
    Set {
        #[command(flatten)]
        change: ChangeOptions,
        /// Also apply the PACKAGE:FACET rules to each package's installed dependencies
        #[arg(long)]
        with_dependencies: bool,
        /// FACET, PREFIX.*, or * for the whole image; PACKAGE:FACET or PREFIX/*:FACET for
        /// packages; `default` removes the rule
        #[arg(required = true, value_name = "[PACKAGE:]FACET=true|false|default")]
        facets: Vec<String>,
    },
}

#[derive(Subcommand)]
enum VariantCommand {
    /// List the image's variant values, including arch
    List {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        json: bool,
    },
    /// Change variants other than arch for installed package actions
    Set {
        #[command(flatten)]
        change: ChangeOptions,
        /// Unset a variant
        #[arg(long, value_name = "NAME")]
        unset: Vec<String>,
        #[arg(value_name = "NAME=VALUE", required_unless_present = "unset")]
        variants: Vec<String>,
    },
}

#[derive(Subcommand)]
enum MediatorCommand {
    /// Show preferred and active providers for mediated links
    List {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        json: bool,
    },
    /// Select the provider of a mediated link group
    Set {
        #[command(flatten)]
        change: ChangeOptions,
        group: String,
        /// Provider to select; omit it to restore the default
        provider: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// List pending vendor configuration copies
    List {
        #[command(flatten)]
        image: Image,
    },
    /// Compare a preserved configuration file with its verified pending vendor copy
    Diff {
        #[command(flatten)]
        image: Image,
        path: String,
    },
    /// Keep the local copy or take the vendor copy of a pending configuration update
    Resolve {
        #[command(flatten)]
        change: ChangeOptions,
        #[arg(long, value_enum)]
        take: ConfigChoice,
        /// Path relative to the image root
        path: String,
    },
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Initialize an empty directory as an image
    Create {
        /// Directory to initialize
        #[arg(long, value_name = "IMAGE")]
        root: PathBuf,
        /// Image configuration with publishers, architecture, facets, and variants
        #[arg(
            long,
            conflicts_with_all = ["repo", "trust_root", "arch", "facet", "variant"]
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
        /// Local repository of the image's only publisher
        #[arg(long, required_unless_present = "config")]
        repo: Option<PathBuf>,
        /// Pinned trust root of the publisher, which also names it
        #[arg(long, required_unless_present = "config")]
        trust_root: Option<PathBuf>,
        /// Image architecture (default: the host's)
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
    /// Compare committed package state between two roots, offline
    Diff {
        /// Include managed file actions and ownership
        #[arg(long)]
        paths: bool,
        #[arg(long)]
        json: bool,
        before: PathBuf,
        after: PathBuf,
    },
    /// Print a portable lock for an image's committed package composition
    Lock {
        #[command(flatten)]
        image: Image,
        /// Export the lock and its verified provisioning objects to a new directory
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// Write an inactive root and its known-good boot entry to a new copy of a GPT disk
    Disk {
        /// Directory that becomes the Btrfs top level. It holds only the root, environments, and /var directories.
        #[arg(long)]
        btrfs_volume: PathBuf,
        /// Inactive CPS root in the volume directory, written as the root subvolume
        #[arg(long)]
        root: PathBuf,
        /// Empty directory in the volume directory, written as the environments subvolume
        #[arg(long)]
        environments: PathBuf,
        /// Empty directory in the volume directory, written as the shared /var subvolume that the root's /etc/fstab mounts
        #[arg(long)]
        var: Option<PathBuf>,
        /// GPT disk with a systemd-boot ESP and the target Linux filesystem partition
        #[arg(long)]
        disk: PathBuf,
        /// GPT PARTUUID of the partition that receives the Btrfs filesystem
        #[arg(long)]
        root_partuuid: String,
        /// New disk file to write
        #[arg(long)]
        output: PathBuf,
        /// Kernel path in the root, for images without kernel-install artifacts
        #[arg(long, default_value = "boot/vmlinuz")]
        kernel: String,
        /// Initramfs path in the root, for images without kernel-install artifacts
        #[arg(long)]
        initrd: Option<String>,
        /// Kernel arguments that the installation requires
        #[arg(long, default_value = "")]
        options: String,
    },
}

/// Where boot environments live: explicit paths, or the running system's configuration.
#[derive(Args)]
struct BootLocation {
    /// Distribution configuration that names the ESP and Btrfs volume
    #[arg(long, default_value = system::CONFIG)]
    config: PathBuf,
    /// Mount point of the EFI System Partition (default: from --config)
    #[arg(long)]
    esp: Option<PathBuf>,
    /// Mounted Btrfs top-level volume holding the roots (default: from --config)
    #[arg(long)]
    btrfs_volume: Option<PathBuf>,
}

impl BootLocation {
    /// The ESP and Btrfs volume, reading the configuration only for missing paths.
    fn resolve(self) -> Result<(PathBuf, PathBuf)> {
        if let (Some(esp), Some(volume)) = (&self.esp, &self.btrfs_volume) {
            return Ok((esp.clone(), volume.clone()));
        }
        let system = System::load(&self.config)?;
        let (esp, volume) = system.boot_paths();
        Ok((
            self.esp.unwrap_or_else(|| esp.to_owned()),
            self.btrfs_volume.unwrap_or_else(|| volume.to_owned()),
        ))
    }
}

#[derive(Subcommand)]
enum BootCommand {
    /// List registered boot environments and the configured default
    List {
        /// Distribution configuration that names the ESP
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
        /// Mount point of the EFI System Partition (default: from --config)
        #[arg(long)]
        esp: Option<PathBuf>,
    },
    /// Select a known-good environment for the next boot
    Rollback {
        #[command(flatten)]
        location: BootLocation,
        /// Compare the configured default with the rollback root without changing boot selection
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Include managed file actions and ownership in the preview. Requires --dry-run.
        #[arg(long, requires = "dry_run")]
        paths: bool,
        /// Print the preview as JSON. Requires --dry-run.
        #[arg(long, requires = "dry_run")]
        json: bool,
        /// Environment ID. Defaults to the previous known-good environment.
        id: Option<String>,
    },
    /// Stage a saved plan in a writable Btrfs snapshot of a read-only baseline
    Stage {
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
    /// Publish a boot entry for a root on a mounted Btrfs volume
    Activate {
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
    /// Retire an inactive root and its boot artifacts, or resume its cleanup
    Destroy {
        #[command(flatten)]
        location: BootLocation,
        /// Run every check without changing anything
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Environment ID to retire, as shown by boot list
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
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Cancel a pending or running trial and remove failed trials
    DiscardTrial {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
    },
    /// Merge the running /etc into the trial selected for the next boot.
    /// cps-finalize-trial.service runs this at shutdown.
    FinalizeTrial {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
        /// Only request the merge at this boot's shutdown, for a trial staged
        /// during an earlier boot
        #[arg(long)]
        arm: bool,
    },
    /// Verify and promote the running direct UEFI trial in the firmware boot order
    Bless {
        #[arg(long, default_value = system::CONFIG)]
        config: PathBuf,
    },
}

#[derive(Args)]
struct BuildInputs {
    /// Local repository of the toolchain publisher
    #[arg(long, required_unless_present = "toolchain_config")]
    repo: Option<PathBuf>,
    /// Trust root file that matches the recipe's key-sha256 pin and names the publisher
    #[arg(long, required_unless_present = "toolchain_config")]
    trust_root: Option<PathBuf>,
    /// Worker-local repository and trust key locations for the pinned publishers
    #[arg(long, conflicts_with_all = ["repo", "trust_root"])]
    toolchain_config: Option<PathBuf>,
    /// Directory of the recipe's source inputs at their recipe paths
    #[arg(long, required_unless_present = "source_cache")]
    sources: Option<PathBuf>,
    /// Source cache filled by cps source fetch or cps source import
    #[arg(long, conflicts_with = "sources")]
    source_cache: Option<PathBuf>,
    /// Build worker limits: time, scratch space, file, output, and log sizes, and jobs
    #[arg(long)]
    worker_config: Option<PathBuf>,
    /// Override a build option declared in the recipe
    #[arg(long = "option", value_name = "NAME=VALUE")]
    recipe_options: Vec<String>,
    /// The toolchain publisher, read from --trust-root
    #[arg(skip)]
    publisher: String,
}

struct PreparedBuildInputs<'a> {
    sources: (&'a Path, SourceLayout),
    toolchain: BuildToolchain<'a>,
    worker: cps::BuildWorker,
    overrides: BTreeMap<String, String>,
}

impl BuildInputs {
    fn prepare(&mut self) -> Result<PreparedBuildInputs<'_>> {
        if let Some(trust_root) = &self.trust_root {
            self.publisher = pinned_publisher(trust_root)?;
        }
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
                publisher: &self.publisher,
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

#[derive(Subcommand)]
enum SourceCommand {
    /// Pin a Git branch, tag, or commit and prepare a versioned build recipe
    #[command(group(clap::ArgGroup::new("git_ref").required(true).args(["branch", "tag", "commit"])))]
    GitSnapshot {
        /// Local Git repository path or remote Git URL
        #[arg(long)]
        git: OsString,
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
        /// The publisher's trust root JSON file that verifies the repository's catalogs
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
    /// Fetch the sources a publisher archived for a published build
    FetchArchived {
        #[arg(long)]
        repo: PathBuf,
        /// Pinned trust root of the publisher, which also names it
        #[arg(long)]
        trust_root: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        /// Select a historical signed catalog by SHA-256
        #[arg(long)]
        catalog: Option<String>,
        package_id: String,
    },
}

#[derive(Subcommand)]
enum ManifestCommand {
    /// Generate KDL from a staged prototype tree
    Generate {
        /// Staged prototype tree to describe
        #[arg(long)]
        proto: PathBuf,
        /// Publisher of the package
        #[arg(long)]
        publisher: String,
        /// Package version, such as 1.0.0-0
        #[arg(long)]
        version: String,
        /// Restrict the package to one architecture, such as x86_64
        #[arg(long)]
        arch: Option<String>,
        /// Owner UID for every generated action
        #[arg(long, default_value_t = 0)]
        uid: u32,
        /// Owner GID for every generated action
        #[arg(long, default_value_t = 0)]
        gid: u32,
        /// Package name, such as apps/tool
        name: String,
    },
    /// Review generated library capabilities and suggest path-based requires using pinned catalogs
    Dependencies {
        #[arg(long)]
        proto: PathBuf,
        /// Image configuration with target architecture, facets, and local publisher paths
        #[arg(long)]
        config: PathBuf,
        /// Exact signed catalog digest for each configured publisher
        #[arg(long = "catalog", required = true, value_name = "PUBLISHER=SHA256")]
        catalogs: Vec<String>,
        /// Target PATH directory for env shebangs, in search order
        #[arg(long = "path-dir", default_values = ["/usr/bin", "/bin"], value_name = "PATH")]
        path_dirs: Vec<String>,
        /// Fail when a detected external provider lacks a matching direct require
        #[arg(long)]
        check: bool,
        manifest: PathBuf,
    },
    /// Show the shared-library capabilities that publication generates
    Generated {
        #[arg(long)]
        proto: PathBuf,
        manifest: PathBuf,
    },
}

#[derive(Subcommand)]
enum RecipeCommand {
    /// Refresh toolchain, manifest, and source pins in recipes, keeping their formatting
    #[command(group(
        clap::ArgGroup::new("pins")
            .required(true)
            .multiple(true)
            .args(["catalogs", "trust_roots", "manifests", "local_sources", "sources"])
    ))]
    Repin {
        /// New catalog digest for a toolchain publisher
        #[arg(long = "toolchain-catalog", value_name = "PUBLISHER=SHA256")]
        catalogs: Vec<String>,
        /// Set key-sha256 of the toolchain publisher that this trust root names
        #[arg(long = "trust-root", value_name = "PATH")]
        trust_roots: Vec<PathBuf>,
        /// Recompute manifest digests from the templates next to each recipe
        #[arg(long)]
        manifests: bool,
        /// Recompute source sizes and digests from files in each recipe's directory
        #[arg(long)]
        local_sources: bool,
        /// Recompute source sizes and digests from this directory laid out by source path (repeatable, searched in order)
        #[arg(long, value_name = "DIR")]
        sources: Vec<PathBuf>,
        /// Print the changes without writing them
        #[arg(short = 'n', long, conflicts_with = "check")]
        dry_run: bool,
        /// Write nothing, print stale pins, and fail if any pin is stale
        #[arg(long)]
        check: bool,
        /// Recipe files or directories to search for recipes
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Print recipes in dependency order for building
    Order {
        /// Show the recipes that each recipe follows and why
        #[arg(long)]
        explain: bool,
        /// Recipe files or directories to search for recipes
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// List the packages and recipes to rebuild after packages change
    Rebuilds {
        /// Image configuration with target architecture and local publisher paths
        #[arg(long)]
        config: PathBuf,
        /// Exact signed catalog digest for each configured publisher
        #[arg(long = "catalog", required = true, value_name = "PUBLISHER=SHA256")]
        catalogs: Vec<String>,
        /// Changed package, compared with its previous version
        #[arg(long = "changed", required = true, value_name = "PACKAGE[@VERSION]")]
        changed: Vec<String>,
        /// Recipe files or directories that produce the packages
        #[arg(long = "recipes", value_name = "PATH")]
        recipes: Vec<PathBuf>,
    },
    /// Fetch payloads for the toolchain pinned by a build recipe
    FetchToolchain {
        #[arg(long)]
        toolchain_config: PathBuf,
        recipe: PathBuf,
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
        #[command(flatten)]
        image: Image,
    },
    /// Verify and append a publisher to the image's search order
    Add {
        #[command(flatten)]
        image: Image,
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
        #[command(flatten)]
        image: Image,
        #[arg(long, required = true, value_name = "PUBLISHER=PATH", value_parser = parse_repository)]
        repo: Vec<(String, PathBuf)>,
    },
    /// Replace a publisher's locally configured builder keys and threshold
    SetBuildTrust {
        #[command(flatten)]
        image: Image,
        #[arg(long)]
        policy: PathBuf,
        /// Change admission policy without auditing existing packages
        #[arg(long)]
        future_only: bool,
        name: String,
    },
    /// Check installed packages against the configured builder policy
    AuditBuilds {
        #[command(flatten)]
        image: Image,
        name: String,
    },
    /// Search this publisher first when selecting new packages
    Prefer {
        #[command(flatten)]
        image: Image,
        name: String,
    },
    /// Remove a publisher that has no installed packages
    Remove {
        #[command(flatten)]
        image: Image,
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
enum PublishCommand {
    /// Publish manifests and their payloads in one signed catalog update
    Manifests {
        /// Local repository to publish into
        #[arg(long)]
        repo: PathBuf,
        /// Prototype tree that holds the manifests' payloads at their action paths
        #[arg(long)]
        proto: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        /// Manifests to publish
        #[arg(required = true)]
        manifests: Vec<PathBuf>,
    },
    /// Publish a reproducible build bundle after verifying its worker signature and reviewed recipe
    Bundle {
        /// Local repository to publish into
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        /// Public key of the build worker that signed the bundle's attestation
        #[arg(long)]
        builder_key: PathBuf,
        /// SHA-256 of the reviewed build recipe
        #[arg(long)]
        recipe_sha256: String,
        /// Another worker's public key and its attestation for the same build
        #[arg(long, value_name = "KEY=ATTESTATION", value_parser = parse_witness)]
        witness: Vec<BuildWitness>,
        /// Cache of verified source files to include in the publication
        #[arg(long)]
        source_cache: Option<PathBuf>,
        /// Build bundle from cps build --key
        bundle: PathBuf,
    },
    /// Commit a set of reviewed build bundles and withdrawals as one signed catalog
    Release {
        /// Local repository to publish into
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
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
}

/// Membership of the next trust root version. Omitted keys and thresholds
/// keep the previous version's values.
#[derive(Args)]
struct TrustMembership {
    /// Root key: a public key file or command:DESCRIPTOR (repeat; replaces the root keys)
    #[arg(long, value_name = "PUBLIC_KEY")]
    root_key: Vec<PathBuf>,
    /// Root key signatures required (default: the previous version's, or 1)
    #[arg(long)]
    root_threshold: Option<usize>,
    /// Catalog key: a public key file or command:DESCRIPTOR (repeat; replaces the catalog keys)
    #[arg(long, value_name = "PUBLIC_KEY")]
    catalog_key: Vec<PathBuf>,
    /// Catalog key signatures required (default: the previous version's, or 1)
    #[arg(long)]
    catalog_threshold: Option<usize>,
    /// RFC 3339 UTC expiry, such as 2027-01-01T00:00:00Z (default: one year from now)
    #[arg(long)]
    expires: Option<String>,
}

impl TrustMembership {
    fn draft<'a>(&'a self, publisher: Option<&'a str>) -> cps::TrustDraft<'a> {
        cps::TrustDraft {
            publisher,
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
        /// Publisher name, required for a repository's first version
        #[arg(long)]
        publisher: Option<String>,
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
            let path = cps::rotate_trust(&repo, &membership.draft(None), &key)?;
            println!("Created {}", path.display());
            print!("{}", cps::show_trust(&path)?);
        }
        TrustCommand::Draft {
            repo,
            publisher,
            membership,
            output,
        } => {
            let draft = cps::draft_trust(&repo, &membership.draft(publisher.as_deref()))?;
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

/// A TUF mirror location: one base URL, or separate metadata and targets URLs.
#[derive(Args)]
struct MirrorUrls {
    /// Base URL of the mirror, containing metadata/ and targets/
    #[arg(long, conflicts_with_all = ["metadata", "targets"], required_unless_present = "metadata")]
    url: Option<String>,
    /// URL of the mirror's metadata directory, when it is not under --url
    #[arg(long, requires = "targets")]
    metadata: Option<String>,
    /// URL of the mirror's targets directory, when it is not under --url
    #[arg(long, requires = "metadata")]
    targets: Option<String>,
}

impl MirrorUrls {
    fn resolve(self) -> (String, String) {
        match (self.url, self.metadata, self.targets) {
            (Some(base), _, _) => {
                let base = base.trim_end_matches('/');
                (format!("{base}/metadata/"), format!("{base}/targets/"))
            }
            (None, Some(metadata), Some(targets)) => (metadata, targets),
            _ => unreachable!("clap requires --url or --metadata with --targets"),
        }
    }
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Create, rotate, sign, and inspect the publisher trust root
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
    /// Withdraw an exact published version from future package selections
    Withdraw {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        package_id: String,
    },
    /// List withdrawn package IDs and their original manifest digests
    Withdrawals {
        #[arg(long)]
        repo: PathBuf,
        /// Pinned trust root of the publisher, which also names it
        #[arg(long)]
        trust_root: PathBuf,
    },
    /// Replace old withdrawal records with signed tombstones up to a catalog sequence
    PruneWithdrawals {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        /// Exact current catalog to build on
        #[arg(long)]
        base: String,
        /// Prune records withdrawn at or before this catalog sequence
        #[arg(long, value_name = "SEQUENCE")]
        through: u64,
    },
    /// List signed package obsoletion cutoffs
    Obsoletions {
        #[arg(long)]
        repo: PathBuf,
        /// Pinned trust root of the publisher, which also names it
        #[arg(long)]
        trust_root: PathBuf,
    },
    /// Publish the exact current catalog again under the newest trust root
    Resign {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        /// Exact current catalog to build on
        #[arg(long)]
        base: String,
    },
    /// Widen one profile's rollout on the exact current catalog
    WidenRollout {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        /// Exact current catalog to build on
        #[arg(long)]
        base: String,
        #[arg(long)]
        profile: String,
        #[arg(long)]
        percentage: u8,
    },
    /// Enable a catalog-signed index for available package file lookups
    IndexFiles {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
    },
    /// Attach verified source files to a published build
    ArchiveSources {
        #[arg(long)]
        repo: PathBuf,
        /// Catalog key: PATH, file:PATH, or command:DESCRIPTOR (repeat to meet the catalog threshold)
        #[arg(long, value_name = "KEY", required = true)]
        key: Vec<PathBuf>,
        #[arg(long)]
        source_cache: PathBuf,
        package_id: String,
    },
    /// Print the signed build evidence for a full package ID
    Attestation {
        #[arg(long)]
        repo: PathBuf,
        /// Pinned trust root of the publisher, which also names it
        #[arg(long)]
        trust_root: PathBuf,
        package_id: String,
    },
    /// Extract the recipe, reviewed manifest, and attestation for an exact package
    RebuildInputs {
        #[arg(long)]
        repo: PathBuf,
        /// Pinned trust root of the publisher, which also names it
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
        /// Pinned trust root of the publisher, which also names it
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
    /// Export signed catalogs and their objects for TUF signing
    Export {
        #[arg(long)]
        repo: PathBuf,
        /// Pinned trust root of the publisher, which also names it
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
    /// Sign an exported mirror with sharded TUF targets metadata
    SignExport {
        /// The mirror's active signed TUF root metadata
        #[arg(long)]
        tuf_root: PathBuf,
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
        /// Version of the new targets, snapshot, and timestamp metadata, above every published version
        #[arg(long)]
        version: std::num::NonZeroU64,
        /// Targets and snapshot expiry: an RFC 3339 time, such as 2027-01-01T00:00:00Z
        #[arg(long, value_name = "TIME")]
        release_expires: String,
        /// Timestamp expiry: an RFC 3339 time, such as 2026-12-08T00:00:00Z
        #[arg(long, value_name = "TIME")]
        timestamp_expires: String,
        /// New directory for the signed metadata and targets
        #[arg(long)]
        output: PathBuf,
        /// Directory written by `cps repo export`
        export: PathBuf,
    },
    /// Re-sign the timestamp, and optionally the snapshot, of a published mirror
    RefreshMetadata {
        /// The mirror's active signed TUF root metadata
        #[arg(long)]
        tuf_root: PathBuf,
        /// The published versioned snapshot metadata, such as 7.snapshot.json
        #[arg(long)]
        snapshot: PathBuf,
        /// Timestamp role key: PATH, file:PATH, or command:DESCRIPTOR (repeatable)
        #[arg(long, value_name = "KEY", required = true)]
        timestamp_key: Vec<PathBuf>,
        /// Also re-sign the snapshot with this key (repeatable)
        #[arg(long, value_name = "KEY", requires = "snapshot_expires")]
        snapshot_key: Vec<PathBuf>,
        /// Expiry of the re-signed snapshot: an RFC 3339 time, such as 2027-01-01T00:00:00Z
        #[arg(long, value_name = "TIME", requires = "snapshot_key")]
        snapshot_expires: Option<String>,
        /// Version of the new timestamp and any re-signed snapshot, above the snapshot's version and every published timestamp version
        #[arg(long)]
        version: std::num::NonZeroU64,
        /// Timestamp expiry: an RFC 3339 time, such as 2026-12-08T00:00:00Z
        #[arg(long, value_name = "TIME")]
        timestamp_expires: String,
        /// New directory for timestamp.json and any re-signed snapshot
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify and cache a release from a TUF mirror
    Sync {
        /// Local cache repository to update
        #[arg(long)]
        repo: PathBuf,
        /// Trusted TUF root metadata of the mirror
        #[arg(long)]
        tuf_root: PathBuf,
        #[command(flatten)]
        mirror: MirrorUrls,
        /// Pinned trust root of the publisher, which also names it
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
    /// Remove unused payload files from a TUF mirror cache
    PruneCache {
        #[arg(long)]
        repo: PathBuf,
        /// Pinned trust root of the publisher, which also names it
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
        /// List the files that would be removed without removing them
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
}

fn print_etc_merge(trial: &str, report: &cps::EtcMergeReport) {
    if report.is_empty() {
        println!("No local /etc changes to merge into {trial}");
        return;
    }
    println!(
        "Merged /etc into {trial}: {} applied, {} merged, {} configuration updates, {} kept from \
         the update",
        report.applied.len(),
        report.merged.len(),
        report.config_updates.len(),
        report.conflicts.len()
    );
    for path in &report.config_updates {
        println!("  /{path}: kept the local edit; review the update with `cps config diff`");
    }
    for conflict in &report.conflicts {
        match &conflict.saved {
            Some(saved) => println!(
                "  /{}: kept the update's version, {}. The running version is saved at /{saved}",
                conflict.path, conflict.reason
            ),
            None => println!(
                "  /{}: kept the update's version, {}",
                conflict.path, conflict.reason
            ),
        }
    }
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

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

fn print_lines(lines: Vec<String>, json: bool) -> Result<()> {
    if json {
        return print_json(&lines);
    }
    let mut out = io::stdout().lock();
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Ask on the terminal unless `yes` already confirmed.
fn confirm(yes: bool, question: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    ensure!(
        io::stdin().is_terminal(),
        "use --yes to confirm in a non-interactive session"
    );
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

type GitSource<'a> = (&'a str, GitSnapshotRef<'a>, Option<system::GitSignatures>);

/// Plan and apply a change on the running system, or on the image `--root` names.
fn change(
    options: ChangeOptions,
    request: Request,
    force_rollout: bool,
    base_only: bool,
    git: Option<GitSource<'_>>,
) -> Result<()> {
    match options.target.root.clone() {
        Some(root) => change_image(&root, &options, request, force_rollout),
        None => change_system(options, request, force_rollout, base_only, git),
    }
}

/// Plan a change for an offline image, then fetch its payloads and apply it.
fn change_image(
    root: &Path,
    options: &ChangeOptions,
    request: Request,
    force_rollout: bool,
) -> Result<()> {
    let plan = plan_with_options(
        root,
        request,
        PlanOptions {
            install_recommends: !options.no_recommends,
            force_phased_update: force_rollout,
        },
    )?;
    if let Some(path) = &options.save_plan {
        save(path, &plan).with_context(|| format!("saving {}", path.display()))?;
    }
    println!("{}", plan.summary());
    if options.dry_run {
        return Ok(());
    }
    if !confirm(options.yes, "Proceed?")? {
        println!("Canceled.");
        return Ok(());
    }
    fetch_plan_with_archives(root, &plan, &[])?;
    println!("Image revision {}", apply(root, &plan)?);
    Ok(())
}

fn change_system(
    options: ChangeOptions,
    mut request: Request,
    force_rollout: bool,
    base_only: bool,
    git: Option<GitSource<'_>>,
) -> Result<()> {
    let recovery = matches!(
        request,
        Request::Repair { .. } | Request::RepairPaths { .. } | Request::Revert { .. }
    );
    let system = System::load(&options.target.config)?
        .with_install_recommends(!options.no_recommends)
        .with_force_phased_update(force_rollout)
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
        confirm(options.yes, "Build this Git commit?")
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
    if options.stage {
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
    if !confirm(options.yes, "Proceed?")? {
        println!("Canceled.");
        return Ok(());
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
                return change_system(options, Request::Update, force_rollout, true, None);
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
            change: options,
            exact,
            profile,
            lock,
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
            let request = if let Some(lock) = lock {
                Request::InstallLock {
                    lock: Box::new(cps::read_image_lock(&lock)?),
                }
            } else if profile {
                Request::InstallProfile
            } else if exact {
                Request::ExactInstall { packages }
            } else {
                Request::Install { packages }
            };
            change(options, request, false, false, source)?;
        }
        Command::Upgrade {
            change: options,
            check,
            explain,
            json,
            force_rollout,
            base_only,
        } => {
            if check {
                return check_system_updates(
                    &options.target.config,
                    !options.no_refresh,
                    json,
                    base_only,
                    explain.as_deref(),
                );
            }
            change(options, Request::Update, force_rollout, base_only, None)?;
        }
        Command::Remove {
            change: options,
            packages,
        } => change(options, Request::Remove { packages }, false, false, None)?,
        Command::Autoremove { change: options } => {
            change(options, Request::Autoremove, false, false, None)?
        }
        Command::Repair {
            change: options,
            paths,
            packages,
        } => {
            let request = if paths.is_empty() {
                Request::Repair { packages }
            } else {
                Request::RepairPaths { paths }
            };
            change(options, request, false, false, None)?;
        }
        Command::Revert {
            change: options,
            paths,
        } => change(options, Request::Revert { paths }, false, false, None)?,
        Command::Freeze {
            change: options,
            reason,
            packages,
        } => change(
            options,
            Request::Freeze { packages, reason },
            false,
            false,
            None,
        )?,
        Command::Unfreeze {
            change: options,
            packages,
        } => change(options, Request::Unfreeze { packages }, false, false, None)?,
        Command::Mark {
            change: options,
            choice,
            packages,
        } => change(
            options,
            Request::Mark { choice, packages },
            false,
            false,
            None,
        )?,
        Command::Facet {
            command:
                FacetCommand::Set {
                    change: options,
                    with_dependencies,
                    facets,
                },
        } => {
            let request = Request::ChangeFacet {
                facets: parse_facet_changes(facets)?,
                with_dependencies,
            };
            change(options, request, false, false, None)?;
        }
        Command::Facet {
            command:
                FacetCommand::List {
                    image,
                    package,
                    facets,
                    json,
                },
        } => {
            let root = image.root;
            if package.is_none() && facets.is_empty() {
                let rows = list_facets(&root)?;
                if json {
                    print_json(&rows)?;
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
                    print_json(&rows)?;
                } else {
                    println!("FACET\tVALUE\tRULE");
                    for row in rows {
                        println!("{}\t{}\t{}", row.facet, row.value, row.source);
                    }
                }
            }
        }
        Command::Variant {
            command:
                VariantCommand::Set {
                    change: options,
                    unset,
                    variants,
                },
        } => {
            let request = Request::ChangeVariant {
                variants: parse_variant_changes(variants, unset)?,
            };
            change(options, request, false, false, None)?;
        }
        Command::Variant {
            command: VariantCommand::List { image, json },
        } => {
            let variants = cps::query::list_variants(&image.root)?;
            if json {
                print_json(&variants)?;
            } else {
                println!("VARIANT\tVALUE");
                for (name, value) in variants {
                    println!("{name}\t{value}");
                }
            }
        }
        Command::Mediator {
            command:
                MediatorCommand::Set {
                    change: options,
                    group,
                    provider,
                },
        } => change(
            options,
            Request::SetMediator { group, provider },
            false,
            false,
            None,
        )?,
        Command::Mediator {
            command: MediatorCommand::List { image, json },
        } => {
            let rows = cps::query::list_mediators(&image.root)?;
            if json {
                print_json(&rows)?;
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
        Command::Config {
            command:
                ConfigCommand::Resolve {
                    change: options,
                    take,
                    path,
                },
        } => change(
            options,
            Request::ResolveConfig { path, take },
            false,
            false,
            None,
        )?,
        Command::Config {
            command: ConfigCommand::Diff { image, path },
        } => {
            let root = image.root;
            let relative = path.strip_prefix('/').unwrap_or(&path);
            let saved = cps::saved_etc_versions(&root)?
                .into_iter()
                .filter(|(_, saved, _)| saved == relative)
                .next_back();
            if let Some((_, _, copy)) = saved
                && !cps::list_config_updates(&root)?
                    .iter()
                    .any(|update| update.path == relative)
            {
                let status = std::process::Command::new("/usr/bin/diff")
                    .args(["-u", "--label", "current", "--label", "saved", "--"])
                    .arg(root.join(relative))
                    .arg(root.join(copy))
                    .status()
                    .context("running /usr/bin/diff")?;
                return match status.code() {
                    Some(0) => Ok(ExitCode::SUCCESS),
                    Some(1) => Ok(ExitCode::FAILURE),
                    _ => anyhow::bail!("configuration diff failed with {status}"),
                };
            }
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
            command: ConfigCommand::List { image },
        } => {
            let root = image.root;
            let updates = list_config_updates(&root)?;
            let saved = cps::saved_etc_versions(&root)?;
            if updates.is_empty() && saved.is_empty() {
                println!("No pending configuration updates");
            } else {
                println!("STATUS\tREVISION\tPATH\tVENDOR COPY");
                for (revision, path, copy) in &saved {
                    println!("saved local\t{revision}\t{path:?}\t{copy:?}");
                }
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
        Command::Fetch {
            root,
            archive_caches,
            plan,
        } => {
            let saved = load_plan(&plan)?;
            let count = fetch_plan_with_archives(&root, &saved, &archive_caches)?;
            println!("Verified {count} plan payloads");
        }
        Command::Apply {
            root,
            dry_run,
            plan,
        } => {
            let saved = load_plan(&plan)?;
            if dry_run {
                cps::validate_plan(&root, &saved)?;
                println!("Plan validated against cached payloads");
            } else {
                println!("Image revision {}", apply(&root, &saved)?);
            }
        }
        Command::Status {
            target,
            plan,
            validate,
        } => match target.root {
            Some(root) => {
                if let Some(path) = plan {
                    let progress = cps::transaction_progress(&root, &load_plan(&path)?, validate)?;
                    print_json(&progress)?;
                    return Ok(ExitCode::SUCCESS);
                }
                print_image_status(&root)?;
            }
            None => {
                let system = System::load(&target.config)?;
                if let Some(path) = plan {
                    let progress = system.transaction_progress(&load_plan(&path)?, validate)?;
                    print_json(&progress)?;
                    return Ok(ExitCode::SUCCESS);
                }
                print_system_status(&system)?;
            }
        },
        Command::Resume { target, repair } => match target.root {
            Some(root) => {
                let aborting = transaction_status(&root)?
                    .pending
                    .is_some_and(|pending| pending.aborting);
                let revision = if repair {
                    repair_pending_transaction(&root)?
                } else {
                    resume_transaction(&root)?
                };
                if let Some(revision) = revision {
                    if aborting {
                        println!("Aborted the pending transaction");
                    }
                    println!("Image revision {revision}");
                } else {
                    println!("No pending transaction");
                }
            }
            None => {
                let system = System::load(&target.config)?;
                let _lock = system.lock()?;
                let aborting = transaction_status(Path::new("/"))?
                    .pending
                    .is_some_and(|pending| pending.aborting);
                let revision = if repair {
                    system.resume_with_repair()?
                } else {
                    system.resume()?
                };
                if let Some(revision) = revision {
                    if aborting {
                        println!(
                            "Aborted the live transaction; package revision {revision} remains."
                        );
                    } else {
                        println!("Recovered the live transaction at package revision {revision}.");
                    }
                } else {
                    println!("No pending transaction.");
                }
            }
        },
        Command::Abort { target, fetch } => {
            let revision = if let Some(root) = target.root {
                abort_transaction(&root, fetch)?
            } else {
                let system = System::load(&target.config)?;
                let _lock = system.lock()?;
                system.abort(fetch)?
            };
            if let Some(revision) = revision {
                println!("Aborted the pending transaction");
                println!("Image revision {revision}");
            } else {
                println!("No pending transaction");
            }
        }
        Command::History {
            image,
            limit,
            json,
            plan,
        } => {
            if let Some(plan) = plan {
                print_json(&transaction_details(&image.root, &plan)?)?;
            } else {
                let history = transaction_history(&image.root, limit)?;
                if json {
                    print_json(&history)?;
                } else {
                    println!("REVISION\tPLAN\tOPERATION");
                    for entry in history {
                        let operation = entry.details.request.operation();
                        println!("{}\t{}\t{operation}", entry.revision, entry.plan_id);
                    }
                }
            }
        }
        Command::List {
            image,
            available,
            all_versions,
            intent,
            frozen,
            json,
        } => {
            let root = image.root;
            if frozen {
                let freezes = list_freezes(&root)?;
                if json {
                    print_json(&freezes)?;
                } else {
                    println!("PACKAGE\tVERSION\tREASON");
                    for (name, freeze) in freezes {
                        println!(
                            "{name}\t{}\t{:?}",
                            freeze.version,
                            freeze.reason.as_deref().unwrap_or("")
                        );
                    }
                }
            } else if intent {
                let rows = list_package_intent(&root)?;
                if json {
                    print_json(&rows)?;
                } else {
                    let mut out = std::io::stdout().lock();
                    writeln!(out, "PACKAGE\tLOCAL REQUEST\tPROFILE")?;
                    for row in rows {
                        let request = if row.requested { "yes" } else { "-" };
                        writeln!(out, "{}\t{request}\t{}", row.id, row.profile)?;
                    }
                }
            } else {
                let scope = match (available, all_versions) {
                    (true, true) => Scope::AllAvailableVersions,
                    (true, false) => Scope::Available,
                    _ => Scope::Installed,
                };
                let rows = list_packages(&root, scope)?;
                if json {
                    print_json(&rows)?;
                } else {
                    print_packages(rows, false)?;
                }
            }
        }
        Command::Search {
            image,
            file: true,
            json,
            term,
            ..
        } => {
            let matches = search_files(&image.root, &term)?;
            ensure!(!matches.is_empty(), "no available package ships {term:?}");
            if json {
                print_json(&matches)?;
            } else {
                let mut out = std::io::stdout().lock();
                for matched in matches {
                    writeln!(out, "{}\t{}", matched.package, matched.path)?;
                }
            }
        }
        Command::Search {
            image,
            installed,
            file: false,
            json,
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
            let rows: Vec<_> = list_packages(&image.root, scope)?
                .into_iter()
                .filter(|row| {
                    row.name.contains(&term)
                        || row
                            .summary
                            .as_ref()
                            .is_some_and(|summary| summary.to_lowercase().contains(&term))
                })
                .collect();
            if json {
                print_json(&rows)?;
            } else {
                print_packages(rows, true)?;
            }
        }
        Command::Policy {
            image,
            json,
            probe,
            package,
        } => {
            let policy = cps::query::package_policy(&image.root, &package, probe)?;
            if json {
                print_json(&policy)?;
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
            image,
            available,
            package,
        } => {
            let manifest = package_manifest(&image.root, &package, available)?;
            std::io::stdout().lock().write_all(manifest.as_bytes())?;
        }
        Command::Owner { image, json, path } => {
            print_lines(path_owners(&image.root, &path)?, json)?
        }
        Command::Contents {
            image,
            json,
            package,
        } => print_lines(package_contents(&image.root, &package)?, json)?,
        Command::Deps {
            image,
            json,
            package,
        } => print_lines(direct_dependencies(&image.root, &package)?, json)?,
        Command::Rdeps {
            image,
            json,
            package,
        } => print_lines(reverse_dependencies(&image.root, &package)?, json)?,
        Command::Why { image, package } => print_why(&image.root, &package)?,
        Command::Verify {
            image,
            pristine,
            packages,
            paths,
            quick,
            runtime,
            library_dirs,
        } => {
            let root = image.root;
            let findings = if pristine {
                verify_pristine(&root)?
            } else if !packages.is_empty() || !paths.is_empty() {
                cps::verify_selected(&root, &packages, &paths, quick)?
            } else if quick {
                cps::verify_quick(&root)?
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
        Command::Publisher { command } => return run_publisher(command),
        Command::Image { command } => run_image(command)?,
        Command::Boot { command } => return run_boot(command),
        Command::Source { command } => run_source(command)?,
        Command::Recipe { command } => return run_recipe(command),
        Command::Manifest { command } => return run_manifest(command),
        Command::Build {
            mut inputs,
            output,
            check_reproducible,
            key,
            recipe,
        } => {
            let inputs = inputs.prepare()?;
            let ids = build_with_recipe_options(
                &recipe,
                inputs.toolchain,
                BuildSettings {
                    sources: inputs.sources.0,
                    output: &output,
                    check_reproducible,
                    builder_signing_key: key.as_deref(),
                },
                inputs.sources.1,
                &inputs.worker,
                &inputs.overrides,
            )?;
            for id in ids {
                println!("Built {id} at {}", output.display());
            }
        }
        Command::Develop {
            mut inputs,
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
        Command::Publish { command } => run_publish(command)?,
        Command::Repo { command } => run_repo(command)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn print_image_status(root: &Path) -> Result<()> {
    let status = transaction_status(root)?;
    let Some(pending) = status.pending else {
        println!("Image revision {}", status.revision);
        println!("No pending transaction");
        return Ok(());
    };
    println!("{}", pending.plan_summary);
    println!(
        "Pending plan {} targets revision {}",
        pending.plan_id, pending.target_revision
    );
    let option = if pending.live { "" } else { " --root IMAGE" };
    if pending.aborting {
        println!("Abort in progress");
        println!("Finish with cps abort{option}");
    } else {
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
        println!("Recover with cps resume{option}, or undo with cps abort{option}");
    }
    Ok(())
}

fn print_system_status(system: &System) -> Result<()> {
    let (running, entries) = system.boot_status()?;
    let status = transaction_status(Path::new("/"))?;
    println!("Running {running}, package revision {}", status.revision);
    if let Some(pending) = status.pending {
        if pending.aborting {
            println!(
                "Transaction {} is being aborted. Run cps abort.",
                pending.plan_id
            );
        } else {
            println!(
                "Transaction {} is incomplete. Run cps resume, or cps abort to undo it.",
                pending.plan_id
            );
        }
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
    Ok(())
}

fn print_why(root: &Path, package: &str) -> Result<()> {
    let explanation = explain_package(root, package)?;
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
            (true, cps::query::ProfileRole::Root) => "local request and signed profile root",
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
    Ok(())
}

fn run_publisher(command: PublisherCommand) -> Result<ExitCode> {
    match command {
        PublisherCommand::List { image } => {
            println!("PUBLISHER\tREPOSITORY");
            for publisher in list_publishers(&image.root)? {
                println!("{}\t{}", publisher.name, publisher.repository.display());
            }
        }
        PublisherCommand::Add {
            image,
            repo,
            url,
            tuf_root,
            cache,
            trust_root,
            build_trust,
            expect_root_key,
            name,
        } => {
            let root = image.root;
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
                    if !confirm(false, "Trust this publisher?")? {
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
        PublisherCommand::SetRepository { image, repo } => {
            set_publisher_repositories(&image.root, &repo)?;
        }
        PublisherCommand::SetBuildTrust {
            image,
            policy,
            future_only,
            name,
        } => {
            set_publisher_build_trust(&image.root, &name, &policy, future_only)?;
            if future_only {
                println!("Configured build trust for future packages from {name}");
            } else {
                println!("Configured build trust for {name}");
            }
        }
        PublisherCommand::AuditBuilds { image, name } => {
            let audit = audit_publisher_builds(&image.root, &name)?;
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
        PublisherCommand::Prefer { image, name } => prefer_publisher(&image.root, &name)?,
        PublisherCommand::Remove { image, name } => remove_publisher(&image.root, &name)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn run_image(command: ImageCommand) -> Result<()> {
    match command {
        ImageCommand::Diff {
            paths,
            json,
            before,
            after,
        } => {
            print_image_diff(&cps::image_diff(&before, &after, paths)?, json)?;
        }
        ImageCommand::Lock { image, export } => {
            if let Some(output) = export {
                cps::export_image_bundle(&image.root, &output)?;
                println!("Exported image bundle to {}", output.display());
            } else {
                print_json(&cps::capture_image_lock(&image.root)?)?;
            }
        }
        ImageCommand::Create {
            root,
            config,
            lock,
            bundle,
            bootstrap_catalog,
            primary_repo,
            repo,
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
                (None, None, None) => {
                    let trust_root = trust_root.context("missing trust root")?;
                    create_image(
                        &root,
                        &repo.context("missing repository")?,
                        &pinned_publisher(&trust_root)?,
                        &trust_root,
                        arch.as_deref().unwrap_or(std::env::consts::ARCH),
                        parse_policy(facet, with_dependencies, variant)?,
                    )?
                }
                _ => anyhow::bail!("select either --lock or --bundle with --config"),
            };
            println!("Created image {id} at {}", root.display());
        }
        ImageCommand::Disk {
            btrfs_volume,
            root,
            environments,
            var,
            disk,
            root_partuuid,
            output,
            kernel,
            initrd,
            options,
        } => {
            let id = create_boot_image(BootImage {
                volume: &btrfs_volume,
                root: &root,
                environments: &environments,
                var: var.as_deref(),
                disk: &disk,
                root_partuuid: &root_partuuid,
                output: &output,
                kernel,
                initrd,
                options,
            })?;
            println!("Wrote {id} to {}", output.display());
        }
    }
    Ok(())
}

fn run_boot(command: BootCommand) -> Result<ExitCode> {
    match command {
        BootCommand::List { config, esp } => {
            let esp = match esp {
                Some(esp) => esp,
                None => System::load(&config)?.boot_paths().0.to_owned(),
            };
            let environments = list_boot_environments(&esp)?;
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
        BootCommand::Destroy {
            location,
            dry_run,
            id,
        } => {
            let (esp, volume) = location.resolve()?;
            let id = destroy_btrfs_boot_environment(&esp, &volume, &id, dry_run)?;
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
            location,
            dry_run,
            paths,
            json,
            id,
        } => {
            let (esp, volume) = location.resolve()?;
            if dry_run {
                let (target, diff) =
                    cps::preview_btrfs_rollback(&esp, &volume, id.as_deref(), paths)?;
                if json {
                    print_json(&serde_json::json!({"target": target, "diff": diff}))?;
                } else {
                    println!("Would select {target} for the next boot");
                    print_image_diff(&diff, false)?;
                }
                return Ok(ExitCode::SUCCESS);
            }
            let id = rollback_btrfs_boot_environment(&esp, &volume, id.as_deref())?;
            println!("Selected {id} for the next boot");
        }
        BootCommand::Activate {
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
            if !known_good {
                system::arm_finalize();
            }
            println!("Activated {id}");
        }
        BootCommand::Bless { config } => {
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
        BootCommand::FinalizeTrial { config, arm } => {
            let system = System::load(&config)?;
            let _lock = system.lock()?;
            if arm {
                match system.arm_finalize()? {
                    Some(id) => println!("The /etc merge into {id} runs at shutdown"),
                    None => println!("No trial is selected for the next boot"),
                }
                return Ok(ExitCode::SUCCESS);
            }
            match system.finalize_trial()? {
                system::Finalized::NoTrial => {
                    println!("No trial is selected for the next boot")
                }
                system::Finalized::NoBase { trial } => {
                    println!("Trial {trial} has no /etc merge base; its /etc stays as staged")
                }
                system::Finalized::Merged { trial, report } => print_etc_merge(&trial, &report),
            }
        }
        BootCommand::Capture {
            source,
            output,
            exclude_subvolume,
        } => {
            let id = capture_boot_environment(&source, &output, &exclude_subvolume)?;
            println!("Captured {id} at {}", output.display());
        }
        BootCommand::Stage {
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
    }
    Ok(ExitCode::SUCCESS)
}

fn run_source(command: SourceCommand) -> Result<()> {
    match command {
        SourceCommand::GitSnapshot {
            git,
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
            let (package, commit) =
                prepare_git_snapshot_ref(&recipe, &git, source, &repo, &trust_root, &output)?;
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
        SourceCommand::FetchArchived {
            repo,
            trust_root,
            cache,
            catalog,
            package_id,
        } => {
            let count = fetch_build_sources(
                &repo,
                &pinned_publisher(&trust_root)?,
                &trust_root,
                &package_id,
                catalog.as_deref(),
                &cache,
            )?;
            println!("Verified {count} archived source object(s) for {package_id}");
        }
    }
    Ok(())
}

fn run_manifest(command: ManifestCommand) -> Result<ExitCode> {
    match command {
        ManifestCommand::Generate {
            proto,
            publisher,
            version,
            arch,
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
                arch.as_deref(),
            )?;
            std::io::stdout().lock().write_all(text.as_bytes())?;
        }
        ManifestCommand::Dependencies {
            proto,
            config,
            catalogs,
            path_dirs,
            check,
            manifest,
        } => {
            let inspect = if check {
                cps::check_manifest_dependencies
            } else {
                suggest_dependencies
            };
            let (suggestions, diagnostics) =
                inspect(&proto, &manifest, &config, &catalogs, &path_dirs)?;
            std::io::stdout().lock().write_all(suggestions.as_bytes())?;
            if !diagnostics.is_empty() {
                for diagnostic in diagnostics {
                    eprintln!("cps: {diagnostic}");
                }
                return Ok(ExitCode::FAILURE);
            }
        }
        ManifestCommand::Generated { proto, manifest } => {
            let text = cps::generated_dependencies(&proto, &manifest)?;
            std::io::stdout().lock().write_all(text.as_bytes())?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_recipe(command: RecipeCommand) -> Result<ExitCode> {
    match command {
        RecipeCommand::Order { explain, paths } => {
            let order = cps::orchestrate::build_order(&paths)?;
            for warning in &order.warnings {
                eprintln!("cps: {warning}");
            }
            for queued in order.recipes {
                println!("{}", queued.recipe.display());
                if explain {
                    for (recipe, reason) in queued.after {
                        println!("    after {}: {reason}", recipe.display());
                    }
                }
            }
        }
        RecipeCommand::Rebuilds {
            config,
            catalogs,
            changed,
            recipes,
        } => {
            let plan = cps::orchestrate::rebuild_plan(&config, &catalogs, &changed, &recipes)?;
            for warning in &plan.warnings {
                eprintln!("cps: {warning}");
            }
            for change in plan.changed {
                match change.previous {
                    Some(previous) => println!("changed {} from {previous}", change.id),
                    None => println!("changed {}", change.id),
                }
                for name in change.removed {
                    println!("    removes {name}");
                }
                for name in change.added {
                    println!("    adds {name}");
                }
            }
            for rebuild in plan.rebuilds {
                let kind = if rebuild.breaks { "breaks" } else { "rebuild" };
                println!("{kind} {}", rebuild.id);
                for reason in rebuild.reasons {
                    println!("    {reason}");
                }
                if let Some(recipe) = rebuild.recipe {
                    println!("    recipe {}", recipe.display());
                }
            }
            for recipe in plan.recipes {
                println!("recipe {}", recipe.display());
            }
        }
        RecipeCommand::FetchToolchain {
            toolchain_config,
            recipe,
        } => {
            let count = fetch_toolchain(&recipe, &toolchain_config)?;
            println!("Verified {count} toolchain payload(s)");
        }
        RecipeCommand::Repin {
            catalogs,
            trust_roots,
            manifests,
            local_sources,
            sources,
            dry_run,
            check,
            paths,
        } => {
            let request = cps::orchestrate::RepinRequest {
                catalogs,
                trust_roots,
                manifests,
                local_sources,
                sources,
            };
            let report = cps::orchestrate::repin_recipes(&paths, &request, !(dry_run || check))?;
            for warning in &report.warnings {
                eprintln!("cps: {warning}");
            }
            let verb = if check {
                "stale"
            } else if dry_run {
                "would repin"
            } else {
                "repinned"
            };
            let mut stale = 0;
            for recipe in &report.recipes {
                for change in &recipe.changes {
                    stale += 1;
                    println!(
                        "{verb} {} {} {} -> {}",
                        recipe.recipe.display(),
                        change.pin,
                        change.from,
                        change.to
                    );
                }
                if let Some(hash) = &recipe.recipe_sha256
                    && !check
                {
                    println!("recipe {} sha256 {hash}", recipe.recipe.display());
                }
            }
            if check && stale > 0 {
                eprintln!(
                    "cps: {stale} pin(s) in {} recipe(s) are stale; run cps recipe repin without --check to update them",
                    report.recipes.len()
                );
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_publish(command: PublishCommand) -> Result<()> {
    match command {
        PublishCommand::Manifests {
            repo,
            proto,
            key,
            manifests,
        } => {
            for id in cps::publish_batch(&repo, &proto, &manifests, &key)? {
                println!("Published {id}");
            }
        }
        PublishCommand::Bundle {
            repo,
            key,
            builder_key,
            recipe_sha256,
            witness,
            source_cache,
            bundle,
        } => {
            for id in publish_witnessed_bundle_with_sources(
                &repo,
                &bundle,
                &key,
                &builder_key,
                &recipe_sha256,
                &witness,
                source_cache.as_deref(),
            )? {
                println!("Published {id}");
            }
        }
        PublishCommand::Release {
            repo,
            key,
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
                &key,
                &release,
                source_cache.as_deref(),
                mode,
            )?;
            println!(
                "{} catalog {hash}",
                if prepare { "Prepared" } else { "Published" }
            );
        }
    }
    Ok(())
}

fn run_repo(command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::Trust { command } => run_trust(command)?,
        RepoCommand::WidenRollout {
            repo,
            key,
            base,
            profile,
            percentage,
        } => {
            let publisher = repository_publisher(&repo)?;
            let hash = cps::widen_rollout(&repo, &publisher, &key, &base, &profile, percentage)?;
            println!("Published catalog {hash}");
        }
        RepoCommand::PruneWithdrawals {
            repo,
            key,
            base,
            through,
        } => {
            let publisher = repository_publisher(&repo)?;
            let (hash, pruned) = cps::prune_withdrawals(&repo, &publisher, &key, &base, through)?;
            println!("Pruned {pruned} withdrawals; catalog {hash}");
        }
        RepoCommand::IndexFiles { repo, key } => {
            let publisher = repository_publisher(&repo)?;
            println!(
                "Catalog {}",
                cps::enable_file_index(&repo, &publisher, &key)?
            );
        }
        RepoCommand::Resign { repo, key, base } => {
            let publisher = repository_publisher(&repo)?;
            println!(
                "Published catalog {}",
                cps::resign_catalog(&repo, &publisher, &key, &base)?
            );
        }
        RepoCommand::Obsoletions { repo, trust_root } => {
            for id in list_obsoletions(&repo, &pinned_publisher(&trust_root)?, &trust_root)? {
                println!("{id}");
            }
        }
        RepoCommand::Attestation {
            repo,
            trust_root,
            package_id,
        } => {
            let bytes = read_attestation(
                &repo,
                &pinned_publisher(&trust_root)?,
                &trust_root,
                &package_id,
            )?;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&bytes)?;
            writeln!(stdout)?;
        }
        RepoCommand::RebuildInputs {
            repo,
            trust_root,
            catalog,
            output,
            package_id,
        } => {
            let recipe = export_rebuild_inputs(
                &repo,
                &pinned_publisher(&trust_root)?,
                &trust_root,
                &package_id,
                catalog.as_deref(),
                &output,
            )?;
            println!("{}", recipe.display());
        }
        RepoCommand::VerifyRebuild {
            repo,
            trust_root,
            catalog,
            bundle,
            witness_key,
            package_id,
        } => {
            let count = verify_rebuild(
                &repo,
                &pinned_publisher(&trust_root)?,
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
            key,
            source_cache,
            package_id,
        } => {
            let (catalog, count) = archive_build_sources(&repo, &key, &source_cache, &package_id)?;
            println!("Archived {count} source object(s) for {package_id} in catalog {catalog}");
        }
        RepoCommand::Withdraw {
            repo,
            key,
            package_id,
        } => {
            withdraw_package(&repo, &key, &package_id)?;
            println!("Withdrawn {package_id}");
        }
        RepoCommand::Withdrawals { repo, trust_root } => {
            for (id, hash) in list_withdrawals(&repo, &pinned_publisher(&trust_root)?, &trust_root)?
            {
                println!("{id}\t{hash}");
            }
        }
        RepoCommand::Export {
            repo,
            trust_root,
            output,
            retained,
            delta_from,
            include_sources,
            archive_catalog,
        } => {
            let publisher = pinned_publisher(&trust_root)?;
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
                release::export_with_sources(&repo, &publisher, &trust_root, &output, &retained)?
            } else {
                release::export_with_retained(&repo, &publisher, &trust_root, &output, &retained)?
            };
            println!("Exported {hash}");
        }
        RepoCommand::SignExport {
            tuf_root,
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
            root: &tuf_root,
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
            tuf_root,
            snapshot,
            timestamp_key,
            snapshot_key,
            snapshot_expires,
            version,
            timestamp_expires,
            output,
        } => release::refresh_metadata(release::RefreshMetadata {
            root: &tuf_root,
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
            tuf_root,
            mirror,
            trust_root,
            index_only,
            image,
            required_catalogs,
            archive_catalog,
        } => {
            let publisher = pinned_publisher(&trust_root)?;
            let (metadata, targets) = mirror.resolve();
            let hash = if let Some(catalog) = archive_catalog {
                release::sync_archive(
                    &repo,
                    &tuf_root,
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
                    &tuf_root,
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
            trust_root,
            catalogs,
            images,
            plans,
            locks,
            system_config,
            dry_run,
        } => {
            let publisher = pinned_publisher(&trust_root)?;
            let apply = !dry_run;
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
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser, error::ErrorKind};

    use super::Cli;

    /// The full command tree needs more stack than a test thread has.
    fn with_stack(test: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(test)
            .unwrap()
            .join()
            .unwrap();
    }

    fn parse(args: &[&str]) -> Result<(), clap::Error> {
        Cli::try_parse_from(std::iter::once("cps").chain(args.iter().copied())).map(drop)
    }

    #[test]
    fn command_line_is_consistent() {
        with_stack(|| Cli::command().debug_assert());
    }

    #[test]
    fn builder_signing_requires_a_reproducibility_check() {
        with_stack(builder_signing);
    }

    fn builder_signing() {
        let build = |extra: &[&str]| {
            let mut args = vec![
                "build",
                "--toolchain-config",
                "t.kdl",
                "--sources",
                "s",
                "--output",
                "o",
                "--key",
                "k.der",
            ];
            args.extend_from_slice(extra);
            args.push("r.kdl");
            parse(&args)
        };
        let error = build(&[])
            .err()
            .expect("signing without a check was accepted");
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        assert!(build(&["--check-reproducible"]).is_ok());
    }

    #[test]
    fn build_publish_and_generate_flags_have_help() {
        with_stack(flags_have_help);
    }

    fn flags_have_help() {
        let mut cli = Cli::command();
        for path in [
            &["build"][..],
            &["publish", "manifests"],
            &["publish", "bundle"],
            &["manifest", "generate"],
        ] {
            let mut command = &mut cli;
            for name in path {
                command = command.find_subcommand_mut(name).unwrap();
            }
            for arg in command.get_arguments() {
                if !matches!(arg.get_id().as_str(), "help" | "version") {
                    assert!(
                        arg.get_help().is_some(),
                        "{path:?} {} has no help",
                        arg.get_id()
                    );
                }
            }
        }
    }

    #[test]
    fn boot_help_describes_every_option() {
        with_stack(boot_help);
    }

    fn boot_help() {
        let mut command = Cli::command();
        for (path, args) in [
            (["boot", "list"], ["esp", "config"].as_slice()),
            (
                ["boot", "destroy"],
                &["id", "esp", "btrfs_volume", "dry_run"],
            ),
            (
                ["boot", "rollback"],
                &["esp", "btrfs_volume", "dry_run", "paths", "json"],
            ),
            (
                ["image", "disk"],
                &[
                    "btrfs_volume",
                    "root",
                    "environments",
                    "disk",
                    "root_partuuid",
                    "output",
                    "kernel",
                    "initrd",
                    "options",
                ],
            ),
        ] {
            let subcommand = command
                .find_subcommand_mut(path[0])
                .unwrap()
                .find_subcommand(path[1])
                .unwrap();
            for arg in args.iter() {
                let arg = subcommand
                    .get_arguments()
                    .find(|candidate| candidate.get_id() == *arg)
                    .unwrap();
                assert!(
                    arg.get_help().is_some(),
                    "{path:?} {} has no help",
                    arg.get_id()
                );
            }
        }
    }

    #[test]
    fn rollback_preview_options_require_dry_run() {
        with_stack(rollback_preview);
    }

    fn rollback_preview() {
        let error = parse(&["boot", "rollback", "--paths"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        parse(&["boot", "rollback", "--dry-run", "--paths"]).unwrap();
        parse(&[
            "boot",
            "rollback",
            "--esp",
            "/efi",
            "--btrfs-volume",
            "/v",
            "-n",
            "--json",
        ])
        .unwrap();
    }

    #[test]
    fn live_only_options_reject_an_offline_image() {
        with_stack(live_only_options);
    }

    fn live_only_options() {
        for args in [
            ["install", "--root", "img", "--stage", "pkg"].as_slice(),
            &["install", "--root", "img", "--no-refresh", "pkg"],
            &["upgrade", "--root", "img", "--base-only"],
            &["upgrade", "--root", "img", "--check"],
            &["install", "--root", "img", "--config", "c.kdl", "pkg"],
            &["install", "--root", "img", "--git", "u", "--commit", "c"],
        ] {
            let error = parse(args).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{args:?}");
        }
        parse(&["install", "--root", "img", "-n", "--save-plan", "p", "pkg"]).unwrap();
        parse(&["install", "--root", "img", "--profile", "-y"]).unwrap();
        parse(&["freeze", "--root", "img", "--reason", "why", "pkg@1"]).unwrap();
        parse(&["upgrade", "--check", "--json", "--explain", "pkg"]).unwrap();
    }

    #[test]
    fn install_needs_packages_or_another_source() {
        with_stack(install_sources);
    }

    fn install_sources() {
        let error = parse(&["install"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        let error = parse(&["install", "--profile", "pkg"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
        parse(&["install", "--lock", "l.json", "--root", "img"]).unwrap();
    }

    #[test]
    fn repo_sync_takes_a_base_url_or_both_directories() {
        with_stack(repo_sync_urls);
    }

    fn repo_sync_urls() {
        let sync = |extra: &[&str]| {
            let mut args = vec![
                "repo",
                "sync",
                "--repo",
                "r",
                "--tuf-root",
                "root.json",
                "--trust-root",
                "trust.json",
            ];
            args.extend_from_slice(extra);
            parse(&args)
        };
        sync(&["--url", "https://m.example/"]).unwrap();
        sync(&["--metadata", "file:///m/", "--targets", "file:///t/"]).unwrap();
        assert!(sync(&[]).is_err());
        assert!(sync(&["--metadata", "file:///m/"]).is_err());
        assert!(sync(&["--url", "https://m/", "--metadata", "m", "--targets", "t"]).is_err());
    }
}
