//! End-to-end checks against `fixtures/megarepo`, driving the real `bazel`.
//!
//! The unit tests in `src/` parse captured output and never shell out. These
//! run the actual binary, which is the only way to notice that a Bazel upgrade
//! changed a flag or a provider name out from under us.
//!
//! They need `bazel` on `PATH` and will trigger a real build, so they skip
//! loudly rather than failing when it is absent. Set `JABAR_REQUIRE_BAZEL=1` in
//! CI to turn a skip into a failure.

use build_model::{BazelCli, TargetLabel};
use paths::{AbsPath, AbsPathBuf, Utf8Path};

/// Returns a CLI pointed at the fixture, or `None` if the environment cannot
/// run it.
fn fixture_cli() -> Option<BazelCli> {
    let root =
        AbsPathBuf::try_from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/megarepo"))
            .expect("CARGO_MANIFEST_DIR is absolute");

    let missing = if !root.join("MODULE.bazel").as_utf8_path().is_file() {
        Some(format!("fixture not found at {root}"))
    } else if which_bazel().is_none() {
        Some("`bazel` is not on PATH".to_owned())
    } else {
        None
    };

    match missing {
        None => Some(BazelCli::new(root)),
        Some(reason) => {
            if std::env::var_os("JABAR_REQUIRE_BAZEL").is_some() {
                panic!("JABAR_REQUIRE_BAZEL is set but {reason}");
            }
            eprintln!("SKIP: {reason}");
            None
        }
    }
}

fn which_bazel() -> Option<()> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| dir.join("bazel").is_file().then_some(()))
}

fn fixture_file(cli: &BazelCli, relative: &str) -> AbsPathBuf {
    cli.workspace_root().join(relative)
}

#[test]
fn resolves_a_source_file_to_its_target() {
    let Some(cli) = fixture_cli() else { return };

    let file = fixture_file(&cli, "java/com/acme/policy/DefaultRetryPolicy.java");
    let target = cli.owning_target(&file).expect("query should succeed");
    assert_eq!(target.map(|t| t.to_string()).as_deref(), Some("//java/com/acme/policy:policy"));
}

#[test]
fn nested_packages_resolve_to_the_deepest_one() {
    // `service/order` declares its own package below `service`. Picking the
    // shallower one would attach the file to the wrong classpath.
    let Some(cli) = fixture_cli() else { return };

    let file = fixture_file(&cli, "java/com/acme/service/order/OrderService.java");
    assert_eq!(cli.package_of(&file).as_deref(), Some("java/com/acme/service/order"));

    let target = cli.owning_target(&file).expect("query should succeed");
    assert_eq!(
        target.map(|t| t.to_string()).as_deref(),
        Some("//java/com/acme/service/order:order")
    );
}

#[test]
fn a_file_in_no_target_is_not_an_error() {
    // `java/com/acme/orphan/` has no BUILD file, so the nearest package is the
    // repo root — the fixture's root `BUILD.bazel` declares one. The file is
    // therefore *in* a package while belonging to no target, which is the
    // common shape of an orphan in a real repo and the harder case: plain
    // `bazel query` exits 7 on it, indistinguishable from a broken workspace.
    let Some(cli) = fixture_cli() else { return };

    let file = fixture_file(&cli, "java/com/acme/orphan/NotInAnyTarget.java");
    assert_eq!(cli.package_of(&file).as_deref(), Some(""), "nearest package is the root");
    assert_eq!(
        cli.owning_target(&file).expect("must not be reported as a failure"),
        None,
        "an unowned file is a fact about the repo, not a broken build"
    );
}

#[test]
fn a_non_source_file_in_a_real_package_has_no_owner() {
    // A BUILD file sits in a package that certainly exists, but is not in any
    // target's `srcs`. Agents open these constantly, so it must resolve to "no
    // owner" rather than to an error.
    let Some(cli) = fixture_cli() else { return };

    let build_file = fixture_file(&cli, "java/com/acme/policy/BUILD.bazel");
    assert_eq!(
        cli.package_of(&build_file).as_deref(),
        Some("java/com/acme/policy"),
        "the package plainly exists"
    );
    assert_eq!(cli.owning_target(&build_file).expect("must not fail"), None);
}

#[test]
fn a_file_outside_the_workspace_has_no_owner() {
    let Some(cli) = fixture_cli() else { return };

    let outside = AbsPath::new_unchecked(Utf8Path::new("/tmp/Elsewhere.java"));
    assert_eq!(cli.owning_target(outside).expect("should not fail"), None);
}

#[test]
fn compile_info_reports_header_jars_and_sources() {
    let Some(cli) = fixture_cli() else { return };

    let target = TargetLabel::parse("//java/com/acme/service/order:order").unwrap();
    let info = cli
        .compile_info(&target)
        .expect("aquery should succeed")
        .expect("a java_library has a Javac action");

    let sources: Vec<_> = info.java_sources().filter_map(|p| p.file_name()).collect();
    assert_eq!(sources, ["OrderRepository.java", "OrderService.java"]);

    // Every classpath entry is a header jar: signatures without bodies. That is
    // the item tree, already built and already remotely cacheable by Bazel.
    assert_eq!(info.classpath.len(), 6, "six direct deps: {:?}", info.classpath);
    for jar in &info.classpath {
        let name = jar.file_name().unwrap_or_default();
        assert!(
            name.ends_with("-hjar.jar") || name.ends_with("-ijar.jar"),
            "expected a header jar, got `{name}`"
        );
    }

    // The binary-only dependency has to appear, or nothing about com.tinyjson
    // is resolvable.
    assert!(
        info.classpath.iter().any(|p| p.as_str().contains("tinyjson")),
        "tinyjson missing from {:?}",
        info.classpath
    );
}

#[test]
fn a_java_import_has_no_javac_action() {
    // `third_party/tinyjson` wraps a prebuilt jar. Nothing compiles it, so
    // asking for its compile info is legitimately empty rather than an error.
    let Some(cli) = fixture_cli() else { return };

    let target = TargetLabel::parse("//third_party/tinyjson:tinyjson").unwrap();
    assert_eq!(
        cli.compile_info(&target).expect("aquery should succeed").map(|i| i.label.to_string()),
        None,
    );
}

#[test]
fn a_nonexistent_target_is_reported_as_a_failure() {
    // The opposite of the case above: asking about something that does not
    // exist must not look like "this target has no sources".
    let Some(cli) = fixture_cli() else { return };

    let target = TargetLabel::parse("//java/com/acme:no_such_target").unwrap();
    assert!(
        cli.compile_info(&target).is_err(),
        "a missing target should surface as an error, not an empty result"
    );
}
