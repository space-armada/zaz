//! Unknown-key detection shared by every config entry point.
//!
//! The config is deserialized through `serde_ignored`, which reports the path of every key that
//! no struct field claims. Each path becomes a collected validation error with a location and a
//! did-you-mean hint.

use crate::error::{ConfigError, Span, ValidationError, ValidationErrorKind, ValidationErrors};
use crate::schema::{Config, Group, ServiceCommand, Settings, TaskCommand};
use crate::toml_spanned::key_span;
use crate::validate::suggest_similar;
use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use toml_edit::ImDocument;

/// One step in the path to an ignored key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathSeg {
    Key(String),
    Index(usize),
}

/// A deserialized config and the path of every key it ignored.
pub(crate) struct Collected {
    pub config: Config,
    pub ignored: Vec<Vec<PathSeg>>,
}

pub(crate) fn collect_toml(contents: &str) -> Result<Collected, ConfigError> {
    let mut ignored = Vec::new();

    let config = serde_ignored::deserialize(toml::Deserializer::new(contents), |path| {
        ignored.push(segments(&path));
    })
    .map_err(|e: toml::de::Error| ConfigError::Toml {
        message: e.message().to_string(),
        span: e.span(),
    })?;

    Ok(Collected { config, ignored })
}

pub(crate) fn collect_json(contents: &str) -> Result<Collected, ConfigError> {
    let mut ignored = Vec::new();
    let mut deserializer = serde_json::Deserializer::from_str(contents);

    let config = serde_ignored::deserialize(&mut deserializer, |path| {
        ignored.push(segments(&path));
    })
    .map_err(ConfigError::Json)?;
    deserializer.end().map_err(ConfigError::Json)?;

    Ok(Collected { config, ignored })
}

/// Turn the ignored paths into validation errors.
///
/// Locations come from `doc` when it is available. JSON configs pass `None`.
pub(crate) fn unknown_key_errors(
    ignored: &[Vec<PathSeg>],
    doc: Option<&ImDocument<&str>>,
    source: &str,
) -> ValidationErrors {
    let mut errors = ValidationErrors::new();

    for path in ignored {
        let Some((PathSeg::Key(key), parents)) = path.split_last() else {
            continue;
        };

        let (table, candidates) = match resolve_table(parents) {
            Some((level, table)) => (table, level.fields()),
            None => (String::new(), Vec::new()),
        };

        let mut error = ValidationError::new(ValidationErrorKind::UnknownKey {
            key: key.clone(),
            table,
        });

        if let Some(range) = doc.and_then(|doc| key_span(doc, path)) {
            error = error.with_span(Span::from_byte_offset(source, range.start));
        }
        if let Some(similar) = suggest_similar(key, &candidates) {
            error = error.with_hint(format!("did you mean '{}'?", similar));
        }

        errors.push(error);
    }

    errors
}

fn segments(path: &serde_ignored::Path) -> Vec<PathSeg> {
    use serde_ignored::Path;

    let mut out = match path {
        Path::Root => Vec::new(),
        Path::Seq { parent, .. } | Path::Map { parent, .. } => segments(parent),
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => segments(parent),
    };

    match path {
        Path::Seq { index, .. } => out.push(PathSeg::Index(*index)),
        Path::Map { key, .. } => out.push(PathSeg::Key(key.clone())),
        _ => {}
    }

    out
}

/// The config table a key can sit in.
#[derive(Clone, Copy)]
enum Level {
    Config,
    Settings,
    Group,
    Task,
    Service,
}

impl Level {
    /// Every key the table accepts, aliases included, read from serde's struct metadata.
    fn fields(self) -> Vec<&'static str> {
        match self {
            Level::Config => fields_of::<Config>(),
            Level::Settings => fields_of::<Settings>(),
            Level::Group => fields_of::<Group>(),
            Level::Task => fields_of::<TaskCommand>(),
            Level::Service => fields_of::<ServiceCommand>(),
        }
    }
}

/// Work out which table holds a key, and name that table the way the file spells it.
fn resolve_table(parents: &[PathSeg]) -> Option<(Level, String)> {
    let mut level = Level::Config;
    let mut label = String::new();
    let mut rest = parents;

    while let Some((PathSeg::Key(name), tail)) = rest.split_first() {
        let next = match (level, name.as_str()) {
            (Level::Config, "settings") => {
                label = name.clone();
                rest = tail;
                Level::Settings
            }
            (Level::Config, "groups" | "group") => {
                let (index, tail) = split_index(tail)?;
                label = format!("{}[{}]", name, index);
                rest = tail;
                Level::Group
            }
            (Level::Group, "tasks" | "task") => {
                let (index, tail) = split_index(tail)?;
                label = format!("{}.{}[{}]", label, name, index);
                rest = tail;
                Level::Task
            }
            (Level::Group, "services" | "service" | "daemons" | "daemon") => {
                let (index, tail) = split_index(tail)?;
                label = format!("{}.{}[{}]", label, name, index);
                rest = tail;
                Level::Service
            }
            _ => return None,
        };
        level = next;
    }

    rest.is_empty().then_some((level, label))
}

fn split_index(path: &[PathSeg]) -> Option<(usize, &[PathSeg])> {
    match path.split_first()? {
        (PathSeg::Index(index), tail) => Some((*index, tail)),
        _ => None,
    }
}

/// Read the field names serde derived for `T`.
///
/// The derive hands its field list, aliases included, to `deserialize_struct`. A deserializer
/// that records the list and then fails exposes it without a hand-written copy.
fn fields_of<T: DeserializeOwned>() -> Vec<&'static str> {
    struct Probe<'a>(&'a mut Vec<&'static str>);

    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            self.0.extend_from_slice(fields);
            Err(de::Error::custom("field list captured"))
        }

        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("not a struct"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum
            identifier ignored_any
        }
    }

    let mut fields = Vec::new();
    let _ = T::deserialize(Probe(&mut fields));

    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fields_include_aliases() {
        let config = fields_of::<Config>();
        assert!(config.contains(&"groups") && config.contains(&"group"));

        let group = fields_of::<Group>();
        for name in ["tasks", "task", "services", "service", "daemon", "daemons"] {
            assert!(group.contains(&name), "{name}");
        }

        let settings = fields_of::<Settings>();
        assert!(settings.contains(&"debounce_ms"));

        let service = fields_of::<ServiceCommand>();
        assert!(service.contains(&"delay_ms"));
    }

    #[test]
    fn test_resolve_table_names_the_nesting() {
        let path = [
            PathSeg::Key("group".into()),
            PathSeg::Index(1),
            PathSeg::Key("daemon".into()),
            PathSeg::Index(0),
        ];

        let (_, label) = resolve_table(&path).unwrap();
        assert_eq!(label, "group[1].daemon[0]");

        assert_eq!(resolve_table(&[]).unwrap().1, "");
        assert!(resolve_table(&[PathSeg::Key("variables".into())]).is_none());
    }
}
