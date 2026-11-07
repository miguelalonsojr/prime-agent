// The Tier-C/D ruling (fleet-uniform, 2026-09-28): stack-resident futures
// by design on hot paths; the fn-length threshold is a style gate, not
// correctness; the harness fns are intentionally linear; 64-bit targets -
// the narrowing sits at OS/protocol boundaries where the values are
// bounded (pid syscalls, epoch/elapsed milliseconds).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Real-pty e2e for the input-latency contract (the operator's
//! "first page fast, time-to-type slow" report + AGENTS.md
//! "Performance and the critical path"): a NEW session's interactive
//! surface must accept keystrokes the moment its startup chrome is
//! mounted — the session open (create, attach, the dock folds, every
//! daemon readiness behind them) runs in the background, so the echo
//! never waits on it (TS `init()`: `ui.start()` is live before
//! `rebindCurrentSession()` awaits). The mock supervisor stalls the
//! `attach` response far past the echo deadline, so an open-gated input
//! path cannot fake a pass.
//!
//! The typed-ahead contract rides the same run: text typed during the
//! stall plus an Enter queued behind the open must submit once the
//! session lands — never lost, never errored.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg::F_SETFL, OFlag};
use nix::pty::{openpty, Winsize};
use serde_json::{json, Value};

use pa_tui::interactive::{
    run_interactive, InteractiveOptions, ModelSelection, SessionSelection, UiMode,
};

/// The child-mode socket: set (with the socket path) only when this very
/// binary is re-executed as the product-under-test.
const CHILD_SOCKET_ENV: &str = "PA_INPUT_LIVE_CHILD_SOCKET";

/// How long the mock supervisor holds the `attach` response: far past the
/// echo deadline below, so the open is provably still in flight while the
/// keystrokes are expected to echo.
const OPEN_STALL_MS: u64 = 2_500;

/// The echo deadline: the keystroke echo must land while the attach is
/// still stalled, well inside the stall window.
const ECHO_DEADLINE_MS: u64 = 1_500;

/// The pty window the child renders into (a 0x0 pty renders nothing; the
/// grid the echo oracle reads matches the window the child lays out).
const PTY_ROWS: usize = 24;
const PTY_COLS: usize = 80;

/// The child half of the e2e: runs the real interactive loop in terminal
/// mode against the parent's mock supervisor. A plain `cargo test` run
/// (no `CHILD_SOCKET_ENV`) passes trivially — only the parent test drives
/// the real path.
#[test]
fn input_live_open_child_mode() {
    let Ok(socket) = std::env::var(CHILD_SOCKET_ENV) else {
        return;
    };
    let options = child_options(PathBuf::from(socket));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let ui = if std::env::var_os("PA_HERDR_HEADLESS").is_some() {
        UiMode::Headless(pa_tui::interactive::HeadlessPlan {
            steps: vec![],
            width: 80,
            height: 24,
        })
    } else {
        UiMode::Terminal
    };
    let _ = runtime.block_on(run_interactive(options, ui));
}

/// The pty harnesses serialize: each drives a raw pty; concurrent
/// byte-level waits flake on the shared sandbox CPUs.
static HARNESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A keystroke echoes into the rendered surface while the session open is
/// still in flight (the attach is stalled for `OPEN_STALL_MS`), and the
/// typed-ahead text plus the queued Enter submit once the open lands.
#[test]
fn typing_echoes_while_the_session_open_is_stalled() {
    if !session_runner() {
        return;
    }
    match nix::unistd::setsid() {
        Ok(_) => {}
        Err(error) => panic!("the harness could not start a fresh session: {error}"),
    }
    let _lock = match HARNESS_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness = InputLiveHarness::start(Arc::clone(&commands));

    // The startup chrome (the new chat's brand splash) paints first: the
    // surface is mounted and in raw mode by then.
    harness.wait_from_start("prime agent", "the startup chrome painted");

    // Type while the open is still stalled. The echo must land inside the
    // echo deadline — far inside the attach's stall window.
    harness.type_text("zzq");
    let echo_at = harness.wait_from_start("zzq", "the keystroke echoed while the open was stalled");

    // The queued submit: the typed-ahead text plus Enter ride the opening
    // phase's queue and dispatch once the session lands.
    harness.type_text("\r");

    // Let the stalled attach land and the fold run.
    let content_at = harness.wait_from_start(
        "openprobe settled answer",
        "the attach's content frame painted",
    );

    // The red-first oracle, ordering-based (load-robust where a wall
    // clock is not): the keystroke echo must have painted while the
    // attach was still stalled, so it became visible strictly BEFORE the
    // attach's content frame in the byte stream. An open-gated input path
    // cannot fake this — its first echo frame can only follow the
    // stalled open, the same event that paints the content frame.
    assert!(
        echo_at < content_at,
        "the keystroke echo (byte {echo_at}) painted only after the          attach's content frame (byte {content_at}): the input path was          open-gated"
    );

    // The typed-ahead submit was delivered: the mock supervisor saw the
    // prompt carrying the typed text.
    let deadline = Instant::now() + Duration::from_secs(10);
    let submitted = loop {
        if let Some(found) = find_prompt_with(&commands, "zzq") {
            break found;
        }
        if Instant::now() > deadline {
            let seen = commands.lock().expect("commands").clone();
            panic!("the typed-ahead submit never reached the daemon; commands seen: {seen:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = submitted;

    // The harness drops here: the child dies with the test on every path.
}

/// Whether this runner is attached to a controlling-terminal session (the
/// child re-exec needs a session it can leave and re-enter safely).
fn session_runner() -> bool {
    // SAFETY: tcgetpgrp only queries the fd's foreground process group.
    let foreground = unsafe { libc::tcgetpgrp(0) };
    if foreground < 0 {
        eprintln!(
            "no controlling-terminal session on the runner (tcgetpgrp(fd 0) \
             failed); skipping the input-live open e2e — it needs a \
             controlling-terminal session to drive the pty child"
        );
        return false;
    }
    true
}

/// One pty-backed product child plus the mock supervisor it opens against.
struct InputLiveHarness {
    child: Child,
    server: Option<std::thread::JoinHandle<()>>,
    /// Whether the mock's `accept` landed: `Drop` joins the serve thread
    /// only then, so a child that dies before connecting cannot hang the
    /// teardown on the still-accepting listener.
    connected: Arc<std::sync::atomic::AtomicBool>,
    cli_socket: Option<PathBuf>,
    cli_shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    master: PtyReader,
}

impl InputLiveHarness {
    fn start(commands: Arc<Mutex<Vec<String>>>) -> InputLiveHarness {
        Self::start_with_shell(commands, None)
    }

    fn start_with_shell(
        commands: Arc<Mutex<Vec<String>>>,
        shell: Option<&std::path::Path>,
    ) -> InputLiveHarness {
        Self::start_with_capability(commands, shell, true)
    }

    fn start_with_capability(
        commands: Arc<Mutex<Vec<String>>>,
        shell: Option<&std::path::Path>,
        cwd_capable: bool,
    ) -> InputLiveHarness {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let socket = dir.path().join("tui.sock");
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let supervisor = MockSupervisor::bind(&socket, cwd_capable);
        let server = {
            let connected = Arc::clone(&connected);
            Some(std::thread::spawn(move || {
                supervisor.serve(&commands, &connected);
            }))
        };
        let pty = openpty(
            Some(&Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .expect("open pty");
        let child = spawn_child(&socket, &pty.slave, shell);
        // Leak the temp dir's socket path on purpose: the child needs the
        // socket for the lifetime of the test, and the whole tree dies
        // with the child at teardown.
        std::mem::forget(dir);
        InputLiveHarness {
            child,
            server,
            connected,
            cli_socket: None,
            cli_shutdown: None,
            master: PtyReader::new(pty.master),
        }
    }

    fn start_cli(
        commands: Arc<Mutex<Vec<String>>>,
        private: &std::path::Path,
        cwd: &std::path::Path,
        agent_dir: &std::path::Path,
        fallback_shell: &std::path::Path,
    ) -> InputLiveHarness {
        let socket = private.join("cli-mock.sock");
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut supervisor = MockSupervisor::bind(&socket, true);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        supervisor.cli_cwd = Some(cwd.to_path_buf());
        supervisor.cli_shutdown = Some(Arc::clone(&shutdown));
        let pty = openpty(
            Some(&Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .expect("CLI pty");
        let master = PtyReader::new(pty.master);
        let mut child =
            spawn_cli_child(&socket, &pty.slave, private, cwd, agent_dir, fallback_shell);
        let server = {
            let connected = Arc::clone(&connected);
            match std::thread::Builder::new()
                .spawn(move || supervisor.serve_cli(&commands, &connected))
            {
                Ok(server) => Some(server),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("spawn CLI mock server: {error}");
                }
            }
        };
        InputLiveHarness {
            child,
            server,
            connected,
            cli_socket: Some(socket),
            cli_shutdown: Some(shutdown),
            master,
        }
    }

    fn wait_for_event(&mut self, needle: &str) {
        self.master.wait_for_event(needle);
    }

    fn type_text(&mut self, text: &str) {
        // The typed bytes must land on the child's stdin (the pty slave):
        // write to the master.
        self.master.write_typed(text);
    }

    /// Wait for a needle in the rendered grid; returns the raw byte
    /// offset at which it first became visible (the ordering oracle).
    fn wait_from_start(&mut self, needle: &str, what: &str) -> usize {
        self.master
            .wait_for(needle, what, ECHO_DEADLINE_MS * 3 + OPEN_STALL_MS * 2)
    }
}

impl Drop for InputLiveHarness {
    fn drop(&mut self) {
        // The teardown runs on every exit path — including the panic
        // unwinds the oracles below produce — so no product child ever
        // outlives the test that spawned it. The child's death ends the
        // mock's read loop (the serve thread joins only when its accept
        // landed; a child that died pre-connect leaves the listener
        // thread to the process exit instead of hanging the drop).
        if let Some(shutdown) = &self.cli_shutdown {
            shutdown.store(true, std::sync::atomic::Ordering::Release);
        }
        // Both launch paths own a separate group; /term shells inherit it.
        // Signal before reaping the leader so its PID cannot be reused.
        if let Ok(pid) = i32::try_from(self.child.id()) {
            if pid > 0 {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(socket) = &self.cli_socket {
            let _ = std::os::unix::net::UnixStream::connect(socket);
            if let Some(server) = self.server.take() {
                let joined = server.join();
                if !std::thread::panicking() {
                    joined.expect("CLI mock server must shut down cleanly");
                }
            }
        } else if self.connected.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
        }
    }
}

/// The rendered screen (a minimal ANSI grid renderer, the probe doc's
/// detection method): the TUI draws char-by-char with cursor moves, so a
/// needle must be matched against the RENDERED grid — a raw byte search
/// false-negatives whenever the wordmark, a name, or an editor echo is
/// painted glyph-by-glyph with cursor hops between them.
struct PtyReader {
    file: std::fs::File,
    /// The raw byte stream (kept for the panic dump).
    output: Vec<u8>,
    rows: usize,
    cols: usize,
    grid: Vec<Vec<char>>,
    row: usize,
    col: usize,
}

impl PtyReader {
    fn new(master: OwnedFd) -> PtyReader {
        let fd = master.as_raw_fd();
        fcntl(fd, F_SETFL(OFlag::O_NONBLOCK)).expect("pty master non-blocking");
        PtyReader {
            file: master.into(),
            output: Vec::new(),
            rows: PTY_ROWS,
            cols: PTY_COLS,
            grid: vec![vec![' '; PTY_COLS]; PTY_ROWS],
            row: 0,
            col: 0,
        }
    }

    fn write_typed(&mut self, text: &str) {
        self.file.write_all(text.as_bytes()).expect("type text");
    }

    fn put(&mut self, ch: char) {
        if self.row < self.rows && self.col < self.cols {
            self.grid[self.row][self.col] = ch;
        }
        self.col = (self.col + 1).min(self.cols - 1);
    }

    /// Feed the raw bytes through the grid: text lands at the cursor, CSI
    /// cursor/erase sequences move or clear it, and every other escape
    /// (SGR, private modes, OSC) is consumed.
    fn feed(&mut self, data: &[u8]) {
        let bytes = data;
        let mut i = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte == 0x1b {
                match bytes.get(i + 1) {
                    Some(b'[') => {
                        // CSI: consume the parameter bytes, then the final.
                        let mut j = i + 2;
                        let start = j;
                        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                            j += 1;
                        }
                        if j >= bytes.len() {
                            break;
                        }
                        self.csi(&String::from_utf8_lossy(&bytes[start..j]), bytes[j]);
                        i = j + 1;
                    }
                    Some(b']') => {
                        // OSC: consumed through its BEL or ST terminator
                        // (a fresh ESC starts a new sequence instead).
                        let mut j = i + 2;
                        let mut consumed = false;
                        while j < bytes.len() {
                            if bytes[j] == 0x07 {
                                i = j + 1;
                                consumed = true;
                                break;
                            }
                            if bytes[j] == 0x1b {
                                let terminator = bytes.get(j + 1) == Some(&b'\\');
                                i = if terminator { j + 2 } else { j };
                                consumed = true;
                                break;
                            }
                            j += 1;
                        }
                        if !consumed {
                            i = j;
                        }
                    }
                    // A two-byte escape the grid does not model (ESC (, ESC =,
                    // ...): skip the pair.
                    Some(_) => i += 2,
                    None => break,
                }
                continue;
            }
            match byte {
                b'\n' => {
                    self.row = (self.row + 1).min(self.rows - 1);
                    i += 1;
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                byte if byte < 0x20 => i += 1,
                byte if byte < 0x80 => {
                    self.put(byte as char);
                    i += 1;
                }
                _ => {
                    // Multi-byte UTF-8: decode the full code point.
                    let len = if byte < 0xe0 {
                        2
                    } else if byte < 0xf0 {
                        3
                    } else {
                        4
                    };
                    let end = (i + len).min(bytes.len());
                    if let Ok(text) = std::str::from_utf8(&bytes[i..end]) {
                        if let Some(ch) = text.chars().next() {
                            self.put(ch);
                        }
                    }
                    i = end;
                }
            }
        }
    }

    fn csi(&mut self, params: &str, final_byte: u8) {
        let params = params.trim_start_matches(['?', '>', '<', '=']);
        let number = |index: usize, default: usize| -> usize {
            params
                .split(';')
                .nth(index)
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(default)
        };
        match final_byte {
            b'H' | b'f' => {
                self.row = number(0, 1).saturating_sub(1).min(self.rows - 1);
                self.col = number(1, 1).saturating_sub(1).min(self.cols - 1);
            }
            b'A' => self.row = self.row.saturating_sub(number(0, 1)),
            b'B' => self.row = (self.row + number(0, 1)).min(self.rows - 1),
            b'C' => self.col = (self.col + number(0, 1)).min(self.cols - 1),
            b'D' => self.col = self.col.saturating_sub(number(0, 1)),
            b'G' => self.col = number(0, 1).saturating_sub(1).min(self.cols - 1),
            b'J' => {
                let mode = number(0, 0);
                if mode == 2 || mode == 3 {
                    self.grid = vec![vec![' '; self.cols]; self.rows];
                }
            }
            _ => {}
        }
    }

    /// The rendered grid as text (trailing blanks trimmed per row).
    fn rendered(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait for actual PTY readability, with a deadline used only to fail a missing event.
    fn wait_for_event(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.rendered().contains(needle) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "PTY never rendered {needle:?}: {}",
                self.rendered()
            );
            let mut fd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `fd` points at one initialized pollfd and is valid for this call.
            let ready = unsafe {
                libc::poll(
                    &raw mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            assert!(
                ready > 0,
                "PTY failed waiting for {needle:?}: {}",
                self.rendered()
            );
            let mut buffer = [0u8; 8192];
            match self.file.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    self.output.extend_from_slice(&buffer[..n]);
                    self.feed(&buffer[..n]);
                }
                Ok(_) | Err(_) if fd.revents & libc::POLLHUP != 0 => {
                    panic!("PTY closed before {needle:?}: {}", self.rendered());
                }
                Ok(_) | Err(_) => {}
            }
        }
    }

    /// Match bytes emitted after a shell phase, not text left in the simplified VT grid.
    fn wait_for_raw_since(&mut self, from: usize, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let needle = needle.as_bytes();
        assert!(!needle.is_empty() && from <= self.output.len());
        while !self.output[from..]
            .windows(needle.len())
            .any(|bytes| bytes == needle)
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "PTY never emitted {needle:?} after offset {from}: {}",
                self.rendered()
            );
            let mut fd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: fd points at one initialized pollfd for this call.
            let ready = unsafe {
                libc::poll(
                    &raw mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            assert!(
                ready > 0,
                "PTY failed emitting {needle:?} after offset {from}: {}",
                self.rendered()
            );
            let mut buffer = [0u8; 8192];
            match self.file.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    self.output.extend_from_slice(&buffer[..n]);
                    self.feed(&buffer[..n]);
                }
                Ok(_) | Err(_) if fd.revents & libc::POLLHUP != 0 => {
                    panic!("PTY closed before {needle:?}: {}", self.rendered());
                }
                Ok(_) | Err(_) => {}
            }
        }
    }

    /// Drain the master until the needle appears in the RENDERED grid,
    /// bounded by the given window; returns the raw byte offset at which
    /// the needle first became visible (the ordering oracle's evidence).
    fn wait_for(&mut self, needle: &str, what: &str, window_ms: u64) -> usize {
        let deadline = Instant::now() + Duration::from_millis(window_ms);
        let mut seen_at = None;
        loop {
            if seen_at.is_none() && self.rendered().contains(needle) {
                seen_at = Some(self.output.len());
            }
            if let Some(at) = seen_at {
                return at;
            }
            let mut buffer = [0u8; 8192];
            match self.file.read(&mut buffer) {
                Ok(0) | Err(_) => {}
                Ok(n) => {
                    self.output.extend_from_slice(&buffer[..n]);
                    self.feed(&buffer[..n]);
                }
            }
            if Instant::now() > deadline {
                let screen = self.rendered();
                let stream = String::from_utf8_lossy(&self.output);
                panic!("timeout waiting for {what} (needle {needle:?}); rendered screen:\n{screen}\npty stream:\n{stream}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The mock's recorded commands: one line per received command envelope.
fn find_prompt_with(commands: &Arc<Mutex<Vec<String>>>, text: &str) -> Option<String> {
    commands
        .lock()
        .expect("commands")
        .iter()
        .find(|line| line.contains("\"prompt\"") && line.contains(text))
        .cloned()
}

/// A child process group of this very binary, re-executed in child mode
/// with the pty slave as its terminal and no tmux.
fn spawn_child(
    socket: &std::path::Path,
    slave: &OwnedFd,
    shell: Option<&std::path::Path>,
) -> Child {
    fn make_process_group() -> std::io::Result<()> {
        nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))?;
        Ok(())
    }
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .arg("--exact")
        .arg("input_live_open_child_mode")
        .env(CHILD_SOCKET_ENV, socket)
        .env_remove("TMUX")
        .env("PS1", "PA_TERM_READY>")
        .stdin(slave_as_stdio(slave))
        .stdout(slave_as_stdio(slave))
        .stderr(slave_as_stdio(slave));
    if let Some(shell) = shell {
        command.env("SHELL", shell);
    }
    // SAFETY: the pre_exec hook is the supported std seam for
    // process-group setup; it runs post-fork pre-exec in the child only
    // and cannot disturb this process.
    unsafe { command.pre_exec(make_process_group) };
    command.spawn().expect("spawn child")
}

fn spawn_cli_child(
    socket: &std::path::Path,
    slave: &OwnedFd,
    private: &std::path::Path,
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    fallback_shell: &std::path::Path,
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    command
        .args([
            "--daemon-socket",
            socket.to_str().expect("socket UTF-8"),
            "--resume",
            "s1",
        ])
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", private)
        .env("XDG_CONFIG_HOME", private.join("config"))
        .env("XDG_DATA_HOME", private.join("data"))
        .env("XDG_CACHE_HOME", private.join("cache"))
        .env("TMPDIR", private)
        .env("PRIME_AGENT_CODING_AGENT_DIR", agent_dir)
        .env("DO_NOT_TRACK", "1")
        .env("LANG", "C.UTF-8")
        .env("TERM", "xterm-256color")
        .env("PS1", "PA_TERM_READY>")
        .env("HC_TERMINAL_ID", "isolated-osc-test")
        .env("HC_TERMINAL_SOCKET", "/tmp/unused-osc-test.sock")
        .env("SHELL", fallback_shell)
        .stdin(slave_as_stdio(slave))
        .stdout(slave_as_stdio(slave))
        .stderr(slave_as_stdio(slave));
    // SAFETY: only the child creates a new process group after fork.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))?;
            Ok(())
        });
    }
    command.spawn().expect("spawn real CLI")
}

fn slave_as_stdio(slave: &OwnedFd) -> Stdio {
    slave.try_clone().expect("clone pty slave").into()
}

fn child_options(socket: PathBuf) -> InteractiveOptions {
    InteractiveOptions {
        models: None,
        socket_path: socket,
        cwd: PathBuf::from("/tmp"),
        session_dir: None,
        script_path: None,
        model_selection: ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        // The fresh-launch flow: a NEW session, so the startup chrome
        // mounts before the open (the exact surface the report measured).
        session: SessionSelection::New,
        initial_message: None,
        show_images: true,
        fullscreen_mouse: true,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
    }
}

/// The supervisor stand-in: the `create` answers immediately, the
/// `attach` stalls past the echo deadline, everything else answers
/// generically. Every received command is recorded for the typed-ahead
/// submit assertion.
struct MockSupervisor {
    listener: std::os::unix::net::UnixListener,
    socket_path: std::path::PathBuf,
    cwd_capable: bool,
    cli_cwd: Option<PathBuf>,
    cli_shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    osc_events: Option<std::sync::mpsc::Sender<std::os::unix::net::UnixStream>>,
    attach_busy: bool,
    osc_requests: Option<std::sync::mpsc::Sender<(String, String)>>,
}

impl MockSupervisor {
    fn bind(socket: &std::path::Path, cwd_capable: bool) -> MockSupervisor {
        MockSupervisor {
            listener: std::os::unix::net::UnixListener::bind(socket).expect("bind mock socket"),
            socket_path: socket.to_path_buf(),
            cwd_capable,
            cli_cwd: None,
            cli_shutdown: None,
            osc_events: None,
            attach_busy: false,
            osc_requests: None,
        }
    }

    fn serve_cli(
        self,
        commands: &Arc<Mutex<Vec<String>>>,
        connected: &std::sync::atomic::AtomicBool,
    ) {
        let (mut probe, _) = self.listener.accept().expect("CLI probe accept");
        connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let shutdown = self.cli_shutdown.as_ref().expect("CLI shutdown flag");
        if shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let hello = json!({
            "type": "daemon_hello", "socketPath": self.socket_path.display().to_string(),
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "schemaId": pa_types::daemon::DAEMON_SCHEMA_ID,
            "schemaRevision": pa_types::daemon::DAEMON_SCHEMA_REVISION,
            "serverCapabilities": ["kernel_bash_activity", "kernel_cwd_propagation"],
            "clientId": "mock",
        });
        if let Err(error) = try_write_json(&mut probe, &hello) {
            if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            panic!("CLI probe hello failed: {error}");
        }
        drop(probe);
        self.serve(commands, connected);
    }

    fn serve(self, commands: &Arc<Mutex<Vec<String>>>, connected: &std::sync::atomic::AtomicBool) {
        let (stream, _) = self.listener.accept().expect("accept");
        connected.store(true, std::sync::atomic::Ordering::SeqCst);
        if self
            .cli_shutdown
            .as_ref()
            .is_some_and(|shutdown| shutdown.load(std::sync::atomic::Ordering::Acquire))
        {
            return;
        }
        let write_stream = stream.try_clone().expect("clone mock socket");
        if let Some(tx) = &self.osc_events {
            tx.send(write_stream.try_clone().expect("OSC event stream"))
                .expect("OSC controller");
        }
        let mut writer = write_stream;
        let mut reader = BufReader::new(stream);

        let hello = json!({
            "type": "daemon_hello",
            "socketPath": self.socket_path.display().to_string(),
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": if self.cwd_capable {
                vec!["kernel_bash_activity", "kernel_cwd_propagation"]
            } else { vec!["kernel_bash_activity"] },
            "schemaRevision": pa_types::daemon::DAEMON_SCHEMA_REVISION,
            "schemaId": pa_types::daemon::DAEMON_SCHEMA_ID,
            "clientId": "mock",
        });
        if let Some(shutdown) = &self.cli_shutdown {
            if let Err(error) = try_write_json(&mut writer, &hello) {
                if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                panic!("CLI UI hello failed: {error}");
            }
        } else {
            write_json(&mut writer, &hello);
        }

        let mut line = String::new();
        let mut osc_wait_seen = false;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(envelope) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let id = envelope.get("id").and_then(Value::as_str).unwrap_or("");
            let command = envelope.get("command").cloned().unwrap_or(Value::Null);
            let command_type = command
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            commands
                .lock()
                .expect("commands")
                .push(format!("{command_type}: {command}"));
            match command_type.as_str() {
                "wait_for_idle" if self.osc_requests.is_some() => {
                    osc_wait_seen = true;
                    self.osc_requests
                        .as_ref()
                        .unwrap()
                        .send((id.to_string(), command_type))
                        .unwrap();
                }
                "get_state" if osc_wait_seen => {
                    self.osc_requests
                        .as_ref()
                        .unwrap()
                        .send((id.to_string(), command_type))
                        .unwrap();
                }
                "create" => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "create",
                            "success": true,
                            "data": { "activeSessionId": "s1", "sessionId": "sess-1" },
                        }),
                    );
                }
                "attach" => {
                    if self.cli_cwd.is_none() {
                        // Existing open-latency fixture holds attach past its echo deadline.
                        std::thread::sleep(Duration::from_millis(OPEN_STALL_MS));
                    }
                    let mut attached = attach_data(id);
                    attached["data"]["snapshot"]["state"]["isStreaming"] = json!(self.attach_busy);
                    if let Some(cwd) = &self.cli_cwd {
                        let cwd = cwd.display().to_string();
                        attached["data"]["snapshot"]["summary"]["cwd"] = json!(cwd);
                        attached["data"]["snapshot"]["state"]["cwd"] = json!(cwd);
                    }
                    write_json(&mut writer, &attached);
                }
                "shutdown" | "list" if self.cli_cwd.is_some() => {
                    panic!("CLI tried to replace its compatible mock daemon: {command_type}");
                }
                "get_commands" | "get_model_catalog" | "roster_subscribe" | "heartbeats_list"
                    if self.cli_cwd.is_some() =>
                {
                    let data = match command_type.as_str() {
                        "get_commands" => json!({ "commands": [] }),
                        "get_model_catalog" => json!({ "models": [], "configuredProviders": [] }),
                        "roster_subscribe" => json!({ "roster": [] }),
                        "heartbeats_list" => json!({ "heartbeats": [] }),
                        _ => unreachable!(),
                    };
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response", "id": id, "command": command_type,
                            "success": true, "data": data,
                        }),
                    );
                }
                "get_session_stats" if self.cli_cwd.is_some() => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response", "id": id, "command": command_type,
                            "success": true,
                            "data": { "contextUsage": { "tokens": 0, "contextWindow": 200_000 }, "cost": 0 },
                        }),
                    );
                }
                _ => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": command_type,
                            "success": true,
                            "data": {},
                        }),
                    );
                    if command_type == "set_kernel_cwd" {
                        write_json(
                            &mut writer,
                            &json!({
                                "type": "session_event", "activeSessionId": "s1",
                                "event": {"type": "kernel_cwd_changed", "cwd": command["dir"]},
                                "meta": {"sequence": 1},
                            }),
                        );
                    }
                }
            }
        }
    }
}

fn try_write_json(
    writer: &mut std::os::unix::net::UnixStream,
    value: &Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

fn write_json(writer: &mut std::os::unix::net::UnixStream, value: &Value) {
    try_write_json(writer, value).expect("write mock frame");
}

/// The attach result: one live session whose transcript carries a marker
/// row — the content frame the fold paints once the stall ends.
fn attach_data(id: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "attach",
        "success": true,
        "data": {
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": "s1",
            "snapshot": {
                "activeSessionId": "s1",
                "summary": { "id": "s1", "cwd": "/tmp" },
                "state": {
                    "activeSessionId": "s1",
                    "cwd": "/tmp",
                    "sessionId": "sess-1",
                    "sessionName": "input live probe",
                    "model": "faux-1",
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [
                    { "role": "user", "content": "openprobe marker question", "timestamp": 1 },
                    { "role": "assistant", "content": "openprobe settled answer", "provider": "scripted", "model": "faux-1", "timestamp": 2 },
                ],
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}

#[test]
fn term_handoff_propagates_changed_cwd_after_nonzero_shell_exit() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let target = tempfile::tempdir().expect("target directory");
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness = InputLiveHarness::start(Arc::clone(&commands));
    harness.wait_for_event("openprobe settled answer");
    harness.type_text("/term\r");
    harness.wait_for_event("PA_TERM_READY>");
    let name = target.path().file_name().unwrap().to_string_lossy();
    harness.type_text(&format!("cd \"$TMPDIR/{name}\"; exit 23\r"));
    let physical_target = target.path().canonicalize().expect("physical target");
    harness.wait_for_event(&physical_target.to_string_lossy());
    let seen = commands.lock().expect("mock commands");
    let changes: Vec<_> = seen
        .iter()
        .filter(|line| line.starts_with("set_kernel_cwd:"))
        .collect();
    assert_eq!(changes.len(), 1, "one changed-CWD gate: {seen:?}");
    assert!(changes[0].contains(&physical_target.to_string_lossy().to_string()));
    assert!(
        !seen.iter().any(|line| line.starts_with("prompt:")),
        "/term is local: {seen:?}"
    );
}

#[test]
fn term_missing_shell_restores_tui_input() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().expect("private path");
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness =
        InputLiveHarness::start_with_shell(commands, Some(&dir.path().join("missing-shell")));
    harness.wait_for_event("openprobe settled answer");
    harness.type_text("/term\r");
    harness.wait_for_event("Shell handoff failed");
    let termios = nix::sys::termios::tcgetattr(&harness.master.file).expect("pty terminal modes");
    assert!(
        !termios
            .local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON),
        "the TUI must restore raw mode after a spawn failure"
    );
    harness.type_text("input-still-alive");
    harness.wait_for_event("input-still-alive");
}

#[test]
fn term_old_daemon_warns_and_keeps_effective_cwd() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let target = tempfile::tempdir().expect("target directory");
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness = InputLiveHarness::start_with_capability(Arc::clone(&commands), None, false);
    harness.wait_for_event("openprobe settled answer");
    let first_shell_phase = harness.master.output.len();
    harness.type_text("/term\r");
    harness
        .master
        .wait_for_raw_since(first_shell_phase, "PA_TERM_READY>");
    let name = target.path().file_name().unwrap().to_string_lossy();
    harness.type_text(&format!("cd \"$TMPDIR/{name}\"; exit 23\r"));
    harness.wait_for_event("does not support kernel CWD propagation");
    let seen = commands.lock().expect("mock commands");
    assert!(
        !seen.iter().any(|line| line.starts_with("set_kernel_cwd:")),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|line| line.starts_with("prompt:")),
        "{seen:?}"
    );
    drop(seen);
    let second_shell_phase = harness.master.output.len();
    harness.type_text("/term\r");
    harness
        .master
        .wait_for_raw_since(second_shell_phase, "PA_TERM_READY>");
    let launch_phase = harness.master.output.len();
    harness.type_text("printf 'launch=%s\n' \"$(pwd -P)\"; exit 0\r");
    let physical_start = std::path::Path::new("/tmp")
        .canonicalize()
        .expect("physical start");
    harness.master.wait_for_raw_since(
        launch_phase,
        &format!("launch={}", physical_start.display()),
    );
}

#[test]
fn term_cli_settings_are_read_at_each_command() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let private = tempfile::tempdir().expect("private CLI settings");
    let cwd = private.path().join("cwd");
    let agent_dir = private.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("private cwd");
    std::fs::create_dir_all(&agent_dir).expect("private agent dir");
    let settings = agent_dir.join("settings.json");
    let wrappers: Vec<_> = ["A", "B", "C"]
        .iter()
        .map(|name| {
            let path = private.path().join(format!("shell-{name}"));
            std::fs::write(
                &path,
                format!("#!/bin/sh\nPS1='WRAPPER_{name}_READY>'\nexport PS1\nexec /bin/sh -i\n"),
            )
            .expect("write shell wrapper");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("executable shell wrapper");
            path
        })
        .collect();
    std::fs::write(
        &settings,
        serde_json::to_vec(&json!({
            "onboardingShown": true, "telemetryEnabled": false,
            "unrelatedSettingsFixture": { "kept": true },
        }))
        .expect("initial settings JSON"),
    )
    .expect("initialize private settings");
    let write_settings = |shell: Option<&std::path::Path>| {
        let mut value: Value =
            serde_json::from_slice(&std::fs::read(&settings).expect("read settings"))
                .expect("parse existing settings");
        let entries = value.as_object_mut().expect("settings object");
        if let Some(shell) = shell {
            entries.insert("shellPath".into(), json!(shell.display().to_string()));
        } else {
            entries.remove("shellPath");
        }
        std::fs::write(
            &settings,
            serde_json::to_vec(&value).expect("settings JSON"),
        )
        .expect("write settings");
        assert_eq!(value["unrelatedSettingsFixture"]["kept"], true);
    };
    write_settings(Some(&wrappers[0]));
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness = InputLiveHarness::start_cli(
        Arc::clone(&commands),
        private.path(),
        &cwd,
        &agent_dir,
        &wrappers[1],
    );
    harness.wait_for_event("openprobe settled answer");
    for (name, selected) in [
        ("A", Some(wrappers[0].as_path())),
        ("B", None),
        ("C", Some(wrappers[2].as_path())),
    ] {
        write_settings(selected);
        let phase = harness.master.output.len();
        harness.type_text("/term\r");
        harness
            .master
            .wait_for_raw_since(phase, &format!("WRAPPER_{name}_READY>"));
        let launch_phase = harness.master.output.len();
        harness.type_text("printf 'launch=%s\\n' \"$(pwd -P)\"\r");
        let physical_cwd = cwd.canonicalize().expect("physical launch cwd");
        harness
            .master
            .wait_for_raw_since(launch_phase, &format!("launch={}", physical_cwd.display()));
        let restore = harness.master.output.len();
        harness.type_text("exit 0\r");
        harness.master.wait_for_raw_since(restore, "Collapsed mode");
    }
    let seen = commands.lock().expect("mock commands");
    assert!(
        !seen.iter().any(|line| line.starts_with("prompt:")),
        "/term is local: {seen:?}"
    );
    assert!(
        !seen.iter().any(|line| line.starts_with("set_kernel_cwd:")),
        "unchanged cwd: {seen:?}"
    );
}

#[test]
fn term_refuses_while_turn_is_running() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness = InputLiveHarness::start(Arc::clone(&commands));
    harness.wait_for_event("openprobe settled answer");
    harness.type_text("busy-turn\r");
    harness.wait_for_event("busy-turn");
    harness.type_text("/term\r");
    harness.wait_for_event("Wait for current work to finish before /term");
    let seen = commands.lock().expect("mock commands");
    assert!(
        !seen.iter().any(|line| line.starts_with("set_kernel_cwd:")),
        "{seen:?}"
    );
}

fn read_shell_pid(reader: &mut std::fs::File) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut receipt = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "shell never wrote its PID");
        let mut fd = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd points to one initialized pollfd for this bounded readiness call.
        let ready = unsafe {
            libc::poll(
                &raw mut fd,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        assert_eq!(ready, 1, "shell FIFO readiness");
        let mut bytes = [0u8; 32];
        let count = reader.read(&mut bytes).expect("shell PID receipt");
        assert!(count > 0, "shell FIFO closed before PID receipt");
        receipt.extend_from_slice(&bytes[..count]);
        assert!(receipt.len() < 32, "shell PID receipt too long");
        if receipt.ends_with(b"\n") {
            return std::str::from_utf8(&receipt)
                .expect("shell PID UTF-8")
                .trim()
                .parse()
                .expect("shell PID number");
        }
    }
}

fn shell_fifo_closed(reader: &mut std::fs::File) -> bool {
    let mut fd = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    // SAFETY: fd points to one initialized pollfd; timeout only bounds a missing EOF.
    let ready = unsafe { libc::poll(&raw mut fd, 1, 2_000) };
    let mut byte = [0u8; 1];
    ready == 1 && matches!(reader.read(&mut byte), Ok(0))
}

fn check_term_panic_teardown(real_cli: bool) -> Result<(), String> {
    let private = tempfile::tempdir().expect("private shell witness");
    let fifo = private.path().join("shell-witness.fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("private FIFO");
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .expect("nonblocking FIFO reader");
    let dummy_writer = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .expect("private FIFO writer prevents premature HUP");
    let shell = private.path().join("shell-observer");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\nexec 3>'{}'\nprintf '%s\\n' \"$$\" >&3\nPS1='PA_UNWIND_READY>'\nexport PS1\nexec /bin/sh -i\n",
            fifo.display()
        ),
    )
    .expect("shell wrapper");
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700))
        .expect("executable shell wrapper");
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut harness = if real_cli {
        let cwd = private.path().join("cwd");
        let agent_dir = private.path().join("agent");
        std::fs::create_dir_all(&cwd).expect("private cwd");
        std::fs::create_dir_all(&agent_dir).expect("private agent dir");
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_vec(&json!({
                "onboardingShown": true,
                "telemetryEnabled": false,
                "shellPath": shell.display().to_string(),
            }))
            .expect("settings JSON"),
        )
        .expect("private settings");
        InputLiveHarness::start_cli(commands, private.path(), &cwd, &agent_dir, &shell)
    } else {
        InputLiveHarness::start_with_shell(commands, Some(&shell))
    };
    let child_pid = i32::try_from(harness.child.id()).expect("positive child PID");
    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness.wait_for_event("openprobe settled answer");
        let phase = harness.master.output.len();
        harness.type_text("/term\r");
        let shell_pid = read_shell_pid(&mut reader);
        assert!(
            shell_pid > 0 && shell_pid != child_pid,
            "actual descendant PID"
        );
        harness.master.wait_for_raw_since(phase, "PA_UNWIND_READY>");
        shell_pid
    }));
    let Ok(shell_pid) = prepared else {
        // Rescue setup failure while the group leader is still owned and unreaped.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child_pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        drop(harness);
        return Err("shell readiness failed; owned group rescued".to_string());
    };
    drop(dummy_writer);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _harness = harness;
        assert_eq!(shell_pid, child_pid, "intentional parent assertion unwind");
    }));
    let mut status = 0;
    // SAFETY: waitpid checks only this test's direct child after fixture Drop.
    let waited = unsafe { libc::waitpid(child_pid, &raw mut status, libc::WNOHANG) };
    let reaped =
        waited == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    let completed = shell_fifo_closed(&mut reader);
    if !completed {
        // Only the independently identified shell is rescued after recording failure.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(shell_pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        if !shell_fifo_closed(&mut reader) {
            return Err(format!(
                "shell {shell_pid} did not close its FIFO after rescue"
            ));
        }
    }
    if unwound.is_ok() || !reaped || !completed {
        return Err(format!(
            "panic={}; direct_child_reaped={reaped}; shell_fifo_eof={completed}",
            unwound.is_err()
        ));
    }
    Ok(())
}

#[test]
fn task4_term_panic_reaps_shell_descendant_for_both_constructors() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let results = [
        ("reexecuted", check_term_panic_teardown(false)),
        ("real CLI", check_term_panic_teardown(true)),
    ];
    assert_eq!(results, [("reexecuted", Ok(())), ("real CLI", Ok(()))]);
}

#[path = "support/herdr_app_osc.rs"]
mod herdr_app_osc;
