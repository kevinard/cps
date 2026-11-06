use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    os::unix::fs::FileExt,
    path::Path,
};

use anyhow::{Result, bail, ensure};
use tempfile::NamedTempFile;

use crate::{
    image::root::Root,
    model::{Action, digest_reader},
    repo::Snapshot,
};

const BLOCK: usize = 4096;
const WINDOW: usize = 128 * 1024;
const INDEX_BLOCKS: u64 = 65536;
const ROLL: u64 = 257;
const COPY: u8 = 1;
const LITERAL: u8 = 2;
const V1_FILE_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Format {
    V1,
    V2,
}

impl Format {
    pub(super) fn for_lengths(base: u64, new: u64) -> Self {
        if base.max(new) <= V1_FILE_LIMIT {
            Self::V1
        } else {
            Self::V2
        }
    }

    fn magic(self) -> &'static [u8; 5] {
        match self {
            Self::V1 => b"CPSD1",
            Self::V2 => b"CPSD2",
        }
    }

    fn number_width(self) -> u64 {
        match self {
            Self::V1 => 4,
            Self::V2 => 8,
        }
    }

    pub(super) fn patch_limit(self, size: u64) -> u64 {
        let useful = size - size / 8;
        match self {
            Self::V1 => useful.min(16 * 1024 * 1024),
            Self::V2 => useful,
        }
    }

    pub(super) fn target_name(self, base: &str, result: &str) -> String {
        let version = match self {
            Self::V1 => 1,
            Self::V2 => 2,
        };
        format!("delta-v{version}-{base}-{result}")
    }

    fn write_number(self, output: &mut impl Write, value: u64) -> Result<()> {
        match self {
            Self::V1 => output.write_all(&u32::try_from(value)?.to_le_bytes())?,
            Self::V2 => output.write_all(&value.to_le_bytes())?,
        }
        Ok(())
    }

    fn read_number(self, patch: &mut impl Read) -> Result<u64> {
        let mut bytes = [0; 8];
        patch.read_exact(&mut bytes[..self.number_width() as usize])?;
        Ok(u64::from_le_bytes(bytes))
    }
}

pub(super) fn verified_file(mut file: File, hash: &str, size: u64) -> Result<File> {
    ensure!(file.metadata()?.len() == size, "invalid delta input length");
    let observed = digest_reader((&mut file).take(size.saturating_add(1)))?;
    ensure!(
        file.stream_position()? == size && observed == hash,
        "delta input failed verification"
    );
    file.rewind()?;
    Ok(file)
}

pub(super) fn export(
    root: &Root,
    output: &Path,
    previous: &Snapshot,
    current: &Snapshot,
    seen: &mut BTreeSet<(String, String)>,
) -> Result<()> {
    let mut by_package = BTreeMap::<_, BTreeMap<_, _>>::new();
    for package in &previous.packages {
        let manifest = &package.manifest;
        by_package
            .entry((manifest.name.as_str(), manifest.architecture.as_deref()))
            .or_default()
            .insert(manifest.version, manifest);
    }
    for package in &current.packages {
        let manifest = &package.manifest;
        let Some(versions) =
            by_package.get(&(manifest.name.as_str(), manifest.architecture.as_deref()))
        else {
            continue;
        };
        if versions.contains_key(&manifest.version) {
            continue;
        }
        let Some((_, base)) = versions.range(..manifest.version).next_back() else {
            continue;
        };
        let files = |action: &Action| {
            matches!(
                action,
                Action::File {
                    sha256: Some(_),
                    size: Some(_),
                    ..
                }
            )
        };
        let bases: BTreeMap<_, _> = base
            .action
            .iter()
            .filter(|action| files(action))
            .map(|action| ((action.path(), action.selectors()), action))
            .collect();
        for action in &manifest.action {
            let Action::File {
                sha256: Some(new),
                size: Some(new_size),
                ..
            } = action
            else {
                continue;
            };
            let Some(Action::File {
                sha256: Some(old),
                size: Some(old_size),
                ..
            }) = bases.get(&(action.path(), action.selectors()))
            else {
                continue;
            };
            if old == new
                || *old_size < BLOCK as u64
                || *new_size < BLOCK as u64
                || !seen.insert((old.clone(), new.clone()))
            {
                continue;
            }
            let base = verified_file(root.file(&format!("blobs/{old}"))?, old, *old_size)?;
            let new_file = verified_file(root.file(&format!("blobs/{new}"))?, new, *new_size)?;
            let format = Format::for_lengths(*old_size, *new_size);
            let mut patch = NamedTempFile::new_in(output)?;
            let complete = {
                let mut writer = BufWriter::new(patch.as_file_mut());
                let complete = encode(&base, *old_size, &new_file, *new_size, format, &mut writer)?;
                writer.flush()?;
                complete
            };
            if complete {
                patch.as_file().sync_all()?;
                patch.persist_noclobber(output.join(format.target_name(old, new)))?;
            }
        }
    }
    Ok(())
}

fn weak_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |hash, &byte| {
        hash.wrapping_mul(ROLL).wrapping_add(u64::from(byte))
    })
}

struct FileWindow<'a> {
    file: &'a File,
    length: u64,
    start: u64,
    bytes: Vec<u8>,
}

impl<'a> FileWindow<'a> {
    fn new(file: &'a File, length: u64) -> Self {
        Self {
            file,
            length,
            start: 0,
            bytes: Vec::with_capacity(WINDOW),
        }
    }

    fn at(&mut self, offset: u64, minimum: usize) -> Result<&[u8]> {
        if let Some(index) = offset.checked_sub(self.start)
            && index
                .checked_add(minimum as u64)
                .is_some_and(|end| end <= self.bytes.len() as u64)
        {
            return Ok(&self.bytes[index as usize..]);
        }
        let size = self.length.saturating_sub(offset).min(WINDOW as u64) as usize;
        ensure!(size >= minimum, "delta window exceeds the input file");
        self.bytes.resize(size, 0);
        self.file.read_exact_at(&mut self.bytes, offset)?;
        self.start = offset;
        Ok(&self.bytes)
    }
}

fn extend_match(
    base: &mut FileWindow<'_>,
    new: &mut FileWindow<'_>,
    base_offset: u64,
    new_offset: u64,
) -> Result<u64> {
    let mut length = BLOCK as u64;
    while base_offset + length < base.length && new_offset + length < new.length {
        let old = base.at(base_offset + length, 1)?;
        let updated = new.at(new_offset + length, 1)?;
        let width = old.len().min(updated.len());
        if old[..width] != updated[..width] {
            let matching = old.iter().zip(updated).take_while(|(a, b)| a == b).count();
            return Ok(length + matching as u64);
        }
        length += width as u64;
    }
    Ok(length)
}

fn copy_range(mut file: &File, offset: u64, length: u64, output: &mut impl Write) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    let copied = io::copy(&mut file.take(length), output)?;
    ensure!(copied == length, "truncated delta input");
    Ok(())
}

struct PatchWriter<'a, W> {
    format: Format,
    output: &'a mut W,
    remaining: u64,
}

impl<W: Write> PatchWriter<'_, W> {
    fn literal(&mut self, file: &File, offset: u64, length: u64) -> Result<bool> {
        if length == 0 {
            return Ok(true);
        }
        let header = 1 + self.format.number_width();
        if length > self.remaining.saturating_sub(header) {
            return Ok(false);
        }
        self.remaining -= header + length;
        self.output.write_all(&[LITERAL])?;
        self.format.write_number(self.output, length)?;
        copy_range(file, offset, length, self.output)?;
        Ok(true)
    }

    fn copy(&mut self, offset: u64, length: u64) -> Result<bool> {
        let header = 1 + 2 * self.format.number_width();
        if header > self.remaining {
            return Ok(false);
        }
        self.remaining -= header;
        self.output.write_all(&[COPY])?;
        self.format.write_number(self.output, offset)?;
        self.format.write_number(self.output, length)?;
        Ok(true)
    }
}

/// Sample aligned blocks in the old file and scan at byte offsets in the new file.
fn encode(
    base_file: &File,
    base_length: u64,
    new_file: &File,
    new_length: u64,
    format: Format,
    output: &mut impl Write,
) -> Result<bool> {
    let block = BLOCK as u64;
    if base_length < block || new_length < block {
        return Ok(false);
    }
    // The number of indexed blocks stays bounded even for very large files.
    let stride = (base_length / block).div_ceil(INDEX_BLOCKS) * block;
    let mut blocks = HashMap::new();
    let mut sample = [0; BLOCK];
    let mut offset = 0;
    while offset <= base_length - block {
        base_file.read_exact_at(&mut sample, offset)?;
        blocks.entry(weak_hash(&sample)).or_insert(offset);
        offset += stride;
    }

    output.write_all(format.magic())?;
    let mut patch = PatchWriter {
        format,
        output,
        remaining: format.patch_limit(new_length) - format.magic().len() as u64,
    };
    let high = ROLL.wrapping_pow((BLOCK - 1) as u32);
    let mut base = FileWindow::new(base_file, base_length);
    let mut new = FileWindow::new(new_file, new_length);
    let mut position = 0;
    let mut literal_start = 0;
    'scan: while position <= new_length - block {
        let window = new.at(position, BLOCK)?;
        let mut index = 0;
        let mut hash = weak_hash(&window[..BLOCK]);
        while index + BLOCK <= window.len() {
            if let Some(&offset) = blocks.get(&hash)
                && base.at(offset, BLOCK)?[..BLOCK] == window[index..index + BLOCK]
            {
                let matched = position + index as u64;
                let length = extend_match(&mut base, &mut new, offset, matched)?;
                if !patch.literal(new_file, literal_start, matched - literal_start)?
                    || !patch.copy(offset, length)?
                {
                    return Ok(false);
                }
                position = matched + length;
                literal_start = position;
                continue 'scan;
            }
            if let Some(&byte) = window.get(index + BLOCK) {
                hash = hash
                    .wrapping_sub(u64::from(window[index]).wrapping_mul(high))
                    .wrapping_mul(ROLL)
                    .wrapping_add(u64::from(byte));
            }
            index += 1;
        }
        position += index as u64;
        if position - literal_start > patch.remaining {
            return Ok(false);
        }
    }
    patch.literal(new_file, literal_start, new_length - literal_start)
}

pub(super) fn reconstruct(
    format: Format,
    base: &File,
    base_length: u64,
    mut patch: impl Read,
    output: &mut impl Write,
    size: u64,
) -> Result<()> {
    let mut magic = [0; 5];
    patch.read_exact(&mut magic)?;
    ensure!(magic == *format.magic(), "unknown delta format");
    let mut remaining = size;
    let mut tag = [0; 1];
    while remaining != 0 {
        patch.read_exact(&mut tag)?;
        let offset = match tag[0] {
            COPY => Some(format.read_number(&mut patch)?),
            LITERAL => None,
            _ => bail!("invalid delta instruction"),
        };
        let length = format.read_number(&mut patch)?;
        ensure!(
            length > 0 && length <= remaining,
            "delta output length mismatch"
        );
        if let Some(offset) = offset {
            ensure!(
                offset
                    .checked_add(length)
                    .is_some_and(|end| end <= base_length),
                "delta copy exceeds base file"
            );
            copy_range(base, offset, length, output)?;
        } else {
            let copied = io::copy(&mut (&mut patch).take(length), output)?;
            ensure!(copied == length, "truncated delta literal");
        }
        remaining -= length;
    }
    ensure!(
        patch.read(&mut tag)? == 0,
        "unexpected trailing delta instructions"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(bytes: &[u8]) -> Result<File> {
        let mut file = tempfile::tempfile()?;
        file.write_all(bytes)?;
        Ok(file)
    }

    #[test]
    fn reuses_shifted_bytes_and_bounds_reconstruction() -> Result<()> {
        let base: Vec<_> = (0..BLOCK * 80)
            .map(|i| ((i / BLOCK) * 19 + i % 17) as u8)
            .collect();
        let mut new = b"inserted".to_vec();
        new.extend_from_slice(&base);
        new[WINDOW + 13] ^= 1;
        let base_file = input(&base)?;
        let new_file = input(&new)?;
        let base_length = base.len() as u64;
        let new_length = new.len() as u64;
        let mut patch = Vec::new();
        assert!(encode(
            &base_file,
            base_length,
            &new_file,
            new_length,
            Format::V1,
            &mut patch
        )?);
        assert!(patch.len() < new.len() / 4);
        let mut output = Vec::new();
        reconstruct(
            Format::V1,
            &base_file,
            base_length,
            patch.as_slice(),
            &mut output,
            new_length,
        )?;
        assert_eq!(output, new);
        let invalid = |base_length, patch: &[u8], size| {
            reconstruct(
                Format::V1,
                &base_file,
                base_length,
                patch,
                &mut Vec::new(),
                size,
            )
            .is_err()
        };
        assert!(invalid(BLOCK as u64, &patch, new_length));
        assert!(invalid(base_length, &patch, 1));
        assert!(invalid(base_length, &patch[..patch.len() - 1], new_length));
        let unrelated = input(&vec![255; base.len()])?;
        assert!(!encode(
            &base_file,
            base_length,
            &unrelated,
            base_length,
            Format::V1,
            &mut Vec::new()
        )?);
        Ok(())
    }

    #[test]
    fn v2_reconstructs_from_beyond_four_gibibytes() -> Result<()> {
        let base = tempfile::tempfile()?;
        let offset = 1u64 << 32;
        let bytes = b"data beyond the v1 offset range";
        let length = bytes.len() as u64;
        let base_length = offset + length;
        base.write_all_at(bytes, offset)?;
        let mut patch = b"CPSD2".to_vec();
        patch.push(COPY);
        patch.extend_from_slice(&offset.to_le_bytes());
        patch.extend_from_slice(&length.to_le_bytes());
        let mut output = Vec::new();
        reconstruct(
            Format::V2,
            &base,
            base_length,
            patch.as_slice(),
            &mut output,
            length,
        )?;
        assert_eq!(output, bytes);
        Ok(())
    }
}
