//! End-to-end coverage for watched environment sources.
//!
//! Each test runs a real daemon. Commands are stubs that append to a counter file, so a test can
//! prove whether a command ran. Tasks append the variables they see to a log, one line per run,
//! so a test can count how often a group was rebuilt and what it saw.
//!
//! A test never waits to prove that nothing happened. It makes a second change that must have an
//! effect and waits for that. Events arrive in order, so the first change is settled by then.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use support::{run_zaz, stdout_string, StartedDaemon};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(20);

struct Project {
    // The daemon is declared first so it stops before its directory goes away.
    daemon: Option<StartedDaemon>,
    _temp: TempDir,
    dir: PathBuf,
    socket: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().canonicalize().unwrap();

        Self::at(temp, dir)
    }

    fn at(temp: TempDir, dir: PathBuf) -> Self {
        let socket = temp.path().join("daemon.sock");

        Self {
            daemon: None,
            _temp: temp,
            dir,
            socket,
        }
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.dir.join(name);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    /// A stub that counts its runs in `<name>.count`, then runs `body`.
    fn stub(&self, name: &str, body: &str) -> String {
        let path = self.dir.join("bin").join(name);
        let count = self.dir.join(format!("{name}.count"));

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho run >> '{}'\n{body}\n", count.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        path.display().to_string()
    }

    fn runs(&self, stub: &str) -> usize {
        self.lines(&format!("{stub}.count")).len()
    }

    fn start(&mut self, config: &str) {
        self.write("zaz.toml", config);

        let config_path = self.dir.join("zaz.toml");
        self.daemon = Some(StartedDaemon::launch(&self.dir, &config_path, &self.socket));
    }

    fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn wait_until(&self, what: &str, mut done: impl FnMut(&Project) -> bool) {
        let deadline = Instant::now() + WAIT;

        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_lines(&self, name: &str, count: usize) -> Vec<String> {
        self.wait_until(&format!("{count} lines in {name}"), |p| {
            p.lines(name).len() >= count
        });

        self.lines(name)
    }

    fn status(&self) -> String {
        let socket = self.socket.to_str().unwrap();

        stdout_string(&run_zaz(&self.dir, &["--socket", socket, "status"]))
    }
}

fn task(file: &str) -> String {
    format!("[[group.task]]\nname = \"show\"\ncommand = 'echo \"V=$V\" >> {file}'\n")
}

const FAST: &str = "[settings]\ndebounce = \"20ms\"\n";

#[test]
fn editing_an_env_file_rebuilds_only_the_groups_it_changes() {
    let mut project = Project::new();
    project.write(".env", "V=1\n");
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_file = \".env\"\n{}\n\
         [[group]]\nname = \"b\"\n{}",
        task("a.log"),
        task("b.log")
    ));
    project.wait_for_lines("a.log", 1);
    project.wait_for_lines("b.log", 1);

    project.write(".env", "V=2\n");

    assert_eq!(project.wait_for_lines("a.log", 2), ["V=1", "V=2"]);
    assert_eq!(project.lines("b.log"), ["V="]);
}

#[test]
fn a_no_op_edit_rebuilds_nothing() {
    let mut project = Project::new();
    project.write(".env", "V=1\n");
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_file = \".env\"\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);

    project.write(".env", "# only a comment changed\nV=1\n");
    project.write(".env", "V=3\n");

    assert_eq!(project.wait_for_lines("a.log", 2), ["V=1", "V=3"]);
}

#[test]
fn a_same_content_save_of_a_listed_file_runs_no_command() {
    let mut project = Project::new();
    project.write("a.lock", "one\n");
    project.write("b.lock", "one\n");
    let stub = project.stub("envcmd", "printf '{\"V\":\"%s\"}' \"$(cat b.lock)\"");
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_command = '{stub}'\n\
         env_watch = [\"a.lock\", \"b.lock\"]\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);
    assert_eq!(project.runs("envcmd"), 1);

    project.write("a.lock", "one\n");
    project.write("b.lock", "two\n");

    assert_eq!(project.wait_for_lines("a.log", 2), ["V=one", "V=two"]);
    assert_eq!(project.runs("envcmd"), 2);
}

#[test]
fn a_failed_command_is_retried_by_a_touch_and_the_failure_is_shown_until_then() {
    let mut project = Project::new();
    project.write("watched", "one\n");
    let fail = project.dir.join("fail");
    let stub = project.stub(
        "envcmd",
        &format!(
            "if [ -e '{}' ]; then echo boom >&2; exit 3; fi\nprintf '{{\"V\":\"%s\"}}' \"$(cat watched)\"",
            fail.display()
        ),
    );
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_command = '{stub}'\nenv_watch = [\"watched\"]\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);
    assert!(!project.status().contains("Environment refresh error"));

    project.write("fail", "");
    project.write("watched", "two\n");
    project.wait_until("the refresh failure to show", |p| {
        p.status().contains("Environment refresh error")
    });

    let status = project.status();
    assert!(status.contains("env_command_failed"), "{status}");
    assert_eq!(project.lines("a.log"), ["V=one"]);

    std::fs::remove_file(&fail).unwrap();
    project.write("watched", "two\n");

    assert_eq!(project.wait_for_lines("a.log", 2), ["V=one", "V=two"]);
    project.wait_until("the failure to clear", |p| {
        !p.status().contains("Environment refresh error")
    });
}

#[test]
fn a_deleted_listed_file_reevaluates_the_command() {
    let mut project = Project::new();
    project.write("flag", "");
    let stub = project.stub(
        "envcmd",
        "if [ -e flag ]; then printf '{\"V\":\"present\"}'; else printf '{\"V\":\"absent\"}'; fi",
    );
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_command = '{stub}'\nenv_watch = [\"flag\"]\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);

    std::fs::remove_file(project.dir.join("flag")).unwrap();

    assert_eq!(
        project.wait_for_lines("a.log", 2),
        ["V=present", "V=absent"]
    );
}

#[test]
fn a_child_command_runs_again_when_its_upstream_changed() {
    let mut project = Project::new();
    project.write("up.txt", "one\n");
    let upstream = project.stub("upstream", "printf '{\"UP\":\"%s\"}' \"$(cat up.txt)\"");
    let child = project.stub("child", "printf '{\"V\":\"child-saw-%s\"}' \"$UP\"");
    project.start(&format!(
        "[settings]\ndebounce = \"20ms\"\nenv_command = '{upstream}'\nenv_watch = [\"up.txt\"]\n\n\
         [[group]]\nname = \"a\"\nenv_command = '{child}'\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);

    project.write("up.txt", "two\n");

    assert_eq!(
        project.wait_for_lines("a.log", 2),
        ["V=child-saw-one", "V=child-saw-two"]
    );
    assert_eq!(project.runs("upstream"), 2);
    assert_eq!(project.runs("child"), 2);
}

#[test]
fn a_resaved_upstream_file_retries_a_failed_child_without_running_the_upstream_again() {
    let mut project = Project::new();
    project.write("up.txt", "one\n");
    let fail = project.dir.join("fail");
    let upstream = project.stub("upstream", "printf '{\"UP\":\"%s\"}' \"$(cat up.txt)\"");
    let child = project.stub(
        "child",
        &format!(
            "if [ -e '{}' ]; then echo boom >&2; exit 3; fi\nprintf '{{\"V\":\"child-saw-%s\"}}' \"$UP\"",
            fail.display()
        ),
    );
    project.start(&format!(
        "[settings]\ndebounce = \"20ms\"\nenv_command = '{upstream}'\nenv_watch = [\"up.txt\"]\n\n\
         [[group]]\nname = \"a\"\nenv_command = '{child}'\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);

    project.write("fail", "");
    project.write("up.txt", "two\n");
    project.wait_until("the child failure to show", |p| {
        p.status().contains("Environment refresh error")
    });
    assert_eq!(project.runs("upstream"), 2);
    assert_eq!(project.runs("child"), 2);

    std::fs::remove_file(&fail).unwrap();
    project.write("up.txt", "two\n");

    assert_eq!(
        project.wait_for_lines("a.log", 2),
        ["V=child-saw-one", "V=child-saw-two"]
    );
    assert_eq!(
        project.runs("upstream"),
        2,
        "the upstream must not run again"
    );
    assert_eq!(project.runs("child"), 3);
}

#[test]
fn a_failed_refresh_keeps_the_environment_and_still_runs_held_file_changes() {
    let mut project = Project::new();
    project.write(".env", "V=1\n");
    project.write("code.txt", "a\n");
    project.start(
        "[settings]\ndebounce = \"400ms\"\n\n\
         [[group]]\nname = \"a\"\nenv_file = \".env\"\npatterns = [\"**/code.txt\"]\n\n\
         [[group.task]]\nname = \"startup\"\ncommand = 'true'\n\n\
         [[group.task]]\nname = \"on-change\"\non_change_only = true\n\
         command = 'echo \"V=$V code=$(cat code.txt)\" >> change.log'\n",
    );
    project.wait_until("startup", |p| p.status().contains("Daemon Status: Running"));

    project.write(".env", "this is not valid\n");
    project.write("code.txt", "b\n");

    assert_eq!(project.wait_for_lines("change.log", 1), ["V=1 code=b"]);
    project.wait_until("the refresh failure to show", |p| {
        p.status().contains("Environment refresh error")
    });

    project.write(".env", "V=2\n");
    project.wait_until("the failure to clear", |p| {
        !p.status().contains("Environment refresh error")
    });
}

#[test]
fn a_source_matched_by_another_groups_ignore_is_still_seen() {
    let mut project = Project::new();
    project.write(".env", "V=1\n");
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"greedy\"\npatterns = [\"**/*\"]\nignore = [\"**/.*\"]\n\n\
         [[group.task]]\ncommand = 'true'\n\n\
         [[group]]\nname = \"a\"\nenv_file = \".env\"\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);

    project.write(".env", "V=2\n");

    assert_eq!(project.wait_for_lines("a.log", 2), ["V=1", "V=2"]);
}

#[test]
fn a_batch_of_a_source_and_a_pattern_match_runs_the_group_once_after_the_rebuild() {
    let mut project = Project::new();
    project.write(".env", "V=1\n");
    project.write("code.txt", "a\n");
    project.start(
        "[settings]\ndebounce = \"400ms\"\n\n\
         [[group]]\nname = \"a\"\nenv_file = \".env\"\npatterns = [\"**/code.txt\"]\n\n\
         [[group.task]]\nname = \"show\"\ncommand = 'echo \"V=$V code=$(cat code.txt)\" >> a.log'\n",
    );
    project.wait_for_lines("a.log", 1);

    project.write(".env", "V=2\n");
    project.write("code.txt", "b\n");
    project.wait_for_lines("a.log", 2);

    project.write(".env", "V=3\n");
    let lines = project.wait_for_lines("a.log", 3);

    assert_eq!(lines, ["V=1 code=a", "V=2 code=b", "V=3 code=b"]);
}

#[test]
fn a_source_outside_the_config_directory_is_watched_through_its_parent() {
    let mut project = Project::new();
    project.write("shared.env", "V=1\n");
    project.write("flake.lock", "one\n");
    let stub = project.stub("envcmd", "printf '{\"W\":\"%s\"}' \"$(cat ../flake.lock)\"");
    project.write("proj/placeholder", "");
    project.dir = project.dir.join("proj");
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_file = \"../shared.env\"\n\
         env_command = '{stub}'\nenv_watch = [\"../flake.lock\"]\n\n\
         [[group.task]]\nname = \"show\"\ncommand = 'echo \"V=$V W=$W\" >> a.log'\n",
    ));
    project.wait_for_lines("a.log", 1);
    assert_eq!(project.lines("a.log"), ["V=1 W=one"]);

    project.write("../shared.env", "V=2\n");
    project.wait_for_lines("a.log", 2);

    project.write("../flake.lock", "two\n");
    let lines = project.wait_for_lines("a.log", 3);

    assert_eq!(lines, ["V=1 W=one", "V=2 W=one", "V=2 W=two"]);
}

#[test]
fn a_config_directory_reached_through_a_symlink_sees_edits_and_new_sources() {
    let real = TempDir::new().unwrap();
    let real_dir = real.path().canonicalize().unwrap();
    let holder = TempDir::new().unwrap();
    let link = holder.path().canonicalize().unwrap().join("link");
    std::os::unix::fs::symlink(&real_dir, &link).unwrap();

    let mut project = Project::at(holder, link);
    project.write(".env", "V=1\n");
    let stub = project.stub(
        "envcmd",
        "printf '{\"W\":\"%s\"}' \"$(cat later 2>/dev/null || echo none)\"",
    );
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_file = \".env\"\n\
         env_command = '{stub}'\nenv_watch = [\"later\"]\n\n\
         [[group.task]]\nname = \"show\"\ncommand = 'echo \"V=$V W=$W\" >> a.log'\n",
    ));
    project.wait_for_lines("a.log", 1);
    assert_eq!(project.lines("a.log"), ["V=1 W=none"]);

    project.write(".env", "V=2\n");
    project.wait_for_lines("a.log", 2);

    project.write("later", "created\n");
    let lines = project.wait_for_lines("a.log", 3);

    assert_eq!(lines, ["V=1 W=none", "V=2 W=none", "V=2 W=created"]);
}

#[test]
fn a_symlinked_source_is_followed_when_created_edited_through_the_link_and_deleted() {
    let mut project = Project::new();
    let outside = TempDir::new().unwrap();
    let target = outside.path().canonicalize().unwrap().join("real.txt");
    std::fs::write(&target, "one\n").unwrap();

    let stub = project.stub(
        "envcmd",
        "printf '{\"V\":\"%s\"}' \"$(cat watched 2>/dev/null || echo none)\"",
    );
    project.start(&format!(
        "{FAST}\n[[group]]\nname = \"a\"\nenv_command = '{stub}'\nenv_watch = [\"watched\"]\n{}",
        task("a.log")
    ));
    project.wait_for_lines("a.log", 1);

    let link = project.dir.join("watched");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    project.wait_for_lines("a.log", 2);

    std::fs::write(&link, "two\n").unwrap();
    project.wait_for_lines("a.log", 3);

    std::fs::write(&target, "three\n").unwrap();
    project.wait_for_lines("a.log", 4);

    std::fs::remove_file(&link).unwrap();
    let lines = project.wait_for_lines("a.log", 5);

    assert_eq!(lines, ["V=none", "V=one", "V=two", "V=three", "V=none"]);
}

#[test]
fn env_watch_without_a_command_is_a_coded_error() {
    let project = Project::new();
    project.write(
        "zaz.toml",
        "[[group]]\nname = \"a\"\nenv_watch = [\".envrc\"]\n\n[[group.task]]\ncommand = \"true\"\n",
    );

    let config = project.dir.join("zaz.toml");
    let output = run_zaz(&project.dir, &["check", "--json", config.to_str().unwrap()]);
    let text = stdout_string(&output);

    assert!(!output.status.success());
    assert!(text.contains("env_watch_without_command"), "{text}");
}
