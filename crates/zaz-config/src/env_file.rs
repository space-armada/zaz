//! The `env_file` dialect, and reading an `env_file` from disk.
//!
//! dotenv has no specification, and its implementations disagree on quoting, escapes, multiline
//! values, the `export` prefix, and `${VAR}` expansion. zaz accepts the subset that means the same
//! thing under Docker Compose, dotenvy, and python-dotenv. Every point where those implementations
//! diverge is a parse error with its own code and a hint.
//!
//! An error names the line where its entry started. It never quotes a line or a value, since an
//! env file often holds secrets.
//!
//! The daemon's evaluation step and `zaz check` both read files through [`read_env_file`], so a
//! file fails with the same code in both.

use crate::resolve_against_config_dir;
use crate::schema::Config;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

/// Where in the config an `env_file` was declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvFileLevel {
    Settings,
    Group { group: usize },
    Task { group: usize, task: usize },
    Service { group: usize, service: usize },
}

/// One declared `env_file`, with its path resolved against the config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileSource {
    /// The level that declared the file.
    pub level: EnvFileLevel,
    /// Human-readable name of the field, such as `group 'api' env_file`.
    pub field: String,
    /// The path as written in the config.
    pub configured: String,
    /// The path resolved against the config directory.
    pub path: PathBuf,
}

/// Every `env_file` a config declares, in settings, group, task, then service order.
///
/// A relative path resolves against `config_dir`, which must be absolute. The path is not
/// normalized and symlinks are kept.
pub fn env_file_sources(config: &Config, config_dir: &Path) -> Vec<EnvFileSource> {
    let mut sources = Vec::new();

    let mut push = |level: EnvFileLevel, field: String, configured: &Option<String>| {
        if let Some(configured) = configured {
            sources.push(EnvFileSource {
                level,
                field,
                configured: configured.clone(),
                path: resolve_against_config_dir(config_dir, configured),
            });
        }
    };

    push(
        EnvFileLevel::Settings,
        "settings env_file".to_string(),
        &config.settings.env_file,
    );

    for (g, group) in config.groups.iter().enumerate() {
        push(
            EnvFileLevel::Group { group: g },
            format!("group '{}' env_file", group.name),
            &group.env_file,
        );

        for (t, task) in group.tasks.iter().enumerate() {
            push(
                EnvFileLevel::Task { group: g, task: t },
                format!("group '{}' task '{}' env_file", group.name, task.name()),
                &task.env_file,
            );
        }

        for (s, service) in group.services.iter().enumerate() {
            push(
                EnvFileLevel::Service {
                    group: g,
                    service: s,
                },
                format!(
                    "group '{}' service '{}' env_file",
                    group.name,
                    service.name()
                ),
                &service.env_file,
            );
        }
    }

    sources
}

/// What is wrong with an `env_file`.
#[derive(Debug)]
pub enum EnvFileErrorKind {
    /// The file does not exist.
    Missing,
    /// The file exists and cannot be read.
    PermissionDenied,
    /// The path names a directory.
    IsDirectory,
    /// Any other I/O failure.
    Io(std::io::Error),
    /// The file is larger than [`ENV_FILE_MAX_BYTES`].
    TooLarge { limit: u64 },
    /// The file starts with a UTF-8 byte order mark.
    ByteOrderMark,
    /// The file is not valid UTF-8.
    InvalidUtf8 { line: usize },
    /// The file holds a NUL byte.
    NulByte { line: usize },
    /// An entry is outside the dialect.
    Parse { line: usize, problem: ParseProblem },
}

/// A rule of the dialect that an entry broke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseProblem {
    MissingEquals,
    InvalidKey,
    SpaceAroundEquals,
    DuplicateKey { key: String, first_line: usize },
    InlineComment,
    UnquotedWhitespace,
    UnquotedQuote,
    DollarSign,
    Backslash,
    BackslashInSingleQuotes,
    BadEscape,
    TrailingContent,
    UnterminatedQuote,
}

impl ParseProblem {
    fn code(&self) -> &'static str {
        match self {
            Self::MissingEquals => "env_file_missing_equals",
            Self::InvalidKey => "env_file_invalid_key",
            Self::SpaceAroundEquals => "env_file_space_around_equals",
            Self::DuplicateKey { .. } => "env_file_duplicate_key",
            Self::InlineComment => "env_file_inline_comment",
            Self::UnquotedWhitespace => "env_file_unquoted_whitespace",
            Self::UnquotedQuote => "env_file_unquoted_quote",
            Self::DollarSign => "env_file_dollar_sign",
            Self::Backslash => "env_file_backslash",
            Self::BackslashInSingleQuotes => "env_file_backslash_in_single_quotes",
            Self::BadEscape => "env_file_bad_escape",
            Self::TrailingContent => "env_file_trailing_content",
            Self::UnterminatedQuote => "env_file_unterminated_quote",
        }
    }

    fn hint(&self) -> &'static str {
        match self {
            Self::MissingEquals => "write the entry as KEY=VALUE, or remove the line",
            Self::InvalidKey => {
                "a key starts with a letter or underscore and holds only letters, digits, and \
                 underscores"
            }
            Self::SpaceAroundEquals => "remove the whitespace around =, as in KEY=VALUE",
            Self::DuplicateKey { .. } => "remove one of the two entries",
            Self::InlineComment => {
                "move the comment to its own line, or quote the value if # is part of it"
            }
            Self::UnquotedWhitespace => "quote the value, as in KEY='a b'",
            Self::UnquotedQuote => "wrap the whole value in quotes",
            Self::DollarSign => {
                "single-quote the value to keep the $ literal, since zaz does not expand \
                 variables in an env_file"
            }
            Self::Backslash => {
                "quote the value, and write a backslash as \\\\ inside double quotes"
            }
            Self::BackslashInSingleQuotes => {
                "dotenv implementations disagree on a backslash in single quotes, so use double \
                 quotes and write it as \\\\"
            }
            Self::BadEscape => "double quotes accept only the escapes \\n, \\\" and \\\\",
            Self::TrailingContent => "nothing may follow the closing quote on its line",
            Self::UnterminatedQuote => "close the quote, or remove it",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::MissingEquals => "entry has no =".to_string(),
            Self::InvalidKey => "invalid key".to_string(),
            Self::SpaceAroundEquals => "whitespace around =".to_string(),
            Self::DuplicateKey { key, first_line } => {
                format!("duplicate key {key} (first defined on line {first_line})")
            }
            Self::InlineComment => "comment after a value".to_string(),
            Self::UnquotedWhitespace => "whitespace in an unquoted value".to_string(),
            Self::UnquotedQuote => "quote character in an unquoted value".to_string(),
            Self::DollarSign => "$ outside single quotes".to_string(),
            Self::Backslash => "backslash in an unquoted value".to_string(),
            Self::BackslashInSingleQuotes => "backslash in a single-quoted value".to_string(),
            Self::BadEscape => "unsupported escape in a double-quoted value".to_string(),
            Self::TrailingContent => "content after the closing quote".to_string(),
            Self::UnterminatedQuote => "unterminated quote".to_string(),
        }
    }
}

/// A failure to load an `env_file`.
///
/// The error names the config, the field, and the resolved absolute path. It never holds the
/// file's content.
#[derive(Debug)]
pub struct EnvFileError {
    /// The config file that declared the `env_file`.
    pub config: PathBuf,
    /// The declaring field, such as `group 'api' env_file`.
    pub field: String,
    /// The resolved path of the env file.
    pub path: PathBuf,
    /// What went wrong.
    pub kind: EnvFileErrorKind,
}

impl EnvFileError {
    /// Error code for JSON output.
    pub fn code(&self) -> &'static str {
        match &self.kind {
            EnvFileErrorKind::Missing => "env_file_missing",
            EnvFileErrorKind::PermissionDenied => "env_file_permission_denied",
            EnvFileErrorKind::IsDirectory => "env_file_is_directory",
            EnvFileErrorKind::Io(_) => "env_file_io",
            EnvFileErrorKind::TooLarge { .. } => "env_file_too_large",
            EnvFileErrorKind::ByteOrderMark => "env_file_byte_order_mark",
            EnvFileErrorKind::InvalidUtf8 { .. } => "env_file_invalid_utf8",
            EnvFileErrorKind::NulByte { .. } => "env_file_nul_byte",
            EnvFileErrorKind::Parse { problem, .. } => problem.code(),
        }
    }

    /// A suggested fix.
    pub fn hint(&self) -> Option<&'static str> {
        match &self.kind {
            EnvFileErrorKind::Missing => {
                Some("create the file, or remove env_file from the config")
            }
            EnvFileErrorKind::PermissionDenied => Some("make the file readable by the daemon"),
            EnvFileErrorKind::IsDirectory => Some("point env_file at a file, not a directory"),
            EnvFileErrorKind::Io(_) => None,
            EnvFileErrorKind::TooLarge { .. } => {
                Some("an env_file holds KEY=VALUE lines, so point it at a smaller file")
            }
            EnvFileErrorKind::ByteOrderMark => {
                Some("save the file without a byte order mark, since dotenv implementations disagree on one")
            }
            EnvFileErrorKind::InvalidUtf8 { .. } => Some("save the file as UTF-8"),
            EnvFileErrorKind::NulByte { .. } => Some("remove the NUL byte"),
            EnvFileErrorKind::Parse { problem, .. } => Some(problem.hint()),
        }
    }

    /// The line of the env file that holds the problem, when the problem has one.
    pub fn line(&self) -> Option<usize> {
        match &self.kind {
            EnvFileErrorKind::InvalidUtf8 { line }
            | EnvFileErrorKind::NulByte { line }
            | EnvFileErrorKind::Parse { line, .. } => Some(*line),
            _ => None,
        }
    }

    /// The field, the path, and the problem, for places that print the config separately.
    pub fn message(&self) -> String {
        format!("{} {}: {}", self.field, self.path.display(), self.detail())
    }

    /// The problem alone.
    pub fn detail(&self) -> String {
        let problem = match &self.kind {
            EnvFileErrorKind::Missing => "file not found".to_string(),
            EnvFileErrorKind::PermissionDenied => "permission denied".to_string(),
            EnvFileErrorKind::IsDirectory => "path is a directory".to_string(),
            EnvFileErrorKind::Io(source) => format!("cannot read file: {source}"),
            EnvFileErrorKind::TooLarge { limit } => {
                format!("file is larger than the {limit} byte limit")
            }
            EnvFileErrorKind::ByteOrderMark => "file starts with a byte order mark".to_string(),
            EnvFileErrorKind::InvalidUtf8 { .. } => "file is not valid UTF-8".to_string(),
            EnvFileErrorKind::NulByte { .. } => "file contains a NUL byte".to_string(),
            EnvFileErrorKind::Parse { problem, .. } => problem.message(),
        };

        match self.line() {
            Some(line) => format!("line {line}: {problem}"),
            None => problem,
        }
    }
}

impl fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.config.display(), self.message())
    }
}

impl std::error::Error for EnvFileError {}

/// The largest `env_file` zaz reads.
///
/// The read goes through a limit so a file that never ends, such as `/dev/zero`, cannot exhaust
/// memory. Named pipes and other non-regular files are read like any file.
pub const ENV_FILE_MAX_BYTES: u64 = 1024 * 1024;

/// Read and parse one declared `env_file`.
///
/// `config` is the config file that declared it, and only appears in the errors. A file-level
/// problem returns one error. A file that reads cleanly returns every entry error it holds.
pub fn read_env_file(
    config: &Path,
    source: &EnvFileSource,
) -> Result<BTreeMap<String, String>, Vec<EnvFileError>> {
    let fail = |kind: EnvFileErrorKind| EnvFileError {
        config: config.to_path_buf(),
        field: source.field.clone(),
        path: source.path.clone(),
        kind,
    };

    let io_failure = |e: std::io::Error| {
        vec![fail(match e.kind() {
            ErrorKind::NotFound => EnvFileErrorKind::Missing,
            ErrorKind::PermissionDenied => EnvFileErrorKind::PermissionDenied,
            ErrorKind::IsADirectory => EnvFileErrorKind::IsDirectory,
            _ if source.path.is_dir() => EnvFileErrorKind::IsDirectory,
            _ => EnvFileErrorKind::Io(e),
        })]
    };

    let mut bytes = Vec::new();
    std::fs::File::open(&source.path)
        .and_then(|file| file.take(ENV_FILE_MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(io_failure)?;

    if bytes.len() as u64 > ENV_FILE_MAX_BYTES {
        return Err(vec![fail(EnvFileErrorKind::TooLarge {
            limit: ENV_FILE_MAX_BYTES,
        })]);
    }

    let text = std::str::from_utf8(&bytes).map_err(|e| {
        let line = bytes[..e.valid_up_to()]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1;
        vec![fail(EnvFileErrorKind::InvalidUtf8 { line })]
    })?;

    if text.starts_with('\u{feff}') {
        return Err(vec![fail(EnvFileErrorKind::ByteOrderMark)]);
    }

    if let Some(offset) = text.find('\0') {
        let line = text[..offset].matches('\n').count() + 1;
        return Err(vec![fail(EnvFileErrorKind::NulByte { line })]);
    }

    parse_env_file(text).map_err(|problems| {
        problems
            .into_iter()
            .map(|(line, problem)| fail(EnvFileErrorKind::Parse { line, problem }))
            .collect()
    })
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();

    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }

    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse the text of an env file.
///
/// Every entry that breaks a rule is reported, each with the line where it started and the rule it
/// broke. An unterminated double quote ends the scan, since everything after it is ambiguous.
pub fn parse_env_file(text: &str) -> Result<BTreeMap<String, String>, Vec<(usize, ParseProblem)>> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    let mut vars = BTreeMap::new();
    let mut problems = Vec::new();
    let mut first_lines: HashMap<String, usize> = HashMap::new();
    let mut index = 0;

    while index < lines.len() {
        let line_number = index + 1;
        let trimmed = lines[index].trim_start_matches(is_blank);

        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }

        let entry = match trimmed.strip_prefix("export") {
            Some(rest) if rest.starts_with(is_blank) => rest.trim_start_matches(is_blank),
            _ => trimmed,
        };

        let parsed = parse_entry(entry, &lines, index);

        let (key, value, last_index) = match parsed {
            Ok(parsed) => parsed,
            Err((problem, last_index)) => {
                problems.push((line_number, problem));

                if last_index >= lines.len() {
                    break;
                }

                index = last_index + 1;
                continue;
            }
        };

        if let Some(first_line) = first_lines.get(key) {
            problems.push((
                line_number,
                ParseProblem::DuplicateKey {
                    key: key.to_string(),
                    first_line: *first_line,
                },
            ));
        } else {
            first_lines.insert(key.to_string(), line_number);
            vars.insert(key.to_string(), value);
        }

        index = last_index + 1;
    }

    if problems.is_empty() {
        Ok(vars)
    } else {
        Err(problems)
    }
}

/// Parse one entry that starts on line `index`.
///
/// On success returns the key, the value, and the index of the last line the entry spans. On
/// failure returns the problem and the index of the last line the entry spans, which is past the
/// end of the file when the entry cannot be delimited.
fn parse_entry<'a>(
    entry: &'a str,
    lines: &[&str],
    index: usize,
) -> Result<(&'a str, String, usize), (ParseProblem, usize)> {
    let fail = |problem: ParseProblem| (problem, index);

    let Some((key_part, value_part)) = entry.split_once('=') else {
        return Err(fail(ParseProblem::MissingEquals));
    };

    let key = key_part.trim_end_matches(is_blank);
    if !is_valid_key(key) {
        return Err(fail(ParseProblem::InvalidKey));
    }

    if key.len() != key_part.len() {
        return Err(fail(ParseProblem::SpaceAroundEquals));
    }

    if value_part.trim_matches(is_blank).is_empty() {
        return Ok((key, String::new(), index));
    }

    if value_part.starts_with(is_blank) {
        return Err(fail(ParseProblem::SpaceAroundEquals));
    }

    if let Some(body) = value_part.strip_prefix('\'') {
        let value = parse_single_quoted(body).map_err(fail)?;
        return Ok((key, value, index));
    }

    if let Some(body) = value_part.strip_prefix('"') {
        let (value, last_index) = parse_double_quoted(body, lines, index)?;
        return Ok((key, value, last_index));
    }

    let value = parse_unquoted(value_part).map_err(fail)?;

    Ok((key, value, index))
}

fn parse_unquoted(value: &str) -> Result<String, ParseProblem> {
    let value = value.trim_end_matches(is_blank);

    for (offset, c) in value.char_indices() {
        match c {
            c if is_blank(c) => {
                let rest = value[offset..].trim_start_matches(is_blank);
                return Err(if rest.starts_with('#') {
                    ParseProblem::InlineComment
                } else {
                    ParseProblem::UnquotedWhitespace
                });
            }
            '#' => return Err(ParseProblem::InlineComment),
            '\'' | '"' => return Err(ParseProblem::UnquotedQuote),
            '$' => return Err(ParseProblem::DollarSign),
            '\\' => return Err(ParseProblem::Backslash),
            _ => {}
        }
    }

    Ok(value.to_string())
}

/// `body` is the line after the opening quote.
fn parse_single_quoted(body: &str) -> Result<String, ParseProblem> {
    let Some(close) = body.find('\'') else {
        return Err(ParseProblem::UnterminatedQuote);
    };

    let content = &body[..close];
    if content.contains('\\') {
        return Err(ParseProblem::BackslashInSingleQuotes);
    }

    check_after_closing_quote(&body[close + 1..])?;

    Ok(content.to_string())
}

/// `body` is the first line after the opening quote. Returns the value and the index of the last
/// line the value spans.
///
/// A bad escape or a `$` does not stop the scan. The scan runs on to the closing quote so the
/// caller knows where the entry ends, and the first problem found is reported. Only a missing
/// closing quote is unrecoverable, and it reports an end index past the last line.
fn parse_double_quoted(
    body: &str,
    lines: &[&str],
    start: usize,
) -> Result<(String, usize), (ParseProblem, usize)> {
    let mut value = String::new();
    let mut problem = None;
    let mut index = start;
    let mut segment = body;

    loop {
        let mut chars = segment.char_indices();

        while let Some((offset, c)) = chars.next() {
            match c {
                '"' => {
                    if let Err(trailing) = check_after_closing_quote(&segment[offset + 1..]) {
                        problem.get_or_insert(trailing);
                    }

                    return match problem {
                        Some(problem) => Err((problem, index)),
                        None => Ok((value, index)),
                    };
                }
                '$' => {
                    problem.get_or_insert(ParseProblem::DollarSign);
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => value.push('\n'),
                    Some((_, '"')) => value.push('"'),
                    Some((_, '\\')) => value.push('\\'),
                    _ => {
                        problem.get_or_insert(ParseProblem::BadEscape);
                    }
                },
                c => value.push(c),
            }
        }

        index += 1;
        let Some(next) = lines.get(index) else {
            return Err((ParseProblem::UnterminatedQuote, lines.len()));
        };

        value.push('\n');
        segment = next;
    }
}

fn check_after_closing_quote(rest: &str) -> Result<(), ParseProblem> {
    let rest = rest.trim_matches(is_blank);

    if rest.is_empty() {
        Ok(())
    } else if rest.starts_with('#') {
        Err(ParseProblem::InlineComment)
    } else {
        Err(ParseProblem::TrailingContent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> BTreeMap<String, String> {
        parse_env_file(text).unwrap()
    }

    fn problem(text: &str) -> (usize, ParseProblem) {
        let mut problems = parse_env_file(text).unwrap_err();
        assert_eq!(problems.len(), 1, "{text}: {problems:?}");

        problems.remove(0)
    }

    #[test]
    fn plain_entries_comments_and_blank_lines() {
        let vars = parse("# comment\n\nA=1\n  B=two\n\t# indented comment\nC=\n");

        assert_eq!(vars.len(), 3);
        assert_eq!(vars["A"], "1");
        assert_eq!(vars["B"], "two");
        assert_eq!(vars["C"], "");
    }

    #[test]
    fn export_prefix_is_optional() {
        let vars = parse("export A=1\nexport   B=2\nexportC=3\nexport=4\n");

        assert_eq!(vars["A"], "1");
        assert_eq!(vars["B"], "2");
        assert_eq!(vars["exportC"], "3");
        assert_eq!(vars["export"], "4");
    }

    #[test]
    fn trailing_whitespace_and_carriage_returns_are_stripped() {
        let vars = parse("A=1  \r\nB='x y'  \r\nC=\"z \"\t\r\nD=   \r\n");

        assert_eq!(vars["A"], "1");
        assert_eq!(vars["B"], "x y");
        assert_eq!(vars["C"], "z ");
        assert_eq!(vars["D"], "");
    }

    #[test]
    fn single_quotes_are_literal() {
        let vars = parse("HASH='$2b$12$abc'\nHASHES='#not a comment'\nQ='say \"hi\"'\n");

        assert_eq!(vars["HASH"], "$2b$12$abc");
        assert_eq!(vars["HASHES"], "#not a comment");
        assert_eq!(vars["Q"], "say \"hi\"");
    }

    #[test]
    fn double_quotes_keep_inner_whitespace_and_decode_three_escapes() {
        let vars = parse("A=\"  padded  \"\nB=\"line\\nbreak \\\"q\\\" back\\\\slash\"\n");

        assert_eq!(vars["A"], "  padded  ");
        assert_eq!(vars["B"], "line\nbreak \"q\" back\\slash");
    }

    #[test]
    fn double_quoted_values_may_span_lines() {
        let pem = "KEY=\"-----BEGIN-----\nabc\n-----END-----\"\nNEXT=1\n";
        let vars = parse(pem);

        assert_eq!(vars["KEY"], "-----BEGIN-----\nabc\n-----END-----");
        assert_eq!(vars["NEXT"], "1");
    }

    #[test]
    fn entry_after_a_multiline_value_reports_its_own_line() {
        assert_eq!(
            problem("A=\"one\ntwo\"\nB\n"),
            (3, ParseProblem::MissingEquals)
        );
    }

    #[test]
    fn missing_equals_is_an_error() {
        assert_eq!(problem("A=1\nBARE\n"), (2, ParseProblem::MissingEquals));
        assert_eq!(problem("export\n"), (1, ParseProblem::MissingEquals));
    }

    #[test]
    fn invalid_keys_are_errors() {
        for text in ["=x", "1A=x", "A-B=x", "A B=x", "é=x", "export =x"] {
            assert_eq!(problem(text), (1, ParseProblem::InvalidKey), "{text}");
        }
    }

    #[test]
    fn whitespace_around_equals_is_an_error() {
        for text in ["A =1", "A= 1", "A = 1", "A\t=1", "A= 'x'"] {
            assert_eq!(
                problem(text),
                (1, ParseProblem::SpaceAroundEquals),
                "{text}"
            );
        }
    }

    #[test]
    fn duplicate_key_names_both_lines() {
        assert_eq!(
            problem("A=1\n\nB=2\nA=3\n"),
            (
                4,
                ParseProblem::DuplicateKey {
                    key: "A".to_string(),
                    first_line: 1
                }
            )
        );
        assert_eq!(
            problem("export A=1\nA=2\n").1,
            ParseProblem::DuplicateKey {
                key: "A".to_string(),
                first_line: 1
            }
        );
    }

    #[test]
    fn comment_after_a_value_is_an_error() {
        for text in [
            "A=1 # note",
            "A=1#note",
            "A='x' # note",
            "A=\"x\"#note",
            "A=\"x\ny\" # note",
        ] {
            assert_eq!(problem(text).1, ParseProblem::InlineComment, "{text}");
        }
    }

    #[test]
    fn unquoted_value_rejects_whitespace_quotes_dollar_and_backslash() {
        assert_eq!(problem("A=a b").1, ParseProblem::UnquotedWhitespace);
        assert_eq!(problem("A=it's").1, ParseProblem::UnquotedQuote);
        assert_eq!(problem("A=a\"b").1, ParseProblem::UnquotedQuote);
        assert_eq!(problem("A=${B}").1, ParseProblem::DollarSign);
        assert_eq!(problem("A=$B").1, ParseProblem::DollarSign);
        assert_eq!(problem("A=C:\\dir").1, ParseProblem::Backslash);
    }

    #[test]
    fn dollar_in_double_quotes_is_an_error() {
        assert_eq!(problem("A=\"${B}\"").1, ParseProblem::DollarSign);
        assert_eq!(problem("A=\"a\nb$\"").1, ParseProblem::DollarSign);
        assert_eq!(problem("A=\"\\$\"").1, ParseProblem::BadEscape);
    }

    #[test]
    fn backslash_in_single_quotes_is_an_error() {
        assert_eq!(problem("A='a\\b'").1, ParseProblem::BackslashInSingleQuotes);
        assert_eq!(problem("A='\\''").1, ParseProblem::BackslashInSingleQuotes);
    }

    #[test]
    fn other_escapes_in_double_quotes_are_errors() {
        for text in ["A=\"\\t\"", "A=\"\\x\"", "A=\"\\'\"", "A=\"end\\\nmore\""] {
            assert_eq!(problem(text).1, ParseProblem::BadEscape, "{text}");
        }
    }

    #[test]
    fn content_after_the_closing_quote_is_an_error() {
        assert_eq!(problem("A='x'y").1, ParseProblem::TrailingContent);
        assert_eq!(problem("A=\"x\" \"y\"").1, ParseProblem::TrailingContent);
    }

    #[test]
    fn unterminated_quote_names_the_line_it_opened_on() {
        assert_eq!(
            problem("A=1\nB='open\nC=2\n"),
            (2, ParseProblem::UnterminatedQuote)
        );
        assert_eq!(
            problem("A=1\nB=\"open\nC=2\n"),
            (2, ParseProblem::UnterminatedQuote)
        );
    }

    #[test]
    fn errors_inside_a_multiline_value_name_the_line_the_entry_started_on() {
        assert_eq!(
            problem("A=1\nB=\"x\ny\\q\"\n"),
            (2, ParseProblem::BadEscape)
        );
    }

    #[test]
    fn every_problem_has_a_distinct_code() {
        let problems = [
            ParseProblem::MissingEquals,
            ParseProblem::InvalidKey,
            ParseProblem::SpaceAroundEquals,
            ParseProblem::DuplicateKey {
                key: "A".to_string(),
                first_line: 1,
            },
            ParseProblem::InlineComment,
            ParseProblem::UnquotedWhitespace,
            ParseProblem::UnquotedQuote,
            ParseProblem::DollarSign,
            ParseProblem::Backslash,
            ParseProblem::BackslashInSingleQuotes,
            ParseProblem::BadEscape,
            ParseProblem::TrailingContent,
            ParseProblem::UnterminatedQuote,
        ];

        let mut codes: Vec<_> = problems.iter().map(ParseProblem::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), problems.len());

        for p in &problems {
            assert!(p.code().starts_with("env_file_"));
            assert!(!p.hint().is_empty());
        }
    }

    fn source_for(path: &Path) -> EnvFileSource {
        EnvFileSource {
            level: EnvFileLevel::Group { group: 0 },
            field: "group 'api' env_file".to_string(),
            configured: path.display().to_string(),
            path: path.to_path_buf(),
        }
    }

    fn read(path: &Path) -> Result<BTreeMap<String, String>, Vec<EnvFileError>> {
        read_env_file(Path::new("/proj/zaz.toml"), &source_for(path))
    }

    fn read_err(path: &Path) -> EnvFileError {
        let mut errors = read(path).unwrap_err();
        assert_eq!(errors.len(), 1, "{errors:?}");

        errors.remove(0)
    }

    #[test]
    fn every_entry_error_in_a_file_is_reported_with_its_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.env");
        std::fs::write(
            &path,
            "GOOD=1\nA B=2\nSECRET=a b\nC=\"x\\q\ny\"\nGOOD=2\nTAIL=ok\nLAST\n",
        )
        .unwrap();

        let errors = read(&path).unwrap_err();

        let found: Vec<_> = errors
            .iter()
            .map(|e| (e.line().unwrap(), e.code()))
            .collect();
        assert_eq!(
            found,
            vec![
                (2, "env_file_invalid_key"),
                (3, "env_file_unquoted_whitespace"),
                (4, "env_file_bad_escape"),
                (6, "env_file_duplicate_key"),
                (8, "env_file_missing_equals"),
            ]
        );

        let shown = format!("{errors:?} {}", errors[1]);
        assert!(!shown.contains("SECRET"), "{shown}");
    }

    #[test]
    fn an_unterminated_double_quote_ends_the_scan() {
        let errors = parse_env_file("A=1 2\nB=\"open\nC=3 4\nD=5 6\n").unwrap_err();

        assert_eq!(
            errors,
            vec![
                (1, ParseProblem::UnquotedWhitespace),
                (2, ParseProblem::UnterminatedQuote),
            ]
        );
    }

    #[test]
    fn an_unterminated_single_quote_does_not_hide_later_errors() {
        let errors = parse_env_file("A='open\nB=1 2\n").unwrap_err();

        assert_eq!(
            errors,
            vec![
                (1, ParseProblem::UnterminatedQuote),
                (2, ParseProblem::UnquotedWhitespace),
            ]
        );
    }

    #[test]
    fn an_error_in_a_multiline_value_does_not_leak_its_lines_as_entries() {
        let errors = parse_env_file("A=\"one $\nBAD LINE\nthree\"\nB=1 2\n").unwrap_err();

        assert_eq!(
            errors,
            vec![
                (1, ParseProblem::DollarSign),
                (4, ParseProblem::UnquotedWhitespace),
            ]
        );
    }

    #[test]
    fn a_byte_order_mark_is_its_own_error_and_is_not_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.env");
        std::fs::write(&path, "\u{feff}A=1\n").unwrap();

        let err = read_err(&path);

        assert_eq!(err.code(), "env_file_byte_order_mark");
        assert!(err.hint().unwrap().contains("byte order mark"));
    }

    #[test]
    fn a_file_over_the_limit_is_a_coded_error_naming_path_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.env");
        let big = format!("A={}\n", "x".repeat(ENV_FILE_MAX_BYTES as usize));
        std::fs::write(&path, big).unwrap();

        let err = read_err(&path);

        assert_eq!(err.code(), "env_file_too_large");
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains(&ENV_FILE_MAX_BYTES.to_string()), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_files_are_read_under_the_same_limit() {
        let vars = read(Path::new("/dev/null")).unwrap();
        assert!(vars.is_empty());

        let err = read_err(Path::new("/dev/zero"));
        assert_eq!(err.code(), "env_file_too_large");
    }

    #[test]
    fn missing_file_is_a_coded_error_naming_config_field_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.env");

        let err = read_err(&path);

        assert_eq!(err.code(), "env_file_missing");
        let msg = err.to_string();
        assert!(msg.contains("/proj/zaz.toml"), "{msg}");
        assert!(msg.contains("group 'api' env_file"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    #[test]
    fn directory_is_a_coded_error() {
        let dir = tempfile::tempdir().unwrap();

        let err = read_err(dir.path());

        assert_eq!(err.code(), "env_file_is_directory");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_a_coded_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.env");
        std::fs::write(&path, "A=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        if std::fs::read(&path).is_ok() {
            return;
        }

        let err = read_err(&path);

        assert_eq!(err.code(), "env_file_permission_denied");
    }

    #[test]
    fn invalid_utf8_and_nul_bytes_name_their_line() {
        let dir = tempfile::tempdir().unwrap();
        let utf8 = dir.path().join("utf8.env");
        let nul = dir.path().join("nul.env");
        std::fs::write(&utf8, b"A=1\nB=\xff\n").unwrap();
        std::fs::write(&nul, b"A=1\nB=2\nC=\x003\n").unwrap();

        let err = read_err(&utf8);
        assert_eq!(err.code(), "env_file_invalid_utf8");
        assert_eq!(err.line(), Some(2));

        let err = read_err(&nul);
        assert_eq!(err.code(), "env_file_nul_byte");
        assert_eq!(err.line(), Some(3));
    }

    #[test]
    fn parse_errors_never_quote_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.env");
        std::fs::write(&path, "TOKEN=hunter2 extra\n").unwrap();

        let err = read_err(&path);

        assert_eq!(err.code(), "env_file_unquoted_whitespace");
        assert_eq!(err.line(), Some(1));
        let shown = format!("{err} {}", err.hint().unwrap_or_default());
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("TOKEN"), "{shown}");
    }

    #[test]
    fn a_valid_file_reads_into_a_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.env");
        std::fs::write(&path, "A=1\nB='two words'\n").unwrap();

        let vars = read(&path).unwrap();

        assert_eq!(vars["A"], "1");
        assert_eq!(vars["B"], "two words");
    }

    #[test]
    fn sources_cover_every_level_and_resolve_against_the_config_dir() {
        let config = crate::parse_toml(
            r#"
[settings]
env_file = "root.env"

[[group]]
name = "api"
patterns = ["*.rs"]
env_file = "/abs/group.env"

[[group.task]]
name = "build"
command = "make"
env_file = "sub/task.env"

[[group.service]]
name = "web"
command = "run"
env_file = "svc.env"
"#,
        )
        .unwrap();

        let sources = env_file_sources(&config, Path::new("/proj"));

        let summary: Vec<_> = sources
            .iter()
            .map(|s| (s.level, s.path.to_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (EnvFileLevel::Settings, "/proj/root.env".to_string()),
                (
                    EnvFileLevel::Group { group: 0 },
                    "/abs/group.env".to_string()
                ),
                (
                    EnvFileLevel::Task { group: 0, task: 0 },
                    "/proj/sub/task.env".to_string()
                ),
                (
                    EnvFileLevel::Service {
                        group: 0,
                        service: 0
                    },
                    "/proj/svc.env".to_string()
                ),
            ]
        );
        assert_eq!(sources[2].field, "group 'api' task 'build' env_file");
        assert_eq!(sources[3].field, "group 'api' service 'web' env_file");
    }
}
