use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
};

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, openat};

use crate::{
    image::{Managed, State, root::Root},
    model::Node,
    plan::{Plan, Request},
    repo::profiles::{
        elf::{self, Elf},
        runtime::{check_library, find_library, library_search, machine, native},
    },
};

pub(super) fn applies(plan: &Plan, state: &State) -> bool {
    matches!(
        plan.request,
        Request::Install { .. }
            | Request::ExactInstall { .. }
            | Request::Remove { .. }
            | Request::Autoremove
            | Request::ChangeFacet { .. }
            | Request::Update
            | Request::UpdateProfile
    ) && !state.paths.is_empty()
        && machine(&state.settings.arch).is_ok()
        && plan.writes.iter().any(|write| write.before != write.after)
}

pub(super) fn check(
    root: &Root,
    stage: &File,
    plan: &Plan,
    previous: &BTreeMap<String, Managed>,
    next: &State,
) -> Result<()> {
    let machine = machine(&next.settings.arch)?;
    let defaults = ["/usr/lib", "/lib", "/usr/lib64", "/lib64"].map(str::to_owned);
    let mut before = Files {
        root,
        stage,
        paths: previous,
        staged: BTreeSet::new(),
        inspected: BTreeMap::new(),
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
        inspected: BTreeMap::new(),
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
        if consumer.needed.is_empty() || !native(&consumer, machine) {
            continue;
        }
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let origin = format!("/{dir}");
        let Ok(search) = library_search(&consumer, &origin, &defaults) else {
            continue;
        };
        let failures: Vec<_> = consumer
            .needed
            .iter()
            .filter_map(|needed| {
                regression(
                    &mut before,
                    &mut after,
                    &consumer,
                    machine,
                    needed,
                    &origin,
                    &search,
                )
            })
            .collect();
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
    Ok(())
}

fn regression(
    before: &mut Files<'_>,
    after: &mut Files<'_>,
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

struct Files<'a> {
    root: &'a Root,
    stage: &'a File,
    paths: &'a BTreeMap<String, Managed>,
    staged: BTreeSet<&'a str>,
    inspected: BTreeMap<&'a str, Result<Option<Elf>>>,
}

impl<'a> Files<'a> {
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

    fn scan(&self, path: &str) -> Result<Option<Elf>> {
        let (mut file, size, _) = self.open(path)?;
        let mut prefix = [0; 5];
        if size < prefix.len() as u64 {
            return Ok(None);
        }
        file.read_exact(&mut prefix)?;
        elf::inspect_header(&mut file, &prefix)
    }

    fn verified(&mut self, path: &'a str) -> Option<Option<&Elf>> {
        if !self.inspected.contains_key(path) {
            let inspected = self
                .open(path)
                .and_then(|(file, size, hash)| elf::inspect(file, size, hash));
            self.inspected.insert(path, inspected);
        }
        self.inspected.get(path)?.as_ref().ok().map(Option::as_ref)
    }
}
