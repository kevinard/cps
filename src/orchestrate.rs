//! Read-only planning across many build recipes: a dependency-ordered build
//! queue, the rebuilds that a changed package requires, and bulk repinning
//! of toolchain catalogs. Nothing here builds, schedules, or publishes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::{
    kdl_format::{build::RECIPE_LIMIT, read_image_definition},
    model::{Dependency, PackageSpec, Version, digest, validate_digest},
    repo::{
        CatalogEntry, CatalogPin, MANIFEST_LIMIT, catalog_index, read_limited, read_trust_root,
    },
};

const RECIPE_KIND: &str = "build-recipe";

/// One package that a recipe produces, read from its manifest.
#[derive(Debug, Default)]
struct Produced {
    name: String,
    publisher: Option<String>,
    requires: Vec<String>,
    any_of: Vec<Vec<String>>,
    capabilities: Vec<String>,
    provides: Vec<String>,
}

#[derive(Debug)]
struct Recipe {
    path: PathBuf,
    toolchain: Vec<(Option<String>, String)>,
    packages: Vec<Produced>,
}

/// A recipe in build order with the recipes it follows and why.
#[derive(Debug)]
pub struct QueuedRecipe {
    pub recipe: PathBuf,
    pub packages: Vec<String>,
    pub after: Vec<(PathBuf, String)>,
}

/// Recipes in dependency order, with diagnostics for recipes read only in part.
#[derive(Debug)]
pub struct BuildOrder {
    pub recipes: Vec<QueuedRecipe>,
    pub warnings: Vec<String>,
}

/// Order the recipes found under `paths` so that each recipe follows the
/// recipes producing its toolchain packages and the runtime dependencies of
/// its own packages.
pub fn build_order(paths: &[PathBuf]) -> Result<BuildOrder> {
    let (recipes, warnings) = read_recipes(paths)?;
    let graph = Graph::new(&recipes)?;
    let order = graph.order(&recipes)?;
    Ok(BuildOrder {
        recipes: order
            .into_iter()
            .map(|index| QueuedRecipe {
                recipe: recipes[index].path.clone(),
                packages: recipes[index]
                    .packages
                    .iter()
                    .map(|package| package.name.clone())
                    .collect(),
                after: graph.edges[index]
                    .iter()
                    .map(|(&other, reasons)| {
                        (
                            recipes[other].path.clone(),
                            reasons.iter().cloned().collect::<Vec<_>>().join("; "),
                        )
                    })
                    .collect(),
            })
            .collect(),
        warnings,
    })
}

fn recipe_candidate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    // A build bundle stores its recipe as build.kdl next to its outputs.
    name != "build.kdl" && (name.ends_with(".kdl") || name.ends_with(".kdl.in"))
}

fn discover(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, found: &mut BTreeSet<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !entry.file_name().to_string_lossy().starts_with('.') {
                    walk(&path, found)?;
                }
            } else if kind.is_file() && recipe_candidate(&path) && is_recipe(&path)? {
                found.insert(path);
            }
        }
        Ok(())
    }
    let mut found = BTreeSet::new();
    for path in paths {
        if path.is_dir() {
            walk(path, &mut found)?;
        } else {
            ensure!(is_recipe(path)?, "{} is not a build recipe", path.display());
            found.insert(path.clone());
        }
    }
    ensure!(!found.is_empty(), "no build recipes found");
    Ok(found.into_iter().collect())
}

fn is_template(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "in")
}

fn read_text(path: &Path, limit: u64) -> Result<String> {
    let bytes = read_limited(
        File::open(path).with_context(|| format!("opening {}", path.display()))?,
        limit,
    )?;
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

/// Make a `.in` template parseable: drop lines that are only a placeholder,
/// such as `@TOOLCHAIN@`, and give bare `=@VALUE@` placeholders a number.
fn fill_placeholders(text: &str) -> String {
    let placeholder = |word: &str| {
        word.len() > 2
            && word.starts_with('@')
            && word.ends_with('@')
            && word[1..word.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    };
    let mut result = String::with_capacity(text.len());
    for line in text.lines() {
        if placeholder(line.trim()) {
            result.push('\n');
            continue;
        }
        let mut rest = line;
        while let Some(index) = rest.find("=@") {
            result.push_str(&rest[..=index]);
            let tail = &rest[index + 1..];
            let end = tail[1..].find('@').map(|end| end + 2);
            match end.filter(|&end| placeholder(&tail[..end])) {
                Some(end) => {
                    result.push('0');
                    rest = &tail[end..];
                }
                None => rest = tail,
            }
        }
        result.push_str(rest);
        result.push('\n');
    }
    result
}

fn parse_document(path: &Path, limit: u64) -> Result<KdlDocument> {
    let mut text = read_text(path, limit)?;
    if is_template(path) {
        text = fill_placeholders(&text);
    }
    text.parse().map_err(|error: kdl::KdlError| {
        anyhow::anyhow!("invalid KDL in {}: {error:?}", path.display())
    })
}

fn document_kind(doc: &KdlDocument) -> Option<&str> {
    doc.get("schema-version")?.get("kind")?.as_string()
}

fn is_recipe(path: &Path) -> Result<bool> {
    // Other KDL documents and unparseable templates are not recipes.
    Ok(parse_document(path, RECIPE_LIMIT)
        .ok()
        .is_some_and(|doc| document_kind(&doc) == Some(RECIPE_KIND)))
}

fn argument(node: &KdlNode) -> Option<&str> {
    node.get(0).and_then(KdlValue::as_string)
}

fn children(node: &KdlNode) -> impl Iterator<Item = &KdlNode> {
    node.children().into_iter().flat_map(KdlDocument::nodes)
}

fn read_recipes(paths: &[PathBuf]) -> Result<(Vec<Recipe>, Vec<String>)> {
    let mut recipes = Vec::new();
    let mut warnings = Vec::new();
    for path in discover(paths)? {
        let recipe = read_recipe(&path, &mut warnings)
            .with_context(|| format!("reading build recipe {}", path.display()))?;
        recipes.push(recipe);
    }
    Ok((recipes, warnings))
}

fn read_recipe(path: &Path, warnings: &mut Vec<String>) -> Result<Recipe> {
    if !is_template(path) {
        crate::kdl_format::build::parse(&read_text(path, RECIPE_LIMIT)?)?;
    }
    let doc = parse_document(path, RECIPE_LIMIT)?;
    let build = doc.get("build").context("missing build node")?;
    let mut toolchain = Vec::new();
    let mut manifests = Vec::new();
    for node in children(build) {
        match node.name().value() {
            "toolchain" => {
                for package in children(node).filter(|node| node.name().value() == "package") {
                    let selector = argument(package).context("toolchain package needs a name")?;
                    let spec = PackageSpec::parse(selector)?;
                    toolchain.push((spec.publisher.map(str::to_owned), spec.name.to_owned()));
                }
            }
            "manifest" => manifests.extend(argument(node)),
            // A recipe may declare several outputs, each with its own manifest.
            _ => manifests.extend(
                children(node)
                    .filter(|node| node.name().value() == "manifest")
                    .filter_map(argument),
            ),
        }
    }
    if toolchain.is_empty() {
        warnings.push(format!(
            "{}: the toolchain is a template placeholder, so build-time needs are unknown",
            path.display()
        ));
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut packages = Vec::new();
    for name in manifests {
        let mut manifest = dir.join(name);
        if !manifest.exists() && is_template(path) {
            manifest = dir.join(format!("{name}.in"));
        }
        if !manifest.exists() {
            warnings.push(format!(
                "{}: manifest {name} is generated elsewhere, so its packages are unknown",
                path.display()
            ));
            continue;
        }
        packages.push(
            read_produced(&manifest)
                .with_context(|| format!("reading manifest {}", manifest.display()))?,
        );
    }
    Ok(Recipe {
        path: path.to_owned(),
        toolchain,
        packages,
    })
}

fn read_produced(path: &Path) -> Result<Produced> {
    let doc = parse_document(path, MANIFEST_LIMIT)?;
    ensure!(
        document_kind(&doc) == Some("package-manifest"),
        "expected a package manifest"
    );
    let package = doc.get("package").context("missing package node")?;
    let mut produced = Produced {
        name: argument(package)
            .context("package needs a name")?
            .to_owned(),
        publisher: package
            .get("publisher")
            .and_then(KdlValue::as_string)
            .filter(|publisher| !publisher.contains('@'))
            .map(str::to_owned),
        ..Produced::default()
    };
    for node in children(package) {
        let target = argument(node).map(str::to_owned);
        match node.name().value() {
            "require" => produced.requires.extend(target),
            "require-capability" => produced.capabilities.extend(target),
            "provides" => produced.provides.extend(target),
            "require-any" => produced.any_of.push(
                children(node)
                    .filter_map(argument)
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => {}
        }
    }
    Ok(produced)
}

struct Graph {
    /// For each recipe, the recipes it must follow with the reasons.
    edges: Vec<BTreeMap<usize, BTreeSet<String>>>,
}

impl Graph {
    fn new(recipes: &[Recipe]) -> Result<Self> {
        let mut producers = BTreeMap::<&str, (usize, Option<&str>)>::new();
        let mut providers = BTreeMap::<&str, BTreeSet<(usize, &str)>>::new();
        for (index, recipe) in recipes.iter().enumerate() {
            for package in &recipe.packages {
                if let Some((other, _)) =
                    producers.insert(&package.name, (index, package.publisher.as_deref()))
                {
                    bail!(
                        "{} and {} both produce {}",
                        recipes[other].path.display(),
                        recipe.path.display(),
                        package.name
                    );
                }
                for capability in &package.provides {
                    providers
                        .entry(capability)
                        .or_default()
                        .insert((index, &package.name));
                }
            }
        }
        let mut edges = vec![BTreeMap::<usize, BTreeSet<String>>::new(); recipes.len()];
        for (index, recipe) in recipes.iter().enumerate() {
            let mut add = |other: usize, reason: String| {
                if other != index {
                    edges[index].entry(other).or_default().insert(reason);
                }
            };
            for (publisher, name) in &recipe.toolchain {
                if let Some(&(other, produced_by)) = producers.get(name.as_str())
                    && (publisher.is_none() || publisher.as_deref() == produced_by)
                {
                    add(other, format!("toolchain needs {name}"));
                }
            }
            for package in &recipe.packages {
                let owner = &package.name;
                for name in package
                    .requires
                    .iter()
                    .chain(package.any_of.iter().flatten())
                {
                    if let Some(&(other, _)) = producers.get(name.as_str()) {
                        add(other, format!("{owner} requires {name}"));
                    }
                }
                for capability in &package.capabilities {
                    for &(other, provider) in
                        providers.get(capability.as_str()).into_iter().flatten()
                    {
                        add(
                            other,
                            format!("{owner} requires {capability} from {provider}"),
                        );
                    }
                }
            }
        }
        Ok(Self { edges })
    }

    /// A topological order that prefers recipe paths in sorted order.
    fn order(&self, recipes: &[Recipe]) -> Result<Vec<usize>> {
        let mut pending: Vec<usize> = self.edges.iter().map(BTreeMap::len).collect();
        let mut dependents = vec![Vec::new(); recipes.len()];
        for (index, edges) in self.edges.iter().enumerate() {
            for &other in edges.keys() {
                dependents[other].push(index);
            }
        }
        let mut ready: BTreeSet<(&Path, usize)> = (0..recipes.len())
            .filter(|&index| pending[index] == 0)
            .map(|index| (recipes[index].path.as_path(), index))
            .collect();
        let mut order = Vec::with_capacity(recipes.len());
        while let Some((_, index)) = ready.pop_first() {
            order.push(index);
            for &dependent in &dependents[index] {
                pending[dependent] -= 1;
                if pending[dependent] == 0 {
                    ready.insert((recipes[dependent].path.as_path(), dependent));
                }
            }
        }
        if order.len() < recipes.len() {
            bail!("build dependency cycle:\n{}", self.cycle(recipes, &pending));
        }
        Ok(order)
    }

    fn cycle(&self, recipes: &[Recipe], pending: &[usize]) -> String {
        // Every unordered recipe still follows another unordered recipe,
        // so walking those edges must revisit a recipe.
        let mut index = (0..recipes.len())
            .find(|&index| pending[index] > 0)
            .expect("an unordered recipe");
        let mut seen = BTreeMap::new();
        let mut path = Vec::new();
        while !seen.contains_key(&index) {
            seen.insert(index, path.len());
            let (&next, reasons) = self.edges[index]
                .iter()
                .find(|(other, _)| pending[**other] > 0)
                .expect("an unordered predecessor");
            path.push((index, next, reasons));
            index = next;
        }
        path[seen[&index]..]
            .iter()
            .map(|(from, to, reasons)| {
                format!(
                    "  {} follows {}: {}",
                    recipes[*from].path.display(),
                    recipes[*to].path.display(),
                    reasons.iter().cloned().collect::<Vec<_>>().join("; ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One package version that changed, and how its capabilities changed.
#[derive(Debug)]
pub struct ChangedPackage {
    pub id: String,
    pub previous: Option<String>,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

/// A package to rebuild, with each reason and its recipe when known.
#[derive(Debug)]
pub struct Rebuild {
    pub id: String,
    pub reasons: Vec<String>,
    /// Whether it requires a capability that the change removed, such as a
    /// soname, so it no longer installs with the changed package.
    pub breaks: bool,
    pub recipe: Option<PathBuf>,
}

#[derive(Debug)]
pub struct RebuildPlan {
    pub changed: Vec<ChangedPackage>,
    pub rebuilds: Vec<Rebuild>,
    /// Recipes of the rebuilt packages, in build order.
    pub recipes: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

struct Offered<'a> {
    id: &'a str,
    version: Version,
    entry: &'a CatalogEntry,
}

/// List the packages and recipes that must be rebuilt after the `changed`
/// packages, from the signed dependency summaries of pinned catalogs.
pub fn rebuild_plan(
    config: &Path,
    pins: &[String],
    changed: &[String],
    recipe_paths: &[PathBuf],
) -> Result<RebuildPlan> {
    ensure!(
        !changed.is_empty(),
        "rebuild-plan needs at least one --changed package"
    );
    let config = read_image_definition(config)?;
    let mut hashes = BTreeMap::new();
    for pin in pins {
        let (publisher, hash) = pin.split_once('=').context("expected PUBLISHER=SHA256")?;
        validate_digest(hash)?;
        ensure!(
            hashes.insert(publisher, hash).is_none(),
            "duplicate catalog pin for {publisher}"
        );
    }
    let mut catalogs = Vec::new();
    for publisher in &config.publishers {
        let name = publisher.name.as_str();
        let hash = hashes
            .remove(name)
            .with_context(|| format!("missing catalog pin for {name}"))?;
        let key = read_trust_root(&publisher.trust_key)?;
        let pin = CatalogPin::primary(name.to_owned(), hash.to_owned());
        let catalog = catalog_index(&publisher.repository, name, &key, Some(&pin))
            .with_context(|| format!("reading pinned catalog {hash} for {name}"))?;
        catalogs.push((name, catalog));
    }
    ensure!(
        hashes.is_empty(),
        "catalog pin for unconfigured publisher {}",
        hashes.first_key_value().map_or("", |(name, _)| *name)
    );
    // Like the solver, take each package name from the first publisher that
    // offers an active version for the architecture.
    let mut packages = BTreeMap::<String, (&str, Vec<Offered<'_>>)>::new();
    for (publisher, catalog) in &catalogs {
        let mut offered = BTreeMap::<&str, Vec<Offered<'_>>>::new();
        for (id, entry) in &catalog.entries {
            if !entry.supports_architecture(&config.arch) {
                continue;
            }
            let spec = PackageSpec::parse(id)?;
            let version = spec.version.context("catalog package has no version")?;
            if catalog
                .obsoletions
                .get(spec.name)
                .is_some_and(|cutoff| version.semantic() <= cutoff.semantic())
            {
                continue;
            }
            offered
                .entry(spec.name)
                .or_default()
                .push(Offered { id, version, entry });
        }
        for (name, mut versions) in offered {
            versions.sort_by(|a, b| a.version.cmp(&b.version));
            packages
                .entry(name.to_owned())
                .or_insert((publisher, versions));
        }
    }

    let mut plan = RebuildPlan {
        changed: Vec::new(),
        rebuilds: Vec::new(),
        recipes: Vec::new(),
        warnings: Vec::new(),
    };
    let mut changed_names = BTreeMap::new();
    for selector in changed {
        let spec = PackageSpec::parse(selector)?;
        let (publisher, versions) = packages
            .get(spec.name)
            .with_context(|| format!("no package {} in the pinned catalogs", spec.name))?;
        if let Some(wanted) = spec.publisher {
            ensure!(
                wanted == *publisher,
                "{} resolves to publisher {publisher}, not {wanted}",
                spec.name
            );
        }
        let position = match &spec.version {
            Some(version) => versions
                .iter()
                .position(|candidate| &candidate.version == version)
                .with_context(|| {
                    format!(
                        "no version {version} of {} in the pinned catalogs",
                        spec.name
                    )
                })?,
            None => versions.len() - 1,
        };
        let current = &versions[position];
        let previous = versions[..position]
            .iter()
            .rev()
            .find(|candidate| candidate.version.semantic() < current.version.semantic());
        let now: BTreeSet<&String> = current.entry.provides().keys().collect();
        let before: BTreeSet<&String> = previous
            .map(|previous| previous.entry.provides().keys().collect())
            .unwrap_or_default();
        plan.changed.push(ChangedPackage {
            id: current.id.to_owned(),
            previous: previous.map(|previous| previous.id.to_owned()),
            removed: before
                .difference(&now)
                .map(|name| (*name).clone())
                .collect(),
            added: now
                .difference(&before)
                .map(|name| (*name).clone())
                .collect(),
        });
        ensure!(
            changed_names
                .insert(spec.name.to_owned(), (now, before))
                .is_none(),
            "{} is listed twice",
            spec.name
        );
    }

    for (name, (_, versions)) in &packages {
        if changed_names.contains_key(name) {
            continue;
        }
        let newest = versions.last().expect("a published version");
        let mut reasons = BTreeSet::new();
        let mut breaks = false;
        for dependency in newest.entry.dependencies() {
            let (targets, verb) = match dependency {
                Dependency::Require(target) => (vec![target], "requires"),
                Dependency::RequireAny { alternatives } => {
                    (alternatives.iter().collect(), "requires one of")
                }
                Dependency::Conditional { target, .. } => (vec![target], "conditionally requires"),
                Dependency::Incorporate(target) => (vec![target], "incorporates"),
                Dependency::RequireCapability(target) => {
                    for (changed, (now, before)) in &changed_names {
                        if before.contains(&target.package) && !now.contains(&target.package) {
                            breaks = true;
                            reasons.insert(format!(
                                "requires {}, which {changed} no longer provides",
                                target.package
                            ));
                        } else if now.contains(&target.package) {
                            reasons.insert(format!("requires {} from {changed}", target.package));
                        }
                    }
                    continue;
                }
                Dependency::Recommend(_) | Dependency::Exclude(_) => continue,
            };
            for target in targets {
                if changed_names.contains_key(&target.package) {
                    reasons.insert(format!("{verb} {}", target.package));
                }
            }
        }
        if !reasons.is_empty() {
            plan.rebuilds.push(Rebuild {
                id: newest.id.to_owned(),
                reasons: reasons.into_iter().collect(),
                breaks,
                recipe: None,
            });
        }
    }

    if !recipe_paths.is_empty() {
        let (recipes, warnings) = read_recipes(recipe_paths)?;
        plan.warnings = warnings;
        let order = Graph::new(&recipes)?.order(&recipes)?;
        let mut selected = BTreeSet::new();
        for rebuild in &mut plan.rebuilds {
            let name = PackageSpec::parse(&rebuild.id)?.name.to_owned();
            match recipes
                .iter()
                .position(|recipe| recipe.packages.iter().any(|package| package.name == name))
            {
                Some(index) => {
                    rebuild.recipe = Some(recipes[index].path.clone());
                    selected.insert(index);
                }
                None => plan
                    .warnings
                    .push(format!("no recipe in the given paths produces {name}")),
            }
        }
        plan.recipes = order
            .into_iter()
            .filter(|index| selected.contains(index))
            .map(|index| recipes[index].path.clone())
            .collect();
    }
    Ok(plan)
}

/// One rewritten toolchain pin.
#[derive(Debug)]
pub struct Repin {
    pub recipe: PathBuf,
    pub publisher: String,
    pub from: String,
    pub to: String,
    /// The digest of the rewritten recipe, for publish-policy approvals.
    /// Templates have none.
    pub recipe_sha256: Option<String>,
}

/// Rewrite the toolchain catalog pins of every recipe under `paths`,
/// keeping comments and formatting. With `dry_run` nothing is written.
pub fn repin_recipes(paths: &[PathBuf], pins: &[String], dry_run: bool) -> Result<Vec<Repin>> {
    let mut catalogs = BTreeMap::new();
    for pin in pins {
        let (publisher, hash) = pin.split_once('=').context("expected PUBLISHER=SHA256")?;
        crate::model::validate_publisher(publisher)?;
        validate_digest(hash)?;
        ensure!(
            catalogs.insert(publisher, hash).is_none(),
            "duplicate toolchain catalog for {publisher}"
        );
    }
    ensure!(
        !catalogs.is_empty(),
        "repin needs at least one --toolchain-catalog"
    );
    let mut changes = Vec::new();
    for path in discover(paths)? {
        let text = read_text(&path, RECIPE_LIMIT)?;
        let pins = toolchain_pins(&parse_document(&path, RECIPE_LIMIT)?);
        let mut expected = pins.clone();
        let mut rewritten = Vec::new();
        for (name, from) in &mut expected {
            if let Some(&to) = catalogs.get(name.as_str())
                // Placeholders such as @CATALOG_SHA256@ belong to the template.
                && from != to
                && validate_digest(from).is_ok()
            {
                rewritten.push((name.clone(), std::mem::replace(from, to.to_owned()), to));
            }
        }
        if rewritten.is_empty() {
            continue;
        }
        // Edit the pin text in place so comments, spacing, and template
        // placeholders stay as they are, then confirm the parsed result.
        let mut output = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let pin = rewritten.iter().find(|(name, from, _)| {
                line.trim_start()
                    .starts_with(&format!("publisher {name:?} "))
                    && line.contains(&format!("catalog=\"{from}\""))
            });
            match pin {
                Some((_, from, to)) => output.push_str(&line.replacen(
                    &format!("catalog=\"{from}\""),
                    &format!("catalog=\"{to}\""),
                    1,
                )),
                None => output.push_str(line),
            }
        }
        let mut check = output.clone();
        if is_template(&path) {
            check = fill_placeholders(&check);
        }
        let parsed: KdlDocument = check
            .parse()
            .map_err(|error: kdl::KdlError| anyhow::anyhow!("invalid KDL: {error:?}"))?;
        ensure!(
            toolchain_pins(&parsed) == expected,
            "{} spells a toolchain pin in a form that repin does not rewrite; edit it by hand",
            path.display()
        );
        let recipe_sha256 = if is_template(&path) {
            None
        } else {
            crate::kdl_format::build::parse(&output)
                .with_context(|| format!("validating repinned {}", path.display()))?;
            Some(digest(output.as_bytes()))
        };
        if !dry_run {
            write_replacing(&path, output.as_bytes())?;
        }
        changes.extend(rewritten.into_iter().map(|(publisher, from, to)| Repin {
            recipe: path.clone(),
            publisher,
            from,
            to: to.to_owned(),
            recipe_sha256: recipe_sha256.clone(),
        }));
    }
    Ok(changes)
}

fn toolchain_pins(doc: &KdlDocument) -> Vec<(String, String)> {
    doc.get("build")
        .into_iter()
        .flat_map(children)
        .filter(|node| node.name().value() == "toolchain")
        .flat_map(children)
        .filter(|node| node.name().value() == "publisher")
        .filter_map(|node| {
            Some((
                argument(node)?.to_owned(),
                node.get("catalog")?.as_string()?.to_owned(),
            ))
        })
        .collect()
}

fn write_replacing(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    let mut file = tempfile::NamedTempFile::new_in(parent.unwrap_or(Path::new(".")))?;
    file.write_all(bytes)?;
    file.as_file()
        .set_permissions(fs::metadata(path)?.permissions())?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(dir: &Path, name: &str, toolchain: &str, manifest: &str) -> Result<()> {
        let hash = "0".repeat(64);
        fs::write(
            dir.join(format!("{name}.kdl")),
            format!(
                "schema-version 1 kind=\"package-manifest\"\npackage \"{name}\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{manifest}}}\n"
            ),
        )?;
        fs::write(
            dir.join(format!("{name}.build.kdl.in")),
            format!(
                "schema-version 1 kind=\"build-recipe\"\nbuild architecture=\"@ARCH@\" epoch=@EPOCH@ {{\n    toolchain solver=\"sat-v2\" {{\n        publisher \"seed\" catalog=\"{hash}\" key-sha256=\"@KEY@\" // keep\n        package \"{toolchain}\"\n    }}\n    manifest \"{name}.kdl\" sha256=\"@MANIFEST_SHA256@\"\n    script \"true\"\n}}\n"
            ),
        )?;
        Ok(())
    }

    #[test]
    fn orders_recipes_by_toolchain_and_runtime_needs_and_reports_cycles() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path();
        recipe(path, "compiler", "seed/cc", "    require \"libc\"\n")?;
        recipe(path, "libc", "seed/cc", "    provides \"cap/libc\"\n")?;
        recipe(
            path,
            "app",
            "compiler",
            "    require-capability \"cap/libc\"\n",
        )?;
        fs::write(path.join("notes.kdl"), "schema-version 1 kind=\"other\"\n")?;
        let order = build_order(&[path.to_owned()])?;
        let names: Vec<_> = order
            .recipes
            .iter()
            .map(|queued| queued.packages.join(","))
            .collect();
        assert_eq!(names, ["libc", "compiler", "app"]);
        let app = &order.recipes[2].after;
        assert!(
            app.iter()
                .any(|(_, reason)| reason == "toolchain needs compiler")
        );
        assert!(
            app.iter()
                .any(|(_, reason)| reason == "app requires cap/libc from libc")
        );

        recipe(path, "libc", "compiler", "    provides \"cap/libc\"\n")?;
        let error = build_order(&[path.to_owned()]).unwrap_err().to_string();
        assert!(error.contains("build dependency cycle"), "{error}");
        assert!(error.contains("toolchain needs compiler"), "{error}");
        assert!(error.contains("compiler requires libc"), "{error}");
        Ok(())
    }

    #[test]
    fn repin_rewrites_digests_and_keeps_formatting() -> Result<()> {
        let dir = tempfile::tempdir()?;
        recipe(dir.path(), "app", "cc", "")?;
        let file = dir.path().join("app.build.kdl.in");
        let before = fs::read_to_string(&file)?;
        let new = "1".repeat(64);
        let dry = repin_recipes(&[dir.path().to_owned()], &[format!("seed={new}")], true)?;
        assert_eq!(dry.len(), 1);
        assert_eq!(fs::read_to_string(&file)?, before);
        repin_recipes(&[dir.path().to_owned()], &[format!("seed={new}")], false)?;
        assert_eq!(
            fs::read_to_string(&file)?,
            before.replace(&"0".repeat(64), &new)
        );
        assert!(
            repin_recipes(&[dir.path().to_owned()], &[format!("seed={new}")], false)?.is_empty()
        );
        Ok(())
    }

    #[test]
    fn placeholders_fill_without_touching_strings() {
        assert_eq!(
            fill_placeholders("@TOOLCHAIN@\nbuild epoch=@EPOCH@ x=\"@ARCH@\" y=a@b\n"),
            "\nbuild epoch=0 x=\"@ARCH@\" y=a@b\n"
        );
    }
}
