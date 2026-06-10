//! End-to-end coverage for the workspace supervisor working-set core.
//!
//! Each test lays out a workspace root (`<temp>/ws/.zaz/`, no `zaz.toml`) with
//! member projects under it. Members carry their own `.zaz/` directory so their
//! daemon sockets resolve to `<member>/.zaz/daemon.sock` deterministically,
//! independent of `$HOME`. `HOME` is still overridden per test so nothing
//! escapes into the real user state directory.
//!
//! The supervisor is driven through the real `zaz` binary: `zaz start` with 2+
//! `--config` flags launches it, and `zaz stop --socket <ws>` tears it down.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use zaz_daemon::socket_path_for_config;

fn zaz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zaz")
}

/// A workspace layout under a tempdir: the workspace root holds `.zaz/` but no
/// config; each member is its own project directory with a `.zaz/` and a config.
struct Workspace {
    _temp: TempDir,
    home: PathBuf,
    root: PathBuf,
    ws_socket: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("home dir");
        let root = temp.path().join("ws");
        std::fs::create_dir_all(root.join(".zaz")).expect("workspace root .zaz");
        let ws_socket = root.join(".zaz").join("daemon.sock");
        Self {
            _temp: temp,
            home,
            root,
            ws_socket,
        }
    }

    /// Create a member project directory with its own `.zaz/` and config, and
    /// return its config path.
    fn member(&self, name: &str, body: &str) -> PathBuf {
        let dir = self.root.join(name);
        std::fs::create_dir_all(dir.join(".zaz")).expect("member .zaz");
        let config = dir.join("zaz.toml");
        std::fs::write(&config, body).expect("write member config");
        config
    }

    fn member_socket(&self, config: &Path) -> PathBuf {
        socket_path_for_config(config)
    }

    fn run(&self, args: &[&OsStr]) -> Output {
        Command::new(zaz_bin())
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .output()
            .expect("run zaz")
    }

    /// `zaz start` with each member config plus an explicit workspace socket.
    fn start_workspace(&self, members: &[&Path]) -> Output {
        let mut args: Vec<&OsStr> = Vec::new();
        for m in members {
            args.push(OsStr::new("-c"));
            args.push(m.as_os_str());
        }
        args.push(OsStr::new("--socket"));
        args.push(self.ws_socket.as_os_str());
        args.push(OsStr::new("start"));
        self.run(&args)
    }

    /// `zaz start` for a single member, taking the ordinary single-config path
    /// against the member's own socket.
    fn start_single(&self, config: &Path) -> Output {
        let socket = self.member_socket(config);
        self.run(&[
            OsStr::new("-c"),
            config.as_os_str(),
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("start"),
        ])
    }

    fn status(&self, socket: &Path) -> Output {
        self.run(&[
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("status"),
        ])
    }

    fn stop(&self, socket: &Path) -> Output {
        self.run(&[
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("stop"),
        ])
    }

    fn status_running(&self, socket: &Path) -> bool {
        let out = self.status(socket);
        out.status.code() == Some(0)
            && String::from_utf8_lossy(&out.stdout).contains("Daemon Status:")
    }

    fn wait_running(&self, socket: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.status_running(socket) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn wait_socket_gone(&self, socket: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !socket.exists() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }
}

/// A minimal valid config whose daemon stays alive for the duration of a test.
fn long_running_config() -> &'static str {
    r#"
[[group]]
name = "g"
patterns = []

[[group.daemon]]
name = "d"
command = "sleep 600"
"#
}

/// A config that fails validation (`deny_unknown_fields`), so attaching its
/// member must fail.
fn invalid_config() -> &'static str {
    r#"
[[group]]
name = "g"
patterns = []
bogus_field = true
"#
}

/// Best-effort teardown: stopping an already-stopped socket is idempotent.
fn cleanup(ws: &Workspace, sockets: &[&Path]) {
    for socket in sockets {
        let _ = ws.stop(socket);
    }
}

#[test]
fn workspace_boot_attach_loop_spawns_member_daemons() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    let out = ws.start_workspace(&[&a, &b]);
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("workspace supervisor started"));

    assert!(
        ws.wait_running(&ws.ws_socket, Duration::from_secs(10)),
        "supervisor socket not responsive"
    );
    assert!(
        ws.wait_running(&a_sock, Duration::from_secs(10)),
        "member a not spawned"
    );
    assert!(
        ws.wait_running(&b_sock, Duration::from_secs(10)),
        "member b not spawned"
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_adopts_live_member_without_killing_it() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    // Pre-start member a as its own single-config daemon.
    let out = ws.start_single(&a);
    assert!(out.status.success(), "single start failed");
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));
    let a_inode_before = std::fs::metadata(&a_sock).expect("a socket metadata").ino();

    // Workspace start over a (already live) and b (new). a must be adopted, not
    // killed and replaced: its socket file is left untouched.
    let out = ws.start_workspace(&[&a, &b]);
    assert!(out.status.success(), "workspace start failed");
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));
    assert!(
        ws.wait_running(&b_sock, Duration::from_secs(10)),
        "b not spawned"
    );

    assert!(ws.status_running(&a_sock), "adopted member a went down");
    let a_inode_after = std::fs::metadata(&a_sock).expect("a socket metadata").ino();
    assert_eq!(
        a_inode_before, a_inode_after,
        "adopted member socket was recreated (killed and replaced)"
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_attach_failure_leaves_running_set_undisturbed() {
    let ws = Workspace::new();
    let good = ws.member("good", long_running_config());
    let bad = ws.member("bad", invalid_config());
    let good_sock = ws.member_socket(&good);
    let bad_sock = ws.member_socket(&bad);

    let out = ws.start_workspace(&[&good, &bad]);
    assert!(
        out.status.success(),
        "supervisor should still come up: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        ws.wait_running(&ws.ws_socket, Duration::from_secs(10)),
        "supervisor not responsive"
    );
    assert!(
        ws.wait_running(&good_sock, Duration::from_secs(10)),
        "valid member not attached"
    );
    assert!(
        !ws.status_running(&bad_sock),
        "invalid member should not have a daemon"
    );

    cleanup(&ws, &[&ws.ws_socket, &good_sock, &bad_sock]);
}

#[test]
fn workspace_shutdown_stops_spawned_leaves_adopted() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    // a is pre-started and adopted; b is spawned by the supervisor.
    assert!(ws.start_single(&a).status.success());
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));
    assert!(ws.wait_running(&b_sock, Duration::from_secs(10)));

    let stop = ws.stop(&ws.ws_socket);
    assert!(stop.status.success(), "workspace stop failed");

    assert!(
        ws.wait_socket_gone(&b_sock, Duration::from_secs(10)),
        "spawned member b was not stopped"
    );
    assert!(
        ws.status_running(&a_sock),
        "adopted member a should be left running"
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_socket_resolves_via_zaz_root_walk() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    // No --socket: resolution must walk up to the `.zaz/` workspace root and
    // bind `<root>/.zaz/daemon.sock`.
    let out = ws.run(&[
        OsStr::new("-c"),
        a.as_os_str(),
        OsStr::new("-c"),
        b.as_os_str(),
        OsStr::new("start"),
    ]);
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        ws.wait_running(&ws.ws_socket, Duration::from_secs(10)),
        "supervisor not bound at the workspace root socket"
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn single_config_start_is_unchanged() {
    let ws = Workspace::new();
    let a = ws.member("solo", long_running_config());
    let a_sock = ws.member_socket(&a);

    // One --config takes the ordinary single-config path: an engine daemon, not
    // a supervisor. Its status reports the configured group.
    let out = ws.start_single(&a);
    assert!(out.status.success(), "single start failed");
    assert!(String::from_utf8_lossy(&out.stdout).contains("daemon started"));
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));

    let status = ws.status(&a_sock);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("g") && stdout.contains("Groups:"),
        "single-config daemon should report its engine group state: {stdout}"
    );

    cleanup(&ws, &[&a_sock]);
}
