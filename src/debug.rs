//! Debug deltas for relocatable ELF objects and static archives.
//!
//! The delta retains the original file layout and removed debug information,
//! referring to identical sections in the stripped file instead of duplicating
//! code or LTO data. Restoring the original bytes also preserves archive indexes,
//! member order, duplicate names, ELF groups, and debug relocations.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use object::{Object, ObjectSection, read::archive::ArchiveFile};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::model::digest_reader;

// Header: magic/version, original size (LE u64), base SHA-256, original SHA-256.
// The remaining bytes are a zstd stream of records:
//   0, length (LE u64), literal bytes
//   1, length (LE u64), offset in stripped file (LE u64)
// Records cover the original file in order, with no terminator or trailing data.
const MAGIC: &[u8; 8] = b"CPSDBG\0\x01";

struct Reference {
    original: u64,
    stripped: u64,
    size: u64,
}

fn elf_references(
    original: &[u8],
    stripped: &[u8],
    original_offset: u64,
    stripped_offset: u64,
    references: &mut Vec<Reference>,
) -> Result<()> {
    let original_file = object::File::parse(original)?;
    let stripped_file = object::File::parse(stripped)?;
    let mut sections = BTreeMap::<_, Vec<_>>::new();
    for section in stripped_file.sections() {
        if let Some((offset, size)) = section.file_range() {
            sections
                .entry((section.name_bytes()?, size))
                .or_default()
                .push((offset, section.data()?));
        }
    }
    for section in original_file.sections() {
        let Some((offset, size)) = section.file_range() else {
            continue;
        };
        if size == 0 {
            continue;
        }
        if let Some(candidates) = sections.get(&(section.name_bytes()?, size)) {
            let data = section.data()?;
            if let Some((base, _)) = candidates.iter().find(|(_, bytes)| *bytes == data) {
                references.push(Reference {
                    original: original_offset + offset,
                    stripped: stripped_offset + base,
                    size,
                });
            }
        }
    }
    Ok(())
}

fn references(original: &[u8], stripped: &[u8]) -> Result<Vec<Reference>> {
    let mut references = Vec::new();
    if original.starts_with(b"!<arch>\n") {
        let original_archive = ArchiveFile::parse(original)?;
        let stripped_archive = ArchiveFile::parse(stripped)?;
        let original_members = original_archive
            .members()
            .collect::<object::Result<Vec<_>>>()?;
        let stripped_members = stripped_archive
            .members()
            .collect::<object::Result<Vec<_>>>()?;
        ensure!(
            original_members.len() == stripped_members.len(),
            "stripping changed the archive member count"
        );
        for (original_member, stripped_member) in original_members.iter().zip(&stripped_members) {
            ensure!(
                original_member.name() == stripped_member.name(),
                "stripping changed the archive member order"
            );
            elf_references(
                original_member.data(original)?,
                stripped_member.data(stripped)?,
                original_member.file_range().0,
                stripped_member.file_range().0,
                &mut references,
            )?;
        }
    } else {
        elf_references(original, stripped, 0, 0, &mut references)?;
    }
    references.sort_unstable_by_key(|reference| reference.original);
    Ok(references)
}

fn write_delta(original: &[u8], stripped: &[u8], mut writer: impl Write) -> Result<()> {
    let references = references(original, stripped)?;
    writer.write_all(MAGIC)?;
    writer.write_all(&(original.len() as u64).to_le_bytes())?;
    writer.write_all(&Sha256::digest(stripped))?;
    writer.write_all(&Sha256::digest(original))?;
    let mut encoder = zstd::stream::write::Encoder::new(writer, 9)?;
    let mut cursor = 0;
    for reference in references {
        let start = usize::try_from(reference.original)?;
        let size = usize::try_from(reference.size)?;
        // Overlapping ELF sections are unusual but need not prevent an exact
        // restoration: retain overlapping bytes in the surrounding literals.
        if start < cursor {
            continue;
        }
        if start > cursor {
            encoder.write_all(&[0])?;
            encoder.write_all(&((start - cursor) as u64).to_le_bytes())?;
            encoder.write_all(&original[cursor..start])?;
        }
        encoder.write_all(&[1])?;
        encoder.write_all(&reference.size.to_le_bytes())?;
        encoder.write_all(&reference.stripped.to_le_bytes())?;
        cursor = start + size;
    }
    if cursor < original.len() {
        encoder.write_all(&[0])?;
        encoder.write_all(&((original.len() - cursor) as u64).to_le_bytes())?;
        encoder.write_all(&original[cursor..])?;
    }
    encoder.finish()?;
    Ok(())
}

/// Replace the deterministic objcopy unstripped copy with its debug delta.
pub(crate) fn create_delta(stripped: &Path, original: &Path) -> Result<()> {
    let original_bytes = fs::read(original)?;
    let stripped_bytes = fs::read(stripped)?;
    let mut delta = NamedTempFile::new_in(original.parent().context("debug file has no parent")?)?;
    write_delta(&original_bytes, &stripped_bytes, &mut delta)
        .with_context(|| format!("splitting debug information for {}", stripped.display()))?;
    delta.persist(original)?;
    Ok(())
}

fn read_u64(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn restore(stripped: &mut File, symbols: impl Read, output: &mut File) -> Result<()> {
    let mut symbols = symbols;
    let mut magic = [0; 8];
    symbols.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "unsupported CPS debug delta format");
    let size = read_u64(&mut symbols)?;
    let mut base_hash = [0; 32];
    let mut original_hash = [0; 32];
    symbols.read_exact(&mut base_hash)?;
    symbols.read_exact(&mut original_hash)?;
    ensure!(
        digest_reader(&mut *stripped)? == hex::encode(base_hash),
        "debug delta does not match the stripped file"
    );
    let base_size = stripped.metadata()?.len();
    let mut decoder = zstd::stream::read::Decoder::new(symbols)?;
    decoder.window_log_max(27)?;
    let mut written = 0;
    while written < size {
        let mut tag = [0];
        decoder.read_exact(&mut tag)?;
        let length = read_u64(&mut decoder)?;
        ensure!(
            length > 0 && length <= size - written,
            "invalid debug delta length"
        );
        let copied = match tag[0] {
            0 => io::copy(&mut decoder.by_ref().take(length), output)?,
            1 => {
                let offset = read_u64(&mut decoder)?;
                ensure!(
                    offset <= base_size && length <= base_size - offset,
                    "debug delta reference is outside the stripped file"
                );
                stripped.seek(SeekFrom::Start(offset))?;
                io::copy(&mut Read::by_ref(stripped).take(length), output)?
            }
            _ => bail!("invalid debug delta record"),
        };
        ensure!(copied == length, "truncated debug delta data");
        written += length;
    }
    ensure!(decoder.read(&mut [0])? == 0, "trailing debug delta data");
    output.rewind()?;
    ensure!(
        digest_reader(output)? == hex::encode(original_hash),
        "restored debug file checksum mismatch"
    );
    Ok(())
}

/// Restore an unstripped object or archive from its installed file and debug
/// delta. The output must be a new path; both inputs remain untouched. Hashes
/// bind the symbols to the exact stripped file and verify the restored bytes.
pub fn restore_debug(stripped: &Path, symbols: &Path, output: &Path) -> Result<()> {
    let mut stripped = File::open(stripped)
        .with_context(|| format!("opening stripped file {}", stripped.display()))?;
    let symbols = File::open(symbols)
        .with_context(|| format!("opening debug delta {}", symbols.display()))?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut restored = NamedTempFile::new_in(parent)?;
    restore(&mut stripped, symbols, restored.as_file_mut())
        .with_context(|| format!("restoring debug information to {}", output.display()))?;
    restored
        .as_file()
        .set_permissions(fs::Permissions::from_mode(
            stripped.metadata()?.permissions().mode() & 0o777,
        ))?;
    restored.persist_noclobber(output).with_context(|| {
        format!(
            "creating restored file {} (output must not exist)",
            output.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(base: &[u8], original: &[u8], records: &[u8]) -> Result<Vec<u8>> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(original.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&Sha256::digest(base));
        bytes.extend_from_slice(&Sha256::digest(original));
        bytes.extend_from_slice(&zstd::stream::encode_all(records, 9)?);
        Ok(bytes)
    }

    fn reference(offset: u64, length: u64) -> Vec<u8> {
        let mut bytes = vec![1];
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes
    }

    #[test]
    fn restoration_verifies_both_hashes_and_preserves_existing_files() -> Result<()> {
        let work = tempfile::tempdir()?;
        let base = work.path().join("base.o");
        let symbols = work.path().join("base.o.cps-debug");
        let output = work.path().join("restored.o");
        fs::write(&base, b"CODE")?;
        let mut records = reference(0, 4);
        records.push(0);
        records.extend_from_slice(&5u64.to_le_bytes());
        records.extend_from_slice(b"DEBUG");
        fs::write(&symbols, delta(b"CODE", b"CODEDEBUG", &records)?)?;
        restore_debug(&base, &symbols, &output)?;
        assert_eq!(fs::read(&output)?, b"CODEDEBUG");
        assert!(restore_debug(&base, &symbols, &output).is_err());
        assert_eq!(fs::read(&output)?, b"CODEDEBUG");
        assert!(restore_debug(&base, &symbols, &base).is_err());
        assert_eq!(fs::read(&base)?, b"CODE");

        fs::remove_file(&output)?;
        fs::write(&base, b"WRONG")?;
        let error = restore_debug(&base, &symbols, &output).unwrap_err();
        assert!(format!("{error:#}").contains("does not match the stripped file"));
        assert!(!output.exists());

        fs::write(&base, b"CODE")?;
        *records.last_mut().unwrap() = b'!';
        fs::write(&symbols, delta(b"CODE", b"CODEDEBUG", &records)?)?;
        let error = restore_debug(&base, &symbols, &output).unwrap_err();
        assert!(format!("{error:#}").contains("checksum mismatch"));
        assert!(!output.exists());
        Ok(())
    }

    #[test]
    fn invalid_records_never_publish_partial_outputs() -> Result<()> {
        let work = tempfile::tempdir()?;
        let base = work.path().join("base.o");
        let symbols = work.path().join("base.o.cps-debug");
        let output = work.path().join("restored.o");
        fs::write(&base, b"CODE")?;
        let mut trailing = reference(0, 4);
        trailing.push(0);
        for records in [
            reference(u64::MAX, 4),
            reference(1, 4),
            reference(0, u64::MAX),
            reference(0, 0),
            vec![0, 4, 0, 0, 0, 0, 0, 0, 0, b'C'],
            vec![2, 4, 0, 0, 0, 0, 0, 0, 0],
            trailing,
        ] {
            fs::write(&symbols, delta(b"CODE", b"CODE", &records)?)?;
            assert!(restore_debug(&base, &symbols, &output).is_err());
            assert!(!output.exists());
        }
        Ok(())
    }
}
