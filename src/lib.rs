//! CPS is a command-line program. The library exists only to share code between
//! the binary and its tests; nothing in it is a stable API.
#![warn(unreachable_pub)]

mod apply;
mod attestation;
mod boot;
mod build;
pub mod cli;
mod debug;
mod file_caps;
mod generate;
mod git_install;
mod image;
mod kdl_format;
mod model;
mod orchestrate;
mod plan;
mod query;
mod repo;
mod system;
mod systemd;
mod transport;
mod triggers;
mod verify;

pub(crate) use apply::{
    TransactionPhase, TransactionProgress, abort_transaction, apply, fetch_plan,
    fetch_plan_with_archives, repair_pending_transaction, resume_transaction, transaction_progress,
    transaction_status, validate_plan,
};
pub(crate) use boot::{
    Activation, BootEnvironment, BootImage, EtcMergeReport, capture_boot_environment,
    create_boot_image, destroy_btrfs_boot_environment, list_boot_environments,
    mount_boot_environment, preview_btrfs_rollback, rename_boot_environment,
    rollback_btrfs_boot_environment, saved_etc_versions, stage_btrfs_boot_environment,
    unmount_boot_environment,
};
pub(crate) use build::{
    BuildSettings, BuildToolchain, BuildWorker, GitSnapshotRef, SourceLayout, develop,
    fetch_toolchain, prepare_git_snapshot_ref,
    sources::{fetch_sources, fetch_sources_with_local, import_sources},
};
pub(crate) use debug::restore_debug;
pub(crate) use generate::{
    check_manifest_dependencies, generate_manifest_for_architecture, generated_dependencies,
    suggest_dependencies,
};
pub(crate) use image::diff::{ImageDiff, ImagePackage, image_diff};
pub(crate) use image::history::{HistoryEntry, transaction_details, transaction_history};
pub(crate) use image::lock::{
    ImageLock, capture_image_lock, create_image_from_bundle, create_image_from_lock,
    export_image_bundle, read_image_lock,
};
pub(crate) use image::publishers::{
    add_publisher_from_url, add_publisher_with_build_trust, audit_publisher_builds,
    list_publishers, prefer_publisher, remove_publisher, set_publisher_build_trust,
    set_publisher_repositories,
};
pub(crate) use image::{create_configured_image_at, create_image};
pub(crate) use kdl_format::worker::read_worker_config;
pub(crate) use model::ImagePolicy;
pub(crate) use plan::{
    ConfigChoice, FrozenPackage, MarkChoice, PackageChange, PhasedUpdateDeferred, Plan,
    PlanOptions, Request, config_diff_inputs, list_config_updates, list_freezes, plan,
    plan_with_options,
};
pub(crate) use repo::{
    BuildWitness, ReleaseAdvisory, ReleaseMode, TrustDraft, archive_build_sources, commit_trust,
    draft_trust, enable_file_index, export_rebuild_inputs, fetch_build_sources, init_trust,
    list_obsoletions, list_withdrawals, pinned_publisher, prune_withdrawals, publish_batch,
    publish_release_with_mode, publish_witnessed_bundle_with_sources, read_attestation, release,
    repository_publisher, resign_catalog, rotate_trust, show_trust, sign_trust, verify_rebuild,
    widen_rollout, withdraw_package,
};
pub(crate) use verify::{
    verify, verify_elf_runtime, verify_pristine, verify_quick, verify_selected,
};

// Conveniences that only the tests call.
#[cfg(test)]
pub(crate) use {
    boot::last_etc_merge,
    generate::generate_manifest,
    image::create_configured_image,
    image::publishers::add_publisher,
    model::PackageFacet,
    plan::plan_with_recommends,
    repo::{publish, publish_bundle, publish_release, publish_release_with_sources},
};

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "../tests/support/tuf.rs"]
mod test_tuf;
