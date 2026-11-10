use super::*;
use crate::{
    ImagePolicy,
    kdl_format::{manifest_text, parse_manifest},
    query::{facet_values, list_variants},
};

fn change_facets(entries: &[(&str, Option<bool>)], with_dependencies: bool) -> Request {
    Request::ChangeFacet {
        facets: entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), *value))
            .collect(),
        with_dependencies,
    }
}

fn change_variants(entries: &[(&str, Option<&str>)]) -> Request {
    Request::ChangeVariant {
        variants: entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.map(str::to_owned)))
            .collect(),
    }
}

fn rule(root: &Path, package: Option<&str>, facet: &str) -> Result<(bool, String)> {
    let row = facet_values(root, package, &[facet.to_owned()])?
        .pop()
        .expect("one facet row");
    Ok((row.value, row.source.to_string()))
}

fn recomposed(plan: &Plan) -> Vec<String> {
    plan.summary()
        .lines()
        .filter_map(|line| line.strip_prefix("Recompose "))
        .map(str::to_owned)
        .collect()
}

const ZLIB_H: &str = "usr/include/zlib.h";
const SQLITE_H: &str = "usr/include/sqlite3.h";
const APP_H: &str = "usr/include/app.h";
const FR: &str = "usr/share/locale/fr/zlib.mo";
const DE: &str = "usr/share/locale/de/sqlite.mo";
const DEBUG: &str = "usr/lib/debug/libz.so.1.debug";
const LIB32: &str = "usr/lib32/libsqlite3.so.0";

fn publish_libraries(f: &Fixture) -> Result<()> {
    f.package(
        "library/zlib",
        "1.0.0-0",
        f.file("usr/lib/libz.so.1", "zlib", "0644", "")?
            + &f.file(ZLIB_H, "zlib header", "0644", "facet=\"devel\"")?
            + &f.file(FR, "zlib french", "0644", "facet=\"locale.fr\"")?
            + &f.file(DEBUG, "zlib symbols", "0644", "facet=\"debug.zlib\"")?,
    )?;
    f.package(
        "library/sqlite",
        "1.0.0-0",
        dependency("require", "library/zlib", "1.0.0-0")
            + &f.file("usr/lib/libsqlite3.so.0", "sqlite", "0644", "")?
            + &f.file(SQLITE_H, "sqlite header", "0644", "facet=\"devel\"")?
            + &f.file(DE, "sqlite german", "0644", "facet=\"locale.de\"")?
            + &f.file(LIB32, "sqlite i686", "0644", "facet=\"isa.i686\"")?,
    )?;
    f.package(
        "tools/app",
        "1.0.0-0",
        dependency("require", "library/zlib", "1.0.0-0")
            + &f.file("usr/bin/app", "app", "0755", "")?
            + &f.file(APP_H, "app header", "0644", "facet=\"devel\"")?,
    )
}

#[test]
fn hierarchical_rules_defaults_and_package_overrides_recompose_offline() -> Result<()> {
    let f = Fixture::new()?;
    publish_libraries(&f)?;
    let mut policy = ImagePolicy::default();
    for (rule, value) in [("devel", false), ("locale.*", false), ("locale.fr", true)] {
        policy.insert_facet(rule, value, false)?;
    }
    create_image(&f.root, &f.repo, "canopa", &f.trust, "x86_64", policy)?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["library/sqlite".into(), "tools/app".into()],
        },
    )?;
    apply(&f.root, &install)?;
    let present = |path: &str| f.root.join(path).exists();
    assert!(present("usr/lib/libz.so.1") && present(FR));
    for path in [ZLIB_H, SQLITE_H, APP_H, DE, DEBUG, LIB32] {
        assert!(!present(path), "{path}");
    }
    let rows = crate::query::list_facets(&f.root)?;
    let listed = rows
        .iter()
        .map(|row| (row.rule.as_str(), row.value, row.source))
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            ("debug.*", false, "default"),
            ("isa.*", false, "default"),
            ("devel", false, "local"),
            ("locale.*", false, "local"),
            ("locale.fr", true, "local"),
        ]
    );
    assert_eq!(
        rule(&f.root, None, "locale.fr")?,
        (true, "locale.fr".into())
    );
    assert_eq!(
        rule(&f.root, None, "locale.de")?,
        (false, "locale.*".into())
    );
    assert_eq!(
        rule(&f.root, None, "debug.zlib")?,
        (false, "debug.* (default)".into())
    );
    assert_eq!(rule(&f.root, None, "doc.man")?, (true, "unset".into()));

    // Facet changes reuse the installed manifests and only recompose affected packages.
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    let headers = plan(
        &f.root,
        change_facets(&[("library/sqlite:devel", Some(true))], false),
    )?;
    assert!(headers.catalogs.is_empty());
    assert_eq!(recomposed(&headers), ["library/sqlite"]);
    assert!(headers.summary().contains(&format!("Add {SQLITE_H}")));
    // Newly enabled actions need their payloads from the repository or cache.
    fs::rename(f.repo.with_extension("offline"), &f.repo)?;
    apply(&f.root, &headers)?;
    assert!(present(SQLITE_H) && !present(ZLIB_H) && !present(APP_H));
    let sqlite = facet_values(&f.root, Some("library/sqlite"), &[])?;
    let sqlite = sqlite
        .iter()
        .map(|row| (row.facet.as_str(), row.value, row.source.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        sqlite,
        [
            ("devel", true, "library/sqlite:devel".to_owned()),
            ("isa.i686", false, "isa.* (default)".to_owned()),
            ("locale.de", false, "locale.*".to_owned()),
        ]
    );

    // The closure option also reaches installed dependencies, but not unrelated packages.
    let closure = plan(
        &f.root,
        change_facets(&[("library/sqlite:devel", Some(true))], true),
    )?;
    assert_eq!(recomposed(&closure), ["library/zlib"]);
    assert!(
        closure
            .summary()
            .contains("Set facet library/sqlite:devel=true with dependencies")
    );
    apply(&f.root, &closure)?;
    assert!(present(SQLITE_H) && present(ZLIB_H) && !present(APP_H));
    assert_eq!(
        rule(&f.root, Some("library/zlib"), "devel")?,
        (
            true,
            "library/sqlite:devel (dependency of library/sqlite)".into()
        )
    );
    assert_eq!(
        rule(&f.root, Some("tools/app"), "devel")?,
        (false, "devel".into())
    );
    // A package's own rule beats an inherited one, and a glob applies to its packages.
    apply(
        &f.root,
        &plan(
            &f.root,
            change_facets(
                &[
                    ("library/*:devel", Some(false)),
                    ("tools/*:devel", Some(true)),
                ],
                false,
            ),
        )?,
    )?;
    assert!(present(SQLITE_H) && !present(ZLIB_H) && present(APP_H));
    assert_eq!(
        rule(&f.root, Some("library/sqlite"), "devel")?,
        (true, "library/sqlite:devel".into())
    );
    assert_eq!(
        rule(&f.root, Some("library/zlib"), "devel")?,
        (false, "library/*:devel".into())
    );
    assert_eq!(
        local_facets(&f.root)?,
        BTreeMap::from([
            ("devel".into(), false),
            ("library/*:devel".into(), false),
            ("library/sqlite:devel".into(), true),
            ("locale.*".into(), false),
            ("locale.fr".into(), true),
            ("tools/*:devel".into(), true),
        ])
    );

    let reset = plan(
        &f.root,
        change_facets(
            &[
                ("library/sqlite:devel", None),
                ("library/*:devel", None),
                ("tools/*:devel", None),
                ("isa.*", Some(true)),
            ],
            false,
        ),
    )?;
    assert_eq!(recomposed(&reset), ["library/sqlite", "tools/app"]);
    apply(&f.root, &reset)?;
    assert!(!present(SQLITE_H) && !present(ZLIB_H) && !present(APP_H) && present(LIB32));
    assert!(!present(DEBUG));
    assert!(crate::verify_pristine(&f.root)?.is_empty());

    let error = plan(&f.root, change_facets(&[("devel", Some(true))], true)).unwrap_err();
    assert!(error.to_string().contains("--with-dependencies requires"));
    for invalid in ["lib*:devel", "library/sqlite:", "Devel", "locale.*.fr"] {
        assert!(
            plan(&f.root, change_facets(&[(invalid, Some(true))], false)).is_err(),
            "{invalid}"
        );
    }
    Ok(())
}

#[test]
fn conflicting_inherited_overrides_are_refused() -> Result<()> {
    let f = Fixture::new()?;
    publish_libraries(&f)?;
    f.create()?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec!["library/sqlite".into(), "tools/app".into()],
            },
        )?,
    )?;
    apply(
        &f.root,
        &plan(
            &f.root,
            change_facets(&[("library/sqlite:devel", Some(true))], true),
        )?,
    )?;
    let error = plan(
        &f.root,
        change_facets(&[("tools/app:devel", Some(false))], true),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("conflicting facet overrides for dependency library/zlib"),
        "{error:#}"
    );
    // An explicit rule for the shared dependency settles the conflict.
    apply(
        &f.root,
        &plan(
            &f.root,
            change_facets(
                &[
                    ("tools/app:devel", Some(false)),
                    ("library/zlib:devel", Some(true)),
                ],
                true,
            ),
        )?,
    )?;
    assert!(f.root.join(ZLIB_H).exists() && !f.root.join(APP_H).exists());
    Ok(())
}

#[test]
fn variants_select_exclusive_content_and_change_offline() -> Result<()> {
    const INIT: &str = "etc/init.conf";
    const GETTY: &str = "usr/bin/getty";
    let f = Fixture::new()?;
    let (uid, gid) = (geteuid().as_raw(), getegid().as_raw());
    let link = |target: &str, value: &str| {
        format!(
            "    symlink {INIT:?} target={target:?} uid={uid} gid={gid} variant.container={value:?}\n"
        )
    };
    f.package(
        "system/init",
        "1.0.0-0",
        f.file(
            "usr/lib/init/container.conf",
            "container",
            "0644",
            "variant.container=\"true\"",
        )? + &f.file(
            "usr/lib/init/metal.conf",
            "bare metal",
            "0644",
            "variant.container=#false",
        )? + &link("../usr/lib/init/container.conf", "true")
            + &link("../usr/lib/init/metal.conf", "false")
            + &f.file("usr/bin/init", "init", "0755", "arch=\"x86_64\"")?
            + &f.file(GETTY, "getty", "0755", "variant.container=#false")?,
    )?;
    f.create()?;
    let install = |root: &Path| {
        plan(
            root,
            Request::Install {
                packages: vec!["system/init".into()],
            },
        )
    };
    let error = install(&f.root).unwrap_err();
    assert!(
        format!("{error:#}").contains("by variant container, which this image does not set"),
        "{error:#}"
    );
    apply(
        &f.root,
        &plan(&f.root, change_variants(&[("container", Some("true"))]))?,
    )?;
    apply(&f.root, &install(&f.root)?)?;
    assert!(f.root.join("usr/bin/init").exists());
    assert_eq!(fs::read_to_string(f.root.join(INIT))?, "container");
    assert!(!f.root.join(GETTY).exists() && !f.root.join("usr/lib/init/metal.conf").exists());
    assert_eq!(
        list_variants(&f.root)?,
        BTreeMap::from([
            ("arch".to_owned(), "x86_64".to_owned()),
            ("container".to_owned(), "true".to_owned()),
        ])
    );

    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    let bare = plan(&f.root, change_variants(&[("container", Some("false"))]))?;
    assert!(bare.catalogs.is_empty());
    assert_eq!(recomposed(&bare), ["system/init"]);
    assert!(bare.summary().contains("Set variant container=false"));
    assert!(bare.summary().contains(&format!("Replace {INIT}")));
    fs::rename(f.repo.with_extension("offline"), &f.repo)?;
    apply(&f.root, &bare)?;
    assert_eq!(fs::read_to_string(f.root.join(INIT))?, "bare metal");
    assert!(f.root.join(GETTY).exists());
    assert!(!f.root.join("usr/lib/init/container.conf").exists());
    assert!(crate::verify_pristine(&f.root)?.is_empty());

    let error = plan(&f.root, change_variants(&[("arch", Some("aarch64"))])).unwrap_err();
    assert!(error.to_string().contains("arch variant is fixed"));
    let error = plan(&f.root, change_variants(&[("container", None)])).unwrap_err();
    assert!(format!("{error:#}").contains("does not set"));
    let error = plan(&f.root, change_variants(&[("container", Some("Yes"))])).unwrap_err();
    assert!(error.to_string().contains("invalid value"));
    Ok(())
}

#[test]
fn manifests_require_variant_exclusive_alternatives_and_plain_dependencies() -> Result<()> {
    let parse = |body: &str| {
        parse_manifest(&format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"test/selectors\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{body}\n}}\n"
        ))
        .and_then(|manifest| {
            manifest.validate(false)?;
            Ok(manifest)
        })
    };
    let file = |extra: &str| format!("file \"etc/tool.conf\" mode=\"0644\" uid=0 gid=0 {extra}\n");
    let exclusive = parse(
        &(file("variant.container=\"true\" facet=\"doc\"")
            + &file("variant.container=\"false\" arch=\"x86_64\"")
            + &file("variant.container=\"false\" arch=\"aarch64\" facet=\"doc\"")),
    )?;
    assert_eq!(parse_manifest(&manifest_text(&exclusive))?, exclusive);
    assert!(manifest_text(&exclusive).contains("variant.container=\"true\""));
    assert!(
        manifest_text(&exclusive).contains(" arch=")
            && !manifest_text(&exclusive).contains("variant.arch")
    );
    // Booleans spell the "true" and "false" values.
    assert_eq!(
        parse(&file("variant.container=#true"))?.action[0]
            .selectors()
            .variants["container"],
        "true"
    );
    for (body, message) in [
        (file("") + &file("facet=\"devel\""), "duplicate action"),
        (file("facet=\"doc\"") + &file("facet=\"devel\""), "duplicate action"),
        (
            file("variant.container=\"true\"") + &file("variant.container=\"true\" facet=\"x\""),
            "duplicate action",
        ),
        (
            file("variant.container=\"true\"") + &file("arch=\"x86_64\""),
            "duplicate action",
        ),
        (file("arch=\"x86_64\" variant.arch=\"x86_64\""), "same variant"),
        (file("variant.Container=\"true\""), "invalid variant"),
        (file("variant.container=\"\""), "invalid value"),
        (file("facet=\"locale.*\""), "invalid facet"),
        (file("variant.container=1"), "variant values"),
        (
            "require \"library/zlib\" facet=\"devel\"".to_owned(),
            "dependencies cannot carry facet",
        ),
        (
            "recommends \"library/zlib\" arch=\"x86_64\"".to_owned(),
            "dependencies cannot carry arch",
        ),
        (
            "require-any {\n package \"library/a\" variant.container=\"true\"\n package \"library/b\"\n}"
                .to_owned(),
            "dependencies cannot carry variant.container",
        ),
        (
            "optional \"library/zlib\" description=\"headers\" facet=\"devel\"".to_owned(),
            "dependencies cannot carry facet",
        ),
    ] {
        let error = parse(&body).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{body}: {error:#}");
    }
    let link = "hardlink \"etc/alias.conf\" target=\"etc/tool.conf\"";
    parse(
        &(file("variant.container=\"true\"") + link + " variant.container=\"true\" facet=\"doc\""),
    )?;
    assert!(parse(&(file("variant.container=\"true\"") + link)).is_err());
    assert!(
        parse(&(file("variant.container=\"true\"") + link + " variant.container=\"false\""))
            .is_err()
    );
    Ok(())
}

#[test]
fn image_configs_and_bundles_carry_per_package_facets_and_variants() -> Result<()> {
    let f = Fixture::new()?;
    publish_libraries(&f)?;
    f.package(
        "system/init",
        "1.0.0-0",
        f.file(
            "etc/init.conf",
            "container",
            "0644",
            "variant.container=\"true\"",
        )?,
    )?;
    let write_config = |name: &str, body: &str| -> Result<PathBuf> {
        let path = f._dir.path().join(name);
        fs::write(
            &path,
            format!(
                "schema-version 1 kind=\"image-config\"\narchitecture \"x86_64\"\npublisher \"canopa\" {{ repository {:?}; trust-root {:?}; }}\n{body}",
                f.repo, f.trust
            ),
        )?;
        Ok(path)
    };
    let image_rules = "facet \"devel\" #false\nvariant \"container\" #true\n";
    let config = write_config(
        "image.kdl",
        &format!(
            "{image_rules}facet \"devel\" #true package=\"library/sqlite\" with-dependencies=#true\n"
        ),
    )?;
    crate::create_configured_image(&f.root, &config)?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec![
                    "library/sqlite".into(),
                    "tools/app".into(),
                    "system/init".into(),
                ],
            },
        )?,
    )?;
    assert!(f.root.join(SQLITE_H).exists() && f.root.join(ZLIB_H).exists());
    assert!(!f.root.join(APP_H).exists());
    assert!(f.root.join("etc/init.conf").exists());

    let lock = crate::capture_image_lock(&f.root)?;
    assert_eq!(
        lock.package_facets["library/sqlite"]["devel"],
        crate::PackageFacet {
            value: true,
            with_dependencies: true
        }
    );
    assert_eq!(lock.variants["container"], "true");
    let bundle = f._dir.path().join("bundle");
    crate::export_image_bundle(&f.root, &bundle)?;
    // The replica's configuration may omit per-package rules: the bundle carries them.
    let plain = write_config("plain.kdl", image_rules)?;
    let replica = f._dir.path().join("replica");
    crate::create_image_from_bundle(&replica, &plain, &bundle)?;
    let lock_plan = plan(
        &replica,
        Request::InstallLock {
            lock: Box::new(crate::read_image_lock(&bundle.join("image.lock.json"))?),
        },
    )?;
    apply(&replica, &lock_plan)?;
    assert!(replica.join(SQLITE_H).exists() && replica.join(ZLIB_H).exists());
    assert!(!replica.join(APP_H).exists());
    assert_eq!(local_facets(&replica)?, local_facets(&f.root)?);

    for (name, body, message) in [
        (
            "other-variant.kdl",
            "facet \"devel\" #false\nvariant \"container\" #false\n",
            "variants or release profile differ from the image lock",
        ),
        (
            "other-override.kdl",
            &format!("{image_rules}facet \"devel\" #false package=\"library/sqlite\"\n"),
            "per-package facet rules differ from the image lock",
        ),
    ] {
        let config = write_config(name, body)?;
        let error = crate::create_image_from_bundle(&f._dir.path().join(name), &config, &bundle)
            .unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }
    for (body, message) in [
        ("facet \"library/sqlite:devel\" #true\n", "package=PACKAGE"),
        (
            "facet \"devel\" #true with-dependencies=#true\n",
            "requires a package",
        ),
        (
            "facet \"devel\" #true package=\"Library\"\n",
            "invalid package pattern",
        ),
        ("variant \"arch\" \"x86_64\"\n", "architecture node"),
        (
            "variant \"container\" #true\nvariant \"container\" #false\n",
            "duplicate variant",
        ),
        (
            "facet \"devel\" #true\nfacet \"devel\" #false\n",
            "duplicate facet rule",
        ),
    ] {
        let config = write_config("invalid.kdl", body)?;
        let error =
            crate::create_configured_image(&f._dir.path().join("invalid"), &config).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{body}: {error:#}");
    }
    Ok(())
}

#[test]
fn profile_images_override_facets_only_for_local_or_allowed_packages() -> Result<()> {
    let f = Fixture::new()?;
    publish_libraries(&f)?;
    let publish_profile = |allow: &str| -> Result<()> {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {{
        package "library/sqlite"
        facet "devel" #false
{allow}    }}
}}
"#
            ),
        )?;
        let release = f._dir.path().join("refresh.kdl");
        let base = fs::read_to_string(f.repo.join("current"))?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim()
            ),
        )?;
        crate::publish_release(&f.repo, &f.private, &release)?;
        Ok(())
    };
    publish_profile("")?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            "schema-version 1 kind=\"image-config\"\narchitecture \"x86_64\"\nrelease-profile \"desktop\"\nfacet \"devel\" #false\npublisher \"canopa\" {{ repository {:?}; trust-root {:?}; }}\n",
            f.repo, f.trust
        ),
    )?;
    crate::create_configured_image(&f.root, &config)?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec!["tools/app".into()],
            },
        )?,
    )?;

    // Packages installed locally accept per-package rules.
    apply(
        &f.root,
        &plan(
            &f.root,
            change_facets(&[("tools/app:devel", Some(true))], false),
        )?,
    )?;
    assert!(f.root.join(APP_H).exists());
    for (request, message) in [
        (
            change_facets(&[("library/sqlite:devel", Some(true))], false),
            "facet rule library/sqlite:devel reaches library/sqlite, which the signed release profile protects",
        ),
        (
            change_facets(&[("library/*:devel", Some(true))], false),
            "which the signed release profile protects",
        ),
        (
            // The closure of a local package can reach protected dependencies.
            change_facets(&[("tools/app:devel", Some(true))], true),
            "facet rule tools/app:devel reaches library/zlib",
        ),
        (
            change_facets(&[("devel", Some(true))], false),
            "fixes this image's image-wide facets",
        ),
        (
            change_variants(&[("container", Some("true"))]),
            "fixes this image's variants",
        ),
    ] {
        let error = plan(&f.root, request).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }

    // A profile that allows overriding a facet accepts rules for its packages.
    publish_profile("        allow-facet-override \"devel\"\n")?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    let headers = plan(
        &f.root,
        change_facets(&[("library/sqlite:devel", Some(true))], true),
    )?;
    assert_eq!(recomposed(&headers), ["library/sqlite", "library/zlib"]);
    apply(&f.root, &headers)?;
    assert!(f.root.join(SQLITE_H).exists() && f.root.join(ZLIB_H).exists());
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

const SHARED_DOC: &str = "usr/share/doc/shared";

/// Two libraries collide only where their documentation applies. The image
/// sets `doc` to `image_doc`, and `apps/r:doc` applies the opposite value with
/// dependencies, so whether `lib/a` and `lib/b` conflict depends on whether
/// `apps/r` is selected.
fn inherited_collision(top: &str, image_doc: bool) -> Result<Fixture> {
    let f = Fixture::new()?;
    for (name, text) in [("lib/a", "a docs"), ("lib/b", "b docs")] {
        f.package(
            name,
            "1.0.0-0",
            f.file(SHARED_DOC, text, "0644", "facet=\"doc\"")?,
        )?;
    }
    let libraries = "    require \"lib/a\"\n    require \"lib/b\"\n";
    f.package("apps/r", "1.0.0-0", libraries.into())?;
    f.package("apps/s", "1.0.0-0", String::new())?;
    f.package("apps/x", "1.0.0-0", String::new())?;
    f.package("apps/top", "1.0.0-0", format!("{libraries}{top}"))?;
    let mut policy = ImagePolicy::default();
    policy.insert_facet("doc", image_doc, false)?;
    policy.insert_facet("apps/r:doc", !image_doc, true)?;
    create_image(&f.root, &f.repo, "canopa", &f.trust, "x86_64", policy)?;
    Ok(f)
}

fn install_packages(f: &Fixture, packages: &[&str]) -> Result<Vec<String>> {
    let install = plan(
        &f.root,
        Request::Install {
            packages: packages.iter().map(|&name| name.into()).collect(),
        },
    )?;
    apply(&f.root, &install)?;
    assert!(!f.root.join(SHARED_DOC).exists());
    Ok(
        crate::query::list_packages(&f.root, crate::query::Scope::Installed)?
            .into_iter()
            .map(|row| row.name)
            .collect(),
    )
}

fn either(first: &str, second: &str) -> String {
    format!("    require-any {{\n        package {first:?}\n        package {second:?}\n    }}\n")
}

#[test]
fn inherited_path_conflicts_leave_other_alternatives_open() -> Result<()> {
    // apps/r enables the colliding documentation, so only apps/s fits.
    for (first, second) in [("apps/r", "apps/s"), ("apps/s", "apps/r")] {
        let f = inherited_collision(&either(first, second), false)?;
        assert_eq!(
            install_packages(&f, &["apps/top"])?,
            ["apps/s", "apps/top", "lib/a", "lib/b"],
            "{first} before {second}"
        );
    }
    // apps/r disables it, so only apps/r fits.
    for (first, second) in [("apps/r", "apps/s"), ("apps/s", "apps/r")] {
        let f = inherited_collision(&either(first, second), true)?;
        assert_eq!(
            install_packages(&f, &["apps/top"])?,
            ["apps/r", "apps/top", "lib/a", "lib/b"],
            "{first} before {second}"
        );
    }
    Ok(())
}

#[test]
fn inherited_path_conflicts_reject_only_the_recommendation_that_causes_them() -> Result<()> {
    let f = inherited_collision(
        "    recommends \"apps/r\"\n    recommends \"apps/x\"\n",
        false,
    )?;
    assert_eq!(
        install_packages(&f, &["apps/top"])?,
        ["apps/top", "apps/x", "lib/a", "lib/b"]
    );
    Ok(())
}

#[test]
fn unavoidable_inherited_path_conflicts_are_explained() -> Result<()> {
    let expect_conflict = |f: &Fixture, packages: &[&str]| {
        let error = plan(
            &f.root,
            Request::Install {
                packages: packages.iter().map(|&name| name.into()).collect(),
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        for detail in [
            "cannot satisfy package constraints",
            SHARED_DOC,
            "lib/a",
            "lib/b",
        ] {
            assert!(message.contains(detail), "missing {detail} in {message}");
        }
    };
    let f = inherited_collision("    require \"apps/r\"\n", false)?;
    expect_conflict(&f, &["apps/top"]);
    // A package whose rule would hide the conflict is never added unrequested.
    let f = inherited_collision("", true)?;
    expect_conflict(&f, &["lib/a", "lib/b"]);
    Ok(())
}

#[test]
fn solver_picks_the_intermediate_version_that_keeps_inherited_facets_apart() -> Result<()> {
    // apps/r enables documentation for its closure. lib/m 2 would put lib/a in
    // that closure next to lib/b, so the solver keeps lib/m 1 instead.
    let f = Fixture::new()?;
    for (name, text) in [("lib/a", "a docs"), ("lib/b", "b docs")] {
        f.package(
            name,
            "1.0.0-0",
            f.file(SHARED_DOC, text, "0644", "facet=\"doc\"")?,
        )?;
    }
    f.package("lib/m", "1.0.0-0", String::new())?;
    f.package("lib/m", "2.0.0-0", "    require \"lib/a\"\n".into())?;
    f.package(
        "apps/r",
        "1.0.0-0",
        "    require \"lib/m\"\n    require \"lib/b\"\n".into(),
    )?;
    f.package(
        "apps/top",
        "1.0.0-0",
        "    require \"lib/a\"\n    require \"apps/r\"\n".into(),
    )?;
    let mut policy = ImagePolicy::default();
    policy.insert_facet("doc", false, false)?;
    policy.insert_facet("apps/r:doc", true, true)?;
    create_image(&f.root, &f.repo, "canopa", &f.trust, "x86_64", policy)?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/top".into()],
        },
    )?;
    assert!(
        install.summary().contains("Install lib/m 1.0.0-0"),
        "{}",
        install.summary()
    );
    apply(&f.root, &install)?;
    assert_eq!(fs::read_to_string(f.root.join(SHARED_DOC))?, "b docs");
    assert_eq!(rule(&f.root, Some("lib/a"), "doc")?, (false, "doc".into()));
    assert_eq!(
        rule(&f.root, Some("lib/b"), "doc")?,
        (true, "apps/r:doc (dependency of apps/r)".into())
    );
    Ok(())
}
