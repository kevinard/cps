use std::path::Path;

use anyhow::{Result, ensure};

use crate::{
    image::Image,
    model::{Node, digest, validate_path},
    plan::{check_dependencies, effective_paths},
};

#[derive(Debug)]
pub struct Finding {
    pub is_error: bool,
    pub message: String,
}

pub fn verify(path: &Path) -> Result<Vec<Finding>> {
    let image = Image::open(path, false)?;
    image.require_clean()?;
    let state = image.state()?;
    let mut findings = Vec::new();
    for (name, package) in &state.installed {
        let manifest = &package.published.manifest;
        manifest.validate(true)?;
        ensure!(
            &manifest.name == name && manifest.publisher == state.publisher.name,
            "invalid installed package identity for {name}"
        );
        ensure!(
            digest(&serde_json::to_vec(manifest)?) == package.published.sha256,
            "installed manifest digest mismatch for {name}"
        );
    }
    if let Err(error) = check_dependencies(&state.installed) {
        findings.push(Finding {
            is_error: true,
            message: error.to_string(),
        });
    }
    let expected = effective_paths(&state.settings, &state.installed)?;
    if expected != state.paths {
        findings.push(Finding {
            is_error: true,
            message: "path ownership index differs from installed manifests".into(),
        });
    }
    for (path, managed) in expected {
        validate_path(&path)?;
        let observed = match image.root.inspect(&path) {
            Ok(node) => node,
            Err(error) => {
                findings.push(Finding {
                    is_error: true,
                    message: format!("{path}: {error:#}"),
                });
                continue;
            }
        };
        if observed.as_ref() == Some(&managed.node)
            || (managed.implicit && observed.as_ref().is_some_and(Node::is_dir))
        {
            continue;
        }
        let reason = match (&managed.node, &observed) {
            (_, None) if managed.preserve => {
                findings.push(Finding {
                    is_error: false,
                    message: format!("locally removed config {path}"),
                });
                continue;
            }
            (
                Node::File { mode, uid, gid, .. },
                Some(Node::File {
                    mode: actual_mode,
                    uid: actual_uid,
                    gid: actual_gid,
                    ..
                }),
            ) if managed.preserve && (mode, uid, gid) == (actual_mode, actual_uid, actual_gid) => {
                findings.push(Finding {
                    is_error: false,
                    message: format!("locally modified config {path}"),
                });
                continue;
            }
            (_, None) => "missing",
            _ => "content, type, or metadata differs",
        };
        findings.push(Finding {
            is_error: true,
            message: format!("{path}: {reason}"),
        });
    }
    Ok(findings)
}
