use std::ffi::OsStr;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use command::blocking::Command as ProcessCommand;
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
fn discovery_selects_override_then_mount_then_defaults_without_merging() {
    let root = tempdir().unwrap();
    let override_dir = root.path().join("override");
    let mounts = root.path().join("mnt");
    let mount_dir = mounts.join("pi-cli-sidecar");
    let mount_manifest = mount_dir.join(MANIFEST_FILE);
    write_manifest(&override_dir.join(MANIFEST_FILE), &["override"]);
    write_manifest(&mount_manifest, &["mount"]);
    for dir in [&override_dir, &mount_dir] {
        write_executable(&dir.join("usr/local/bin").join(PROGRAM));
    }

    let selected = discover(Harness::Pi, Some(&override_dir), &mounts).unwrap();
    assert_eq!(selected.launch.args, ["override"]);
    assert_eq!(
        Path::new(&selected.launch.program),
        override_dir.join("usr/local/bin").join(PROGRAM)
    );
    assert!(selected.is_sidecar);
    let selected = discover(Harness::Pi, None, &mounts).unwrap();
    assert_eq!(selected.launch.args, ["mount"]);
    fs::remove_file(mount_manifest).unwrap();
    let selected = discover(Harness::Pi, None, &mounts).unwrap();
    assert_eq!(selected.launch.program, "pi-acp");
    assert!(selected.launch.args.is_empty());
    assert!(!selected.is_sidecar);
}

#[test]
fn override_errors_do_not_fall_through_to_defaults() {
    let root = tempdir().unwrap();
    let override_dir = root.path().join("override");
    let path = override_dir.join(MANIFEST_FILE);
    write_manifest(&path, &[]);
    let mut wrong: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    wrong["harness"] = json!("codex");
    fs::write(&path, wrong.to_string()).unwrap();

    let error = discover(Harness::Pi, Some(&override_dir), root.path())
        .err()
        .unwrap();
    assert!(error.to_string().contains("expected pi"));
    fs::remove_file(&path).unwrap();
    let error = discover(Harness::Pi, Some(&override_dir), root.path())
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
    for program in ["/adapter", "../adapter", r"bin\adapter", "C:adapter.exe"] {
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
    if std::env::var_os("WARP_TEST_MANIFEST_ENV").is_none() {
        let mut command = ProcessCommand::new(std::env::current_exe().unwrap());
        #[cfg(windows)]
        command.creation_flags(0);
        let output = command
            .args([
                "--exact",
                "ai::agent_sdk::driver::harness::acp::manifest::tests::manifest_directory_substitution_is_env_only_and_process_values_win",
                "--nocapture",
            ])
            .env("WARP_TEST_MANIFEST_ENV", "1")
            .env_remove("PI_PACKAGE_DIR")
            .env_remove("LITERAL")
            .env_remove("CODEX_HOME")
            .env("codex_home", "process-codex-home")
            .env("WARP_MANIFEST_OVERRIDE", "process-package-dir")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }
    let root = tempdir().unwrap();
    let path = root.path().join(MANIFEST_FILE);
    write_manifest(&path, &["${MANIFEST_DIR}"]);
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["env"] = json!({
        "PI_PACKAGE_DIR": "${MANIFEST_DIR}/usr/local/lib/pi-packages",
        "LITERAL": "${OTHER}",
        "WARP_MANIFEST_OVERRIDE": "manifest-package-dir",
        "CODEX_HOME": "manifest-codex-home"
    });
    fs::write(&path, manifest.to_string()).unwrap();
    let manifest = read_manifest(&path, Harness::Pi).unwrap();
    let dir = path.parent().unwrap();
    let env = resolve_env(&manifest.env, dir);
    assert_eq!(
        PathBuf::from(&env[OsStr::new("PI_PACKAGE_DIR")]),
        dir.join("usr/local/lib/pi-packages")
    );
    assert_eq!(env[OsStr::new("LITERAL")], "${OTHER}");
    assert_eq!(manifest.acp.args, ["${MANIFEST_DIR}"]);
    assert_eq!(
        env[OsStr::new("WARP_MANIFEST_OVERRIDE")],
        "process-package-dir"
    );
    assert_eq!(
        env[OsStr::new("CODEX_HOME")],
        if cfg!(windows) {
            "process-codex-home"
        } else {
            "manifest-codex-home"
        }
    );
}

#[test]
fn unsupported_versions_and_unknown_section_fields_are_rejected() {
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
    let mut future_section = valid.clone();
    future_section["tui"] = json!({"program": "pi"});
    fs::write(&path, future_section.to_string()).unwrap();
    assert!(read_manifest(&path, Harness::Pi).is_ok());
    let mut acp = valid.clone();
    acp["acp"]["unknown"] = json!(true);
    let mut auth = valid;
    auth["auth"] = json!({"unknown": true});
    for invalid in [acp, auth] {
        fs::write(&path, invalid.to_string()).unwrap();
        assert!(
            format!("{:#}", read_manifest(&path, Harness::Pi).unwrap_err())
                .contains("unknown field")
        );
    }
}
