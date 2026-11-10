use std::collections::BTreeSet;

use std::path::Path;

use jiff::Timestamp;
use tough::{editor::signed::PathExists, error::Error as TufError, schema::RoleType};
use url::Url;

#[path = "../../tests/support/tuf.rs"]
mod tuf;
use tuf::sign_root;

use super::*;
use crate::{
    attestation::{self, PinnedPackage, Toolchain},
    create_configured_image, fetch_plan, fetch_plan_with_archives, fetch_toolchain,
    kdl_format::{build, parse_manifest},
    list_obsoletions, list_withdrawals,
    model::FileDigest,
    publish_bundle, publish_release,
    query::{Scope, Status, list_packages, package_manifest},
    read_attestation, release as mirror_release,
    repo::{read_key, read_signing_key, snapshot},
    rotate_publisher_key, withdraw_package,
};

fn attach_evidence(
    fixture: &Fixture,
    catalog: &str,
    builder: &Fixture,
) -> Result<(String, Vec<u8>)> {
    let bundle = fixture._dir.path().join("bundle");
    fs::create_dir_all(bundle.join("proto/usr/bin"))?;
    let manifest_bytes = fs::read(&fixture.recipe)?;
    let manifest = parse_manifest(std::str::from_utf8(&manifest_bytes)?)?;
    let key = read_key(&fixture.public)?;
    let key_hash = digest(&key);
    let publisher = &manifest.publisher;
    let recipe = format!(
        "schema-version 1 kind=\"build-recipe\"\nbuild architecture=\"x86_64\" epoch=1700000000 {{\n\
         toolchain solver=\"sat-v2\" {{\n publisher \"{publisher}\" catalog=\"{catalog}\" key-sha256=\"{key_hash}\"\n package \"desktop/compositor\"\n }}\n\
         manifest \"manifest.kdl\" sha256=\"{}\"\n script \"true\"\n}}\n",
        digest(&manifest_bytes),
    );
    let release = snapshot(&fixture.repo, &manifest.publisher, &key, Some(catalog))?;
    let payload = fs::read(fixture.proto.join(COMPOSITOR))?;
    let recipe_model = build::parse(&recipe)?;
    let envelope = attestation::sign(
        &read_signing_key(&builder.private)?,
        std::slice::from_ref(&manifest),
        &recipe_model,
        recipe.as_bytes(),
        Toolchain {
            publishers: recipe_model.toolchain.clone(),
            package_manifests: BTreeMap::from([(
                "desktop/compositor".to_owned(),
                PinnedPackage {
                    publisher: publisher.clone(),
                    version: release.packages[0].manifest.version.clone(),
                    sha256: release.packages[0].sha256.clone(),
                },
            )]),
        },
        &BTreeMap::from([(
            COMPOSITOR.to_owned(),
            FileDigest {
                sha256: digest(&payload),
                size: payload.len() as u64,
            },
        )]),
    )?;
    fs::write(bundle.join("build.kdl"), &recipe)?;
    crate::build::write_explicit_outputs(&bundle, &[("manifest.kdl", &manifest_bytes)])?;
    fs::write(bundle.join("attestation.json"), &envelope)?;
    fs::write(bundle.join("proto").join(COMPOSITOR), payload)?;
    let [id] = &publish_bundle(
        &fixture.repo,
        &bundle,
        &fixture.private,
        &builder.public,
        &digest(recipe.as_bytes()),
    )?[..] else {
        panic!("expected one output");
    };
    Ok((id.clone(), envelope))
}

#[test]
fn publisher_independently_checks_the_signed_toolchain_closure() -> Result<()> {
    use crate::kdl_format::build::PublisherPin;

    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let tools = Fixture::new()?;
    let builder = Fixture::new()?;
    let library = "build/library";
    let compiler = "build/compiler";
    seed.package_as("seed", library, "1.0.0-0", String::new())?;
    seed.package_as("seed", library, "2.0.0-0", String::new())?;
    tools.package_as(
        "tools",
        compiler,
        "1.0.0-0",
        format!("    require {library:?}\n"),
    )?;
    let sources = [("seed", &seed), ("tools", &tools)]
        .into_iter()
        .map(|(name, source)| -> Result<_> {
            let key = read_key(&source.public)?;
            let release = snapshot(&source.repo, name, &key, None)?;
            Ok((
                PublisherPin {
                    name: name.to_owned(),
                    catalog_sha256: release.sha256,
                    key_sha256: digest(&key),
                },
                release.packages,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let package = |name: &str, version: &str| {
        let published = sources
            .iter()
            .flat_map(|(_, packages)| packages)
            .find(|pkg| pkg.manifest.name == name && pkg.manifest.version.to_string() == version)
            .unwrap();
        PinnedPackage {
            publisher: published.manifest.publisher.clone(),
            version: published.manifest.version.clone(),
            sha256: published.sha256.clone(),
        }
    };
    let config = f._dir.path().join("toolchain.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="toolchain-config"
publisher "seed" {{ repository {:?}; trust-key {:?}; }}
publisher "tools" {{ repository {:?}; trust-key {:?}; }}
"#,
            seed.repo, seed.public, tools.repo, tools.public
        ),
    )?;
    let bundle = f._dir.path().join("bundle");
    fs::create_dir_all(bundle.join("proto"))?;
    let manifest =
        b"schema-version 1 kind=\"package-manifest\"\npackage \"test/compiled\" publisher=\"canopa\" version=\"1.0.0-0\"\n";
    crate::build::write_explicit_outputs(&bundle, &[("manifest.kdl", manifest)])?;
    let mut recipe =
        "schema-version 1 kind=\"build-recipe\"\nbuild architecture=\"x86_64\" epoch=1700000000 {\n    toolchain solver=\"sat-v2\" {\n"
            .to_owned();
    for (pin, _) in &sources {
        recipe.push_str(&format!(
            "        publisher {:?} catalog={:?} key-sha256={:?}\n",
            pin.name, pin.catalog_sha256, pin.key_sha256
        ));
    }
    recipe.push_str(&format!(
        "        package \"cps://tools/{compiler}@1.0.0-0\"\n    }}\n    manifest \"manifest.kdl\" sha256={:?}\n    script \"true\"\n}}\n",
        digest(manifest),
    ));
    let reviewed = digest(recipe.as_bytes());
    fs::write(bundle.join("build.kdl"), &recipe)?;
    let sign = |library_version: Option<&str>| -> Result<()> {
        let mut package_manifests =
            BTreeMap::from([(compiler.to_owned(), package(compiler, "1.0.0-0"))]);
        if let Some(version) = library_version {
            package_manifests.insert(library.to_owned(), package(library, version));
        }
        let envelope = attestation::sign(
            &read_signing_key(&builder.private)?,
            &[parse_manifest(std::str::from_utf8(manifest)?)?],
            &build::parse(&recipe)?,
            recipe.as_bytes(),
            Toolchain {
                publishers: sources.iter().map(|(pin, _)| pin.clone()).collect(),
                package_manifests,
            },
            &BTreeMap::new(),
        )?;
        fs::write(bundle.join("attestation.json"), envelope)?;
        Ok(())
    };
    // A higher version on the current pointer cannot change a reviewed pin.
    seed.package_as("seed", library, "3.0.0-0", String::new())?;
    fs::create_dir(&f.repo)?;
    let policy = f.repo.join("publish-policy.kdl");
    let write_policy = |strict: bool| {
        let setting = if strict { "#true" } else { "#false" };
        fs::write(
            &policy,
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" verify-toolchains={setting} {{
    builder-key {:?}
    approve "test/compiled" version="1.0.0-0" sha256="{reviewed}" toolchain-config="../toolchain.kdl"
}}
"#,
                builder.public
            ),
        )
    };
    write_policy(true)?;
    let publish = || publish_bundle(&f.repo, &bundle, &f.private, &builder.public, &reviewed);
    let release = f._dir.path().join("release.kdl");
    fs::copy(&builder.public, f._dir.path().join("builder.der"))?;
    fs::write(
        &release,
        format!(
            r#"schema-version 1 kind="publish-release"
release "canopa" base="empty" {{
    bundle "cps://canopa/test/compiled@1.0.0-0" path="bundle" builder-key="builder.der" recipe-sha256="{reviewed}"
}}
"#
        ),
    )?;
    for version in [None, Some("1.0.0-0")] {
        sign(version)?;
        for error in [
            publish().unwrap_err(),
            publish_release(&f.repo, &f.private, &release).unwrap_err(),
        ] {
            assert!(
                format!("{error:#}").contains(
                    "attested toolchain differs from the independently resolved toolchain"
                )
            );
        }
        assert!(!f.repo.join("current").exists());
    }
    sign(Some("2.0.0-0"))?;
    let current = publish_release(&f.repo, &f.private, &release)?;
    publish()?;
    write_policy(false)?;
    let error = publish().unwrap_err();
    assert!(format!("{error:#}").contains("protected repository requires toolchain verification"));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?.trim(), current);
    Ok(())
}

#[test]
fn release_batch_checks_its_base_and_keeps_withdrawals_atomic() -> Result<()> {
    let f = Fixture::new()?;
    f.package("test/one", "1.0.0-0", String::new())?;
    f.package("test/two", "1.0.0-0", String::new())?;
    let pointer = f.repo.join("current");
    let base = fs::read_to_string(&pointer)?;
    let release = f._dir.path().join("release.kdl");
    let write = |second: &str| {
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} {{\n    withdraw \"cps://canopa/test/one@1.0.0-0\"\n    withdraw {second:?}\n}}\n",
                base.trim(),
            ),
        )
    };
    write("cps://canopa/test/missing@1.0.0-0")?;
    let error = publish_release(&f.repo, &f.private, &release).unwrap_err();
    assert!(error.to_string().contains("no published package"));
    assert_eq!(fs::read_to_string(&pointer)?, base);
    assert!(list_withdrawals(&f.repo, "canopa", &f.public)?.is_empty());

    write("cps://canopa/test/one@1.0.0-0")?;
    let error = publish_release(&f.repo, &f.private, &release).unwrap_err();
    assert!(format!("{error:#}").contains("duplicate release action"));

    write("cps://canopa/test/two@1.0.0-0")?;
    let publish =
        |mode| crate::publish_release_with_mode(&f.repo, &f.private, &release, None, mode);
    let prepared = publish(crate::ReleaseMode::Prepare)?;
    assert_eq!(fs::read_to_string(&pointer)?, base);
    let key = read_key(&f.public)?;
    assert!(
        snapshot(&f.repo, "canopa", &key, Some(&prepared))?
            .packages
            .is_empty()
    );
    let wrong = "0".repeat(64);
    assert!(publish(crate::ReleaseMode::RequireCandidate(&wrong)).is_err());
    assert_eq!(fs::read_to_string(&pointer)?, base);
    let committed = publish(crate::ReleaseMode::RequireCandidate(&prepared))?;
    assert_eq!(committed, prepared);
    assert_eq!(fs::read_to_string(&pointer)?.trim(), committed);
    assert_eq!(list_withdrawals(&f.repo, "canopa", &f.public)?.len(), 2);
    assert!(snapshot(&f.repo, "canopa", &key, None)?.packages.is_empty());
    assert_eq!(
        snapshot(&f.repo, "canopa", &key, Some(base.trim()))?
            .packages
            .len(),
        2
    );

    let error = publish_release(&f.repo, &f.private, &release).unwrap_err();
    assert!(error.to_string().contains("release base is stale"));
    assert_eq!(fs::read_to_string(&pointer)?.trim(), committed);
    Ok(())
}

#[test]
fn signed_profiles_select_packages_for_each_target_architecture() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let payload = "usr/lib/runtime";
    let publish_runtime =
        |fixture: &Fixture, publisher: &str, arch: &str, version: &str| -> Result<()> {
            fixture.file(payload, arch, "0644", "")?;
            let manifest = crate::generate_manifest_for_architecture(
                &fixture.proto,
                publisher,
                "library/runtime",
                version,
                geteuid().as_raw(),
                getegid().as_raw(),
                Some(arch),
            )?;
            fs::write(&fixture.recipe, manifest)?;
            publish(
                &fixture.repo,
                &fixture.proto,
                &fixture.recipe,
                &fixture.private,
            )?;
            Ok(())
        };
    publish_runtime(&f, "canopa", "x86_64", "2.0.0-0")?;
    publish_runtime(&seed, "seed", "aarch64", "1.0.0-0")?;
    f.package(
        "desktop/session",
        "1.0.0-0",
        "    require \"library/runtime\"\n".to_owned(),
    )?;
    let base = fs::read_to_string(f.repo.join("current"))?;
    let seed_pin = fs::read_to_string(seed.repo.join("current"))?;
    let profile = |arch, root| {
        format!(
            r#"    profile "{arch}" architecture="{arch}" {{
        package "{root}"
        publisher "seed" catalog={:?} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
"#,
            seed_pin.trim(),
            seed.repo,
            seed.public
        )
    };
    let publish_profiles = |arm_root| -> Result<String> {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                "schema-version 1 kind=\"publish-policy\"\npublisher \"canopa\" {{\n    builder-key \"unused.der\"\n{}{}\n}}\n",
                profile("x86_64", "desktop/session"),
                profile("aarch64", arm_root),
            ),
        )?;
        let release = f._dir.path().join("release.kdl");
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim(),
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)
    };
    let error = publish_profiles("library/runtime").unwrap_err();
    assert!(format!("{error:#}").contains("profile package library/runtime is unavailable"));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, base);
    let release = publish_profiles("desktop/session")?;

    let expires = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let mirror = |fixture: &Fixture, publisher| -> Result<(PathBuf, Url, Url)> {
        let root = fixture._dir.path().join("root.json");
        let key = fixture._dir.path().join("tuf.der");
        let mirror = fixture._dir.path().join("mirror");
        sign_root(&root, &key, None)?;
        sign_publisher_release(fixture, &mirror, 1, expires, &root, &key, publisher)?;
        Ok((
            root,
            Url::from_directory_path(mirror.join("metadata")).unwrap(),
            Url::from_directory_path(mirror.join("targets")).unwrap(),
        ))
    };
    let (root, metadata, targets) = mirror(&f, "canopa")?;
    let (seed_root, seed_metadata, seed_targets) = mirror(&seed, "seed")?;
    for (arch, expected, incompatible) in [
        (
            "x86_64",
            "cps://canopa/library/runtime@2.0.0-0",
            "cps://seed/library/runtime",
        ),
        (
            "aarch64",
            "cps://seed/library/runtime@1.0.0-0",
            "cps://canopa/library/runtime",
        ),
    ] {
        let work = f._dir.path().join(arch);
        fs::create_dir(&work)?;
        let config = work.join("image.kdl");
        fs::write(
            &config,
            format!(
                r#"schema-version 1 kind="image-config"
architecture "{arch}"
release-profile "{arch}"
publisher "canopa" {{ repository "canopa"; trust-key {:?}; }}
publisher "seed" {{ repository "seed"; trust-key {:?}; }}
mirror "canopa" root={root:?} metadata="{metadata}" targets="{targets}"
mirror "seed" root={seed_root:?} metadata="{seed_metadata}" targets="{seed_targets}"
"#,
                f.public, seed.public,
            ),
        )?;
        assert_eq!(mirror_release::sync_profile(&config, true)?, release);
        let image = work.join("image");
        create_configured_image(&image, &config)?;
        let available = list_packages(&image, Scope::AllAvailableVersions)?;
        let matches = available
            .iter()
            .filter(|row| row.name == "library/runtime")
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(matches, [expected]);
        assert!(package_manifest(&image, incompatible, true).is_err());
        let manifest = parse_manifest(&package_manifest(&image, expected, true)?)?;
        assert_eq!(manifest.architecture.as_deref(), Some(arch));
        assert!(
            plan(
                &image,
                Request::Install {
                    packages: vec![incompatible.to_owned()]
                }
            )
            .is_err()
        );
        let install = plan(&image, Request::InstallProfile)?;
        fetch_plan(&image, &install)?;
        apply(&image, &install)?;
        assert_eq!(fs::read_to_string(image.join(payload))?, arch);
        assert!(verify(&image)?.iter().all(|finding| !finding.is_error));
    }
    Ok(())
}

#[test]
fn bundle_cannot_claim_a_different_architecture_than_its_build() -> Result<()> {
    let f = Fixture::new()?;
    let action = f.file(COMPOSITOR, "different target", "0755", "")?;
    fs::write(
        &f.recipe,
        format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"desktop/compositor\" publisher=\"canopa\" version=\"1.0.0-0\" architecture=\"aarch64\" {{\n{action}}}\n"
        ),
    )?;
    publish(&f.repo, &f.proto, &f.recipe, &f.private)?;
    let catalog = fs::read_to_string(f.repo.join("current"))?;
    let builder = Fixture::new()?;
    let error = attach_evidence(&f, catalog.trim(), &builder).unwrap_err();
    assert!(format!("{error:#}").contains("targets architecture aarch64, expected x86_64"));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, catalog);
    let release = snapshot(
        &f.repo,
        "canopa",
        &read_key(&f.public)?,
        Some(catalog.trim()),
    )?;
    let package = &release.packages[0];
    let published = FileDigest {
        sha256: package.sha256.clone(),
        size: fs::metadata(f.repo.join("manifests").join(&package.sha256))?.len(),
    };
    let trust = attestation::BuildTrust::load(1, std::slice::from_ref(&builder.public))?;
    let envelope = fs::read(f._dir.path().join("bundle/attestation.json"))?;
    let error = trust
        .verify(&envelope, &package.manifest, published)
        .unwrap_err();
    assert!(format!("{error:#}").contains("targets architecture aarch64, expected x86_64"));
    Ok(())
}

#[test]
fn release_profiles_pin_latest_roots_and_use_image_facets() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(MANPAGE, "base documentation", "0644", "facet=\"doc.man\"")?,
    )?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        dependency("require", "library/unavailable", "1.0.0-0"),
    )?;
    f.package(
        "desktop/manual",
        "1.0.0-0",
        f.file(MANPAGE, "shared documentation", "0644", "")?,
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    let old = fs::read_to_string(f.repo.join("current"))?;
    let id = "cps://canopa/test/retire@1.0.0-0";
    let write_policy = |root: &str, docs: bool| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                "schema-version 1 kind=\"publish-policy\"\npublisher \"canopa\" {{\n    builder-key \"builder.der\"\n    profile \"desktop\" architecture=\"x86_64\" {{\n        package {root:?}\n        package \"desktop/manual\"\n        facet \"doc.man\" #{docs}\n    }}\n}}\n",
            ),
        )
    };

    write_policy("desktop/compositor", false)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    assert!(format!("{error:#}").contains("library/unavailable"));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("desktop/compositor@1.0.0-0", true)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    assert!(format!("{error:#}").contains(MANPAGE));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("desktop/compositor@1.0.0-0", false)?;
    withdraw_package(&f.repo, &f.private, id)?;
    assert!(list_withdrawals(&f.repo, "canopa", &f.public)?.contains_key(id));
    Ok(())
}

#[test]
fn signed_composition_replays_only_its_manifest_digests() -> Result<()> {
    let f = Fixture::new()?;
    for version in ["1.0.0-0", "2.0.0-0"] {
        f.package(
            "library/runtime",
            version,
            "    provides \"cap/desktop-runtime\"\n".into(),
        )?;
    }
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        "    require-capability \"cap/desktop-runtime\"\n".into(),
    )?;
    let release = snapshot(&f.repo, "canopa", &read_key(&f.public)?, None)?;
    let version = "1.0.0-0".parse()?;
    let composition = release
        .packages
        .iter()
        .filter(|package| package.manifest.version == version)
        .map(|package| (package.manifest.name.clone(), package.sha256.clone()))
        .collect();
    let profile = crate::repo::ReleaseProfile {
        architecture: "x86_64".to_owned(),
        facets: BTreeMap::new(),
        variants: BTreeMap::new(),
        facet_overrides: BTreeSet::new(),
        companions: Vec::new(),
        roots: BTreeMap::from([("desktop/compositor".to_owned(), version)]),
        composition,
        elf_library_dirs: Vec::new(),
        elf_symbol_versions: false,
        elf_require_declarations: false,
        rollout_percentage: None,
        advisories: Vec::new(),
    };
    let mut candidates = crate::plan::CandidateSet {
        packages: release.packages,
        obsoletions: BTreeMap::new(),
    };
    let installed = candidates.replay(&profile)?;
    let locked = &profile.composition["library/runtime"];
    assert_eq!(&installed["library/runtime"].published.sha256, locked);
    let mut incomplete = profile.clone();
    incomplete.composition.remove("library/runtime");
    let error = candidates.replay(&incomplete).unwrap_err();
    assert!(format!("{error:#}").contains("missing a provider for capability cap/desktop-runtime"));
    candidates
        .packages
        .retain(|package| &package.sha256 != locked);
    let error = candidates.replay(&profile).unwrap_err();
    assert!(
        format!("{error:#}").contains("signed profile package library/runtime"),
        "{error:#}"
    );
    f.create()?;
    let error = plan(&f.root, Request::InstallProfile).unwrap_err();
    assert!(format!("{error:#}").contains("requires a configured signed release profile"));
    Ok(())
}

#[test]
fn phased_profile_updates_defer_widen_and_allow_a_saved_override() -> Result<()> {
    use crate::{PhasedUpdateDeferred, PlanOptions, plan_with_options, stage_boot_environment};

    let f = Fixture::new()?;
    let companion = Fixture::new()?;
    let mut pins = Vec::new();
    for version in ["1.0.0-0", "2.0.0-0"] {
        companion.package_as("seed", "library/runtime", version, String::new())?;
        pins.push(fs::read_to_string(companion.repo.join("current"))?);
        f.package(
            "desktop/base",
            version,
            dependency("require", "library/runtime", version),
        )?;
    }
    for name in ["test/one", "test/two"] {
        f.package(name, "1.0.0-0", String::new())?;
    }
    let release = f._dir.path().join("refresh.kdl");
    let publish_profile = |version: &str, pin: &str, percentage: u8| -> Result<String> {
        let advisory = if version == "2.0.0-0" && percentage == 0 {
            r#"advisory "CPS-TEST-1" package="library/runtime" affected="<2.0.0-0" summary="Runtime security fix" action="Restart clients""#
        } else {
            ""
        };
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" rollout-percentage={percentage} {{
        package "desktop/base@{version}"
        {advisory}
        publisher "seed" catalog={:?} {{ repository {:?}; trust-key {:?}; }}
    }}
}}
"#,
                pin.trim(),
                companion.repo,
                companion.public,
            ),
        )?;
        let base = fs::read_to_string(f.repo.join("current"))?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim()
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)
    };
    let baseline = publish_profile("1.0.0-0", &pins[0], 100)?;
    assert!(publish_profile("2.0.0-0", &pins[1], 101).is_err());
    assert_eq!(fs::read_to_string(f.repo.join("current"))?.trim(), baseline);
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-key {:?}; }}
publisher "seed" {{ repository {:?}; trust-key {:?}; }}
"#,
            f.repo, f.public, companion.repo, companion.public,
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;

    let paused = publish_profile("2.0.0-0", &pins[1], 0)?;
    let deferred = |root: &Path| {
        plan(root, Request::Update)
            .unwrap_err()
            .downcast::<PhasedUpdateDeferred>()
    };
    let initial = deferred(&f.root)?;
    assert_eq!(initial.catalog, paused);
    assert_eq!(initial.percentage, 0);
    let mut advisories = Vec::new();
    let mut held_back = Vec::new();
    let error = crate::plan::inspect_update(
        &f.root,
        crate::plan::UpdateDiagnostics {
            held_back: &mut held_back,
            explain: None,
            advisories: Some(&mut advisories),
        },
        &Default::default(),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<PhasedUpdateDeferred>().is_some());
    assert_eq!(advisories.len(), 1);
    assert_eq!(advisories[0].publisher, "seed");
    assert_eq!(advisories[0].fixed.to_string(), "2.0.0-0");
    let boot_one = f._dir.path().join("boot-one");
    let boot_two = f._dir.path().join("boot-two");
    for (source, target, package) in [
        (&f.root, &boot_one, "test/one"),
        (&boot_one, &boot_two, "test/two"),
    ] {
        let local = plan(
            source,
            Request::Install {
                packages: vec![package.into()],
            },
        )?;
        stage_boot_environment(source, target, &local)?;
    }
    assert_eq!(deferred(&boot_two)?.cohort, initial.cohort);

    let fresh = f._dir.path().join("fresh");
    create_configured_image(&fresh, &config)?;
    apply(&fresh, &plan(&fresh, Request::InstallProfile)?)?;
    assert!(!plan(&fresh, Request::Update)?.has_changes());
    crate::plan::inspect_update(
        &fresh,
        crate::plan::UpdateDiagnostics {
            held_back: &mut held_back,
            explain: None,
            advisories: Some(&mut advisories),
        },
        &Default::default(),
    )?;
    assert!(advisories.is_empty());

    let options = PlanOptions {
        force_phased_update: true,
        ..PlanOptions::default()
    };
    let packages = vec!["desktop/base".into()];
    let freeze = plan(
        &f.root,
        Request::Freeze {
            packages: packages.clone(),
            reason: None,
        },
    )?;
    apply(&f.root, &freeze)?;
    let error = plan_with_options(&f.root, Request::Update, options).unwrap_err();
    assert!(format!("{error:#}").contains("frozen at 1.0.0-0"));
    let unfreeze = plan(&f.root, Request::Unfreeze { packages })?;
    apply(&f.root, &unfreeze)?;

    let forced = plan_with_options(&f.root, Request::Update, options)?;
    let saved = serde_json::to_value(&forced)?;
    let mut without_override = saved.clone();
    without_override
        .as_object_mut()
        .unwrap()
        .remove("force_phased_update");
    assert!(
        apply(&f.root, &serde_json::from_value(without_override)?)
            .unwrap_err()
            .downcast_ref::<PhasedUpdateDeferred>()
            .is_some()
    );
    apply(&f.root, &serde_json::from_value(saved)?)?;
    let history = crate::transaction_history(&f.root, 1)?;
    assert!(history[0].details.force_phased_update);
    assert!(!plan(&f.root, Request::Update)?.has_changes());

    if initial.cohort > 0 {
        publish_profile("2.0.0-0", &pins[1], initial.cohort)?;
        assert_eq!(deferred(&boot_two)?.cohort, initial.cohort);
    }
    let widened = publish_profile("2.0.0-0", &pins[1], initial.cohort + 1)?;
    let (_, profile) = crate::repo::release_profile(
        &f.repo,
        "canopa",
        &read_key(&f.public)?,
        Some(&widened),
        "desktop",
    )?;
    assert_eq!(profile.advisories.len(), 1);
    assert_eq!(profile.advisories[0].id, "CPS-TEST-1");
    apply(&boot_two, &plan(&boot_two, Request::Update)?)?;
    for root in [&f.root, &boot_two, &fresh] {
        let installed = list_packages(root, Scope::Installed)?;
        for name in ["desktop/base", "library/runtime"] {
            assert!(
                installed
                    .iter()
                    .any(|row| row.name == name && row.id.ends_with("@2.0.0-0"))
            );
        }
        assert!(verify(root)?.is_empty());
    }
    Ok(())
}

#[test]
fn signed_profiles_include_recommendations_in_the_published_composition() -> Result<()> {
    let f = Fixture::new()?;
    f.package("desktop/help", "1.0.0-0", String::new())?;
    f.package(
        "desktop/base",
        "1.0.0-0",
        "    recommends \"desktop/help\"\n".into(),
    )?;
    fs::write(
        f.repo.join("publish-policy.kdl"),
        r#"schema-version 1 kind="publish-policy"
publisher "canopa" {
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {
        package "desktop/base"
    }
}
"#,
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
    publish_release(&f.repo, &f.private, &release)?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-key {:?}; }}
"#,
            f.repo, f.public
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    let install = crate::plan_with_recommends(&f.root, Request::InstallProfile, false)?;
    apply(&f.root, &install)?;
    let help = crate::query::explain_package(&f.root, "desktop/help")?;
    assert_eq!(help.recommenders, ["cps://canopa/desktop/base@1.0.0-0"]);
    assert!(help.profile_name.is_some());
    Ok(())
}

#[test]
fn local_transactions_preserve_the_applied_signed_composition() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    seed.package_as("seed", "library/z", "1.0.0-0", String::new())?;
    let seed_pointer = seed.repo.join("current");
    let previous = fs::read_to_string(&seed_pointer)?;
    seed.package_as("seed", "library/a", "1.0.0-0", String::new())?;
    let next = fs::read_to_string(seed_pointer)?;
    f.package(
        "desktop/base",
        "1.0.0-0",
        r#"    require-any {
        package "library/a"
        package "library/z"
    }
"#
        .into(),
    )?;
    f.package(
        "desktop/tool",
        "1.0.0-0",
        f.file(COMPOSITOR, "previous", "0755", "")?,
    )?;
    f.package(
        "desktop/tool",
        "2.0.0-0",
        f.file(COMPOSITOR, "next", "0755", "")?,
    )?;
    f.package(
        "desktop/addon",
        "1.0.0-0",
        "    require \"library/a\"\n".into(),
    )?;
    let publish_profile = |version: &str, pin: &str| -> Result<()> {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/base"
        package "desktop/tool@{version}"
        publisher "seed" catalog={:?} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
                pin.trim(),
                seed.repo,
                seed.public
            ),
        )?;
        let release = f._dir.path().join("refresh.kdl");
        let base = fs::read_to_string(f.repo.join("current"))?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim(),
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)?;
        Ok(())
    };
    publish_profile("1.0.0-0", &previous)?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-key {:?}; }}
publisher "seed" {{ repository {:?}; trust-key {:?}; }}
"#,
            f.repo, f.public, seed.repo, seed.public,
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    let install = plan(&f.root, Request::InstallProfile)?;
    let offline = f.repo.with_extension("offline");
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("install-profile.json"),
        &install,
        COMPOSITOR,
    )?;
    fs::rename(&f.repo, &offline)?;
    apply(&f.root, &install)?;
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    fs::rename(&offline, &f.repo)?;

    // The available signed profile can move ahead before the image applies it.
    publish_profile("2.0.0-0", &next)?;
    let error = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/tool@2.0.0-0".into()],
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("protected by the installed signed profile"));
    let exact = plan(
        &f.root,
        Request::ExactInstall {
            packages: vec!["desktop/addon".into()],
        },
    )?;
    apply(&f.root, &exact)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "previous");
    let installed = list_packages(&f.root, Scope::Installed)?;
    assert_eq!(installed.len(), 5);
    assert!(
        installed
            .iter()
            .any(|row| row.name == "library/z" && row.status == Status::Dependency)
    );

    fs::rename(&f.repo, &offline)?;
    let remove = |name: &str| Request::Remove {
        packages: vec![name.into()],
    };
    for name in ["desktop/tool", "library/z"] {
        let error = plan(&f.root, remove(name)).unwrap_err();
        assert!(format!("{error:#}").contains("protected by the installed signed profile"));
    }
    apply(&f.root, &plan(&f.root, remove("desktop/addon"))?)?;
    apply(&f.root, &plan(&f.root, Request::Autoremove)?)?;
    assert!(package_manifest(&f.root, "library/a", false).is_err());
    package_manifest(&f.root, "library/z", false)?;

    fs::rename(&offline, &f.repo)?;
    let upgrade = plan(&f.root, Request::Update)?;
    let staged = f._dir.path().join("staged");
    crate::stage_boot_environment(&f.root, &staged, &upgrade)?;
    assert_eq!(fs::read_to_string(staged.join(COMPOSITOR))?, "next");
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "previous");
    fs::rename(&f.repo, &offline)?;
    assert!(plan(&f.root, remove("library/z")).is_err());
    assert!(plan(&staged, remove("library/a")).is_err());
    assert!(package_manifest(&staged, "library/z", false).is_err());
    assert!(verify(&staged)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

#[test]
fn profile_updates_reconcile_defaults_and_keep_operator_roots() -> Result<()> {
    use crate::{
        MarkChoice,
        query::{ProfileRole, explain_package, list_package_intent},
    };

    let f = Fixture::new()?;
    let legacy = "usr/lib/desktop/legacy";
    let default = "usr/share/desktop/default";
    let shared = "library/shared";
    let optional = "desktop/optional";
    let addon = "desktop/addon";
    f.package(
        "library/legacy",
        "1.0.0-0",
        f.file(legacy, "legacy", "0644", "")?,
    )?;
    f.package(shared, "1.0.0-0", String::new())?;
    f.package(optional, "1.0.0-0", String::new())?;
    f.package(addon, "1.0.0-0", dependency("require", shared, "1.0.0-0"))?;
    f.package(
        "desktop/base",
        "1.0.0-0",
        dependency("require", "library/legacy", "1.0.0-0")
            + &dependency("require", shared, "1.0.0-0"),
    )?;
    f.package(
        "desktop/base",
        "2.0.0-0",
        dependency("exclude", "library/legacy", "1.0.0-0"),
    )?;
    f.package(
        "desktop/old-default",
        "1.0.0-0",
        f.file(default, "old", "0644", "")?,
    )?;
    f.package(
        "desktop/new-default",
        "1.0.0-0",
        f.file(default, "new", "0644", "")?,
    )?;
    let publish_profile = |version, defaults: &str| -> Result<()> {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/base@{version}"
        {defaults}
    }}
}}
"#,
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
        publish_release(&f.repo, &f.private, &release)?;
        Ok(())
    };
    publish_profile(
        "1.0.0-0",
        "package \"desktop/old-default\"; package \"desktop/optional\"",
    )?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-key {:?}; }}
"#,
            f.repo, f.public,
        ),
    )?;
    let exact_root = f._dir.path().join("exact-root");
    for root in [&f.root, &exact_root] {
        create_configured_image(root, &config)?;
        apply(root, &plan(root, Request::InstallProfile)?)?;
        let mut packages = vec![addon.into()];
        if root == &exact_root {
            packages.push(format!("cps://canopa/{optional}@1.0.0-0"));
        }
        apply(root, &plan(root, Request::Install { packages })?)?;
    }
    apply(
        &exact_root,
        &plan(
            &exact_root,
            Request::ExactInstall {
                packages: vec![addon.into()],
            },
        )?,
    )?;
    package_manifest(&exact_root, optional, false)?;

    let offline = f.repo.with_extension("offline");
    fs::rename(&f.repo, &offline)?;
    let mark = |root: &Path, choice| {
        apply(
            root,
            &plan(
                root,
                Request::Mark {
                    choice,
                    packages: vec![optional.into()],
                },
            )?,
        )
    };
    mark(&f.root, MarkChoice::Explicit)?;
    mark(&exact_root, MarkChoice::Explicit)?;
    mark(&exact_root, MarkChoice::Automatic)?;
    for (root, requested) in [(&f.root, true), (&exact_root, false)] {
        let rows = list_package_intent(root)?;
        assert!(
            rows.iter()
                .any(|row| row.id == "cps://canopa/desktop/optional@1.0.0-0"
                    && row.requested == requested
                    && row.profile == ProfileRole::Root)
        );
        assert!(
            rows.iter()
                .any(|row| row.id == "cps://canopa/library/shared@1.0.0-0"
                    && !row.requested
                    && row.profile == ProfileRole::Dependency)
        );
        let profile_root = explain_package(root, "cps://canopa/desktop/optional")?;
        assert_eq!(profile_root.intent.requested, requested);
        assert_eq!(profile_root.intent.profile, ProfileRole::Root);
        assert_eq!(profile_root.profile_name.as_deref(), Some("desktop"));
        assert!(profile_root.dependents.is_empty());
        let dependency = explain_package(root, shared)?;
        assert!(!dependency.intent.requested);
        assert_eq!(dependency.intent.profile, ProfileRole::Dependency);
        assert_eq!(dependency.profile_name.as_deref(), Some("desktop"));
        assert_eq!(
            dependency.dependents,
            [
                "cps://canopa/desktop/addon@1.0.0-0",
                "cps://canopa/desktop/base@1.0.0-0",
            ]
        );
    }
    fs::rename(&offline, &f.repo)?;
    publish_profile("2.0.0-0", "package \"desktop/new-default\"")?;
    let staged = f._dir.path().join("staged");
    let upgrade = plan(&f.root, Request::Update)?;
    assert!(upgrade.summary().contains("Remove library/legacy 1.0.0-0"));
    crate::stage_boot_environment(&f.root, &staged, &upgrade)?;
    assert_eq!(fs::read_to_string(f.root.join(default))?, "old");
    assert!(f.root.join(legacy).is_file());
    let interrupted = plan(&exact_root, Request::Update)?;
    interrupt_apply_at(
        &exact_root,
        &f._dir.path().join("exact-update.json"),
        &interrupted,
        default,
    )?;
    fs::rename(&f.repo, &offline)?;
    assert_eq!(crate::resume_transaction(&exact_root)?, Some(6));
    for (root, keeps_optional) in [(&staged, true), (&exact_root, false)] {
        assert!(!root.join(legacy).exists());
        assert_eq!(fs::read_to_string(root.join(default))?, "new");
        let installed = list_packages(root, Scope::Installed)?;
        assert!(
            installed
                .iter()
                .all(|row| row.name != "desktop/old-default")
        );
        assert!(
            installed
                .iter()
                .any(|row| row.name == addon && row.status == Status::Explicit)
        );
        assert!(
            installed
                .iter()
                .any(|row| row.name == shared && row.status == Status::Dependency)
        );
        assert_eq!(
            installed
                .iter()
                .find(|row| row.name == optional)
                .map(|row| row.status),
            keeps_optional.then_some(Status::Explicit),
        );
        assert!(verify(root)?.iter().all(|finding| !finding.is_error));
    }
    Ok(())
}

#[test]
fn url_publishers_refresh_and_offer_choices_without_replacing_the_signed_profile() -> Result<()> {
    let main = Fixture::new()?;
    let companion = Fixture::new()?;
    let extras = Fixture::new()?;
    let base = "desktop/base";
    let library = "library/runtime";
    let editor = "apps/editor";
    companion.package_as("seed", library, "1.0.0-0", String::new())?;
    let pin = fs::read_to_string(companion.repo.join("current"))?;
    companion.package_as("seed", library, "2.0.0-0", String::new())?;
    for version in ["1.0.0-0", "2.0.0-0"] {
        main.package(
            base,
            version,
            main.file(COMPOSITOR, version, "0755", "")?
                + &dependency("require", library, "1.0.0-0"),
        )?;
    }
    main.package(editor, "1.0.0-0", String::new())?;
    extras.package_as("extras", base, "9.0.0-0", String::new())?;
    extras.package_as("extras", library, "9.0.0-0", String::new())?;
    extras.package_as(
        "extras",
        editor,
        "2.0.0-0",
        format!("    require {library:?}\n"),
    )?;
    extras.package_as(
        "extras",
        "apps/conflict",
        "1.0.0-0",
        extras.file(COMPOSITOR, "conflicting content", "0755", "")?,
    )?;
    let work = extras._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    let cache = work.join("cache");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_root(&anchor, &tuf_key, None)?;
    let served = work.join("mirror");
    let serve_extras = |version| -> Result<()> {
        let mirror = work.join(format!("mirror-{version}"));
        sign_publisher_release(
            &extras, &mirror, version, fresh, &anchor, &tuf_key, "extras",
        )?;
        let next = work.join("next-mirror");
        std::os::unix::fs::symlink(&mirror, &next)?;
        fs::rename(next, &served)?;
        Ok(())
    };
    serve_extras(1)?;
    let url = Url::from_directory_path(&served).unwrap();
    let url = url.as_str().trim_end_matches('/');

    let refresh = |version: &str| -> Result<()> {
        fs::write(
            main.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/base@{version}"
        publisher "seed" catalog={:?} {{ repository {:?}; trust-key {:?}; }}
    }}
}}
"#,
                pin.trim(),
                companion.repo,
                companion.public
            ),
        )?;
        let release = main._dir.path().join("refresh.kdl");
        let current = fs::read_to_string(main.repo.join("current"))?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                current.trim()
            ),
        )?;
        publish_release(&main.repo, &main.private, &release)?;
        Ok(())
    };
    refresh("1.0.0-0")?;
    let config = main._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-key {:?}; }}
publisher "seed" {{ repository {:?}; trust-key {:?}; }}
"#,
            main.repo, main.public, companion.repo, companion.public
        ),
    )?;
    create_configured_image(&main.root, &config)?;
    let add = |key: &Path, cache: &Path| {
        crate::add_publisher_from_url(&main.root, "extras", url, &anchor, key, cache, None)
    };
    assert!(add(&extras.public, &cache).is_err());
    assert!(!cache.exists());
    apply(&main.root, &plan(&main.root, Request::InstallProfile)?)?;
    let install = |packages| plan(&main.root, Request::Install { packages });
    let previous = install(vec![editor.into()])?;
    assert!(
        add(&extras.public, &companion.repo)
            .unwrap_err()
            .to_string()
            .contains("overlaps")
    );
    assert!(add(&main.public, &cache).is_err());
    assert!(!cache.exists());
    assert_eq!(crate::list_publishers(&main.root)?.len(), 2);
    assert_eq!(add(&extras.public, &cache)?, cache);
    assert!(
        apply(&main.root, &previous)
            .unwrap_err()
            .to_string()
            .contains("publisher configuration changed")
    );
    assert!(crate::prefer_publisher(&main.root, "seed").is_err());
    assert!(crate::remove_publisher(&main.root, "canopa").is_err());
    assert!(install(vec![format!("cps://extras/{base}")]).is_err());
    assert!(install(vec!["cps://extras/apps/conflict".into()]).is_err());

    let mut requests = [base.to_owned(), editor.to_owned(), editor.to_owned()];
    let mut prompts = 0;
    crate::query::select_install_sources(&main.root, &mut requests, &mut |name, choices| {
        prompts += 1;
        assert_eq!(name, editor);
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.publisher.as_str(), choice.version.as_str()))
                .collect::<Vec<_>>(),
            [("canopa", "1.0.0-0"), ("extras", "2.0.0-0")]
        );
        Ok(1)
    })?;
    assert_eq!(prompts, 1);
    assert_eq!(
        requests,
        [base, "cps://extras/apps/editor", "cps://extras/apps/editor"]
    );
    apply(&main.root, &install(requests.into())?)?;
    let locked = crate::capture_image_lock(&main.root)?;
    assert!(crate::remove_publisher(&main.root, "extras").is_err());
    assert!(install(vec![format!("cps://canopa/{editor}")]).is_err());

    extras.package_as(
        "extras",
        editor,
        "3.0.0-0",
        format!("    require {library:?}\n")
            + &extras.file("usr/share/extras/editor", "third-party payload", "0644", "")?,
    )?;
    serve_extras(2)?;
    let saved_root = cache.join(".tuf/refresh-root.json");
    let original = fs::read(&saved_root)?;
    let old_extra = fs::read(cache.join("current"))?;
    fs::write(&saved_root, b"changed anchor")?;
    let error = mirror_release::refresh_url_publishers(
        &main.root,
        &BTreeSet::from(["extras"]),
        false,
        true,
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("different trust root or publisher"));
    assert_eq!(fs::read(cache.join("current"))?, old_extra);
    fs::write(&saved_root, original)?;

    // Even a marked cache cannot make a signed companion refresh independently.
    fs::create_dir(companion.repo.join(".tuf"))?;
    fs::write(companion.repo.join(".tuf/refresh-root.json"), b"invalid")?;
    let primary_before = fs::read(main.repo.join("current"))?;
    let companion_before = fs::read(companion.repo.join("current"))?;
    assert!(
        mirror_release::refresh_url_publishers(
            &main.root,
            &BTreeSet::new(),
            false,
            true,
            &BTreeMap::new()
        )?
        .is_empty()
    );
    assert_ne!(fs::read(cache.join("current"))?, old_extra);
    assert_eq!(fs::read(main.repo.join("current"))?, primary_before);
    assert_eq!(fs::read(companion.repo.join("current"))?, companion_before);
    refresh("2.0.0-0")?;
    let locked_config = work.join("locked-image.kdl");
    fs::write(
        &locked_config,
        format!(
            "{}publisher \"extras\" {{ repository {cache:?}; trust-key {:?}; }}\n",
            fs::read_to_string(&config)?,
            extras.public,
        ),
    )?;
    let locked_root = work.join("locked-root");
    crate::create_image_from_lock(&locked_root, &locked_config, &locked)?;
    let request = Request::InstallLock {
        lock: Box::new(locked.clone()),
    };
    let saved_lock_plan = plan(&locked_root, request.clone())?;
    assert_eq!(saved_lock_plan.catalogs["seed"], pin.trim());

    let mut altered = serde_json::to_value(&locked)?;
    assert_eq!(altered["format"], 1);
    assert_eq!(altered["kind"], "image-lock");
    let lock_file = work.join("test.lock.json");
    let mut untyped = altered.clone();
    untyped.as_object_mut().unwrap().remove("kind");
    fs::write(&lock_file, serde_json::to_vec(&untyped)?)?;
    let error = crate::read_image_lock(&lock_file).unwrap_err();
    assert!(format!("{error:#}").contains("missing field `kind`"));
    let mut wrong_version = altered.clone();
    wrong_version["format"] = 2.into();
    fs::write(&lock_file, serde_json::to_vec(&wrong_version)?)?;
    let error = crate::read_image_lock(&lock_file).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported image lock format"));
    fs::write(&lock_file, serde_json::to_vec(&altered)?)?;
    assert_eq!(crate::read_image_lock(&lock_file)?, locked);

    altered["packages"][editor]["manifest_sha256"] = serde_json::Value::String("0".repeat(64));
    let bad = Request::InstallLock {
        lock: Box::new(serde_json::from_value(altered)?),
    };
    assert!(
        format!("{:#}", plan(&locked_root, bad).unwrap_err()).contains("exact manifest digest")
    );
    let saved_lock_plan = serde_json::from_slice(&serde_json::to_vec(&saved_lock_plan)?)?;
    fetch_plan(&locked_root, &saved_lock_plan)?;
    apply(&locked_root, &saved_lock_plan)?;
    let diff = crate::image_diff(&main.root, &locked_root, true)?;
    assert_ne!(diff.before.id, diff.after.id);
    assert!(diff.packages.is_empty() && diff.paths.unwrap().is_empty());
    assert_eq!(crate::capture_image_lock(&locked_root)?, locked);
    let bundle = work.join("bundle");
    crate::export_image_bundle(&locked_root, &bundle)?;
    assert!(crate::export_image_bundle(&locked_root, &bundle).is_err());
    let repo_offline = main.repo.with_extension("offline");
    fs::rename(&main.repo, &repo_offline)?;
    let bundled_root = work.join("bundled-root");
    let blob = fs::read_dir(bundle.join("repositories/canopa/blobs"))?
        .next()
        .unwrap()?
        .path();
    let saved_blob = blob.with_extension("saved");
    fs::rename(&blob, &saved_blob)?;
    let error =
        crate::create_image_from_bundle(&bundled_root, &locked_config, &bundle).unwrap_err();
    assert!(format!("{error:#}").contains("missing or damaged bundle payloads"));
    assert!(!bundled_root.exists());
    fs::rename(&saved_blob, &blob)?;
    crate::create_image_from_bundle(&bundled_root, &locked_config, &bundle)?;
    let bundle_lock = crate::read_image_lock(&bundle.join("image.lock.json"))?;
    let bundled_plan = plan(
        &bundled_root,
        Request::InstallLock {
            lock: Box::new(bundle_lock),
        },
    )?;
    apply(&bundled_root, &bundled_plan)?;
    assert_eq!(crate::capture_image_lock(&bundled_root)?, locked);
    assert!(
        crate::image_diff(&locked_root, &bundled_root, true)?
            .packages
            .is_empty()
    );
    fs::rename(&repo_offline, &main.repo)?;
    assert!(plan(&locked_root, request).is_err());

    // A failed optional mirror can have a newer catalog cached with missing
    // payloads. Plan the base update without selecting any of those packages.
    let offline_mirror = served.with_extension("offline");
    fs::rename(&served, &offline_mirror)?;
    let full_upgrade = plan(&main.root, Request::Update)?;
    let fetch_error = fetch_plan(&main.root, &full_upgrade).unwrap_err();
    let publisher = fetch_error
        .downcast_ref::<crate::apply::PublisherFetchError>()
        .unwrap();
    assert_eq!(publisher.0, "extras");
    let refresh_with = |policies: &BTreeMap<String, mirror_release::RefreshPolicy>| {
        mirror_release::refresh_url_publishers(&main.root, &BTreeSet::new(), false, true, policies)
    };
    // By default an unavailable optional mirror fails the update.
    let error = refresh_with(&BTreeMap::new()).unwrap_err();
    assert!(format!("{error:#}").contains("does not allow deferral"));
    let defer = BTreeMap::from([(
        "extras".to_owned(),
        mirror_release::RefreshPolicy {
            defer_unavailable_hours: Some(24),
        },
    )]);
    let freshness = mirror_release::url_publisher_freshness(&main.root, &defer)?;
    assert_eq!(freshness.len(), 1);
    assert!(!freshness[0].beyond_policy && freshness[0].metadata_expires.is_some());
    let deferred = refresh_with(&defer)?;
    assert_eq!(deferred, BTreeSet::from(["extras".to_owned()]));
    assert_eq!(
        mirror_release::refresh_url_publishers(
            &main.root,
            &BTreeSet::new(),
            true,
            false,
            &BTreeMap::new()
        )?,
        deferred
    );
    // Deferral ends once the cache is older than the policy allows.
    let refreshed = cache.join(".tuf/refreshed");
    let last_refresh = fs::read(&refreshed)?;
    fs::write(&refreshed, b"1000\n")?;
    let error = refresh_with(&defer).unwrap_err();
    assert!(format!("{error:#}").contains("older than the refresh policy allows"));
    assert!(mirror_release::url_publisher_freshness(&main.root, &defer)?[0].beyond_policy);
    fs::write(&refreshed, last_refresh)?;
    // A mirror that fails verification is never deferred.
    let tampered = work.join("tampered-mirror");
    fs::create_dir_all(tampered.join("metadata"))?;
    std::os::unix::fs::symlink(
        fs::canonicalize(&offline_mirror)?.join("targets"),
        tampered.join("targets"),
    )?;
    for entry in fs::read_dir(offline_mirror.join("metadata"))? {
        let entry = entry?;
        fs::copy(
            entry.path(),
            tampered.join("metadata").join(entry.file_name()),
        )?;
    }
    let timestamp = tampered.join("metadata/timestamp.json");
    let mut forged: serde_json::Value = serde_json::from_slice(&fs::read(&timestamp)?)?;
    forged["signed"]["version"] = 999.into();
    fs::write(&timestamp, serde_json::to_vec(&forged)?)?;
    std::os::unix::fs::symlink(&tampered, &served)?;
    let error = refresh_with(&defer).unwrap_err();
    assert!(
        format!("{error:#}").contains("not availability"),
        "{error:#}"
    );
    fs::remove_file(&served)?;
    let (base_only, _) = crate::plan::plan_system(
        &main.root,
        Request::Update,
        crate::PlanOptions::default(),
        crate::plan::UpdateSelection {
            deferred_publishers: Some(&deferred),
            ..Default::default()
        },
    )?;
    assert_eq!(base_only.deferred_publishers(), &deferred);
    assert_eq!(base_only.catalogs["seed"], pin.trim());
    assert!(!base_only.catalogs.contains_key("extras"));
    assert!(
        base_only
            .package_changes()
            .iter()
            .any(|change| change.name == base)
    );
    assert!(
        base_only
            .package_changes()
            .iter()
            .all(|change| change.name != editor)
    );

    // A saved plan must still be applicable when even the optional local
    // cache is unavailable. The installed optional package keeps its archive.
    fs::rename(&cache, cache.with_extension("unavailable"))?;
    let base_only = serde_json::from_slice(&serde_json::to_vec(&base_only)?)?;
    fetch_plan(&main.root, &base_only)?;
    let base_upgrade = work.join("base-only-upgrade");
    crate::stage_boot_environment(&main.root, &base_upgrade, &base_only)?;
    assert!(verify(&base_upgrade)?.is_empty());
    assert!(
        list_packages(&base_upgrade, Scope::Installed)?
            .iter()
            .any(|row| { row.id == "cps://extras/apps/editor@2.0.0-0" })
    );
    assert_eq!(
        fs::read_to_string(base_upgrade.join(COMPOSITOR))?,
        "2.0.0-0"
    );
    fs::rename(cache.with_extension("unavailable"), &cache)?;
    fs::rename(&offline_mirror, &served)?;

    let upgrade = plan(&main.root, Request::Update)?;
    assert_eq!(upgrade.catalogs["seed"], pin.trim());
    let staged = main._dir.path().join("upgrade");
    fetch_plan(&main.root, &upgrade)?;
    crate::stage_boot_environment(&main.root, &staged, &upgrade)?;
    assert_eq!(fs::read_to_string(staged.join(COMPOSITOR))?, "2.0.0-0");
    let installed = list_packages(&staged, Scope::Installed)?;
    assert!(
        installed
            .iter()
            .any(|row| row.id == "cps://extras/apps/editor@3.0.0-0")
    );
    assert!(
        installed
            .iter()
            .any(|row| row.id == "cps://seed/library/runtime@1.0.0-0")
    );
    for repo in [&main.repo, &companion.repo, &cache] {
        fs::rename(repo, repo.with_extension("offline"))?;
    }
    assert!(verify(&staged)?.is_empty());
    Ok(())
}

#[test]
fn release_upgrade_profiles_check_retained_packages_and_path_transitions() -> Result<()> {
    let f = Fixture::new()?;
    f.package("library/legacy", "1.0.0-0", String::new())?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        dependency("require", "library/legacy", "1.0.0-0"),
    )?;
    f.package(
        "desktop/session",
        "1.0.0-0",
        dependency("require", "library/legacy", "1.0.0-0"),
    )?;
    f.package(
        "test/layout",
        "1.0.0-0",
        "    dir \"opt/layout\" mode=\"0755\" uid=0 gid=0\n".to_owned(),
    )?;
    let baseline = fs::read_to_string(f.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        dependency("exclude", "library/legacy", "1.0.0-0"),
    )?;
    f.package("desktop/session", "2.0.0-0", String::new())?;
    f.package(
        "test/layout",
        "2.0.0-0",
        f.file("opt/layout", "replacement", "0644", "")?,
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    let old = fs::read_to_string(f.repo.join("current"))?;
    let id = "cps://canopa/test/retire@1.0.0-0";
    let write_policy = |root: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                "schema-version 1 kind=\"publish-policy\"\npublisher \"canopa\" {{\n    builder-key \"builder.der\"\n    profile \"desktop\" architecture=\"x86_64\" upgrade-from={:?} {{\n        package {root:?}\n    }}\n}}\n",
                baseline.trim(),
            ),
        )
    };
    write_policy("desktop/compositor")?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("update selects cps://canopa/desktop/compositor@1.0.0-0 instead of cps://canopa/desktop/compositor@2.0.0-0"), "{message}");
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("desktop/session")?;
    let error =
        withdraw_package(&f.repo, &f.private, "cps://canopa/library/legacy@1.0.0-0").unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("update retains unavailable package cps://canopa/library/legacy@1.0.0-0"),
        "{message}"
    );
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);

    write_policy("test/layout")?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("cannot change between directory and leaf actions at opt/layout"),
        "{message}"
    );
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, old);
    write_policy("desktop/compositor")?;
    let release = f._dir.path().join("obsolete.kdl");
    fs::write(
        &release,
        format!(
            "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} {{\n    obsolete \"cps://canopa/library/legacy@2.0.0-0\"\n}}\n",
            old.trim(),
        ),
    )?;
    publish_release(&f.repo, &f.private, &release)?;
    assert_eq!(
        list_obsoletions(&f.repo, "canopa", &f.public)?,
        ["cps://canopa/library/legacy@2.0.0-0"]
    );
    Ok(())
}

#[test]
fn signed_renames_upgrade_in_one_transaction_and_preserve_install_intent() -> Result<()> {
    use crate::{add_publisher, prefer_publisher, resume_transaction};

    let f = Fixture::new()?;
    let other = Fixture::new()?;
    let old = "apps/old-editor";
    let intermediate = "apps/editor";
    let successor = "apps/text-editor";
    let old_addon = "apps/old-addon";
    let addon = "apps/addon";
    let launcher = "apps/launcher";
    f.package(old, "1.0.0-0", f.file(COMPOSITOR, "old", "0755", "")?)?;
    f.package(old_addon, "1.0.0-0", String::new())?;
    f.package(
        launcher,
        "1.0.0-0",
        dependency("require", old_addon, "1.0.0-0"),
    )?;
    other.package_as("other", successor, "9.0.0-0", String::new())?;
    f.create()?;
    add_publisher(&f.root, "other", &other.repo, &other.public)?;
    prefer_publisher(&f.root, "other")?;
    let install = |packages: &[&str]| Request::Install {
        packages: packages.iter().map(|name| (*name).into()).collect(),
    };
    apply(&f.root, &plan(&f.root, install(&[old, launcher]))?)?;
    let conflicting = f._dir.path().join("conflicting-publisher");
    crate::stage_boot_environment(
        &f.root,
        &conflicting,
        &plan(&f.root, install(&["cps://other/apps/text-editor"]))?,
    )?;
    f.package(
        intermediate,
        "1.0.0-0",
        f.file(COMPOSITOR, "intermediate", "0755", "")?,
    )?;
    f.package(successor, "1.0.0-0", f.file(COMPOSITOR, "new", "0755", "")?)?;
    f.package(addon, "1.0.0-0", String::new())?;

    let release = f._dir.path().join("rename.kdl");
    let publish_rename = |actions: &str| -> Result<String> {
        let current = fs::read_to_string(f.repo.join("current"))?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} {{\n{actions}\n}}\n",
                current.trim()
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)
    };
    let current = fs::read_to_string(f.repo.join("current"))?;
    for target in ["cps://other/apps/editor", "apps/missing"] {
        assert!(
            publish_rename(&format!(
                "rename \"cps://canopa/{old}@2.0.0-0\" to={target:?}"
            ))
            .is_err()
        );
        assert_eq!(fs::read_to_string(f.repo.join("current"))?, current);
    }
    publish_rename(&format!(
        "rename \"cps://canopa/{old}@2.0.0-0\" to={intermediate:?}\nrename \"cps://canopa/{old_addon}@2.0.0-0\" to={addon:?}"
    ))?;
    assert!(
        plan(&f.root, Request::Update).is_err(),
        "launcher still requires the old addon"
    );
    assert!(
        format!("{:#}", plan(&f.root, install(&[launcher])).unwrap_err())
            .contains("upgrade the image")
    );
    f.package(launcher, "2.0.0-0", String::new())?;
    let saved = plan(&f.root, Request::Update)?;
    publish_rename(&format!(
        "rename \"cps://canopa/{intermediate}@2.0.0-0\" to={successor:?}"
    ))?;
    let error = plan(&conflicting, Request::Update).unwrap_err();
    assert!(format!("{error:#}").contains("reserved for a different publisher"));
    let staged = f._dir.path().join("pinned-rename");
    crate::stage_boot_environment(&f.root, &staged, &saved)?;
    assert_eq!(fs::read_to_string(staged.join(COMPOSITOR))?, "intermediate");
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    let current = fs::read_to_string(f.repo.join("current"))?;
    assert!(
        publish_rename(&format!(
            "rename \"cps://canopa/{successor}@2.0.0-0\" to={old:?}"
        ))
        .is_err()
    );
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, current);
    assert!(f.package(old, "3.0.0-0", String::new()).is_err());

    for (freeze, unfreeze, message) in [
        (old.to_owned(), old, "validate the replacement"),
        (
            format!("{successor}@9.0.0-0"),
            successor,
            "resolving package selection with freezes",
        ),
    ] {
        apply(
            &f.root,
            &plan(
                &f.root,
                Request::Freeze {
                    packages: vec![freeze],
                    reason: Some("validate the replacement".into()),
                },
            )?,
        )?;
        let error = plan(&f.root, Request::Update).unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
        apply(
            &f.root,
            &plan(
                &f.root,
                Request::Unfreeze {
                    packages: vec![unfreeze.into()],
                },
            )?,
        )?;
    }

    let upgrade = plan(&f.root, Request::Update)?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("rename-plan.json"),
        &upgrade,
        COMPOSITOR,
    )?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    resume_transaction(&f.root)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "new");
    let installed = list_packages(&f.root, Scope::Installed)?;
    assert_eq!(
        installed
            .iter()
            .map(|row| (row.id.as_str(), row.status))
            .collect::<Vec<_>>(),
        [
            ("cps://canopa/apps/addon@1.0.0-0", Status::Dependency),
            ("cps://canopa/apps/launcher@2.0.0-0", Status::Explicit),
            ("cps://canopa/apps/text-editor@1.0.0-0", Status::Explicit),
        ]
    );
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn signed_profile_renames_require_the_successor_and_keep_local_marks_separate() -> Result<()> {
    use crate::query::{ProfileRole, list_package_intent};

    let f = Fixture::new()?;
    let base = "apps/base";
    let next_base = "apps/new-base";
    let local = "apps/local";
    let next_local = "apps/new-local";
    for name in [base, next_base, local, next_local, "apps/unrelated"] {
        f.package(name, "1.0.0-0", String::new())?;
    }
    let policy = |name: &str| -> Result<()> {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                "schema-version 1 kind=\"publish-policy\"\npublisher \"canopa\" {{\n    builder-key \"unused.der\"\n    profile \"desktop\" architecture=\"x86_64\" {{ package {name:?}; }}\n}}\n"
            ),
        )?;
        Ok(())
    };
    let release = f._dir.path().join("release.kdl");
    let publish_profile = |actions: &str| -> Result<String> {
        let current = fs::read_to_string(f.repo.join("current"))?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true {{\n{actions}\n}}\n",
                current.trim()
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)
    };
    policy(base)?;
    publish_profile("")?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            "schema-version 1 kind=\"image-config\"\nrelease-profile \"desktop\"\npublisher \"canopa\" {{ repository {:?}; trust-key {:?}; }}\n",
            f.repo, f.public
        ),
    )?;
    crate::create_configured_image(&f.root, &config)?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;
    apply(
        &f.root,
        &plan(
            &f.root,
            Request::Install {
                packages: vec![local.into()],
            },
        )?,
    )?;
    let actions = format!(
        "rename \"cps://canopa/{base}@2.0.0-0\" to={next_base:?}\nrename \"cps://canopa/{local}@2.0.0-0\" to={next_local:?}"
    );
    let current = fs::read_to_string(f.repo.join("current"))?;
    policy("apps/unrelated")?;
    let error = publish_profile(&actions).unwrap_err();
    assert!(format!("{error:#}").contains("signed profile update must include its successor"));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, current);
    policy(next_base)?;
    publish_profile(&actions)?;
    let update = plan(&f.root, Request::Update)?;
    assert_eq!(update.request, Request::UpdateProfile);
    apply(&f.root, &update)?;
    let intent = list_package_intent(&f.root)?;
    assert_eq!(
        intent
            .iter()
            .map(|row| (row.id.as_str(), row.requested, row.profile))
            .collect::<Vec<_>>(),
        [
            (
                "cps://canopa/apps/new-base@1.0.0-0",
                false,
                ProfileRole::Root
            ),
            (
                "cps://canopa/apps/new-local@1.0.0-0",
                true,
                ProfileRole::None
            ),
        ]
    );
    assert!(verify(&f.root)?.is_empty());
    Ok(())
}

#[test]
fn upgrade_profile_reuses_signed_baseline_roots_and_companions() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let root_dir = format!("    dir \"opt/pinned-root\" mode=\"0755\" uid={uid} gid={gid}\n");
    let companion_dir =
        format!("    dir \"opt/pinned-companion\" mode=\"0755\" uid={uid} gid={gid}\n");
    let runtime_requirement = "    require \"library/runtime\"\n";
    seed.package_as("seed", "library/runtime", "1.0.0-0", companion_dir.clone())?;
    let old_pin = fs::read_to_string(seed.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        format!("{runtime_requirement}{root_dir}"),
    )?;
    let replacement = f.file("opt/pinned-root", "replacement", "0644", "")?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        format!("{runtime_requirement}{replacement}"),
    )?;
    f.package("desktop/addon", "1.0.0-0", String::new())?;
    f.package("desktop/addon", "2.0.0-0", String::new())?;
    f.package(
        "desktop/runtime-plugin",
        "1.0.0-0",
        dependency("require", "library/runtime", "1.0.0-0"),
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;

    let write_policy = |version: &str, pin: &str, base: &str, override_pin: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" {base} {{
        package "desktop/compositor@{version}"
        publisher "seed" catalog={:?} {override_pin} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
                pin.trim(),
                seed.repo,
                seed.public,
            ),
        )
    };
    let pointer = f.repo.join("current");
    let unsigned_base = fs::read_to_string(&pointer)?;
    write_policy("1.0.0-0", &old_pin, "", "")?;
    withdraw_package(&f.repo, &f.private, "cps://canopa/test/retire@1.0.0-0")?;
    let signed_base = fs::read_to_string(&pointer)?;
    let base = format!("upgrade-from={:?}", signed_base.trim());
    let refresh = f._dir.path().join("refresh.kdl");
    fs::write(
        &refresh,
        format!(
            "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
            signed_base.trim()
        ),
    )?;
    let rejected = |release: &Path, expected: &str| -> Result<()> {
        let error = publish_release(&f.repo, &f.private, release).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(fs::read_to_string(&pointer)?, signed_base);
        Ok(())
    };
    let policy = f.repo.join("publish-policy.kdl");
    fs::write(
        &policy,
        fs::read_to_string(&policy)?.replace("architecture=\"x86_64\"", "architecture=\"aarch64\""),
    )?;
    rejected(
        &refresh,
        "image configuration differs from the signed baseline",
    )?;
    write_policy("1.0.0-0", &old_pin, "", "")?;
    assert_eq!(
        publish_release(&f.repo, &f.private, &refresh)?,
        signed_base.trim()
    );

    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{
    repository {:?}
    trust-key {:?}
}}
publisher "seed" {{
    repository {:?}
    trust-key {:?}
}}
"#,
            f.repo, f.public, seed.repo, seed.public,
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;
    assert!(f.root.join("opt/pinned-root").is_dir());
    assert!(f.root.join("opt/pinned-companion").is_dir());
    let installed = list_packages(&f.root, Scope::Installed)?;
    assert_eq!(installed.len(), 2);
    assert!(installed.iter().any(|row| {
        row.id == "cps://canopa/desktop/compositor@1.0.0-0" && row.status == Status::Explicit
    }));
    assert!(installed.iter().any(|row| {
        row.id == "cps://seed/library/runtime@1.0.0-0" && row.status == Status::Dependency
    }));
    let error = plan(&f.root, Request::InstallProfile).unwrap_err();
    assert!(format!("{error:#}").contains("requires an image with no installed packages"));
    let same_release = plan(&f.root, Request::Update)?;
    assert_eq!(same_release.request, Request::UpdateProfile);
    assert!(!same_release.summary().contains("Update "));
    let newer_root = f._dir.path().join("locally-newer-image");
    create_configured_image(&newer_root, &config)?;
    let manual = plan(
        &newer_root,
        Request::Install {
            packages: vec!["desktop/compositor@2.0.0-0".to_owned()],
        },
    )?;
    apply(&newer_root, &manual)?;
    let error = plan(&newer_root, Request::Update).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("signed profile would downgrade cps://canopa/desktop/compositor@2.0.0-0"),
        "{error:#}"
    );
    package_manifest(
        &newer_root,
        "cps://canopa/desktop/compositor@2.0.0-0",
        false,
    )?;

    let conflicting_root = f._dir.path().join("conflicting-image");
    create_configured_image(&conflicting_root, &config)?;
    apply(
        &conflicting_root,
        &plan(&conflicting_root, Request::InstallProfile)?,
    )?;
    let plugin = plan(
        &conflicting_root,
        Request::Install {
            packages: vec!["desktop/runtime-plugin".to_owned()],
        },
    )?;
    apply(&conflicting_root, &plugin)?;
    let addon = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/addon@1.0.0-0".to_owned()],
        },
    )?;
    apply(&f.root, &addon)?;

    write_policy("2.0.0-0", &old_pin, "", "")?;
    rejected(
        &refresh,
        "cannot change between directory and leaf actions at opt/pinned-root",
    )?;
    // The older catalog would select 2.0 in its baseline, so only the
    // mandatory check from the signed profile at 1.0 detects the transition.
    let older = format!("upgrade-from={:?}", unsigned_base.trim());
    write_policy("2.0.0-0", &old_pin, &older, "")?;
    rejected(
        &refresh,
        "cannot change between directory and leaf actions at opt/pinned-root",
    )?;

    // Withdraw the rejected 2.0 layout so the next release can update the companion alone.
    let retire_newer = f._dir.path().join("withdraw-newer.kdl");
    fs::write(
        &retire_newer,
        format!(
            "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} {{\n    withdraw \"cps://canopa/desktop/compositor@2.0.0-0\"\n}}\n",
            signed_base.trim()
        ),
    )?;
    let replacement = seed.file("opt/pinned-companion", "replacement", "0644", "")?;
    seed.package_as("seed", "library/runtime", "2.0.0-0", replacement)?;
    let new_pin = fs::read_to_string(seed.repo.join("current"))?;
    write_policy(
        "1.0.0-0",
        &new_pin,
        &base,
        &format!("upgrade-from={:?}", new_pin.trim()),
    )?;
    rejected(&retire_newer, "differs from the signed baseline pin")?;
    write_policy("1.0.0-0", &new_pin, "", "")?;
    rejected(
        &retire_newer,
        "cannot change between directory and leaf actions at opt/pinned-companion",
    )?;

    seed.package_as("seed", "library/runtime", "3.0.0-0", companion_dir)?;
    let latest_pin = fs::read_to_string(seed.repo.join("current"))?;
    write_policy("1.0.0-0", &latest_pin, "", "")?;
    let next = publish_release(&f.repo, &f.private, &retire_newer)?;
    fs::write(
        &refresh,
        format!(
            "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={next:?} refresh-profiles=#true\n"
        ),
    )?;
    let matrix = fs::read_to_string(&policy)?.replace(
        "        package \"desktop/compositor@1.0.0-0\"",
        &format!(
            "        upgrade-from {:?}\n        upgrade-from {next:?}\n        package \"desktop/compositor@1.0.0-0\"",
            signed_base.trim(),
        ),
    );
    fs::write(&policy, &matrix)?;
    assert_eq!(publish_release(&f.repo, &f.private, &refresh)?, next);
    fs::write(
        &policy,
        matrix.replace(
            &format!("upgrade-from {next:?}"),
            &format!("upgrade-from {:?}", unsigned_base.trim()),
        ),
    )?;
    let error = publish_release(&f.repo, &f.private, &refresh).unwrap_err();
    assert!(
        format!("{error:#}").contains("baseline catalog has no signed composition"),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(&pointer)?.trim(), next);

    write_policy("1.0.0-0", &latest_pin, &older, "")?;
    let error = publish_release(&f.repo, &f.private, &refresh).unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!("checking upgrade from {}", unsigned_base.trim())),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(&pointer)?.trim(), next);
    let error = plan(&conflicting_root, Request::Update).unwrap_err();
    assert!(
        format!("{error:#}").contains("library/runtime"),
        "{error:#}"
    );
    package_manifest(
        &conflicting_root,
        "cps://seed/library/runtime@1.0.0-0",
        false,
    )?;
    let upgrade = plan(&f.root, Request::Update)?;
    assert_eq!(upgrade.request, Request::UpdateProfile);
    assert_eq!(upgrade.catalogs["seed"], latest_pin.trim());
    let staged = f._dir.path().join("staged");
    crate::stage_boot_environment(&f.root, &staged, &upgrade)?;
    assert!(staged.join("opt/pinned-root").is_dir());
    assert!(staged.join("opt/pinned-companion").is_dir());
    package_manifest(&staged, "cps://seed/library/runtime@3.0.0-0", false)?;
    package_manifest(&f.root, "cps://seed/library/runtime@1.0.0-0", false)?;
    assert!(list_packages(&staged, Scope::Installed)?.iter().any(|row| {
        row.id == "cps://canopa/desktop/addon@2.0.0-0" && row.status == Status::Explicit
    }));
    Ok(())
}

#[test]
fn release_profiles_pin_and_authenticate_sticky_companion_publishers() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    seed.package_as("seed", "library/runtime", "1.0.0-0", String::new())?;
    let seed_baseline = fs::read_to_string(seed.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        dependency("require", "library/runtime", "1.0.0-0"),
    )?;
    let baseline = fs::read_to_string(f.repo.join("current"))?;
    seed.package_as("seed", "library/runtime", "2.0.0-0", String::new())?;
    let seed_current = fs::read_to_string(seed.repo.join("current"))?;
    f.package("library/runtime", "2.0.0-0", String::new())?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        dependency("require", "library/runtime", "2.0.0-0"),
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    let pointer = f.repo.join("current");
    let old = fs::read_to_string(&pointer)?;
    let id = "cps://canopa/test/retire@1.0.0-0";
    let write_policy = |pin: &str, key: &Path| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" upgrade-from={:?} {{
        package "desktop/compositor"
        publisher "seed" catalog={:?} upgrade-from={:?} {{
            repository {:?}
            trust-key {key:?}
        }}
    }}
}}
"#,
                baseline.trim(),
                pin.trim(),
                seed_baseline.trim(),
                seed.repo,
            ),
        )
    };

    write_policy(&seed_current, &f.public)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    assert!(format!("{error:#}").contains("invalid signature on catalog"));
    assert_eq!(fs::read_to_string(&pointer)?, old);

    write_policy(&seed_baseline, &seed.public)?;
    let error = withdraw_package(&f.repo, &f.private, id).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("update selects cps://canopa/desktop/compositor@1.0.0-0 instead of cps://canopa/desktop/compositor@2.0.0-0"), "{message}");
    assert_eq!(fs::read_to_string(&pointer)?, old);

    write_policy(&seed_current, &seed.public)?;
    fs::remove_file(seed.repo.join("current"))?;
    withdraw_package(&f.repo, &f.private, id)?;
    assert!(list_withdrawals(&f.repo, "canopa", &f.public)?.contains_key(id));
    Ok(())
}

#[test]
fn profiled_image_follows_signed_catalog_set_and_recovers_saved_plans() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let payload = "usr/lib/runtime.marker";
    seed.package_as(
        "seed",
        "library/runtime",
        "1.0.0-0",
        seed.file(payload, "old", "0644", "")?,
    )?;
    let old_pin = fs::read_to_string(seed.repo.join("current"))?;
    seed.package_as(
        "seed",
        "library/runtime",
        "2.0.0-0",
        seed.file(payload, "new", "0644", "")?,
    )?;
    let new_pin = fs::read_to_string(seed.repo.join("current"))?;
    // Create a same-sequence fork and extend it without the real second release.
    fs::write(seed.repo.join("current"), &old_pin)?;
    seed.package_as("seed", "library/fork", "1.0.0-0", String::new())?;
    let equal_fork = fs::read_to_string(seed.repo.join("current"))?;
    seed.package_as("seed", "library/fork", "2.0.0-0", String::new())?;
    let newer_fork = fs::read_to_string(seed.repo.join("current"))?;
    fs::write(seed.repo.join("current"), &new_pin)?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        "    require \"library/runtime\"\n".to_owned(),
    )?;
    let pointer = f.repo.join("current");
    let base = fs::read_to_string(&pointer)?;
    let refresh = |base: &str| -> Result<String> {
        let path = f._dir.path().join("refresh.kdl");
        fs::write(
            &path,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim()
            ),
        )?;
        publish_release(&f.repo, &f.private, &path)
    };
    let error = refresh(&base).unwrap_err();
    assert!(
        format!("{error:#}").contains("refresh-profiles requires release composition profiles")
    );
    assert_eq!(fs::read_to_string(&pointer)?, base);
    let config = f._dir.path().join("image.kdl");
    let write_config = |arch: &str| {
        fs::write(
            &config,
            format!(
                r#"schema-version 1 kind="image-config"
architecture {arch:?}
release-profile "desktop"
publisher "canopa" {{
    repository {:?}
    trust-key {:?}
}}
publisher "seed" {{
    repository {:?}
    trust-key {:?}
}}
"#,
                f.repo, f.public, seed.repo, seed.public
            ),
        )
    };
    let write_policy = |pin: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/compositor"
        publisher "seed" catalog={:?} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
                pin.trim(),
                seed.repo,
                seed.public
            ),
        )
    };
    write_policy(&old_pin)?;
    let first = refresh(&base)?;
    assert_eq!(refresh(&first)?, first);
    write_config("aarch64")?;
    let error = create_configured_image(&f.root, &config).unwrap_err();
    assert!(format!("{error:#}").contains("architecture, facets or variants differ"));
    assert!(!f.root.exists());
    write_config("x86_64")?;
    create_configured_image(&f.root, &config)?;
    let error = crate::prefer_publisher(&f.root, "seed").unwrap_err();
    assert!(format!("{error:#}").contains("fixes this image's publishers"));
    let available = list_packages(&f.root, Scope::Available)?;
    assert!(
        available
            .iter()
            .any(|entry| entry.id == "cps://seed/library/runtime@1.0.0-0")
    );
    assert!(package_manifest(&f.root, "cps://seed/library/runtime@2.0.0-0", true).is_err());
    let saved = plan(&f.root, Request::InstallProfile)?;
    assert_eq!(saved.catalogs["seed"], old_pin.trim());
    let mut altered = saved.clone();
    altered
        .catalogs
        .insert("seed".to_owned(), new_pin.trim().to_owned());
    let error = apply(&f.root, &altered).unwrap_err();
    assert!(format!("{error:#}").contains("differs from the signed release profile"));
    assert!(!f.root.join(payload).exists());

    write_policy(&digest(b"missing companion"))?;
    let error = refresh(&first).unwrap_err();
    assert!(format!("{error:#}").contains("reading companion catalog"));
    assert_eq!(fs::read_to_string(&pointer)?.trim(), first);
    write_policy(&new_pin)?;
    let error = refresh(&base).unwrap_err();
    assert!(format!("{error:#}").contains("release base is stale"));
    let second = refresh(&first)?;
    assert_ne!(first, second);
    for (pin, expected) in [
        (&old_pin, "publisher catalog rollback or fork"),
        (&equal_fork, "publisher catalog rollback or fork"),
        (
            &newer_fork,
            "release drops or changes the published manifest",
        ),
    ] {
        write_policy(pin)?;
        let error = refresh(&second).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(fs::read_to_string(&pointer)?.trim(), second);
    }
    let rotated = Fixture::new()?;
    let rotated_pin = rotate_publisher_key(
        &seed.repo,
        "seed",
        new_pin.trim(),
        &seed.private,
        &rotated.private,
    )?;
    write_policy(&rotated_pin)?;
    let third = refresh(&second)?;
    assert_ne!(second, third);
    fs::remove_file(seed.repo.join("current"))?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read_to_string(f.root.join(payload))?, "old");
    let upgrade = plan(&f.root, Request::Update)?;
    assert_eq!(upgrade.catalogs["seed"], rotated_pin);
    let staged = f._dir.path().join("staged");
    crate::stage_boot_environment(&f.root, &staged, &upgrade)?;
    assert_eq!(fs::read_to_string(staged.join(payload))?, "new");
    assert_eq!(fs::read_to_string(f.root.join(payload))?, "old");
    Ok(())
}

fn sign_release(
    fixture: &Fixture,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
) -> Result<()> {
    sign_publisher_release(fixture, mirror, version, expires, root, tuf_key, "canopa")
}

fn sign_publisher_release(
    fixture: &Fixture,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
    publisher: &str,
) -> Result<()> {
    fs::create_dir(mirror)?;
    let export = mirror.join("export");
    mirror_release::export(&fixture.repo, publisher, &fixture.public, &export)?;
    sign_export(&export, mirror, version, expires, root, tuf_key)
}

fn sign_export(
    export: &Path,
    mirror: &Path,
    version: u64,
    expires: Timestamp,
    root: &Path,
    tuf_key: &Path,
) -> Result<()> {
    tuf::sign_export(
        export,
        mirror,
        version,
        expires,
        root,
        tuf_key,
        PathExists::Fail,
    )
}

fn signed_compressed_payload(export: &Path, mirror: &Path, hash: &str) -> Result<PathBuf> {
    let name = format!("blobs-{hash}.zst");
    let compressed_hash = crate::model::digest_reader(fs::File::open(export.join(&name))?)?;
    Ok(mirror
        .join("targets")
        .join(format!("{compressed_hash}.{name}")))
}

#[test]
fn one_build_publishes_several_packages_under_one_attestation() -> Result<()> {
    use crate::{build::expand, kdl_format::manifest_text};

    let f = Fixture::new()?;
    let builder = Fixture::new()?;
    let work = f._dir.path();
    let compiler_template = br#"schema-version 1 kind="package-manifest"
package "devel/compiler" publisher="canopa" license="GPL-3.0-or-later WITH GCC-exception-3.1" homepage="https://gcc.gnu.org/" {
    require "runtime/libcc" version="={version}"
    collect "usr/bin/*"
    collect "usr/include/**"
}
"#;
    let runtime_template = br#"schema-version 1 kind="package-manifest"
package "runtime/libcc" publisher="canopa" {
    file "usr/lib/libcc.so.1" mode="0755" uid=0 gid=0
}
"#;
    let catalog = digest(b"pinned toolchain");
    let key_hash = digest(&read_key(&f.public)?);
    let recipe = format!(
        "schema-version 1 kind=\"build-recipe\"\nbuild architecture=\"x86_64\" epoch=1700000000 {{\n    toolchain solver=\"sat-v2\" {{ publisher \"seed\" catalog=\"{catalog}\" key-sha256=\"{key_hash}\"; package \"build/tool\"; }}\n    version \"2.1.0-1\" upstream=\"2.1\"\n    manifest \"compiler.kdl\" sha256=\"{}\"\n    manifest \"runtime.kdl\" sha256=\"{}\"\n    discard \"usr/share/info/**\"\n    script \"true\"\n}}\n",
        digest(compiler_template),
        digest(runtime_template),
    );
    let reviewed = digest(recipe.as_bytes());
    let parsed = build::parse(&recipe)?;
    let templates = [compiler_template.as_slice(), runtime_template]
        .into_iter()
        .map(|text| build::parse_template(std::str::from_utf8(text).unwrap(), &parsed))
        .collect::<Result<Vec<_>>>()?;
    let file = |inode, executable| expand::Entry::File {
        executable,
        inode: (1, inode),
    };
    let tree = expand::Tree::from([
        ("usr/bin/cc".into(), file(1, true)),
        ("usr/bin/c++".into(), file(1, true)),
        ("usr/include/cc.h".into(), file(2, false)),
        ("usr/lib/libcc.so.1".into(), file(3, true)),
        ("usr/share/info/cc.info".into(), file(4, false)),
    ]);
    let manifests = expand::expand(&templates, &parsed.discard, &tree)?;
    let payloads: [(&str, &[u8]); 3] = [
        ("usr/bin/c++", b"compiler"),
        ("usr/include/cc.h", b"header"),
        ("usr/lib/libcc.so.1", b"runtime"),
    ];
    let bundle = work.join("bundle");
    let mut artifacts = BTreeMap::new();
    for (path, bytes) in payloads {
        let target = bundle.join("proto").join(path);
        fs::create_dir_all(target.parent().unwrap())?;
        fs::write(target, bytes)?;
        artifacts.insert(
            path.to_owned(),
            FileDigest {
                sha256: digest(bytes),
                size: bytes.len() as u64,
            },
        );
    }
    fs::write(bundle.join("build.kdl"), &recipe)?;
    fs::create_dir_all(bundle.join("templates"))?;
    fs::write(bundle.join("templates/compiler.kdl"), compiler_template)?;
    fs::write(bundle.join("templates/runtime.kdl"), runtime_template)?;
    for manifest in &manifests {
        let path = bundle.join(crate::build::expanded_manifest_path(&manifest.name));
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, manifest_text(manifest))?;
    }
    let envelope = attestation::sign(
        &read_signing_key(&builder.private)?,
        &manifests,
        &parsed,
        recipe.as_bytes(),
        Toolchain {
            publishers: parsed.toolchain.clone(),
            package_manifests: BTreeMap::from([(
                "build/tool".into(),
                PinnedPackage {
                    publisher: "seed".into(),
                    version: "1.0.0-0".parse()?,
                    sha256: digest(b"tool"),
                },
            )]),
        },
        &artifacts,
    )?;
    fs::write(bundle.join("attestation.json"), &envelope)?;
    let compiler = "cps://canopa/devel/compiler@2.1.0-1";
    let runtime = "cps://canopa/runtime/libcc@2.1.0-1";

    // An expanded manifest must be what its pinned template can produce.
    let tampered = work.join("tampered");
    copy_tree(&bundle, &tampered)?;
    let mut changed = manifests[0].clone();
    changed.dependency.clear();
    fs::write(
        tampered.join("manifests/devel/compiler.kdl"),
        manifest_text(&changed),
    )?;
    let error =
        publish_bundle(&f.repo, &tampered, &f.private, &builder.public, &reviewed).unwrap_err();
    assert!(
        format!("{error:#}").contains("differs from its template"),
        "{error:#}"
    );

    fs::create_dir(&f.repo)?;
    let policy = |approved: &[&str]| {
        let approvals: String = approved
            .iter()
            .map(|name| {
                format!("    approve \"{name}\" version=\"2.1.0-1\" sha256=\"{reviewed}\"\n")
            })
            .collect();
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                "schema-version 1 kind=\"publish-policy\"\npublisher \"canopa\" archive-build-inputs=#true {{\n    builder-key {:?}\n{approvals}}}\n",
                builder.public
            ),
        )
    };
    policy(&["devel/compiler"])?;
    let error =
        publish_bundle(&f.repo, &bundle, &f.private, &builder.public, &reviewed).unwrap_err();
    assert!(format!("{error:#}").contains("not approved for cps://canopa/runtime/libcc"));
    assert!(!f.repo.join("current").exists());
    policy(&["devel/compiler", "runtime/libcc"])?;
    let ids = publish_bundle(&f.repo, &bundle, &f.private, &builder.public, &reviewed)?;
    assert_eq!(ids, [compiler, runtime]);
    assert_eq!(
        read_attestation(&f.repo, "canopa", &f.public, compiler)?,
        read_attestation(&f.repo, "canopa", &f.public, runtime)?
    );

    // Clients verify each package against the shared statement.
    let release = snapshot(&f.repo, "canopa", &read_key(&f.public)?, None)?;
    let trust = attestation::BuildTrust::load(1, std::slice::from_ref(&builder.public))?;
    for package in &release.packages {
        release.builds.verify(&f.repo, [&trust], package)?;
    }
    let installed = release
        .packages
        .iter()
        .find(|package| package.manifest.name == "devel/compiler")
        .unwrap();
    assert_eq!(
        installed.manifest.license.as_deref(),
        Some("GPL-3.0-or-later WITH GCC-exception-3.1")
    );
    assert_eq!(installed.manifest.upstream_version.as_deref(), Some("2.1"));
    assert!(installed.manifest.action.iter().any(|action| matches!(
        action,
        crate::model::Action::Hardlink { path, target, .. } if path == "usr/bin/cc" && target == "usr/bin/c++"
    )));

    // A rebuild of either package proves both came from the admitted build.
    for id in [compiler, runtime] {
        assert_eq!(
            crate::verify_rebuild(&f.repo, "canopa", &f.public, id, None, &bundle, None)?,
            3
        );
    }
    let error = crate::verify_rebuild(&f.repo, "canopa", &f.public, runtime, None, &tampered, None)
        .unwrap_err();
    assert!(format!("{error:#}").contains("differs from its template"));
    let partial = work.join("partial");
    copy_tree(&bundle, &partial)?;
    fs::remove_file(partial.join("proto/usr/lib/libcc.so.1"))?;
    assert!(
        crate::verify_rebuild(&f.repo, "canopa", &f.public, compiler, None, &partial, None)
            .is_err()
    );

    let inputs = work.join("inputs");
    let path = crate::export_rebuild_inputs(&f.repo, "canopa", &f.public, runtime, None, &inputs)?;
    assert_eq!(fs::read(path)?, recipe.as_bytes());
    assert_eq!(
        fs::read(inputs.join("source/compiler.kdl"))?,
        compiler_template
    );
    assert_eq!(
        fs::read(inputs.join("source/runtime.kdl"))?,
        runtime_template
    );
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[test]
fn rebuild_inputs_survive_withdrawal_and_are_verified_in_fresh_mirror_caches() -> Result<()> {
    let f = Fixture::new()?;
    let builder = Fixture::new()?;
    let work = f._dir.path();
    let id = "cps://canopa/test/rebuild@1.0.0-0";
    let output = "usr/share/cps/reference";
    let bytes = b"deterministic build output";
    let source_bytes = b"pinned source bytes";
    let source_hash = digest(source_bytes);
    let manifest = br#"schema-version 1 kind="package-manifest"
// Reviewed bytes and layout
package "test/rebuild" publisher="canopa" version="1.0.0-0" {
    file "usr/share/cps/reference" mode="0644" uid=0 gid=0
}
"#;
    let catalog = digest(b"pinned toolchain");
    let key_hash = digest(&read_key(&f.public)?);
    let recipe = format!(
        "schema-version 1 kind=\"build-recipe\"\nbuild architecture=\"x86_64\" epoch=1700000000 {{\n    toolchain solver=\"sat-v2\" {{ publisher \"seed\" catalog=\"{catalog}\" key-sha256=\"{key_hash}\"; package \"build/tool\"; }}\n    manifest \"build.kdl/nested/manifest.kdl\" sha256=\"{}\"\n    source \"input\" sha256=\"{source_hash}\" size={}\n    script \"true\"\n}}\n",
        digest(manifest),
        source_bytes.len(),
    );
    let reviewed = digest(recipe.as_bytes());
    let bundle = work.join("bundle");
    fs::create_dir_all(bundle.join("proto/usr/share/cps"))?;
    fs::write(bundle.join("proto").join(output), bytes)?;
    fs::write(bundle.join("build.kdl"), &recipe)?;
    crate::build::write_explicit_outputs(&bundle, &[("build.kdl/nested/manifest.kdl", manifest)])?;
    let envelope = attestation::sign(
        &read_signing_key(&builder.private)?,
        &[parse_manifest(std::str::from_utf8(manifest)?)?],
        &build::parse(&recipe)?,
        recipe.as_bytes(),
        Toolchain {
            publishers: build::parse(&recipe)?.toolchain,
            package_manifests: BTreeMap::from([(
                "build/tool".into(),
                PinnedPackage {
                    publisher: "seed".into(),
                    version: "1.0.0-0".parse()?,
                    sha256: digest(b"tool"),
                },
            )]),
        },
        &BTreeMap::from([(
            output.into(),
            FileDigest {
                sha256: digest(bytes),
                size: bytes.len() as u64,
            },
        )]),
    )?;
    fs::write(bundle.join("attestation.json"), &envelope)?;
    fs::create_dir(&f.repo)?;
    let policy = |archive: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" archive-build-inputs={archive} {{
    builder-key {:?}
    approve "test/rebuild" version="1.0.0-0" sha256="{reviewed}"
}}
"#,
                builder.public
            ),
        )
    };
    let publish = || publish_bundle(&f.repo, &bundle, &f.private, &builder.public, &reviewed);
    let extract = |repo: &Path, catalog, output: &Path| {
        crate::export_rebuild_inputs(repo, "canopa", &f.public, id, catalog, output)
    };
    let compare = |repo: &Path, catalog| {
        crate::verify_rebuild(repo, "canopa", &f.public, id, catalog, &bundle, None)
    };
    policy("#false")?;
    publish()?;
    let no_archive = work.join("no-archive");
    let error = extract(&f.repo, None, &no_archive).unwrap_err();
    assert!(format!("{error:#}").contains("no archived rebuild inputs"));
    assert!(!no_archive.exists());
    assert_eq!(compare(&f.repo, None)?, 1);
    policy("#true")?;
    publish()?;
    let archived = fs::read_to_string(f.repo.join("current"))?;
    publish()?;
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, archived);
    policy("#false")?;
    let error = publish().unwrap_err();
    assert!(
        format!("{error:#}").contains("protected repository requires archiving rebuild inputs")
    );
    policy("#true")?;
    let check = |repo: &Path, catalog, output: &Path| -> Result<()> {
        let path = extract(repo, catalog, output)?;
        assert_eq!(path, output.join("source/rebuild.kdl"));
        assert_eq!(fs::read(path)?, recipe.as_bytes());
        assert_eq!(
            fs::read(output.join("source/build.kdl/nested/manifest.kdl"))?,
            manifest
        );
        assert_eq!(fs::read(output.join("attestation.json"))?, envelope);
        assert!(extract(repo, catalog, output).is_err());
        Ok(())
    };
    check(&f.repo, None, &work.join("local"))?;

    withdraw_package(&f.repo, &f.private, id)?;
    let source_cache = work.join("publisher-sources");
    fs::create_dir_all(source_cache.join("sha256"))?;
    let input = source_cache.join("sha256").join(&source_hash);
    let current = fs::read_to_string(f.repo.join("current"))?;
    fs::write(&input, vec![b'x'; source_bytes.len()])?;
    let archive_sources = || crate::archive_build_sources(&f.repo, &f.private, &source_cache, id);
    let error = archive_sources().unwrap_err();
    assert!(format!("{error:#}").contains("source digest or length mismatch"));
    assert_eq!(fs::read_to_string(f.repo.join("current"))?, current);
    fs::write(&input, source_bytes)?;
    let (with_sources, count) = archive_sources()?;
    assert_eq!(count, 1);
    assert_ne!(with_sources, current.trim());
    assert_eq!(archive_sources()?.0, with_sources);
    fs::remove_file(input)?;
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf.der");
    sign_root(&anchor, &tuf_key, None)?;
    let mirror = work.join("mirror");
    fs::create_dir(&mirror)?;
    let export = mirror.join("export");
    mirror_release::export_with_retained(
        &f.repo,
        "canopa",
        &f.public,
        &export,
        &[archived.trim().into()],
    )?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_export(&export, &mirror, 1, fresh, &anchor, &tuf_key)?;
    let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
    let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
    let cache = work.join("cache");
    let sync = || {
        mirror_release::sync_index(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    assert_eq!(sync()?, fs::read_to_string(f.repo.join("current"))?.trim());
    let corrupted = cache.join("build-inputs").join(&reviewed);
    fs::write(&corrupted, vec![b'x'; recipe.len()])?;
    let corrupt = work.join("corrupt");
    let error = extract(&cache, None, &corrupt).unwrap_err();
    assert!(format!("{error:#}").contains("digest mismatch for repository object build-inputs"));
    assert!(!corrupt.exists());
    sync()?;
    let current_only = work.join("current-only");
    sign_release(&f, &current_only, 1, fresh, &anchor, &tuf_key)?;
    let withdrawn_cache = work.join("withdrawn-cache");
    let metadata = Url::from_directory_path(current_only.join("metadata")).unwrap();
    let targets = Url::from_directory_path(current_only.join("targets")).unwrap();
    mirror_release::sync_index(
        &withdrawn_cache,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "canopa",
        &f.public,
    )?;
    let rebuild_cache = work.join("rebuild-cache");
    let fetch = |repo: &Path, pin| {
        crate::fetch_build_sources(repo, "canopa", &f.public, id, pin, &rebuild_cache)
    };
    let error = fetch(&withdrawn_cache, None).unwrap_err();
    assert!(format!("{error:#}").contains("checking archived source"));
    let sync_sources = |mirror: &Path, version, corrupted: bool| -> Result<()> {
        fs::create_dir(mirror)?;
        let export = mirror.join("export");
        mirror_release::export_with_sources(&f.repo, "canopa", &f.public, &export, &[])?;
        let target = export.join(format!("source-archives-{source_hash}"));
        assert_eq!(fs::read(&target)?, source_bytes);
        if corrupted {
            fs::write(&target, vec![b'x'; source_bytes.len()])?;
        }
        sign_export(&export, mirror, version, fresh, &anchor, &tuf_key)?;
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync_index(
            &withdrawn_cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )?;
        Ok(())
    };
    sync_sources(&work.join("wrong-sources"), 2, true)?;
    let error = fetch(&withdrawn_cache, None).unwrap_err();
    assert!(format!("{error:#}").contains("signed digest mismatch for source-archives"));
    assert!(!rebuild_cache.join("sha256").join(&source_hash).exists());
    let source_mirror = work.join("source-mirror");
    sync_sources(&source_mirror, 3, false)?;
    let target = source_mirror
        .join("targets")
        .join(format!("{source_hash}.source-archives-{source_hash}"));
    fs::write(&target, vec![b'x'; source_bytes.len()])?;
    let error = fetch(&withdrawn_cache, None).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"));
    assert!(!rebuild_cache.join("sha256").join(&source_hash).exists());
    fs::write(&target, source_bytes)?;
    assert_eq!(fetch(&withdrawn_cache, Some(&with_sources))?, 1);
    assert_eq!(
        fs::read(rebuild_cache.join("sha256").join(&source_hash))?,
        source_bytes
    );
    assert!(!withdrawn_cache.join("source-archives").exists());
    fs::rename(&source_mirror, work.join("offline-source-mirror"))?;
    assert_eq!(fetch(&withdrawn_cache, None)?, 1);

    let protected = work.join("protected-repo");
    fs::create_dir(&protected)?;
    let policy_path = protected.join("publish-policy.kdl");
    let policy = fs::read_to_string(f.repo.join("publish-policy.kdl"))?;
    fs::write(
        &policy_path,
        policy.replace(
            "archive-build-inputs=#true",
            "archive-build-inputs=#true require-source-archives=#true",
        ),
    )?;
    let release = work.join("protected-release.kdl");
    fs::write(
        &release,
        format!(
            "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base=\"empty\" {{\n\
         bundle {id:?} path=\"bundle\" builder-key=\"builder.der\" recipe-sha256={reviewed:?}\n}}\n"
        ),
    )?;
    fs::copy(&builder.public, work.join("builder.der"))?;
    let publish =
        |cache| crate::publish_release_with_sources(&protected, &f.private, &release, cache);
    let error = publish(None).unwrap_err();
    assert!(format!("{error:#}").contains("provide --source-cache to archive source"));
    assert!(!protected.join("current").exists());
    let admitted = publish(Some(&rebuild_cache))?;
    assert_eq!(fetch(&protected, Some(&admitted))?, 1);
    fs::write(&policy_path, policy)?;
    let error =
        publish_bundle(&protected, &bundle, &f.private, &builder.public, &reviewed).unwrap_err();
    assert!(format!("{error:#}").contains("protected repository requires source archives"));
    withdraw_package(&protected, &f.private, id)?;

    let complete_export = work.join("protected-export");
    let expected = mirror_release::export(&protected, "canopa", &f.public, &complete_export)?;
    let source_target = complete_export.join(format!("source-archives-{source_hash}"));
    assert_eq!(fs::read(&source_target)?, source_bytes);
    let protected_cache = work.join("protected-cache");
    let sync_protected = |mirror: &Path| {
        mirror_release::sync_index(
            &protected_cache,
            &anchor,
            Url::from_directory_path(mirror.join("metadata"))
                .unwrap()
                .as_str(),
            Url::from_directory_path(mirror.join("targets"))
                .unwrap()
                .as_str(),
            "canopa",
            &f.public,
        )
    };
    fs::remove_file(&source_target)?;
    let incomplete = work.join("incomplete-protected-mirror");
    sign_export(&complete_export, &incomplete, 1, fresh, &anchor, &tuf_key)?;
    let error = sync_protected(&incomplete).unwrap_err();
    assert!(format!("{error:#}").contains("checking required archived source"));
    assert!(!protected_cache.join("current").exists());
    fs::write(&source_target, vec![b'x'; source_bytes.len()])?;
    let wrong = work.join("wrong-protected-mirror");
    sign_export(&complete_export, &wrong, 2, fresh, &anchor, &tuf_key)?;
    let error = sync_protected(&wrong).unwrap_err();
    assert!(format!("{error:#}").contains("signed digest mismatch for source-archives"));
    assert!(!protected_cache.join("current").exists());
    fs::write(&source_target, source_bytes)?;
    let complete = work.join("protected-mirror");
    sign_export(&complete_export, &complete, 3, fresh, &anchor, &tuf_key)?;
    assert_eq!(sync_protected(&complete)?, expected);
    let new_source_cache = work.join("new-source-cache");
    assert_eq!(
        crate::fetch_build_sources(
            &protected_cache,
            "canopa",
            &f.public,
            id,
            None,
            &new_source_cache
        )?,
        1
    );
    assert_eq!(
        fs::read(new_source_cache.join("sha256").join(&source_hash))?,
        source_bytes
    );

    let archive_targets = work.join("protected-archive-targets");
    mirror_release::export_archive(
        &protected,
        "canopa",
        &f.public,
        &archive_targets,
        &admitted,
        false,
    )?;
    let archive = work.join("protected-archive-mirror");
    sign_export(&archive_targets, &archive, 1, fresh, &anchor, &tuf_key)?;
    let archive_cache = work.join("protected-archive-cache");
    assert_eq!(
        mirror_release::sync_archive(
            &archive_cache,
            &anchor,
            Url::from_directory_path(archive.join("metadata"))
                .unwrap()
                .as_str(),
            Url::from_directory_path(archive.join("targets"))
                .unwrap()
                .as_str(),
            "canopa",
            &f.public,
            mirror_release::ArchiveSyncOptions {
                catalog: &admitted,
                index_only: true
            },
        )?,
        admitted
    );
    check(
        &archive_cache,
        Some(&admitted),
        &work.join("archive-inputs"),
    )?;
    let archive_sources = work.join("archive-sources");
    assert_eq!(
        crate::fetch_build_sources(
            &archive_cache,
            "canopa",
            &f.public,
            id,
            Some(&admitted),
            &archive_sources
        )?,
        1
    );
    assert_eq!(
        fs::read(archive_sources.join("sha256").join(&source_hash))?,
        source_bytes
    );
    fs::remove_file(protected_cache.join("build-inputs").join(&reviewed))?;
    let publisher = crate::image::Publisher {
        name: "canopa".into(),
        repo: protected_cache.clone(),
        key: read_key(&f.public)?,
        build_trust: None,
        git: None,
    };
    mirror_release::ArchiveCache::open(&archive_cache, &publisher)?.restore_metadata(&publisher)?;
    check(
        &protected_cache,
        Some(&admitted),
        &work.join("restored-archive-inputs"),
    )?;
    assert_eq!(
        fs::read_to_string(protected_cache.join("current"))?.trim(),
        expected
    );

    fs::rename(&f.repo, work.join("offline-repo"))?;
    fs::rename(&mirror, work.join("offline-mirror"))?;
    fs::rename(&current_only, work.join("offline-current-only"))?;
    check(&withdrawn_cache, None, &work.join("withdrawn"))?;
    check(&cache, Some(archived.trim()), &work.join("historical"))?;
    fs::remove_file(bundle.join("attestation.json"))?;
    assert_eq!(fs::read_dir(withdrawn_cache.join("blobs"))?.count(), 0);
    assert_eq!(compare(&withdrawn_cache, None)?, 1);
    assert_eq!(compare(&cache, Some(archived.trim()))?, 1);
    Ok(())
}

#[test]
fn tuf_retained_catalogs_bootstrap_profiled_images_without_rewinding_current() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let payload = "usr/lib/runtime.marker";
    seed.package_as(
        "seed",
        "library/runtime",
        "1.0.0-0",
        seed.file(payload, "old", "0644", "")?,
    )?;
    let previous = fs::read_to_string(seed.repo.join("current"))?;
    seed.package_as(
        "seed",
        "library/runtime",
        "2.0.0-0",
        seed.file(payload, "new", "0644", "")?,
    )?;
    withdraw_package(
        &seed.repo,
        &seed.private,
        "cps://seed/library/runtime@1.0.0-0",
    )?;
    let current = fs::read_to_string(seed.repo.join("current"))?;

    let work = f._dir.path();
    let mirror = work.join("mirror");
    fs::create_dir(&mirror)?;
    let export = mirror.join("export");
    let invalid = mirror_release::export_with_retained(
        &seed.repo,
        "seed",
        &seed.public,
        &export,
        &[current.trim().to_owned()],
    )
    .unwrap_err();
    assert!(
        invalid
            .to_string()
            .contains("current catalog cannot also be retained")
    );
    assert!(!export.exists());
    mirror_release::export_with_retained(
        &seed.repo,
        "seed",
        &seed.public,
        &export,
        &[previous.trim().to_owned()],
    )?;
    assert_eq!(fs::read_to_string(export.join("retained"))?, previous);
    assert_eq!(fs::read_to_string(export.join("current"))?, current);

    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_export(&export, &mirror, 1, fresh, &anchor, &tuf_key)?;
    let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
    let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
    let seed_cache = work.join("seed-cache");
    let hash = mirror_release::sync_index(
        &seed_cache,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "seed",
        &seed.public,
    )?;
    assert_eq!(hash, current.trim());
    assert_eq!(
        fs::read_to_string(seed_cache.join(".tuf/catalog"))?,
        current
    );
    let key = read_key(&seed.public)?;
    let now = snapshot(&seed_cache, "seed", &key, None)?;
    assert_eq!(now.packages.len(), 1);
    assert_eq!(
        now.packages[0].manifest.id(),
        "cps://seed/library/runtime@2.0.0-0"
    );
    let blob = format!("blobs/{}", digest(b"old"));
    assert!(!seed_cache.join(&blob).exists());

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "compositor", "0755", "")? + "    require \"library/runtime\"\n",
    )?;
    f.package("test/retire", "1.0.0-0", String::new())?;
    fs::write(
        f.repo.join("publish-policy.kdl"),
        format!(
            r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/compositor"
        facet "devel" #false
        publisher "seed" catalog={:?} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
            previous.trim(),
            seed.repo,
            seed.public
        ),
    )?;
    withdraw_package(&f.repo, &f.private, "cps://canopa/test/retire@1.0.0-0")?;
    let first = fs::read_to_string(f.repo.join("current"))?;
    let primary_root = work.join("primary-root.json");
    let primary_key = work.join("primary-tuf-key.der");
    sign_root(&primary_root, &primary_key, None)?;
    let primary_mirror = work.join("primary-mirror");
    sign_release(&f, &primary_mirror, 1, fresh, &primary_root, &primary_key)?;
    let primary_cache = work.join("primary-cache");
    let config = work.join("image.kdl");
    let write_config = |primary: &Path, seed_mirror: &Path| -> Result<()> {
        let endpoint = |mirror: &Path, directory| {
            Url::from_directory_path(mirror.join(directory))
                .unwrap()
                .to_string()
        };
        fs::write(
            &config,
            format!(
                r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
facet "devel" #false
publisher "canopa" {{
    repository {:?}
    trust-key {:?}
}}
mirror "canopa" root={primary_root:?} metadata={:?} targets={:?}
publisher "seed" {{
    repository {:?}
    trust-key {:?}
}}
mirror "seed" root={anchor:?} metadata={:?} targets={:?}
"#,
                primary_cache,
                f.public,
                endpoint(primary, "metadata"),
                endpoint(primary, "targets"),
                seed_cache,
                seed.public,
                endpoint(seed_mirror, "metadata"),
                endpoint(seed_mirror, "targets"),
            ),
        )?;
        Ok(())
    };
    write_config(&primary_mirror, &mirror)?;
    assert_eq!(mirror_release::sync_profile(&config, true)?, first.trim());
    create_configured_image(&f.root, &config)?;
    let saved = plan(&f.root, Request::InstallProfile)?;
    assert_eq!(saved.catalogs["seed"], previous.trim());
    fetch_plan(&f.root, &saved)?;
    assert_eq!(fs::read(seed_cache.join(&blob))?, b"old");
    let prune_seed = |cache: &Path, images: &[PathBuf], apply| {
        mirror_release::prune_payloads(
            cache,
            "seed",
            &seed.public,
            mirror_release::CacheRetention {
                catalogs: &[],
                images,
                plans: &[],
            },
            apply,
        )
    };
    assert!(prune_seed(&seed_cache, &[], false)?.blobs.is_empty());

    let full = work.join("full-cache");
    mirror_release::sync(
        &full,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "seed",
        &seed.public,
    )?;
    assert_eq!(fs::read_to_string(full.join("current"))?, current);
    assert_eq!(fs::read(full.join(&blob))?, b"old");

    // The mirror changes its retention list without a new current catalog.
    // The published profile still needs the original companion pin.
    let unretained_mirror = work.join("unretained-mirror");
    sign_publisher_release(
        &seed,
        &unretained_mirror,
        2,
        fresh,
        &anchor,
        &tuf_key,
        "seed",
    )?;
    mirror_release::sync_index(
        &seed_cache,
        &anchor,
        Url::from_directory_path(unretained_mirror.join("metadata"))
            .unwrap()
            .as_str(),
        Url::from_directory_path(unretained_mirror.join("targets"))
            .unwrap()
            .as_str(),
        "seed",
        &seed.public,
    )?;
    let orphan = digest(b"unreferenced");
    let orphan_path = seed_cache.join("blobs").join(&orphan);
    fs::write(&orphan_path, b"unreferenced")?;
    assert_eq!(prune_seed(&seed_cache, &[], true)?.blobs, [(orphan, 12)]);
    assert!(!orphan_path.exists());
    assert_eq!(fs::read(seed_cache.join(&blob))?, b"old");

    seed.package_as(
        "seed",
        "library/runtime",
        "3.0.0-0",
        seed.file(payload, "latest", "0644", "")?
            + &seed.file(
                "usr/include/runtime.h",
                "disabled header",
                "0644",
                "facet=\"devel\"",
            )?,
    )?;
    seed.package_as(
        "seed",
        "library/unrelated",
        "1.0.0-0",
        seed.file("usr/lib/unrelated.marker", "unrelated package", "0644", "")?,
    )?;
    let latest = fs::read_to_string(seed.repo.join("current"))?;
    let policy = f.repo.join("publish-policy.kdl");
    fs::write(
        &policy,
        fs::read_to_string(&policy)?.replace(previous.trim(), latest.trim()),
    )?;
    let refresh = work.join("refresh.kdl");
    fs::write(
        &refresh,
        format!(
            "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
            first.trim()
        ),
    )?;
    let second = publish_release(&f.repo, &f.private, &refresh)?;
    let next_primary_mirror = work.join("next-primary-mirror");
    sign_release(
        &f,
        &next_primary_mirror,
        2,
        fresh,
        &primary_root,
        &primary_key,
    )?;
    write_config(&next_primary_mirror, &unretained_mirror)?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!(
            "mirror for seed does not advertise required catalog {}",
            latest.trim()
        )),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, first);
    assert_eq!(
        fs::read_to_string(primary_cache.join(".tuf/catalog"))?.trim(),
        second
    );
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, current);
    assert_eq!(plan(&f.root, Request::InstallProfile)?, saved);
    let error = mirror_release::sync_index(
        &primary_cache,
        &primary_root,
        Url::from_directory_path(next_primary_mirror.join("metadata"))
            .unwrap()
            .as_str(),
        Url::from_directory_path(next_primary_mirror.join("targets"))
            .unwrap()
            .as_str(),
        "canopa",
        &f.public,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("managed by repo sync-profile"));

    let next_seed_mirror = work.join("next-seed-mirror");
    sign_publisher_release(
        &seed,
        &next_seed_mirror,
        3,
        fresh,
        &anchor,
        &tuf_key,
        "seed",
    )?;
    write_config(&next_primary_mirror, &next_seed_mirror)?;
    let correct_config = fs::read_to_string(&config)?;
    fs::write(
        &config,
        correct_config.replace(
            seed_cache.to_str().unwrap(),
            work.join("different-seed-cache").to_str().unwrap(),
        ),
    )?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("bound to a different release profile or companion cache set")
    );
    let builder = Fixture::new()?;
    let seed_key = format!("    trust-key {:?}", seed.public);
    fs::write(
        &config,
        correct_config.replace(
            &seed_key,
            &format!(
                "{seed_key}\n    build-trust {{ builder-key {:?}; }}",
                builder.public
            ),
        ),
    )?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}")
            .contains("no build attestation for cps://seed/library/runtime@3.0.0-0"),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, first);
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, current);
    assert_eq!(plan(&f.root, Request::InstallProfile)?, saved);
    fs::write(&config, correct_config)?;

    let prefetch = |config: &Path| {
        mirror_release::sync_profile_with_payloads(
            config,
            mirror_release::ProfilePayloads::Composition,
            None,
        )
    };
    let latest_blob = digest(b"latest");
    let latest_target = next_seed_mirror
        .join("targets")
        .join(format!("{latest_blob}.blobs-{latest_blob}"));
    let saved_target = work.join("latest-target");
    fs::rename(&latest_target, &saved_target)?;
    let error = prefetch(&config).unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!("reading target blobs/{latest_blob}")),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, first);
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, current);
    fs::write(&latest_target, b"broken")?;
    let error = prefetch(&config).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"), "{error:#}");
    assert!(!seed_cache.join("blobs").join(&latest_blob).exists());
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, first);
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, current);
    fs::rename(&saved_target, &latest_target)?;

    // The companion can promote before a write failure leaves the previous
    // primary visible. Pruning must protect the old pin through this window.
    let endpoints = primary_cache.join(".tuf/endpoints");
    let saved_endpoints = work.join("previous-endpoints");
    fs::rename(&endpoints, &saved_endpoints)?;
    fs::create_dir(&endpoints)?;
    let error = prefetch(&config).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<tempfile::PersistError>()
            .unwrap()
            .error
            .kind(),
        std::io::ErrorKind::IsADirectory
    );
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, first);
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, latest);
    assert!(prune_seed(&seed_cache, &[], true)?.blobs.is_empty());
    assert_eq!(plan(&f.root, Request::InstallProfile)?, saved);
    fs::remove_dir(&endpoints)?;
    fs::rename(&saved_endpoints, &endpoints)?;
    assert_eq!(prefetch(&config)?, second);
    assert_eq!(
        fs::read_to_string(primary_cache.join("current"))?.trim(),
        second
    );
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, latest);
    let fresh_root = work.join("fresh-image");
    create_configured_image(&fresh_root, &config)?;
    let new_plan = plan(&fresh_root, Request::InstallProfile)?;
    assert_eq!(new_plan.catalogs["seed"], latest.trim());
    assert_eq!(
        fs::read(seed_cache.join("blobs").join(&latest_blob))?,
        b"latest"
    );
    for bytes in [b"new".as_slice(), b"disabled header", b"unrelated package"] {
        assert!(!seed_cache.join("blobs").join(digest(bytes)).exists());
    }

    let bootstrap_config = work.join("bootstrap.kdl");
    fs::write(
        &bootstrap_config,
        fs::read_to_string(&config)?.replace(
            "release-profile \"desktop\"",
            &format!(
                "release-profile \"desktop\" bootstrap-catalog={:?}",
                first.trim()
            ),
        ),
    )?;
    let error = mirror_release::sync_profile(&bootstrap_config, true).unwrap_err();
    assert!(format!("{error:#}").contains(&format!(
        "mirror for canopa does not advertise required catalog {}",
        first.trim()
    )));
    assert_eq!(
        fs::read_to_string(primary_cache.join("current"))?.trim(),
        second
    );
    let bootstrap_root = work.join("bootstrap-image");
    create_configured_image(&bootstrap_root, &bootstrap_config)?;
    let error = plan(&bootstrap_root, Request::Update).unwrap_err();
    assert!(format!("{error:#}").contains("plan install-profile first"));
    let bootstrap = plan(&bootstrap_root, Request::InstallProfile)?;
    assert_eq!(bootstrap.catalogs, saved.catalogs);
    assert_eq!(
        list_packages(&bootstrap_root, Scope::Available)?
            .into_iter()
            .find(|row| row.name == "library/runtime")
            .unwrap()
            .id,
        "cps://seed/library/runtime@1.0.0-0"
    );
    assert!(
        prune_seed(&seed_cache, &[], false)?
            .blobs
            .contains(&(digest(b"old"), 3))
    );
    assert!(
        prune_seed(&seed_cache, std::slice::from_ref(&bootstrap_root), false)?
            .blobs
            .is_empty()
    );
    let mut wrong_bootstrap = bootstrap.clone();
    wrong_bootstrap.catalogs = new_plan.catalogs.clone();
    let error = apply(&bootstrap_root, &wrong_bootstrap).unwrap_err();
    assert!(format!("{error:#}").contains("differs from the signed release profile"));
    fetch_plan(&bootstrap_root, &bootstrap)?;
    assert_eq!(apply(&bootstrap_root, &bootstrap)?, 1);
    assert_eq!(apply(&bootstrap_root, &bootstrap)?, 1);
    package_manifest(&bootstrap_root, "cps://seed/library/runtime@1.0.0-0", false)?;
    let upgrade_bootstrap = plan(&bootstrap_root, Request::Update)?;
    assert_eq!(upgrade_bootstrap.catalogs, new_plan.catalogs);
    apply(&bootstrap_root, &upgrade_bootstrap)?;
    package_manifest(&bootstrap_root, "cps://seed/library/runtime@3.0.0-0", false)?;
    assert_eq!(
        fs::read_to_string(primary_cache.join("current"))?.trim(),
        second
    );

    let archive_primary = work.join("archive-primary-mirror");
    let archive_seed = work.join("archive-seed-mirror");
    let archive = work.join("archive-primary-export");
    mirror_release::export_with_retained(
        &f.repo,
        "canopa",
        &f.public,
        &archive,
        &[first.trim().to_owned()],
    )?;
    sign_export(
        &archive,
        &archive_primary,
        3,
        fresh,
        &primary_root,
        &primary_key,
    )?;
    let archive = work.join("archive-seed-export");
    mirror_release::export_with_retained(
        &seed.repo,
        "seed",
        &seed.public,
        &archive,
        &[previous.trim().to_owned()],
    )?;
    sign_export(&archive, &archive_seed, 4, fresh, &anchor, &tuf_key)?;

    let cold_primary = work.join("cold-primary-cache");
    let cold_seed = work.join("cold-seed-cache");
    let cold_config = work.join("cold-bootstrap.kdl");
    let configure_cold = |primary: &Path, seed: &Path| -> Result<()> {
        write_config(primary, seed)?;
        fs::write(
            &cold_config,
            fs::read_to_string(&config)?
                .replace(
                    primary_cache.to_str().unwrap(),
                    cold_primary.to_str().unwrap(),
                )
                .replace(seed_cache.to_str().unwrap(), cold_seed.to_str().unwrap())
                .replace(
                    "release-profile \"desktop\"",
                    &format!(
                        "release-profile \"desktop\" bootstrap-catalog={:?}",
                        first.trim()
                    ),
                ),
        )?;
        Ok(())
    };
    for (primary, seed, missing, hash) in [
        (
            &next_primary_mirror,
            &next_seed_mirror,
            "canopa",
            first.trim(),
        ),
        (&archive_primary, &next_seed_mirror, "seed", previous.trim()),
    ] {
        configure_cold(primary, seed)?;
        let error = mirror_release::sync_profile(&cold_config, true).unwrap_err();
        assert!(
            format!("{error:#}").contains(&format!(
                "mirror for {missing} does not advertise required catalog {hash}"
            )),
            "{error:#}"
        );
        assert!(!cold_primary.join("current").exists());
        assert!(!cold_seed.join("current").exists());
    }
    configure_cold(&archive_primary, &archive_seed)?;
    assert_eq!(prefetch(&cold_config)?, second);
    assert_eq!(
        fs::read_to_string(cold_primary.join("current"))?.trim(),
        second
    );
    assert_eq!(fs::read_to_string(cold_seed.join("current"))?, latest);
    let cold_root = work.join("cold-bootstrap-image");
    create_configured_image(&cold_root, &cold_config)?;
    let cold_plan = plan(&cold_root, Request::InstallProfile)?;
    assert_eq!(cold_plan.catalogs, saved.catalogs);
    assert_eq!(fs::read(cold_seed.join(&blob))?, b"old");
    assert_eq!(
        fs::read(cold_seed.join("blobs").join(&latest_blob))?,
        b"latest"
    );
    assert_eq!(
        fs::read(cold_primary.join("blobs").join(digest(b"compositor")))?,
        b"compositor"
    );
    let unretained_archive = work.join("unretained-archive");
    sign_publisher_release(
        &seed,
        &unretained_archive,
        5,
        fresh,
        &anchor,
        &tuf_key,
        "seed",
    )?;
    mirror_release::sync_index(
        &cold_seed,
        &anchor,
        Url::from_directory_path(unretained_archive.join("metadata"))
            .unwrap()
            .as_str(),
        Url::from_directory_path(unretained_archive.join("targets"))
            .unwrap()
            .as_str(),
        "seed",
        &seed.public,
    )?;
    assert!(prune_seed(&cold_seed, &[], true)?.blobs.is_empty());
    fs::rename(&archive_primary, work.join("offline-archive-primary"))?;
    fs::rename(&archive_seed, work.join("offline-archive-seed"))?;
    fs::rename(&unretained_archive, work.join("offline-unretained-archive"))?;
    apply(&cold_root, &cold_plan)?;
    assert_eq!(fs::read(cold_root.join(payload))?, b"old");

    let broken = crate::repo::sign_profile_with_missing_dependency(
        &f.repo,
        "canopa",
        &f.private,
        "desktop",
        "library/runtime",
    )?;
    let broken_mirror = work.join("broken-primary-mirror");
    sign_release(&f, &broken_mirror, 3, fresh, &primary_root, &primary_key)?;
    write_config(&broken_mirror, &next_seed_mirror)?;
    let error = mirror_release::sync_profile(&config, true).unwrap_err();
    assert!(
        format!("{error:#}").contains("signed profile package library/runtime with manifest"),
        "{error:#}"
    );
    assert_eq!(
        fs::read_to_string(primary_cache.join("current"))?.trim(),
        second
    );
    assert_eq!(
        fs::read_to_string(primary_cache.join(".tuf/catalog"))?.trim(),
        broken
    );

    fs::rename(&mirror, work.join("offline-mirror"))?;
    fs::rename(&unretained_mirror, work.join("offline-unretained-mirror"))?;
    fs::rename(&next_seed_mirror, work.join("offline-next-seed-mirror"))?;
    fs::rename(
        &next_primary_mirror,
        work.join("offline-next-primary-mirror"),
    )?;
    fs::rename(&broken_mirror, work.join("offline-broken-primary-mirror"))?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read(f.root.join(payload))?, b"old");
    apply(&fresh_root, &new_plan)?;
    assert_eq!(fs::read(fresh_root.join(payload))?, b"latest");
    let update = plan(&f.root, Request::Update)?;
    assert_eq!(update.request, Request::UpdateProfile);
    assert_eq!(update.catalogs["seed"], latest.trim());
    let staged = work.join("staged-profile");
    crate::stage_boot_environment(&f.root, &staged, &update)?;
    assert_eq!(fs::read(staged.join(payload))?, b"latest");
    assert_eq!(fs::read(f.root.join(payload))?, b"old");
    Ok(())
}

#[test]
fn automatic_payload_pruning_keeps_cached_versions_and_registered_roots() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let sync = |version, index_only| -> Result<()> {
        let mirror = work.join(format!("mirror-{version}"));
        sign_release(&f, &mirror, version, fresh, &anchor, &tuf_key)?;
        mirror_release::sync_with_options(
            &cache,
            &anchor,
            Url::from_directory_path(mirror.join("metadata"))
                .unwrap()
                .as_str(),
            Url::from_directory_path(mirror.join("targets"))
                .unwrap()
                .as_str(),
            "canopa",
            &f.public,
            mirror_release::SyncOptions {
                index_only,
                image: None,
            },
        )?;
        Ok(())
    };
    let package = |version, bytes| {
        f.package(
            "desktop/compositor",
            version,
            f.file(COMPOSITOR, bytes, "0644", "")?,
        )
    };
    let install = |root, version| -> Result<()> {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
        let plan = plan(
            root,
            Request::Install {
                packages: vec![format!("desktop/compositor@{version}")],
            },
        )?;
        apply(root, &plan)?;
        assert!(
            fs::read_dir(root.join("var/lib/cps/image/stage"))?
                .next()
                .is_none()
        );
        Ok(())
    };
    let fallback = work.join("fallback");
    let current = work.join("running");
    package("1.0.0-0", "fallback")?;
    sync(1, false)?;
    install(&fallback, "1.0.0-0")?;
    package("2.0.0-0", "obsolete")?;
    package("3.0.0-0", "retained")?;
    package("10.0.0-0", "latest")?;
    f.package(
        "test/other",
        "1.0.0-0",
        f.file(LIBRARY, "other", "0644", "")?,
    )?;
    sync(2, false)?;
    install(&current, "10.0.0-0")?;
    withdraw_package(
        &f.repo,
        &f.private,
        "cps://canopa/desktop/compositor@3.0.0-0",
    )?;
    package("20.0.0-0", "unfetched")?;
    sync(3, true)?;
    let blob = |bytes: &str| cache.join("blobs").join(digest(bytes.as_bytes()));
    let orphan = blob("orphan");
    fs::write(&orphan, b"orphan")?;
    let prune = |versions| -> Result<()> {
        let images = [
            ("running".into(), crate::image::Image::open_query(&current)?),
            (
                "fallback".into(),
                crate::image::Image::open_query(&fallback)?,
            ),
        ];
        let publishers = images[0].1.publishers()?;
        mirror_release::prune_system_payloads(&publishers.as_slice()[0], &images, versions)?;
        Ok(())
    };
    prune(2)?;
    assert!(!orphan.exists());
    assert!(!blob("obsolete").exists());
    assert!(!blob("unfetched").exists());
    for content in ["fallback", "retained", "latest", "other"] {
        assert!(blob(content).is_file(), "missing {content}");
    }
    prune(0)?;
    assert!(!blob("retained").exists());
    assert!(!blob("other").exists());
    fs::rename(work.join("mirror-3"), work.join("offline"))?;
    for (root, expected) in [(&fallback, "fallback"), (&current, "latest")] {
        fs::write(root.join(COMPOSITOR), "damaged")?;
        let repair = plan(
            root,
            Request::Repair {
                packages: Vec::new(),
            },
        )?;
        fetch_plan(root, &repair)?;
        apply(root, &repair)?;
        assert_eq!(fs::read_to_string(root.join(COMPOSITOR))?, expected);
    }
    Ok(())
}

#[test]
fn tuf_payload_pruning_preserves_saved_plans_and_offline_repairs() -> Result<()> {
    let f = Fixture::new()?;
    let builder = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let sync = |version| -> Result<PathBuf> {
        let mirror = work.join(format!("mirror-{version}"));
        sign_release(&f, &mirror, version, fresh, &anchor, &tuf_key)?;
        mirror_release::sync(
            &cache,
            &anchor,
            Url::from_directory_path(mirror.join("metadata"))
                .unwrap()
                .as_str(),
            Url::from_directory_path(mirror.join("targets"))
                .unwrap()
                .as_str(),
            "canopa",
            &f.public,
        )?;
        Ok(mirror)
    };
    let prune = |catalogs: &[String], images: &[PathBuf], plans: &[Plan], apply| {
        mirror_release::prune_payloads(
            &cache,
            "canopa",
            &f.public,
            mirror_release::CacheRetention {
                catalogs,
                images,
                plans,
            },
            apply,
        )
    };
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "previous", "0644", "")?,
    )?;
    let unsigned_pin = fs::read_to_string(f.repo.join("current"))?;
    let (_, envelope) = attach_evidence(&f, unsigned_pin.trim(), &builder)?;
    crate::enable_file_index(&f.repo, "canopa", &f.private)?;
    let first = sync(1)?;
    let old_catalog = fs::read_to_string(cache.join("current"))?.trim().to_owned();
    let installed = work.join("installed");
    for root in [&installed, &f.root] {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
    }
    assert_eq!(crate::query::search_files(&installed, COMPOSITOR)?.len(), 1);
    let request = Request::Install {
        packages: vec!["desktop/compositor".to_owned()],
    };
    let saved = plan(&f.root, request.clone())?;
    apply(&installed, &plan(&installed, request.clone())?)?;
    let saved_lock = crate::capture_image_lock(&installed)?;
    let pending = work.join("pending-image");
    let pending_config = work.join("pending-image.kdl");
    fs::write(
        &pending_config,
        format!(
            "schema-version 1 kind=\"image-config\"\npublisher \"canopa\" {{\n repository {cache:?}\n trust-key {:?}\n build-trust {{ builder-key {:?}; }}\n}}\n",
            f.public, builder.public,
        ),
    )?;
    create_configured_image(&pending, &pending_config)?;
    let pending_plan = plan(&pending, request)?;

    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "latest", "0644", "")?,
    )?;
    withdraw_package(
        &f.repo,
        &f.private,
        "cps://canopa/desktop/compositor@1.0.0-0",
    )?;
    let second = sync(2)?;
    let current = fs::read_to_string(cache.join("current"))?;
    let old_blob = digest(b"previous");
    let new_blob = digest(b"latest");
    let orphan = digest(b"orphan");
    let blob_dir = cache.join("blobs");
    fs::write(blob_dir.join(&orphan), b"orphan")?;
    fs::write(blob_dir.join("local-note"), b"leave this alone")?;
    let unrelated = work.join("unrelated");
    fs::write(&unrelated, b"outside the cache")?;
    let symlink = blob_dir.join(digest(b"unexpected symlink"));
    std::os::unix::fs::symlink(&unrelated, &symlink)?;

    let inventory = cache.join(".tuf/retention").join(current.trim());
    let saved_inventory = fs::read(&inventory)?;
    fs::remove_file(&inventory)?;
    let error =
        prune(&[], &[], &[], true).expect_err("prune accepted a cache without an inventory");
    assert!(format!("{error:#}").contains("cache has no retention inventory"));
    assert!(blob_dir.join(&old_blob).exists() && blob_dir.join(&orphan).exists());
    fs::write(inventory, saved_inventory)?;

    let report = prune(&[], &[], &[], false)?;
    assert_eq!(report.bytes, 14);
    assert!(report.blobs.iter().any(|(hash, _)| hash == &old_blob));
    let report = mirror_release::prune_payloads_with_locks(
        &cache,
        "canopa",
        &f.public,
        mirror_release::CacheRetention {
            catalogs: &[],
            images: &[],
            plans: &[],
        },
        &[saved_lock],
        false,
    )?;
    assert_eq!(report.blobs, [(orphan.clone(), 6)]);
    assert!(blob_dir.join(&old_blob).exists() && blob_dir.join(&orphan).exists());
    let report = mirror_release::prune_with_registered_roots(
        &cache,
        "canopa",
        &f.public,
        mirror_release::CacheRetention {
            catalogs: &[],
            images: &[],
            plans: &[],
        },
        &[(
            "fallback".into(),
            crate::image::Image::open_query(&installed)?,
        )],
        &[],
        false,
    )?;
    assert_eq!(report.blobs, [(orphan.clone(), 6)]);
    assert_eq!(report.registered_roots[0].id, "fallback");
    assert_eq!(report.registered_roots[0].package_payloads, 1);
    for report in [
        prune(std::slice::from_ref(&old_catalog), &[], &[], false)?,
        prune(&[], std::slice::from_ref(&installed), &[], false)?,
        prune(&[], &[], std::slice::from_ref(&saved), true)?,
    ] {
        assert_eq!(report.blobs, [(orphan.clone(), 6)]);
    }
    assert!(!blob_dir.join(&orphan).exists());
    assert!(blob_dir.join(&old_blob).exists() && blob_dir.join(&new_blob).exists());
    assert!(symlink.symlink_metadata()?.file_type().is_symlink());
    assert_eq!(fs::read(&unrelated)?, b"outside the cache");
    assert!(blob_dir.join("local-note").exists());

    fs::rename(first, work.join("offline-first"))?;
    let offline_second = work.join("offline-second");
    fs::rename(&second, &offline_second)?;
    apply(&f.root, &saved)?;
    assert_eq!(fs::read(f.root.join(COMPOSITOR))?, b"previous");
    fs::write(installed.join(COMPOSITOR), b"damaged")?;
    let repair = plan(
        &installed,
        Request::Repair {
            packages: Vec::new(),
        },
    )?;
    apply(&installed, &repair)?;
    assert_eq!(fs::read(installed.join(COMPOSITOR))?, b"previous");
    assert_eq!(prune(&[], &[], &[], true)?.blobs, [(old_blob.clone(), 8)]);
    assert!(!blob_dir.join(&old_blob).exists());
    assert!(blob_dir.join(new_blob).exists());
    assert_eq!(fs::read_to_string(cache.join("current"))?, current);
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, current);
    assert_eq!(
        snapshot(&cache, "canopa", &read_key(&f.public)?, Some(&old_catalog))?
            .packages
            .len(),
        1
    );

    fs::write(installed.join(COMPOSITOR), b"damaged")?;
    let request = Request::Repair {
        packages: Vec::new(),
    };
    let repair = plan(&installed, request.clone())?;
    fs::rename(&offline_second, &second)?;
    let error = fetch_plan(&installed, &repair).unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!("checking TUF target blobs/{old_blob}")),
        "{error:#}"
    );
    fs::rename(&second, &offline_second)?;

    let provision_archive = |repo: &Path, pin: &str, name: &str| -> Result<(PathBuf, PathBuf)> {
        let targets = work.join(format!("{name}-targets"));
        mirror_release::export_archive(repo, "canopa", &f.public, &targets, pin, false)?;
        let mirror = work.join(format!("{name}-mirror"));
        sign_export(&targets, &mirror, 1, fresh, &anchor, &tuf_key)?;
        let archive = work.join(name);
        assert_eq!(
            mirror_release::sync_archive(
                &archive,
                &anchor,
                Url::from_directory_path(mirror.join("metadata"))
                    .unwrap()
                    .as_str(),
                Url::from_directory_path(mirror.join("targets"))
                    .unwrap()
                    .as_str(),
                "canopa",
                &f.public,
                mirror_release::ArchiveSyncOptions {
                    catalog: pin,
                    index_only: true
                },
            )?,
            pin
        );
        Ok((mirror, archive))
    };
    let restore = |archive: &Path| {
        fetch_plan_with_archives(
            &installed,
            &repair,
            &[("canopa".into(), archive.to_owned())],
        )
    };
    let error = restore(&cache).unwrap_err();
    assert!(format!("{error:#}").contains("archive cache requires repo sync --archive-catalog"));
    let fork = work.join("fork");
    let actions = f.file(COMPOSITOR, "different", "0644", "")?;
    fs::write(
        &f.recipe,
        format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"desktop/compositor\" publisher=\"canopa\" version=\"1.0.0-0\" {{\n{actions}}}\n"
        ),
    )?;
    publish(&fork, &f.proto, &f.recipe, &f.private)?;
    let fork_pin = fs::read_to_string(fork.join("current"))?;
    let (_, fork_cache) = provision_archive(&fork, fork_pin.trim(), "fork-cache")?;
    let error = restore(&fork_cache).unwrap_err();
    assert!(
        format!("{error:#}").contains("installed manifest digest differs from the signed catalog")
    );
    assert!(!blob_dir.join(&old_blob).exists());

    let (archive_mirror, archive) = provision_archive(&f.repo, &old_catalog, "old-archive")?;
    let archive_blob = archive.join("blobs").join(&old_blob);
    assert!(!archive_blob.exists());
    let target = archive_mirror
        .join("targets")
        .join(format!("{old_blob}.blobs-{old_blob}"));
    fs::write(&target, b"tampered")?;
    let error = restore(&archive).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"), "{error:#}");
    assert!(!archive_blob.exists());
    assert!(!blob_dir.join(&old_blob).exists());
    fs::write(&target, b"previous")?;
    assert_eq!(restore(&archive)?, 1);
    assert_eq!(fs::read(&archive_blob)?, b"previous");
    assert_eq!(fs::read(blob_dir.join(&old_blob))?, b"previous");
    fs::rename(&archive_mirror, work.join("offline-archive"))?;
    fs::write(blob_dir.join(&old_blob), b"tampered")?;
    assert_eq!(restore(&archive)?, 1);
    assert_eq!(fs::read(installed.join(COMPOSITOR))?, b"damaged");
    assert_eq!(plan(&installed, request)?, repair);
    apply(&installed, &repair)?;
    assert!(verify(&installed)?.is_empty());
    assert_eq!(fs::read(installed.join(COMPOSITOR))?, b"previous");
    assert_eq!(fs::read_to_string(cache.join("current"))?, current);
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, current);
    assert!(!archive.join("current").exists());

    // Recreate the cache at the image's original path from the newer mirror,
    // which no longer exports the saved plan's catalog or manifest.
    fs::rename(&cache, work.join("original-cache"))?;
    fs::rename(&offline_second, &second)?;
    assert_eq!(
        mirror_release::sync_index(
            &cache,
            &anchor,
            Url::from_directory_path(second.join("metadata"))
                .unwrap()
                .as_str(),
            Url::from_directory_path(second.join("targets"))
                .unwrap()
                .as_str(),
            "canopa",
            &f.public,
        )?,
        current.trim()
    );
    fs::rename(&second, &offline_second)?;
    let restored_catalog = cache.join("catalogs").join(&old_catalog);
    let old_manifest = snapshot(
        &archive,
        "canopa",
        &read_key(&f.public)?,
        Some(&old_catalog),
    )?
    .packages
    .remove(0)
    .sha256;
    let restored_manifest = cache.join("manifests").join(old_manifest);
    let restored_evidence = cache.join("attestations").join(digest(&envelope));
    assert!(
        !restored_catalog.exists() && !restored_manifest.exists() && !restored_evidence.exists()
    );
    let error = fetch_plan(&pending, &pending_plan).unwrap_err();
    assert!(
        format!("{error:#}").contains("reading signed catalog"),
        "{error:#}"
    );
    let mut stale = pending_plan.clone();
    stale.base_revision += 1;
    let archives = [("canopa".to_owned(), archive.clone())];
    let error = fetch_plan_with_archives(&pending, &stale, &archives).unwrap_err();
    assert!(format!("{error:#}").contains("stale plan"));
    assert!(!restored_catalog.exists());
    let archived_evidence = archive.join("attestations").join(digest(&envelope));
    let saved_evidence = fs::read(&archived_evidence)?;
    fs::write(&archived_evidence, b"corrupt archive evidence")?;
    let error = fetch_plan_with_archives(&pending, &pending_plan, &archives).unwrap_err();
    assert!(format!("{error:#}").contains("digest mismatch for repository object attestations"));
    assert!(!restored_catalog.exists());
    fs::write(&archived_evidence, saved_evidence)?;
    assert_eq!(
        fetch_plan_with_archives(&pending, &pending_plan, &archives)?,
        1
    );
    assert!(restored_catalog.is_file());
    assert_eq!(fs::read(&restored_evidence)?, envelope);
    fs::write(&restored_manifest, b"corrupt manifest")?;
    fs::write(&restored_evidence, b"corrupt attestation")?;
    assert!(fetch_plan(&pending, &pending_plan).is_err());
    assert_eq!(
        fetch_plan_with_archives(&pending, &pending_plan, &archives)?,
        1
    );
    apply(&pending, &pending_plan)?;
    assert!(verify(&pending)?.is_empty());
    assert_eq!(fs::read(pending.join(COMPOSITOR))?, b"previous");
    assert_eq!(fs::read_to_string(cache.join("current"))?, current);
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, current);
    Ok(())
}

#[test]
fn image_rejects_catalog_replay_and_retired_signer_forks_without_cache_history() -> Result<()> {
    let f = Fixture::new()?;
    let rotated_key = Fixture::new()?;
    let current = f.repo.join("current");
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "first", "0755", "")?,
    )?;
    let first = fs::read_to_string(&current)?;
    f.create()?;
    let request = Request::Install {
        packages: vec!["desktop/compositor".into()],
    };
    let mut stale_plan = plan(&f.root, request.clone())?;

    f.package("test/fork", "1.0.0-0", String::new())?;
    let equal_fork = fs::read_to_string(&current)?;
    f.package("test/fork", "2.0.0-0", String::new())?;
    let higher_fork = fs::read_to_string(&current)?;
    fs::write(&current, &first)?;
    let rotated = rotate_publisher_key(
        &f.repo,
        "canopa",
        first.trim(),
        &f.private,
        &rotated_key.private,
    )?;

    let install = plan(&f.root, request.clone())?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("checkpointed-install.json"),
        &install,
        COMPOSITOR,
    )?;
    fs::write(&current, &first)?;
    let revision = apply(&f.root, &install)?;
    stale_plan.base_revision = revision;
    let error = apply(&f.root, &stale_plan).unwrap_err();
    assert!(format!("{error:#}").contains("publisher catalog rollback or fork"));

    for (rejected, reason) in [
        (&first, "publisher catalog rollback or fork"),
        (&equal_fork, "publisher catalog rollback or fork"),
        (
            &higher_fork,
            "release drops or changes the publisher key history",
        ),
    ] {
        fs::write(&current, rejected)?;
        let error = plan(&f.root, request.clone()).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        let error = list_packages(&f.root, Scope::Available).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        assert_eq!(list_packages(&f.root, Scope::Installed)?.len(), 1);
    }

    fs::write(&current, format!("{rotated}\n"))?;
    let update = f.file(COMPOSITOR, "next", "0755", "")?;
    fs::write(
        &f.recipe,
        format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"desktop/compositor\" publisher=\"canopa\" version=\"2.0.0-0\" {{\n{update}}}\n"
        ),
    )?;
    publish(&f.repo, &f.proto, &f.recipe, &rotated_key.private)?;
    fs::remove_file(f.repo.join("catalogs").join(&rotated))?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "next");
    Ok(())
}

#[test]
fn profiled_image_rejects_higher_sequence_forks_without_cache_history() -> Result<()> {
    let f = Fixture::new()?;
    f.package("desktop/base", "1.0.0-0", String::new())?;
    for name in [
        "initial",
        "retired",
        "successor",
        "fork-one",
        "fork-two",
        "fork-three",
    ] {
        f.package(&format!("test/{name}"), "1.0.0-0", String::new())?;
    }
    let set_policy = |elf: &str| {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/base"
        {elf}
    }}
}}
"#,
            ),
        )
    };
    let retire = |name| {
        withdraw_package(
            &f.repo,
            &f.private,
            &format!("cps://canopa/test/{name}@1.0.0-0"),
        )
    };
    let current = f.repo.join("current");
    set_policy("")?;
    retire("initial")?;
    let branch = fs::read_to_string(&current)?;
    let elf = r#"elf-check { library-dir "/usr/lib"; }"#;
    set_policy(elf)?;
    retire("retired")?;
    let applied = fs::read_to_string(&current)?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-key {:?}; }}
"#,
            f.repo, f.public,
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;

    retire("successor")?;
    let successor = fs::read_to_string(&current)?;
    let good_plan = plan(&f.root, Request::Update)?;
    fs::write(&current, branch)?;
    set_policy("")?;
    retire("fork-one")?;
    retire("fork-two")?;
    let weaker = fs::read_to_string(&current)?;
    set_policy(elf)?;
    retire("fork-three")?;
    let resurrection = fs::read_to_string(&current)?;
    fs::remove_file(f.repo.join("catalogs").join(applied.trim()))?;

    for (fork, reason) in [
        (
            &weaker,
            "release drops ELF checking for signed profile desktop",
        ),
        (
            &resurrection,
            "release drops or changes the withdrawal for cps://canopa/test/retired",
        ),
    ] {
        fs::write(&current, fork)?;
        {
            let image = crate::image::Image::open_query(&f.root)?;
            let settings = image.settings()?;
            let (checkpoint, _) = crate::repo::release_profile(
                &f.repo,
                "canopa",
                &read_key(&f.public)?,
                None,
                "desktop",
            )?;
            settings.check_catalog("canopa", &checkpoint)?;
        }
        for error in [
            plan(&f.root, Request::Update).unwrap_err(),
            list_packages(&f.root, Scope::Available).unwrap_err(),
            package_manifest(&f.root, "desktop/base", true).unwrap_err(),
        ] {
            assert!(format!("{error:#}").contains(reason), "{error:#}");
        }
        let mut changed = good_plan.clone();
        changed
            .catalogs
            .insert("canopa".into(), fork.trim().to_owned());
        let error = fetch_plan(&f.root, &changed).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        let error = apply(&f.root, &changed).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
    }
    fs::write(&current, successor)?;
    assert_eq!(plan(&f.root, Request::Update)?, good_plan);
    apply(&f.root, &good_plan)?;
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

#[test]
fn offline_verification_checks_catalog_admission_and_pending_archives() -> Result<()> {
    let f = Fixture::new()?;
    let install = |name: &str| {
        plan(
            &f.root,
            Request::Install {
                packages: vec![name.to_owned()],
            },
        )
    };
    f.package("test/retired", "1.0.0-0", String::new())?;
    f.create()?;
    apply(&f.root, &install("test/retired")?)?;
    withdraw_package(&f.repo, &f.private, "cps://canopa/test/retired@1.0.0-0")?;

    const SECOND: &str = "usr/share/test/second";
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "first", "0755", "")? + &f.file(SECOND, "second", "0644", "")?,
    )?;
    let plan = install("desktop/compositor")?;
    let saved = f._dir.path().join("pending.json");
    interrupt_apply_at(&f.root, &saved, &plan, COMPOSITOR)?;
    fs::remove_file(saved)?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
    let status = crate::transaction_status(&f.root)?;
    assert_eq!(status.revision, 1);
    let pending = status.pending.unwrap();
    assert_eq!(pending.plan_id, plan.id()?);
    assert_eq!(pending.target_revision, 2);
    assert!(!pending.files_ready);
    assert_eq!(pending.plan_summary, plan.summary());
    let journal: String =
        db.query_row("SELECT data FROM pending WHERE id=1", [], |row| row.get(0))?;
    for (field, reason) in [
        (
            "$.journaled.plan.payload_bytes",
            "journaled plan digest mismatch",
        ),
        (
            "$.journaled.next.settings.revision",
            "journaled image state digest mismatch",
        ),
    ] {
        db.execute(
            "UPDATE pending SET data=json_set(?1, ?2, 0) WHERE id=1",
            (&journal, field),
        )?;
        let error = crate::resume_transaction(&f.root).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        assert!(!f.root.join(SECOND).exists());
    }
    db.execute("UPDATE pending SET data=?1 WHERE id=1", [&journal])?;
    let archive: Vec<u8> = db.query_row(
        "SELECT catalog FROM catalog_archives WHERE publisher='canopa'",
        [],
        |row| row.get(0),
    )?;
    db.execute("DELETE FROM catalog_archives WHERE publisher='canopa'", [])?;
    let error = crate::resume_transaction(&f.root).unwrap_err();
    assert!(format!("{error:#}").contains("reading the archived catalog for canopa"));
    assert!(!f.root.join(SECOND).exists());
    db.execute(
        "INSERT INTO catalog_archives VALUES ('canopa', ?1)",
        [&archive],
    )?;
    let payload = f
        .root
        .join("var/lib/cps/image/stage")
        .join(digest(b"second"));
    fs::write(&payload, b"broken")?;
    let error = crate::resume_transaction(&f.root).unwrap_err();
    assert!(format!("{error:#}").contains("is corrupt"));
    assert!(!f.root.join(SECOND).exists());
    fs::write(&payload, b"second")?;
    assert_eq!(crate::resume_transaction(&f.root)?, Some(2));
    assert!(!payload.exists());
    assert_eq!(crate::resume_transaction(&f.root)?, None);
    let status = crate::transaction_status(&f.root)?;
    assert_eq!(status.revision, 2);
    assert!(status.pending.is_none());
    fs::write(&payload, b"second")?;
    assert_eq!(apply(&f.root, &plan)?, 2);
    assert!(!payload.exists());
    assert_eq!(fs::read_to_string(f.root.join(SECOND))?, "second");
    assert!(verify(&f.root)?.is_empty());

    let document: String = db.query_row(
        "SELECT json_extract(data, '$.published.document') FROM installed WHERE name='test/retired'",
        [],
        |row| row.get(0),
    )?;
    let edited = format!("{document}\n");
    db.execute(
        "UPDATE installed SET data=json_set(data, '$.published.document', ?1, '$.published.sha256', ?2)
         WHERE name='test/retired'",
        (&edited, digest(edited.as_bytes())),
    )?;
    let error = verify(&f.root).unwrap_err();
    assert!(
        format!("{error:#}").contains("installed manifest digest differs from the signed catalog")
    );
    let error = crate::verify_pristine(&f.root).unwrap_err();
    assert!(
        format!("{error:#}").contains("installed manifest digest differs from the signed catalog")
    );
    Ok(())
}

#[test]
fn standalone_and_companion_publishers_keep_withdrawals_after_cache_replacement() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    for name in [
        "library/runtime",
        "test/retired",
        "test/successor",
        "test/fork-one",
        "test/fork-two",
    ] {
        seed.package_as("seed", name, "1.0.0-0", String::new())?;
    }
    f.package(
        "desktop/base",
        "1.0.0-0",
        f.file(COMPOSITOR, "base", "0755", "")? + "    require \"library/runtime\"\n",
    )?;
    let retire = |name| {
        withdraw_package(
            &seed.repo,
            &seed.private,
            &format!("cps://seed/test/{name}@1.0.0-0"),
        )
    };
    let primary_pointer = f.repo.join("current");
    let seed_pointer = seed.repo.join("current");
    let publish_profile = || -> Result<()> {
        let pin = fs::read_to_string(&seed_pointer)?;
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "unused.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/base"
        publisher "seed" catalog={:?} {{
            repository {:?}
            trust-key {:?}
        }}
    }}
}}
"#,
                pin.trim(),
                seed.repo,
                seed.public,
            ),
        )?;
        let release = f._dir.path().join("refresh.kdl");
        let base = fs::read_to_string(&primary_pointer)?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim(),
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)?;
        Ok(())
    };
    publish_profile()?;
    let primary_branch = fs::read_to_string(&primary_pointer)?;
    let seed_branch = fs::read_to_string(&seed_pointer)?;
    retire("retired")?;
    let applied_seed = fs::read_to_string(&seed_pointer)?;
    publish_profile()?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{ repository {:?}; trust-key {:?}; }}
publisher "seed" {{ repository {:?}; trust-key {:?}; }}
"#,
            f.repo, f.public, seed.repo, seed.public,
        ),
    )?;
    let plain = f._dir.path().join("plain");
    create_image(
        &plain,
        &seed.repo,
        "seed",
        &seed.public,
        "x86_64",
        BTreeMap::new(),
    )?;
    apply(
        &plain,
        &plan(
            &plain,
            Request::Install {
                packages: vec!["library/runtime".into()],
            },
        )?,
    )?;
    create_configured_image(&f.root, &config)?;
    let install = plan(&f.root, Request::InstallProfile)?;
    interrupt_apply_at(
        &f.root,
        &f._dir.path().join("pending.json"),
        &install,
        COMPOSITOR,
    )?;
    for repo in [&f.repo, &seed.repo] {
        fs::rename(repo, repo.with_extension("offline"))?;
    }
    let replacements = [
        ("canopa".to_owned(), f.repo.with_extension("offline")),
        ("seed".to_owned(), seed.repo.with_extension("offline")),
    ];
    let error = crate::set_publisher_repositories(&f.root, &replacements).unwrap_err();
    assert!(error.to_string().contains("incomplete transaction"));
    apply(&f.root, &install)?;
    let error = plan(
        &f.root,
        Request::ChangeFacet {
            facets: BTreeMap::from([("doc.man".into(), Some(false))]),
            with_dependencies: false,
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the signed release profile fixes this image's image-wide facets")
    );
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    {
        let cached = seed.repo.with_extension("offline");
        let previous = crate::repo::catalog_bytes(&cached, seed_branch.trim())?;
        let applied = crate::repo::catalog_bytes(&cached, applied_seed.trim())?;
        let sequence = crate::repo::read_catalog(
            &cached,
            seed_branch.trim(),
            "seed",
            &read_key(&seed.public)?,
        )?
        .sequence();
        let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
        let settings: String =
            db.query_row("SELECT data FROM image WHERE id=1", [], |row| row.get(0))?;
        db.execute(
            "UPDATE image SET data=json_set(data, '$.catalog_checkpoints.seed.catalog', ?1,
             '$.catalog_checkpoints.seed.sequence', ?2) WHERE id=1",
            (seed_branch.trim(), i64::try_from(sequence)?),
        )?;
        db.execute(
            "UPDATE catalog_archives SET catalog=?1 WHERE publisher='seed'",
            [previous],
        )?;
        let error = verify(&f.root).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("archived catalog for seed differs from the signed profile pin")
        );
        db.execute("UPDATE image SET data=?1 WHERE id=1", [settings])?;
        db.execute(
            "UPDATE catalog_archives SET catalog=?1 WHERE publisher='seed'",
            [applied],
        )?;
    }
    for repo in [&f.repo, &seed.repo] {
        fs::rename(repo.with_extension("offline"), repo)?;
    }

    retire("successor")?;
    publish_profile()?;
    let good_primary = fs::read_to_string(&primary_pointer)?;
    let good_seed = fs::read_to_string(&seed_pointer)?;
    let good_plan = plan(&f.root, Request::Update)?;
    let plain_plan = plan(&plain, Request::Update)?;

    fs::write(&seed_pointer, seed_branch)?;
    fs::write(&primary_pointer, primary_branch)?;
    retire("fork-one")?;
    publish_profile()?;
    retire("fork-two")?;
    publish_profile()?;
    fs::remove_file(seed.repo.join("catalogs").join(applied_seed.trim()))?;
    let reason = "release drops or changes the withdrawal for cps://seed/test/retired";
    for image in [&f.root, &plain] {
        let error = plan(image, Request::Update).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        let error = list_packages(image, Scope::Available).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        let error = package_manifest(image, "cps://seed/library/runtime", true).unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
    }
    let mut fork = good_plan.clone();
    for (publisher, pointer) in [("canopa", &primary_pointer), ("seed", &seed_pointer)] {
        fork.catalogs
            .insert(publisher.into(), fs::read_to_string(pointer)?.trim().into());
    }
    let error = apply(&f.root, &fork).unwrap_err();
    assert!(format!("{error:#}").contains(reason), "{error:#}");
    let extras = Fixture::new()?;
    extras.package_as("extras", "apps/extra", "1.0.0-0", String::new())?;
    crate::add_publisher(&f.root, "extras", &extras.repo, &extras.public)?;
    check_guarded_sync(&f, &seed, &plain, [&good_primary, &good_seed], reason)?;

    for (source, (_, replacement)) in [&f.repo, &seed.repo].into_iter().zip(&replacements) {
        fs::rename(source, replacement)?;
    }
    let settings = || crate::image::Image::open_query(&f.root)?.settings();
    let before = settings()?;
    let error = crate::set_publisher_repositories(&f.root, &replacements).unwrap_err();
    assert!(format!("{error:#}").contains(reason), "{error:#}");
    assert_eq!(settings()?, before);
    assert_eq!(crate::list_publishers(&f.root)?[0].repository, f.repo);
    assert_eq!(crate::list_publishers(&f.root)?[1].repository, seed.repo);
    let error = crate::set_publisher_repositories(&plain, &replacements[1..]).unwrap_err();
    assert!(format!("{error:#}").contains(reason), "{error:#}");
    assert_eq!(crate::list_publishers(&plain)?[0].repository, seed.repo);

    fs::write(replacements[0].1.join("current"), good_primary)?;
    fs::write(replacements[1].1.join("current"), good_seed)?;
    crate::set_publisher_repositories(&f.root, &replacements)?;
    let mut expected = before;
    expected.publisher_generation += 1;
    assert_eq!(settings()?, expected);
    crate::set_publisher_repositories(&f.root, &replacements)?;
    assert_eq!(settings()?, expected);
    assert_eq!(
        crate::list_publishers(&f.root)?[0].repository,
        replacements[0].1
    );
    assert_eq!(
        crate::list_publishers(&f.root)?[1].repository,
        replacements[1].1
    );
    assert!(
        apply(&f.root, &good_plan)
            .unwrap_err()
            .to_string()
            .contains("publisher configuration changed")
    );
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    crate::set_publisher_repositories(&plain, &replacements[1..])?;
    assert!(
        apply(&plain, &plain_plan)
            .unwrap_err()
            .to_string()
            .contains("publisher configuration changed")
    );
    apply(&plain, &plan(&plain, Request::Update)?)?;
    Ok(())
}

fn check_guarded_sync(
    primary: &Fixture,
    seed: &Fixture,
    plain: &Path,
    good: [&str; 2],
    reason: &str,
) -> Result<()> {
    let work = primary._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let primary_pointer = primary.repo.join("current");
    let seed_pointer = seed.repo.join("current");
    let primary_fork = fs::read_to_string(&primary_pointer)?;
    let seed_fork = fs::read_to_string(&seed_pointer)?;
    let endpoint = |mirror: &Path, role| Url::from_directory_path(mirror.join(role)).unwrap();
    let plain_cache = work.join("guarded-standalone");
    let sync_plain = |mirror: &Path| {
        mirror_release::sync_with_options(
            &plain_cache,
            &anchor,
            endpoint(mirror, "metadata").as_str(),
            endpoint(mirror, "targets").as_str(),
            "seed",
            &seed.public,
            mirror_release::SyncOptions {
                index_only: true,
                image: Some(plain),
            },
        )
    };
    let fork_mirror = work.join("fork-seed-mirror");
    sign_publisher_release(seed, &fork_mirror, 1, fresh, &anchor, &tuf_key, "seed")?;
    let error = sync_plain(&fork_mirror).unwrap_err();
    assert!(format!("{error:#}").contains(reason), "{error:#}");
    assert!(!plain_cache.join("current").exists());
    assert!(!plain_cache.join(".tuf/catalog").exists());

    let primary_cache = work.join("guarded-primary");
    let seed_cache = work.join("guarded-companion");
    let config = work.join("guarded-sync.kdl");
    let sync_profile = |primary_mirror: &Path, seed_mirror: &Path| -> Result<String> {
        let mut document =
            "schema-version 1 kind=\"image-config\"\narchitecture \"x86_64\"\nrelease-profile \"desktop\"\n".to_owned();
        for (name, repo, key, mirror) in [
            ("canopa", &primary_cache, &primary.public, primary_mirror),
            ("seed", &seed_cache, &seed.public, seed_mirror),
        ] {
            document.push_str(&format!(
                "publisher {name:?} {{ repository {repo:?}; trust-key {key:?}; }}\n"
            ));
            document.push_str(&format!(
                "mirror {name:?} root={anchor:?} metadata={:?} targets={:?}\n",
                endpoint(mirror, "metadata").as_str(),
                endpoint(mirror, "targets").as_str()
            ));
        }
        fs::write(&config, document)?;
        mirror_release::sync_profile_for_image(&config, true, &primary.root)
    };
    let fork_primary_mirror = work.join("fork-primary-mirror");
    sign_release(primary, &fork_primary_mirror, 1, fresh, &anchor, &tuf_key)?;
    // The companion's current catalog preserves the applied withdrawal. The
    // higher-sequence fork pinned by the primary is still retained and invalid.
    withdraw_package(&seed.repo, &seed.private, "cps://seed/test/retired@1.0.0-0")?;
    let export = work.join("seed-with-fork");
    mirror_release::export_with_retained(
        &seed.repo,
        "seed",
        &seed.public,
        &export,
        &[seed_fork.trim().to_owned()],
    )?;
    let retained_fork = work.join("retained-fork-mirror");
    sign_export(&export, &retained_fork, 1, fresh, &anchor, &tuf_key)?;
    let error = sync_profile(&fork_primary_mirror, &retained_fork).unwrap_err();
    assert!(format!("{error:#}").contains(reason), "{error:#}");
    for repo in [&primary_cache, &seed_cache] {
        assert!(!repo.join("current").exists());
        assert!(!repo.join(".tuf/catalog").exists());
    }

    fs::write(&primary_pointer, good[0])?;
    fs::write(&seed_pointer, good[1])?;
    let good_primary = work.join("good-primary-mirror");
    let good_seed = work.join("good-seed-mirror");
    sign_release(primary, &good_primary, 2, fresh, &anchor, &tuf_key)?;
    sign_publisher_release(seed, &good_seed, 2, fresh, &anchor, &tuf_key, "seed")?;
    assert_eq!(sync_plain(&good_seed)?, good[1].trim());
    assert_eq!(sync_profile(&good_primary, &good_seed)?, good[0].trim());
    assert_eq!(fs::read_to_string(primary_cache.join("current"))?, good[0]);
    assert_eq!(fs::read_to_string(seed_cache.join("current"))?, good[1]);
    fs::write(primary_pointer, primary_fork)?;
    fs::write(seed_pointer, seed_fork)?;
    Ok(())
}

#[test]
fn fetch_toolchain_uses_retained_catalogs_and_only_fetches_selected_payloads() -> Result<()> {
    let primary = Fixture::new()?;
    let tools = Fixture::new()?;
    let shell = "pinned shell";
    let compiler = "pinned compiler";
    let latest = "current compiler";
    let unused = "unused tool";
    primary.package(
        "build/shell",
        "1.0.0-0",
        primary.file("usr/bin/sh", shell, "0755", "")?,
    )?;
    primary.package(
        "build/unused",
        "1.0.0-0",
        primary.file("usr/bin/unused", unused, "0755", "")?,
    )?;
    let compiler_actions = |bytes| -> Result<String> {
        Ok(tools.file("usr/bin/cc", bytes, "0755", "")?
            + &dependency("require", "build/shell", "1.0.0-0"))
    };
    tools.package_as(
        "tools",
        "build/compiler",
        "1.0.0-0",
        compiler_actions(compiler)?,
    )?;
    let current = |repo: &Path| -> Result<String> {
        Ok(fs::read_to_string(repo.join("current"))?.trim().to_owned())
    };
    let primary_pin = current(&primary.repo)?;
    let tools_pin = current(&tools.repo)?;
    tools.package_as(
        "tools",
        "build/compiler",
        "2.0.0-0",
        compiler_actions(latest)?,
    )?;
    withdraw_package(
        &tools.repo,
        &tools.private,
        "cps://tools/build/compiler@1.0.0-0",
    )?;

    let work = primary._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let primary_cache = work.join("canopa-cache");
    let tools_cache = work.join("tools-cache");
    let sync = |fixture: &Fixture, cache: &Path, publisher, mirror: &Path, pin: &str| {
        mirror_release::sync_with_required_catalogs(
            cache,
            &anchor,
            Url::from_directory_path(mirror.join("metadata"))
                .unwrap()
                .as_str(),
            Url::from_directory_path(mirror.join("targets"))
                .unwrap()
                .as_str(),
            publisher,
            &fixture.public,
            mirror_release::SyncOptions {
                index_only: true,
                image: None,
            },
            &[pin.to_owned()],
        )
    };
    let primary_mirror = work.join("canopa-mirror");
    sign_release(&primary, &primary_mirror, 1, fresh, &anchor, &tuf_key)?;
    assert_eq!(
        sync(
            &primary,
            &primary_cache,
            "canopa",
            &primary_mirror,
            &primary_pin
        )?,
        primary_pin
    );

    let unretained = work.join("without-retained");
    sign_publisher_release(&tools, &unretained, 1, fresh, &anchor, &tuf_key, "tools")?;
    let error = sync(&tools, &tools_cache, "tools", &unretained, &tools_pin).unwrap_err();
    assert!(
        format!("{error:#}").contains(&format!("does not advertise required catalog {tools_pin}"))
    );
    assert!(!tools_cache.join("current").exists());

    let export = work.join("tools-export");
    let latest_catalog = mirror_release::export_with_retained(
        &tools.repo,
        "tools",
        &tools.public,
        &export,
        std::slice::from_ref(&tools_pin),
    )?;
    let tools_mirror = work.join("tools-mirror");
    sign_export(&export, &tools_mirror, 2, fresh, &anchor, &tuf_key)?;
    assert_eq!(
        sync(&tools, &tools_cache, "tools", &tools_mirror, &tools_pin)?,
        latest_catalog
    );

    let recipe = work.join("build.kdl");
    fs::write(
        &recipe,
        format!(
            "schema-version 1 kind=\"build-recipe\"\nbuild architecture=\"x86_64\" epoch=1700000000 {{\n\
         toolchain solver=\"sat-v2\" {{\n\
         publisher \"canopa\" catalog={primary_pin:?} key-sha256={:?}\n\
         publisher \"tools\" catalog={tools_pin:?} key-sha256={:?}\n\
         package \"cps://tools/build/compiler@1.0.0-0\"\n }}\n\
         manifest \"manifest.kdl\" sha256={:?}\n script \"exit 1\"\n}}\n",
            digest(&read_key(&primary.public)?),
            digest(&read_key(&tools.public)?),
            digest(b"reviewed manifest"),
        ),
    )?;
    let config = work.join("toolchain.kdl");
    let write_config = |policy: &str| {
        fs::write(
            &config,
            format!(
                "schema-version 1 kind=\"toolchain-config\"\n\
             publisher \"canopa\" {{ repository {primary_cache:?}; trust-key {:?}; }}\n\
             publisher \"tools\" {{ repository {tools_cache:?}; trust-key {:?}; {policy} }}\n",
                primary.public, tools.public,
            ),
        )
    };
    let shell_blob = primary_cache.join("blobs").join(digest(shell.as_bytes()));
    let compiler_blob = tools_cache.join("blobs").join(digest(compiler.as_bytes()));
    write_config(&format!(
        "build-trust {{ builder-key {:?}; }};",
        primary.public
    ))?;
    let error = fetch_toolchain(&recipe, &config).unwrap_err();
    assert!(
        format!("{error:#}").contains("build attestation"),
        "{error:#}"
    );
    assert!(!shell_blob.exists());
    assert!(!compiler_blob.exists());

    write_config("")?;
    assert_eq!(fetch_toolchain(&recipe, &config)?, 2);
    assert_eq!(fs::read_to_string(&shell_blob)?, shell);
    assert_eq!(fs::read_to_string(&compiler_blob)?, compiler);
    assert!(
        !primary_cache
            .join("blobs")
            .join(digest(unused.as_bytes()))
            .exists()
    );
    assert!(
        !tools_cache
            .join("blobs")
            .join(digest(latest.as_bytes()))
            .exists()
    );
    assert_eq!(current(&primary_cache)?, primary_pin);
    assert_eq!(current(&tools_cache)?, latest_catalog);

    fs::write(&compiler_blob, "corrupt bytes!!")?;
    assert_eq!(fetch_toolchain(&recipe, &config)?, 2);
    assert_eq!(fs::read_to_string(&compiler_blob)?, compiler);
    fs::rename(&primary_mirror, work.join("offline-canopa"))?;
    fs::rename(&tools_mirror, work.join("offline-tools"))?;
    assert_eq!(fetch_toolchain(&recipe, &config)?, 2);
    Ok(())
}

#[test]
fn archive_mirror_provisions_a_withdrawn_toolchain_without_a_live_pointer() -> Result<()> {
    let fixture = Fixture::new()?;
    let payload = "pinned compiler\n".repeat(4096);
    let path = "usr/bin/cc";
    fixture.package(
        "build/compiler",
        "1.0.0-0",
        fixture.file(path, &payload, "0755", "")?,
    )?;
    let pointer = fixture.repo.join("current");
    let pin = fs::read_to_string(&pointer)?.trim().to_owned();
    fixture.package(
        "build/compiler",
        "2.0.0-0",
        fixture.file(path, "new compiler", "0755", "")?,
    )?;
    withdraw_package(
        &fixture.repo,
        &fixture.private,
        "cps://canopa/build/compiler@1.0.0-0",
    )?;
    let live_pin = fs::read_to_string(&pointer)?.trim().to_owned();
    let work = fixture._dir.path();
    fs::write(&pointer, format!("{pin}\n"))?;
    fixture.package("build/unrelated", "1.0.0-0", String::new())?;
    let fork = fs::read_to_string(&pointer)?.trim().to_owned();
    fs::write(&pointer, format!("{live_pin}\n"))?;
    let rejected = work.join("fork-export");
    let error = mirror_release::export_archive(
        &fixture.repo,
        "canopa",
        &fixture.public,
        &rejected,
        &fork,
        false,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("release drops or changes the published manifest"));
    assert!(!rejected.exists());

    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let live = work.join("live");
    sign_release(&fixture, &live, 2, fresh, &anchor, &tuf_key)?;
    let targets = work.join("archive-targets");
    assert_eq!(
        mirror_release::export_archive(
            &fixture.repo,
            "canopa",
            &fixture.public,
            &targets,
            &pin,
            false
        )?,
        pin
    );
    assert_eq!(fs::read_to_string(targets.join("archive"))?.trim(), pin);
    assert!(
        targets
            .join(format!("blobs-{}.zst", digest(payload.as_bytes())))
            .is_file()
    );
    assert!(!targets.join("current").exists());
    assert!(!targets.join(format!("catalogs-{live_pin}")).exists());
    assert!(
        !targets
            .join(format!("blobs-{}", digest(b"new compiler")))
            .exists()
    );
    assert_eq!(fs::read_to_string(&pointer)?.trim(), live_pin);
    let archive = work.join("archive");
    sign_export(&targets, &archive, 1, fresh, &anchor, &tuf_key)?;

    let endpoint = |mirror: &Path, role| Url::from_directory_path(mirror.join(role)).unwrap();
    let sync_live = |cache: &Path, mirror: &Path| {
        mirror_release::sync_index(
            cache,
            &anchor,
            endpoint(mirror, "metadata").as_str(),
            endpoint(mirror, "targets").as_str(),
            "canopa",
            &fixture.public,
        )
    };
    let sync_archive = |cache: &Path, mirror: &Path, catalog: &str| {
        mirror_release::sync_archive(
            cache,
            &anchor,
            endpoint(mirror, "metadata").as_str(),
            endpoint(mirror, "targets").as_str(),
            "canopa",
            &fixture.public,
            mirror_release::ArchiveSyncOptions {
                catalog,
                index_only: true,
            },
        )
    };
    let live_cache = work.join("live-cache");
    let archive_cache = work.join("archive-cache");
    let error = sync_live(&live_cache, &archive).unwrap_err();
    assert!(format!("{error:#}").contains("archive mirror requires repo sync --archive-catalog"));
    assert!(!live_cache.join(".tuf/trusted").exists());
    assert_eq!(sync_live(&live_cache, &live)?, live_pin);
    let error = sync_archive(&live_cache, &archive, &pin).unwrap_err();
    assert!(format!("{error:#}").contains("archive sync requires a dedicated cache"));
    let error = sync_archive(&archive_cache, &live, &pin).unwrap_err();
    assert!(format!("{error:#}").contains("archive mirror advertises a live catalog"));
    assert!(!archive_cache.join(".tuf/trusted").exists());
    let error = sync_archive(&work.join("wrong-pin"), &archive, &live_pin).unwrap_err();
    assert!(format!("{error:#}").contains("checking the signed archive marker"));
    assert_eq!(sync_archive(&archive_cache, &archive, &pin)?, pin);
    assert!(!archive_cache.join("current").exists());
    assert!(!archive_cache.join(".tuf/catalog").exists());
    let error = sync_live(&archive_cache, &live).unwrap_err();
    assert!(format!("{error:#}").contains("archive cache requires repo sync --archive-catalog"));
    let error = sync_archive(&archive_cache, &archive, &live_pin).unwrap_err();
    assert!(format!("{error:#}").contains("archive cache is bound to a different catalog"));

    let recipe = work.join("build.kdl");
    fs::write(
        &recipe,
        format!(
            "schema-version 1 kind=\"build-recipe\"\nbuild architecture=\"x86_64\" epoch=1700000000 {{\n\
         toolchain solver=\"sat-v2\" {{ publisher \"canopa\" catalog={pin:?} key-sha256={:?}; package \"build/compiler\"; }}\n\
         manifest \"manifest.kdl\" sha256={:?}\n script \"exit 1\"\n}}\n",
            digest(&read_key(&fixture.public)?),
            digest(b"reviewed manifest"),
        ),
    )?;
    let config = work.join("toolchain.kdl");
    fs::write(
        &config,
        format!(
            "schema-version 1 kind=\"toolchain-config\"\npublisher \"canopa\" {{ repository {archive_cache:?}; trust-key {:?}; }}\n",
            fixture.public,
        ),
    )?;
    let blob = archive_cache.join("blobs").join(digest(payload.as_bytes()));
    assert!(!blob.exists());
    let saved = work.join("offline-archive");
    fs::rename(&archive, &saved)?;
    fs::rename(&live, &archive)?;
    let error = fetch_toolchain(&recipe, &config).unwrap_err();
    assert!(format!("{error:#}").contains("archive mirror advertises a live catalog"));
    assert!(!blob.exists());
    fs::rename(&archive, &live)?;
    fs::rename(&saved, &archive)?;
    assert_eq!(fetch_toolchain(&recipe, &config)?, 1);
    assert_eq!(fs::read_to_string(blob)?, payload);
    fs::rename(&archive, &saved)?;
    assert_eq!(fetch_toolchain(&recipe, &config)?, 1);
    assert_eq!(
        fs::read_to_string(live_cache.join("current"))?.trim(),
        live_pin
    );
    Ok(())
}

#[test]
fn publisher_rotation_survives_partial_sync_and_preserves_pinned_plans() -> Result<()> {
    let f = Fixture::new()?;
    let second_key = Fixture::new()?;
    let third_key = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_root(&anchor, &tuf_key, None)?;
    let cache = work.join("cache");
    let sign = |version| -> Result<PathBuf> {
        let mirror = work.join(format!("mirror-{version}"));
        sign_release(&f, &mirror, version, fresh, &anchor, &tuf_key)?;
        Ok(mirror)
    };
    let refresh = |cache: &Path, mirror: &Path| {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let create = |root: &Path| -> Result<()> {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
        Ok(())
    };
    let install = |root: &Path| {
        plan(
            root,
            Request::Install {
                packages: vec!["desktop/compositor".to_owned()],
            },
        )
    };

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    let initial = refresh(&cache, &sign(1)?)?;
    create(&f.root)?;
    apply(&f.root, &install(&f.root)?)?;
    let historical = work.join("historical");
    create(&historical)?;
    let saved = install(&historical)?;

    let rotated =
        rotate_publisher_key(&f.repo, "canopa", &initial, &f.private, &second_key.private)?;
    let current = f.repo.join("current");
    assert_eq!(fs::read_to_string(&current)?.trim(), rotated);
    let error = rotate_publisher_key(
        &f.repo,
        "canopa",
        &initial,
        &second_key.private,
        &third_key.private,
    )
    .unwrap_err();
    assert!(error.to_string().contains("rotation base is stale"));
    let error = publish(&f.repo, &f.proto, &f.recipe, &f.private).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("signing key is not the current publisher key")
    );

    let actions = f.file(COMPOSITOR, "new", "0755", "")?;
    fs::write(
        &f.recipe,
        format!(
            "schema-version 1 kind=\"package-manifest\"\npackage \"desktop/compositor\" publisher=\"canopa\" version=\"2.0.0-0\" {{\n{actions}}}\n"
        ),
    )?;
    publish(&f.repo, &f.proto, &f.recipe, &second_key.private)?;
    let second = fs::read_to_string(&current)?;
    let mirror = sign(2)?;
    let blob = digest(b"new");
    fs::write(
        mirror.join("targets").join(format!("{blob}.blobs-{blob}")),
        b"bad",
    )?;
    let error = refresh(&cache, &mirror).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"));
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, second);

    // Simulate the retired signer creating a higher-sequence fork and getting
    // it wrapped in fresh TUF metadata. The publisher checkpoint must win.
    fs::write(&current, format!("{initial}\n"))?;
    for version in ["1.0.0-0", "2.0.0-0", "3.0.0-0"] {
        f.package("test/fork", version, String::new())?;
    }
    let fork = sign(3)?;
    fs::write(&current, &second)?;
    let error = refresh(&cache, &fork).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("drops or changes the publisher key history")
    );
    assert_eq!(fs::read_to_string(cache.join(".tuf/catalog"))?, second);

    let latest = rotate_publisher_key(
        &f.repo,
        "canopa",
        second.trim(),
        &second_key.private,
        &third_key.private,
    )?;
    let error = rotate_publisher_key(&f.repo, "canopa", &latest, &third_key.private, &f.private)
        .unwrap_err();
    assert!(error.to_string().contains("publisher key was already used"));
    assert_eq!(fs::read_to_string(&current)?.trim(), latest);
    let newest = sign(4)?;
    assert_eq!(refresh(&cache, &newest)?, latest);
    assert_eq!(refresh(&work.join("new-cache"), &newest)?, latest);
    let old_id = "cps://canopa/desktop/compositor@1.0.0-0";
    assert!(withdraw_package(&f.repo, &second_key.private, old_id).is_err());
    withdraw_package(&f.repo, &third_key.private, old_id)?;
    refresh(&cache, &sign(5)?)?;

    apply(&historical, &saved)?;
    assert_eq!(fs::read_to_string(historical.join(COMPOSITOR))?, "old");
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "new");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

#[test]
fn tuf_obsoletion_removes_only_when_dependencies_allow_and_preserves_history() -> Result<()> {
    use crate::query::{Scope, list_packages, package_manifest};

    const NAME: &str = "library/legacy";
    const OLD: &str = "cps://canopa/library/legacy@1.0.0-0";
    const CUTOFF: &str = "cps://canopa/library/legacy@2.0.0-0";
    let f = Fixture::new()?;
    let extras = Fixture::new()?;
    let work = f._dir.path();
    let cache = work.join("cache");
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let sync = |version| {
        let mirror = work.join(format!("mirror-{version}"));
        sign_release(&f, &mirror, version, fresh, &anchor, &tuf_key)?;
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let create = |root: &Path| -> Result<()> {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
        crate::add_publisher(root, "extras", &extras.repo, &extras.public)
    };
    let install = |root: &Path, package: &str| {
        plan(
            root,
            Request::Install {
                packages: vec![package.to_owned()],
            },
        )
    };
    let config = f.file(CONFIG, "vendor", "0644", "preserve=#true")?;
    f.package(
        NAME,
        "1.0.0-0",
        f.file(LIBRARY, "legacy", "0644", "")? + &config,
    )?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "original", "0755", "")? + &dependency("require", NAME, "1.0.0-0"),
    )?;
    extras.package_as("extras", NAME, "9.0.0-0", String::new())?;
    let base = sync(1)?;
    create(&f.root)?;
    apply(&f.root, &install(&f.root, "desktop/compositor")?)?;
    fs::write(f.root.join(CONFIG), "local settings")?;
    let historical = work.join("historical");
    create(&historical)?;
    let saved = install(&historical, OLD)?;
    let skipped = work.join("skipped");
    create(&skipped)?;
    apply(&skipped, &install(&skipped, OLD)?)?;

    let release = work.join("retire.kdl");
    let retire = |id: &str| {
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={base:?} {{\n    obsolete {id:?}\n}}\n"
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)
    };
    assert!(
        retire(OLD)
            .unwrap_err()
            .to_string()
            .contains("must be newer")
    );
    let retired = retire(CUTOFF)?;
    assert_eq!(sync(2)?, retired);
    assert_eq!(list_obsoletions(&cache, "canopa", &f.public)?, [CUTOFF]);
    assert!(list_withdrawals(&cache, "canopa", &f.public)?.contains_key(OLD));
    let error = plan(&f.root, Request::Update).unwrap_err();
    assert!(format!("{error:#}").contains(NAME));
    assert_eq!(fs::read_to_string(f.root.join(LIBRARY))?, "legacy");
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "original");

    let fresh_root = work.join("fresh");
    create(&fresh_root)?;
    let error = install(&fresh_root, NAME).unwrap_err();
    assert!(error.to_string().contains("is obsolete through 2.0.0-0"));
    assert!(
        install(&fresh_root, OLD)
            .unwrap_err()
            .to_string()
            .contains("is obsolete")
    );
    assert!(
        package_manifest(&fresh_root, NAME, true)
            .unwrap_err()
            .to_string()
            .contains("is obsolete")
    );
    assert!(
        list_packages(&fresh_root, Scope::Available)?
            .iter()
            .all(|row| row.name != NAME)
    );
    let qualified = "cps://extras/library/legacy@9.0.0-0";
    apply(&fresh_root, &install(&fresh_root, qualified)?)?;
    apply(&fresh_root, &plan(&fresh_root, Request::Update)?)?;
    assert!(
        list_packages(&fresh_root, Scope::Installed)?
            .iter()
            .any(|row| row.id == qualified)
    );

    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "current", "0755", "")?,
    )?;
    sync(3)?;
    let update = plan(&f.root, Request::Update)?;
    assert!(
        update
            .summary()
            .contains("Remove cps://canopa/library/legacy")
    );
    apply(&f.root, &update)?;
    assert!(!f.root.join(LIBRARY).exists());
    assert_eq!(fs::read_to_string(f.root.join(CONFIG))?, "local settings");
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "current");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));

    apply(&historical, &saved)?;
    assert_eq!(fs::read_to_string(historical.join(LIBRARY))?, "legacy");
    apply(&historical, &plan(&historical, Request::Update)?)?;
    assert!(list_packages(&historical, Scope::Installed)?.is_empty());
    assert!(!historical.join(LIBRARY).exists());
    assert!(
        f.package(NAME, "2.0.0-0", String::new())
            .unwrap_err()
            .to_string()
            .contains("obsolete")
    );
    f.package(NAME, "3.0.0-0", f.file(LIBRARY, "revived", "0644", "")?)?;
    sync(4)?;
    apply(&skipped, &plan(&skipped, Request::Update)?)?;
    assert_eq!(fs::read_to_string(skipped.join(LIBRARY))?, "revived");
    assert_eq!(list_obsoletions(&cache, "canopa", &f.public)?, [CUTOFF]);
    Ok(())
}

#[test]
fn offline_build_audit_requires_the_catalog_admitted_attestation() -> Result<()> {
    let f = Fixture::new()?;
    let builder = Fixture::new()?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "verified payload", "0755", "")?,
    )?;
    let pointer = f.repo.join("current");
    let initial = fs::read_to_string(&pointer)?;
    let recipe = fs::read(&f.recipe)?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    apply(&f.root, &install)?;

    f.package("test/extra", "1.0.0-0", String::new())?;
    let branch = fs::read_to_string(&pointer)?;
    fs::write(&f.recipe, recipe)?;
    let (_, envelope) = attach_evidence(&f, initial.trim(), &builder)?;
    let admitted = fs::read_to_string(&pointer)?;
    fs::write(&pointer, &branch)?;
    let (_, alternative) = attach_evidence(&f, branch.trim(), &builder)?;
    assert_ne!(envelope, alternative);
    fs::write(&pointer, &admitted)?;

    let policy = f._dir.path().join("policy.kdl");
    fs::write(
        &policy,
        format!(
            "schema-version 1 kind=\"build-trust\"\nbuild-trust {{ builder-key {:?}; }}\n",
            builder.public
        ),
    )?;
    crate::set_publisher_build_trust(&f.root, "canopa", &policy, false)?;
    let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
    let supplemental: String = db.query_row(
        "SELECT hash FROM build_admission_catalogs WHERE publisher='canopa'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(supplemental, admitted.trim());
    let offline = f.repo.with_extension("offline");
    for advance in [false, true] {
        if advance {
            apply(&f.root, &plan(&f.root, Request::Update)?)?;
            db.execute("DELETE FROM build_admission_catalogs", [])?;
        }
        fs::rename(&f.repo, &offline)?;
        let audit = crate::audit_publisher_builds(&f.root, "canopa")?;
        assert_eq!(audit.verified, 1, "{:?}", audit.failures);
        db.execute(
            "UPDATE build_receipts SET receipt=json_set(receipt, '$.attestation', ?1), envelope=?2",
            (digest(&alternative), &alternative),
        )?;
        let audit = crate::audit_publisher_builds(&f.root, "canopa")?;
        assert_eq!(audit.verified, 0);
        assert_eq!(audit.failures.len(), 1);
        assert!(
            audit.failures[0]
                .contains("archived build attestation differs from the signed catalog")
        );
        db.execute(
            "UPDATE build_receipts SET receipt=json_set(receipt, '$.attestation', ?1), envelope=?2",
            (digest(&envelope), &envelope),
        )?;
        fs::rename(&offline, &f.repo)?;
    }
    Ok(())
}

#[test]
fn signed_build_trust_checks_retained_companion_packages() -> Result<()> {
    let f = Fixture::new()?;
    let seed = Fixture::new()?;
    let builder = Fixture::new()?;
    seed.package_as(
        "seed",
        "desktop/compositor",
        "1.0.0-0",
        seed.file(COMPOSITOR, "guarded", "0755", "")?,
    )?;
    let seed_pointer = seed.repo.join("current");
    let initial = fs::read_to_string(&seed_pointer)?;
    let (_, envelope) = attach_evidence(&seed, initial.trim(), &builder)?;
    let local = "library/local";
    seed.package_as("seed", local, "1.0.0-0", String::new())?;
    let pin = fs::read_to_string(seed_pointer)?;
    let pin = pin.trim();
    f.package(
        "desktop/session",
        "1.0.0-0",
        "    require \"desktop/compositor\"\n".to_owned(),
    )?;

    let publish_profile = |build_trust: &str| -> Result<String> {
        fs::write(
            f.repo.join("publish-policy.kdl"),
            format!(
                r#"schema-version 1 kind="publish-policy"
publisher "canopa" {{
    builder-key "builder.der"
    profile "desktop" architecture="x86_64" {{
        package "desktop/session"
        publisher "seed" catalog="{pin}" {{
            repository {:?}
            trust-key {:?}
            {build_trust}
        }}
    }}
}}
"#,
                seed.repo, seed.public,
            ),
        )?;
        let release = f._dir.path().join("refresh.kdl");
        let base = fs::read_to_string(f.repo.join("current"))?;
        fs::write(
            &release,
            format!(
                "schema-version 1 kind=\"publish-release\"\nrelease \"canopa\" base={:?} refresh-profiles=#true\n",
                base.trim(),
            ),
        )?;
        publish_release(&f.repo, &f.private, &release)
    };
    let first = publish_profile("")?;
    let config = f._dir.path().join("image.kdl");
    fs::write(
        &config,
        format!(
            r#"schema-version 1 kind="image-config"
architecture "x86_64"
release-profile "desktop"
publisher "canopa" {{
    repository {:?}
    trust-key {:?}
}}
publisher "seed" {{
    repository {:?}
    trust-key {:?}
}}
"#,
            f.repo, f.public, seed.repo, seed.public,
        ),
    )?;
    create_configured_image(&f.root, &config)?;
    apply(&f.root, &plan(&f.root, Request::InstallProfile)?)?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec![local.to_owned()],
        },
    )?;
    apply(&f.root, &install)?;

    let second = publish_profile(&format!(
        "build-trust {{ builder-key {:?}; }}",
        builder.public
    ))?;
    assert_ne!(first, second);
    let error = plan(&f.root, Request::Update).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("checking retained cps://seed/library/local@1.0.0-0")
            && message.contains("no build attestation"),
        "{message}",
    );

    let remove = plan(
        &f.root,
        Request::Remove {
            packages: vec![local.to_owned()],
        },
    )?;
    apply(&f.root, &remove)?;
    let upgrade = plan(&f.root, Request::Update)?;
    assert_eq!(upgrade.catalogs["canopa"], second);
    fetch_plan(&f.root, &upgrade)?;
    let staged = f._dir.path().join("staged");
    crate::stage_boot_environment(&f.root, &staged, &upgrade)?;
    assert!(crate::audit_publisher_builds(&f.root, "seed").is_err());
    apply(&f.root, &upgrade)?;

    let attestation = seed.repo.join("attestations").join(digest(&envelope));
    fs::remove_file(&attestation)?;
    let followup = plan(&f.root, Request::Update)?;
    fetch_plan(&f.root, &followup)?;
    apply(&f.root, &followup)?;

    {
        let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
        // The same valid DSSE envelope with different bytes has a different
        // digest and must not stand in for the catalog's admitted object.
        let mut alternative = envelope.clone();
        alternative.push(b'\n');
        assert_eq!(
            db.execute(
                "UPDATE build_receipts SET receipt=json_set(receipt, '$.attestation', ?1), envelope=?2",
                (digest(&alternative), alternative),
            )?,
            1
        );
    }
    let error = plan(&f.root, Request::Update).unwrap_err();
    assert!(
        format!("{error:#}").contains("reading build attestation"),
        "{error:#}"
    );
    fs::write(&attestation, envelope)?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    let interrupted = f._dir.path().join("interrupted");
    create_configured_image(&interrupted, &config)?;
    let install = plan(&interrupted, Request::InstallProfile)?;
    interrupt_apply_at(
        &interrupted,
        &f._dir.path().join("pending-profile.json"),
        &install,
        COMPOSITOR,
    )?;

    fs::remove_file(&attestation)?;
    plan(&f.root, Request::Update)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "guarded");

    let old_catalog = crate::repo::catalog_bytes(&f.repo, &first)?;
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    fs::rename(&seed.repo, seed.repo.with_extension("offline"))?;
    apply(&interrupted, &install)?;
    for root in [&f.root, &staged, &interrupted] {
        let audit = crate::audit_publisher_builds(root, "seed")?;
        assert_eq!(audit.verified, 1);
        assert!(audit.failures.is_empty(), "{:?}", audit.failures);
    }
    let error = plan(
        &interrupted,
        Request::Remove {
            packages: vec![
                "desktop/session".to_owned(),
                "desktop/compositor".to_owned(),
            ],
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("protected by the installed signed profile"));
    let audit = crate::audit_publisher_builds(&interrupted, "seed")?;
    assert_eq!(audit.verified, 1);
    assert!(audit.failures.is_empty(), "{:?}", audit.failures);

    let other_builder = Fixture::new()?;
    let local_policy = f._dir.path().join("local-trust.kdl");
    fs::write(
        &local_policy,
        format!(
            "schema-version 1 kind=\"build-trust\"\nbuild-trust {{ builder-key {:?}; }}\n",
            other_builder.public,
        ),
    )?;
    crate::set_publisher_build_trust(&staged, "seed", &local_policy, true)?;
    let audit = crate::audit_publisher_builds(&staged, "seed")?;
    assert_eq!(audit.verified, 0);
    assert_eq!(audit.failures.len(), 1);

    let db = rusqlite::Connection::open(f.root.join("var/lib/cps/image/state.sqlite"))?;
    assert_eq!(
        db.execute(
            "UPDATE catalog_archives SET catalog=?1 WHERE publisher='canopa'",
            [old_catalog],
        )?,
        1,
    );
    let Err(error) = crate::audit_publisher_builds(&f.root, "seed") else {
        panic!("audit accepted a different signed profile catalog");
    };
    assert!(format!("{error:#}").contains("archived catalog digest mismatch"));
    Ok(())
}

#[test]
fn tuf_cache_rejects_corruption_replay_and_expiry() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let first = work.join("first");
    let second = work.join("second");
    let expired = work.join("expired");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let refresh = |mirror: &Path| {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    sign_release(&f, &first, 1, fresh, &anchor, &tuf_key)?;
    let initial = refresh(&first)?;
    create_image(
        &f.root,
        &cache,
        "canopa",
        &f.public,
        "x86_64",
        BTreeMap::new(),
    )?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".to_owned()],
        },
    )?;

    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "new", "0755", "")?,
    )?;
    let (id, envelope) = attach_evidence(&f, &initial, &Fixture::new()?)?;
    sign_release(&f, &second, 2, fresh, &anchor, &tuf_key)?;
    let blob = digest(b"new");
    let target = second.join("targets").join(format!("{blob}.blobs-{blob}"));
    fs::write(&target, b"bad")?;
    let corrupt = refresh(&second).unwrap_err();
    assert!(
        format!("{corrupt:#}").contains("Hash mismatch"),
        "{corrupt:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);
    assert!(!cache.join("blobs").join(&blob).exists());

    let replay = refresh(&first).unwrap_err();
    assert!(
        matches!(
            replay.downcast_ref::<TufError>(),
            Some(TufError::OlderMetadata {
                role: RoleType::Timestamp,
                current_version: 2,
                new_version: 1,
                ..
            })
        ),
        "{replay:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);

    fs::write(&target, b"new")?;
    let hash = digest(&envelope);
    let evidence = second
        .join("targets")
        .join(format!("{hash}.attestations-{hash}"));
    fs::write(&evidence, vec![b'!'; envelope.len()])?;
    let corrupt = refresh(&second).unwrap_err();
    assert!(
        format!("{corrupt:#}").contains("attestations/"),
        "{corrupt:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), initial);
    fs::write(&evidence, &envelope)?;
    let upgraded = refresh(&second)?;
    assert_ne!(upgraded, initial);
    assert_eq!(refresh(&second)?, upgraded);

    // A failed repair leaves the cache untouched. Correct targets can replace
    // locally corrupted content even when the release pointer stays the same.
    let cached_blob = cache.join("blobs").join(&blob);
    fs::write(&cached_blob, b"bad")?;
    fs::write(&target, b"old")?;
    let corrupt = refresh(&second).unwrap_err();
    assert!(format!("{corrupt:#}").contains("Hash mismatch"));
    assert_eq!(fs::read(&cached_blob)?, b"bad");
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), upgraded);
    fs::write(&target, b"new")?;
    let cached_catalog = cache.join("catalogs").join(&upgraded);
    fs::write(&cached_catalog, b"truncated catalog")?;
    assert_eq!(refresh(&second)?, upgraded);
    assert_eq!(fs::read(&cached_blob)?, b"new");
    assert_eq!(digest(&fs::read(&cached_catalog)?), upgraded);
    assert_eq!(
        read_attestation(&cache, "canopa", &f.public, &id)?,
        envelope
    );

    // The earlier plan still pins a verified catalog after the cache advances.
    apply(&f.root, &install)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "old");
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "new");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));

    let past = Timestamp::now().checked_sub(jiff::Span::new().hours(24))?;
    sign_release(&f, &expired, 3, past, &anchor, &tuf_key)?;
    let freeze = refresh(&expired).unwrap_err();
    assert!(
        matches!(
            freeze.downcast_ref::<TufError>(),
            Some(TufError::ExpiredMetadata { .. })
        ),
        "{freeze:#}"
    );
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), upgraded);
    assert!(publish(&cache, &f.proto, &f.recipe, &f.private).is_err());

    let rotated = work.join("rotated");
    let next_root = work.join("next-root.json");
    let next_key = work.join("next-key.der");
    sign_root(&next_root, &next_key, Some(&tuf_key))?;
    sign_release(&f, &rotated, 3, fresh, &next_root, &next_key)?;
    let timestamp = rotated.join("metadata/timestamp.json");
    let timestamp_bytes = fs::read(&timestamp)?;
    fs::remove_file(&timestamp)?;
    assert!(refresh(&rotated).is_err());
    assert!(refresh(&second).is_err()); // Old timestamp key was revoked.
    fs::remove_file(rotated.join("metadata/2.root.json"))?;
    fs::write(&timestamp, timestamp_bytes)?;
    assert_eq!(refresh(&rotated)?, upgraded); // The client retained the rotated root.

    let origin = cache.join(".tuf/origin");
    let current = fs::read(cache.join("current"))?;
    let checkpoint = fs::read(cache.join(".tuf/trusted/timestamp.json"))?;
    let record = fs::read_to_string(&origin)?;
    let unversioned = record.strip_prefix("cps mirror cache v1\n").unwrap();
    let wrong_version = record.replacen("v1", "v2", 1);
    for invalid in [unversioned, &wrong_version] {
        fs::write(&origin, invalid)?;
        let error = refresh(&rotated).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported TUF cache format"));
        let error = mirror_release::prune_payloads(
            &cache,
            "canopa",
            &f.public,
            mirror_release::CacheRetention::default(),
            true,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("unsupported TUF cache format"));
        assert_eq!(fs::read_to_string(&origin)?, invalid);
        assert_eq!(fs::read(cache.join("current"))?, current);
        assert_eq!(
            fs::read(cache.join(".tuf/trusted/timestamp.json"))?,
            checkpoint
        );
    }
    fs::remove_file(&origin)?;
    let error = refresh(&rotated).unwrap_err();
    assert!(format!("{error:#}").contains("missing its trust configuration"));
    assert!(!origin.exists());
    fs::write(&origin, record)?;
    assert_eq!(refresh(&rotated)?, upgraded);
    Ok(())
}

#[test]
fn tuf_withdrawal_keeps_cached_recovery_and_rejects_resurrection() -> Result<()> {
    use crate::query::{Scope, list_packages, package_manifest};

    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let refresh = |cache: &Path, mirror: &Path| {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let create = |root: &Path, cache: &Path| {
        create_image(root, cache, "canopa", &f.public, "x86_64", BTreeMap::new())
    };
    let install = |root: &Path, package: &str| {
        plan(
            root,
            Request::Install {
                packages: vec![package.to_owned()],
            },
        )
    };
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "previous", "0755", "")?,
    )?;
    let previous = fs::read_to_string(f.repo.join("current"))?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "withdrawn", "0755", "")?,
    )?;
    let (id, envelope) = attach_evidence(&f, previous.trim(), &Fixture::new()?)?;
    let original = work.join("original");
    let resurrected = work.join("resurrected");
    sign_release(&f, &original, 1, fresh, &anchor, &tuf_key)?;
    sign_release(&f, &resurrected, 3, fresh, &anchor, &tuf_key)?;
    let original_hash = refresh(&cache, &original)?;
    let key = read_key(&f.public)?;
    let published = snapshot(&cache, "canopa", &key, Some(&original_hash))?;
    let retired = published
        .packages
        .iter()
        .find(|pkg| pkg.manifest.id() == id)
        .unwrap();
    create(&f.root, &cache)?;
    let saved = install(&f.root, &id)?;

    for invalid in [
        "desktop/compositor@2.0.0-0",
        "cps://extras/desktop/compositor@2.0.0-0",
        "cps://canopa/desktop/compositor@3.0.0-0",
    ] {
        assert!(withdraw_package(&f.repo, &f.private, invalid).is_err());
    }
    assert!(withdraw_package(&f.repo, &Fixture::new()?.private, &id).is_err());
    assert!(withdraw_package(&cache, &f.private, &id).is_err());
    assert_eq!(
        fs::read_to_string(f.repo.join("current"))?.trim(),
        original_hash
    );

    withdraw_package(&f.repo, &f.private, &id)?;
    let withdrawn_pointer = fs::read_to_string(f.repo.join("current"))?;
    withdraw_package(&f.repo, &f.private, &id)?;
    let error = publish(&f.repo, &f.proto, &f.recipe, &f.private).unwrap_err();
    assert!(error.to_string().contains("was withdrawn"));
    assert_eq!(
        fs::read_to_string(f.repo.join("current"))?,
        withdrawn_pointer
    );
    assert_eq!(
        read_attestation(&f.repo, "canopa", &f.public, &id)?,
        envelope
    );

    let withdrawn = work.join("withdrawn");
    sign_release(&f, &withdrawn, 2, fresh, &anchor, &tuf_key)?;
    let withdrawn_hash = refresh(&cache, &withdrawn)?;
    assert_eq!(withdrawn_hash, withdrawn_pointer.trim());
    assert_eq!(refresh(&cache, &withdrawn)?, withdrawn_hash);
    assert_eq!(
        list_withdrawals(&cache, "canopa", &f.public)?,
        BTreeMap::from([(id.clone(), retired.sha256.clone())])
    );
    assert_eq!(
        read_attestation(&cache, "canopa", &f.public, &id)?,
        envelope
    );

    let new_cache = work.join("new-cache");
    assert_eq!(refresh(&new_cache, &withdrawn)?, withdrawn_hash);
    assert!(!new_cache.join("manifests").join(&retired.sha256).exists());
    assert!(!new_cache.join("blobs").join(digest(b"withdrawn")).exists());
    assert!(
        !new_cache
            .join("attestations")
            .join(digest(&envelope))
            .exists()
    );
    let new_root = work.join("new-root");
    create(&new_root, &new_cache)?;
    assert_eq!(
        list_packages(&new_root, Scope::Available)?[0].id,
        "cps://canopa/desktop/compositor@1.0.0-0"
    );
    assert!(package_manifest(&new_root, &id, true).is_err());
    let error = install(&new_root, &id).unwrap_err();
    assert!(error.to_string().contains("was withdrawn"));
    apply(&new_root, &install(&new_root, "desktop/compositor")?)?;
    assert_eq!(fs::read_to_string(new_root.join(COMPOSITOR))?, "previous");

    apply(&f.root, &saved)?;
    let error = install(&f.root, "desktop/compositor@2.0.0-0").unwrap_err();
    assert!(error.to_string().contains("was withdrawn"));
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "withdrawn");
    fs::write(f.root.join(COMPOSITOR), "damaged")?;
    let repair = plan(&f.root, Request::Repair { packages: vec![] })?;
    apply(&f.root, &repair)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "withdrawn");

    let error = refresh(&cache, &resurrected).unwrap_err();
    assert!(error.to_string().contains("catalog rollback or fork"));
    assert_eq!(
        fs::read_to_string(cache.join("current"))?.trim(),
        withdrawn_hash
    );

    f.package(
        "desktop/compositor",
        "3.0.0-0",
        f.file(COMPOSITOR, "replacement", "0755", "")?,
    )?;
    let replacement = work.join("replacement");
    sign_release(&f, &replacement, 4, fresh, &anchor, &tuf_key)?;
    refresh(&cache, &replacement)?;
    apply(&f.root, &plan(&f.root, Request::Update)?)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "replacement");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));
    Ok(())
}

#[test]
fn tuf_cache_remembers_a_newer_publisher_catalog_after_a_failed_download() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    let cache = work.join("cache");
    let refresh = |name: &str| {
        let mirror = work.join(name);
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let sign =
        |name: &str, version| sign_release(&f, &work.join(name), version, fresh, &anchor, &tuf_key);

    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "old", "0755", "")?,
    )?;
    let current = f.repo.join("current");
    let first = fs::read_to_string(&current)?;
    sign("first", 1)?;
    sign("replay", 3)?; // Fresh TUF metadata, original CPS publisher catalog.

    f.package("test/fork", "1.0.0-0", String::new())?;
    sign("fork", 4)?; // A different publisher-signed catalog at sequence 2.
    fs::write(&current, &first)?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, "new", "0755", "")?,
    )?;
    let second = fs::read_to_string(&current)?;
    publish(&f.repo, &f.proto, &f.recipe, &f.private)?;
    assert_eq!(fs::read_to_string(&current)?, second);
    sign("second", 2)?;

    assert_eq!(refresh("first")?, first.trim());
    let checkpoint = cache.join(".tuf/catalog");
    fs::remove_file(&checkpoint)?;
    let error = refresh("second").unwrap_err();
    assert!(format!("{error:#}").contains("missing its publisher catalog checkpoint"));
    assert!(!checkpoint.exists());
    fs::write(&checkpoint, &first)?;
    let blob = digest(b"new");
    fs::write(
        work.join("second/targets")
            .join(format!("{blob}.blobs-{blob}")),
        b"bad",
    )?;
    let corrupt = refresh("second").unwrap_err();
    assert!(format!("{corrupt:#}").contains("Hash mismatch"));
    assert_eq!(fs::read_to_string(&checkpoint)?, second);
    assert_eq!(fs::read_to_string(cache.join("current"))?, first);

    for rejected in ["replay", "fork"] {
        let error = refresh(rejected).unwrap_err();
        assert!(error.to_string().contains("catalog rollback or fork"));
        assert_eq!(fs::read_to_string(&checkpoint)?, second);
        assert_eq!(fs::read_to_string(cache.join("current"))?, first);
    }

    sign("restored", 5)?;
    assert_eq!(refresh("restored")?, second.trim());
    assert_eq!(refresh("restored")?, second.trim());
    assert_eq!(fs::read_to_string(cache.join("current"))?, second);
    Ok(())
}

#[test]
fn tuf_delta_fetch_reconstructs_from_installed_files_and_falls_back() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let key = work.join("tuf-key.der");
    let cache = work.join("cache");
    let second_image = work.join("fallback-image");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_root(&anchor, &key, None)?;
    let old = "old data in a binary file\n".repeat(4096);
    let new = format!("inserted prefix!{old}");
    let old_hash = digest(old.as_bytes());
    let new_hash = digest(new.as_bytes());
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, &old, "0755", "")?,
    )?;
    let previous = fs::read_to_string(f.repo.join("current"))?
        .trim()
        .to_owned();
    let mirror = work.join("base-mirror");
    sign_release(&f, &mirror, 1, fresh, &anchor, &key)?;
    let sync = |mirror: &Path| -> Result<String> {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync_index(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    sync(&mirror)?;
    for root in [&f.root, &second_image] {
        create_image(root, &cache, "canopa", &f.public, "x86_64", BTreeMap::new())?;
        let install = plan(
            root,
            Request::Install {
                packages: vec!["desktop/compositor".into()],
            },
        )?;
        fetch_plan(root, &install)?;
        apply(root, &install)?;
    }
    fs::remove_file(cache.join("blobs").join(&old_hash))?;
    f.package(
        "desktop/compositor",
        "2.0.0-0",
        f.file(COMPOSITOR, &new, "0755", "")?,
    )?;
    let export = work.join("delta-export");
    mirror_release::export_with_deltas(
        &f.repo,
        "canopa",
        &f.public,
        &export,
        &[],
        false,
        &[previous],
    )?;
    let delta_name = format!("payload-delta-v1-{old_hash}-{new_hash}");
    let mut patch = fs::read(export.join(&delta_name))?;
    assert!(patch.len() < new.len() / 8);
    let updated = work.join("updated-mirror");
    sign_export(&export, &updated, 2, fresh, &anchor, &key)?;
    fs::remove_file(signed_compressed_payload(&export, &updated, &new_hash)?)?;
    sync(&updated)?;
    let upgrade = plan(&f.root, Request::Update)?;
    fetch_plan(&f.root, &upgrade)?;
    apply(&f.root, &upgrade)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, new);

    // The second patch is valid TUF data and produces the right length, but
    // it reconstructs different bytes. CPS must reject it before publishing.
    fs::remove_file(cache.join("blobs").join(&new_hash))?;
    let literal = patch
        .windows(b"inserted prefix!".len())
        .position(|bytes| bytes == b"inserted prefix!")
        .expect("inserted literal");
    patch[literal] ^= 1;
    fs::write(export.join(delta_name), patch)?;
    let fallback = work.join("fallback-mirror");
    sign_export(&export, &fallback, 3, fresh, &anchor, &key)?;
    let full = signed_compressed_payload(&export, &fallback, &new_hash)?;
    let offline = work.join("full-offline");
    fs::rename(&full, &offline)?;
    sync(&fallback)?;
    let upgrade = plan(&second_image, Request::Update)?;
    assert!(fetch_plan(&second_image, &upgrade).is_err());
    assert!(!cache.join("blobs").join(&new_hash).exists());
    assert_eq!(fs::read_to_string(second_image.join(COMPOSITOR))?, old);
    fs::rename(&offline, &full)?;
    fetch_plan(&second_image, &upgrade)?;
    apply(&second_image, &upgrade)?;
    assert_eq!(fs::read_to_string(second_image.join(COMPOSITOR))?, new);
    Ok(())
}

#[test]
fn tuf_delta_fetch_upgrades_files_larger_than_64_mib() -> Result<()> {
    use std::{fs::File, os::unix::fs::FileExt};

    let f = Fixture::new()?;
    let work = f._dir.path();
    let size = 65 * 1024 * 1024;
    let action = f.file(COMPOSITOR, "base", "0755", "")?;
    let path = f.proto.join(COMPOSITOR);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(size)?;
    let old_hash = crate::model::digest_reader(File::open(&path)?)?;
    f.package("desktop/compositor", "1.0.0-0", action.clone())?;
    let previous = fs::read_to_string(f.repo.join("current"))?
        .trim()
        .to_owned();
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    apply(&f.root, &install)?;

    let file = fs::OpenOptions::new().write(true).open(&path)?;
    file.write_all_at(b"updated bytes", size / 2)?;
    let new_hash = crate::model::digest_reader(File::open(&path)?)?;
    f.package("desktop/compositor", "2.0.0-0", action)?;

    let export = work.join("export");
    mirror_release::export_with_deltas(
        &f.repo,
        "canopa",
        &f.public,
        &export,
        &[],
        false,
        &[previous],
    )?;
    let delta_name = format!("payload-delta-v1-{old_hash}-{new_hash}");
    assert!(fs::metadata(export.join(delta_name))?.len() < 1024);
    let anchor = work.join("root.json");
    let key = work.join("tuf-key.der");
    let mirror = work.join("mirror");
    let cache = work.join("cache");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_root(&anchor, &key, None)?;
    sign_export(&export, &mirror, 1, fresh, &anchor, &key)?;
    fs::remove_file(signed_compressed_payload(&export, &mirror, &new_hash)?)?;
    let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
    let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
    mirror_release::sync_index(
        &cache,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "canopa",
        &f.public,
    )?;
    crate::set_publisher_repositories(&f.root, &[("canopa".to_owned(), cache)])?;
    let upgrade = plan(&f.root, Request::Update)?;
    fetch_plan(&f.root, &upgrade)?;
    apply(&f.root, &upgrade)?;
    assert_eq!(
        crate::model::digest_reader(File::open(f.root.join(COMPOSITOR))?)?,
        new_hash
    );
    Ok(())
}

#[test]
fn tuf_full_sync_checks_compressed_payloads_before_caching() -> Result<()> {
    let f = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let key = work.join("tuf-key.der");
    let export = work.join("export");
    let cache = work.join("cache");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;
    sign_root(&anchor, &key, None)?;
    let program = "compressible data\n".repeat(160_000);
    let hash = digest(program.as_bytes());
    let small_hash = digest(b"short");
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, &program, "0755", "")? + &f.file(MANPAGE, "short", "0644", "")?,
    )?;
    let catalog = mirror_release::export(&f.repo, "canopa", &f.public, &export)?;
    assert!(!export.join(format!("blobs-{hash}")).exists());
    assert!(!export.join(format!("blobs-{small_hash}.zst")).exists());
    assert_eq!(
        fs::read(export.join(format!("blobs-{small_hash}")))?,
        b"short"
    );
    let compressed_target = export.join(format!("blobs-{hash}.zst"));
    let compressed = fs::read(&compressed_target)?;
    assert!(compressed.len() < program.len() / 10);

    let sync = |mirror: &Path| {
        let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
        let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
        mirror_release::sync(
            &cache,
            &anchor,
            metadata.as_str(),
            targets.as_str(),
            "canopa",
            &f.public,
        )
    };
    let good = work.join("good");
    sign_export(&export, &good, 1, fresh, &anchor, &key)?;
    let signed_target = signed_compressed_payload(&export, &good, &hash)?;
    let cached = cache.join("blobs").join(&hash);
    let mut damaged = compressed.clone();
    damaged[0] ^= 1;
    fs::write(&signed_target, damaged)?;
    let error = sync(&good).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"), "{error:#}");
    assert!(!cached.exists());
    fs::write(&signed_target, &compressed)?;
    assert_eq!(sync(&good)?, catalog);
    assert_eq!(fs::read(&cached)?, program.as_bytes());
    assert_eq!(fs::read(cache.join("blobs").join(small_hash))?, b"short");

    // These compressed targets have valid TUF signatures. The manifest still
    // controls which bytes may be written to the cache.
    fs::write(&cached, b"damaged")?;
    let wrong = vec![b'x'; program.len()];
    let too_long = vec![b'x'; program.len() + 1];
    for (version, name, bytes, expected) in [
        (
            2,
            "wrong",
            zstd::stream::encode_all(wrong.as_slice(), 3)?,
            "decompressed payload digest mismatch",
        ),
        (
            3,
            "oversized",
            zstd::stream::encode_all(too_long.as_slice(), 3)?,
            "decompressed payload length mismatch",
        ),
        (
            4,
            "truncated",
            compressed[..compressed.len() - 1].to_vec(),
            "decoding",
        ),
    ] {
        fs::write(&compressed_target, bytes)?;
        let mirror = work.join(name);
        sign_export(&export, &mirror, version, fresh, &anchor, &key)?;
        let error = sync(&mirror).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(fs::read(&cached)?, b"damaged");
    }

    fs::write(&compressed_target, &compressed)?;
    let repaired = work.join("repaired");
    sign_export(&export, &repaired, 5, fresh, &anchor, &key)?;
    assert_eq!(sync(&repaired)?, catalog);
    assert_eq!(fs::read(&cached)?, program.as_bytes());
    Ok(())
}

#[test]
fn tuf_index_and_plan_fetch_support_offline_apply_and_repair() -> Result<()> {
    let f = Fixture::new()?;
    let extras = Fixture::new()?;
    let work = f._dir.path();
    let anchor = work.join("root.json");
    let tuf_key = work.join("tuf-key.der");
    sign_root(&anchor, &tuf_key, None)?;
    let mirror = work.join("mirror");
    let cache = work.join("cache");
    let extra_anchor = work.join("extras-root.json");
    let extra_tuf_key = work.join("extras-tuf-key.der");
    sign_root(&extra_anchor, &extra_tuf_key, None)?;
    let extra_mirror = work.join("extras-mirror");
    let extra_cache = work.join("extras-cache");
    let fresh = Timestamp::now().checked_add(jiff::Span::new().hours(24))?;

    extras.package_as(
        "extras",
        "library/libdisplay",
        "1.0.0-0",
        extras.file(LIBRARY, "library", "0644", "")?,
    )?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        f.file(COMPOSITOR, "program", "0755", "")?
            + &f.file(MANPAGE, "documentation", "0644", "facet=\"doc.man\"")?
            + &dependency("require", "library/libdisplay", "1.0.0-0"),
    )?;
    f.package(
        "desktop/unused",
        "1.0.0-0",
        f.file(PLUGIN, "unused", "0644", "")?,
    )?;
    sign_release(&f, &mirror, 1, fresh, &anchor, &tuf_key)?;
    sign_publisher_release(
        &extras,
        &extra_mirror,
        1,
        fresh,
        &extra_anchor,
        &extra_tuf_key,
        "extras",
    )?;
    let metadata = Url::from_directory_path(mirror.join("metadata")).unwrap();
    let targets = Url::from_directory_path(mirror.join("targets")).unwrap();
    let blob = |content: &[u8]| cache.join("blobs").join(digest(content));
    let target = |content: &[u8]| {
        let hash = digest(content);
        mirror.join("targets").join(format!("{hash}.blobs-{hash}"))
    };
    fs::remove_file(target(b"unused"))?;
    let hash = mirror_release::sync_index(
        &cache,
        &anchor,
        metadata.as_str(),
        targets.as_str(),
        "canopa",
        &f.public,
    )?;
    let extra_metadata = Url::from_directory_path(extra_mirror.join("metadata")).unwrap();
    let extra_targets = Url::from_directory_path(extra_mirror.join("targets")).unwrap();
    mirror_release::sync_index(
        &extra_cache,
        &extra_anchor,
        extra_metadata.as_str(),
        extra_targets.as_str(),
        "extras",
        &extras.public,
    )?;
    assert_eq!(fs::read_dir(cache.join("blobs"))?.count(), 0);
    assert_eq!(fs::read_dir(extra_cache.join("blobs"))?.count(), 0);
    create_image(
        &f.root,
        &cache,
        "canopa",
        &f.public,
        "x86_64",
        BTreeMap::from([("doc.man".into(), false)]),
    )?;
    crate::add_publisher(&f.root, "extras", &extra_cache, &extras.public)?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".to_owned()],
        },
    )?;
    assert!(apply(&f.root, &install).is_err());
    assert!(!f.root.join(COMPOSITOR).exists());

    let mut tampered = install.clone();
    tampered.writes.first_mut().unwrap().path = "usr/bin/other".to_owned();
    let error = fetch_plan(&f.root, &tampered).unwrap_err();
    assert!(error.to_string().contains("differs from the verified plan"));
    assert_eq!(fs::read_dir(cache.join("blobs"))?.count(), 0);

    fs::write(target(b"program"), b"corrupt")?;
    let error = fetch_plan(&f.root, &install).unwrap_err();
    assert!(format!("{error:#}").contains("Hash mismatch"));
    assert!(!blob(b"program").exists());
    assert!(!f.root.join(COMPOSITOR).exists());
    assert_eq!(fs::read_to_string(cache.join("current"))?.trim(), hash);
    fs::write(target(b"program"), b"program")?;
    assert_eq!(fetch_plan(&f.root, &install)?, 2);
    assert_eq!(fs::read_dir(cache.join("blobs"))?.count(), 1);
    assert_eq!(fs::read_dir(extra_cache.join("blobs"))?.count(), 1);
    assert!(!blob(b"unused").exists());

    let offline = work.join("offline");
    let extra_offline = work.join("extras-offline");
    fs::rename(&mirror, &offline)?;
    fs::rename(&extra_mirror, &extra_offline)?;
    assert_eq!(fetch_plan(&f.root, &install)?, 2);
    apply(&f.root, &install)?;
    assert_eq!(fs::read(f.root.join(COMPOSITOR))?, b"program");
    assert_eq!(fs::read(f.root.join(LIBRARY))?, b"library");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));

    fs::rename(&extra_offline, &extra_mirror)?;
    let replacement = work.join("extras-replacement");
    mirror_release::sync_index(
        &replacement,
        &extra_anchor,
        extra_metadata.as_str(),
        extra_targets.as_str(),
        "extras",
        &extras.public,
    )?;
    fs::rename(&extra_cache, work.join("old-extras-cache"))?;
    crate::set_publisher_repositories(&f.root, &[("extras".to_owned(), replacement.clone())])?;
    let library = replacement.join("blobs").join(digest(b"library"));
    assert!(!library.exists());
    fs::write(f.root.join(LIBRARY), b"damaged")?;
    let repair = plan(&f.root, Request::Repair { packages: vec![] })?;
    assert_eq!(fetch_plan(&f.root, &repair)?, 1);
    assert_eq!(fs::read(library)?, b"library");
    fs::rename(&extra_mirror, &extra_offline)?;
    apply(&f.root, &repair)?;
    assert_eq!(fs::read(f.root.join(LIBRARY))?, b"library");
    assert!(verify(&f.root)?.iter().all(|finding| !finding.is_error));

    fs::rename(&offline, &mirror)?;
    let enable = plan(
        &f.root,
        Request::ChangeFacet {
            facets: BTreeMap::from([("doc.man".into(), None)]),
            with_dependencies: false,
        },
    )?;
    assert!(!f.root.join(MANPAGE).exists());
    assert!(!blob(b"documentation").exists());
    assert_eq!(fetch_plan(&f.root, &enable)?, 1);
    fs::rename(&mirror, &offline)?;
    apply(&f.root, &enable)?;
    assert_eq!(fs::read(f.root.join(MANPAGE))?, b"documentation");
    assert!(crate::verify_pristine(&f.root)?.is_empty());
    Ok(())
}

fn object_count(repo: &Path, kind: &str) -> Result<usize> {
    Ok(fs::read_dir(repo.join(kind))?.count())
}

fn write_manifest(f: &Fixture, name: &str, version: &str, body: &str) -> Result<PathBuf> {
    let path = f
        ._dir
        .path()
        .join(format!("{}-{version}.kdl", name.replace('/', "-")));
    fs::write(
        &path,
        format!(
            "schema-version 1 kind=\"package-manifest\"\npackage {name:?} publisher=\"canopa\" version={version:?} {{\n{body}}}\n"
        ),
    )?;
    Ok(path)
}

#[test]
fn batched_publications_sign_once_and_rewrite_only_changed_shards() -> Result<()> {
    let f = Fixture::new()?;
    let names: Vec<_> = (0..48)
        .map(|number| format!("bulk/pkg-{number:02}"))
        .collect();
    let mut manifests = Vec::new();
    for name in &names {
        let file = f.file(&format!("usr/share/{name}"), name, "0644", "")?;
        manifests.push(write_manifest(&f, name, "1.0.0-0", &file)?);
    }
    let ids = crate::publish_batch(&f.repo, &f.proto, &manifests, &f.private)?;
    assert_eq!(ids.len(), names.len());
    let first = fs::read_to_string(f.repo.join("current"))?
        .trim()
        .to_owned();
    assert_eq!(object_count(&f.repo, "catalogs")?, 1);
    let shards = object_count(&f.repo, crate::repo::SHARD_OBJECTS)?;
    let key = read_key(&f.public)?;
    let catalog = crate::repo::read_catalog(&f.repo, &first, "canopa", &key)?;
    assert_eq!(catalog.sequence(), 1);
    assert!(shards > 1 && shards <= names.len());
    assert_eq!(crate::repo::catalog_shards(&f.repo, &first)?.len(), shards);

    // One new package rewrites one shard and the root, nothing else.
    f.package("bulk/late", "1.0.0-0", String::new())?;
    let second = fs::read_to_string(f.repo.join("current"))?
        .trim()
        .to_owned();
    assert_eq!(object_count(&f.repo, "catalogs")?, 2);
    assert_eq!(
        object_count(&f.repo, crate::repo::SHARD_OBJECTS)?,
        shards + 1
    );
    let before: BTreeSet<_> = crate::repo::catalog_shards(&f.repo, &first)?
        .into_iter()
        .collect();
    let after: BTreeSet<_> = crate::repo::catalog_shards(&f.repo, &second)?
        .into_iter()
        .collect();
    assert_eq!(after.difference(&before).count(), 1);
    assert!(before.difference(&after).count() <= 1);

    // Republishing identical manifests changes nothing; the old pin still reads.
    crate::publish_batch(&f.repo, &f.proto, &manifests, &f.private)?;
    assert_eq!(
        fs::read_to_string(f.repo.join("current"))?.trim(),
        second.as_str()
    );
    assert_eq!(
        snapshot(&f.repo, "canopa", &key, Some(&first))?
            .packages
            .len(),
        names.len()
    );
    assert_eq!(
        snapshot(&f.repo, "canopa", &key, None)?.packages.len(),
        names.len() + 1
    );

    // A batch is atomic: one bad manifest publishes none of them.
    let good = write_manifest(&f, "bulk/good", "1.0.0-0", "")?;
    let bad = write_manifest(&f, "bulk/pkg-00", "1.0.0-0", "    require \"bulk/good\"\n")?;
    let error = crate::publish_batch(&f.repo, &f.proto, &[good, bad], &f.private).unwrap_err();
    assert!(format!("{error:#}").contains("already published with different content"));
    assert_eq!(
        fs::read_to_string(f.repo.join("current"))?.trim(),
        second.as_str()
    );

    // A shard that does not match its root commitment is rejected.
    let shard = crate::repo::catalog_shards(&f.repo, &second)?.remove(0);
    let path = f.repo.join(crate::repo::SHARD_OBJECTS).join(&shard);
    let original = fs::read(&path)?;
    fs::write(&path, b"{}")?;
    let error = crate::repo::catalog_bytes(&f.repo, &second).unwrap_err();
    assert!(
        format!("{error:#}").contains("digest mismatch"),
        "{error:#}"
    );
    fs::write(&path, original)?;
    Ok(())
}

#[test]
fn planning_reads_manifests_only_for_the_selected_packages() -> Result<()> {
    let f = Fixture::new()?;
    let library = f.file(LIBRARY, "library", "0644", "")?;
    f.package("library/display", "1.0.0-0", library)?;
    f.package(
        "desktop/compositor",
        "1.0.0-0",
        format!(
            "    require \"library/display\"\n{}",
            f.file(COMPOSITOR, "compositor", "0755", "")?
        ),
    )?;
    for number in 0..16 {
        f.package(&format!("tools/unused-{number}"), "1.0.0-0", String::new())?;
    }
    let key = read_key(&f.public)?;
    let unused = snapshot(&f.repo, "canopa", &key, None)?
        .packages
        .into_iter()
        .filter(|package| package.manifest.name.starts_with("tools/"))
        .collect::<Vec<_>>();
    f.create()?;
    for package in &unused {
        fs::remove_file(f.repo.join("manifests").join(&package.sha256))?;
    }
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["desktop/compositor".into()],
        },
    )?;
    assert_eq!(install.package_changes().len(), 2, "{}", install.summary());
    apply(&f.root, &install)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "compositor");
    let error = plan(
        &f.root,
        Request::Install {
            packages: vec!["tools/unused-0".into()],
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("reading manifest for cps://canopa/tools/unused-0"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn withdrawal_pruning_keeps_pins_installed_packages_and_identities() -> Result<()> {
    let f = Fixture::new()?;
    f.package(
        "apps/tool",
        "1.0.0-0",
        f.file(COMPOSITOR, "one", "0755", "")?,
    )?;
    f.package(
        "apps/tool",
        "2.0.0-0",
        f.file(COMPOSITOR, "two", "0755", "")?,
    )?;
    f.package("apps/other", "1.0.0-0", String::new())?;
    f.create()?;
    let install = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/tool@1.0.0-0".into()],
        },
    )?;
    apply(&f.root, &install)?;
    let installed = fs::read_to_string(f.repo.join("current"))?
        .trim()
        .to_owned();
    withdraw_package(&f.repo, &f.private, "cps://canopa/apps/tool@1.0.0-0")?;
    withdraw_package(&f.repo, &f.private, "cps://canopa/apps/other@1.0.0-0")?;
    let withdrawn = fs::read_to_string(f.repo.join("current"))?
        .trim()
        .to_owned();
    assert_eq!(list_withdrawals(&f.repo, "canopa", &f.public)?.len(), 2);
    let key = read_key(&f.public)?;
    let sequence = crate::repo::read_catalog(&f.repo, &withdrawn, "canopa", &key)?.sequence();

    let error =
        crate::prune_withdrawals(&f.repo, "canopa", &f.private, &installed, sequence).unwrap_err();
    assert!(format!("{error:#}").contains("pruning base is stale"));
    let error = crate::prune_withdrawals(&f.repo, "canopa", &f.private, &withdrawn, sequence + 1)
        .unwrap_err();
    assert!(format!("{error:#}").contains("beyond the current catalog sequence"));
    // The first withdrawal happened at sequence - 1; prune only that one.
    let (pruned, count) =
        crate::prune_withdrawals(&f.repo, "canopa", &f.private, &withdrawn, sequence - 1)?;
    assert_eq!(count, 1);
    assert_eq!(
        list_withdrawals(&f.repo, "canopa", &f.public)?
            .into_keys()
            .collect::<Vec<_>>(),
        ["cps://canopa/apps/other@1.0.0-0"]
    );
    let error =
        crate::prune_withdrawals(&f.repo, "canopa", &f.private, &pruned, sequence - 2).unwrap_err();
    assert!(format!("{error:#}").contains("must not move back"));

    // Pruned identities stay reserved and stay admitted for installed images.
    let error = f
        .package(
            "apps/tool",
            "1.0.0-0",
            f.file(COMPOSITOR, "new", "0755", "")?,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("was withdrawn and cannot be republished"));
    let previous = crate::repo::read_catalog(&f.repo, &installed, "canopa", &key)?;
    let next = crate::repo::read_catalog(&f.repo, &pruned, "canopa", &key)?;
    previous.check_successor(&next, &installed)?;
    let refresh = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/tool".into()],
        },
    )?;
    assert!(
        refresh.summary().contains("2.0.0-0"),
        "{}",
        refresh.summary()
    );
    let keep = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/tool@1.0.0-0".into()],
        },
    )
    .unwrap_err();
    assert!(format!("{keep:#}").contains("was withdrawn"), "{keep:#}");
    let mark = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/other".into()],
        },
    );
    assert!(mark.is_err());

    // A transaction on the pruned catalog keeps the pruned package installed
    // and the image still verifies its admission offline.
    f.package("apps/new", "1.0.0-0", String::new())?;
    let added = plan(
        &f.root,
        Request::Install {
            packages: vec!["apps/new".into()],
        },
    )?;
    apply(&f.root, &added)?;
    assert_eq!(fs::read_to_string(f.root.join(COMPOSITOR))?, "one");
    fs::rename(&f.repo, f.repo.with_extension("offline"))?;
    assert!(verify(&f.root)?.is_empty());
    fs::rename(f.repo.with_extension("offline"), &f.repo)?;

    // The pinned catalog of the original install still reads in full.
    let pinned = snapshot(&f.repo, "canopa", &key, Some(&installed))?;
    assert_eq!(pinned.packages.len(), 3);
    Ok(())
}

#[test]
fn file_indexes_follow_publications_one_shard_at_a_time() -> Result<()> {
    use crate::query::search_files;

    let f = Fixture::new()?;
    for number in 0..24 {
        let path = format!("usr/share/indexed/file-{number:02}");
        f.package(
            &format!("indexed/pkg-{number:02}"),
            "1.0.0-0",
            f.file(&path, "x", "0644", "")?,
        )?;
    }
    f.create()?;
    crate::enable_file_index(&f.repo, "canopa", &f.private)?;
    let indexes = object_count(&f.repo, "file-indexes")?;
    let current = fs::read_to_string(f.repo.join("current"))?;
    assert_eq!(
        indexes,
        crate::repo::catalog_shards(&f.repo, current.trim())?.len(),
        "one file index per shard of the indexed catalog"
    );
    f.package(
        "indexed/late",
        "1.0.0-0",
        f.file("usr/share/indexed/late-file", "y", "0644", "")?,
    )?;
    assert_eq!(object_count(&f.repo, "file-indexes")?, indexes + 1);
    let found = search_files(&f.root, "late-file")?;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].package, "cps://canopa/indexed/late@1.0.0-0");
    assert_eq!(search_files(&f.root, "file-07")?.len(), 1);
    Ok(())
}
