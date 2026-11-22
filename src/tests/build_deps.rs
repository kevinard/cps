use std::collections::BTreeSet;

use super::*;
use crate::{
    attestation::{self, PinnedPackage, Toolchain},
    image::{Image, root::Root},
    kdl_format::build,
    model::{FileDigest, PackageSpec},
    repo::SigningKey,
};

fn archive(
    fixture: &Fixture,
    builder: &Fixture,
    outputs: &[(&str, &str)],
    dependencies: &[&str],
    architecture: &str,
) -> Result<String> {
    let outputs = outputs
        .iter()
        .map(|(name, version)| {
            (
                *name,
                *version,
                "    require \"runtime/not-needed-for-building\"\n",
            )
        })
        .collect::<Vec<_>>();
    publish(fixture, builder, &outputs, dependencies, architecture, true)
}

fn package(
    fixture: &Fixture,
    builder: &Fixture,
    name: &str,
    version: &str,
    actions: String,
) -> Result<()> {
    publish(
        fixture,
        builder,
        &[(name, version, &actions)],
        &["build/bootstrap"],
        "x86_64",
        true,
    )?;
    Ok(())
}

fn publish(
    fixture: &Fixture,
    builder: &Fixture,
    outputs: &[(&str, &str, &str)],
    dependencies: &[&str],
    architecture: &str,
    archive_inputs: bool,
) -> Result<String> {
    let bundle = tempfile::tempdir_in(fixture._dir.path())?;
    fs::create_dir(bundle.path().join("proto"))?;
    fs::create_dir(bundle.path().join("templates"))?;
    let mut templates = Vec::new();
    let mut publishers = BTreeSet::new();
    let mut packages = BTreeMap::new();
    for dependency in dependencies {
        let spec = PackageSpec::parse(dependency)?;
        let publisher = spec.publisher.unwrap_or("canopa");
        publishers.insert(publisher);
        packages.insert(
            spec.name.to_owned(),
            PinnedPackage {
                publisher: publisher.to_owned(),
                version: spec.version.unwrap_or("1.0.0-0".parse()?),
                sha256: digest(dependency.as_bytes()),
            },
        );
    }
    let pins = publishers
        .into_iter()
        .map(|publisher| {
            format!(
                "        publisher {publisher:?} catalog-sha256={:?} key-sha256={:?}\n",
                digest(b"historical toolchain catalog"),
                digest(b"historical toolchain key")
            )
        })
        .collect::<String>();
    let requests = dependencies
        .iter()
        .map(|dependency| format!("        package {dependency:?}\n"))
        .collect::<String>();
    let mut manifest_nodes = String::new();
    let mut approvals = String::new();
    for (index, (name, version, actions)) in outputs.iter().enumerate() {
        let path = format!("template-{index}.kdl");
        let manifest = format!(
            "schema-version 1 kind=\"package-manifest\"\npackage {name:?} publisher=\"canopa\" version={version:?} {{\n{actions}}}\n"
        );
        manifest_nodes.push_str(&format!(
            "    manifest {path:?} sha256={:?}\n",
            digest(manifest.as_bytes())
        ));
        templates.push((path, manifest));
    }
    let recipe = format!(
        "schema-version 1 kind=\"build-recipe\"\nbuild architecture={architecture:?} epoch=1700000000 {{\n    toolchain solver=\"sat-v2\" {{\n{pins}{requests}    }}\n{manifest_nodes}    script \"exit 99\"\n}}\n"
    );
    let reviewed = digest(recipe.as_bytes());
    let parsed = build::parse(&recipe)?;
    let manifests = templates
        .iter()
        .map(|(_, text)| build::parse_template(text, &parsed).map(|template| template.manifest))
        .collect::<Result<Vec<_>>>()?;
    fs::write(bundle.path().join("build.kdl"), &recipe)?;
    for (path, text) in &templates {
        fs::write(bundle.path().join("templates").join(path), text)?;
    }
    crate::build::write_explicit_outputs(
        bundle.path(),
        &templates
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_bytes()))
            .collect::<Vec<_>>(),
    )?;
    let mut artifacts = BTreeMap::new();
    for manifest in &manifests {
        for path in manifest.payload_paths() {
            let bytes = fs::read(fixture.proto.join(path))?;
            let target = bundle.path().join("proto").join(path);
            fs::create_dir_all(target.parent().unwrap())?;
            fs::write(target, &bytes)?;
            artifacts.insert(
                path.to_owned(),
                FileDigest {
                    sha256: digest(&bytes),
                    size: bytes.len() as u64,
                },
            );
        }
    }
    let envelope = attestation::sign(
        &SigningKey::open(&builder.private)?,
        &manifests,
        &parsed,
        recipe.as_bytes(),
        Toolchain {
            publishers: parsed.toolchain.clone(),
            package_manifests: packages,
        },
        &artifacts,
        &Root::open(&bundle.path().join("proto"))?,
    )?;
    fs::write(bundle.path().join("attestation.json"), envelope)?;
    for (name, version, _) in outputs {
        approvals.push_str(&format!(
            "    approve {name:?} version={version:?} sha256={reviewed:?}\n"
        ));
    }
    fs::write(
        fixture.repo.join("publish-policy.kdl"),
        format!(
            "schema-version 1 kind=\"publish-policy\"\npublisher \"canopa\" archive-build-inputs=#{archive_inputs} {{\n    builder-key {:?}\n{approvals}}}\n",
            builder.public
        ),
    )?;
    crate::publish_bundle(
        &fixture.repo,
        bundle.path(),
        &fixture.private,
        &builder.public,
        &reviewed,
    )?;
    Ok(reviewed)
}

fn request(packages: &[&str]) -> Request {
    Request::InstallBuildDeps {
        packages: packages
            .iter()
            .map(|package| (*package).to_owned())
            .collect(),
    }
}

fn installed(root: &Path) -> Result<BTreeSet<String>> {
    Ok(Image::open_query(root)?
        .state()?
        .installed
        .into_keys()
        .collect())
}

#[test]
fn installs_build_dependencies_without_installing_outputs_or_running_the_recipe() -> Result<()> {
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    package(
        &fixture,
        &builder,
        "runtime/compiler",
        "1.0.0-0",
        String::new(),
    )?;
    package(&fixture, &builder, "devel/docs", "1.0.0-0", String::new())?;
    package(&fixture, &builder, "apps/keep", "1.0.0-0", String::new())?;
    package(
        &fixture,
        &builder,
        "devel/compiler",
        "1.0.0-0",
        dependency("require", "runtime/compiler", "1.0.0-0")
            + &dependency("recommends", "devel/docs", "1.0.0-0"),
    )?;
    package(
        &fixture,
        &builder,
        "library/headers",
        "2.0.0-0",
        fixture.file("usr/include/example.h", "header", "0644", "facet=\"devel\"")?,
    )?;
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["devel/compiler", "library/headers"],
        "x86_64",
    )?;
    fixture.create()?;
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::Install {
                packages: vec!["apps/keep".into()],
            },
        )?,
    )?;
    let before = installed(&fixture.root)?;
    assert!(
        plan(&fixture.root, request(&["apps/tool"]))?
            .summary()
            .contains("devel/docs")
    );
    let planned = crate::plan_with_recommends(&fixture.root, request(&["apps/tool"]), false)?;
    assert_eq!(installed(&fixture.root)?, before);
    assert_eq!(planned.request.operation(), "install --build-deps");
    assert!(!planned.summary().contains("devel/docs"));
    apply(&fixture.root, &planned)?;
    assert_eq!(
        installed(&fixture.root)?,
        BTreeSet::from([
            "apps/keep".into(),
            "devel/compiler".into(),
            "library/headers".into(),
            "runtime/compiler".into()
        ])
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("usr/include/example.h"))?,
        "header"
    );
    let state = Image::open_query(&fixture.root)?.state()?;
    assert!(state.installed["devel/compiler"].explicit);
    assert!(state.installed["library/headers"].explicit);
    assert!(!state.installed["runtime/compiler"].explicit);
    let details = crate::transaction_details(&fixture.root, &planned.id()?)?.details;
    assert_eq!(details.request, request(&["apps/tool"]));
    assert!(!plan(&fixture.root, Request::Autoremove)?.has_changes());
    assert!(
        !plan(&fixture.root, request(&["apps/tool"]))?
            .package_changes()
            .iter()
            .any(|change| change.name == "apps/tool")
    );
    assert!(verify(&fixture.root)?.is_empty());
    Ok(())
}

#[test]
fn build_dependency_targets_select_versions_and_combine_multiple_outputs() -> Result<()> {
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    for name in ["devel/old", "devel/new"] {
        package(&fixture, &builder, name, "1.0.0-0", String::new())?;
    }
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["devel/old"],
        "x86_64",
    )?;
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "2.0.0-0"), ("apps/tool-data", "2.0.0-0")],
        &["devel/new"],
        "x86_64",
    )?;
    fixture.create()?;
    let latest = plan(&fixture.root, request(&["apps/tool"]))?;
    assert_eq!(
        latest
            .package_changes()
            .iter()
            .map(|change| change.name.as_str())
            .collect::<Vec<_>>(),
        ["devel/new"]
    );
    let combined = plan(
        &fixture.root,
        request(&["cps://canopa/apps/tool@1.0.0-0", "apps/tool-data"]),
    )?;
    apply(&fixture.root, &combined)?;
    assert_eq!(
        installed(&fixture.root)?,
        BTreeSet::from(["devel/new".into(), "devel/old".into()])
    );
    Ok(())
}

#[test]
fn build_dependency_installs_require_authenticated_archived_inputs() -> Result<()> {
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    publish(
        &fixture,
        &builder,
        &[("apps/unarchived", "1.0.0-0", "")],
        &["build/bootstrap"],
        "x86_64",
        false,
    )?;
    package(
        &fixture,
        &builder,
        "devel/compiler",
        "1.0.0-0",
        String::new(),
    )?;
    let reviewed = archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["devel/compiler"],
        "x86_64",
    )?;
    fixture.create()?;
    for (target, expected) in [
        ("apps/unarchived", "no archived rebuild inputs"),
        ("apps/missing", "no available package"),
        ("apps/tool@9.0.0-0", "no available package"),
    ] {
        let error = plan(&fixture.root, request(&[target])).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    let object = fixture.repo.join("build-inputs").join(&reviewed);
    fs::write(object, "tampered recipe")?;
    let error = plan(&fixture.root, request(&["apps/tool"])).unwrap_err();
    assert!(format!("{error:#}").contains("digest"), "{error:#}");
    assert!(installed(&fixture.root)?.is_empty());
    Ok(())
}

#[test]
fn saved_build_dependency_plans_keep_their_catalogs_and_validate_the_request() -> Result<()> {
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    for name in ["devel/old", "devel/new"] {
        package(&fixture, &builder, name, "1.0.0-0", String::new())?;
    }
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["devel/old"],
        "x86_64",
    )?;
    fixture.create()?;
    let saved = plan(&fixture.root, request(&["apps/tool"]))?;
    let saved: Plan = serde_json::from_slice(&serde_json::to_vec(&saved)?)?;
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "2.0.0-0")],
        &["devel/new"],
        "x86_64",
    )?;
    let mut tampered = saved.clone();
    tampered.request = request(&["apps/tool@2.0.0-0"]);
    assert!(crate::validate_plan(&fixture.root, &tampered).is_err());
    crate::validate_plan(&fixture.root, &saved)?;
    apply(&fixture.root, &saved)?;
    assert_eq!(
        installed(&fixture.root)?,
        BTreeSet::from(["devel/old".into()])
    );
    Ok(())
}

#[test]
fn build_deps_honor_qualified_versions_and_sticky_publishers() -> Result<()> {
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    let tools = Fixture::new()?;
    for version in ["1.0.0-0", "2.0.0-0"] {
        tools.package_as("tools", "devel/compiler", version, String::new())?;
    }
    package(
        &fixture,
        &builder,
        "devel/compiler",
        "3.0.0-0",
        String::new(),
    )?;
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["cps://tools/devel/compiler@1.0.0-0"],
        "x86_64",
    )?;
    fixture.create()?;
    crate::add_publisher(&fixture.root, "tools", &tools.repo, &tools.trust)?;
    let dependencies = plan(&fixture.root, request(&["apps/tool"]))?;
    let changes = dependencies.package_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].publisher.as_deref(), Some("tools"));
    assert_eq!(changes[0].to.as_deref(), Some("1.0.0-0"));
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::Install {
                packages: vec!["cps://canopa/devel/compiler@3.0.0-0".into()],
            },
        )?,
    )?;
    let error = plan(&fixture.root, request(&["apps/tool"])).unwrap_err();
    assert!(
        format!("{error:#}").contains("sticky publisher canopa"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn build_deps_respect_freezes_and_facet_policy() -> Result<()> {
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    for version in ["1.0.0-0", "2.0.0-0"] {
        package(
            &fixture,
            &builder,
            "library/headers",
            version,
            fixture.file("usr/include/example.h", version, "0644", "facet=\"devel\"")?,
        )?;
    }
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["library/headers@2.0.0-0"],
        "x86_64",
    )?;
    fixture.create()?;
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::Install {
                packages: vec!["library/headers@1.0.0-0".into()],
            },
        )?,
    )?;
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::Freeze {
                packages: vec!["library/headers".into()],
                reason: Some("hold headers".into()),
            },
        )?,
    )?;
    let error = plan(&fixture.root, request(&["apps/tool"])).unwrap_err();
    assert!(format!("{error:#}").contains("frozen"), "{error:#}");
    assert_eq!(
        fs::read_to_string(fixture.root.join("usr/include/example.h"))?,
        "1.0.0-0"
    );
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::Unfreeze {
                packages: vec!["library/headers".into()],
            },
        )?,
    )?;
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::ChangeFacet {
                facets: BTreeMap::from([("devel".into(), Some(false))]),
                with_dependencies: false,
            },
        )?,
    )?;
    apply(
        &fixture.root,
        &plan(&fixture.root, request(&["apps/tool"]))?,
    )?;
    let state = Image::open_query(&fixture.root)?.state()?;
    assert_eq!(
        state.installed["library/headers"]
            .published
            .manifest
            .version
            .to_string(),
        "2.0.0-0"
    );
    assert!(!fixture.root.join("usr/include/example.h").exists());
    assert_eq!(state.settings.facets.get("devel"), Some(&false));
    Ok(())
}

#[test]
fn build_deps_enforce_builder_trust_even_for_installed_targets() -> Result<()> {
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    let trusted = Fixture::new()?;
    package(
        &fixture,
        &builder,
        "devel/compiler",
        "1.0.0-0",
        String::new(),
    )?;
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["devel/compiler"],
        "x86_64",
    )?;
    fixture.create()?;
    apply(
        &fixture.root,
        &plan(
            &fixture.root,
            Request::Install {
                packages: vec!["devel/compiler".into()],
            },
        )?,
    )?;
    let policy = fixture._dir.path().join("build-trust.kdl");
    fs::write(
        &policy,
        format!(
            "schema-version 1 kind=\"build-trust\"\nbuild-trust {{ builder-key {:?}; }}\n",
            trusted.public
        ),
    )?;
    crate::set_publisher_build_trust(&fixture.root, "canopa", &policy, true)?;
    for target in ["apps/tool", "devel/compiler"] {
        let error = plan(&fixture.root, request(&[target])).unwrap_err();
        assert!(
            format!("{error:#}").contains("trusted builder signatures"),
            "{error:#}"
        );
    }
    Ok(())
}

#[test]
fn build_dependency_installs_reject_wrong_architectures_and_unconfigured_publishers() -> Result<()>
{
    let fixture = Fixture::new()?;
    let builder = Fixture::new()?;
    package(
        &fixture,
        &builder,
        "devel/compiler",
        "1.0.0-0",
        String::new(),
    )?;
    archive(
        &fixture,
        &builder,
        &[("apps/foreign", "1.0.0-0")],
        &["devel/compiler"],
        "aarch64",
    )?;
    archive(
        &fixture,
        &builder,
        &[("apps/tool", "1.0.0-0")],
        &["cps://tools/devel/compiler@1.0.0-0"],
        "x86_64",
    )?;
    fixture.create()?;
    for (target, expected) in [
        ("apps/foreign", "targets architecture aarch64"),
        ("apps/tool", "publisher tools is not configured"),
    ] {
        let error = plan(&fixture.root, request(&[target])).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    assert!(installed(&fixture.root)?.is_empty());
    Ok(())
}
