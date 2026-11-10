//! Shared-library declarations that publication derives from ELF payloads.
//!
//! A package that ships a shared object in one of the loader's library
//! directories provides `cap/elf/ABI/SONAME`, plus `cap/elf/ABI/SONAME/VERSION`
//! for each GNU version node the object defines. A package whose ELF objects
//! name a `DT_NEEDED` library that the package does not supply itself requires
//! the matching capabilities. Only content that every image installs takes
//! part: facets and variants never gate dependencies.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek},
};

use anyhow::{Context, Result, ensure};

use crate::{
    model::{ARCH_VARIANT, Action, Dependency, DependencyTarget, FileDigest, Manifest},
    repo::profiles::{
        elf::{self, Elf, library_capability, needed_capabilities, version_capability},
        runtime::{PathKind, expand_absolute, walk_path},
    },
};

/// Directories where a shared object is public. These are the dynamic
/// loader's default directories, including the multilib and multiarch
/// layouts, so a library elsewhere is private to its package. The set is
/// fixed so that builders and publishers derive identical declarations.
pub(crate) const LIBRARY_DIRS: [&str; 14] = [
    "/usr/lib",
    "/lib",
    "/usr/lib64",
    "/lib64",
    "/usr/lib32",
    "/lib32",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/x86_64-linux-gnu",
    "/usr/lib/i386-linux-gnu",
    "/lib/i386-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/usr/lib/arm-linux-gnueabihf",
    "/lib/arm-linux-gnueabihf",
];

/// Declarations generated for one package.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct Declarations {
    pub provides: BTreeSet<String>,
    /// Each required capability with the payload paths that need it.
    pub requires: BTreeMap<String, BTreeSet<String>>,
}

impl Declarations {
    /// Add the declarations to an authored manifest, after its own.
    pub(crate) fn attach(&self, manifest: &mut Manifest) {
        for name in &self.provides {
            manifest.provides.insert(name.clone(), None);
        }
        manifest.dependency.extend(self.requires.keys().map(|name| {
            Dependency::RequireCapability(DependencyTarget {
                package: name.clone(),
                version: None,
            })
        }));
    }

    /// Render the declarations as KDL lines for review.
    pub(crate) fn text(&self) -> String {
        use std::fmt::Write;

        let mut text = String::new();
        for name in &self.provides {
            writeln!(text, "provides {name:?}").unwrap();
        }
        for (name, paths) in &self.requires {
            let paths: Vec<_> = paths.iter().collect();
            writeln!(text, "require-capability {name:?} // {paths:?}").unwrap();
        }
        text
    }
}

/// Whether every image that installs the package installs this action.
pub(crate) fn unconditional(manifest: &Manifest, action: &Action) -> bool {
    let selectors = action.selectors();
    selectors.facet.is_none()
        && selectors.variants.iter().all(|(name, value)| {
            name == ARCH_VARIANT && manifest.architecture.as_deref() == Some(value)
        })
}

/// The manifest that publication signs: the authored manifest with payload
/// digests and the declarations generated from those payloads.
pub(crate) fn published_manifest(
    mut manifest: Manifest,
    artifacts: &BTreeMap<String, FileDigest>,
    open: impl Fn(&str, &FileDigest) -> Result<File>,
) -> Result<Manifest> {
    payload_declarations(&manifest, artifacts, open)?.attach(&mut manifest);
    manifest.with_payloads(artifacts)
}

fn payload_declarations(
    manifest: &Manifest,
    artifacts: &BTreeMap<String, FileDigest>,
    open: impl Fn(&str, &FileDigest) -> Result<File>,
) -> Result<Declarations> {
    Payloads::scan(manifest, |path| {
        let digest = artifacts
            .get(path)
            .with_context(|| format!("missing package payload {path}"))?;
        let mut file = open(path, digest)?;
        // Skip data files without hashing them a second time.
        let mut magic = [0; 4];
        let length = file.by_ref().take(4).read(&mut magic)?;
        if magic[..length] != object::elf::ELFMAG {
            return Ok(None);
        }
        file.rewind()?;
        elf::inspect(file, digest.size, &digest.sha256)
    })?
    .declarations()
}

/// The unconditional content of one package and its ELF objects.
pub(crate) struct Payloads<'a> {
    actions: BTreeMap<&'a str, &'a Action>,
    dirs: BTreeSet<&'a str>,
    elf: BTreeMap<&'a str, Elf>,
}

impl<'a> Payloads<'a> {
    /// Read the unconditional regular files with `inspect`, which returns
    /// `None` for a file that is not a loadable ELF object.
    pub(crate) fn scan(
        manifest: &'a Manifest,
        mut inspect: impl FnMut(&str) -> Result<Option<Elf>>,
    ) -> Result<Self> {
        let mut payloads = Self {
            actions: BTreeMap::new(),
            dirs: BTreeSet::new(),
            elf: BTreeMap::new(),
        };
        for action in &manifest.action {
            if !unconditional(manifest, action) {
                continue;
            }
            let path = action.path();
            for (separator, _) in path.match_indices('/') {
                payloads.dirs.insert(&path[..separator]);
            }
            payloads.actions.insert(path, action);
            if let Action::File { .. } = action
                && let Some(info) =
                    inspect(path).with_context(|| format!("inspecting ELF payload {path}"))?
            {
                payloads.elf.insert(path, info);
            }
        }
        Ok(payloads)
    }

    /// Each unconditional regular file or hard link that is an ELF object.
    pub(crate) fn objects(&self) -> impl Iterator<Item = (&'a str, &Elf)> {
        self.actions.iter().filter_map(|(&path, action)| {
            let source = match action {
                Action::File { .. } => path,
                Action::Hardlink { target, .. } => target.as_str(),
                _ => return None,
            };
            self.elf.get(source).map(|info| (path, info))
        })
    }

    fn resolve(&self, absolute: &str) -> Result<Option<&'a str>> {
        walk_path(absolute, |path| {
            Ok(match self.actions.get(path) {
                Some(Action::Dir { .. }) => Some(PathKind::Directory),
                Some(Action::File { path, .. }) => Some(PathKind::File(path)),
                Some(Action::Hardlink { target, .. }) => Some(PathKind::File(target)),
                Some(Action::Symlink { target, .. }) => Some(PathKind::Symlink(target)),
                None => self.dirs.contains(path).then_some(PathKind::Directory),
            })
        })
    }

    /// The shared object for `abi` that this package installs at `absolute`.
    fn library(&self, absolute: &str, abi: &str) -> Option<&Elf> {
        let target = self.resolve(absolute).ok()??;
        self.elf
            .get(target)
            .filter(|info| info.shared_object && info.abi() == abi)
    }

    /// The `DT_NEEDED` entries of one object that this package does not
    /// satisfy through the object's search path, with their capabilities.
    /// Entries spelled as paths are left to path-based review.
    pub(crate) fn requirements(
        &self,
        path: &str,
        info: &Elf,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let abi = info.abi();
        let origin = format!("/{}", path.rsplit_once('/').map_or("", |(dir, _)| dir));
        let mut search = Vec::new();
        // An object with an unsupported RUNPATH token can still name a
        // public library, which the default directories find.
        for dir in info.runpath.iter().flat_map(|runpath| runpath.split(':')) {
            if let Ok(dir) = expand_absolute(dir, &origin) {
                search.push(dir);
            }
        }
        if info.use_default_dirs {
            search.extend(LIBRARY_DIRS.map(str::to_owned));
        }
        let mut result = Vec::new();
        for needed in &info.needed {
            ensure!(!needed.is_empty(), "empty ELF dependency in {path}");
            if needed.contains(['/', '$']) {
                continue;
            }
            let local = search.iter().any(|dir| {
                self.library(&format!("{}/{needed}", dir.trim_end_matches('/')), &abi)
                    .is_some()
            });
            if !local {
                result.push((needed.clone(), needed_capabilities(info, needed)?));
            }
        }
        Ok(result)
    }

    /// Capabilities for each shared object reachable by its soname in a
    /// library directory.
    pub(crate) fn provides(&self) -> Result<BTreeSet<String>> {
        let mut provides = BTreeSet::new();
        for (&path, info) in &self.elf {
            let Some(soname) = info.soname.as_deref().filter(|_| info.shared_object) else {
                continue;
            };
            if soname.contains('/') || soname.is_empty() {
                continue;
            }
            // Only the object that the loader finds by this soname provides
            // it, not another copy that records the same soname.
            if !LIBRARY_DIRS.iter().any(|dir| {
                self.resolve(&format!("{dir}/{soname}"))
                    .is_ok_and(|target| target == Some(path))
            }) {
                continue;
            }
            let abi = info.abi();
            provides.insert(library_capability(&abi, soname)?);
            for version in info.defined_versions.keys() {
                provides.insert(version_capability(&abi, soname, version)?);
            }
        }
        Ok(provides)
    }

    pub(crate) fn declarations(&self) -> Result<Declarations> {
        let provides = self.provides()?;
        let mut requires = BTreeMap::<String, BTreeSet<String>>::new();
        for (path, info) in self.objects() {
            for (_, capabilities) in self
                .requirements(path, info)
                .with_context(|| format!("reading ELF dependencies of {path}"))?
            {
                for capability in capabilities {
                    if !provides.contains(&capability) {
                        requires
                            .entry(capability)
                            .or_default()
                            .insert(path.to_owned());
                    }
                }
            }
        }
        Ok(Declarations { provides, requires })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdl_format::parse_manifest;

    fn object(soname: Option<&str>, needed: &[&str]) -> Elf {
        Elf {
            native_64_le: true,
            elf64: true,
            little_endian: true,
            machine: object::elf::EM_X86_64,
            soname: soname.map(str::to_owned),
            interpreter: None,
            needed: needed.iter().map(|name| (*name).to_owned()).collect(),
            runpath: None,
            shared_object: soname.is_some(),
            use_default_dirs: true,
            required_versions: BTreeMap::new(),
            defined_versions: BTreeMap::new(),
        }
    }

    fn manifest(body: &str) -> Manifest {
        parse_manifest(&format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"demo/pkg\" publisher=\"canopa\" version=\"1.0.0-0\" architecture=\"x86_64\" {{\n{body}}}\n"
        ))
        .unwrap()
    }

    #[test]
    fn libraries_in_library_dirs_provide_and_consumers_require() -> Result<()> {
        let manifest = manifest(
            r#"
    file "usr/lib/libdemo.so.1.0" mode="0755" uid=0 gid=0
    symlink "usr/lib/libdemo.so.1" target="libdemo.so.1.0" uid=0 gid=0
    file "usr/lib/private/libhidden.so.2" mode="0755" uid=0 gid=0
    file "usr/lib/loader-copy" mode="0755" uid=0 gid=0
    file "usr/bin/demo" mode="0755" uid=0 gid=0 arch="x86_64"
    hardlink "usr/bin/demo-alias" target="usr/bin/demo" arch="x86_64"
    file "usr/bin/optional" mode="0755" uid=0 gid=0 facet="extra"
    file "usr/lib/libgpu.so.3" mode="0755" uid=0 gid=0 variant.gpu="true"
"#,
        );
        let mut library = object(Some("libdemo.so.1"), &["libc.so.6"]);
        library.defined_versions.insert("DEMO_1".into(), 1);
        library.required_versions.insert(
            "libc.so.6".into(),
            BTreeMap::from([("GLIBC_2.34".into(), 2)]),
        );
        let mut seen = Vec::new();
        let payloads = Payloads::scan(&manifest, |path| {
            seen.push(path.to_owned());
            Ok(match path {
                "usr/lib/libdemo.so.1.0" => Some(library.clone()),
                "usr/lib/private/libhidden.so.2" => Some(object(Some("libhidden.so.2"), &[])),
                "usr/lib/loader-copy" => {
                    let mut copy = object(Some("libdemo.so.1"), &[]);
                    copy.defined_versions.insert("DEMO_2".into(), 3);
                    Some(copy)
                }
                "usr/bin/demo" => Some(object(
                    None,
                    &[
                        "libdemo.so.1",
                        "libhidden.so.2",
                        "libstdc++.so.6",
                        "$ORIGIN/x.so",
                    ],
                )),
                _ => None,
            })
        })?;
        assert_eq!(
            seen,
            [
                "usr/lib/libdemo.so.1.0",
                "usr/lib/private/libhidden.so.2",
                "usr/lib/loader-copy",
                "usr/bin/demo"
            ]
        );
        let declarations = payloads.declarations()?;
        assert_eq!(
            declarations.provides,
            BTreeSet::from([
                "cap/elf/x86_64/libdemo.so.1".to_owned(),
                "cap/elf/x86_64/libdemo.so.1/DEMO_1".to_owned(),
            ])
        );
        let requires: Vec<_> = declarations.requires.keys().map(String::as_str).collect();
        assert_eq!(
            requires,
            [
                "cap/elf/x86_64/libc.so.6/GLIBC_2.34",
                "cap/elf/x86_64/libhidden.so.2",
                "cap/elf/x86_64/libstdc++.so.6",
            ]
        );
        assert_eq!(
            declarations.requires["cap/elf/x86_64/libstdc++.so.6"],
            BTreeSet::from(["usr/bin/demo".to_owned(), "usr/bin/demo-alias".to_owned()])
        );

        let mut published = manifest.clone();
        declarations.attach(&mut published);
        assert!(
            published
                .validate(false)
                .unwrap_err()
                .to_string()
                .contains("reserved for declarations that publication generates")
        );
        Ok(())
    }

    #[test]
    fn runpath_and_abi_decide_local_libraries() -> Result<()> {
        let manifest = manifest(
            r#"
    file "opt/demo/lib/libown.so.1" mode="0755" uid=0 gid=0
    file "usr/lib/libwide.so.1" mode="0755" uid=0 gid=0
    file "opt/demo/bin/tool" mode="0755" uid=0 gid=0
"#,
        );
        let mut narrow = object(Some("libwide.so.1"), &[]);
        narrow.elf64 = false;
        narrow.machine = object::elf::EM_386;
        narrow.native_64_le = false;
        let mut tool = object(None, &["libown.so.1", "libwide.so.1"]);
        tool.runpath = Some("$ORIGIN/../lib:$LIB".into());
        let payloads = Payloads::scan(&manifest, |path| {
            Ok(match path {
                "opt/demo/lib/libown.so.1" => Some(object(Some("libown.so.1"), &[])),
                "usr/lib/libwide.so.1" => Some(narrow.clone()),
                _ => Some(tool.clone()),
            })
        })?;
        let declarations = payloads.declarations()?;
        assert_eq!(
            declarations.provides,
            BTreeSet::from(["cap/elf/i686/libwide.so.1".to_owned()])
        );
        assert_eq!(
            declarations.requires.keys().collect::<Vec<_>>(),
            ["cap/elf/x86_64/libwide.so.1"]
        );
        Ok(())
    }

    #[test]
    fn capability_names_reject_unrepresentable_sonames() {
        assert!(library_capability("x86_64", "lib/x.so").is_err());
        assert!(library_capability("x86_64", "..").is_err());
        assert!(version_capability("x86_64", "libc.so.6", "GLIBC 2").is_err());
        assert_eq!(
            version_capability("x86_64", "libstdc++.so.6", "GLIBCXX_3.4.29").unwrap(),
            "cap/elf/x86_64/libstdc++.so.6/GLIBCXX_3.4.29"
        );
    }
}
