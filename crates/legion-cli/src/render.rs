//! What a person sees on a terminal. Anything that reads the output reads JSON instead, so nothing
//! here is a format to keep stable.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use legion_core::Event;
use legion_core::proto::{ErrorBody, EventKind, ToolSpec};
use serde_json::Value;

/// A tool's result as aligned `key  value` lines, nested values indented beneath their key.
pub fn value(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::Object(_) | Value::Array(_) => block(&mut out, value, 0),
        scalar => {
            out.push_str(&scalar_text(scalar));
            out.push('\n');
        }
    }
    out
}

fn block(out: &mut String, value: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            let width = map
                .iter()
                .filter(|(_, value)| inline(value).is_some())
                .map(|(key, _)| key.chars().count())
                .max()
                .unwrap_or(0);
            for (key, value) in map {
                match inline(value) {
                    Some(text) => {
                        let _ = writeln!(
                            out,
                            "{pad}{key:<width$}  {}",
                            indent_lines(&text, indent + width + 2)
                        );
                    }
                    None => {
                        let _ = writeln!(out, "{pad}{key}");
                        block(out, value, indent + 2);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match inline(item) {
                    Some(text) => {
                        let _ = writeln!(out, "{pad}- {}", indent_lines(&text, indent + 2));
                    }
                    None => {
                        // The item's own block, with its first line marked as the start of an item.
                        let mut nested = String::new();
                        block(&mut nested, item, indent + 2);
                        let marked = nested.replacen(&format!("{pad}  "), &format!("{pad}- "), 1);
                        out.push_str(&marked);
                    }
                }
            }
        }
        scalar => {
            let _ = writeln!(out, "{pad}{}", scalar_text(scalar));
        }
    }
}

/// The value on one line, when it fits on one: a scalar, an empty container, or a short list of
/// scalars.
fn inline(value: &Value) -> Option<String> {
    match value {
        Value::Array(items) if items.is_empty() => Some("-".to_owned()),
        Value::Object(map) if map.is_empty() => Some("-".to_owned()),
        Value::Array(items)
            if items
                .iter()
                .all(|item| !item.is_array() && !item.is_object()) =>
        {
            let joined = items.iter().map(scalar_text).collect::<Vec<_>>().join(", ");
            (joined.chars().count() <= 72 && !joined.contains('\n')).then_some(joined)
        }
        Value::Array(_) | Value::Object(_) => None,
        scalar => Some(scalar_text(scalar)),
    }
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => "-".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Continues a multi-line value under where its first line started.
fn indent_lines(text: &str, indent: usize) -> String {
    text.trim_end_matches('\n')
        .replace('\n', &format!("\n{}", " ".repeat(indent)))
}

/// `legion tools` as a table: name, effect, and the first sentence of the description.
pub fn tools(specs: &[ToolSpec]) -> String {
    let width = specs.iter().map(|spec| spec.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for spec in specs {
        let summary = first_sentence(&spec.description);
        let _ = writeln!(
            out,
            "{:<width$}  {:<7}  {summary}",
            spec.name,
            spec.effect.as_str()
        );
    }
    out
}

fn first_sentence(text: &str) -> &str {
    match text.find(". ") {
        Some(end) => &text[..=end],
        None => text,
    }
}

/// `legion help <tool>`: what it does and the arguments it takes, from its schema.
pub fn help(spec: &ToolSpec) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} ({})\n\n{}\n",
        spec.name, spec.effect, spec.description
    );
    let schema = &spec.input_schema;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let properties = schema.get("properties").and_then(Value::as_object);
    match properties {
        Some(properties) if !properties.is_empty() => {
            out.push_str("Arguments:\n");
            let rows: Vec<[String; 4]> = properties
                .iter()
                .map(|(name, property)| {
                    let need = if required.contains(&name.as_str()) {
                        "required"
                    } else {
                        "optional"
                    };
                    let mut about = property
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(default) =
                        property.get("default").filter(|default| !default.is_null())
                    {
                        let _ = write!(about, " Default: {default}.");
                    }
                    [
                        name.clone(),
                        type_of(property),
                        need.to_owned(),
                        about.trim().to_owned(),
                    ]
                })
                .collect();
            let widths: Vec<usize> = (0..3)
                .map(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0))
                .collect();
            for [name, kind, need, about] in &rows {
                let _ = writeln!(
                    out,
                    "  {name:<0$}  {kind:<1$}  {need:<2$}  {about}",
                    widths[0], widths[1], widths[2]
                );
            }
        }
        _ => out.push_str("Arguments: none.\n"),
    }
    out
}

/// A property's JSON type, as `string`, `integer`, or `string or null`.
fn type_of(property: &Value) -> String {
    match property.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .filter(|kind| *kind != "null")
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "any".to_owned(),
    }
}

/// An error as a person reads it on stderr.
pub fn error(error: &ErrorBody) -> String {
    let mut out = format!("legion: {}: {}\n", error.code, error.message);
    if let Some(hint) = &error.hint {
        let _ = writeln!(out, "  {hint}");
    }
    out
}

/// A fan-out result as one readable block per machine on a terminal.
pub fn fan_out(results: &BTreeMap<String, Result<Value, ErrorBody>>) -> String {
    if results.is_empty() {
        return "No machines in fleet.toml.\n".to_owned();
    }
    let mut out = String::new();
    for (index, (machine, outcome)) in results.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "{machine}");
        match outcome {
            Ok(val) => match val {
                Value::Object(_) | Value::Array(_) => block(&mut out, val, 2),
                scalar => {
                    let _ = writeln!(out, "  {}", scalar_text(scalar));
                }
            },
            Err(err) => {
                let _ = writeln!(out, "  error ({}): {}", err.code, err.message);
                if let Some(hint) = &err.hint {
                    let _ = writeln!(out, "    {hint}");
                }
            }
        }
    }
    out
}

/// Formats one streamed event for a terminal stderr, prefixing each line with the machine name
/// when more than one machine is involved.
pub fn event(event: &Event, prefix_machine: bool) -> String {
    let text = match (event.kind, &event.data) {
        (EventKind::Stdout | EventKind::Stderr, Value::String(text)) => text.clone(),
        (_, Value::String(text)) => format!("{text}\n"),
        (kind, data) => format!("{}: {data}\n", kind.as_str()),
    };
    if prefix_machine && let Some(machine) = event.machine.as_deref() {
        let mut out = String::new();
        for chunk in text.split_inclusive('\n') {
            let _ = write!(out, "[{machine}] {chunk}");
        }
        out
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use legion_core::proto::{Effect, ErrorCode};
    use serde_json::json;

    use super::*;

    #[test]
    fn objects_become_aligned_lines_and_nesting_is_indented() {
        let text = value(&json!({
            "name": "legion",
            "version": "0.1.0",
            "paths": null,
            "tools": ["meta.version", "meta.whoami"],
            "checks": [
                {"name": "state", "ok": true},
                {"name": "policy.toml", "ok": false}
            ],
            "nested": {"a": 1, "deeper": {"b": [1, 2]}},
            "empty": []
        }));
        assert_eq!(
            text,
            "\
name     legion
version  0.1.0
paths    -
tools    meta.version, meta.whoami
checks
  - name  state
    ok    true
  - name  policy.toml
    ok    false
nested
  a  1
  deeper
    b  1, 2
empty    -
"
        );
    }

    #[test]
    fn multi_line_strings_stay_under_their_column() {
        assert_eq!(value(&json!({"log": "one\ntwo"})), "log  one\n     two\n");
    }

    #[test]
    fn tools_and_help_read_from_the_spec() {
        let spec = ToolSpec {
            name: "audit.tail".into(),
            description: "Show the calls. More detail.".into(),
            effect: Effect::Read,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "How many.", "default": 20},
                    "tool": {"type": ["string", "null"], "description": "Which."}
                },
                "required": ["tool"]
            }),
        };
        assert_eq!(
            tools(std::slice::from_ref(&spec)),
            "audit.tail  read     Show the calls.\n"
        );
        let text = help(&spec);
        assert!(
            text.starts_with("audit.tail (read)\n\nShow the calls. More detail.\n\nArguments:\n"),
            "{text}"
        );
        assert!(
            text.contains("  limit  integer  optional  How many. Default: 20.\n"),
            "{text}"
        );
        assert!(
            text.contains("  tool   string   required  Which.\n"),
            "{text}"
        );
        let bare = ToolSpec {
            input_schema: json!({"type": "object"}),
            ..spec
        };
        assert!(help(&bare).ends_with("Arguments: none.\n"));
    }

    #[test]
    fn errors_say_what_and_what_to_do() {
        let body = ErrorBody::new(ErrorCode::Denied, "No.").with_hint("Ask.");
        assert_eq!(error(&body), "legion: denied: No.\n  Ask.\n");
    }

    #[test]
    fn fan_out_renders_one_block_per_machine() {
        let mut results = BTreeMap::new();
        results.insert("pi".to_owned(), Ok(json!({"node": "pi", "ok": true})));
        results.insert(
            "tower".to_owned(),
            Err(ErrorBody::new(ErrorCode::Unreachable, "Asleep.").with_hint("Wake it.")),
        );
        let text = fan_out(&results);
        assert_eq!(
            text,
            "\
pi
  node  pi
  ok    true

tower
  error (unreachable): Asleep.
    Wake it.
"
        );
        assert_eq!(fan_out(&BTreeMap::new()), "No machines in fleet.toml.\n");
    }

    #[test]
    fn fan_out_events_prefix_lines_with_the_machine_name() {
        let ev = Event {
            kind: EventKind::Stdout,
            data: json!("line one\nline two\n"),
            machine: Some("pi".to_owned()),
        };
        assert_eq!(event(&ev, false), "line one\nline two\n");
        assert_eq!(event(&ev, true), "[pi] line one\n[pi] line two\n");

        let progress = Event {
            kind: EventKind::Progress,
            data: json!({"step": 1}),
            machine: Some("tower".to_owned()),
        };
        assert_eq!(event(&progress, true), "[tower] progress: {\"step\":1}\n");
    }
}
