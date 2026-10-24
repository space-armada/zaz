//! Running an `env_command` and reading its output.
//!
//! A command prints nothing, or one JSON object whose values are strings or `null`. A string sets
//! a variable and a `null` unsets it. Anything else is a coded error that names the kind of
//! failure and, when there is one, the key and a position. An error never echoes the output,
//! since a command such as `sops -d .env` prints secrets.

use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;
use zaz_process::{strip_ansi, CancelHandle, CaptureError, CaptureSpec, Captured};

/// The most stdout bytes an `env_command` may print.
pub(crate) const MAX_STDOUT_BYTES: usize = 1024 * 1024;

/// The most stderr bytes an `env_command` may print.
pub(crate) const MAX_STDERR_BYTES: usize = 1024 * 1024;

/// The most stripped stderr bytes an error embeds.
const EMBEDDED_STDERR_BYTES: usize = 4096;

/// The deadline of an `env_command` when `env_command_timeout` is unset.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// What an `env_command` printed: a value sets a variable and `None` unsets it.
pub(crate) type CommandOutput = BTreeMap<String, Option<String>>;

/// A failure to run an `env_command` or to read its output.
///
/// The error names the config, the field, and the directory the command ran in. It never holds
/// the command's stdout.
#[derive(Debug)]
pub struct EnvCommandError {
    /// The config file that declared the command.
    pub config: PathBuf,
    /// The declaring field, such as `group 'api' env_command`.
    pub field: String,
    /// The directory the command ran in.
    pub working_dir: PathBuf,
    /// What went wrong.
    pub kind: EnvCommandErrorKind,
}

/// What went wrong with an `env_command`.
#[derive(Debug)]
pub enum EnvCommandErrorKind {
    /// The command exited with a non-zero status.
    Failed {
        /// How the command ended, such as `exited with status 1`.
        status: String,
        /// Its stderr, stripped of escape sequences and capped.
        stderr: String,
    },
    /// The command was still running at the deadline.
    Timeout { limit: Duration },
    /// The command exited but a background process held its output open at the deadline.
    HeldPipe {
        exited_ago: Duration,
        limit: Duration,
    },
    /// Stdout grew past its cap.
    StdoutTooLarge { limit: usize },
    /// Stderr grew past its cap.
    StderrTooLarge { limit: usize },
    /// Stdout is not one flat JSON object. The text never quotes the output.
    MalformedOutput { problem: String },
    /// The shell could not be started.
    SpawnFailed { detail: String },
    /// The command's string does not expand.
    Expansion { detail: String },
    /// The evaluation was cancelled.
    Cancelled,
}

impl EnvCommandError {
    /// Error code for JSON output.
    pub fn code(&self) -> &'static str {
        match &self.kind {
            EnvCommandErrorKind::Failed { .. } => "env_command_failed",
            EnvCommandErrorKind::Timeout { .. } => "env_command_timeout",
            EnvCommandErrorKind::HeldPipe { .. } => "env_command_held_pipe",
            EnvCommandErrorKind::StdoutTooLarge { .. } => "env_command_stdout_too_large",
            EnvCommandErrorKind::StderrTooLarge { .. } => "env_command_stderr_too_large",
            EnvCommandErrorKind::MalformedOutput { .. } => "env_command_malformed_output",
            EnvCommandErrorKind::SpawnFailed { .. } => "env_command_spawn_failed",
            EnvCommandErrorKind::Expansion { .. } => "env_command_malformed_variable",
            EnvCommandErrorKind::Cancelled => "evaluation_cancelled",
        }
    }

    /// A suggested fix.
    pub fn hint(&self) -> Option<&'static str> {
        match &self.kind {
            EnvCommandErrorKind::Failed { .. } => None,
            EnvCommandErrorKind::Timeout { .. } => {
                Some("raise env_command_timeout in [settings], or make the command finish sooner")
            }
            EnvCommandErrorKind::HeldPipe { .. } => {
                Some("redirect background helpers to /dev/null, as in 'helper >/dev/null 2>&1 &'")
            }
            EnvCommandErrorKind::StdoutTooLarge { .. } => Some(
                "env_command prints one JSON object of variables, so check it prints nothing else",
            ),
            EnvCommandErrorKind::StderrTooLarge { .. } => {
                Some("quiet the command's stderr, or redirect it to /dev/null")
            }
            EnvCommandErrorKind::MalformedOutput { .. } => Some(
                "env_command must print one JSON object of string or null values, as 'direnv \
                 export json' does",
            ),
            EnvCommandErrorKind::SpawnFailed { .. } => {
                Some("check the working_dir and the shell setting")
            }
            EnvCommandErrorKind::Expansion { .. } => {
                Some("close the reference, or escape the dollar sign as \\${")
            }
            EnvCommandErrorKind::Cancelled => None,
        }
    }

    /// The headline: where the command is declared, where it ran, and what went wrong.
    ///
    /// Stderr is not part of it. See [`EnvCommandError::with_code`].
    pub fn message(&self) -> String {
        format!(
            "{} (in {}): {}",
            self.field,
            self.working_dir.display(),
            self.detail()
        )
    }

    /// The problem alone.
    pub fn detail(&self) -> String {
        match &self.kind {
            EnvCommandErrorKind::Failed { status, .. } => status.clone(),
            EnvCommandErrorKind::Timeout { limit } => {
                format!("did not finish within {}", format_limit(*limit))
            }
            EnvCommandErrorKind::HeldPipe { exited_ago, limit } => format!(
                "exited {} before the {} deadline, but a background process it started still \
                 held its output open",
                format_limit(*exited_ago),
                format_limit(*limit)
            ),
            EnvCommandErrorKind::StdoutTooLarge { limit } => {
                format!("wrote more than {limit} bytes to stdout")
            }
            EnvCommandErrorKind::StderrTooLarge { limit } => {
                format!("wrote more than {limit} bytes to stderr")
            }
            EnvCommandErrorKind::MalformedOutput { problem } => {
                format!("output is not a flat JSON object: {problem}")
            }
            EnvCommandErrorKind::SpawnFailed { detail } => format!("cannot start: {detail}"),
            EnvCommandErrorKind::Expansion { detail } => {
                format!("cannot expand variables: {detail}")
            }
            EnvCommandErrorKind::Cancelled => "evaluation was cancelled".to_string(),
        }
    }

    /// The error as `config: headline [code]`, followed by the command's stderr when it has any.
    ///
    /// The code sits on the first line so a per-line report keeps it beside the headline.
    pub fn with_code(&self) -> String {
        let mut out = format!(
            "{}: {} [{}]",
            self.config.display(),
            self.message(),
            self.code()
        );

        if let EnvCommandErrorKind::Failed { stderr, .. } = &self.kind {
            for line in stderr.lines() {
                out.push_str("\n    ");
                out.push_str(line);
            }
        }

        out
    }
}

impl fmt::Display for EnvCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.with_code())
    }
}

impl std::error::Error for EnvCommandError {}

fn format_limit(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

/// What to run.
pub(crate) struct RunSpec<'a> {
    pub(crate) shell: &'a str,
    pub(crate) command: &'a str,
    pub(crate) working_dir: &'a Path,
    pub(crate) env: &'a std::collections::HashMap<std::ffi::OsString, std::ffi::OsString>,
    pub(crate) timeout: Duration,
}

/// Run an `env_command` and parse what it printed.
///
/// The returned kind carries no config or field. The caller adds them.
pub(crate) fn run(
    spec: &RunSpec<'_>,
    cancel: &CancelHandle,
) -> Result<CommandOutput, EnvCommandErrorKind> {
    let captured = zaz_process::capture(
        &CaptureSpec {
            shell: spec.shell,
            command: spec.command,
            working_dir: spec.working_dir,
            env: spec.env,
            timeout: spec.timeout,
            max_stdout: MAX_STDOUT_BYTES,
            max_stderr: MAX_STDERR_BYTES,
        },
        cancel,
    )
    .map_err(|e| classify(e, spec))?;

    interpret(captured)
}

fn classify(error: CaptureError, spec: &RunSpec<'_>) -> EnvCommandErrorKind {
    match error {
        CaptureError::Spawn(source) => EnvCommandErrorKind::SpawnFailed {
            detail: if spec.working_dir.is_dir() {
                format!("the shell '{}' did not start: {source}", spec.shell)
            } else {
                format!(
                    "the working directory '{}' does not exist or is not a directory",
                    spec.working_dir.display()
                )
            },
        },
        CaptureError::Read(source) => EnvCommandErrorKind::SpawnFailed {
            detail: format!("reading the command's output failed: {source}"),
        },
        CaptureError::Timeout { .. } => EnvCommandErrorKind::Timeout {
            limit: spec.timeout,
        },
        CaptureError::HeldPipe { exited_ago } => EnvCommandErrorKind::HeldPipe {
            exited_ago,
            limit: spec.timeout,
        },
        CaptureError::StdoutTooLarge { limit } => EnvCommandErrorKind::StdoutTooLarge { limit },
        CaptureError::StderrTooLarge { limit } => EnvCommandErrorKind::StderrTooLarge { limit },
        CaptureError::Cancelled => EnvCommandErrorKind::Cancelled,
    }
}

/// Turn a finished command into its output, or the reason it failed.
///
/// Output is parsed only after exit 0 and end of output on both pipes. Stderr from a successful
/// run is logged at debug level.
fn interpret(captured: Captured) -> Result<CommandOutput, EnvCommandErrorKind> {
    if !captured.status.success() {
        return Err(EnvCommandErrorKind::Failed {
            status: describe_status(captured.status),
            stderr: embeddable_stderr(&captured.stderr),
        });
    }

    if !captured.stderr.is_empty() {
        tracing::debug!(
            stderr = %embeddable_stderr(&captured.stderr),
            "env_command wrote to stderr"
        );
    }

    parse_output(&captured.stdout)
        .map_err(|problem| EnvCommandErrorKind::MalformedOutput { problem })
}

fn describe_status(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with status {code}"),
        (None, Some(signal)) => format!("was killed by signal {signal}"),
        (None, None) => "ended abnormally".to_string(),
    }
}

/// Stderr as an error embeds it: lossy UTF-8, stripped of escape sequences, and capped.
fn embeddable_stderr(stderr: &[u8]) -> String {
    let stripped = strip_ansi(&String::from_utf8_lossy(stderr));
    let trimmed = stripped.trim();

    if trimmed.is_empty() {
        return "(no stderr)".to_string();
    }

    if trimmed.len() <= EMBEDDED_STDERR_BYTES {
        return trimmed.to_string();
    }

    let mut end = EMBEDDED_STDERR_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}\n(stderr truncated)", &trimmed[..end])
}

/// Parse an `env_command`'s stdout.
///
/// The problem never quotes the output. It names the kind of failure, the key when there is one,
/// and a byte or line position.
pub(crate) fn parse_output(stdout: &[u8]) -> Result<CommandOutput, String> {
    let text = std::str::from_utf8(stdout)
        .map_err(|e| format!("invalid UTF-8 at byte {}", e.valid_up_to()))?;

    if text.trim().is_empty() {
        return Ok(CommandOutput::new());
    }

    let mut deserializer = serde_json::Deserializer::from_str(text);

    let flat = FlatObject::deserialize(&mut deserializer).map_err(|e| describe_json_error(&e))?;

    deserializer.end().map_err(|e| describe_json_error(&e))?;

    Ok(flat.0)
}

fn describe_json_error(error: &serde_json::Error) -> String {
    match error.classify() {
        serde_json::error::Category::Data => error.to_string(),
        serde_json::error::Category::Eof => {
            format!(
                "the output ends early, at line {} column {}",
                error.line(),
                error.column()
            )
        }
        _ => format!(
            "not valid JSON at line {} column {}",
            error.line(),
            error.column()
        ),
    }
}

struct FlatObject(CommandOutput);

impl<'de> Deserialize<'de> for FlatObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ObjectVisitor)
    }
}

/// The top-level visitor. A map is the only shape it accepts.
///
/// serde_json keeps the last value of a repeated key when it deserializes into a map. This visitor
/// reads the entries itself so a repeat is an error.
struct ObjectVisitor;

impl<'de> Visitor<'de> for ObjectVisitor {
    type Value = FlatObject;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = CommandOutput::new();

        while let Some(key) = map.next_key::<String>()? {
            if key.is_empty() {
                return Err(de::Error::custom("a key is empty"));
            }

            if key.contains('=') || key.contains('\0') {
                return Err(de::Error::custom(format!(
                    "key {key:?} contains '=' or a NUL byte"
                )));
            }

            let value = map.next_value_seed(ValueSeed { key: &key })?;

            if entries.insert(key.clone(), value).is_some() {
                return Err(de::Error::custom(format!("key {key:?} appears twice")));
            }
        }

        Ok(FlatObject(entries))
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, _: A) -> Result<Self::Value, A::Error> {
        Err(de::Error::custom("expected an object but found an array"))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Err(E::custom("expected an object but found a boolean"))
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Err(E::custom("expected an object but found a number"))
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Err(E::custom("expected an object but found a number"))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Err(E::custom("expected an object but found a number"))
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Err(E::custom("expected an object but found a string"))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Err(E::custom("expected an object but found null"))
    }
}

struct ValueSeed<'a> {
    key: &'a str,
}

impl<'de> de::DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Option<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ValueSeed<'_> {
    type Value = Option<String>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string or null")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value.contains('\0') {
            return Err(E::custom(format!(
                "the value of key {:?} contains a NUL byte",
                self.key
            )));
        }

        Ok(Some(value.to_string()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Err(self.wrong("a boolean"))
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Err(self.wrong("a number"))
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Err(self.wrong("a number"))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Err(self.wrong("a number"))
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, _: A) -> Result<Self::Value, A::Error> {
        Err(self.wrong("an array"))
    }

    fn visit_map<A: MapAccess<'de>>(self, _: A) -> Result<Self::Value, A::Error> {
        Err(self.wrong("an object"))
    }
}

impl ValueSeed<'_> {
    fn wrong<E: de::Error>(&self, found: &str) -> E {
        E::custom(format!(
            "the value of key {:?} must be a string or null, but is {found}",
            self.key
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> CommandOutput {
        parse_output(text.as_bytes()).unwrap()
    }

    fn problem(text: &str) -> String {
        parse_output(text.as_bytes()).unwrap_err()
    }

    #[test]
    fn empty_and_whitespace_output_is_no_variables() {
        assert!(ok("").is_empty());
        assert!(ok(" \n\t\n").is_empty());
        assert!(ok("{}").is_empty());
    }

    #[test]
    fn strings_set_and_null_unsets() {
        let output = ok(r#"{"A": "1", "B": null, "C": ""}"#);

        assert_eq!(output["A"], Some("1".to_string()));
        assert_eq!(output["B"], None);
        assert_eq!(output["C"], Some(String::new()));
        assert_eq!(output.len(), 3);
    }

    #[test]
    fn escapes_decode_and_unicode_survives() {
        let output = ok(r#"{"A": "line\nbreak é 😀"}"#);

        assert_eq!(output["A"].as_deref(), Some("line\nbreak \u{e9} \u{1f600}"));
    }

    #[test]
    fn a_repeated_key_is_rejected_and_named() {
        let message = problem(r#"{"A": "1", "B": "x", "A": "2"}"#);

        assert!(message.contains("\"A\" appears twice"), "{message}");
    }

    #[test]
    fn contract_violations_are_named_without_echoing_values() {
        let secret = "hunter2-secret";
        let cases = [
            (format!(r#"["{secret}"]"#), "found an array"),
            (format!(r#""{secret}""#), "found a string"),
            ("42".to_string(), "found a number"),
            ("true".to_string(), "found a boolean"),
            ("null".to_string(), "found null"),
            (
                format!(r#"{{"A": 1234567}}"#),
                "key \"A\" must be a string or null, but is a number",
            ),
            (
                format!(r#"{{"A": {{"x": "{secret}"}}}}"#),
                "key \"A\" must be a string or null, but is an object",
            ),
            (format!(r#"{{"A": ["{secret}"]}}"#), "but is an array"),
            (r#"{"A": false}"#.to_string(), "but is a boolean"),
            (format!(r#"{{"": "{secret}"}}"#), "a key is empty"),
            (
                format!(r#"{{"A=B": "{secret}"}}"#),
                "contains '=' or a NUL byte",
            ),
            (
                format!(r#"{{"A\u0000B": "{secret}"}}"#),
                "contains '=' or a NUL byte",
            ),
            (
                format!(r#"{{"A": "{secret}\u0000"}}"#),
                "value of key \"A\" contains a NUL byte",
            ),
        ];

        for (input, expected) in cases {
            let message = problem(&input);

            assert!(message.contains(expected), "{input} -> {message}");
            assert!(!message.contains(secret), "{input} -> {message}");
        }
    }

    #[test]
    fn non_json_and_trailing_text_are_syntax_errors_with_a_position() {
        for input in [
            "FOO=bar\nBAZ=1\n",
            "{\"A\": \"1\"} extra",
            "{\"A\": \"1\"}{\"B\": \"2\"}",
        ] {
            let message = problem(input);

            assert!(
                message.contains("not valid JSON at line"),
                "{input} -> {message}"
            );
            assert!(!message.contains("bar"), "{message}");
        }
    }

    #[test]
    fn truncated_json_says_the_output_ends_early() {
        let message = problem(r#"{"A": "1""#);

        assert!(message.contains("ends early"), "{message}");
    }

    #[test]
    fn invalid_utf8_reports_the_byte_offset() {
        let message = parse_output(b"{\"A\": \"\xff\"}").unwrap_err();

        assert!(message.contains("invalid UTF-8 at byte 7"), "{message}");
    }

    #[test]
    fn stderr_is_stripped_capped_and_never_empty() {
        assert_eq!(embeddable_stderr(b""), "(no stderr)");
        assert_eq!(
            embeddable_stderr(b"\x1b[31mdirenv: error\x1b[0m\n"),
            "direnv: error"
        );

        let long = "é".repeat(EMBEDDED_STDERR_BYTES);
        let embedded = embeddable_stderr(long.as_bytes());

        assert!(embedded.ends_with("(stderr truncated)"));
        assert!(embedded.len() <= EMBEDDED_STDERR_BYTES + "\n(stderr truncated)".len());
    }

    #[test]
    fn a_failure_lists_stderr_beneath_a_headline_that_carries_the_code() {
        let error = EnvCommandError {
            config: PathBuf::from("/p/zaz.toml"),
            field: "settings env_command".to_string(),
            working_dir: PathBuf::from("/p"),
            kind: EnvCommandErrorKind::Failed {
                status: "exited with status 1".to_string(),
                stderr: "first\nsecond".to_string(),
            },
        };

        assert_eq!(
            error.with_code(),
            "/p/zaz.toml: settings env_command (in /p): exited with status 1 \
             [env_command_failed]\n    first\n    second"
        );
    }
}
