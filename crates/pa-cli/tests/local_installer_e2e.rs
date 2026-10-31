#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _temp: tempfile::TempDir,
    script: PathBuf,
    home: PathBuf,
    caller: PathBuf,
    command: PathBuf,
    checkout: PathBuf,
    bin_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout's source");
        let script_dir = checkout.join("scripts");
        let bin_dir = temp.path().join("fake-bin");
        let home = temp.path().join("isolated-home");
        let caller = temp.path().join("other directory");
        for dir in [&script_dir, &bin_dir, &home, &caller] {
            fs::create_dir_all(dir).unwrap();
        }
        let script = script_dir.join("install-local.sh");
        fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/install-local.sh"
            ),
            &script,
        )
        .unwrap();
        let cargo = bin_dir.join("cargo");
        write_executable(&cargo, "#!/bin/sh\nif [ \"$*\" != \"build --release -p pa-cli --locked\" ]; then exit 65; fi\nif [ \"${FAKE_CARGO_FAIL:-}\" = 1 ]; then exit 42; fi\nexit 0\n");
        let binary = checkout.join("target/release/prime-agent");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        write_executable(
            &binary,
            "#!/bin/sh\nprintf 'cwd=%s\\nroot=%s\\n' \"$PWD\" \"$PI_PACKAGE_DIR\"\nprintf 'arg=%s\\n' \"$@\"\n",
        );
        let command = home.join(".local/bin/prime-agent");
        Self {
            _temp: temp,
            script,
            home,
            caller,
            command,
            checkout,
            bin_dir,
        }
    }

    fn install(
        &self,
        args: &[&str],
        home: Option<&Path>,
        on_path: bool,
        fail_build: bool,
    ) -> Output {
        let mut command = Command::new("/bin/sh");
        command
            .arg(&self.script)
            .args(args)
            .current_dir(&self.caller)
            .env_clear()
            .env(
                "PATH",
                if on_path {
                    format!(
                        "{}:{}/.local/bin:/usr/bin:/bin",
                        self.bin_dir.display(),
                        self.home.display()
                    )
                } else {
                    format!("{}:/usr/bin:/bin", self.bin_dir.display())
                },
            )
            .env("FAKE_CARGO_FAIL", if fail_build { "1" } else { "0" });
        if let Some(home) = home {
            command.env("HOME", home);
        }
        command.output().unwrap()
    }
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn launcher_preserves_arguments_cwd_and_absolute_checkout_paths() {
    let fixture = Fixture::new();
    let install = fixture.install(&[], Some(&fixture.home), false, false);
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(String::from_utf8_lossy(&install.stdout).contains(".local/bin"));
    let invoke = Command::new(&fixture.command)
        .args(["quoted ' argument", "two words", ""])
        .current_dir(&fixture.caller)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        invoke.status.success(),
        "{}",
        String::from_utf8_lossy(&invoke.stderr)
    );
    assert_eq!(
        String::from_utf8(invoke.stdout).unwrap(),
        format!(
            "cwd={}\nroot={}\narg=quoted ' argument\narg=two words\narg=\n",
            fixture.caller.display(),
            fixture.checkout.display()
        )
    );
    assert!(fixture.command.exists());
}

#[test]
fn installer_rejects_missing_home_and_unknown_or_multiple_flags() {
    let fixture = Fixture::new();
    for (args, home) in [
        (&[][..], None),
        (&["--invalid"][..], Some(fixture.home.as_path())),
        (&["--force", "--force"][..], Some(fixture.home.as_path())),
    ] {
        let result = fixture.install(args, home, false, false);
        assert!(!result.status.success(), "accepted {args:?}");
        assert!(!fixture.command.exists());
    }
}

#[test]
fn force_replaces_regular_and_dangling_symlink_but_never_directory() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.command.parent().unwrap()).unwrap();
    for symlink_destination in [false, true] {
        if symlink_destination {
            symlink(fixture.home.join("missing"), &fixture.command).unwrap();
        } else {
            fs::write(&fixture.command, "old command").unwrap();
        }
        let rejected = fixture.install(&[], Some(&fixture.home), true, false);
        assert!(!rejected.status.success());
        assert!(fs::symlink_metadata(&fixture.command).is_ok());
        let forced = fixture.install(&["--force"], Some(&fixture.home), true, false);
        assert!(
            forced.status.success(),
            "{}",
            String::from_utf8_lossy(&forced.stderr)
        );
        assert!(fs::read_to_string(&fixture.command)
            .unwrap()
            .contains("PI_PACKAGE_DIR="));
        fs::remove_file(&fixture.command).unwrap();
    }
    fs::create_dir(&fixture.command).unwrap();
    assert!(!fixture
        .install(&["--force"], Some(&fixture.home), true, false)
        .status
        .success());
    assert!(fixture.command.is_dir());
}

#[test]
fn live_symlink_rejects_without_force_and_force_preserves_linked_target() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.command.parent().unwrap()).unwrap();
    let target = fixture.home.join("existing command");
    let original = b"#!/bin/sh\necho linked command\n";
    fs::write(&target, original).unwrap();
    symlink(&target, &fixture.command).unwrap();

    let rejected = fixture.install(&[], Some(&fixture.home), true, false);
    assert!(!rejected.status.success());
    assert!(fs::symlink_metadata(&fixture.command)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&fixture.command).unwrap(), target);
    assert_eq!(fs::read(&target).unwrap(), original);

    let forced = fixture.install(&["--force"], Some(&fixture.home), true, false);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(!fs::symlink_metadata(&fixture.command)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(fs::read_to_string(&fixture.command)
        .unwrap()
        .contains("PI_PACKAGE_DIR="));
    assert_eq!(fs::read(&target).unwrap(), original);
}

#[test]
fn failed_build_keeps_existing_command_and_does_not_publish_new_one() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.command.parent().unwrap()).unwrap();
    let original = b"#!/bin/sh\necho old command\n";
    fs::write(&fixture.command, original).unwrap();
    let result = fixture.install(&["--force"], Some(&fixture.home), true, true);
    assert!(!result.status.success());
    assert_eq!(fs::read(&fixture.command).unwrap(), original);
    fs::remove_file(&fixture.command).unwrap();
    assert!(!fixture
        .install(&[], Some(&fixture.home), true, true)
        .status
        .success());
    assert!(!fixture.command.exists());
}

#[test]
fn path_hint_is_only_shown_when_install_directory_is_absent() {
    let missing = Fixture::new();
    let output = missing.install(&[], Some(&missing.home), false, false);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("PATH"));
    let present = Fixture::new();
    let output = present.install(&[], Some(&present.home), true, false);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PATH"));
}

#[test]
fn relative_cargo_target_directory_resolves_against_checkout_not_caller() {
    let fixture = Fixture::new();
    let binary = fixture.checkout.join("other target/release/prime-agent");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    write_executable(
        &binary,
        r#"#!/bin/sh
printf 'root=%s\n' "$PI_PACKAGE_DIR"
"#,
    );
    let install = Command::new("/bin/sh")
        .arg(&fixture.script)
        .current_dir(&fixture.caller)
        .env_clear()
        .env("HOME", &fixture.home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", fixture.bin_dir.display()),
        )
        .env("CARGO_TARGET_DIR", "other target")
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let invoke = Command::new(&fixture.command)
        .current_dir(&fixture.caller)
        .env_clear()
        .env("HOME", &fixture.home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        invoke.status.success(),
        "{}",
        String::from_utf8_lossy(&invoke.stderr)
    );
    assert_eq!(
        String::from_utf8(invoke.stdout).unwrap(),
        format!("root={}\n", fixture.checkout.display())
    );
}
#[test]
fn launcher_uses_rebuilt_checkout_binary_and_fails_after_checkout_removal() {
    let fixture = Fixture::new();
    let install = fixture.install(&[], Some(&fixture.home), true, false);
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );
    let binary = fixture.checkout.join("target/release/prime-agent");
    write_executable(&binary, "#!/bin/sh\nprintf 'rebuilt\n'\n");
    let invoke = Command::new(&fixture.command)
        .current_dir(&fixture.caller)
        .env_clear()
        .env("HOME", &fixture.home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(invoke.status.success());
    assert_eq!(invoke.stdout, b"rebuilt\n");
    fs::remove_dir_all(&fixture.checkout).unwrap();
    let removed = Command::new(&fixture.command)
        .current_dir(&fixture.caller)
        .env_clear()
        .env("HOME", &fixture.home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(!removed.status.success());
}

#[test]
fn publication_failure_preserves_old_command_and_cleans_staging_file() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.command.parent().unwrap()).unwrap();
    let original = b"#!/bin/sh\necho old command\n";
    fs::write(&fixture.command, original).unwrap();
    write_executable(&fixture.bin_dir.join("chmod"), "#!/bin/sh\nexit 43\n");
    let result = fixture.install(&["--force"], Some(&fixture.home), true, false);
    assert!(!result.status.success());
    assert_eq!(fs::read(&fixture.command).unwrap(), original);
    let names: Vec<_> = fs::read_dir(fixture.command.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("prime-agent")]);
}
