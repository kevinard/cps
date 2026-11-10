//! The order in which a transaction writes its paths.
//!
//! A live transaction changes files under running programs, so a program
//! started partway through should find each library it needs before the
//! files that need it. Writes therefore follow the packages' dependency
//! graph: a package's dependencies, including generated `cap/elf/...`
//! capabilities, are written before the package itself, and within a package
//! shared libraries come before other files, and executables last. Removals
//! run in the opposite order, after every new file is in place.
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
        writes.sort_by_cached_key(|write| self.key(write));
    }

    fn key(&self, write: &Write) -> (u8, usize, u8, usize, String, String) {
        let path = write.path.as_str();
        let depth = path.bytes().filter(|b| *b == b'/').count();
        let key = |class, rank, kind, depth, target: &str| {
            (class, rank, kind, depth, target.to_owned(), path.to_owned())
        };
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
            config_update_target("var/lib/cps/image/config-updates/12/etc/app.conf"),
            Some("etc/app.conf")
        );
        assert_eq!(config_update_target("var/lib/cps/image/stage/x"), None);
    }
}
