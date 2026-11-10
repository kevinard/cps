//! Expand package templates against a build's destination tree.
//!
//! Each template becomes one output package. Explicit actions keep their exact
//! form. `collect` rules turn the installed files and symlinks they match into
//! explicit actions. Every installed file or symlink must belong to exactly one
//! output unless a recipe `discard` pattern drops it, so a build cannot silently
//! lose what it installed.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FileType, readlinkat};

use crate::{
    image::root::Root,
    kdl_format::build::{CollectRule, FacetChoice, Template},
    model::{Action, Manifest},
};

use super::pattern;

/// The facet of split debug information. See `strip`.
pub(crate) const DEBUG_FACET: &str = "debug.symbols";
const UNPACKAGED_SHOWN: usize = 20;

/// One entry in the destination tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Entry {
    File { executable: bool, inode: (u64, u64) },
    Symlink(String),
    Dir,
}

pub(crate) type Tree = BTreeMap<String, Entry>;

/// List the destination tree without following symlinks.
pub(crate) fn scan(root: &Root) -> Result<Tree> {
    let mut tree = Tree::new();
    root.walk(None, |path, entry, info| {
        let item = match FileType::from_raw_mode(info.st_mode) {
            FileType::Directory => Entry::Dir,
            FileType::RegularFile => Entry::File {
                executable: info.st_mode & 0o111 != 0,
                inode: (info.st_dev, info.st_ino),
            },
            FileType::Symlink => Entry::Symlink(
                readlinkat(entry, "", Vec::new())?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-UTF-8 symlink target at {path}"))?,
            ),
            _ => bail!("the build installed an unsupported filesystem object at {path}"),
        };
        tree.insert(path.to_owned(), item);
        Ok(true)
    })?;
    Ok(tree)
}

/// The facet that the built-in rules give an installed path.
pub(crate) fn default_facet(path: &str, symlink: bool) -> Option<String> {
    const RULES: &[(&str, &str)] = &[
        ("usr/include/**", "devel"),
        ("usr/lib/pkgconfig/**", "devel"),
        ("usr/share/pkgconfig/**", "devel"),
        ("usr/lib/cmake/**", "devel"),
        ("usr/share/aclocal/**", "devel"),
        ("usr/share/man/**", "doc.man"),
        ("usr/share/info/**", "doc.info"),
        ("usr/share/doc/**", "doc"),
        ("usr/lib/debug/**", DEBUG_FACET),
    ];
    if let Some((_, facet)) = RULES.iter().find(|(rule, _)| pattern::matches(rule, path)) {
        return Some((*facet).to_owned());
    }
    // An unversioned library symlink exists only for the linker.
    if symlink && pattern::matches("usr/lib/*.so", path) {
        return Some("devel".to_owned());
    }
    let language = path
        .strip_prefix("usr/share/locale/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(language, _)| language)?;
    let language: String = language
        .chars()
        .map(|ch| match ch.to_ascii_lowercase() {
            ch @ ('a'..='z' | '0'..='9' | '_' | '-') => ch,
            _ => '-',
        })
        .collect();
    language
        .starts_with(|ch: char| ch.is_ascii_alphanumeric())
        .then(|| format!("locale.{language}"))
}

fn rule_facet(rule: &CollectRule, path: &str, symlink: bool) -> Option<String> {
    match &rule.facet {
        FacetChoice::Default => default_facet(path, symlink),
        FacetChoice::None => None,
        FacetChoice::Facet(facet) => Some(facet.clone()),
    }
}

fn file_mode(rule: &CollectRule, executable: bool) -> String {
    rule.mode
        .clone()
        .unwrap_or_else(|| if executable { "0755" } else { "0644" }.to_owned())
}

fn first_rule<'a>(template: &'a Template, path: &str) -> Option<&'a CollectRule> {
    template
        .collect
        .iter()
        .find(|rule| pattern::matches(&rule.pattern, path))
}

/// The first collected path of an inode, which later collected paths of the
/// same inode may hard-link to.
struct LinkTarget {
    path: String,
    facet: Option<String>,
    variants: BTreeMap<String, String>,
    mode: String,
    uid: u32,
    gid: u32,
}

/// Expand every template against the tree and check that each installed
/// file and symlink belongs to one output or matches a discard pattern.
pub(crate) fn expand(
    templates: &[Template],
    discard: &[String],
    tree: &Tree,
) -> Result<Vec<Manifest>> {
    let name = |index: usize| &templates[index].manifest.name;
    let mut explicit = BTreeMap::<&str, usize>::new();
    for (index, template) in templates.iter().enumerate() {
        for action in &template.manifest.action {
            let path = action.path();
            if let Some(&other) = explicit.get(path).filter(|&&other| other != index) {
                bail!(
                    "{path} belongs to both {} and {}; a path may belong to only one output",
                    name(other),
                    name(index)
                );
            }
            explicit.insert(path, index);
            if matches!(action, Action::File { .. }) {
                ensure!(
                    matches!(tree.get(path), Some(Entry::File { .. })),
                    "the build did not install declared file {path} for {}",
                    name(index)
                );
            }
        }
    }

    let mut claimed = BTreeMap::<usize, Vec<(&str, &Entry, &CollectRule)>>::new();
    let mut used = BTreeSet::new();
    let mut unpackaged = Vec::new();
    for (path, entry) in tree {
        if matches!(entry, Entry::Dir) || explicit.contains_key(path.as_str()) {
            continue;
        }
        let mut owners = templates
            .iter()
            .enumerate()
            .filter_map(|(index, template)| first_rule(template, path).map(|rule| (index, rule)));
        match (owners.next(), owners.next()) {
            (Some((first, _)), Some((second, _))) => bail!(
                "{path} matches collect rules in both {} and {}; a path may belong to only one output",
                name(first),
                name(second)
            ),
            (Some((index, rule)), None) => {
                used.insert((index, rule.pattern.as_str()));
                claimed.entry(index).or_default().push((path, entry, rule));
            }
            (None, _) => {
                if !discard
                    .iter()
                    .any(|pattern| pattern::matches(pattern, path))
                {
                    unpackaged.push(path.as_str());
                }
            }
        }
    }
    if !unpackaged.is_empty() {
        let shown = unpackaged
            .iter()
            .take(UNPACKAGED_SHOWN)
            .map(|path| format!("\n  {path}"))
            .collect::<String>();
        let more = unpackaged.len().saturating_sub(UNPACKAGED_SHOWN);
        bail!(
            "the build installed {} file(s) that no output claims; add collect or file actions, or a recipe discard pattern:{shown}{}",
            unpackaged.len(),
            if more > 0 {
                format!("\n  ... and {more} more")
            } else {
                String::new()
            }
        );
    }
    for (index, template) in templates.iter().enumerate() {
        for rule in &template.collect {
            ensure!(
                used.contains(&(index, rule.pattern.as_str())),
                "collect rule {} in {} matched no installed files",
                rule.pattern,
                name(index)
            );
        }
    }

    let mut outputs = Vec::with_capacity(templates.len());
    for (index, template) in templates.iter().enumerate() {
        let mut manifest = template.manifest.clone();
        let mut inodes = BTreeMap::<(u64, u64), LinkTarget>::new();
        for (path, entry, rule) in claimed.remove(&index).unwrap_or_default() {
            let facet = rule_facet(rule, path, matches!(entry, Entry::Symlink(_)));
            let variants = rule.variants.clone();
            manifest.action.push(match entry {
                Entry::Symlink(target) => Action::Symlink {
                    path: path.to_owned(),
                    target: target.clone(),
                    uid: rule.uid,
                    gid: rule.gid,
                    facet,
                    variants,
                },
                Entry::File { executable, inode } => {
                    let mode = file_mode(rule, *executable);
                    // A collected hard link shares the first collected path of its inode
                    // when every image that selects the alias also selects that path and
                    // the alias's own rule gives it the same metadata.
                    match inodes.get(inode) {
                        Some(link)
                            if (link.facet.is_none() || link.facet == facet)
                                && link.variants == variants
                                && !rule.preserve
                                && (&link.mode, link.uid, link.gid)
                                    == (&mode, rule.uid, rule.gid) =>
                        {
                            Action::Hardlink {
                                path: path.to_owned(),
                                target: link.path.clone(),
                                facet,
                                variants,
                            }
                        }
                        _ => {
                            if !rule.preserve {
                                inodes.entry(*inode).or_insert_with(|| LinkTarget {
                                    path: path.to_owned(),
                                    facet: facet.clone(),
                                    variants: variants.clone(),
                                    mode: mode.clone(),
                                    uid: rule.uid,
                                    gid: rule.gid,
                                });
                            }
                            Action::File {
                                path: path.to_owned(),
                                mode,
                                uid: rule.uid,
                                gid: rule.gid,
                                preserve: rule.preserve,
                                facet,
                                variants,
                                sha256: None,
                                size: None,
                                capabilities: None,
                                original_name: None,
                            }
                        }
                    }
                }
                Entry::Dir => unreachable!("directories are never claimed"),
            });
        }
        manifest
            .validate(false)
            .with_context(|| format!("expanding {}", manifest.name))?;
        outputs.push(manifest);
    }
    Ok(outputs)
}

/// Check that an expanded manifest is what its template can produce: the same
/// package metadata and explicit actions, followed only by actions that its
/// collect rules or debug-information splitting generate.
pub(crate) fn check_expansion(template: &Template, expanded: &Manifest, strip: bool) -> Result<()> {
    let explicit = &template.manifest.action;
    let mut metadata = expanded.clone();
    metadata.action.clear();
    let mut expected = template.manifest.clone();
    expected.action.clear();
    ensure!(
        metadata == expected,
        "expanded manifest for {} differs from its template outside the collected actions",
        expanded.name
    );
    ensure!(
        expanded.action.len() >= explicit.len()
            && expanded.action[..explicit.len()] == explicit[..],
        "expanded manifest for {} does not keep the template's explicit actions",
        expanded.name
    );
    for action in &expanded.action[explicit.len()..] {
        let path = action.path();
        if strip && is_debug_action(action) {
            continue;
        }
        let rule = first_rule(template, path)
            .with_context(|| format!("no collect rule in {} produces {path}", expanded.name))?;
        let selectors = action.selectors();
        let symlink = matches!(action, Action::Symlink { .. });
        let consistent = selectors.facet.map(str::to_owned) == rule_facet(rule, path, symlink)
            && selectors.variants == &rule.variants
            && match action {
                Action::File {
                    mode,
                    uid,
                    gid,
                    preserve,
                    capabilities,
                    original_name,
                    ..
                } => {
                    (mode == &file_mode(rule, true) || mode == &file_mode(rule, false))
                        && (*uid, *gid, *preserve) == (rule.uid, rule.gid, rule.preserve)
                        && capabilities.is_none()
                        && original_name.is_none()
                }
                Action::Symlink { uid, gid, .. } => (*uid, *gid) == (rule.uid, rule.gid),
                Action::Hardlink { target, .. } => {
                    !rule.preserve && link_matches_rule(&expanded.action, target, rule)
                }
                Action::Dir { .. } => false,
            };
        ensure!(
            consistent,
            "collected action for {path} in {} differs from collect rule {}",
            expanded.name,
            rule.pattern
        );
    }
    Ok(())
}

/// A collected hard link gets its target's metadata, so the target must be a
/// file with the metadata that the alias's own collect rule gives it.
fn link_matches_rule(actions: &[Action], target: &str, rule: &CollectRule) -> bool {
    actions.iter().any(|action| {
        matches!(action, Action::File { path, mode, uid, gid, preserve: false, .. }
            if path == target
                && (mode == &file_mode(rule, true) || mode == &file_mode(rule, false))
                && (*uid, *gid) == (rule.uid, rule.gid))
    })
}

/// The path of the split debug information for an ELF build ID.
pub(crate) fn debug_path(build_id: &[u8]) -> String {
    let hex = hex::encode(build_id);
    format!("usr/lib/debug/.build-id/{}/{}.debug", &hex[..2], &hex[2..])
}

fn is_debug_action(action: &Action) -> bool {
    let Action::File {
        path,
        mode,
        preserve: false,
        facet: Some(facet),
        capabilities: None,
        original_name: None,
        ..
    } = action
    else {
        return false;
    };
    let name = path
        .strip_prefix("usr/lib/debug/.build-id/")
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(prefix, name)| Some((prefix, name.strip_suffix(".debug")?)));
    mode == "0644"
        && facet == DEBUG_FACET
        && name.is_some_and(|(prefix, name)| {
            prefix.len() == 2
                && !name.is_empty()
                && prefix
                    .bytes()
                    .chain(name.bytes())
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdl_format::build::{BuildRecipe, parse, parse_template};

    fn recipe(extra: &str) -> BuildRecipe {
        let digest = "0".repeat(64);
        parse(&format!(
            r#"schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {{
    toolchain solver="sat-v2" {{
        publisher "seed" catalog="{digest}" key-sha256="{digest}"
        package "devel/base"
    }}
    version "1.2.0-1" upstream="1.2"
    manifest "compiler.kdl" sha256="{digest}"
    manifest "runtime.kdl" sha256="{digest}"
{extra}
    script "true"
}}
"#
        ))
        .unwrap()
    }

    fn file(executable: bool, inode: u64) -> Entry {
        Entry::File {
            executable,
            inode: (1, inode),
        }
    }

    fn templates(recipe: &BuildRecipe) -> Vec<Template> {
        [
            r#"schema-version 1 kind="package-manifest"
package "devel/compiler" publisher="canopa" license="GPL-3.0-or-later" {
    require "runtime/libcc" version="={version}"
    file "usr/bin/cc" mode="0755" uid=0 gid=0
    collect "usr/lib/cc/**"
    collect "usr/include/**"
    collect "usr/share/man/**"
    collect "usr/share/locale/*/**"
    collect "usr/bin/c*"
}
"#,
            r#"schema-version 1 kind="package-manifest"
package "runtime/libcc" publisher="canopa" {
    collect "usr/lib/libcc.so*"
    collect "etc/cc.conf" preserve=#true facet=#null mode="0640" gid=5
}
"#,
        ]
        .into_iter()
        .map(|text| parse_template(text, recipe).unwrap())
        .collect()
    }

    fn tree() -> Tree {
        Tree::from([
            ("usr".into(), Entry::Dir),
            ("usr/bin/cc".into(), file(true, 1)),
            ("usr/bin/c++".into(), file(true, 1)),
            ("usr/bin/cpp".into(), Entry::Symlink("cc".into())),
            ("usr/lib/cc/plugin.so".into(), file(true, 2)),
            ("usr/lib/libcc.so.1".into(), file(true, 3)),
            (
                "usr/lib/libcc.so".into(),
                Entry::Symlink("libcc.so.1".into()),
            ),
            ("usr/include/cc.h".into(), file(false, 4)),
            ("usr/share/man/man1/cc.1".into(), file(false, 5)),
            (
                "usr/share/locale/pt_BR/LC_MESSAGES/cc.mo".into(),
                file(false, 6),
            ),
            ("usr/share/info/cc.info".into(), file(false, 7)),
            ("usr/lib/libcc.la".into(), file(false, 8)),
            ("etc/cc.conf".into(), file(false, 9)),
        ])
    }

    #[test]
    fn templates_expand_into_explicit_partitioned_outputs() -> Result<()> {
        let recipe = recipe("    discard \"usr/share/info/**\"\n    discard \"**/*.la\"");
        let templates = templates(&recipe);
        let outputs = expand(&templates, &recipe.discard, &tree())?;
        let [compiler, runtime] = &outputs[..] else {
            panic!("expected two outputs");
        };
        assert_eq!(compiler.version.to_string(), "1.2.0-1");
        assert_eq!(compiler.upstream_version.as_deref(), Some("1.2"));
        assert_eq!(compiler.license.as_deref(), Some("GPL-3.0-or-later"));
        let crate::model::Dependency::Require(target) = &compiler.dependency[0] else {
            panic!("expected a require");
        };
        assert_eq!(target.version.as_deref(), Some("=1.2.0-1"));
        let actions: BTreeMap<_, _> = compiler
            .action
            .iter()
            .map(|action| (action.path(), action))
            .collect();
        // usr/bin/c++ shares the explicit cc inode but is collected separately.
        assert!(matches!(actions["usr/bin/c++"], Action::File { mode, .. } if mode == "0755"));
        assert!(matches!(actions["usr/bin/cpp"], Action::Symlink { target, .. } if target == "cc"));
        assert_eq!(actions["usr/include/cc.h"].selectors().facet, Some("devel"));
        assert_eq!(
            actions["usr/share/man/man1/cc.1"].selectors().facet,
            Some("doc.man")
        );
        assert_eq!(
            actions["usr/share/locale/pt_BR/LC_MESSAGES/cc.mo"]
                .selectors()
                .facet,
            Some("locale.pt_br")
        );
        assert!(!actions.contains_key("usr/share/info/cc.info"));
        let runtime_actions: BTreeMap<_, _> = runtime
            .action
            .iter()
            .map(|action| (action.path(), action))
            .collect();
        assert_eq!(
            runtime_actions["usr/lib/libcc.so"].selectors().facet,
            Some("devel")
        );
        assert_eq!(
            runtime_actions["usr/lib/libcc.so.1"].selectors().facet,
            None
        );
        assert!(matches!(
            runtime_actions["etc/cc.conf"],
            Action::File { mode, gid: 5, preserve: true, facet: None, .. } if mode == "0640"
        ));
        for (template, output) in templates.iter().zip(&outputs) {
            check_expansion(template, output, true)?;
        }
        Ok(())
    }

    #[test]
    fn unclaimed_and_doubly_claimed_paths_fail_the_build() {
        let info = recipe("    discard \"usr/share/info/**\"");
        let error = expand(&templates(&info), &info.discard, &tree()).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("1 file(s) that no output claims"),
            "{message}"
        );
        assert!(message.contains("usr/lib/libcc.la"), "{message}");

        let recipe = recipe("    discard \"**\"");
        let mut templates = templates(&recipe);
        templates[1].collect.push(CollectRule {
            pattern: "usr/lib/**".into(),
            mode: None,
            uid: 0,
            gid: 0,
            preserve: false,
            facet: FacetChoice::Default,
            variants: BTreeMap::new(),
        });
        let message = format!(
            "{:#}",
            expand(&templates, &recipe.discard, &tree()).unwrap_err()
        );
        assert!(
            message.contains("usr/lib/cc/plugin.so matches collect rules in both"),
            "{message}"
        );

        let templates = self::templates(&recipe);
        let mut tree = tree();
        tree.remove("usr/include/cc.h");
        let message = format!(
            "{:#}",
            expand(&templates, &recipe.discard, &tree).unwrap_err()
        );
        assert!(
            message.contains("usr/include/** in devel/compiler matched no"),
            "{message}"
        );

        let mut tree = self::tree();
        tree.insert("usr/bin/cc".into(), Entry::Symlink("x".into()));
        assert!(expand(&templates, &recipe.discard, &tree).is_err());
    }

    #[test]
    fn collected_hard_links_and_debug_actions_pass_the_consistency_check() -> Result<()> {
        let recipe = recipe("    discard \"**\"");
        let templates = templates(&recipe);
        let tree = Tree::from([
            ("usr/bin/cc".into(), file(true, 1)),
            ("usr/bin/c1".into(), file(true, 2)),
            ("usr/bin/c2".into(), file(true, 2)),
            ("usr/lib/cc/x".into(), file(false, 3)),
            ("usr/include/cc.h".into(), file(false, 4)),
            ("usr/share/man/cc.1".into(), file(false, 5)),
            ("usr/share/locale/fr/cc.mo".into(), file(false, 6)),
            ("usr/lib/libcc.so.1".into(), file(true, 7)),
            ("etc/cc.conf".into(), file(false, 8)),
        ]);
        let mut outputs = expand(&templates, &recipe.discard, &tree)?;
        let compiler = &mut outputs[0];
        assert!(compiler.action.iter().any(|action| matches!(
            action,
            Action::Hardlink { path, target, .. } if path == "usr/bin/c2" && target == "usr/bin/c1"
        )));
        compiler.action.push(Action::File {
            path: debug_path(&[0xab, 0xcd, 0xef]),
            mode: "0644".into(),
            uid: 0,
            gid: 0,
            preserve: false,
            facet: Some(DEBUG_FACET.into()),
            variants: BTreeMap::new(),
            sha256: None,
            size: None,
            capabilities: None,
            original_name: None,
        });
        assert_eq!(
            compiler.action.last().unwrap().path(),
            "usr/lib/debug/.build-id/ab/cdef.debug"
        );
        check_expansion(&templates[0], compiler, true)?;
        assert!(check_expansion(&templates[0], compiler, false).is_err());

        let mut changed = compiler.clone();
        changed.dependency.clear();
        assert!(check_expansion(&templates[0], &changed, true).is_err());
        let mut changed = compiler.clone();
        if let Some(Action::File { mode, .. }) = changed
            .action
            .iter_mut()
            .find(|action| action.path() == "usr/include/cc.h")
        {
            *mode = "4755".into();
        }
        assert!(check_expansion(&templates[0], &changed, true).is_err());
        let mut changed = compiler.clone();
        changed.action.push(Action::Symlink {
            path: "etc/passwd".into(),
            target: "x".into(),
            uid: 0,
            gid: 0,
            facet: None,
            variants: BTreeMap::new(),
        });
        assert!(check_expansion(&templates[0], &changed, true).is_err());
        let mut changed = compiler.clone();
        changed.action.remove(0);
        assert!(check_expansion(&templates[0], &changed, true).is_err());
        Ok(())
    }

    #[test]
    fn hard_link_aliases_keep_their_own_collect_rule() -> Result<()> {
        let recipe = recipe("");
        let template = |rules: &str| {
            parse_template(
                &format!(
                    "schema-version 1 kind=\"package-manifest\"\npackage \"devel/compiler\" publisher=\"canopa\" {{\n{rules}}}\n"
                ),
                &recipe,
            )
            .unwrap()
        };
        let action = |manifest: &Manifest, path: &str| {
            manifest
                .action
                .iter()
                .find(|action| action.path() == path)
                .cloned()
                .unwrap()
        };

        // A preserved alias with its own mode stays a file.
        let state = template(
            "    collect \"usr/share/a/**\"\n    collect \"var/lib/a/state\" preserve=#true mode=\"0600\"\n",
        );
        let tree = Tree::from([
            ("usr/share/a/x".into(), file(false, 1)),
            ("var/lib/a/state".into(), file(false, 1)),
        ]);
        let output = &expand(std::slice::from_ref(&state), &[], &tree)?[0];
        assert!(matches!(
            action(output, "var/lib/a/state"),
            Action::File { mode, preserve: true, .. } if mode == "0600"
        ));
        check_expansion(&state, output, false)?;

        // A setuid alias keeps its mode.
        let helper = template(
            "    collect \"usr/bin/*\"\n    collect \"usr/sbin/helper\" mode=\"4755\"\n",
        );
        let tree = Tree::from([
            ("usr/bin/x".into(), file(true, 2)),
            ("usr/sbin/helper".into(), file(true, 2)),
        ]);
        let output = &expand(std::slice::from_ref(&helper), &[], &tree)?[0];
        assert!(matches!(
            action(output, "usr/sbin/helper"),
            Action::File { mode, .. } if mode == "4755"
        ));
        check_expansion(&helper, output, false)?;

        // An alias with the target's metadata still becomes a hard link.
        let same = template("    collect \"usr/bin/*\"\n    collect \"usr/sbin/helper\"\n");
        let linked = &expand(std::slice::from_ref(&same), &[], &tree)?[0];
        assert!(matches!(
            action(linked, "usr/sbin/helper"),
            Action::Hardlink { target, .. } if target == "usr/bin/x"
        ));
        check_expansion(&same, linked, false)?;

        // The consistency check rejects a hard link whose target has other metadata.
        assert!(check_expansion(&helper, linked, false).is_err());
        let mut changed = linked.clone();
        if let Some(Action::File { uid, .. }) = changed
            .action
            .iter_mut()
            .find(|action| action.path() == "usr/bin/x")
        {
            *uid = 1;
        }
        assert!(check_expansion(&same, &changed, false).is_err());
        Ok(())
    }

    #[test]
    fn templates_reject_a_second_version_source() {
        let recipe = recipe("");
        let template = r#"schema-version 1 kind="package-manifest"
package "devel/compiler" publisher="canopa" version="1.0.0-0" {
}
"#;
        assert!(parse_template(template, &recipe).is_err());
        assert!(
            parse_template(
                &template.replace(" version=\"1.0.0-0\"", " upstream-version=\"1\""),
                &recipe
            )
            .is_err()
        );
        assert!(
            parse_template(
                &template
                    .replace(" version=\"1.0.0-0\"", "")
                    .replace("{\n", "{\n collect \"usr/**\" facet=\"Bad\"\n"),
                &recipe
            )
            .is_err()
        );
    }
}
