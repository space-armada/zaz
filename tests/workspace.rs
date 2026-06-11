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
        self.run_in(&self.root, args)
    }

    /// Run `zaz` with `dir` as the working directory, so socket resolution walks
    /// up from a member directory rather than the workspace root.
    fn run_in(&self, dir: &Path, args: &[&OsStr]) -> Output {
        Command::new(zaz_bin())
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .output()
            .expect("run zaz")
    }

    /// Start a single-config daemon for `config` bound at an arbitrary `socket`,
    /// independent of the config's own resolved socket. Used to plant a foreign
    /// daemon at a member's socket path.
    fn start_single_at(&self, config: &Path, socket: &Path) -> Output {
        self.run(&[
            OsStr::new("-c"),
            config.as_os_str(),
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("start"),
        ])
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

    fn restart(&self, socket: &Path, target: Option<&str>) -> Output {
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("restart"),
        ];
        if let Some(target) = target {
            args.push(OsStr::new(target));
        }
        self.run(&args)
    }

    fn reload(&self, socket: &Path) -> Output {
        self.run(&[
            OsStr::new("--socket"),
            socket.as_os_str(),
            OsStr::new("reload"),
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

/// A long-running config that pins an explicit `[settings] name` project token.
fn named_config(name: &str) -> String {
    format!(
        r#"
[settings]
name = "{name}"

[[group]]
name = "g"
patterns = []

[[group.daemon]]
name = "d"
command = "sleep 600"
"#
    )
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

    // The identity check permits the legitimate same-config adopt, so the
    // adopted member is addressable through the supervisor.
    let routed = ws.restart(&ws.ws_socket, Some("a/g"));
    assert!(
        routed.status.success(),
        "adopted member should be routable: {}",
        String::from_utf8_lossy(&routed.stderr)
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_refuses_member_socket_bound_elsewhere() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let foreign = ws.member("foreign", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    // Plant a daemon serving `foreign` at member a's socket path. Attaching a
    // would otherwise adopt this foreign daemon and double-manage a config.
    assert!(ws.start_single_at(&foreign, &a_sock).status.success());
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));
    let a_inode_before = std::fs::metadata(&a_sock).expect("a socket metadata").ino();

    let out = ws.start_workspace(&[&a, &b]);
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
        ws.wait_running(&b_sock, Duration::from_secs(10)),
        "member b not spawned"
    );

    // The foreign daemon is left untouched: never killed, never replaced.
    assert!(ws.status_running(&a_sock), "foreign daemon was taken down");
    let a_inode_after = std::fs::metadata(&a_sock).expect("a socket metadata").ino();
    assert_eq!(
        a_inode_before, a_inode_after,
        "foreign daemon socket was recreated"
    );

    // Member a was refused, not adopted, so the supervisor cannot address it.
    let miss = ws.restart(&ws.ws_socket, Some("a/g"));
    assert!(!miss.status.success(), "refused member should not route");
    assert!(
        String::from_utf8_lossy(&miss.stderr).contains("unknown project"),
        "error should report the refused member as unknown: {}",
        String::from_utf8_lossy(&miss.stderr)
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn member_dir_command_reaches_member_daemon() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);
    let a_dir = a.parent().expect("member dir").to_path_buf();

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));

    // From inside member a's directory, with no --socket, resolution must land on
    // a's own child daemon, not the supervisor. The member engine reports its
    // configured group and daemon; the supervisor's status would not.
    let status = ws.run_in(&a_dir, &[OsStr::new("status")]);
    assert!(status.status.success(), "member-dir status failed");
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("[daemon] d"),
        "member-dir status should report the member engine state, got: {stdout}"
    );

    // A bare, unqualified group name restarts only against the member daemon; the
    // supervisor would reject it as malformed, so success proves member targeting.
    let restart = ws.run_in(&a_dir, &[OsStr::new("restart"), OsStr::new("g")]);
    assert!(
        restart.status.success(),
        "bare-name restart from member dir failed: {}",
        String::from_utf8_lossy(&restart.stderr)
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

#[test]
fn workspace_restart_group_routes_to_member() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));
    assert!(ws.wait_running(&b_sock, Duration::from_secs(10)));

    let out = ws.restart(&ws.ws_socket, Some("a/g"));
    assert!(
        out.status.success(),
        "qualified restart failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("a/g"),
        "response should re-qualify the group name: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Routing to a must not disturb b.
    assert!(
        ws.status_running(&a_sock),
        "member a went down after restart"
    );
    assert!(
        ws.status_running(&b_sock),
        "member b disturbed by restart of a"
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_restart_all_fans_out() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));
    assert!(ws.wait_running(&b_sock, Duration::from_secs(10)));

    let out = ws.restart(&ws.ws_socket, None);
    assert!(
        out.status.success(),
        "restart-all fan-out failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("2 project(s)"),
        "fan-out summary should mention project count: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_reload_fans_out() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));
    assert!(ws.wait_running(&b_sock, Duration::from_secs(10)));

    let out = ws.reload(&ws.ws_socket);
    assert!(
        out.status.success(),
        "reload fan-out failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("config reloaded"),
        "reload summary should mention config reloaded: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_unknown_project_errors() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));

    let out = ws.restart(&ws.ws_socket, Some("nope/g"));
    assert!(!out.status.success(), "unknown project should fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nope"),
        "error should name the unknown project: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_malformed_name_errors() {
    let ws = Workspace::new();
    let a = ws.member("a", long_running_config());
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));

    let out = ws.restart(&ws.ws_socket, Some("bareword"));
    assert!(!out.status.success(), "unqualified name should fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("project/group"),
        "error should name the expected format: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_explicit_name_overrides_basename() {
    let ws = Workspace::new();
    let a = ws.member("a", &named_config("frontend"));
    let b = ws.member("b", long_running_config());
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    assert!(ws.start_workspace(&[&a, &b]).status.success());
    assert!(ws.wait_running(&ws.ws_socket, Duration::from_secs(10)));
    assert!(ws.wait_running(&a_sock, Duration::from_secs(10)));

    // The explicit token addresses the member; the directory basename does not.
    let ok = ws.restart(&ws.ws_socket, Some("frontend/g"));
    assert!(
        ok.status.success(),
        "explicit-name restart failed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );

    let miss = ws.restart(&ws.ws_socket, Some("a/g"));
    assert!(
        !miss.status.success(),
        "basename should not address a member with an explicit name"
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}

#[test]
fn workspace_duplicate_token_aborts_startup() {
    let ws = Workspace::new();
    // Distinct directories, but both pin the same explicit token.
    let a = ws.member("a", &named_config("dup"));
    let b = ws.member("b", &named_config("dup"));
    let a_sock = ws.member_socket(&a);
    let b_sock = ws.member_socket(&b);

    let out = ws.start_workspace(&[&a, &b]);
    assert!(
        !out.status.success(),
        "duplicate project token should abort startup"
    );

    // The supervisor never binds its control socket, and the children it spawned
    // during the boot loop are torn down rather than orphaned.
    assert!(
        !ws.status_running(&ws.ws_socket),
        "supervisor should not be running after a collision abort"
    );
    assert!(
        ws.wait_socket_gone(&a_sock, Duration::from_secs(10)),
        "spawned child a leaked after collision abort"
    );
    assert!(
        ws.wait_socket_gone(&b_sock, Duration::from_secs(10)),
        "spawned child b leaked after collision abort"
    );

    cleanup(&ws, &[&ws.ws_socket, &a_sock, &b_sock]);
}
