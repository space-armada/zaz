//! End-to-end coverage for `env_command`.
//!
//! The commands are stubs written into a temp directory that is put on the `PATH` of the zaz
//! process, so each test controls exactly what the command prints and how it behaves. Most tests
//! run `zaz task`, which evaluates the config the same way the daemon does and then runs the
//! tasks, so a task that writes its environment to a file shows what reached it.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use support::{run_zaz_with_envs, stderr_string, zaz_bin};
use tempfile::TempDir;

struct Project {
    temp: TempDir,
}

impl Project {
    fn new(config: &str) -> Self {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("zaz.toml"), config).unwrap();
        std::fs::create_dir(temp.path().join("bin")).unwrap();

        Self { temp }
    }

    fn path(&self) -> PathBuf {
        self.temp.path().canonicalize().unwrap()
    }

    fn stub(&self, name: &str, body: &str) {
        let path = self.path().join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn search_path(&self) -> String {
        format!(
            "{}:{}",
            self.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn zaz_task(&self, envs: &[(&str, &str)]) -> Output {
        let mut all: Vec<(String, String)> = vec![("PATH".to_string(), self.search_path())];
        all.extend(envs.iter().map(|(k, v)| (k.to_string(), v.to_string())));

        run_zaz_with_envs(
            &self.path(),
            all.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            ["task"],
        )
    }

    fn spawn_task(&self) -> Child {
        Command::new(zaz_bin())
            .arg("task")
            .current_dir(self.path())
            .env("PATH", self.search_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn zaz task")
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path().join(name)).unwrap_or_default()
    }
}

fn wait_for_file(path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;

    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_exit(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;

    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the process did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn signal(pid: u32, name: &str) {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(status.success());
}

fn is_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn assert_dies(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);

    while is_alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} is still alive");
        thread::sleep(Duration::from_millis(50));
    }
}

const WRITE_ENV: &str = r#"printf '%s|%s|%s' "\${A-unset}" "\${B-unset}" "\${C-unset}" > out.txt"#;

fn config_with_command(command_line: &str) -> String {
    format!(
        "[settings]\nenv_command = 'stub'\n{command_line}\n\n\
         [[group]]\nname = \"app\"\n\n[[group.task]]\nname = \"show\"\ncommand = '''{WRITE_ENV}'''\n"
    )
}

#[test]
fn json_output_sets_and_null_unsets() {
    let project = Project::new(&config_with_command("env_passthrough = [\"B\"]"));
    project.stub("stub", r#"echo '{"A":"1","B":null,"C":""}'"#);

    let output = project.zaz_task(&[("B", "from-daemon")]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("out.txt"), "1|unset|");
}

#[test]
fn empty_output_changes_nothing() {
    let project = Project::new(&config_with_command("env_passthrough = [\"B\"]"));
    project.stub("stub", "printf ' \\n'");

    let output = project.zaz_task(&[("B", "kept")]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("out.txt"), "unset|kept|unset");
}

#[test]
fn every_contract_violation_is_a_coded_error_that_never_echoes_stdout() {
    let cases: &[(&str, &str)] = &[
        ("not json", "FOO=hunter2"),
        ("array", r#"["hunter2"]"#),
        ("string", r#""hunter2""#),
        ("number value", r#"{"A":12345}"#),
        ("nested value", r#"{"A":{"b":"hunter2"}}"#),
        ("duplicate key", r#"{"A":"hunter2","A":"2"}"#),
        ("empty key", r#"{"":"hunter2"}"#),
        ("key with equals", r#"{"A=B":"hunter2"}"#),
        ("nul in value", r#"{"A":"hunter2\u0000"}"#),
        ("nul in key", r#"{"A\u0000B":"hunter2"}"#),
        ("trailing text", r#"{"A":"1"} hunter2"#),
    ];

    for (name, stdout) in cases {
        let project = Project::new(&config_with_command(""));
        project.stub("stub", &format!("cat <<'DONE'\n{stdout}\nDONE"));

        let output = project.zaz_task(&[]);
        let stderr = stderr_string(&output);

        assert_ne!(output.status.code(), Some(0), "{name}");
        assert!(
            stderr.contains("env_command_malformed_output"),
            "{name}: {stderr}"
        );
        assert!(!stderr.contains("hunter2"), "{name}: {stderr}");
        assert_eq!(project.read("out.txt"), "", "{name}: the task must not run");
    }
}

#[test]
fn invalid_utf8_output_is_a_coded_error() {
    let project = Project::new(&config_with_command(""));
    project.stub("stub", r#"printf '{"A":"\377"}'"#);

    let output = project.zaz_task(&[]);

    assert_ne!(output.status.code(), Some(0));
    assert!(stderr_string(&output).contains("invalid UTF-8"));
}

#[test]
fn a_non_zero_exit_fails_the_task_run_with_stderr_and_no_task_runs() {
    let project = Project::new(&config_with_command(""));
    project.stub(
        "stub",
        r#"printf '\033[31mdirenv: error .envrc is blocked\033[0m\n' >&2; echo '{"A":"1"}'; exit 7"#,
    );

    let output = project.zaz_task(&[]);
    let stderr = stderr_string(&output);

    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("env_command_failed"), "{stderr}");
    assert!(stderr.contains("exited with status 7"), "{stderr}");
    assert!(
        stderr.contains("direnv: error .envrc is blocked"),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert_eq!(project.read("out.txt"), "");
}

#[test]
fn stderr_from_a_successful_run_is_not_a_failure() {
    let project = Project::new(&config_with_command(""));
    project.stub(
        "stub",
        r#"echo 'direnv: loading .envrc' >&2; echo '{"A":"1"}'"#,
    );

    let output = project.zaz_task(&[]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("out.txt"), "1|unset|unset");
}

#[test]
fn a_command_past_its_timeout_has_its_own_code() {
    let project = Project::new(&config_with_command("env_command_timeout = \"500ms\""));
    project.stub("stub", "sleep 30");

    let started = Instant::now();
    let output = project.zaz_task(&[]);

    assert_ne!(output.status.code(), Some(0));
    assert!(stderr_string(&output).contains("env_command_timeout"));
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[test]
fn a_descendant_holding_the_pipe_is_a_held_pipe_not_a_timeout() {
    let project = Project::new(&config_with_command("env_command_timeout = \"500ms\""));
    let pid_file = project.path().join("helper.pid");
    project.stub(
        "stub",
        &format!("sleep 30 &\necho $! > {}\necho '{{}}'", pid_file.display()),
    );

    let output = project.zaz_task(&[]);
    let stderr = stderr_string(&output);
    let pid = wait_for_file(&pid_file, Duration::from_secs(5));

    assert_ne!(output.status.code(), Some(0));
    assert!(stderr.contains("env_command_held_pipe"), "{stderr}");
    assert!(stderr.contains("/dev/null"), "{stderr}");
    assert_dies(&pid);
}

#[test]
fn oversized_stdout_and_stderr_have_their_own_codes() {
    let project = Project::new(&config_with_command(""));
    project.stub("stub", "head -c 2000000 /dev/zero | tr '\\0' 'a'");
    let stderr = stderr_string(&project.zaz_task(&[]));
    assert!(stderr.contains("env_command_stdout_too_large"), "{stderr}");

    let project = Project::new(&config_with_command(""));
    project.stub(
        "stub",
        "head -c 2000000 /dev/zero | tr '\\0' 'a' >&2; echo '{}'",
    );
    let stderr = stderr_string(&project.zaz_task(&[]));
    assert!(stderr.contains("env_command_stderr_too_large"), "{stderr}");
}

#[test]
fn a_missing_working_directory_or_shell_is_a_spawn_failure() {
    let project = Project::new(
        "[[group]]\nname = \"app\"\nworking_dir = \"nowhere\"\nenv_command = 'true'\n\n\
         [[group.task]]\ncommand = 'true'\n",
    );
    let stderr = stderr_string(&project.zaz_task(&[]));
    assert!(stderr.contains("env_command_spawn_failed"), "{stderr}");
    assert!(stderr.contains("working directory"), "{stderr}");

    let project = Project::new(
        "[settings]\nshell = \"/no/such/shell\"\nenv_command = 'true'\n\n\
         [[group]]\nname = \"app\"\n\n[[group.task]]\ncommand = 'true'\n",
    );
    let stderr = stderr_string(&project.zaz_task(&[]));
    assert!(stderr.contains("env_command_spawn_failed"), "{stderr}");
    assert!(stderr.contains("/no/such/shell"), "{stderr}");
}

#[test]
fn the_command_runs_under_settings_shell_and_never_the_daemons_shell() {
    let project = Project::new(
        "[settings]\nshell = \"/bin/bash\"\nenv_command = 'echo \"{\\\"A\\\":\\\"\\${BASH_VERSION:+bash}\\\"}\"'\n\n\
         [[group]]\nname = \"app\"\n\n[[group.task]]\nname = \"show\"\ncommand = 'printf \"%s\" \"$A\" > out.txt'\n",
    );

    let output = project.zaz_task(&[("SHELL", "/no/such/shell")]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("out.txt"), "bash");
}

#[test]
fn a_process_under_a_command_does_not_see_inherited_variables_outside_the_base() {
    let project = Project::new(
        "[[group]]\nname = \"with\"\nenv_command = 'echo {}'\n\n\
         [[group.task]]\nname = \"a\"\ncommand = 'printf \"%s\" \"\\${OUTSIDE_THE_BASE-unset}\" > with.txt'\n\n\
         [[group]]\nname = \"without\"\n\n\
         [[group.task]]\nname = \"b\"\ncommand = 'printf \"%s\" \"\\${OUTSIDE_THE_BASE-unset}\" > without.txt'\n",
    );

    let output = project.zaz_task(&[("OUTSIDE_THE_BASE", "inherited")]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("with.txt"), "unset");
    assert_eq!(project.read("without.txt"), "inherited");
}

#[test]
fn a_nested_chain_applies_an_upstream_unset_to_what_the_lower_command_sees() {
    let project = Project::new(
        "[settings]\nenv_command = 'echo \"{\\\"A\\\":\\\"1\\\",\\\"B\\\":\\\"2\\\"}\"'\n\n\
         [[group]]\nname = \"app\"\nenv_command = 'diff-stub'\n\n\
         [[group.task]]\nname = \"show\"\ncommand = 'printf \"%s|%s|%s\" \"\\${A-unset}\" \"\\${B-unset}\" \"\\${C-unset}\" > out.txt'\n",
    );
    project.stub(
        "diff-stub",
        r#"if [ "${A-}" = 1 ]; then echo '{"A":null,"B":null,"C":"3"}'; else echo '{"C":"3"}'; fi"#,
    );

    let output = project.zaz_task(&[]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("out.txt"), "unset|unset|3");
}

#[test]
fn an_inline_override_above_a_command_is_input_to_it_and_loses_to_its_output() {
    let project = Project::new(
        "[[group]]\nname = \"app\"\nenv = { X = \"inline\" }\n\n\
         [[group.task]]\nname = \"show\"\nenv_command = 'reporter'\n\
         command = 'printf \"%s|%s\" \"$SEEN\" \"$X\" > out.txt'\n",
    );
    project.stub(
        "reporter",
        r#"printf '{"SEEN":"%s","X":"from-command"}' "$X""#,
    );

    let output = project.zaz_task(&[]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("out.txt"), "inline|from-command");
}

#[test]
fn commands_run_by_level_then_in_config_order_and_in_their_own_directories() {
    let project = Project::new(
        "[[group]]\nname = \"one\"\nworking_dir = \"d1\"\nenv_command = 'recorder group-one'\n\n\
         [[group.task]]\nname = \"t1\"\nworking_dir = \"d3\"\nenv_command = 'recorder task-one'\ncommand = 'true'\n\n\
         [[group]]\nname = \"two\"\nworking_dir = \"d2\"\nenv_command = 'recorder group-two'\n\n\
         [[group.task]]\nname = \"t2\"\ncommand = 'true'\n\n\
         [settings]\nenv_command = 'recorder settings'\n",
    );
    for dir in ["d1", "d2", "d3"] {
        std::fs::create_dir(project.path().join(dir)).unwrap();
    }
    let log = project.path().join("order.log");
    project.stub(
        "recorder",
        &format!(
            "echo \"$1 $(basename \"$(pwd)\")\" >> {}\necho '{{}}'",
            log.display()
        ),
    );

    let output = project.zaz_task(&[]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));

    let order = std::fs::read_to_string(&log).unwrap();
    let basename = project
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let expected = [
        format!("settings {basename}"),
        "group-one d1".to_string(),
        "group-two d2".to_string(),
        "task-one d3".to_string(),
    ];
    assert_eq!(order.lines().collect::<Vec<_>>(), expected);
}

#[test]
fn ctrl_c_during_evaluation_signals_the_command_and_exits_130() {
    let project = Project::new(&config_with_command(""));
    let pid_file = project.path().join("stub.pid");
    project.stub(
        "stub",
        &format!("echo $$ > {}\nexec sleep 60", pid_file.display()),
    );

    let child = project.spawn_task();
    let pid = wait_for_file(&pid_file, Duration::from_secs(10));
    signal(child.id(), "INT");

    let output = wait_exit(child, Duration::from_secs(15));

    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        stderr_string(&output)
    );
    assert_dies(&pid);
    assert_eq!(project.read("out.txt"), "");
}

#[test]
fn ctrl_c_during_tasks_signals_their_groups_and_exits_130() {
    let pid_file_name = "task.pid";
    let project = Project::new(&format!(
        "[[group]]\nname = \"app\"\n\n[[group.task]]\nname = \"slow\"\n\
         command = 'echo $$ > {pid_file_name}; exec sleep 60'\n"
    ));

    let child = project.spawn_task();
    let pid = wait_for_file(&project.path().join(pid_file_name), Duration::from_secs(10));
    signal(child.id(), "INT");

    let output = wait_exit(child, Duration::from_secs(15));

    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        stderr_string(&output)
    );
    assert_dies(&pid);
}

fn assert_shutdown_kills_the_command(stub_body: impl Fn(&Path) -> String) {
    let project = Project::new(&config_with_command(""));
    let pid_file = project.path().join("stub.pid");
    project.stub("stub", &stub_body(&pid_file));
    let socket = project.path().join("d.sock");
    let log = project.path().join("zaz.log");

    let zaz = |sub: &str| {
        let mut cmd = Command::new(zaz_bin());
        cmd.arg("--config")
            .arg(project.path().join("zaz.toml"))
            .arg("--socket")
            .arg(&socket)
            .arg("--log-file")
            .arg(&log)
            .arg(sub)
            .current_dir(project.path())
            .env("PATH", project.search_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    };

    let start = zaz("start").spawn().unwrap();
    let pid = wait_for_file(&pid_file, Duration::from_secs(10));

    let stop = zaz("stop").output().unwrap();
    assert_eq!(stop.status.code(), Some(0), "{}", stderr_string(&stop));

    assert_dies(&pid);
    let _ = wait_exit(start, Duration::from_secs(30));
}

#[test]
fn shutdown_during_evaluation_signals_the_command_group() {
    assert_shutdown_kills_the_command(|pid_file| {
        format!("echo $$ > {}\nexec sleep 60", pid_file.display())
    });
}

#[test]
fn shutdown_kills_a_command_that_ignores_sigterm_before_the_daemon_exits() {
    assert_shutdown_kills_the_command(|pid_file| {
        format!("trap '' TERM\necho $$ > {}\nsleep 60", pid_file.display())
    });
}

#[test]
fn check_lists_each_env_command_it_did_not_run() {
    let project = Project::new(
        "[settings]\nenv_command = 'direnv export json'\n\n\
         [[group]]\nname = \"app\"\n\n[[group.task]]\nname = \"t\"\ncommand = 'true'\nenv_command = 'mise env --json'\n",
    );
    let marker = project.path().join("ran");
    project.stub("direnv", &format!("touch {}", marker.display()));

    let json = run_zaz_with_envs(
        &project.path(),
        [("PATH", project.search_path())],
        ["check", "--json"],
    );
    assert_eq!(json.status.code(), Some(0), "{}", stderr_string(&json));

    let result: serde_json::Value =
        serde_json::from_str(&String::from_utf8(json.stdout).unwrap()).unwrap();
    let not_run = result["not_evaluated"]["not_run"].as_array().unwrap();
    assert_eq!(not_run.len(), 2);
    assert_eq!(not_run[0]["field"], "settings env_command");
    assert_eq!(not_run[0]["command"], "direnv export json");
    assert_eq!(not_run[1]["field"], "group 'app' task 't' env_command");
    assert!(result["errors"].as_array().unwrap().is_empty());

    let text = run_zaz_with_envs(
        &project.path(),
        [("PATH", project.search_path())],
        ["check"],
    );
    assert!(stderr_string(&text).contains("not evaluated, not run"));
    assert!(!marker.exists(), "check must not run the command");
}

#[test]
fn check_rejects_the_static_env_command_mistakes() {
    let project = Project::new(
        "[settings]\nenv_command_timeout = \"5s\"\nenv_passthrough = [\"A*B\"]\n\n\
         [[group]]\nname = \"app\"\nenv_command = 'x ${zaz:files} ${nope}'\n",
    );

    let output = run_zaz_with_envs(
        &project.path(),
        std::iter::empty::<(&str, &str)>(),
        ["--config", "zaz.toml", "check", "--json"],
    );
    let result: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    let codes: Vec<&str> = result["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap())
        .collect();

    assert_eq!(result["valid"], false);
    for code in [
        "invalid_env_passthrough",
        "env_command_file_builtin",
        "env_command_undefined_variable",
    ] {
        assert!(codes.contains(&code), "{code} missing from {codes:?}");
    }
}

fn direnv_available() -> bool {
    Command::new("direnv")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn direnv_export_json_applies_a_nested_envrc_with_an_unset() {
    if !direnv_available() {
        eprintln!("skipping: direnv is not installed");
        return;
    }

    let project = Project::new(
        "[settings]\nenv_command = 'direnv export json'\n\n\
         [[group]]\nname = \"app\"\nworking_dir = \"child\"\nenv_command = 'direnv export json'\n\n\
         [[group.task]]\nname = \"show\"\n\
         command = 'printf \"%s|%s|%s\" \"\\${A-unset}\" \"\\${B-unset}\" \"\\${C-unset}\" > out.txt'\n",
    );
    let root = project.path();
    std::fs::create_dir(root.join("child")).unwrap();
    std::fs::write(root.join(".envrc"), "export A=root\nexport B=root\n").unwrap();
    std::fs::write(root.join("child/.envrc"), "export C=child\nunset B\n").unwrap();

    let xdg_data = root.join("xdg-data");
    let xdg_config = root.join("xdg-config");
    let xdg_cache = root.join("xdg-cache");
    for dir in [&xdg_data, &xdg_config, &xdg_cache] {
        std::fs::create_dir(dir).unwrap();
    }

    for dir in [root.clone(), root.join("child")] {
        let status = Command::new("direnv")
            .arg("allow")
            .current_dir(&dir)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_CACHE_HOME", &xdg_cache)
            .env_remove("DIRENV_DIR")
            .status()
            .unwrap();
        assert!(status.success());
    }

    let output = project.zaz_task(&[
        ("XDG_DATA_HOME", xdg_data.to_str().unwrap()),
        ("XDG_CONFIG_HOME", xdg_config.to_str().unwrap()),
        ("XDG_CACHE_HOME", xdg_cache.to_str().unwrap()),
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_string(&output));
    assert_eq!(project.read("child/out.txt"), "unset|unset|child");
}
