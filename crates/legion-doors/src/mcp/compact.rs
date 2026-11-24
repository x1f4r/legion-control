//! Compact mode: three tools that list, describe and call the others, for agents that should not
//! carry every tool's schema in their context.

use legion_core::proto::{ErrorBody, Target};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::catalog::on_schema;

/// Lists the tools.
pub(crate) const TOOLS: &str = "legion_tools";
/// Describes one tool.
pub(crate) const DESCRIBE: &str = "legion_describe";
/// Calls one tool.
pub(crate) const CALL: &str = "legion_call";

/// The three tools, as `tools/list` shows them.
pub(crate) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            TOOLS,
            "List the Legion tools you may use on this machine: each one's name, effect and what it does. Give family, such as fs or meta, to list only that family.",
            schema(json!({
                "family": {
                    "type": "string",
                    "description": "Only the tools of this family, such as fs or meta."
                }
            }), &[]),
        )
        .with_title("List Legion tools")
        .with_annotations(ToolAnnotations::with_title("List Legion tools").read_only(true)),
        Tool::new(
            DESCRIBE,
            "Describe one Legion tool: what it does, its effect, and the JSON Schema of the arguments legion_call takes for it.",
            schema(json!({
                "tool": {
                    "type": "string",
                    "description": "The tool's name, as legion_tools lists it, such as meta.whoami."
                }
            }), &["tool"]),
        )
        .with_title("Describe a Legion tool")
        .with_annotations(ToolAnnotations::with_title("Describe a Legion tool").read_only(true)),
        Tool::new(
            CALL,
            "Run one Legion tool with its arguments, here or on the machines on names. Returns the tool's JSON result, or an error object whose code says what went wrong and whose hint says what to do.",
            schema(json!({
                "tool": {
                    "type": "string",
                    "description": "The tool's name, as legion_tools lists it, such as meta.whoami."
                },
                "args": {
                    "type": "object",
                    "description": "The tool's arguments, as legion_describe shows them. Leave it out for a tool that takes none."
                },
                "on": on_schema()
            }), &["tool"]),
        )
        .with_title("Run a Legion tool")
        // It may run any tool, including the ones that remove things or restart the machine.
        .with_annotations(
            ToolAnnotations::with_title("Run a Legion tool")
                .read_only(false)
                .destructive(true),
        ),
    ]
}

fn schema(properties: Value, required: &[&str]) -> JsonObject {
    let mut schema = Map::new();
    schema.insert("type".to_owned(), json!("object"));
    schema.insert("properties".to_owned(), properties);
    if !required.is_empty() {
        schema.insert("required".to_owned(), json!(required));
    }
    schema.insert("additionalProperties".to_owned(), json!(false));
    schema
}

/// `legion_tools` arguments.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolsArgs {
    /// Only this family.
    #[serde(default)]
    pub(crate) family: Option<String>,
}

/// `legion_describe` arguments.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DescribeArgs {
    /// The tool.
    pub(crate) tool: String,
}

/// `legion_call` arguments.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallArgs {
    /// The tool.
    pub(crate) tool: String,
    /// Its arguments.
    #[serde(default)]
    pub(crate) args: Option<Map<String, Value>>,
    /// Where to run it.
    #[serde(default)]
    pub(crate) on: Option<Target>,
}

/// Reads a compact tool's arguments, saying in an error how to find out what it takes.
pub(crate) fn parse<A: serde::de::DeserializeOwned>(
    tool: &str,
    args: JsonObject,
) -> Result<A, ErrorBody> {
    legion_core::parse_args(tool, Value::Object(args)).map_err(|error| ErrorBody {
        hint: Some(format!(
            "tools/list shows the arguments {tool} takes in its input schema."
        )),
        ..error
    })
}

/// The family a `family` argument names: `fs`, whether written `fs`, `fs.` or `fs.*`.
pub(crate) fn family(given: &str) -> &str {
    given.trim().trim_end_matches(".*").trim_end_matches('.')
}

/// Whether a tool belongs to a family.
pub(crate) fn in_family(tool: &str, family: &str) -> bool {
    tool.strip_prefix(family)
        .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use legion_core::proto::ErrorCode;

    use super::*;

    #[test]
    fn three_tools_with_closed_object_schemas() {
        let tools = tools();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, [TOOLS, DESCRIBE, CALL]);
        for tool in &tools {
            assert_eq!(tool.input_schema["type"], json!("object"));
            assert_eq!(tool.input_schema["additionalProperties"], json!(false));
        }
        assert_eq!(tools[2].input_schema["required"], json!(["tool"]));
        assert_eq!(tools[2].input_schema["properties"]["on"], on_schema());
    }

    #[test]
    fn arguments_are_checked() {
        let call: CallArgs = parse(
            CALL,
            json!({"tool": "meta.version", "on": ["tower", "pi"]})
                .as_object()
                .cloned()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(call.tool, "meta.version");
        assert_eq!(
            call.on,
            Some(Target::Many(vec!["tower".into(), "pi".into()]))
        );
        assert!(call.args.is_none());
        let error = parse::<CallArgs>(CALL, Map::new()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs);
        assert!(error.hint.unwrap().contains("tools/list"));
        let error = parse::<DescribeArgs>(
            DESCRIBE,
            json!({"tool": "a.b", "x": 1}).as_object().cloned().unwrap(),
        )
        .unwrap_err();
        assert!(error.message.contains("`x`"), "{}", error.message);
    }

    #[test]
    fn families_match_whole_parts() {
        assert_eq!(family("fs"), "fs");
        assert_eq!(family("fs."), "fs");
        assert_eq!(family(" fs.* "), "fs");
        assert!(in_family("fs.read", "fs"));
        assert!(!in_family("fsx.read", "fs"));
        assert!(!in_family("fs", "fs"));
        assert!(in_family("a.b.c", "a.b"));
    }
}
