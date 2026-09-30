//! A portable tool input schema: every subschema states its `type`.
//!
//! JSON Schema lets a node omit `type` (`{"enum": ["a", "b"]}` is valid), but
//! strict vendor validators refuse it. Moonshot's chat endpoint answers HTTP 400
//! "tools.function.parameters is not a valid moonshot flavored json schema,
//! details: <At path 'properties.evidence.properties.kind': type is not
//! defined>" before the model runs, so every request that offers such a tool
//! fails in well under a second. A built-in tool is therefore refused at
//! registration when its schema has an untyped node: the defect shows when the
//! tool set is built, not as a failed call on one vendor.
//!
//! A node that composes others (`anyOf`, `oneOf`, `allOf`, `$ref`) takes its
//! type from them and is not required to state one; its members are checked.
//! Every keyword that holds a subschema is walked (JSON Schema 2020-12 and the
//! draft-07 names), and a local `$ref` is followed to its target, which must
//! exist: a `$ref` that resolves to nothing, or to another document, gives the
//! node no type a validator can read, so it is named.

use std::collections::BTreeSet;

use serde_json::Value;

/// Keywords whose value is one subschema.
const ONE_SCHEMA: &[&str] = &[
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "additionalItems",
    "unevaluatedItems",
    "contains",
    "not",
    "if",
    "then",
    "else",
];

/// Keywords whose value is a list of subschemas.
const SCHEMA_LISTS: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];

/// Keywords whose value maps names to subschemas.
const SCHEMA_MAPS: &[&str] = &[
    "properties",
    "patternProperties",
    "dependentSchemas",
    "$defs",
    "definitions",
];

/// The paths of the schema nodes below the root that state no `type`, in
/// Moonshot's dotted form (`properties.evidence.properties.kind`), sorted. The
/// root is checked too when it is not an empty object. Empty means portable.
pub fn untyped_schema_nodes(schema: &Value) -> Vec<String> {
    let root_empty = schema.as_object().is_some_and(serde_json::Map::is_empty);
    if root_empty {
        return Vec::new();
    }
    let mut w = Walker {
        root: schema,
        out: BTreeSet::new(),
        followed: BTreeSet::new(),
    };
    w.walk(schema, "");
    w.out.into_iter().collect()
}

fn join(path: &str, part: &str) -> String {
    if path.is_empty() {
        part.to_owned()
    } else {
        format!("{path}.{part}")
    }
}

/// One JSON Pointer token, unescaped (`~1` is `/`, `~0` is `~`).
fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

struct Walker<'a> {
    root: &'a Value,
    /// The untyped paths found; a set, since a `$ref` target is also reached
    /// where it is defined.
    out: BTreeSet<String>,
    /// Local `$ref` targets already walked, so a cycle ends.
    followed: BTreeSet<String>,
}

impl<'a> Walker<'a> {
    fn untyped(&mut self, path: &str) {
        self.out.insert(if path.is_empty() {
            "(root)".to_owned()
        } else {
            path.to_owned()
        });
    }

    /// Follow a `$ref`: walk a local target once, under its own path. A
    /// reference that resolves to nothing, or to another document, leaves
    /// the referring node untyped.
    fn follow(&mut self, reference: &str, path: &str) {
        let Some(pointer) = reference.strip_prefix('#') else {
            self.untyped(path);
            return;
        };
        let tokens: Vec<String> = pointer.split('/').skip(1).map(unescape).collect();
        let mut target = self.root;
        for t in &tokens {
            let next = match target {
                Value::Object(m) => m.get(t),
                Value::Array(l) => t.parse::<usize>().ok().and_then(|i| l.get(i)),
                _ => None,
            };
            let Some(next) = next else {
                self.untyped(path);
                return;
            };
            target = next;
        }
        if self.followed.insert(pointer.to_owned()) {
            self.walk(target, &tokens.join("."));
        }
    }

    fn walk(&mut self, node: &'a Value, path: &str) {
        let Some(obj) = node.as_object() else {
            return;
        };
        let composed = ["anyOf", "oneOf", "allOf", "$ref"]
            .iter()
            .any(|k| obj.contains_key(*k));
        if !obj.contains_key("type") && !composed {
            self.untyped(path);
        }
        if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
            self.follow(reference, path);
        }
        for key in SCHEMA_MAPS {
            if let Some(map) = obj.get(*key).and_then(Value::as_object) {
                for (name, sub) in map {
                    self.walk(sub, &join(path, &format!("{key}.{name}")));
                }
            }
        }
        for key in ONE_SCHEMA {
            if let Some(sub @ Value::Object(_)) = obj.get(*key) {
                self.walk(sub, &join(path, key));
            }
        }
        match obj.get("items") {
            Some(items @ Value::Object(_)) => self.walk(items, &join(path, "items")),
            Some(Value::Array(list)) => {
                for (i, sub) in list.iter().enumerate() {
                    self.walk(sub, &join(path, &format!("items[{i}]")));
                }
            }
            _ => {}
        }
        for key in SCHEMA_LISTS {
            if let Some(list) = obj.get(*key).and_then(Value::as_array) {
                for (i, sub) in list.iter().enumerate() {
                    self.walk(sub, &join(path, &format!("{key}[{i}]")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_enum_without_a_type_is_named_by_its_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "claim": {"type": "string"},
                "evidence": {
                    "type": "object",
                    "properties": {"kind": {"enum": ["quote", "none"]}}
                },
                "list": {"type": "array", "items": {"description": "no type"}},
                "either": {"anyOf": [{"type": "string"}, {"const": 1}]}
            }
        });
        assert_eq!(
            untyped_schema_nodes(&schema),
            vec![
                "properties.either.anyOf[1]".to_owned(),
                "properties.evidence.properties.kind".to_owned(),
                "properties.list.items".to_owned(),
            ]
        );
    }

    /// Every keyword that holds a subschema is walked, and a local `$ref` is
    /// followed to its target: an untyped node anywhere a validator reads is
    /// named.
    #[test]
    fn every_schema_keyword_and_local_ref_target_is_checked() {
        let schema = json!({
            "type": "object",
            "$defs": {"kind": {"enum": ["a"]}, "ok": {"type": "string"}},
            "definitions": {"old": {"const": 1}},
            "properties": {
                "k": {"$ref": "#/$defs/kind"},
                "o": {"$ref": "#/definitions/old"},
                "fine": {"$ref": "#/$defs/ok"},
                "gone": {"$ref": "#/$defs/missing"},
                "far": {"$ref": "https://example.com/s.json"},
                "p": {
                    "type": "object",
                    "patternProperties": {"^x": {"description": "no type"}},
                    "dependentSchemas": {"a": {"required": ["b"]}},
                    "propertyNames": {"pattern": "^[a-z]+$"},
                    "unevaluatedProperties": {"minLength": 1}
                },
                "n": {"type": "string", "not": {"const": "x"}},
                "c": {
                    "type": "string",
                    "if": {"minLength": 2},
                    "then": {"type": "string"},
                    "else": {"maxLength": 1}
                },
                "arr": {"type": "array", "contains": {"minimum": 1}}
            }
        });
        assert_eq!(
            untyped_schema_nodes(&schema),
            vec![
                "$defs.kind".to_owned(),
                "definitions.old".to_owned(),
                "properties.arr.contains".to_owned(),
                "properties.c.else".to_owned(),
                "properties.c.if".to_owned(),
                "properties.far".to_owned(),
                "properties.gone".to_owned(),
                "properties.n.not".to_owned(),
                "properties.p.dependentSchemas.a".to_owned(),
                "properties.p.patternProperties.^x".to_owned(),
                "properties.p.propertyNames".to_owned(),
                "properties.p.unevaluatedProperties".to_owned(),
            ]
        );
        // A ref cycle ends; a typed target satisfies the ref.
        let cyclic = json!({
            "type": "object",
            "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
            "properties": {"root": {"$ref": "#/$defs/node"}}
        });
        assert!(untyped_schema_nodes(&cyclic).is_empty());
        // A ref into a map key with `/` or `~` is escaped as JSON Pointer says.
        let escaped = json!({
            "type": "object",
            "$defs": {"a/b": {"enum": [1]}},
            "properties": {"x": {"$ref": "#/$defs/a~1b"}}
        });
        assert_eq!(untyped_schema_nodes(&escaped), vec!["$defs.a/b".to_owned()]);
    }

    #[test]
    fn typed_and_empty_schemas_are_portable() {
        assert!(untyped_schema_nodes(&json!({})).is_empty());
        assert!(untyped_schema_nodes(&json!({"type": "object"})).is_empty());
        assert!(
            untyped_schema_nodes(&json!({
                "type": "object",
                "properties": {"k": {"type": "string", "enum": ["a"]}},
                "additionalProperties": false
            }))
            .is_empty()
        );
        assert_eq!(
            untyped_schema_nodes(&json!({"properties": {}})),
            vec!["(root)".to_owned()]
        );
    }
}
