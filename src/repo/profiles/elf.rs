mod versions;

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek},
    os::unix::fs::MetadataExt,
};

use anyhow::{Context, Result, bail, ensure};
use object::{
    elf,
    endian::Endianness,
    read::{
        Bytes, ReadCache, ReadRef,
        elf::{Dyn, FileHeader, ProgramHeader},
    },
};

use crate::model::{ELF_CAPABILITY_PREFIX, digest_reader, elf_name_component};

const PATH_LIMIT: u64 = 4096;
const DYNAMIC_LIMIT: u64 = 1024 * 1024;
const STRINGS_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Elf {
    pub native_64_le: bool,
    pub elf64: bool,
    pub little_endian: bool,
    pub machine: u16,
    pub soname: Option<String>,
    pub interpreter: Option<String>,
    pub needed: Vec<String>,
    pub runpath: Option<String>,
    pub shared_object: bool,
    pub use_default_dirs: bool,
    pub required_versions: BTreeMap<String, versions::Names>,
    pub defined_versions: versions::Names,
}

impl Elf {
    /// The loader ABI that this object links against: machine, class, and
    /// byte order. Generated library capabilities are keyed by it, so 32-bit
    /// and 64-bit objects with one soname never satisfy each other.
    pub(crate) fn abi(&self) -> String {
        let name = match (self.machine, self.elf64, self.little_endian) {
            (elf::EM_X86_64, true, true) => "x86_64",
            (elf::EM_X86_64, false, true) => "x32",
            (elf::EM_386, false, true) => "i686",
            (elf::EM_AARCH64, true, true) => "aarch64",
            (elf::EM_AARCH64, true, false) => "aarch64_be",
            (elf::EM_ARM, false, true) => "arm",
            (elf::EM_RISCV, true, true) => "riscv64",
            (elf::EM_RISCV, false, true) => "riscv32",
            (elf::EM_PPC64, true, true) => "ppc64le",
            (elf::EM_PPC64, true, false) => "ppc64",
            (elf::EM_S390, true, false) => "s390x",
            (elf::EM_LOONGARCH, true, true) => "loongarch64",
            (machine, elf64, little) => {
                return format!(
                    "elf{}{}-{machine}",
                    if elf64 { 64 } else { 32 },
                    if little { "le" } else { "be" }
                );
            }
        };
        name.to_owned()
    }
}

/// The capability for a library's soname, keyed by the loader ABI.
pub(crate) fn library_capability(abi: &str, soname: &str) -> Result<String> {
    ensure!(
        elf_name_component(soname),
        "ELF soname {soname:?} cannot name a capability"
    );
    Ok(format!("{ELF_CAPABILITY_PREFIX}{abi}/{soname}"))
}

/// The capability for one GNU version node of a library.
pub(crate) fn version_capability(abi: &str, soname: &str, version: &str) -> Result<String> {
    ensure!(
        elf_name_component(version),
        "ELF version {version:?} of {soname} cannot name a capability"
    );
    Ok(format!("{}/{version}", library_capability(abi, soname)?))
}

/// The capabilities that satisfy one `DT_NEEDED` entry. A required version
/// node implies the soname, so the soname alone is required only when the
/// consumer names no non-weak version.
pub(crate) fn needed_capabilities(consumer: &Elf, needed: &str) -> Result<Vec<String>> {
    let abi = consumer.abi();
    match consumer.required_versions.get(needed) {
        Some(versions) if !versions.is_empty() => versions
            .keys()
            .map(|version| version_capability(&abi, needed, version))
            .collect(),
        _ => Ok(vec![library_capability(&abi, needed)?]),
    }
}

pub(crate) fn inspect(file: File, size: u64, hash: &str) -> Result<Option<Elf>> {
    inspect_with(file, size, hash, inspect_header)
}

/// Like `inspect`, but a file whose ELF structure does not parse is not a
/// loadable object. Generated declarations use this, so that a deliberately
/// malformed ELF file, such as a test fixture, does not block a build or a
/// publication. The size and digest checks still apply.
pub(crate) fn inspect_loadable(file: File, size: u64, hash: &str) -> Result<Option<Elf>> {
    inspect_with(file, size, hash, loadable_header)
}

/// Like `inspect_header`, but an ELF structure that does not parse is not a
/// loadable object.
pub(crate) fn loadable_header(file: &mut File, prefix: &[u8]) -> Result<Option<Elf>> {
    Ok(inspect_header(file, prefix).ok().flatten())
}

fn inspect_with(
    mut file: File,
    size: u64,
    hash: &str,
    header: impl FnOnce(&mut File, &[u8]) -> Result<Option<Elf>>,
) -> Result<Option<Elf>> {
    let before = file.metadata()?;
    ensure!(before.len() == size, "payload size differs from manifest");
    let mut magic = [0; 5];
    let prefix = size.min(magic.len() as u64) as usize;
    file.read_exact(&mut magic[..prefix])?;
    file.rewind()?;
    ensure!(
        digest_reader(file.by_ref().take(size.saturating_add(1)))? == hash,
        "payload digest differs from manifest"
    );
    let info = header(&mut file, &magic[..prefix])?;
    let after = file.metadata()?;
    ensure!(
        (before.ctime(), before.ctime_nsec(), size)
            == (after.ctime(), after.ctime_nsec(), after.len()),
        "payload changed during ELF inspection"
    );
    Ok(info)
}

pub(crate) fn inspect_header(file: &mut File, prefix: &[u8]) -> Result<Option<Elf>> {
    if !prefix.starts_with(&elf::ELFMAG) {
        return Ok(None);
    }
    let data = ReadCache::new(file);
    match prefix.get(4).copied() {
        Some(elf::ELFCLASS32) => parse::<elf::FileHeader32<Endianness>>(&data),
        Some(elf::ELFCLASS64) => parse::<elf::FileHeader64<Endianness>>(&data),
        _ => bail!("unsupported ELF class"),
    }
}

fn parse<'a, E: FileHeader>(data: impl ReadRef<'a>) -> Result<Option<Elf>> {
    let header = E::parse(data)?;
    let endian = header.endian()?;
    let kind = header.e_type(endian);
    if !matches!(kind, elf::ET_EXEC | elf::ET_DYN) {
        return Ok(None);
    }
    ensure!(
        header.phnum(endian, data)? <= 1024,
        "too many ELF program headers"
    );
    let segments = header.program_headers(endian, data)?;
    ensure!(
        segments.iter().any(|p| p.p_type(endian) == elf::PT_LOAD),
        "ELF has no load segment"
    );
    let mut result = Elf {
        native_64_le: header.is_type_64() && header.is_little_endian(),
        elf64: header.is_type_64(),
        little_endian: header.is_little_endian(),
        machine: header.e_machine(endian),
        soname: None,
        interpreter: None,
        needed: Vec::new(),
        runpath: None,
        shared_object: kind == elf::ET_DYN,
        use_default_dirs: true,
        required_versions: BTreeMap::new(),
        defined_versions: BTreeMap::new(),
    };
    let mut dynamic = None;
    for segment in segments {
        match segment.p_type(endian) {
            elf::PT_INTERP => {
                ensure!(result.interpreter.is_none(), "duplicate ELF interpreter");
                ensure!(
                    segment.file_range(endian).1 <= PATH_LIMIT,
                    "ELF interpreter is too long"
                );
                let bytes = segment
                    .interpreter(endian, data)?
                    .context("missing ELF interpreter")?;
                result.interpreter = Some(std::str::from_utf8(bytes)?.to_owned());
            }
            elf::PT_DYNAMIC => {
                ensure!(dynamic.is_none(), "duplicate ELF dynamic segment");
                ensure!(
                    segment.file_range(endian).1 <= DYNAMIC_LIMIT,
                    "ELF dynamic segment is too large"
                );
                dynamic = segment.dynamic(endian, data)?;
            }
            _ => {}
        }
    }
    if let Some(entries) = dynamic {
        let mut needed = Vec::new();
        let mut tags = BTreeMap::new();
        let mut terminated = false;
        for entry in entries {
            let value: u64 = entry.d_val(endian).into();
            let tag = entry.tag32(endian);
            match tag {
                Some(elf::DT_NULL) => {
                    terminated = true;
                    break;
                }
                Some(elf::DT_NEEDED) => needed.push(value),
                Some(
                    tag @ (elf::DT_STRTAB
                    | elf::DT_STRSZ
                    | elf::DT_SONAME
                    | elf::DT_RUNPATH
                    | elf::DT_RPATH
                    | elf::DT_VERNEED
                    | elf::DT_VERNEEDNUM
                    | elf::DT_VERDEF
                    | elf::DT_VERDEFNUM),
                ) => {
                    ensure!(
                        tags.insert(tag, value).is_none(),
                        "duplicate ELF dynamic tag {tag:#x}"
                    );
                }
                Some(elf::DT_FLAGS_1) => {
                    result.use_default_dirs &= value & u64::from(elf::DF_1_NODEFLIB) == 0;
                    result.shared_object &= value & u64::from(elf::DF_1_PIE) == 0;
                }
                _ => {}
            }
        }
        ensure!(terminated, "unterminated ELF dynamic segment");
        let runpath = tags
            .get(&elf::DT_RUNPATH)
            .or_else(|| tags.get(&elf::DT_RPATH));
        let load = |address: u64, size: u64| {
            segments
                .iter()
                .filter(|segment| segment.p_type(endian) == elf::PT_LOAD)
                .find_map(|segment| {
                    let (offset, length) = segment.file_range(endian);
                    let relative = address.checked_sub(segment.p_vaddr(endian).into())?;
                    let available = length.checked_sub(relative)?;
                    (available >= size).then_some(())?;
                    Some((offset.checked_add(relative)?, available))
                })
                .context("ELF dynamic data is outside file-backed segments")
        };
        let table = |address_tag, count_tag| -> Result<Option<versions::Table<'a>>> {
            match (tags.get(&address_tag), tags.get(&count_tag)) {
                (None, None) => Ok(None),
                (Some(&address), Some(&count)) => {
                    ensure!(
                        (1..=u64::from(elf::VERSYM_VERSION)).contains(&count),
                        "invalid ELF version record count"
                    );
                    let (offset, available) = load(address, 1)?;
                    let bytes = data
                        .read_bytes_at(offset, available.min(DYNAMIC_LIMIT))
                        .ok()
                        .context("reading ELF version records")?;
                    Ok(Some(versions::Table {
                        bytes: Bytes(bytes),
                        count: count as usize,
                    }))
                }
                _ => bail!("incomplete ELF version table tags"),
            }
        };
        let required = table(elf::DT_VERNEED, elf::DT_VERNEEDNUM)?;
        let defined = table(elf::DT_VERDEF, elf::DT_VERDEFNUM)?;
        let soname = tags.get(&elf::DT_SONAME);
        if !needed.is_empty()
            || runpath.is_some()
            || soname.is_some()
            || required.is_some()
            || defined.is_some()
        {
            ensure!(needed.len() <= 4096, "too many ELF dependencies");
            let address = *tags
                .get(&elf::DT_STRTAB)
                .context("missing ELF dynamic string table address")?;
            let size = *tags
                .get(&elf::DT_STRSZ)
                .context("missing ELF dynamic string table size")?;
            ensure!(
                size <= STRINGS_LIMIT,
                "ELF dynamic string table is too large"
            );
            let (offset, _) = load(address, size)?;
            let strings = data
                .read_bytes_at(offset, size)
                .ok()
                .context("reading ELF dynamic strings")?;
            let get = |offset: u64| -> Result<String> {
                let string = strings
                    .get(usize::try_from(offset)?..)
                    .context("invalid ELF string offset")?;
                let end = string
                    .iter()
                    .take(PATH_LIMIT as usize)
                    .position(|byte| *byte == 0)
                    .context("unterminated or oversized ELF path")?;
                Ok(std::str::from_utf8(&string[..end])?.to_owned())
            };
            result.needed = needed.into_iter().map(get).collect::<Result<_>>()?;
            result.runpath = runpath.copied().map(get).transpose()?;
            result.soname = soname.copied().map(get).transpose()?;
            if let Some(required) = required {
                result.required_versions = versions::required(required, endian, get)?;
                for library in result.required_versions.keys() {
                    ensure!(
                        result.needed.contains(library),
                        "ELF version requirement names undeclared dependency {library}"
                    );
                }
            }
            if let Some(defined) = defined {
                result.defined_versions = versions::defined(defined, endian, get)?;
            }
        }
    }
    Ok(Some(result))
}
