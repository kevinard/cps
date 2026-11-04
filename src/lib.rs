mod apply;
mod attestation;
mod boot;
mod build;
mod file_caps;
mod generate;
mod image;
mod kdl_format;
mod model;
mod plan;
pub mod query;
mod repo;
mod systemd;
mod transport;
mod verify;

pub use apply::{apply, fetch_plan};
pub use boot::{
    Activation, BootEnvironment, RootDeployment, activate_boot_environment,
    activate_boot_environment_on_disk, activate_deployed_boot_environment,
    activate_device_boot_environment, deploy_and_activate_boot_environment,
    deploy_boot_environment, export_boot_environment, export_boot_environment_receipt,
    list_boot_environments, list_boot_environments_on_disk, rollback_boot_environment,
    rollback_boot_environment_on_disk, stage_boot_environment,
};
pub use build::{
    BuildOptions, BuildSettings, BuildWorker, SourceLayout, build, build_cached_package,
    build_package, build_with_toolchain_config, build_with_worker,
    sources::{fetch_sources, fetch_sources_with_local, import_sources},
};
pub use generate::generate_manifest;
pub use image::publishers::{
    PublisherInfo, add_publisher, add_publisher_with_build_trust, audit_publisher_builds,
    list_publishers, prefer_publisher, remove_publisher, set_publisher_build_trust,
};
pub use image::{create_configured_image, create_image};
pub use kdl_format::worker::read_worker_config;
pub use kdl_format::{ImageConfig, read_image_config};
pub use plan::{ConfigChoice, ConfigUpdate, Plan, Request, list_config_updates, plan};
pub use repo::{
    BuildAudit, BuildWitness, list_obsoletions, list_withdrawals, publish, publish_bundle,
    publish_release, publish_witnessed_bundle, read_attestation, release, rotate_publisher_key,
    withdraw_package,
};
pub use verify::{Finding, verify, verify_pristine};

#[cfg(test)]
mod tests;
