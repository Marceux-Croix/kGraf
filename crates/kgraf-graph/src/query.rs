//! Structural queries.
//!
//! The acceptance criterion asks for querying the graph **by structural
//! pattern, not just text**. The distinction is whether a query can constrain
//! the *shape* around a node: a text search finds `parse`, a structural query
//! finds functions that call something named `parse` and live in a Rust file.
//!
//! The syntax is one node predicate, optionally followed by an edge hop and a
//! second node predicate:
//!
//! ```text
//! kind=function
//! kind=function name~^handle
//! kind=function -calls-> name=parse
//! kind=method <-contains- kind=class name=Store
//! path~^crates/ kind=struct
//! ```
//!
//! Predicate terms are `kind=`, `name=`, `name~` (regex-free prefix/substring
//! match), `path=`, `path~`, and `qualified~`. Terms conjoin. An unparseable
//! query raises rather than degrading into a substring search over everything,
//! because a query that silently means something else is the fail-loud rule's failure in
//! query form.

use crate::error::{GraphError, Result};
use crate::store::{EdgeKind, GraphStore, NodeRecord};
use rusqlite::ToSql;

/// Which way an edge points, relative to the node being matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeDirection {
    /// `-kind->`: the matched node is the edge's source.
    Outgoing,
    /// `<-kind-`: the matched node is the edge's destination.
    Incoming,
}

/// Constraints on a single node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodePredicate {
    pub kind: Option<String>,
    pub name_exact: Option<String>,
    pub name_contains: Option<String>,
    pub path_exact: Option<String>,
    pub path_contains: Option<String>,
    pub qualified_contains: Option<String>,
}

impl NodePredicate {
    /// Whether this predicate constrains nothing, which would match the whole
    /// graph.
    pub fn is_empty(&self) -> bool {
        self == &NodePredicate::default()
    }

    /// Parse a whitespace-separated list of terms.
    fn parse(text: &str, whole: &str) -> Result<Self> {
        let mut predicate = NodePredicate::default();
        for term in text.split_whitespace() {
            let bad = |reason: &str| GraphError::BadQuery {
                query: whole.to_owned(),
                reason: format!("{reason} in term `{term}`"),
            };
            let (field, value) = if let Some((f, v)) = term.split_once('~') {
                (f, Matcher::Contains(v))
            } else if let Some((f, v)) = term.split_once('=') {
                (f, Matcher::Exact(v))
            } else {
                return Err(bad("expected `field=value` or `field~substring`"));
            };
            if value.as_str().is_empty() {
                return Err(bad("empty value"));
            }
            match (field, value) {
                ("kind", Matcher::Exact(v)) => {
                    if crate::chunk::NodeKind::parse(v).is_none() {
                        return Err(GraphError::BadQuery {
                            query: whole.to_owned(),
                            reason: format!("`{v}` is not a node kind"),
                        });
                    }
                    predicate.kind = Some(v.to_owned());
                }
                ("name", Matcher::Exact(v)) => predicate.name_exact = Some(v.to_owned()),
                ("name", Matcher::Contains(v)) => predicate.name_contains = Some(v.to_owned()),
                ("path", Matcher::Exact(v)) => predicate.path_exact = Some(v.to_owned()),
                ("path", Matcher::Contains(v)) => predicate.path_contains = Some(v.to_owned()),
                ("qualified", Matcher::Contains(v)) => {
                    predicate.qualified_contains = Some(v.to_owned())
                }
                ("qualified", Matcher::Exact(v)) => {
                    predicate.qualified_contains = Some(v.to_owned())
                }
                (other, _) => {
                    return Err(GraphError::BadQuery {
                        query: whole.to_owned(),
                        reason: format!(
                            "unknown field `{other}`; expected kind, name, path or qualified"
                        ),
                    });
                }
            }
        }
        Ok(predicate)
    }

    /// SQL fragment and bound values for this predicate, over alias `n` and its
    /// joined file alias `f`.
    fn to_sql(&self, alias: &str, file_alias: &str) -> (String, Vec<Box<dyn ToSql>>) {
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        let mut push = |clause: String, value: String| {
            clauses.push(clause);
            values.push(Box::new(value) as Box<dyn ToSql>);
        };
        if let Some(kind) = &self.kind {
            push(format!("{alias}.kind = ?"), kind.clone());
        }
        if let Some(name) = &self.name_exact {
            push(format!("{alias}.name = ?"), name.clone());
        }
        if let Some(name) = &self.name_contains {
            push(format!("{alias}.name LIKE ? ESCAPE '\\'"), like_for(name));
        }
        if let Some(path) = &self.path_exact {
            push(format!("{file_alias}.path = ?"), path.clone());
        }
        if let Some(path) = &self.path_contains {
            push(
                format!("{file_alias}.path LIKE ? ESCAPE '\\'"),
                like_for(path),
            );
        }
        if let Some(qualified) = &self.qualified_contains {
            push(
                format!("{alias}.qualified_name LIKE ? ESCAPE '\\'"),
                like_for(qualified),
            );
        }
        let sql = if clauses.is_empty() {
            "1".to_owned()
        } else {
            clauses.join(" AND ")
        };
        (sql, values)
    }
}

enum Matcher<'a> {
    Exact(&'a str),
    Contains(&'a str),
}

impl Matcher<'_> {
    fn as_str(&self) -> &str {
        match self {
            Matcher::Exact(v) | Matcher::Contains(v) => v,
        }
    }
}

/// `^prefix` anchors, otherwise the term is a substring. `%` and `_` in the term
/// are escaped so a user's underscore does not silently become a wildcard.
fn like_for(term: &str) -> String {
    let escaped = term
        .trim_start_matches('^')
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    if term.starts_with('^') {
        format!("{escaped}%")
    } else {
        format!("%{escaped}%")
    }
}

/// A parsed structural query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// Constraints on the node the query returns.
    pub subject: NodePredicate,
    /// An optional hop: the edge kind, its direction, and the node at the far end.
    pub hop: Option<(EdgeKind, EdgeDirection, NodePredicate)>,
}

/// One match: the subject, and the far-end node where the pattern had a hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryMatch {
    pub node: NodeRecord,
    pub via: Option<NodeRecord>,
}

impl Pattern {
    /// Parse a query string.
    pub fn parse(query: &str) -> Result<Self> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(GraphError::BadQuery {
                query: query.to_owned(),
                reason: "empty query".to_owned(),
            });
        }
        // Find a hop operator, if any. Outgoing is `-kind->`, incoming `<-kind-`.
        if let Some(start) = trimmed.find("<-") {
            let rest = &trimmed[start + 2..];
            let end = rest.find('-').ok_or_else(|| GraphError::BadQuery {
                query: query.to_owned(),
                reason: "unterminated `<-kind-` hop".to_owned(),
            })?;
            let kind = EdgeKind::parse(rest[..end].trim()).ok_or_else(|| GraphError::BadQuery {
                query: query.to_owned(),
                reason: format!("`{}` is not an edge kind", rest[..end].trim()),
            })?;
            return Ok(Pattern {
                subject: NodePredicate::parse(&trimmed[..start], query)?,
                hop: Some((
                    kind,
                    EdgeDirection::Incoming,
                    NodePredicate::parse(&rest[end + 1..], query)?,
                )),
            });
        }
        if let Some(start) = trimmed.find('-')
            && let Some(arrow) = trimmed[start..].find("->")
        {
            let kind_text = trimmed[start + 1..start + arrow].trim();
            let kind = EdgeKind::parse(kind_text).ok_or_else(|| GraphError::BadQuery {
                query: query.to_owned(),
                reason: format!("`{kind_text}` is not an edge kind"),
            })?;
            return Ok(Pattern {
                subject: NodePredicate::parse(&trimmed[..start], query)?,
                hop: Some((
                    kind,
                    EdgeDirection::Outgoing,
                    NodePredicate::parse(&trimmed[start + arrow + 2..], query)?,
                )),
            });
        }
        let subject = NodePredicate::parse(trimmed, query)?;
        if subject.is_empty() {
            return Err(GraphError::BadQuery {
                query: query.to_owned(),
                reason: "a query with no constraints would match the whole graph".to_owned(),
            });
        }
        Ok(Pattern { subject, hop: None })
    }

    /// Run the pattern against a store.
    pub fn run(&self, store: &GraphStore, limit: usize) -> Result<Vec<QueryMatch>> {
        const NODE_COLUMNS: &str =
            "n.id, n.file_id, f.path, n.kind, n.name, n.qualified_name, n.parent_id,
             n.start_line, n.end_line, n.start_byte, n.end_byte, n.signature,
             n.identity_stable";
        const VIA_COLUMNS: &str =
            "m.id, m.file_id, g.path, m.kind, m.name, m.qualified_name, m.parent_id,
             m.start_line, m.end_line, m.start_byte, m.end_byte, m.signature,
             m.identity_stable";

        let (subject_sql, subject_values) = self.subject.to_sql("n", "f");
        let mut values = subject_values;
        let sql = match &self.hop {
            None => format!(
                "SELECT {NODE_COLUMNS}, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL
                 FROM nodes n JOIN files f ON f.id = n.file_id
                 WHERE {subject_sql}
                 ORDER BY f.path, n.start_line LIMIT ?",
            ),
            Some((kind, direction, far)) => {
                let (far_sql, far_values) = far.to_sql("m", "g");
                values.push(Box::new(kind.as_str().to_owned()));
                values.extend(far_values);
                let join = match direction {
                    EdgeDirection::Outgoing => "e.src = n.id AND e.dst = m.id",
                    EdgeDirection::Incoming => "e.dst = n.id AND e.src = m.id",
                };
                format!(
                    "SELECT {NODE_COLUMNS}, {VIA_COLUMNS}
                     FROM nodes n
                     JOIN files f ON f.id = n.file_id
                     JOIN edges e ON {join}
                     JOIN nodes m ON m.id = CASE WHEN e.src = n.id THEN e.dst ELSE e.src END
                     JOIN files g ON g.id = m.file_id
                     WHERE {subject_sql} AND e.kind = ? AND {far_sql}
                     ORDER BY f.path, n.start_line LIMIT ?",
                )
            }
        };
        values.push(Box::new(limit as i64));

        let conn = store.conn();
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn ToSql> = values.iter().map(|v| v.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            let node = row_to_node(row, 0)?;
            let via_id: Option<String> = row.get(13)?;
            let via = match via_id {
                Some(_) => Some(row_to_node(row, 13)?),
                None => None,
            };
            Ok(QueryMatch { node, via })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn row_to_node(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<NodeRecord> {
    let kind: String = row.get(base + 3)?;
    Ok(NodeRecord {
        id: row.get(base)?,
        file_id: row.get(base + 1)?,
        path: row.get(base + 2)?,
        kind: crate::chunk::NodeKind::parse(&kind).unwrap_or(crate::chunk::NodeKind::File),
        name: row.get(base + 4)?,
        qualified_name: row.get(base + 5)?,
        parent_id: row.get(base + 6)?,
        start_line: row.get(base + 7)?,
        end_line: row.get(base + 8)?,
        start_byte: row.get::<_, i64>(base + 9)? as u64,
        end_byte: row.get::<_, i64>(base + 10)? as u64,
        signature: row.get(base + 11)?,
        identity_stable: row.get(base + 12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Indexer;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, GraphStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |rel: &str, body: &str| {
            let full = dir.path().join(rel);
            fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            fs::write(full, body).expect("write");
        };
        write("src/parse.rs", "pub fn parse() {}\n");
        write(
            "src/handle.rs",
            "fn handle_one() { parse(); }\nfn handle_two() {}\nfn other() { parse(); }\n",
        );
        write(
            "ui/store.ts",
            "class Store { save(): void {} load(): void {} }\n",
        );
        let store = GraphStore::open_in_memory(dir.path()).expect("store");
        Indexer::new(&store).index().expect("index");
        (dir, store)
    }

    #[test]
    fn a_kind_predicate_matches_across_languages() {
        let (_dir, store) = fixture();
        let matches = Pattern::parse("kind=method")
            .expect("parse")
            .run(&store, 100)
            .expect("run");
        let names: Vec<_> = matches.iter().map(|m| m.node.name.as_str()).collect();
        assert!(names.contains(&"save"), "got {names:?}");
        assert!(names.contains(&"load"), "got {names:?}");
    }

    #[test]
    fn a_prefix_match_is_anchored() {
        let (_dir, store) = fixture();
        let matches = Pattern::parse("kind=function name~^handle")
            .expect("parse")
            .run(&store, 100)
            .expect("run");
        let names: Vec<_> = matches.iter().map(|m| m.node.name.clone()).collect();
        assert_eq!(names.len(), 2, "got {names:?}");
        assert!(names.contains(&"handle_one".to_owned()));
        assert!(!names.contains(&"other".to_owned()));
    }

    #[test]
    fn a_hop_constrains_the_shape_not_just_the_text() {
        // This is the property that makes the query structural: `other` matches
        // the same text predicate as `handle_one` and is excluded only by where
        // its edges go.
        let (_dir, store) = fixture();
        let matches = Pattern::parse("kind=function name~^handle -calls-> name=parse")
            .expect("parse")
            .run(&store, 100)
            .expect("run");
        let names: Vec<_> = matches.iter().map(|m| m.node.name.clone()).collect();
        assert_eq!(names, vec!["handle_one".to_owned()], "got {names:?}");
        assert_eq!(
            matches[0].via.as_ref().expect("via").name,
            "parse",
            "the far end of the hop must be reported"
        );
    }

    #[test]
    fn an_incoming_hop_walks_the_other_way() {
        let (_dir, store) = fixture();
        let matches = Pattern::parse("kind=method <-contains- kind=class name=Store")
            .expect("parse")
            .run(&store, 100)
            .expect("run");
        let names: Vec<_> = matches.iter().map(|m| m.node.name.clone()).collect();
        assert_eq!(names.len(), 2, "got {names:?}");
        assert!(names.contains(&"save".to_owned()));
    }

    #[test]
    fn a_path_predicate_scopes_by_location() {
        let (_dir, store) = fixture();
        let matches = Pattern::parse("path~^ui/ kind=class")
            .expect("parse")
            .run(&store, 100)
            .expect("run");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].node.name, "Store");
    }

    #[test]
    fn an_unparseable_query_raises_rather_than_falling_back_to_text_search() {
        for bad in [
            "",
            "just some words",
            "kind=nonexistent",
            "nosuchfield=x",
            "kind=function -nosuchedge-> name=parse",
            "name=",
        ] {
            let err = Pattern::parse(bad).expect_err("must raise");
            assert!(
                matches!(err, GraphError::BadQuery { .. }),
                "`{bad}` produced {err:?}"
            );
        }
    }

    #[test]
    fn an_unconstrained_query_is_refused() {
        // Matching the whole graph is never what the user meant, and silently
        // returning the first N nodes would look like a working query.
        assert!(Pattern::parse("   ").is_err());
    }

    #[test]
    fn like_wildcards_in_a_users_term_are_escaped() {
        // An underscore is a SQL wildcard and is extremely common in identifiers,
        // so `name~handle_one` must not also match `handleXone`.
        let (_dir, store) = fixture();
        let matches = Pattern::parse("name~handle_one")
            .expect("parse")
            .run(&store, 100)
            .expect("run");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].node.name, "handle_one");
        assert!(like_for("a_b").contains("\\_"));
        assert!(like_for("50%").contains("\\%"));
    }

    #[test]
    fn the_limit_is_honored() {
        let (_dir, store) = fixture();
        let matches = Pattern::parse("kind=function")
            .expect("parse")
            .run(&store, 1)
            .expect("run");
        assert_eq!(matches.len(), 1);
    }
}
