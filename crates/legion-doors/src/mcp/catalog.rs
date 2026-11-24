//! The registry as MCP clients see it: names without dots, schemas that carry `on`, effects as
//! annotations.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use legion_core::proto::{Effect, ErrorBody, ErrorCode, Target, ToolSpec};
use legion_core::{Node, Registry};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};

/// The argument every tool gains in full mode, naming where to run it.
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
    /// The tool as `tools/list` shows it in full mode.
    pub(crate) tool: Tool,
}

impl Catalog {
    /// The catalog of these tools.
    ///
    /// `full` is whether each tool is served as an MCP tool of its own. Only then do two registry
    /// names that become the same MCP name, or a tool that takes an argument called `on`, make the
    /// door unable to serve the registry; compact mode names tools as the registry does and passes
    /// `on` beside the arguments, so neither is a problem there.
    pub(crate) fn new(specs: Vec<ToolSpec>, full: bool) -> Result<Catalog, ErrorBody> {
        let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for spec in &specs {
            aliases
                .entry(mcp_name(&spec.name))
                .or_default()
                .push(spec.name.clone());
        }
        let mut entries = Vec::with_capacity(specs.len());
        for spec in specs {
            if full {
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
                    .with_hint("Rename one of the tools, or serve the registry with --compact, which keeps the registry's names."));
                }
            }
            let tool = mcp_tool(&spec, full)?;
            entries.push(Entry { spec, tool });
        }
        entries.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        Ok(Catalog { entries, aliases })
    }

    /// Builds a catalog when extensions may be reloaded dynamically: built-in tools are kept
    /// first, and any extension that would collide in full mode or declare its own `on` argument
    /// is skipped rather than taking down a running door.
    pub(crate) fn from_specs_lenient(
        builtins: &Registry,
        specs: Vec<ToolSpec>,
        full: bool,
    ) -> Catalog {
        let (builtin_specs, extra_specs): (Vec<ToolSpec>, Vec<ToolSpec>) = specs
            .into_iter()
            .partition(|spec| builtins.get(&spec.name).is_some());
        let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut entries = Vec::with_capacity(builtin_specs.len() + extra_specs.len());
        for spec in builtin_specs {
            let alias = mcp_name(&spec.name);
            if let Ok(tool) = mcp_tool(&spec, full) {
                aliases.entry(alias).or_default().push(spec.name.clone());
                entries.push(Entry { spec, tool });
            }
        }
        for spec in extra_specs {
            let alias = mcp_name(&spec.name);
            if full && aliases.contains_key(&alias) {
                continue;
            }
            if let Ok(tool) = mcp_tool(&spec, full) {
                aliases.entry(alias).or_default().push(spec.name.clone());
                entries.push(Entry { spec, tool });
            }
        }
        entries.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        Catalog { entries, aliases }
    }

    /// Every tool, sorted by registry name.
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The tool a client names, by its registry name or its MCP name.
    pub(crate) fn get(&self, name: &str) -> Option<&Entry> {
        let name = self.resolve(name);
        self.entries.iter().find(|entry| entry.spec.name == name)
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

struct CachedCatalog {
    specs: Vec<ToolSpec>,
    catalog: Arc<Catalog>,
    version: u64,
}

/// Keeps an MCP [`Catalog`] in step with [`Node::specs`] so long-lived MCP sessions pick up
/// extensions added, changed or removed on disk without a restart, and bumps its version whenever
/// the catalog's tool specs actually change.
pub(crate) struct CatalogCache {
    full: bool,
    cached: Mutex<CachedCatalog>,
}

impl CatalogCache {
    /// Validates built-in tools strictly (refusing built-in collisions or `on` conflicts in full
    /// mode) and caches the initial catalog including any current extensions.
    pub(crate) fn new(node: &Node, full: bool) -> Result<Arc<CatalogCache>, ErrorBody> {
        Catalog::new(node.registry().specs(), full)?;
        let specs = node.specs();
        let catalog = Arc::new(Catalog::from_specs_lenient(
            node.registry(),
            specs.clone(),
            full,
        ));
        Ok(Arc::new(CatalogCache {
            full,
            cached: Mutex::new(CachedCatalog {
                specs,
                catalog,
                version: 0,
            }),
        }))
    }

    /// The catalog for `node`'s current specs and its monotonic version, rebuilt only when
    /// `node.specs()` changed.
    pub(crate) fn current_with_version(&self, node: &Node) -> (Arc<Catalog>, u64) {
        let specs = node.specs();
        let mut guard = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.specs == specs {
            return (Arc::clone(&guard.catalog), guard.version);
        }
        let catalog = Arc::new(Catalog::from_specs_lenient(
            node.registry(),
            specs.clone(),
            self.full,
        ));
        guard.specs = specs;
        guard.catalog = Arc::clone(&catalog);
        guard.version = guard.version.saturating_add(1);
        (catalog, guard.version)
    }

    /// The catalog for `node`'s current specs, rebuilt only when `node.specs()` changed.
    pub(crate) fn current(&self, node: &Node) -> Arc<Catalog> {
        self.current_with_version(node).0
    }

    /// The catalog's current version, refreshed against `node.specs()`.
    pub(crate) fn version(&self, node: &Node) -> u64 {
        self.current_with_version(node).1
    }
}

/// The MCP name of a registry tool: `fs.read` is `fs_read`, since many clients and model APIs
/// refuse a dot in a tool's name.
pub(crate) fn mcp_name(tool: &str) -> String {
    tool.replace('.', "_")
}

/// The MCP tool serving a registry tool: its name without dots, its own description, its schema
/// with `on` added when `with_on`, and its effect as annotations.
pub(crate) fn mcp_tool(spec: &ToolSpec, with_on: bool) -> Result<Tool, ErrorBody> {
    let schema = if with_on {
        schema_with_on(spec)?
    } else {
        object_schema(&spec.input_schema)
    };
    Ok(
        Tool::new(mcp_name(&spec.name), spec.description.clone(), schema)
            .with_title(spec.name.clone())
            .with_annotations(annotations(&spec.name, spec.effect)),
    )
}

/// What a tool's effect tells a client. `read` tools change nothing. Every other effect may lose
/// something that cannot be had back: a write can replace a file's content, a command can do
/// whatever the account can, and admin changes what Legion allows. A client skips confirmation
/// for a tool that says it is not destructive, so none of them says so.
pub(crate) fn annotations(title: &str, effect: Effect) -> ToolAnnotations {
    let annotations = ToolAnnotations::with_title(title);
    match effect {
        Effect::Read => annotations.read_only(true),
        Effect::Write | Effect::Exec | Effect::Destroy | Effect::Power | Effect::Admin => {
            annotations.read_only(false).destructive(true)
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
        .with_hint("Rename the tool's argument, or serve the registry with --compact, which passes on beside the arguments."));
    }
    properties.insert(ON.to_owned(), on_schema());
    Ok(())
}

/// The schema of `on`, the same in full and compact mode: one string, the type every client can
/// send, naming several machines with commas the way the command line's `--on` does.
pub(crate) fn on_schema() -> Value {
    json!({
        "type": "string",
        "description": "Where to run: a machine's name, several names separated by commas, or \"all\". Leave it out to run on this machine."
    })
}

/// The machines `on` names. A list of names is taken as well as the string the schema asks for,
/// since it is what a client that knows the wire protocol sends.
pub(crate) fn target(on: Value) -> Result<Option<Target>, String> {
    match on {
        Value::Null => Ok(None),
        Value::String(text) => Target::parse(&text).map(Some),
        other => serde_json::from_value(other)
            .map(Some)
            .map_err(|error| error.to_string()),
    }
}

/// The first sentence of a description, or its first line, for listings that show many tools.
pub(crate) fn summary(description: &str) -> &str {
    let line = description.lines().next().unwrap_or_default().trim();
    match line.find(". ") {
        Some(end) => &line[..=end],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use legion_core::schema::problems;

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
        let tool = mcp_tool(&spec("fs.read", Effect::Read, closed()), true).unwrap();
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
        for effect in [
            Effect::Write,
            Effect::Exec,
            Effect::Destroy,
            Effect::Power,
            Effect::Admin,
        ] {
            assert_eq!(hints(effect), (Some(false), Some(true)), "{effect}");
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
            true,
        )
        .unwrap();
        assert_eq!(odd.input_schema.get("$schema"), None);
        assert_eq!(odd.input_schema["type"], json!("object"));
        let broken = mcp_tool(&spec("x.broken", Effect::Read, json!("string")), true).unwrap();
        assert_eq!(broken.input_schema["type"], json!("object"));
        assert!(broken.input_schema["properties"]["on"].is_object());
        let compact = mcp_tool(&spec("x.y", Effect::Read, closed()), false).unwrap();
        assert_eq!(compact.input_schema["properties"].get("on"), None);
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
        let tool = mcp_tool(&spec("exec.run", Effect::Exec, schema), true).unwrap();
        let branches = tool.input_schema["oneOf"].as_array().unwrap();
        assert!(branches[0]["properties"]["on"].is_object());
        assert!(branches[1]["properties"]["on"].is_object());
        assert_eq!(branches[2].get("properties"), None);
    }

    #[test]
    fn a_tool_with_its_own_on_cannot_be_served_in_full_mode() {
        let schema = json!({"type": "object", "properties": {"on": {"type": "boolean"}}});
        let tools = vec![spec("x.switch", Effect::Write, schema)];
        let error = Catalog::new(tools.clone(), true).err().unwrap();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(error.message.contains("x.switch"), "{}", error.message);
        assert!(Catalog::new(tools, false).is_ok());
    }

    #[test]
    fn names_that_would_collide_are_refused_in_full_mode_only() {
        let tools = vec![
            spec("a.b_c", Effect::Read, closed()),
            spec("a_b.c", Effect::Read, closed()),
            spec("fs.read", Effect::Read, closed()),
        ];
        let error = Catalog::new(tools.clone(), true).err().unwrap();
        assert!(
            error.message.contains("a.b_c and a_b.c"),
            "{}",
            error.message
        );
        assert!(error.message.contains("a_b_c"), "{}", error.message);
        let catalog = Catalog::new(tools, false).unwrap();
        assert_eq!(catalog.resolve("fs_read"), "fs.read");
        assert_eq!(catalog.resolve("fs.read"), "fs.read");
        // Ambiguous, so left as it is, for the node to call unknown.
        assert_eq!(catalog.resolve("a_b_c"), "a_b_c");
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
        let catalog = Catalog::new(registry.specs(), true).unwrap();
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
            assert_eq!(problems(&schema), Vec::<String>::new(), "{name}");
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
            let annotations = tool.annotations.as_ref().unwrap();
            if entry.spec.effect == Effect::Read {
                assert_eq!(annotations.read_only_hint, Some(true), "{name}");
            } else {
                assert_eq!(
                    (annotations.read_only_hint, annotations.destructive_hint),
                    (Some(false), Some(true)),
                    "{name} would let a client skip confirming it"
                );
            }
        }
    }

    #[test]
    fn on_is_a_name_names_separated_by_commas_or_all() {
        assert_eq!(target(Value::Null), Ok(None));
        assert_eq!(
            target(json!("tower")),
            Ok(Some(Target::One("tower".into())))
        );
        assert_eq!(
            target(json!("tower, pi")),
            Ok(Some(Target::Many(vec!["tower".into(), "pi".into()])))
        );
        assert_eq!(target(json!("all")), Ok(Some(Target::All)));
        assert_eq!(
            target(json!(["tower", "pi"])),
            Ok(Some(Target::Many(vec!["tower".into(), "pi".into()])))
        );
        assert!(target(json!("")).is_err());
        assert!(target(json!(5)).is_err());
    }

    #[test]
    fn a_summary_is_the_first_sentence() {
        assert_eq!(summary("Does it. Then more."), "Does it.");
        assert_eq!(summary("One line\nand another."), "One line");
        assert_eq!(summary("Reads 1.5 GiB files."), "Reads 1.5 GiB files.");
        assert_eq!(summary(""), "");
    }
}
