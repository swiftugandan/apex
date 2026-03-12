use cargo_metadata::MetadataCommand;
use std::collections::HashSet;
use std::path::Path;

/// Returns the set of direct dependency names for `pkg_name` in the workspace.
fn direct_deps(pkg_name: &str) -> HashSet<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("Cargo.toml");

    let metadata = MetadataCommand::new()
        .manifest_path(manifest)
        .exec()
        .expect("failed to run cargo metadata");

    let pkg = metadata
        .packages
        .iter()
        .find(|p| p.name == pkg_name)
        .unwrap_or_else(|| panic!("package `{pkg_name}` not found in workspace"));

    let resolve = metadata.resolve.as_ref().expect("no resolve graph");
    let node = resolve
        .nodes
        .iter()
        .find(|n| n.id == pkg.id)
        .unwrap_or_else(|| panic!("no resolve node for `{pkg_name}`"));

    node.deps.iter().map(|d| d.name.clone()).collect()
}

/// Asserts that `pkg_name` does NOT depend on `forbidden`.
fn assert_no_dependency(pkg_name: &str, forbidden: &str) {
    let deps = direct_deps(pkg_name);
    assert!(
        !deps.contains(forbidden),
        "`{pkg_name}` must not depend on `{forbidden}`, but it does. \
         Deps: {deps:?}"
    );
}

#[test]
fn apex_core_has_no_apex_dependencies() {
    let deps = direct_deps("apex-core");
    let violations: Vec<_> = deps
        .iter()
        .filter(|d| d.starts_with("apex-") || d.starts_with("apex_"))
        .collect();
    assert!(
        violations.is_empty(),
        "apex-core must have zero apex-* dependencies, but found: {violations:?}"
    );
}

#[test]
fn apex_engine_does_not_depend_on_tools_infra_or_rfbmq() {
    for forbidden in ["apex-tools", "apex-infra", "rfbmq-core"] {
        assert_no_dependency("apex-engine", forbidden);
    }
}

#[test]
fn apex_tools_does_not_depend_on_engine_infra_bin_or_rfbmq() {
    for forbidden in ["apex-engine", "apex-infra", "apex-bin", "rfbmq-core"] {
        assert_no_dependency("apex-tools", forbidden);
    }
}

#[test]
fn apex_infra_does_not_depend_on_engine_tools_or_bin() {
    for forbidden in ["apex-engine", "apex-tools", "apex-bin"] {
        assert_no_dependency("apex-infra", forbidden);
    }
}
