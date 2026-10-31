//! One local shell handoff, with exit-time CWD observation.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub(crate) struct ShellSessionOptions {
    pub shell: PathBuf,
    pub shell_args: Vec<String>,
    pub cwd: PathBuf,
}

pub(crate) struct ShellSessionOutcome {
    pub final_cwd: Option<PathBuf>,
    pub exit_code: Option<i32>,
}

struct HookDirectory(PathBuf);
impl HookDirectory {
    fn create() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("prime-agent-term-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .with_context(|| format!("create shell hook directory {}", path.display()))?;
        Ok(Self(path))
    }
}
impl Drop for HookDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) async fn run_shell_session(options: ShellSessionOptions) -> Result<ShellSessionOutcome> {
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        anyhow::bail!("local shell CWD observation is unavailable on this platform");
    }
    let hook = HookDirectory::create()?;
    let cwd_file = hook.0.join("cwd");
    let hook_file = hook.0.join("cwd-hook");
    let name = options
        .shell
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut args = options.shell_args;
    let mut command = tokio::process::Command::new(&options.shell);
    command
        .current_dir(&options.cwd)
        .kill_on_drop(true)
        .env("PRIME_AGENT_TERM_CWD_FILE", &cwd_file);
    let report = r#"pwd -P > "$PRIME_AGENT_TERM_CWD_FILE""#;
    match name.as_str() {
        "bash" => {
            let script = format!(r#"if [ -n "${{PRIME_AGENT_ORIGINAL_BASH_ENV:-}}" ] && [ -r "$PRIME_AGENT_ORIGINAL_BASH_ENV" ]; then . "$PRIME_AGENT_ORIGINAL_BASH_ENV"; fi
case $- in *i*) [ ! -r "$HOME/.bashrc" ] || . "$HOME/.bashrc" ;; esac
trap '{report}' EXIT
"#);
            std::fs::write(&hook_file, script)?;
            if let Some(value) = std::env::var_os("BASH_ENV") { command.env("PRIME_AGENT_ORIGINAL_BASH_ENV", value); }
            command.env("BASH_ENV", &hook_file);
            args.splice(0..0, ["--rcfile".to_string(), hook_file.to_string_lossy().into_owned()]);
        }
        "zsh" => {
            let original = std::env::var_os("ZDOTDIR").or_else(|| std::env::var_os("HOME"));
            std::fs::write(hook.0.join(".zshenv"), format!(r#"if [[ -n "${{PRIME_AGENT_ORIGINAL_ZDOTDIR:-}}" && -r "$PRIME_AGENT_ORIGINAL_ZDOTDIR/.zshenv" ]]; then source "$PRIME_AGENT_ORIGINAL_ZDOTDIR/.zshenv"; fi
autoload -Uz add-zsh-hook
function __prime_agent_report_cwd() {{ {report}; }}
add-zsh-hook zshexit __prime_agent_report_cwd
"#))?;
            if let Some(original) = original {
                #[cfg(unix)] for file in [".zprofile", ".zshrc", ".zlogin", ".zlogout"] {
                    let original_path = Path::new(&original).join(file);
                    if original_path.exists() { std::os::unix::fs::symlink(original_path, hook.0.join(file))?; }
                }
                command.env("PRIME_AGENT_ORIGINAL_ZDOTDIR", original);
            }
            command.env("ZDOTDIR", &hook.0);
        }
        "fish" => args.splice(0..0, ["--init-command".to_string(),
            r#"function __prime_agent_report_cwd --on-event fish_exit; pwd -P > "$PRIME_AGENT_TERM_CWD_FILE"; end"#.to_string()]).for_each(drop),
        "sh" | "dash" | "ksh" | "mksh" => {
            std::fs::write(&hook_file, format!(r#"if [ -n "${{PRIME_AGENT_ORIGINAL_ENV:-}}" ] && [ -r "$PRIME_AGENT_ORIGINAL_ENV" ]; then . "$PRIME_AGENT_ORIGINAL_ENV"; fi
trap '{report}' 0
"#))?;
            if let Some(value) = std::env::var_os("ENV") { command.env("PRIME_AGENT_ORIGINAL_ENV", value); }
            command.env("ENV", &hook_file);
            if let Some(index) = args.iter().position(|arg| arg == "-c") {
                if let Some(script) = args.get_mut(index + 1) {
                    *script = format!("trap '{report}' 0\n{script}");
                }
            }
        }
        _ => {}
    }
    let mut child = command
        .args(args)
        .spawn()
        .with_context(|| format!("launch shell {}", options.shell.display()))?;
    let pid = child.id();
    let mut last_observed = None;
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(300));
    let status = loop {
        tokio::select! {
            result = child.wait() => break result?,
            _ = interval.tick() => { if let Some(pid) = pid {
                #[cfg(target_os = "macos")]
                let observed = observe_process_cwd(pid).await;
                #[cfg(not(target_os = "macos"))]
                let observed = observe_process_cwd(pid);
                if let Some(path) = observed { last_observed = Some(path); }
            } }
        }
    };
    let reported = std::fs::read_to_string(&cwd_file).ok().and_then(|text| {
        let path = PathBuf::from(text.trim_end_matches(['\r', '\n']));
        (!path.as_os_str().is_empty()).then_some(path)
    });
    let final_cwd = reported.or(last_observed).filter(|path| path.is_dir());
    Ok(ShellSessionOutcome {
        final_cwd,
        exit_code: status.code(),
    })
}

#[cfg(target_os = "linux")]
fn observe_process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
async fn observe_process_cwd(pid: u32) -> Option<PathBuf> {
    let output = tokio::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .await
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix('n').map(PathBuf::from))
    })?
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn observe_process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn process_cwd_reads_self_and_returns_none_for_absent_pid() {
        #[cfg(target_os = "linux")]
        {
            let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(
                999_999_999 > pid_max,
                "absent PID must exceed kernel pid_max"
            );
        }
        #[cfg(target_os = "macos")]
        assert!(
            std::process::Command::new("lsof")
                .arg("-v")
                .output()
                .is_ok(),
            "lsof is required"
        );
        let pid = std::process::id();
        #[cfg(target_os = "macos")]
        let observed = observe_process_cwd(pid).await;
        #[cfg(target_os = "linux")]
        let observed = observe_process_cwd(pid);
        assert_eq!(
            observed.unwrap().canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        #[cfg(target_os = "macos")]
        let absent = observe_process_cwd(999_999_999).await;
        #[cfg(target_os = "linux")]
        let absent = observe_process_cwd(999_999_999);
        assert_eq!(absent, None);
    }

    #[tokio::test]
    async fn rapid_cd_exit_23_records_final_directory_and_status() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("dir's name");
        std::fs::create_dir(&target).unwrap();
        let script = format!("cd \"{}\"; exit 23", target.display());
        let result = run_shell_session(ShellSessionOptions {
            shell: "/bin/sh".into(),
            shell_args: vec!["-c".into(), script],
            cwd: root.path().into(),
        })
        .await
        .unwrap();
        assert_eq!(result.exit_code, Some(23));
        assert_eq!(result.final_cwd, Some(target.canonicalize().unwrap()));
    }

    #[tokio::test]
    async fn exit_hook_files_are_removed_after_shell_exit() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let shell = root.path().join("sh");
        let seen = root.path().join("hook-path");
        std::fs::write(&shell, format!("#!/bin/sh\nprintf '%s' \"$PRIME_AGENT_TERM_CWD_FILE\" > '{}'\nexec /bin/sh \"$@\"\n", seen.display())).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let outcome = run_shell_session(ShellSessionOptions {
            shell,
            shell_args: vec!["-c".into(), "exit 23".into()],
            cwd: root.path().into(),
        })
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, Some(23));
        let hook_path = PathBuf::from(std::fs::read_to_string(seen).unwrap());
        assert!(
            !hook_path.parent().unwrap().exists(),
            "private hook directory survives exit"
        );
    }
    #[tokio::test]
    async fn removed_final_directory_is_not_propagated() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("removed");
        std::fs::create_dir(&target).unwrap();
        let outcome = run_shell_session(ShellSessionOptions {
            shell: "/bin/sh".into(),
            shell_args: vec![
                "-c".into(),
                format!(
                    "cd '{}'; rmdir '{}'; exit 23",
                    target.display(),
                    target.display()
                ),
            ],
            cwd: root.path().into(),
        })
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, Some(23));
        assert_eq!(outcome.final_cwd, None);
    }
}

#[cfg(all(test, not(any(target_os = "linux", target_os = "macos"))))]
mod unsupported_platform_tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_platform_refuses_shell_handoff() {
        let outcome = run_shell_session(ShellSessionOptions {
            shell: "/bin/sh".into(),
            shell_args: Vec::new(),
            cwd: "/".into(),
        })
        .await;
        assert!(outcome.is_err());
    }
}
