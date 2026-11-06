use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use object::{elf, endian::Endian, read::Bytes};

pub(crate) type Names = BTreeMap<String, u32>;

pub(super) struct Table<'a> {
    pub bytes: Bytes<'a>,
    pub count: usize,
}

const RECORD_LIMIT: usize = elf::VERSYM_VERSION as usize;

pub(super) fn required<E: Endian>(
    table: Table<'_>,
    endian: E,
    string: impl Fn(u64) -> Result<String>,
) -> Result<BTreeMap<String, Names>> {
    let mut result = BTreeMap::new();
    let mut offset = 0;
    let mut budget = RECORD_LIMIT;
    for index in 0..table.count {
        let header = table
            .bytes
            .read_at::<elf::Verneed<E>>(offset)
            .ok()
            .context("truncated ELF version requirement")?;
        ensure!(
            header.vn_version.get(endian) == elf::VER_NEED_CURRENT,
            "unsupported ELF version requirement revision"
        );
        let file = string(u64::from(header.vn_file.get(endian)))?;
        let count = usize::from(header.vn_cnt.get(endian));
        let size = size_of_val(header);
        let mut aux = start_aux(offset, size, header.vn_aux.get(endian), count, &mut budget)?;
        let mut versions = Names::new();
        for index in 0..count {
            let need = table
                .bytes
                .read_at::<elf::Vernaux<E>>(aux)
                .ok()
                .context("truncated ELF version requirement name")?;
            let name = version_name(&string, need.vna_name.get(endian))?;
            if need.vna_flags.get(endian) & elf::VER_FLG_WEAK == 0 {
                ensure!(
                    versions.insert(name, need.vna_hash.get(endian)).is_none(),
                    "duplicate required ELF version"
                );
            }
            aux = next(
                aux,
                size_of_val(need),
                need.vna_next.get(endian),
                index + 1 == count,
            )?;
        }
        ensure!(
            result.insert(file, versions).is_none(),
            "duplicate ELF version requirement library"
        );
        offset = next(
            offset,
            size,
            header.vn_next.get(endian),
            index + 1 == table.count,
        )?;
    }
    Ok(result)
}

pub(super) fn defined<E: Endian>(
    table: Table<'_>,
    endian: E,
    string: impl Fn(u64) -> Result<String>,
) -> Result<Names> {
    let mut result = Names::new();
    let mut offset = 0;
    let mut budget = RECORD_LIMIT;
    for index in 0..table.count {
        let header = table
            .bytes
            .read_at::<elf::Verdef<E>>(offset)
            .ok()
            .context("truncated ELF version definition")?;
        ensure!(
            header.vd_version.get(endian) == elf::VER_DEF_CURRENT,
            "unsupported ELF version definition revision"
        );
        let count = usize::from(header.vd_cnt.get(endian));
        let size = size_of_val(header);
        let mut aux = start_aux(offset, size, header.vd_aux.get(endian), count, &mut budget)?;
        for index in 0..count {
            let definition = table
                .bytes
                .read_at::<elf::Verdaux<E>>(aux)
                .ok()
                .context("truncated ELF version definition name")?;
            let name = version_name(&string, definition.vda_name.get(endian))?;
            // The first entry names this version node. Later names describe
            // parent nodes and do not establish that those nodes are exported.
            if index == 0 && header.vd_flags.get(endian) & elf::VER_FLG_BASE == 0 {
                ensure!(
                    result.insert(name, header.vd_hash.get(endian)).is_none(),
                    "duplicate defined ELF version"
                );
            }
            aux = next(
                aux,
                size_of_val(definition),
                definition.vda_next.get(endian),
                index + 1 == count,
            )?;
        }
        offset = next(
            offset,
            size,
            header.vd_next.get(endian),
            index + 1 == table.count,
        )?;
    }
    Ok(result)
}

fn version_name(string: &impl Fn(u64) -> Result<String>, offset: u32) -> Result<String> {
    let name = string(u64::from(offset))?;
    ensure!(
        (1..=1024).contains(&name.len()),
        "invalid ELF version name length"
    );
    Ok(name)
}

fn start_aux(
    offset: usize,
    size: usize,
    relative: u32,
    count: usize,
    budget: &mut usize,
) -> Result<usize> {
    ensure!(
        (1..=*budget).contains(&count),
        "invalid ELF auxiliary version count"
    );
    *budget -= count;
    ensure!(
        relative as usize >= size,
        "overlapping ELF auxiliary version record"
    );
    offset
        .checked_add(relative as usize)
        .context("ELF version offset overflow")
}

fn next(offset: usize, size: usize, relative: u32, last: bool) -> Result<usize> {
    let advance = if last {
        ensure!(
            relative == 0,
            "ELF version count does not match the record chain"
        );
        size
    } else {
        ensure!(relative as usize >= size, "short ELF version record chain");
        relative as usize
    };
    offset
        .checked_add(advance)
        .context("ELF version offset overflow")
}

#[cfg(test)]
mod tests {
    use object::endian::LittleEndian;

    use super::*;

    #[test]
    fn weak_requirements_are_optional_and_bad_chains_fail() -> Result<()> {
        let mut bytes = [
            1, 0, 1, 0, // Revision and auxiliary count.
            1, 0, 0, 0, // Library name.
            16, 0, 0, 0, // Auxiliary record offset.
            0, 0, 0, 0, // Last requirement.
            42, 0, 0, 0, // Version hash.
            2, 0, 2, 0, // Weak flag and version index.
            2, 0, 0, 0, // Version name.
            0, 0, 0, 0, // Last auxiliary record.
        ];
        let read = |bytes: &[u8]| {
            required(
                Table {
                    bytes: Bytes(bytes),
                    count: 1,
                },
                LittleEndian,
                |offset| {
                    Ok(match offset {
                        1 => "libdemo.so.1",
                        2 => "DEMO_1.0",
                        _ => anyhow::bail!("unexpected string offset"),
                    }
                    .to_owned())
                },
            )
        };
        assert!(read(&bytes)?["libdemo.so.1"].is_empty());
        bytes[20] = 0;
        assert_eq!(read(&bytes)?["libdemo.so.1"]["DEMO_1.0"], 42);
        bytes[28] = 16;
        assert!(
            read(&bytes)
                .unwrap_err()
                .to_string()
                .contains("record chain")
        );
        Ok(())
    }
}
