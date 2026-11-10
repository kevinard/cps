//! Strip ELF outputs and split their debug information into the same package.
//!
//! CPS selects the ELF executables and shared objects that an output ships,
//! then runs `objcopy` from the pinned toolchain inside the build sandbox. The
//! debug information moves to `usr/lib/debug/.build-id/xx/yyyy.debug`, where
//! `xxyyyy` is the GNU build ID, and ships as a file action with facet
//! `debug.symbols` in the package that ships the stripped object. `objcopy`
//! keeps the build ID note and the section order, so a reproducible build stays
//! reproducible.

use std::{collections::BTreeMap, fs, io::Read};

use anyhow::{Context, Result, ensure};
use object::{Object, ObjectSection, elf};

use crate::{
    image::root::Root,
    model::{Action, Manifest},
};

use super::expand::{DEBUG_FACET, debug_path};

/// One ELF object to strip.
pub(crate) struct Split {
    pub path: String,
    /// The debug file to create, or none when another object already provides it.
    pub debug: Option<String>,
}

fn machine(arch: &str) -> Option<u16> {
    match arch {
        "x86_64" => Some(elf::EM_X86_64),
        "aarch64" => Some(elf::EM_AARCH64),
        _ => None,
    }
}

/// The GNU build ID of an unstripped native executable or shared object.
fn inspect(bytes: &[u8], arch: &str, path: &str) -> Result<Option<Vec<u8>>> {
    if !bytes.starts_with(b"\x7fELF") {
        return Ok(None);
    }
    let Ok(file) = object::File::parse(bytes) else {
        return Ok(None);
    };
    let header_type = match &file {
        object::File::Elf64(file) => file.elf_header().e_type.get(file.endian()),
        object::File::Elf32(file) => file.elf_header().e_type.get(file.endian()),
        _ => return Ok(None),
    };
    let machine_type = match &file {
        object::File::Elf64(file) => file.elf_header().e_machine.get(file.endian()),
        object::File::Elf32(file) => file.elf_header().e_machine.get(file.endian()),
        _ => return Ok(None),
    };
    if !matches!(header_type, elf::ET_EXEC | elf::ET_DYN) || Some(machine_type) != machine(arch) {
        return Ok(None);
    }
    let debug = file.sections().any(|section| {
        section.name().is_ok_and(|name| {
            name == ".symtab" || name.starts_with(".debug_") || name.starts_with(".zdebug_")
        })
    });
    if !debug {
        return Ok(None);
    }
    let id = file
        .build_id()
        .ok()
        .flatten()
        .filter(|id| !id.is_empty())
        .with_context(|| {
            format!("{path} has symbols but no GNU build ID; link it with -Wl,--build-id or set strip #false in the recipe")
        })?;
    Ok(Some(id.to_vec()))
}

/// Choose the shipped ELF objects to strip and the debug file for each build ID.
/// Returns the strip plan and, for each output index, its debug file actions.
pub(crate) fn plan(
    root: &Root,
    outputs: &[Manifest],
    arch: &str,
) -> Result<(Vec<Split>, Vec<Vec<Action>>)> {
    let mut splits = Vec::new();
    let mut debug_actions = vec![Vec::new(); outputs.len()];
    let mut owners = BTreeMap::<Vec<u8>, usize>::new();
    for (index, manifest) in outputs.iter().enumerate() {
        for action in &manifest.action {
            let Action::File {
                path,
                uid,
                gid,
                variants,
                ..
            } = action
            else {
                continue;
            };
            if path.starts_with("usr/lib/debug/") {
                continue;
            }
            // Read only the magic number of files that cannot be ELF objects.
            let mut file = root.file(path)?;
            let mut bytes = Vec::new();
            file.by_ref().take(4).read_to_end(&mut bytes)?;
            if bytes != b"\x7fELF" {
                continue;
            }
            file.read_to_end(&mut bytes)?;
            let Some(id) = inspect(&bytes, arch, path)? else {
                continue;
            };
            let debug = match owners.get(&id) {
                Some(&owner) => {
                    ensure!(
                        owner == index,
                        "{} and {} ship ELF objects with the same build ID; their debug information can belong to only one output",
                        outputs[owner].name,
                        manifest.name
                    );
                    None
                }
                None => {
                    owners.insert(id.clone(), index);
                    let debug = debug_path(&id);
                    debug_actions[index].push(Action::File {
                        path: debug.clone(),
                        mode: "0644".into(),
                        uid: *uid,
                        gid: *gid,
                        preserve: false,
                        facet: Some(DEBUG_FACET.into()),
                        variants: variants.clone(),
                        sha256: None,
                        size: None,
                        capabilities: None,
                        original_name: None,
                    });
                    Some(debug)
                }
            };
            splits.push(Split {
                path: path.clone(),
                debug,
            });
        }
    }
    Ok((splits, debug_actions))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The sandbox script that splits and strips each planned object under `/out`.
pub(crate) fn script(splits: &[Split]) -> String {
    let mut script = String::from(
        "command -v objcopy >/dev/null || { echo 'cps: stripping needs objcopy in the build toolchain; add binutils or set strip #false in the recipe' >&2; exit 1; }\n",
    );
    for split in splits {
        let object = quote(&format!("/out/{}", split.path));
        if let Some(debug) = &split.debug {
            let debug = format!("/out/{debug}");
            let directory = debug.rsplit_once('/').map_or("/out", |(parent, _)| parent);
            script.push_str(&format!(
                "mkdir -p {}\nobjcopy --only-keep-debug {object} {}\n",
                quote(directory),
                quote(&debug)
            ));
        }
        script.push_str(&format!("objcopy --strip-unneeded {object}\n"));
    }
    script
}

/// Check that each planned debug file exists, and give it a fixed mode.
pub(crate) fn finish(output: &std::path::Path, splits: &[Split]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for debug in splits.iter().filter_map(|split| split.debug.as_ref()) {
        let path = output.join(debug);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("objcopy did not create {debug}"))?;
        ensure!(metadata.is_file(), "{debug} is not a regular file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_script_quotes_paths_and_creates_build_id_directories() {
        let script = script(&[
            Split {
                path: "usr/bin/it's".into(),
                debug: Some(debug_path(&[0x12, 0x34])),
            },
            Split {
                path: "usr/bin/copy".into(),
                debug: None,
            },
        ]);
        assert!(script.contains("mkdir -p '/out/usr/lib/debug/.build-id/12'\n"));
        assert!(script.contains(
            "objcopy --only-keep-debug '/out/usr/bin/it'\\''s' '/out/usr/lib/debug/.build-id/12/34.debug'\n"
        ));
        assert!(script.contains("objcopy --strip-unneeded '/out/usr/bin/copy'\n"));
        assert_eq!(script.matches("--only-keep-debug").count(), 1);
    }

    #[test]
    fn only_native_unstripped_objects_with_build_ids_are_split() -> Result<()> {
        assert_eq!(inspect(b"#!/bin/sh\n", "x86_64", "script")?, None);
        assert_eq!(inspect(b"\x7fELF garbage", "x86_64", "broken")?, None);
        let own = fs::read(std::env::current_exe()?)?;
        let arch = std::env::consts::ARCH;
        let file = object::File::parse(own.as_slice())?;
        let has_symbols = file
            .sections()
            .any(|section| section.name().is_ok_and(|name| name == ".symtab"));
        match (has_symbols, file.build_id()?) {
            (true, Some(id)) => assert_eq!(inspect(&own, arch, "test")?, Some(id.to_vec())),
            (true, None) => assert!(inspect(&own, arch, "test").is_err()),
            (false, _) => assert_eq!(inspect(&own, arch, "test")?, None),
        }
        let other = if arch == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        assert_eq!(inspect(&own, other, "test")?, None);
        Ok(())
    }
}
