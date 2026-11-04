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
    Activation, BootEnvironment, activate_boot_environment, export_boot_environment,
    list_boot_environments, rollback_boot_environment, stage_boot_environment,
};
pub use build::{
    BuildOptions, BuildSettings, BuildWorker, SourceLayout, build, build_cached_package,
    build_package, build_with_toolchain_config, build_with_worker,
    sources::{fetch_sources, fetch_sources_with_local, import_sources},
};
pub use generate::generate_manifest;
pub use image::publishers::{
    PublisherInfo, add_publisher, list_publishers, prefer_publisher, remove_publisher,
};
pub use image::{create_configured_image, create_image};
pub use kdl_format::worker::read_worker_config;
pub use kdl_format::{ImageConfig, read_image_config};
pub use plan::{Plan, Request, plan};
pub use repo::{
    BuildWitness, list_obsoletions, list_withdrawals, publish, publish_bundle, publish_release,
    publish_witnessed_bundle, read_attestation, release, rotate_publisher_key, withdraw_package,
};
pub use verify::{Finding, verify, verify_pristine};

#[cfg(test)]
mod tests;
