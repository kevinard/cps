use std::{
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::{
    build::{BuildWorker, cgroup::CgroupLimits},
    repo::read_limited,
};

use super::{Fields, document, one, schema};

/// Read worker resource limits from an operator-owned KDL configuration.
pub fn read_worker_config(path: &Path) -> Result<BuildWorker> {
    let bytes = read_limited(File::open(path)?, 64 * 1024)?;
    parse(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("parsing build worker configuration {}", path.display()))
}

fn parse(text: &str) -> Result<BuildWorker> {
    let doc = document(text)?;
    schema(&doc, "build-worker")?;
    ensure!(
        doc.nodes().len() == 2,
        "expected schema-version and worker nodes"
    );
    let node = one(&doc, "worker")?;
    let fields = Fields::read(
        node,
        0,
        &[
            "timeout-seconds",
            "scratch-bytes",
            "file-bytes",
            "output-bytes",
            "log-bytes",
        ],
        true,
    )?;
    let cgroup = node
        .children()
        .map(|children| -> Result<CgroupLimits> {
            ensure!(
                children.nodes().len() == 1,
                "expected one cgroup node in the worker block"
            );
            let fields = Fields::read(
                one(children, "cgroup")?,
                1,
                &["cpu-percent", "memory-bytes", "tasks"],
                false,
            )?;
            let root = PathBuf::from(fields.string_arg(0)?);
            ensure!(root.is_absolute(), "cgroup root must be an absolute path");
            let cpu_percent = fields
                .integer("cpu-percent")?
                .context("missing cgroup cpu-percent")?;
            let memory_bytes = fields
                .integer("memory-bytes")?
                .context("missing cgroup memory-bytes")?;
            let tasks = fields.integer("tasks")?.context("missing cgroup tasks")?;
            ensure!(
                (1..=100_000).contains(&cpu_percent),
                "cgroup cpu-percent is out of range"
            );
            ensure!(
                (1..=i64::MAX as u64).contains(&memory_bytes),
                "cgroup memory-bytes is out of range"
            );
            ensure!(tasks > 0, "cgroup tasks must be positive");
            Ok(CgroupLimits {
                root,
                cpu_percent,
                memory_bytes,
                tasks,
            })
        })
        .transpose()?;
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
        cgroup,
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
