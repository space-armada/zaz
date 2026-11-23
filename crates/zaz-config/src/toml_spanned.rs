//! Source positions for TOML configs.
//!
//! The config is deserialized into the plain schema types. Positions come from a separate walk
//! of a `toml_edit` document, which keeps the span of every key and value.

use crate::error::Span;
use crate::unknown_keys::PathSeg;
use std::collections::HashMap;
use std::ops::Range;
use toml_edit::{ImDocument, Item, TableLike, Value};

/// Span information extracted from TOML parsing.
#[derive(Debug, Clone, Default)]
pub struct SpanInfo {
    /// Spans for group names, keyed by group index.
    pub group_names: HashMap<usize, Range<usize>>,
    /// Spans for dependency references, keyed by (group_index, dep_index).
    pub dependencies: HashMap<(usize, usize), Range<usize>>,
}

impl SpanInfo {
    /// Collect the group name and dependency spans from a parsed document.
    pub(crate) fn from_document(doc: &ImDocument<&str>) -> Self {
        let mut info = Self::default();
        let root = Node::Item(doc.as_item());

        let Some(groups) = root.key("groups").or_else(|| root.key("group")) else {
            return info;
        };

        let mut group_index = 0;
        while let Some(group) = groups.index(group_index) {
            if let Some(span) = group.key("name").and_then(|name| name.span()) {
                info.group_names.insert(group_index, span);
            }

            if let Some(deps) = group.key("depends_on") {
                let mut dep_index = 0;
                while let Some(dep) = deps.index(dep_index) {
                    if let Some(span) = dep.span() {
                        info.dependencies.insert((group_index, dep_index), span);
                    }
                    dep_index += 1;
                }
            }

            group_index += 1;
        }

        info
    }

    /// Get a Span for a group name, converting byte offset to line/column.
    pub fn group_name_span(&self, source: &str, group_index: usize) -> Option<Span> {
        self.group_names
            .get(&group_index)
            .map(|range| Span::from_byte_offset(source, range.start))
    }

    /// Get a Span for a dependency reference, converting byte offset to line/column.
    pub fn dependency_span(
        &self,
        source: &str,
        group_index: usize,
        dep_index: usize,
    ) -> Option<Span> {
        self.dependencies
            .get(&(group_index, dep_index))
            .map(|range| Span::from_byte_offset(source, range.start))
    }
}

/// Find the byte range of the key at the end of a path.
///
/// Every earlier segment is followed through tables, arrays of tables, and inline arrays.
pub(crate) fn key_span(doc: &ImDocument<&str>, path: &[PathSeg]) -> Option<Range<usize>> {
    let (last, parents) = path.split_last()?;
    let PathSeg::Key(key) = last else {
        return None;
    };

    let mut node = Node::Item(doc.as_item());
    for seg in parents {
        node = match seg {
            PathSeg::Key(name) => node.key(name)?,
            PathSeg::Index(index) => node.index(*index)?,
        };
    }

    let (key, _) = node.table()?.get_key_value(key)?;
    key.span()
}

/// A position in a `toml_edit` document.
///
/// Arrays of tables hand out `Table`, and inline arrays hand out `Value`, so a plain `Item`
/// cannot represent every step of a walk.
#[derive(Clone, Copy)]
enum Node<'a> {
    Item(&'a Item),
    Value(&'a Value),
    Table(&'a dyn TableLike),
}

impl<'a> Node<'a> {
    fn table(self) -> Option<&'a dyn TableLike> {
        match self {
            Node::Item(item) => item.as_table_like(),
            Node::Value(value) => value.as_inline_table().map(|t| t as &dyn TableLike),
            Node::Table(table) => Some(table),
        }
    }

    fn key(self, name: &str) -> Option<Node<'a>> {
        self.table()?.get(name).map(Node::Item)
    }

    fn index(self, index: usize) -> Option<Node<'a>> {
        match self {
            Node::Item(Item::ArrayOfTables(tables)) => {
                tables.get(index).map(|t| Node::Table(t as &dyn TableLike))
            }
            Node::Item(Item::Value(value)) | Node::Value(value) => {
                value.as_array()?.get(index).map(Node::Value)
            }
            _ => None,
        }
    }

    fn span(self) -> Option<Range<usize>> {
        match self {
            Node::Item(item) => item.span(),
            Node::Value(value) => value.span(),
            Node::Table(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_of(source: &str) -> SpanInfo {
        SpanInfo::from_document(&ImDocument::parse(source).unwrap())
    }

    #[test]
    fn test_group_and_dependency_spans() {
        let toml = r#"
[[group]]
name = "backend"
patterns = ["*.go"]
depends_on = ["frontend"]

[[group]]
name = "frontend"
patterns = ["*.ts"]
"#;
        let spans = spans_of(toml);

        assert!(spans.group_names.contains_key(&0));
        assert!(spans.group_names.contains_key(&1));
        assert!(spans.dependencies.contains_key(&(0, 0)));

        let name = spans.group_name_span(toml, 0).unwrap();
        assert_eq!((name.line, name.column), (3, 8));

        let dep = spans.dependency_span(toml, 0, 0).unwrap();
        assert_eq!((dep.line, dep.column), (5, 15));
    }

    #[test]
    fn test_group_spans_with_the_groups_spelling() {
        let toml = "groups = [{ name = \"a\", depends_on = [\"b\"] }]\n";
        let spans = spans_of(toml);

        assert!(spans.group_names.contains_key(&0));
        assert!(spans.dependencies.contains_key(&(0, 0)));
    }

    #[test]
    fn test_span_byte_to_line_column() {
        let source = "line1\nname = \"test\"\nline3";
        // "name" starts at byte 6 (after "line1\n")
        let span = Span::from_byte_offset(source, 6);
        assert_eq!(span.line, 2);
        assert_eq!(span.column, 1);
    }

    #[test]
    fn test_key_span_follows_nested_tables() {
        let toml = "[[group]]\nname = \"a\"\n\n[[group.task]]\nname = \"t\"\ncomand = \"x\"\n";
        let doc = ImDocument::parse(toml).unwrap();
        let path = [
            PathSeg::Key("group".into()),
            PathSeg::Index(0),
            PathSeg::Key("task".into()),
            PathSeg::Index(0),
            PathSeg::Key("comand".into()),
        ];

        let range = key_span(&doc, &path).unwrap();
        let span = Span::from_byte_offset(toml, range.start);
        assert_eq!((span.line, span.column), (6, 1));
    }
}
