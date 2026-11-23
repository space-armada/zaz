//! End-to-end coverage for `zaz env`.
//!
//! The command reads the loaded environment of a running daemon. Every value in these configs
//! carries the marker `zz-val`, so a test can prove that no value reaches the output.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use support::{run_zaz, stderr_string, stdout_string, StartedDaemon};
use tempfile::TempDir;
use zaz_daemon::ApiResponse;

const CONFIG: &str = r#"
[settings]
debounce = "20ms"
env_file = "base.env"

[[group]]
name = "api"
patterns = []
env_command = "cat group.json"
env = { INLINE_KEY = "zz-val-inline" }

[[group.task]]
name = "build"
command = "true"
env_file = "task.env"

[[group.service]]
name = "web"
command = "sleep 60"
env_command = "printf ''"

[[group]]
name = "plain"
patterns = []

[[group.task]]
name = "noop"
command = "true"
"#;

struct Project {
    _daemon: StartedDaemon,
    _temp: TempDir,
    dir: PathBuf,
    socket: PathBuf,
}

impl Project {
    fn start() -> Self {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        let socket = temp.path().join("daemon.sock");

        std::fs::write(dir.join("base.env"), "BASE_KEY=zz-val-base\n").unwrap();
        std::fs::write(dir.join("task.env"), "TASK_KEY=zz-val-task\n").unwrap();
        std::fs::write(
            dir.join("group.json"),
            r#"{"GROUP_KEY":"zz-val-group","BASE_KEY":null}"#,
        )
        .unwrap();
        std::fs::write(dir.join("zaz.toml"), CONFIG).unwrap();

        let daemon = StartedDaemon::launch(&dir, &dir.join("zaz.toml"), &socket);

        Self {
            _daemon: daemon,
            _temp: temp,
            dir,
            socket,
        }
    }

    fn env(&self, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["--socket", self.socket.to_str().unwrap(), "env"];
        args.extend_from_slice(extra);

        run_zaz(&self.dir, &args)
    }
}

#[test]
fn env_lists_key_names_and_sources_and_never_a_value() {
    let project = Project::start();

    let output = project.env(&[]);
    let stdout = stdout_string(&output);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert!(!stdout.contains("zz-val"), "{stdout}");
    assert!(stdout.starts_with("Loaded environment of the running config"));
    assert!(stdout.contains("group api (starts from the base environment)"));
    assert!(stdout.contains("settings env_file (base.env): sets BASE_KEY"));
    assert!(stdout
        .contains("group 'api' env_command (`cat group.json`): sets GROUP_KEY; unsets BASE_KEY"));
    assert!(stdout.contains("group 'api' env: sets INLINE_KEY"));
    assert!(stdout.contains("group 'api' task 'build' env_file (task.env): sets TASK_KEY"));
    assert!(
        stdout.contains("group 'api' service 'web' env_command (`printf ''`): produced no keys")
    );
    assert!(stdout.contains("group plain (starts from the daemon's environment)"));
}

#[test]
fn env_narrows_to_a_group_and_rejects_an_unknown_one() {
    let project = Project::start();

    let narrowed = stdout_string(&project.env(&["plain"]));
    assert!(narrowed.contains("group plain"));
    assert!(!narrowed.contains("group api"));

    let output = project.env(&["nope"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_string(&output).contains("no group named 'nope'"));
}

#[test]
fn env_shows_a_refresh_failure_and_the_environment_still_running() {
    let project = Project::start();

    std::fs::write(project.dir.join("base.env"), "not a valid line\n").unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let stdout = loop {
        let stdout = stdout_string(&project.env(&[]));
        if stdout.contains("Environment refresh error") {
            break stdout;
        }
        assert!(
            Instant::now() < deadline,
            "no refresh error shown: {stdout}"
        );
        thread::sleep(Duration::from_millis(50));
    };

    assert!(stdout.contains("running on the previous environment"));
    assert!(stdout.contains("settings env_file (base.env): sets BASE_KEY"));
    assert!(!stdout.contains("zz-val"));
}

#[test]
fn env_without_a_daemon_exits_3_on_stdout() {
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("none.sock");

    let output = run_zaz(temp.path(), &["--socket", socket.to_str().unwrap(), "env"]);

    assert_eq!(output.status.code(), Some(3));
    assert!(stdout_string(&output).contains("no daemon running"));
}

fn fake_older_daemon(socket: &Path) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).expect("bind fake socket");

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.contains("get_env"), "{line}");

        let answer = ApiResponse::error(
            "invalid request: unknown variant `get_env`, expected one of `status`, `subscribe`",
        );
        serde_json::to_writer(&mut stream, &answer).unwrap();
        stream.write_all(b"\n").unwrap();
    })
}

#[test]
fn env_reports_an_older_daemon_as_too_old() {
    let temp = TempDir::new().unwrap();
    let socket = temp.path().join("old.sock");
    let server = fake_older_daemon(&socket);

    let output = run_zaz(temp.path(), &["--socket", socket.to_str().unwrap(), "env"]);
    server.join().unwrap();
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("too old"), "{stderr}");
    assert!(stderr.contains("zaz stop"), "{stderr}");
}
