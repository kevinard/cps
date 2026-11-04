use std::{
    fs::File,
    io::{Read, Seek},
    os::unix::fs::MetadataExt,
};

use anyhow::{Context, Result, bail, ensure};
use object::{
    elf,
    endian::Endianness,
    read::{
        ReadCache, ReadRef,
        elf::{Dyn, FileHeader, ProgramHeader},
    },
};

use crate::model::digest_reader;

const PATH_LIMIT: u64 = 4096;
const DYNAMIC_LIMIT: u64 = 1024 * 1024;
const STRINGS_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Elf {
    pub native_64_le: bool,
    pub machine: u16,
    pub interpreter: Option<String>,
    pub needed: Vec<String>,
    pub runpath: Option<String>,
    pub shared_object: bool,
    pub use_default_dirs: bool,
}

pub(super) fn inspect(mut file: File, size: u64, hash: &str) -> Result<Option<Elf>> {
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
    let info = if magic[..4] == elf::ELFMAG {
        let data = ReadCache::new(&mut file);
        match magic[4] {
            elf::ELFCLASS32 => parse::<elf::FileHeader32<Endianness>>(&data)?,
            elf::ELFCLASS64 => parse::<elf::FileHeader64<Endianness>>(&data)?,
            _ => bail!("unsupported ELF class"),
        }
    } else {
        None
    };
    let after = file.metadata()?;
    ensure!(
        (before.ctime(), before.ctime_nsec(), size)
            == (after.ctime(), after.ctime_nsec(), after.len()),
        "payload changed during ELF inspection"
    );
    Ok(info)
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
        machine: header.e_machine(endian),
        interpreter: None,
        needed: Vec::new(),
        runpath: None,
        shared_object: kind == elf::ET_DYN,
        use_default_dirs: true,
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
        let (mut strtab, mut strsz, mut runpath, mut rpath) = (None, None, None, None);
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
                Some(elf::DT_STRTAB) => set_once(&mut strtab, value, "DT_STRTAB")?,
                Some(elf::DT_STRSZ) => set_once(&mut strsz, value, "DT_STRSZ")?,
                Some(elf::DT_RUNPATH) => set_once(&mut runpath, value, "DT_RUNPATH")?,
                Some(elf::DT_RPATH) => set_once(&mut rpath, value, "DT_RPATH")?,
                Some(elf::DT_FLAGS_1) => {
                    result.use_default_dirs &= value & u64::from(elf::DF_1_NODEFLIB) == 0;
                    result.shared_object &= value & u64::from(elf::DF_1_PIE) == 0;
                }
                _ => {}
            }
        }
        ensure!(terminated, "unterminated ELF dynamic segment");
        let runpath = runpath.or(rpath);
        if !needed.is_empty() || runpath.is_some() {
            ensure!(needed.len() <= 4096, "too many ELF dependencies");
            let address = strtab.context("missing ELF dynamic string table address")?;
            let size = strsz.context("missing ELF dynamic string table size")?;
            ensure!(
                size <= STRINGS_LIMIT,
                "ELF dynamic string table is too large"
            );
            let offset = segments
                .iter()
                .filter(|segment| segment.p_type(endian) == elf::PT_LOAD)
                .find_map(|segment| {
                    let (offset, length) = segment.file_range(endian);
                    let start: u64 = segment.p_vaddr(endian).into();
                    let relative = address.checked_sub(start)?;
                    (relative.checked_add(size)? <= length).then_some(())?;
                    offset.checked_add(relative)
                })
                .context("ELF dynamic string table is outside file-backed segments")?;
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
            result.runpath = runpath.map(get).transpose()?;
        }
    }
    Ok(Some(result))
}

fn set_once(slot: &mut Option<u64>, value: u64, tag: &str) -> Result<()> {
    ensure!(slot.replace(value).is_none(), "duplicate ELF {tag}");
    Ok(())
}
