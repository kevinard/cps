use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, fstat, openat};
use serde::{Deserialize, Serialize};

use crate::{
    image::{
        FileRecords, Managed, State,
        root::{Fingerprint, Observation, Root},
    },
    model::Node,
    plan::{Plan, Request},
    repo::profiles::{
        elf::{self, Elf},
        runtime::{
            DEFAULT_LIBRARY_DIRS, check_library, compatible_interpreter, find_library,
            library_search, machine, native, resolve,
        },
    },
    verify::Checked,
};

pub(super) fn applies(plan: &Plan, state: &State) -> bool {
    matches!(
        plan.request,
        Request::Install { .. }
            | Request::ExactInstall { .. }
            | Request::Remove { .. }
            | Request::Autoremove
            | Request::ChangeFacet { .. }
            | Request::ChangeVariant { .. }
            | Request::SetMediator { .. }
            | Request::Update
            | Request::UpdateProfile
    ) && !state.paths.is_empty()
        && machine(&state.settings.arch).is_ok()
        && plan.writes.iter().any(|write| write.before != write.after)
}

/// The ELF metadata the check needs, cached in the image state by content.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CachedElf {
    native_64_le: bool,
    elf64: bool,
    little_endian: bool,
    machine: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    soname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interpreter: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    needed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runpath: Option<String>,
    shared_object: bool,
    use_default_dirs: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    required_versions: BTreeMap<String, BTreeMap<String, u32>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    defined_versions: BTreeMap<String, u32>,
}

fn encode(info: Option<&Elf>) -> Result<String> {
    let cached = info.map(|info| CachedElf {
        native_64_le: info.native_64_le,
        elf64: info.elf64,
        little_endian: info.little_endian,
        machine: info.machine,
        soname: info.soname.clone(),
        interpreter: info.interpreter.clone(),
        needed: info.needed.clone(),
        runpath: info.runpath.clone(),
        shared_object: info.shared_object,
        use_default_dirs: info.use_default_dirs,
        required_versions: info.required_versions.clone(),
        defined_versions: info.defined_versions.clone(),
    });
    Ok(serde_json::to_string(&cached)?)
}

fn decode(json: &str) -> Result<Option<Elf>> {
    let cached: Option<CachedElf> = serde_json::from_str(json)?;
    Ok(cached.map(|cached| Elf {
        native_64_le: cached.native_64_le,
        elf64: cached.elf64,
        little_endian: cached.little_endian,
        machine: cached.machine,
        soname: cached.soname,
        interpreter: cached.interpreter,
        needed: cached.needed,
        runpath: cached.runpath,
        shared_object: cached.shared_object,
        use_default_dirs: cached.use_default_dirs,
        required_versions: cached.required_versions,
        defined_versions: cached.defined_versions,
    }))
}

/// Files whose contents the drift check established for this transaction,
/// with any ELF metadata cached from earlier transactions.
pub(super) struct Known<'a> {
    files: BTreeMap<&'a str, (&'a Observation, Option<&'a str>)>,
}

impl<'a> Known<'a> {
    /// Combine recorded fingerprints with the drift check's results. Only
    /// paths that the drift check covered, and whose fingerprint it did not
    /// find stale, are known.
    pub(super) fn new(records: &'a FileRecords, drift: &'a Checked, plan: &Plan) -> Self {
        let mut files = BTreeMap::new();
        if !super::drift_checked(plan) {
            return Self { files };
        }
        let changed = super::changed_paths(plan);
        for (path, record) in records {
            if !changed.contains(path.as_str()) && !drift.stale.contains(path) {
                files.insert(path.as_str(), (&record.observation, record.elf.as_deref()));
            }
        }
        for (path, observation) in &drift.observed {
            files.insert(path.as_str(), (observation, None));
        }
        Self { files }
    }
}

/// Check that the transaction keeps every managed ELF consumer's interpreter
/// and libraries. Returns newly learned ELF metadata to cache.
pub(super) fn check<'a>(
    root: &Root,
    stage: &File,
    plan: &Plan,
    previous: &BTreeMap<String, Managed>,
    next: &State,
    known: &Known<'a>,
) -> Result<Vec<(&'a str, &'a Observation, String)>> {
    let machine = machine(&next.settings.arch)?;
    let defaults = DEFAULT_LIBRARY_DIRS.map(str::to_owned);
    let mut before = Files {
        root,
        stage,
        paths: previous,
        staged: BTreeSet::new(),
        known,
        inspected: BTreeMap::new(),
        learned: BTreeMap::new(),
    };
    let mut after = Files {
        root,
        stage,
        paths: &next.paths,
        staged: plan
            .writes
            .iter()
            .filter(|write| write.needs_blob())
            .map(|write| write.path.as_str())
            .collect(),
        known,
        inspected: BTreeMap::new(),
        learned: BTreeMap::new(),
    };
    let mut broken = Vec::new();
    for (path, managed) in &next.paths {
        let source = match &managed.node {
            Node::File { .. } => path,
            Node::Hardlink { target } => target,
            _ => continue,
        };
        let Ok(Some(consumer)) = after.scan(source) else {
            continue;
        };
        if !native(&consumer, machine) {
            continue;
        }
        let mut failures = Vec::new();
        if let Some(interpreter) = &consumer.interpreter {
            failures.extend(interpreter_regression(
                &mut before,
                &mut after,
                interpreter,
                machine,
            ));
        }
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let origin = format!("/{dir}");
        if !consumer.needed.is_empty()
            && let Ok(search) = library_search(&consumer, &origin, &defaults)
        {
            failures.extend(consumer.needed.iter().filter_map(|needed| {
                regression(
                    &mut before,
                    &mut after,
                    &consumer,
                    machine,
                    needed,
                    &origin,
                    &search,
                )
            }));
        }
        if !failures.is_empty()
            && after
                .verified(source)
                .is_some_and(|info| info == Some(&consumer))
        {
            for owner in &managed.owners {
                let package = next.installed[owner].published.manifest.id();
                broken.extend(
                    failures
                        .iter()
                        .map(|failure| format!("{package}: {path}: {failure}")),
                );
            }
        }
    }
    ensure!(
        broken.is_empty(),
        "transaction would break managed shared-library dependencies:\n{}",
        broken.join("\n")
    );
    let mut learned = before.learned;
    learned.append(&mut after.learned);
    Ok(learned
        .into_iter()
        .filter_map(|(path, json)| {
            let (path, (observation, _)) = known.files.get_key_value(path.as_str())?;
            Some((*path, *observation, json))
        })
        .collect())
}

fn interpreter_regression(
    before: &mut Files<'_, '_>,
    after: &mut Files<'_, '_>,
    interpreter: &str,
    machine: u16,
) -> Option<String> {
    let old = resolve(before.paths, interpreter).ok()??;
    let new = resolve(after.paths, interpreter).ok()?;
    let old_node = &before.paths[old].node;
    if new.is_some_and(|new| new == old && &after.paths[new].node == old_node)
        || !before.root.resolves_to_file(interpreter, old).ok()?
        || before
            .root
            .regular_entry(old)
            .ok()??
            .metadata()
            .ok()?
            .mode()
            & 0o111
            == 0
        || !compatible_interpreter(old_node, before.verified(old)?, machine)
    {
        return None;
    }
    let Some(new) = new else {
        return Some(format!(
            "missing ELF interpreter {interpreter} previously provided at {old}"
        ));
    };
    (!compatible_interpreter(&after.paths[new].node, after.verified(new)?, machine))
        .then(|| format!("incompatible ELF interpreter {interpreter} at {new}"))
}

fn regression(
    before: &mut Files<'_, '_>,
    after: &mut Files<'_, '_>,
    consumer: &Elf,
    machine: u16,
    needed: &str,
    origin: &str,
    search: &[String],
) -> Option<String> {
    let (old, lookup) = find_library(before.paths, needed, origin, search).ok()??;
    let new = find_library(after.paths, needed, origin, search).ok()?;
    if new.as_ref().is_some_and(|(path, _)| *path == old)
        && before.paths[old].node == after.paths[old].node
    {
        return None;
    }
    if !before.root.resolves_to_file(&lookup, old).ok()? {
        return None;
    }
    check_library(consumer, machine, needed, old, before.verified(old)?).ok()?;
    match new {
        Some((path, _)) => check_library(consumer, machine, needed, path, after.verified(path)?)
            .err()
            .map(|error| error.to_string()),
        None => Some(format!(
            "missing ELF dependency {needed} previously provided at {old}"
        )),
    }
}

struct Files<'a, 'k> {
    root: &'a Root,
    stage: &'a File,
    paths: &'a BTreeMap<String, Managed>,
    staged: BTreeSet<&'a str>,
    known: &'a Known<'k>,
    inspected: BTreeMap<&'a str, Result<Option<Elf>>>,
    learned: BTreeMap<String, String>,
}

impl<'a> Files<'a, '_> {
    fn open(&self, path: &str) -> Result<(File, u64, &'a str)> {
        let managed = &self.paths[path];
        let Node::File { sha256, size, .. } = &managed.node else {
            anyhow::bail!("ELF inspection requires a regular file");
        };
        ensure!(
            !managed.preserve,
            "ELF inspection requires managed contents"
        );
        let file = if self.staged.contains(path) {
            File::from(openat(
                self.stage,
                sha256.as_str(),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?)
        } else {
            let entry = self.root.regular_entry(path)?.context("missing ELF file")?;
            File::open(Root::fd_path(&entry))?
        };
        Ok((file, *size, sha256))
    }

    /// The fingerprint of an image file whose contents the drift check
    /// established to match this state's manifest, and any cached metadata.
    fn trusted(&self, path: &str) -> Option<(&'a Fingerprint, Option<&'a str>)> {
        if self.staged.contains(path) {
            return None;
        }
        let managed = self.paths.get(path)?;
        let (observation, elf) = self.known.files.get(path)?;
        match &managed.node {
            Node::File { sha256, .. } if !managed.preserve && *sha256 == observation.sha256 => {
                Some((&observation.fingerprint, *elf))
            }
            _ => None,
        }
    }

    /// Read the ELF metadata of a file with trusted contents, from the cache
    /// or from the file itself, without hashing it again.
    fn trusted_metadata(&mut self, path: &str) -> Option<Result<Option<Elf>>> {
        let (fingerprint, cached) = self.trusted(path)?;
        if let Some(json) = cached {
            return Some(decode(json));
        }
        Some((|| {
            let mut file = self.root.file_matching(path, fingerprint)?;
            let info = header(&mut file, fingerprint.size)?;
            ensure!(
                Fingerprint::of(&fstat(&file)?) == *fingerprint,
                "{path} changed during ELF inspection"
            );
            self.learned.insert(path.to_owned(), encode(info.as_ref())?);
            Ok(info)
        })())
    }

    fn scan(&mut self, path: &str) -> Result<Option<Elf>> {
        if let Some(info) = self.trusted_metadata(path) {
            return info;
        }
        let (mut file, size, _) = self.open(path)?;
        header(&mut file, size)
    }

    fn verified(&mut self, path: &'a str) -> Option<Option<&Elf>> {
        if !self.inspected.contains_key(path) {
            let inspected = self.trusted_metadata(path).unwrap_or_else(|| {
                self.open(path)
                    .and_then(|(file, size, hash)| elf::inspect(file, size, hash))
            });
            self.inspected.insert(path, inspected);
        }
        self.inspected.get(path)?.as_ref().ok().map(Option::as_ref)
    }
}

fn header(file: &mut File, size: u64) -> Result<Option<Elf>> {
    let mut prefix = [0; 5];
    if size < prefix.len() as u64 {
        return Ok(None);
    }
    file.read_exact(&mut prefix)?;
    elf::inspect_header(file, &prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_metadata_round_trips() -> Result<()> {
        assert_eq!(encode(None)?, "null");
        assert!(decode("null")?.is_none());
        let info = Elf {
            native_64_le: true,
            elf64: true,
            little_endian: true,
            machine: 62,
            soname: Some("libtool.so.1".into()),
            interpreter: Some("/lib64/ld-linux-x86-64.so.2".into()),
            needed: vec!["libc.so.6".into()],
            runpath: Some("$ORIGIN/../lib".into()),
            shared_object: false,
            use_default_dirs: true,
            required_versions: BTreeMap::from([(
                "libc.so.6".into(),
                BTreeMap::from([("GLIBC_2.34".into(), 7)]),
            )]),
            defined_versions: BTreeMap::new(),
        };
        assert_eq!(decode(&encode(Some(&info))?)?, Some(info));
        assert!(decode(r#"{"native_64_le":true}"#).is_err());
        Ok(())
    }
}
