use std::{fs::File, path::Path};

use anyhow::{Context, Result, ensure};

use crate::{build::BuildWorker, repo::read_limited};

use super::{Fields, document, one, schema};

/// Read worker resource limits from an operator-owned KDL configuration.
pub fn read_worker_config(path: &Path) -> Result<BuildWorker> {
    let bytes = read_limited(File::open(path)?, 64 * 1024)?;
    parse(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("parsing build worker configuration {}", path.display()))
}

fn parse(text: &str) -> Result<BuildWorker> {
    let doc = document(text)?;
    schema(&doc)?;
    ensure!(
        doc.nodes().len() == 2,
        "expected schema-version and worker nodes"
    );
    let fields = Fields::read(
        one(&doc, "worker")?,
        0,
        &[
            "timeout-seconds",
            "scratch-bytes",
            "file-bytes",
            "output-bytes",
            "log-bytes",
        ],
        false,
    )?;
    let defaults = BuildWorker::default();
    let worker = BuildWorker {
        timeout_secs: fields
            .integer("timeout-seconds")?
            .unwrap_or(defaults.timeout_secs),
        scratch_bytes: fields
            .integer("scratch-bytes")?
            .unwrap_or(defaults.scratch_bytes),
        file_bytes: fields.integer("file-bytes")?.unwrap_or(defaults.file_bytes),
        output_bytes: fields
            .integer("output-bytes")?
            .unwrap_or(defaults.output_bytes),
        log_bytes: fields.integer("log-bytes")?.unwrap_or(defaults.log_bytes),
    };
    ensure!(
        (1..=7 * 24 * 60 * 60).contains(&worker.timeout_secs),
        "timeout-seconds must be between one second and seven days"
    );
    for (name, bytes, minimum) in [
        ("scratch-bytes", worker.scratch_bytes, 4096),
        ("file-bytes", worker.file_bytes, 1),
        ("output-bytes", worker.output_bytes, 1),
        ("log-bytes", worker.log_bytes, 1),
    ] {
        ensure!(
            (minimum..=i64::MAX as u64).contains(&bytes),
            "{name} is out of range"
        );
    }
    Ok(worker)
}
