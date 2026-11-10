//! The order in which a transaction writes its paths.
//!
//! A live transaction changes files under running programs, so a program
//! started partway through should find each library it needs before the
//! files that need it. Writes therefore follow the packages' dependency
//! graph: a package's dependencies, including generated `cap/elf/...`
//! capabilities, are written before the package itself, and within a package
//! shared libraries come before other files, and executables last. A symlink
//! comes after the write that creates its target, so an upgraded
//! `libz.so.1` never points at a `libz.so.1.3.1` that does not exist yet.
//! Removals run in the opposite order, after every new file is in place.
//!
//! The order is a pure function of the plan and the two states, so a saved
//! plan, its journal, and an abort of it always agree on it.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    image::{CONFIG_UPDATES_DIRECTORY, Installed, Managed},
    model::{Dependency, Node},
};

use super::Write;

/// Position of each package in a dependency-first traversal.
struct Ranks<'a> {
    ranks: BTreeMap<&'a str, usize>,
    paths: &'a BTreeMap<String, Managed>,
}

impl<'a> Ranks<'a> {
    fn new(
        installed: &'a BTreeMap<String, Installed>,
        paths: &'a BTreeMap<String, Managed>,
    ) -> Self {
        Self {
            ranks: dependency_order(installed),
            paths,
        }
    }

    /// The rank of the package owning `path`. Unowned paths come first.
    fn of(&self, path: &str) -> usize {
        let path = config_update_target(path).unwrap_or(path);
        self.paths
            .get(path)
            .and_then(|managed| {
                managed
                    .owners
                    .iter()
                    .filter_map(|name| self.ranks.get(name.as_str()))
                    .max()
            })
            .map_or(0, |rank| rank + 1)
    }
}

/// Context for ordering the writes from one state to another.
pub(crate) struct WriteOrder<'a> {
    added: Ranks<'a>,
    removed: Ranks<'a>,
    retired: &'a BTreeSet<&'a str>,
}

impl<'a> WriteOrder<'a> {
    /// Order writes from `before` to `after`. `retired` lists source paths
    /// of configuration moves, which go only after their destinations.
    pub(crate) fn new(
        before: (
            &'a BTreeMap<String, Installed>,
            &'a BTreeMap<String, Managed>,
        ),
        after: (
            &'a BTreeMap<String, Installed>,
            &'a BTreeMap<String, Managed>,
        ),
        retired: &'a BTreeSet<&'a str>,
    ) -> Self {
        Self {
            added: Ranks::new(after.0, after.1),
            removed: Ranks::new(before.0, before.1),
            retired,
        }
    }

    pub(crate) fn sort(&self, writes: &mut [Write]) {
        let by_path: BTreeMap<&str, &Write> = writes
            .iter()
            .map(|write| (write.path.as_str(), write))
            .collect();
        let trees: Vec<&str> = writes
            .iter()
            .filter(|write| write.transition() == Some(true))
            .map(|write| write.path.as_str())
            .collect();
        let mut keys = BTreeMap::new();
        for write in writes.iter() {
            self.resolve(write, &by_path, &trees, &mut keys, 0);
        }
        writes.sort_by_cached_key(|write| keys[write.path.as_str()].clone());
    }

    /// The key of a write, after the write that creates its symlink target.
    fn resolve(
        &self,
        write: &Write,
        by_path: &BTreeMap<&str, &Write>,
        trees: &[&str],
        keys: &mut BTreeMap<String, Key>,
        hops: usize,
    ) -> Key {
        if let Some(key) = keys.get(&write.path) {
            return key.clone();
        }
        let mut key = self.key(write);
        if let Some(Node::Symlink { target, .. }) = &write.after
            && write.transition().is_none()
            && hops < MAX_HOPS
            && let Some(resolved) = link_target(&write.path, target)
        {
            // A target inside a new directory tree appears with the tree.
            let creator = trees
                .iter()
                .find(|tree| {
                    resolved
                        .strip_prefix(**tree)
                        .is_some_and(|rest| rest.starts_with('/'))
                })
                .map(|tree| by_path[tree])
                .or_else(|| {
                    by_path
                        .get(resolved.as_str())
                        .copied()
                        .filter(|creator| creator.after.is_some() && creator.path != write.path)
                });
            if let Some(creator) = creator {
                let created = self.resolve(creator, by_path, trees, keys, hops + 1);
                if (created.0, created.1, created.2) >= (key.0, key.1, key.2) {
                    (key.0, key.1, key.2, key.3) = (created.0, created.1, created.2, created.3 + 1);
                }
            }
        }
        keys.insert(write.path.clone(), key.clone());
        key
    }

    fn key(&self, write: &Write) -> Key {
        let path = write.path.as_str();
        let depth = path.bytes().filter(|b| *b == b'/').count();
        let key = |class, rank, kind, depth, target: &str| {
            (
                class,
                rank,
                kind,
                0,
                depth,
                target.to_owned(),
                path.to_owned(),
            )
        };
        match write.transition() {
            // A new directory tree replaces a file or symlink after the
            // files it can link to, and before links into it.
            Some(true) => return key(2, 0, 0, depth, ""),
            // A file or symlink replaces a directory tree after everything
            // the old tree's contents moved to, such as its symlink target.
            Some(false) => return key(5, 0, 0, depth, ""),
            None => {}
        }
        if write.removes_directory() {
            key(7, 0, 0, usize::MAX - depth, "")
        } else if self.retired.contains(path) {
            // Publish both the new path and its vendor proposal before retiring the old path.
            key(4, 0, 0, depth, "")
        } else if let Some(target) = write.hardlink_target() {
            key(3, 0, 0, 0, target)
        } else if write.after.as_ref().is_some_and(Node::is_dir) {
            key(0, 0, 0, depth, "")
        } else if let Some(node) = &write.after {
            key(1, self.added.of(path), file_kind(path, node), depth, "")
        } else {
            // Dependents go before the packages they depend on.
            key(6, usize::MAX - self.removed.of(path), 0, depth, "")
        }
    }
}

/// Class, package rank, file kind, symlink hops to an earlier write,
/// depth, hard link target, and path.
type Key = (u8, usize, u8, usize, usize, String, String);

/// Symlink chains longer than this are not followed for ordering.
const MAX_HOPS: usize = 40;

/// The image path that a symlink target names, resolved lexically from the
/// link's directory.
fn link_target(path: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        let mut parts: Vec<_> = path.split('/').collect();
        parts.pop();
        parts
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Libraries first, then other files, then executables.
fn file_kind(path: &str, node: &Node) -> u8 {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    if name.ends_with(".so") || name.contains(".so.") {
        return 0;
    }
    let directory = parent.rsplit('/').next().unwrap_or(parent);
    let executable = matches!(directory, "bin" | "sbin" | "libexec")
        || matches!(node, Node::File { mode, .. } if mode & 0o111 != 0);
    if executable { 2 } else { 1 }
}

/// The managed path that a staged vendor configuration proposal belongs to.
fn config_update_target(path: &str) -> Option<&str> {
    let rest = path
        .strip_prefix(CONFIG_UPDATES_DIRECTORY)?
        .strip_prefix('/')?;
    let (revision, target) = rest.split_once('/')?;
    revision
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then_some(target)
}

/// Number each package after everything it depends on. Cycles are broken
/// deterministically by name order.
fn dependency_order(installed: &BTreeMap<String, Installed>) -> BTreeMap<&str, usize> {
    let mut providers = BTreeMap::<&str, BTreeSet<&str>>::new();
    for (name, package) in installed {
        for capability in package.published.manifest.provides.keys() {
            providers
                .entry(capability.as_str())
                .or_default()
                .insert(name.as_str());
        }
    }
    let dependencies = |name: &str| -> Vec<&str> {
        let mut found = BTreeSet::new();
        let mut add = |target: &str| {
            if let Some((name, _)) = installed.get_key_value(target) {
                found.insert(name.as_str());
            }
        };
        for dependency in &installed[name].published.manifest.dependency {
            match dependency {
                Dependency::Require(target) | Dependency::Recommend(target) => add(&target.package),
                Dependency::RequireAny { alternatives } => {
                    for target in alternatives {
                        add(&target.package);
                    }
                }
                Dependency::Conditional { target, predicate } => {
                    if installed.contains_key(&predicate.package) {
                        add(&target.package);
                    }
                }
                Dependency::RequireCapability(target) => {
                    for provider in providers.get(target.package.as_str()).into_iter().flatten() {
                        add(provider);
                    }
                }
                Dependency::Incorporate(_) | Dependency::Exclude(_) => {}
            }
        }
        found.remove(name);
        found.into_iter().collect()
    };
    let mut ranks = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for root in installed.keys() {
        if !seen.insert(root.as_str()) {
            continue;
        }
        let mut stack = vec![(root.as_str(), dependencies(root).into_iter())];
        while let Some((name, pending)) = stack.last_mut() {
            if let Some(dependency) = pending.next() {
                if seen.insert(dependency) {
                    stack.push((dependency, dependencies(dependency).into_iter()));
                }
            } else {
                let rank = ranks.len();
                ranks.insert(*name, rank);
                stack.pop();
            }
        }
    }
    ranks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_libraries_data_and_executables() {
        let file = |mode| Node::File {
            sha256: String::new(),
            size: 0,
            mode,
            uid: 0,
            gid: 0,
            capabilities: None,
        };
        assert_eq!(file_kind("usr/lib/libz.so.1.3", &file(0o755)), 0);
        assert_eq!(file_kind("usr/lib/libz.so", &file(0o644)), 0);
        assert_eq!(file_kind("usr/share/zlib/README", &file(0o644)), 1);
        assert_eq!(file_kind("usr/lib/tool/helper", &file(0o755)), 2);
        let link = Node::Symlink {
            target: "busybox".into(),
            uid: 0,
            gid: 0,
        };
        assert_eq!(file_kind("usr/bin/sh", &link), 2);
        assert_eq!(
            config_update_target("usr/lib/sysimage/cps/config-updates/12/etc/app.conf"),
            Some("etc/app.conf")
        );
        assert_eq!(config_update_target("usr/lib/sysimage/cps/stage/x"), None);
    }

    #[test]
    fn resolves_symlink_targets_lexically() {
        assert_eq!(
            link_target("usr/lib/libz.so.1", "libz.so.1.3.1").as_deref(),
            Some("usr/lib/libz.so.1.3.1")
        );
        assert_eq!(
            link_target("usr/lib64/libz.so", "../lib/./libz.so.1").as_deref(),
            Some("usr/lib/libz.so.1")
        );
        assert_eq!(
            link_target("bin/sh", "/usr/bin/busybox").as_deref(),
            Some("usr/bin/busybox")
        );
        assert_eq!(link_target("bin", "usr/bin").as_deref(), Some("usr/bin"));
        assert_eq!(link_target("a/b", "../../..").as_deref(), None);
    }

    fn write(path: &str, after: Node) -> Write {
        Write {
            path: path.into(),
            before: None,
            after: Some(after),
            detach: false,
            local_config: false,
            config_move: None,
        }
    }

    #[test]
    fn symlinks_follow_the_writes_that_create_their_targets() {
        let file = |mode| Node::File {
            sha256: String::new(),
            size: 0,
            mode,
            uid: 0,
            gid: 0,
            capabilities: None,
        };
        let link = |target: &str| Node::Symlink {
            target: target.into(),
            uid: 0,
            gid: 0,
        };
        let dir = Node::Dir {
            mode: 0o755,
            uid: 0,
            gid: 0,
        };
        let mut tree = write("opt/tree", dir.clone());
        tree.before = Some(link("elsewhere"));
        let mut writes = vec![
            write("usr/lib/libz.so", link("libz.so.1")),
            write("usr/lib/libz.so.1", link("libz.so.1.3.1")),
            write("usr/lib/libz.so.1.3.1", file(0o755)),
            write("usr/bin/sh", link("busybox")),
            write("usr/bin/busybox", file(0o755)),
            // A library link to a data file goes after the data file.
            write("usr/lib/liba.so", link("../share/a/data")),
            write("usr/share/a/data", file(0o644)),
            // A link into a new directory tree goes after the tree.
            write("usr/lib/libt.so", link("/opt/tree/libt.so")),
            tree,
            write("opt/tree/libt.so", file(0o644)),
            // Links in a cycle still get an order.
            write("usr/share/loop-a", link("loop-b")),
            write("usr/share/loop-b", link("loop-a")),
            write("usr/lib", dir),
        ];
        let empty = (BTreeMap::new(), BTreeMap::new());
        let retired = BTreeSet::new();
        WriteOrder::new((&empty.0, &empty.1), (&empty.0, &empty.1), &retired).sort(&mut writes);
        let position = |path: &str| {
            writes
                .iter()
                .position(|write| write.path == path)
                .unwrap_or_else(|| panic!("{path}"))
        };
        for (link, target) in [
            ("usr/lib/libz.so.1", "usr/lib/libz.so.1.3.1"),
            ("usr/lib/libz.so", "usr/lib/libz.so.1"),
            ("usr/bin/sh", "usr/bin/busybox"),
            ("usr/lib/liba.so", "usr/share/a/data"),
            ("usr/lib/libt.so", "opt/tree"),
        ] {
            assert!(position(link) > position(target), "{link}: {writes:?}");
        }
        // Libraries still precede other files and executables.
        assert!(position("usr/lib/libz.so") < position("usr/bin/busybox"));
        assert!(position("usr/lib") < position("usr/lib/libz.so.1.3.1"));
    }
}
