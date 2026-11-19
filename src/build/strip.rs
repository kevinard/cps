//! Strip ELF outputs and split their debug information into the same package.
//!
//! CPS selects the native ELF objects and static archives that an output ships,
//! then runs `objcopy` from the pinned toolchain inside the build sandbox. The
//! debug information moves to `usr/lib/debug/.build-id/xx/yyyy.debug`, where
//! `xxyyyy` is the GNU build ID, and ships as a file action with facet
//! `debug.symbols` in the package that ships the stripped object. `objcopy`
//! keeps the build ID note and the section order, so a reproducible build stays
//! reproducible. Relocatable objects and archives retain their linker symbols;
//! debug deltas live under `usr/lib/debug/` with their original paths plus
//! `.cps-debug`. Unchanged code and LTO sections remain in the ordinary file.
//! Signed kernel modules ship unchanged because their signature covers the
//! whole file, including debug sections.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
};

use anyhow::{Context, Result, ensure};
use object::{Object, ObjectSection, elf};

use crate::{
    image::root::Root,
    model::{Action, Manifest},
};

use super::expand::{DEBUG_FACET, debug_delta_path, debug_path};

/// One ELF object or archive to strip.
pub(crate) struct Split {
    pub path: String,
    /// The debug file to create, or none when another object already provides it.
    pub debug: Option<String>,
    /// Preserve code, relocations and linker symbols in the ordinary file.
    pub linkable: bool,
}

#[derive(Debug, PartialEq)]
enum Symbols {
    BuildId(Vec<u8>),
    Linkable,
}

/// The marker that ends a Linux module signature appended outside the ELF container.
const MODULE_SIGNATURE: &[u8] = b"~Module signature appended~\n";

fn machine(arch: &str) -> Option<u16> {
    match arch {
        "x86_64" => Some(elf::EM_X86_64),
        "aarch64" => Some(elf::EM_AARCH64),
        _ => None,
    }
}

fn native_elf<'a>(bytes: &'a [u8], arch: &str) -> Option<(object::File<'a>, u16)> {
    if !bytes.starts_with(b"\x7fELF") {
        return None;
    }
    let file = object::File::parse(bytes).ok()?;
    let header_type = match &file {
        object::File::Elf64(file) => file.elf_header().e_type.get(file.endian()),
        object::File::Elf32(file) => file.elf_header().e_type.get(file.endian()),
        _ => return None,
    };
    let machine_type = match &file {
        object::File::Elf64(file) => file.elf_header().e_machine.get(file.endian()),
        object::File::Elf32(file) => file.elf_header().e_machine.get(file.endian()),
        _ => return None,
    };
    (Some(machine_type) == machine(arch)).then_some((file, header_type))
}

fn has_debug(file: &object::File<'_>) -> bool {
    file.sections().any(|section| {
        section.name().is_ok_and(|name| {
            name.starts_with(".debug_")
                || name.starts_with(".zdebug_")
                || name.starts_with(".stab")
                || name.starts_with(".gnu.linkonce.wi.")
        })
    })
}

/// Choose a split strategy without mistaking required linker symbols for debug data.
fn inspect(bytes: &[u8], arch: &str, path: &str) -> Result<Option<Symbols>> {
    if bytes.starts_with(b"!<arch>\n") {
        let archive = object::read::archive::ArchiveFile::parse(bytes)
            .with_context(|| format!("reading static archive {path}"))?;
        let mut debug = false;
        for member in archive.members() {
            let member = member.with_context(|| format!("reading archive member in {path}"))?;
            let bytes = member.data(bytes)?;
            let Some((file, elf::ET_REL)) = native_elf(bytes, arch) else {
                // Do not rewrite mixed-format or foreign archives (including bitcode).
                return Ok(None);
            };
            debug |= has_debug(&file);
        }
        return Ok(debug.then_some(Symbols::Linkable));
    }
    if bytes.ends_with(MODULE_SIGNATURE) {
        // Any rewrite, even of debug sections, invalidates an appended signature.
        return Ok(None);
    }
    let Some((file, header_type)) = native_elf(bytes, arch) else {
        return Ok(None);
    };
    if header_type == elf::ET_REL {
        return Ok(has_debug(&file).then_some(Symbols::Linkable));
    }
    if !matches!(header_type, elf::ET_EXEC | elf::ET_DYN)
        || !(has_debug(&file) || file.section_by_name(".symtab").is_some())
    {
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
    Ok(Some(Symbols::BuildId(id.to_vec())))
}

/// Choose the shipped objects to strip and the debug file for each one.
/// Returns the strip plan and, for each output index, its debug file actions.
pub(crate) fn plan(
    root: &Root,
    outputs: &[Manifest],
    arch: &str,
) -> Result<(Vec<Split>, Vec<Vec<Action>>)> {
    let mut splits = Vec::new();
    let mut debug_actions = vec![Vec::new(); outputs.len()];
    let mut owners = BTreeMap::<Vec<u8>, usize>::new();
    let packaged_paths: BTreeSet<_> = outputs
        .iter()
        .flat_map(|output| output.action.iter().map(Action::path))
        .collect();
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
            // Read only the magic number of files that cannot be ELF objects or archives.
            let mut file = root.file(path)?;
            let mut bytes = Vec::new();
            file.by_ref().take(8).read_to_end(&mut bytes)?;
            if !bytes.starts_with(b"\x7fELF") && bytes != b"!<arch>\n" {
                continue;
            }
            file.read_to_end(&mut bytes)?;
            let Some(symbols) = inspect(&bytes, arch, path)? else {
                continue;
            };
            let linkable = symbols == Symbols::Linkable;
            let debug = match symbols {
                Symbols::Linkable => Some(debug_delta_path(path)),
                Symbols::BuildId(id) => match owners.get(&id) {
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
                        Some(debug_path(&id))
                    }
                },
            };
            if let Some(debug) = &debug {
                ensure!(
                    !packaged_paths.contains(debug.as_str()),
                    "generated debug file {debug} conflicts with a packaged path"
                );
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
            }
            splits.push(Split {
                path: path.clone(),
                debug,
                linkable,
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
            let flags = if split.linkable {
                "--enable-deterministic-archives "
            } else {
                "--only-keep-debug "
            };
            script.push_str(&format!(
                "mkdir -p {}\nobjcopy {flags}{object} {}\n",
                quote(directory),
                quote(&debug)
            ));
        }
        if split.linkable {
            // LTO's serialized compiler data can reference these debug sections.
            // Removing them can make even a non-debug static link fail.
            script.push_str(&format!(
                "objcopy --enable-deterministic-archives --strip-debug --keep-section='.gnu.lto_*' --keep-section='.gnu.debuglto_*' {object}\n"
            ));
        } else {
            script.push_str(&format!("objcopy --strip-unneeded {object}\n"));
        }
    }
    script
}

/// Turn temporary linkable copies into debug deltas and fix debug file modes.
pub(crate) fn finish(output: &std::path::Path, splits: &[Split]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for split in splits {
        let Some(debug) = &split.debug else { continue };
        let path = output.join(debug);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("objcopy did not create {debug}"))?;
        ensure!(metadata.is_file(), "{debug} is not a regular file");
        if split.linkable {
            crate::debug::create_delta(&output.join(&split.path), &path)?;
        }
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
                linkable: false,
            },
            Split {
                path: "usr/bin/copy".into(),
                debug: None,
                linkable: false,
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
            (true, Some(id)) => assert_eq!(
                inspect(&own, arch, "test")?,
                Some(Symbols::BuildId(id.to_vec()))
            ),
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

    /// A minimal little-endian ELF64 relocatable object with one debug section.
    fn relocatable_with_debug(arch: &str) -> Vec<u8> {
        let names = b"\0.debug_info\0.shstrtab\0";
        let debug = b"debug";
        let mut bytes = vec![0; 64];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&elf::ET_REL.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine(arch).unwrap().to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[58..60].copy_from_slice(&64u16.to_le_bytes());
        bytes[60..62].copy_from_slice(&3u16.to_le_bytes());
        bytes[62..64].copy_from_slice(&2u16.to_le_bytes());
        let debug_offset = bytes.len() as u64;
        bytes.extend_from_slice(debug);
        let names_offset = bytes.len() as u64;
        bytes.extend_from_slice(names);
        bytes.resize(bytes.len().next_multiple_of(8), 0);
        let headers = bytes.len() as u64;
        bytes[40..48].copy_from_slice(&headers.to_le_bytes());
        let mut section = |name: u32, kind: u32, offset: u64, size: u64| {
            let mut header = [0; 64];
            header[..4].copy_from_slice(&name.to_le_bytes());
            header[4..8].copy_from_slice(&kind.to_le_bytes());
            header[24..32].copy_from_slice(&offset.to_le_bytes());
            header[32..40].copy_from_slice(&size.to_le_bytes());
            header[48..56].copy_from_slice(&1u64.to_le_bytes());
            bytes.extend_from_slice(&header);
        };
        section(0, elf::SHT_NULL, 0, 0);
        section(1, elf::SHT_PROGBITS, debug_offset, debug.len() as u64);
        section(13, elf::SHT_STRTAB, names_offset, names.len() as u64);
        bytes
    }

    #[test]
    fn signed_modules_are_left_unchanged() -> Result<()> {
        let arch = "x86_64";
        let module = relocatable_with_debug(arch);
        assert_eq!(inspect(&module, arch, "plain.ko")?, Some(Symbols::Linkable));

        // struct module_signature with an empty signature, then the marker.
        let mut signed = module.clone();
        signed.extend_from_slice(&[0; 12]);
        signed.extend_from_slice(MODULE_SIGNATURE);
        assert!(native_elf(&signed, arch).is_some());
        assert_eq!(inspect(&signed, arch, "signed.ko")?, None);

        let mut archive = b"!<arch>\n".to_vec();
        archive.extend_from_slice(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                "plain.o/",
                0,
                0,
                0,
                644,
                module.len()
            )
            .as_bytes(),
        );
        archive.extend_from_slice(&module);
        assert_eq!(
            inspect(&archive, arch, "libplain.a")?,
            Some(Symbols::Linkable)
        );
        Ok(())
    }
}
