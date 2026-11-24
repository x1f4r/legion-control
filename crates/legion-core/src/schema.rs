//! Tool schemas in the plain JSON Schema every client accepts.
//!
//! Generated schemas spell some things in ways strict clients refuse: an optional field's type as
//! `["string", "null"]`, a documented enum as a `oneOf` of `const`s, a Rust integer's width as a
//! `format` no standard defines. [`normalise`] rewrites those into the plain forms, and
//! [`problems`] says what is left that a strict client would refuse, so a test can hold every tool
//! to it.

use serde_json::{Map, Value};

/// The formats the JSON Schema standard defines. A client may refuse any other.
const STANDARD_FORMATS: [&str; 19] = [
    "date-time",
    "date",
    "time",
    "duration",
    "email",
    "idn-email",
    "hostname",
    "idn-hostname",
    "ipv4",
    "ipv6",
    "uri",
    "uri-reference",
    "iri",
    "iri-reference",
    "uuid",
    "uri-template",
    "json-pointer",
    "relative-json-pointer",
    "regex",
];

/// Keywords whose value is one schema.
const ONE: [&str; 10] = [
    "items",
    "additionalProperties",
    "not",
    "contains",
    "propertyNames",
    "if",
    "then",
    "else",
    "unevaluatedItems",
    "unevaluatedProperties",
];
/// Keywords whose value maps names to schemas.
const MAP: [&str; 5] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];
/// Keywords whose value is a list of schemas.
const LIST: [&str; 4] = ["anyOf", "oneOf", "allOf", "prefixItems"];

/// Rewrites a schema into its plain form:
///
/// - an optional field has its plain type: it is optional by not being `required`, so `null` is
///   dropped from a `type` list, a `{"type": "null"}` branch from `anyOf` and `oneOf`, and the
///   `"default": null` that came with it;
/// - an enum of constants is `{"type": "string", "enum": [...]}`, with each documented value's
///   description added to the field's own, one value a line;
/// - a `format` the standard does not define, such as `uint32` or `double`, is dropped, while
///   `minimum` and `maximum` stay.
///
/// Only what the schema says is changed, never what it accepts beyond `null`: the tool's own
/// parsing still takes an explicit `null` for an optional field.
pub fn normalise(schema: &mut Value) {
    let Value::Object(map) = schema else {
        return;
    };
    drop_null(map);
    fold_constants(map);
    if let Some(Value::String(format)) = map.get("format")
        && !STANDARD_FORMATS.contains(&format.as_str())
    {
        map.remove("format");
    }
    for (key, value) in map.iter_mut() {
        match value {
            Value::Object(children) if MAP.contains(&key.as_str()) => {
                children.values_mut().for_each(normalise);
            }
            Value::Array(children) if LIST.contains(&key.as_str()) => {
                children.iter_mut().for_each(normalise);
            }
            child if ONE.contains(&key.as_str()) => normalise(child),
            _ => {}
        }
    }
}

/// Drops `null` from where a generator put it to say a field is optional, folding a choice of
/// one remaining branch into the schema itself. The field's own description stays; the
/// branch's, which describes its type, is used only when the field has none.
fn drop_null(map: &mut Map<String, Value>) {
    let mut nullable = false;
    if let Some(Value::Array(types)) = map.get_mut("type") {
        let before = types.len();
        types.retain(|kind| kind != "null");
        nullable = types.len() != before;
        if let [only] = types.as_slice() {
            let only = only.clone();
            map.insert("type".to_owned(), only);
        }
    }
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(branches)) = map.get_mut(key) else {
            continue;
        };
        let before = branches.len();
        branches.retain(|branch| branch.get("type").and_then(Value::as_str) != Some("null"));
        if branches.len() == before {
            continue;
        }
        nullable = true;
        if branches.len() == 1
            && let Some(Value::Array(mut branches)) = map.remove(key)
            && let Some(Value::Object(branch)) = branches.pop()
        {
            for (name, value) in branch {
                map.entry(name).or_insert(value);
            }
        }
    }
    if nullable && map.get("default") == Some(&Value::Null) {
        map.remove("default");
    }
}

/// Turns a `oneOf` or `anyOf` of constants of one type, and a lone `const`, into an `enum`.
fn fold_constants(map: &mut Map<String, Value>) {
    for key in ["oneOf", "anyOf"] {
        let Some(Value::Array(branches)) = map.get(key) else {
            continue;
        };
        let Some((kind, values, notes)) = constants(branches) else {
            continue;
        };
        map.remove(key);
        map.insert("type".to_owned(), Value::String(kind.to_owned()));
        map.insert("enum".to_owned(), Value::Array(values));
        if !notes.is_empty() {
            let mut description = map
                .get("description")
                .and_then(Value::as_str)
                .map(|text| format!("{}\n", text.trim_end()))
                .unwrap_or_default();
            description.push_str(&notes.join("\n"));
            map.insert("description".to_owned(), Value::String(description));
        }
    }
    if let Some(value) = map.remove("const") {
        map.insert("type".to_owned(), Value::String(type_of(&value).to_owned()));
        map.insert("enum".to_owned(), Value::Array(vec![value]));
    }
}

/// The values of a choice that is only constants of one JSON type, with that type and a line for
/// each value that has a description; `None` for any other choice.
fn constants(branches: &[Value]) -> Option<(&'static str, Vec<Value>, Vec<String>)> {
    let mut kind = None;
    let mut values = Vec::with_capacity(branches.len());
    let mut notes = Vec::new();
    for branch in branches {
        let branch = branch.as_object()?;
        let plain = branch
            .keys()
            .all(|key| matches!(key.as_str(), "const" | "type" | "description" | "title"));
        let value = branch.get("const").filter(|_| plain)?;
        let this = type_of(value);
        if kind.is_some_and(|kind| kind != this) {
            return None;
        }
        kind = Some(this);
        if let Some(description) = branch.get("description").and_then(Value::as_str) {
            let shown = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned);
            notes.push(format!("`{shown}`: {}", description.trim()));
        }
        values.push(value.clone());
    }
    Some((kind?, values, notes))
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// What in a schema a strict client would refuse: a `type` list, `oneOf`, `anyOf` or `const`, a
/// `$ref` that does not lead to a definition in the same document, or a `format` the standard
/// does not define. Each problem names where it is, as a JSON Pointer.
pub fn problems(schema: &Value) -> Vec<String> {
    let mut found = Vec::new();
    walk(schema, schema, "", &mut found);
    found
}

fn walk(root: &Value, schema: &Value, at: &str, found: &mut Vec<String>) {
    let Value::Object(map) = schema else {
        return;
    };
    let here = if at.is_empty() { "/" } else { at };
    if map.get("type").is_some_and(Value::is_array) {
        found.push(format!("{here}: `type` is a list"));
    }
    for keyword in ["oneOf", "anyOf", "const"] {
        if map.contains_key(keyword) {
            found.push(format!("{here}: uses `{keyword}`"));
        }
    }
    if let Some(Value::String(format)) = map.get("format")
        && !STANDARD_FORMATS.contains(&format.as_str())
    {
        found.push(format!(
            "{here}: `format` {format:?} is not a standard format"
        ));
    }
    if let Some(reference) = map.get("$ref") {
        let resolves = reference
            .as_str()
            .and_then(|reference| reference.strip_prefix('#'))
            .is_some_and(|pointer| root.pointer(pointer).is_some());
        if !resolves {
            found.push(format!(
                "{here}: `$ref` {reference} leads nowhere in this schema"
            ));
        }
    }
    for (key, value) in map {
        let key_at = format!("{at}/{}", escape(key));
        match value {
            Value::Object(children) if MAP.contains(&key.as_str()) => {
                for (name, child) in children {
                    walk(root, child, &format!("{key_at}/{}", escape(name)), found);
                }
            }
            Value::Array(children) if LIST.contains(&key.as_str()) => {
                for (index, child) in children.iter().enumerate() {
                    walk(root, child, &format!("{key_at}/{index}"), found);
                }
            }
            child if ONE.contains(&key.as_str()) => walk(root, child, &key_at, found),
            _ => {}
        }
    }
}

/// A key as one step of a JSON Pointer.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn normalised(mut schema: Value) -> Value {
        normalise(&mut schema);
        schema
    }

    #[test]
    fn an_optional_field_has_its_plain_type() {
        assert_eq!(
            normalised(json!({"type": ["string", "null"], "description": "Who.", "default": null})),
            json!({"type": "string", "description": "Who."})
        );
        assert_eq!(
            normalised(json!({
                "type": "object",
                "properties": {"tags": {"type": ["array", "null"], "items": {"type": ["integer", "null"]}}}
            })),
            json!({
                "type": "object",
                "properties": {"tags": {"type": "array", "items": {"type": "integer"}}}
            })
        );
        // A default that is not null is a real default and stays.
        assert_eq!(
            normalised(json!({"type": ["integer", "null"], "default": 3})),
            json!({"type": "integer", "default": 3})
        );
    }

    #[test]
    fn a_documented_enum_becomes_an_enum_with_its_values_described() {
        let variants = json!([
            {"type": "string", "const": "utf8", "description": "UTF-8 text."},
            {"type": "string", "const": "base64", "description": "Base64 bytes."}
        ]);
        let expected = json!({
            "type": "string",
            "enum": ["utf8", "base64"],
            "description": "How to encode.\n`utf8`: UTF-8 text.\n`base64`: Base64 bytes."
        });
        assert_eq!(
            normalised(json!({"oneOf": variants, "description": "How to encode."})),
            expected
        );
        // Optional, as a generator writes it: the field's description wins over the type's.
        assert_eq!(
            normalised(json!({
                "anyOf": [
                    {"oneOf": variants, "description": "An encoding."},
                    {"type": "null"}
                ],
                "description": "How to encode.",
                "default": null
            })),
            expected
        );
        assert_eq!(
            normalised(json!({"const": "only"})),
            json!({"type": "string", "enum": ["only"]})
        );
        // A choice between real schemas is left for the test to catch.
        let choice = json!({"oneOf": [{"type": "string"}, {"type": "object"}]});
        assert_eq!(normalised(choice.clone()), choice);
    }

    #[test]
    fn non_standard_formats_go_and_bounds_stay() {
        assert_eq!(
            normalised(json!({"type": "integer", "format": "uint32", "minimum": 0, "maximum": 9})),
            json!({"type": "integer", "minimum": 0, "maximum": 9})
        );
        assert_eq!(
            normalised(json!({"type": "string", "format": "date-time"})),
            json!({"type": "string", "format": "date-time"})
        );
    }

    #[test]
    fn data_is_not_mistaken_for_a_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "object", "default": {"type": ["a", "null"], "format": "q"}}}
        });
        assert_eq!(normalised(schema.clone()), schema);
    }

    #[test]
    fn problems_name_where_they_are() {
        let schema = json!({
            "type": "object",
            "$defs": {"Name": {"type": "string"}},
            "properties": {
                "a": {"type": ["string", "null"]},
                "b": {"oneOf": [{"const": 1}]},
                "c": {"type": "integer", "format": "uint64"},
                "d": {"$ref": "#/$defs/Name"},
                "e": {"$ref": "#/$defs/Missing"},
                "f/g": {"type": "array", "items": {"anyOf": []}}
            }
        });
        assert_eq!(
            problems(&schema),
            [
                "/properties/a: `type` is a list",
                "/properties/b: uses `oneOf`",
                "/properties/b/oneOf/0: uses `const`",
                "/properties/c: `format` \"uint64\" is not a standard format",
                "/properties/e: `$ref` \"#/$defs/Missing\" leads nowhere in this schema",
                "/properties/f~1g/items: uses `anyOf`",
            ]
        );
        assert_eq!(problems(&json!({"type": "object"})), Vec::<String>::new());
    }
}
