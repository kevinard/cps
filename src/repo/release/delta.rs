use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::File,
    io::{self, Read, Write},
    path::Path,
};

use anyhow::{Result, bail, ensure};

use crate::{
    image::root::Root,
    model::{Action, digest},
    repo::{Snapshot, read_limited, write_atomic},
};

const MAGIC: &[u8] = b"CPSD1";
const BLOCK: usize = 4096;
const ROLL: u64 = 257;
const COPY: u8 = 1;
const LITERAL: u8 = 2;
pub(super) const FILE_LIMIT: u64 = 64 * 1024 * 1024;

/// Bound optional transfers and require a useful size saving.
pub(super) fn patch_limit(size: u64) -> u64 {
    (size - size / 8).min(16 * 1024 * 1024)
}

pub(super) fn target_name(base: &str, result: &str) -> String {
    format!("delta-v1-{base}-{result}")
}

pub(super) fn verified_bytes(file: File, hash: &str, size: u64) -> Result<Vec<u8>> {
    ensure!(
        size <= FILE_LIMIT && file.metadata()?.len() == size,
        "invalid delta input length"
    );
    let bytes = read_limited(file, size)?;
    ensure!(
        bytes.len() as u64 == size && digest(&bytes) == hash,
        "delta input failed verification"
    );
    Ok(bytes)
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
                || !(BLOCK as u64..=FILE_LIMIT).contains(old_size)
                || !(BLOCK as u64..=FILE_LIMIT).contains(new_size)
                || !seen.insert((old.clone(), new.clone()))
            {
                continue;
            }
            let old_bytes = verified_bytes(root.file(&format!("blobs/{old}"))?, old, *old_size)?;
            let new_bytes = verified_bytes(root.file(&format!("blobs/{new}"))?, new, *new_size)?;
            if let Some(patch) = encode(&old_bytes, &new_bytes) {
                write_atomic(&output.join(target_name(old, new)), &patch, true)?;
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

/// Index aligned blocks in the old file and scan at byte offsets in the new file.
pub(super) fn encode(base: &[u8], new: &[u8]) -> Option<Vec<u8>> {
    if !(BLOCK..=FILE_LIMIT as usize).contains(&base.len())
        || !(BLOCK..=FILE_LIMIT as usize).contains(&new.len())
    {
        return None;
    }
    let mut blocks = HashMap::new();
    for (index, block) in base.as_chunks::<BLOCK>().0.iter().enumerate() {
        blocks.entry(weak_hash(block)).or_insert(index * BLOCK);
    }
    let limit = patch_limit(new.len() as u64) as usize;
    let high = ROLL.wrapping_pow((BLOCK - 1) as u32);
    let mut patch = MAGIC.to_vec();
    let mut position = 0;
    let mut literal_start = 0;
    let mut hash = weak_hash(&new[..BLOCK]);
    while position + BLOCK <= new.len() {
        if let Some(&offset) = blocks.get(&hash)
            && base[offset..offset + BLOCK] == new[position..position + BLOCK]
        {
            let length = BLOCK
                + base[offset + BLOCK..]
                    .iter()
                    .zip(&new[position + BLOCK..])
                    .take_while(|(old, new)| old == new)
                    .count();
            literal(&mut patch, &new[literal_start..position], limit)?;
            if patch.len() + 9 > limit {
                return None;
            }
            patch.push(COPY);
            patch.extend_from_slice(&(offset as u32).to_le_bytes());
            patch.extend_from_slice(&(length as u32).to_le_bytes());
            position += length;
            literal_start = position;
            if position + BLOCK <= new.len() {
                hash = weak_hash(&new[position..position + BLOCK]);
            }
        } else {
            if let Some(&byte) = new.get(position + BLOCK) {
                hash = hash
                    .wrapping_sub(u64::from(new[position]).wrapping_mul(high))
                    .wrapping_mul(ROLL)
                    .wrapping_add(u64::from(byte));
            }
            position += 1;
        }
    }
    literal(&mut patch, &new[literal_start..], limit)?;
    Some(patch)
}

fn literal(patch: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Option<()> {
    if !bytes.is_empty() {
        if patch.len() + 5 + bytes.len() > limit {
            return None;
        }
        patch.push(LITERAL);
        patch.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        patch.extend_from_slice(bytes);
    }
    Some(())
}

fn number(patch: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    patch.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub(super) fn reconstruct(
    base: &[u8],
    mut patch: impl Read,
    output: &mut impl Write,
    size: u64,
) -> Result<()> {
    ensure!(size <= FILE_LIMIT, "delta result exceeds size limit");
    let mut magic = [0; MAGIC.len()];
    patch.read_exact(&mut magic)?;
    ensure!(magic == MAGIC, "unknown delta format");
    let mut remaining = size;
    let mut tag = [0; 1];
    while remaining != 0 {
        patch.read_exact(&mut tag)?;
        let offset = match tag[0] {
            COPY => Some(number(&mut patch)? as usize),
            LITERAL => None,
            _ => bail!("invalid delta instruction"),
        };
        let length = u64::from(number(&mut patch)?);
        ensure!(
            length > 0 && length <= remaining,
            "delta output length mismatch"
        );
        if let Some(offset) = offset {
            let Some(bytes) = offset
                .checked_add(length as usize)
                .and_then(|end| base.get(offset..end))
            else {
                bail!("delta copy exceeds base file");
            };
            output.write_all(bytes)?;
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

    #[test]
    fn reuses_shifted_bytes_and_bounds_reconstruction() -> Result<()> {
        let base: Vec<_> = (0..BLOCK * 8)
            .map(|i| ((i / BLOCK) * 19 + i % 17) as u8)
            .collect();
        let mut new = b"inserted".to_vec();
        new.extend_from_slice(&base);
        new[BLOCK * 4 + 13] ^= 1;
        let patch = encode(&base, &new).expect("mostly identical files");
        assert!(patch.len() < new.len() / 4);
        let mut output = Vec::new();
        reconstruct(&base, patch.as_slice(), &mut output, new.len() as u64)?;
        assert_eq!(output, new);
        assert!(
            reconstruct(
                &base[..BLOCK],
                patch.as_slice(),
                &mut Vec::new(),
                new.len() as u64
            )
            .is_err()
        );
        assert!(reconstruct(&base, patch.as_slice(), &mut Vec::new(), 1).is_err());
        assert!(
            reconstruct(
                &base,
                &patch[..patch.len() - 1],
                &mut Vec::new(),
                new.len() as u64
            )
            .is_err()
        );
        assert!(encode(&base, &vec![255; base.len()]).is_none());
        Ok(())
    }
}
