#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::{fs, path::Path, process::Command};

#[test]
fn local_installer_preserves_failed_build_and_publishes_forced_launcher() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.join("../..").canonicalize().unwrap();
    let script = root.join("scripts/install-local.sh");
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    let bin = sandbox.path().join("bin");
    let target = sandbox.path().join("target");
    fs::create_dir_all(home.join(".local/bin")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let launcher = home.join(".local/bin/prime-agent");
    fs::write(&launcher, "previous launcher\n").unwrap();
    let cargo = bin.join("cargo");
    fs::write(&cargo, r#"#!/bin/sh
if [ "${FAIL_BUILD:-}" = 1 ]; then exit 42; fi
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/bin/sh\nprintf "%%s\\n" "$PI_PACKAGE_DIR" "$@"\n' > "$CARGO_TARGET_DIR/release/prime-agent"
chmod 755 "$CARGO_TARGET_DIR/release/prime-agent"
"#).unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let install = |fail| {
        Command::new("sh")
            .arg(&script)
            .arg("--force")
            .env("HOME", &home)
            .env("CARGO_TARGET_DIR", &target)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FAIL_BUILD", if fail { "1" } else { "0" })
            .output()
            .unwrap()
    };
    let failed = install(true);
    assert!(!failed.status.success(), "{failed:?}");
    assert_eq!(fs::read(&launcher).unwrap(), b"previous launcher\n");
    let installed = install(false);
    assert!(installed.status.success(), "{installed:?}");
    assert_eq!(
        fs::metadata(&launcher).unwrap().permissions().mode() & 0o111,
        0o111
    );
    let output = Command::new(&launcher)
        .args(["one two", "it's quoted"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\none two\nit's quoted\n", root.display())
    );
}
