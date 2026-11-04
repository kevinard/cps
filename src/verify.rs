use std::{collections::BTreeMap, path::Path};

use anyhow::Result;

use crate::{
    image::{Image, Managed, State, root::Root},
    model::{Node, validate_path},
    plan::{check_dependencies, effective_paths},
};

#[derive(Debug)]
pub struct Finding {
    pub is_error: bool,
    pub message: String,
}

pub fn verify(path: &Path) -> Result<Vec<Finding>> {
    verify_image(&Image::open(path, false)?)
}

pub(crate) fn verify_image(image: &Image) -> Result<Vec<Finding>> {
    image.require_clean()?;
    verify_state(&image.root, &image.state()?)
}

pub(crate) fn manifest_paths(state: &State) -> Result<BTreeMap<String, Managed>> {
    for (name, package) in &state.installed {
        package.verify(name, &state.publishers)?;
    }
    effective_paths(&state.settings, &state.installed)
}

pub(crate) fn verify_state(root: &Root, state: &State) -> Result<Vec<Finding>> {
    let expected = manifest_paths(state)?;
    let mut findings = Vec::new();
    if let Err(error) = check_dependencies(&state.installed) {
        findings.push(Finding {
            is_error: true,
            message: error.to_string(),
        });
    }
    if expected != state.paths {
        findings.push(Finding {
            is_error: true,
            message: "path ownership index differs from installed manifests".into(),
        });
    }
    for (path, managed) in expected {
        validate_path(&path)?;
        let observed = match root.inspect(&path) {
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
