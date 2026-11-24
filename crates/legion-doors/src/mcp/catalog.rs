//! The registry as MCP clients see it: names without dots, schemas that carry `on`, effects as
//! annotations.

use std::collections::BTreeMap;

use legion_core::proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};

/// The argument every tool gains, naming where to run it.
pub(crate) const ON: &str = "on";

/// The registry's tools, each with the MCP tool that serves it, sorted by registry name.
pub(crate) struct Catalog {
    entries: Vec<Entry>,
    /// MCP name to the registry names that would be served under it. More than one is a collision.
    aliases: BTreeMap<String, Vec<String>>,
}

/// One registry tool.
pub(crate) struct Entry {
    /// The tool as the node knows it.
    pub(crate) spec: ToolSpec,
    /// The tool as `tools/list` shows it.
    pub(crate) tool: Tool,
}

impl Catalog {
    /// The catalog of these tools, or why they cannot be served: two registry names that become
    /// the same MCP name, or a tool that takes an argument called `on`.
    pub(crate) fn new(specs: Vec<ToolSpec>) -> Result<Catalog, ErrorBody> {
        let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for spec in &specs {
            aliases
                .entry(mcp_name(&spec.name))
                .or_default()
                .push(spec.name.clone());
        }
        let mut entries = Vec::with_capacity(specs.len());
        for spec in specs {
            let taken = aliases
                .get(&mcp_name(&spec.name))
                .map_or(&[][..], Vec::as_slice);
            if let [first, second, ..] = taken {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "{first} and {second} would both be served over MCP as {}, so neither could be called.",
                        mcp_name(first)
                    ),
                )
                .with_hint("Rename one of the tools."));
            }
            let tool = mcp_tool(&spec)?;
            entries.push(Entry { spec, tool });
        }
        entries.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        Ok(Catalog { entries, aliases })
    }

    /// Every tool, sorted by registry name.
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The registry name a client means: the name itself, or the MCP name of exactly one tool.
    /// Agents switch between `fs.read` and `fs_read` freely, and both are unambiguous.
    pub(crate) fn resolve<'a>(&'a self, name: &'a str) -> &'a str {
        if self.entries.iter().any(|entry| entry.spec.name == name) {
            return name;
        }
        match self.aliases.get(name).map(Vec::as_slice) {
            Some([only]) => only,
            _ => name,
        }
    }
}

/// The MCP name of a registry tool: `fs.read` is `fs_read`, since many clients and model APIs
/// refuse a dot in a tool's name.
pub(crate) fn mcp_name(tool: &str) -> String {
    tool.replace('.', "_")
}

/// The MCP tool serving a registry tool: its name without dots, its own description, its schema
/// with `on` added, and its effect as annotations.
pub(crate) fn mcp_tool(spec: &ToolSpec) -> Result<Tool, ErrorBody> {
    let schema = schema_with_on(spec)?;
    Ok(
        Tool::new(mcp_name(&spec.name), spec.description.clone(), schema)
            .with_title(spec.name.clone())
            .with_annotations(annotations(&spec.name, spec.effect)),
    )
}

/// What a tool's effect tells a client. `read` tools change nothing; `destroy` and `power` can
/// lose something that cannot be had back. The other effects change things without destroying
/// them, so they claim neither.
pub(crate) fn annotations(title: &str, effect: Effect) -> ToolAnnotations {
    let annotations = ToolAnnotations::with_title(title);
    match effect {
        Effect::Read => annotations.read_only(true),
        Effect::Destroy | Effect::Power => annotations.read_only(false).destructive(true),
        Effect::Write | Effect::Exec | Effect::Admin => {
            annotations.read_only(false).destructive(false)
        }
    }
}

/// A tool's input schema as strict clients accept it: a JSON object of `type: "object"` and
/// without `$schema`, whose dialect some clients refuse to guess at. A tool whose schema is not an
/// object schema is served with an open one, since the node checks the arguments anyway and the
/// tool stays callable.
fn object_schema(schema: &Value) -> JsonObject {
    let mut schema = match schema {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    schema.remove("$schema");
    schema.insert("type".to_owned(), json!("object"));
    schema
}

/// [`object_schema`], with the optional `on` added: at the top, and in each branch of a top-level
/// `anyOf`, `oneOf` or `allOf` that lists its properties, since a branch that closes its
/// properties would otherwise refuse a call that names a machine.
fn schema_with_on(spec: &ToolSpec) -> Result<JsonObject, ErrorBody> {
    let mut schema = object_schema(&spec.input_schema);
    add_on(&mut schema, spec)?;
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = schema.get_mut(combinator) {
            for branch in branches {
                if let Value::Object(branch) = branch
                    && branch.contains_key("properties")
                {
                    add_on(branch, spec)?;
                }
            }
        }
    }
    Ok(schema)
}

fn add_on(schema: &mut JsonObject, spec: &ToolSpec) -> Result<(), ErrorBody> {
    let properties = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if !properties.is_object() {
        *properties = Value::Object(Map::new());
    }
    let Value::Object(properties) = properties else {
        return Ok(());
    };
    if properties.contains_key(ON) {
        return Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "{} takes an argument called on, which the MCP door uses for the machine a call runs on.",
                spec.name
            ),
        )
        .with_hint("Rename the tool's argument."));
    }
    properties.insert(ON.to_owned(), on_schema());
    Ok(())
}

/// The schema of `on`.
pub(crate) fn on_schema() -> Value {
    json!({
        "description": "Where to run: a machine's name, a list of machine names, or \"all\". Leave it out to run on this machine.",
        "anyOf": [
            {"type": "string"},
            {"type": "array", "items": {"type": "string"}, "minItems": 1}
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, effect: Effect, schema: Value) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: "Does it. Then more.".to_owned(),
            effect,
            input_schema: schema,
        }
    }

    fn closed() -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn a_tool_is_served_without_dots_with_on_and_its_effect() {
        let tool = mcp_tool(&spec("fs.read", Effect::Read, closed())).unwrap();
        assert_eq!(tool.name, "fs_read");
        assert_eq!(tool.title.as_deref(), Some("fs.read"));
        assert_eq!(tool.description.as_deref(), Some("Does it. Then more."));
        let schema = Value::Object((*tool.input_schema).clone());
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["required"], json!(["path"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["on"], on_schema());
        let annotations = tool.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, None);
    }

    #[test]
    fn effects_become_annotations() {
        let hints = |effect| {
            let a = annotations("x.y", effect);
            (a.read_only_hint, a.destructive_hint)
        };
        assert_eq!(hints(Effect::Read), (Some(true), None));
        assert_eq!(hints(Effect::Destroy), (Some(false), Some(true)));
        assert_eq!(hints(Effect::Power), (Some(false), Some(true)));
        for effect in [Effect::Write, Effect::Exec, Effect::Admin] {
            assert_eq!(hints(effect), (Some(false), Some(false)), "{effect}");
        }
    }

    #[test]
    fn schemas_are_made_plain_objects() {
        let odd = mcp_tool(
            &spec(
                "x.odd",
                Effect::Read,
                json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": ["object", "null"]}),
            ),
        )
        .unwrap();
        assert_eq!(odd.input_schema.get("$schema"), None);
        assert_eq!(odd.input_schema["type"], json!("object"));
        let broken = mcp_tool(&spec("x.broken", Effect::Read, json!("string"))).unwrap();
        assert_eq!(broken.input_schema["type"], json!("object"));
        assert!(broken.input_schema["properties"]["on"].is_object());
    }

    #[test]
    fn on_reaches_branches_that_close_their_properties() {
        let schema = json!({
            "type": "object",
            "oneOf": [
                {"properties": {"argv": {"type": "array"}}, "required": ["argv"], "additionalProperties": false},
                {"properties": {"cmd": {"type": "string"}}, "required": ["cmd"], "additionalProperties": false},
                {"required": ["x"]}
            ]
        });
        let tool = mcp_tool(&spec("exec.run", Effect::Exec, schema)).unwrap();
        let branches = tool.input_schema["oneOf"].as_array().unwrap();
        assert!(branches[0]["properties"]["on"].is_object());
        assert!(branches[1]["properties"]["on"].is_object());
        assert_eq!(branches[2].get("properties"), None);
    }

    #[test]
    fn a_tool_with_its_own_on_cannot_be_served() {
        let schema = json!({"type": "object", "properties": {"on": {"type": "boolean"}}});
        let tools = vec![spec("x.switch", Effect::Write, schema)];
        let error = Catalog::new(tools).err().unwrap();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(error.message.contains("x.switch"), "{}", error.message);
    }

    #[test]
    fn names_that_would_collide_are_refused() {
        let tools = vec![
            spec("a.b_c", Effect::Read, closed()),
            spec("a_b.c", Effect::Read, closed()),
            spec("fs.read", Effect::Read, closed()),
        ];
        let error = Catalog::new(tools).err().unwrap();
        assert!(
            error.message.contains("a.b_c and a_b.c"),
            "{}",
            error.message
        );
        assert!(error.message.contains("a_b_c"), "{}", error.message);
    }

    #[test]
    fn a_tool_is_found_by_either_name() {
        let catalog = Catalog::new(vec![spec("fs.read", Effect::Read, closed())]).unwrap();
        assert_eq!(catalog.resolve("fs_read"), "fs.read");
        assert_eq!(catalog.resolve("fs.read"), "fs.read");
        assert_eq!(catalog.resolve("nope"), "nope");
    }

    /// Every `$ref` in `schema`, with where it was found.
    fn refs<'a>(schema: &'a Value, at: String, found: &mut Vec<(String, &'a str)>) {
        match schema {
            Value::Object(map) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        ("$ref", Value::String(target)) => found.push((at.clone(), target)),
                        _ => refs(value, format!("{at}/{key}"), found),
                    }
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    refs(item, format!("{at}/{index}"), found);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn every_registry_tool_is_served_with_a_plain_object_schema_taking_on() {
        let registry = legion_tools::registry();
        let catalog = Catalog::new(registry.specs()).unwrap();
        assert_eq!(catalog.entries().len(), registry.names().count());
        for entry in catalog.entries() {
            let name = &entry.spec.name;
            let tool = &entry.tool;
            assert_eq!(tool.name, mcp_name(name));
            assert!(
                tool.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && tool.name.len() <= 64,
                "{name}: {} is not a name every client accepts",
                tool.name
            );
            assert!(
                !tool.description.as_deref().unwrap_or_default().is_empty(),
                "{name}"
            );
            let schema = Value::Object((*tool.input_schema).clone());
            assert_eq!(schema["type"], json!("object"), "{name}: {schema}");
            assert_eq!(schema.get("$schema"), None, "{name}");
            let properties = schema["properties"].as_object().unwrap();
            assert_eq!(properties[ON], on_schema(), "{name}");
            if let Some(required) = schema.get("required") {
                let required = required.as_array().unwrap();
                assert!(!required.contains(&json!(ON)), "{name}");
                assert!(
                    required
                        .iter()
                        .all(|field| properties.contains_key(field.as_str().unwrap())),
                    "{name}: requires a field it does not describe"
                );
            }
            let mut found = Vec::new();
            refs(&schema, String::new(), &mut found);
            for (at, target) in found {
                let pointer = target.strip_prefix('#').unwrap_or_else(|| {
                    panic!("{name}: {at} refers outside the schema, to {target}")
                });
                assert!(
                    schema.pointer(pointer).is_some(),
                    "{name}: {at} refers to {target}, which is not there"
                );
            }
            assert!(tool.annotations.is_some(), "{name}");
        }
    }
}
