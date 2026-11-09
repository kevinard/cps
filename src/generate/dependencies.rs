use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};

use crate::{
    image::root::Root,
    kdl_format::{
        ImageDefinition, parse_manifest, publish_policy::validate_elf_dirs, read_image_definition,
    },
    model::{
        Action, Dependency, Manifest, PackageSpec, Version, VersionConstraint, parse_mode,
        validate_digest,
    },
    repo::{
        CatalogPin, MANIFEST_LIMIT, Published, catalog_index,
        profiles::{
            elf,
            runtime::{PathKind, expand_absolute, library_search, machine, native, walk_path},
        },
        read_key, read_limited, read_package,
    },
};

type Package<'a> = (&'a str, &'a Version, &'a str);

/// Generate reviewable KDL require lines and diagnostics using only local inputs.
pub fn suggest_dependencies(
    proto: &Path,
    manifest: &Path,
    config: &Path,
    pins: &[String],
    library_dirs: &[String],
    path_dirs: &[String],
) -> Result<(String, Vec<String>)> {
    inspect_dependencies(
        proto,
        manifest,
        config,
        pins,
        library_dirs,
        path_dirs,
        false,
    )
}

/// Check that every detected external provider has a direct manifest requirement.
pub fn check_manifest_dependencies(
    proto: &Path,
    manifest: &Path,
    config: &Path,
    pins: &[String],
    library_dirs: &[String],
    path_dirs: &[String],
) -> Result<(String, Vec<String>)> {
    inspect_dependencies(proto, manifest, config, pins, library_dirs, path_dirs, true)
}

fn inspect_dependencies(
    proto: &Path,
    manifest: &Path,
    config: &Path,
    pins: &[String],
    library_dirs: &[String],
    path_dirs: &[String],
    check: bool,
) -> Result<(String, Vec<String>)> {
    let config = read_image_definition(config)?;
    let machine = machine(&config.arch)?;
    validate_elf_dirs(&config.arch, library_dirs)?;
    validate_elf_dirs(&config.arch, path_dirs).context("invalid interpreter search directories")?;
    let bytes = read_limited(File::open(manifest)?, MANIFEST_LIMIT)?;
    let manifest = parse_manifest(std::str::from_utf8(&bytes)?)?;
    manifest.validate(false)?;
    manifest.check_architecture(&config.arch)?;
    let root = Root::open(proto)?;
    let providers = load_providers(&config, pins, &manifest.name)?;
    let mut paths = Paths::default();
    paths.add(&manifest, true, &config)?;
    for provider in &providers {
        paths.add(&provider.manifest, false, &config)?;
    }

    let mut report = Report {
        paths,
        suggestions: BTreeMap::new(),
        diagnostics: Vec::new(),
    };
    let selection = config.policy.selection(&config.arch);
    for action in &manifest.action {
        if !selection.applies(&manifest.name, action) {
            continue;
        }
        let (path, mode) = match action {
            Action::File { path, mode, .. } => (path, parse_mode(mode)?),
            Action::Hardlink { path, target, .. } => {
                let (Some(offer), _) = report.paths.lookup(target)? else {
                    bail!("missing hard link target {target:?}");
                };
                let Action::File { mode, .. } = offer.action else {
                    bail!("invalid hard link target {target:?}");
                };
                (path, parse_mode(mode)?)
            }
            _ => continue,
        };
        let source = match action {
            Action::Hardlink { target, .. } => target,
            _ => path,
        };
        let scan = || -> Result<_> {
            let mut file = root.file(source)?;
            let mut prefix = Vec::new();
            file.by_ref().take(256).read_to_end(&mut prefix)?;
            let elf = elf::inspect_header(&mut file, &prefix)?;
            Ok((elf, prefix))
        };
        match scan() {
            Ok((Some(info), _)) => {
                if let Err(error) = report.elf(path, &info, machine, library_dirs) {
                    report.diagnostics.push(format!("{path:?}: {error:#}"));
                }
            }
            Ok((None, prefix)) if mode & 0o111 != 0 && prefix.starts_with(b"#!") => {
                if let Err(error) = report.script(path, &prefix, path_dirs) {
                    report.diagnostics.push(format!("{path:?}: {error:#}"));
                }
            }
            Ok(_) => {}
            Err(error) => report.diagnostics.push(format!("{path:?}: {error:#}")),
        }
    }
    if check {
        report.check_requirements(&manifest)?;
    }
    Ok(report.finish())
}

fn load_providers(
    config: &ImageDefinition,
    pins: &[String],
    own_name: &str,
) -> Result<Vec<Published>> {
    let mut hashes = BTreeMap::new();
    for pin in pins {
        let (publisher, hash) = pin.split_once('=').context("expected PUBLISHER=SHA256")?;
        validate_digest(hash)?;
        ensure!(
            hashes.insert(publisher, hash).is_none(),
            "duplicate catalog pin for {publisher}"
        );
    }
    let mut selected = Vec::new();
    let mut claimed = BTreeSet::from([own_name.to_owned()]);
    for publisher in &config.publishers {
        let name = publisher.name.as_str();
        let hash = hashes
            .remove(name)
            .with_context(|| format!("missing catalog pin for {name}"))?;
        let key = read_key(&publisher.trust_key)?;
        let pin = CatalogPin::primary(name.to_owned(), hash.to_owned());
        let catalog = catalog_index(&publisher.repository, name, &key, Some(&pin))
            .with_context(|| format!("reading pinned catalog {hash} for {name}"))?;
        let mut latest = BTreeMap::new();
        for (id, entry) in &catalog.entries {
            if !entry.supports_architecture(&config.arch) {
                continue;
            }
            let spec = PackageSpec::parse(id)?;
            let version = spec.version.context("catalog package has no version")?;
            if claimed.contains(spec.name)
                || catalog
                    .obsoletions
                    .get(spec.name)
                    .is_some_and(|cutoff| version <= *cutoff)
            {
                continue;
            }
            let best = latest
                .entry(spec.name)
                .or_insert_with(|| (version.clone(), id, entry));
            if version > best.0 {
                *best = (version, id, entry);
            }
        }
        for (stem, (_, id, entry)) in latest {
            selected.push(read_package(&publisher.repository, name, id, entry)?);
            claimed.insert(stem.to_owned());
        }
        claimed.extend(catalog.obsoletions.into_keys());
    }
    ensure!(
        hashes.is_empty(),
        "catalog pin for unconfigured publisher {}",
        hashes.first_key_value().map_or("", |(name, _)| *name)
    );
    Ok(selected)
}

#[derive(Clone, Copy)]
struct Offer<'a> {
    action: &'a Action,
    package: Option<Package<'a>>,
}

#[derive(Default)]
struct Paths<'a>(BTreeMap<&'a str, Vec<Offer<'a>>>);

impl<'a> Paths<'a> {
    fn add(&mut self, manifest: &'a Manifest, local: bool, config: &ImageDefinition) -> Result<()> {
        let package = (!local).then_some((
            manifest.name.as_str(),
            &manifest.version,
            manifest.publisher.as_str(),
        ));
        let selection = config.policy.selection(&config.arch);
        for action in &manifest.action {
            if !selection.applies(&manifest.name, action) {
                continue;
            }
            let path = action.path();
            for (separator, _) in path.match_indices('/') {
                self.0.entry(&path[..separator]).or_default();
            }
            let offers = self.0.entry(path).or_default();
            ensure!(
                !local || offers.is_empty(),
                "prototype manifest selects multiple actions at {path:?}"
            );
            offers.push(Offer { action, package });
        }
        Ok(())
    }

    // A directory needs no provider. Every explicit leaf on a successful
    // path, including a symlink, can add a required package.
    fn lookup(&self, path: &str) -> Result<(Option<Offer<'a>>, Option<PathKind<'a>>)> {
        let Some((&key, offers)) = self.0.get_key_value(path) else {
            return Ok((None, None));
        };
        if offers
            .iter()
            .all(|offer| matches!(offer.action, Action::Dir { .. }))
        {
            return Ok((None, Some(PathKind::Directory)));
        }
        let first = offers[0];
        if first.package.is_some() && offers.len() > 1 {
            let ids: Vec<_> = offers
                .iter()
                .filter_map(|offer| offer.package.map(package_id))
                .collect();
            bail!("ambiguous providers for /{path}: {}", ids.join(", "));
        }
        let kind = match first.action {
            Action::Dir { .. } => PathKind::Directory,
            Action::Symlink { target, .. } => PathKind::Symlink(target),
            Action::Hardlink { target, .. } => PathKind::File(target),
            Action::File { .. } => PathKind::File(key),
        };
        Ok((Some(first), Some(kind)))
    }

    fn resolve(&self, absolute: &str, executable: bool) -> Result<Option<BTreeSet<Package<'a>>>> {
        let mut providers = BTreeSet::new();
        let target = walk_path(absolute, |path| {
            let (offer, kind) = self.lookup(path)?;
            if let Some(package) = offer.and_then(|offer| offer.package) {
                providers.insert(package);
            }
            Ok(kind)
        })?;
        let Some(target) = target else {
            return Ok(None);
        };
        let (offer, _) = self.lookup(target)?;
        let Some(Offer {
            action: Action::File { mode, .. },
            package,
        }) = offer
        else {
            bail!("invalid regular file provider at /{target}");
        };
        if executable {
            ensure!(
                parse_mode(mode)? & 0o111 != 0,
                "provider at /{target} is not executable"
            );
        }
        providers.extend(package);
        Ok(Some(providers))
    }
}

fn package_id((name, version, publisher): Package<'_>) -> String {
    format!("cps://{publisher}/{name}@{version}")
}

struct Report<'a> {
    paths: Paths<'a>,
    suggestions: BTreeMap<Package<'a>, BTreeSet<String>>,
    diagnostics: Vec<String>,
}

impl Report<'_> {
    fn check_requirements(&mut self, manifest: &Manifest) -> Result<()> {
        let declared = manifest
            .dependency
            .iter()
            .filter_map(|dependency| match dependency {
                Dependency::Require(target) => Some(target),
                _ => None,
            })
            .map(|target| {
                Ok((
                    target.package.as_str(),
                    VersionConstraint::parse(target.version.as_deref())?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        self.suggestions.retain(|&package, reasons| {
            let (name, version, _) = package;
            if declared
                .iter()
                .any(|(target, range)| *target == name && range.matches(version))
            {
                return false;
            }
            self.diagnostics.push(format!(
                "missing matching direct require for {} ({})",
                package_id(package),
                reasons.iter().next().expect("suggestion has a source")
            ));
            true
        });
        Ok(())
    }

    fn require(
        &mut self,
        source: &str,
        requirement: &str,
        candidates: &[String],
        executable: bool,
    ) {
        for path in candidates {
            match self.paths.resolve(path, executable) {
                Ok(Some(providers)) => {
                    for provider in providers {
                        self.suggestions
                            .entry(provider)
                            .or_default()
                            .insert(format!("{source:?} needs {requirement} via {path:?}"));
                    }
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    self.diagnostics
                        .push(format!("{source:?} needs {requirement}: {error:#}"));
                    return;
                }
            }
        }
        self.diagnostics.push(format!(
            "{source:?} needs {requirement}: no provider in pinned catalogs (searched {candidates:?})"
        ));
    }

    fn elf(
        &mut self,
        path: &str,
        info: &elf::Elf,
        machine: u16,
        library_dirs: &[String],
    ) -> Result<()> {
        ensure!(
            native(info, machine),
            "ELF does not match the configured architecture"
        );
        if let Some(interpreter) = &info.interpreter {
            self.require(
                path,
                &format!("ELF interpreter {interpreter:?}"),
                std::slice::from_ref(interpreter),
                true,
            );
        }
        let (dir, _) = path.rsplit_once('/').unwrap_or(("", path));
        let origin = format!("/{dir}");
        let search = library_search(info, &origin, library_dirs)?;
        for name in &info.needed {
            ensure!(!name.is_empty(), "empty ELF dependency");
            let candidates = if name.contains(['/', '$']) {
                vec![expand_absolute(name, &origin)?]
            } else {
                search
                    .iter()
                    .map(|dir| format!("{}/{name}", dir.trim_end_matches('/')))
                    .collect()
            };
            let mut requirement = format!("ELF library {name:?}");
            if let Some(versions) = info.required_versions.get(name) {
                let versions: Vec<_> = versions.keys().map(String::as_str).collect();
                requirement.push_str(&format!(" (GNU versions {versions:?})"));
            }
            self.require(path, &requirement, &candidates, false);
        }
        Ok(())
    }

    fn script(&mut self, path: &str, prefix: &[u8], path_dirs: &[String]) -> Result<()> {
        let end = prefix
            .iter()
            .position(|&byte| byte == b'\n')
            .unwrap_or(prefix.len());
        ensure!(end < 256, "shebang exceeds the supported length");
        let line = std::str::from_utf8(&prefix[2..end])?.trim_matches([' ', '\t']);
        ensure!(!line.contains(['\r', '\0']), "invalid shebang");
        let (interpreter, argument) = line.split_once([' ', '\t']).unwrap_or((line, ""));
        ensure!(
            interpreter.starts_with('/'),
            "shebang interpreter must be absolute"
        );
        self.require(
            path,
            &format!("shebang interpreter {interpreter:?}"),
            &[interpreter.to_owned()],
            true,
        );
        if interpreter.rsplit('/').next() == Some("env") {
            let argument = argument.trim_start_matches([' ', '\t']);
            let command = if let Some(words) = argument
                .strip_prefix("-S")
                .filter(|suffix| suffix.starts_with([' ', '\t']))
            {
                ensure!(
                    !words.contains(['\'', '"', '\\', '$', '#']),
                    "unsupported env -S shebang"
                );
                words.split_ascii_whitespace().next().unwrap_or("")
            } else {
                argument
            };
            ensure!(
                !command.is_empty()
                    && !command.starts_with('-')
                    && !command.contains(['/', '=', ' ', '\t']),
                "unsupported env shebang command {command:?}"
            );
            let candidates: Vec<_> = path_dirs
                .iter()
                .map(|dir| format!("{dir}/{command}"))
                .collect();
            self.require(path, &format!("env command {command:?}"), &candidates, true);
        }
        Ok(())
    }

    fn finish(self) -> (String, Vec<String>) {
        use std::fmt::Write;

        let mut text = String::new();
        for (package @ (name, version, _), reasons) in self.suggestions {
            writeln!(text, "// {}", package_id(package)).unwrap();
            for reason in reasons {
                writeln!(text, "// {reason}").unwrap();
            }
            writeln!(text, "require {name:?} version=\"={version}\"").unwrap();
        }
        (text, self.diagnostics)
    }
}
