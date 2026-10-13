use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::tempdir;
use warp_cli::agent::Harness;

use super::{MANIFEST_FILE, discover, read_manifest, resolve_env, resolve_sidecar_program};

#[cfg(windows)]
const PROGRAM: &str = "adapter.exe";
#[cfg(not(windows))]
const PROGRAM: &str = "adapter";

fn write_manifest(path: &Path, args: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        json!({
            "schema_version": 1,
            "harness": "pi",
            "acp": {"program": PROGRAM, "args": args}
        })
        .to_string(),
    )
    .unwrap();
}

fn write_executable(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "").unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn discovery_selects_override_then_mount_then_bundle_without_merging() {
    let root = tempdir().unwrap();
    let override_dir = root.path().join("override");
    let mounts = root.path().join("mnt");
    let mount_dir = mounts.join("pi-cli-sidecar");
    let resources = root.path().join("resources");
    let mount_manifest = mount_dir.join(MANIFEST_FILE);
    write_manifest(&override_dir.join(MANIFEST_FILE), &["override"]);
    write_manifest(&mount_manifest, &["mount"]);
    write_manifest(&resources.join("bundled/harnesses/pi.json"), &["bundle"]);
    for dir in [&override_dir, &mount_dir] {
        write_executable(&dir.join("usr/local/bin").join(PROGRAM));
    }
    let mut bundle: Value =
        serde_json::from_slice(&fs::read(resources.join("bundled/harnesses/pi.json")).unwrap())
            .unwrap();
    bundle["install_docs_url"] = json!("https://example.test/install");
    fs::write(
        resources.join("bundled/harnesses/pi.json"),
        bundle.to_string(),
    )
    .unwrap();

    let selected = discover(Harness::Pi, Some(&override_dir), &mounts, Some(&resources)).unwrap();
    assert_eq!(selected.launch.args, ["override"]);
    assert_eq!(
        Path::new(&selected.launch.program),
        override_dir.join("usr/local/bin").join(PROGRAM)
    );
    assert!(selected.install_docs_url.is_none());
    let selected = discover(Harness::Pi, None, &mounts, Some(&resources)).unwrap();
    assert_eq!(selected.launch.args, ["mount"]);
    assert!(selected.install_docs_url.is_none());
    fs::remove_file(mount_manifest).unwrap();
    let selected = discover(Harness::Pi, None, &mounts, Some(&resources)).unwrap();
    assert_eq!(selected.launch.args, ["bundle"]);
    assert_eq!(selected.launch.program, PROGRAM);
    assert!(!selected.is_sidecar);
}

#[test]
fn override_errors_do_not_fall_through_to_a_valid_bundle() {
    let root = tempdir().unwrap();
    let override_dir = root.path().join("override");
    let path = override_dir.join(MANIFEST_FILE);
    let resources = root.path().join("resources");
    write_manifest(&resources.join("bundled/harnesses/pi.json"), &[]);
    write_manifest(&path, &[]);
    let mut wrong: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    wrong["harness"] = json!("codex");
    fs::write(&path, wrong.to_string()).unwrap();

    let error = discover(
        Harness::Pi,
        Some(&override_dir),
        root.path(),
        Some(&resources),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("expected pi"));
    fs::remove_file(&path).unwrap();
    let error = discover(
        Harness::Pi,
        Some(&override_dir),
        root.path(),
        Some(&resources),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains(&path.display().to_string()));
}

#[test]
fn sidecar_program_requires_an_executable_under_usr_local_bin() {
    let root = tempdir().unwrap();
    write_executable(&root.path().join("bin").join(PROGRAM));
    assert!(resolve_sidecar_program(root.path(), PROGRAM).is_err());
    let executable = root.path().join("usr/local/bin").join(PROGRAM);
    write_executable(&executable);
    #[cfg(unix)]
    {
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            resolve_sidecar_program(root.path(), PROGRAM)
                .unwrap_err()
                .to_string()
                .contains("not executable")
        );
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(
        Path::new(&resolve_sidecar_program(root.path(), PROGRAM).unwrap()),
        executable
    );
    for program in ["/adapter", "../adapter", r"bin\adapter"] {
        assert!(
            resolve_sidecar_program(root.path(), program)
                .unwrap_err()
                .to_string()
                .contains("bare filename")
        );
    }
}

#[test]
fn manifest_directory_substitution_is_env_only_and_process_values_win() {
    let root = tempdir().unwrap();
    let resources = root.path().join("resources");
    let path = resources.join("bundled/harnesses/pi.json");
    write_manifest(&path, &["${MANIFEST_DIR}"]);
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["env"] = json!({
        "PI_PACKAGE_DIR": "${MANIFEST_DIR}/usr/local/lib/pi-packages",
        "LITERAL": "${OTHER}"
    });
    fs::write(&path, manifest.to_string()).unwrap();
    let manifest = read_manifest(&path, Harness::Pi).unwrap();
    let dir = path.parent().unwrap();
    let env = resolve_env(&manifest.env, dir, HashMap::new());
    assert_eq!(
        PathBuf::from(&env[OsStr::new("PI_PACKAGE_DIR")]),
        dir.join("usr/local/lib/pi-packages")
    );
    assert_eq!(env[OsStr::new("LITERAL")], "${OTHER}");
    assert_eq!(manifest.acp.args, ["${MANIFEST_DIR}"]);
    let env = resolve_env(
        &manifest.env,
        dir,
        HashMap::from([(
            OsString::from("PI_PACKAGE_DIR"),
            OsString::from("process-package-dir"),
        )]),
    );
    assert_eq!(env[OsStr::new("PI_PACKAGE_DIR")], "process-package-dir");
}

#[test]
fn bundled_manifests_use_schema_one_and_matching_harness_ids() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../resources/bundled/harnesses");
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let harness = match path.file_stem().unwrap().to_str().unwrap() {
            "claude" => Harness::Claude,
            "codex" => Harness::Codex,
            "gemini" => Harness::Gemini,
            "pi" => Harness::Pi,
            other => panic!("Unexpected bundled harness {other}"),
        };
        let manifest = read_manifest(&path, harness).unwrap();
        assert_eq!(manifest.schema_version, 1);
        if harness == Harness::Pi {
            assert!(manifest.env.is_empty());
        }
    }
}

#[test]
fn unsupported_versions_and_unknown_fields_are_rejected() {
    let root = tempdir().unwrap();
    let path = root.path().join(MANIFEST_FILE);
    write_manifest(&path, &[]);
    let valid: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut unsupported = valid.clone();
    unsupported["schema_version"] = json!(2);
    fs::write(&path, unsupported.to_string()).unwrap();
    assert!(
        read_manifest(&path, Harness::Pi)
            .unwrap_err()
            .to_string()
            .contains("unsupported schema_version")
    );
    let mut top_level = valid.clone();
    top_level["unknown"] = json!(true);
    let mut acp = valid.clone();
    acp["acp"]["unknown"] = json!(true);
    let mut auth = valid;
    auth["auth"] = json!({"unknown": true});
    for invalid in [top_level, acp, auth] {
        fs::write(&path, invalid.to_string()).unwrap();
        assert!(
            format!("{:#}", read_manifest(&path, Harness::Pi).unwrap_err())
                .contains("unknown field")
        );
    }
}

#[test]
fn missing_manifest_error_lists_all_discovery_locations() {
    let root = tempdir().unwrap();
    let mounts = root.path().join("mnt");
    let resources = root.path().join("resources");
    let error = discover(Harness::Pi, None, &mounts, Some(&resources))
        .err()
        .unwrap()
        .to_string();
    for expected in [
        "pi".to_owned(),
        "$WARP_HARNESS_SIDECAR/warp-harness.json".to_owned(),
        mounts
            .join("pi-cli-sidecar/warp-harness.json")
            .display()
            .to_string(),
        resources
            .join("bundled/harnesses/pi.json")
            .display()
            .to_string(),
    ] {
        assert!(error.contains(&expected), "{error}");
    }
}
