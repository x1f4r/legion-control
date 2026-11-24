//! The tools a node offers and the configuration files they read.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use legion_proto::{Effect, ErrorBody, ToolSpec};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::{ConfigStore, Document, NodeFile};
use crate::paths::Paths;
use crate::policy::Policy;
use crate::tool::{FnTool, Handler, Tool};

/// One health check a [`ToolSource`] reports to `meta.doctor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCheck {
    /// What was checked, such as `tools.d` or `tools.d/broken.toml`.
    pub name: String,
    /// Whether it is as it should be.
    pub ok: bool,
    /// What was found.
    pub detail: String,
    /// What to do about it, when something is wrong.
    pub hint: Option<String>,
}

/// A reloadable source of tools consulted after the built-in [`Registry`].
///
/// Built-in tools are fixed once the node starts, while extension tools on disk can be added,
/// changed or removed without restarting a long-lived door. [`Node`](crate::Node) checks its
/// built-in registry first so built-ins always win, and then asks each registered source.
pub trait ToolSource: Send + Sync + 'static {
    /// Looks up one tool by name.
    fn get(&self, paths: &Paths, builtins: &Registry, name: &str) -> Option<Arc<dyn Tool>>;

    /// Every valid tool this source currently offers.
    fn specs(&self, paths: &Paths, builtins: &Registry) -> Vec<ToolSpec>;

    /// Health checks for `meta.doctor`, such as reporting extension files that did not parse.
    fn checks(&self, _paths: &Paths, _builtins: &Registry) -> Vec<SourceCheck> {
        Vec::new()
    }
}

/// Every tool a node offers, by name, and every configuration document `meta.doctor` checks.
///
/// Each tool family has a `register(&mut Registry)` that adds its tools here, and the doors read
/// from the same registry, so a tool added once appears in all of them.
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    documents: Vec<DocumentInfo>,
    sources: Vec<Arc<dyn ToolSource>>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

impl Registry {
    /// A registry with no tools, knowing the documents every node reads: `node.toml` and
    /// `policy.toml`.
    pub fn new() -> Registry {
        let mut registry = Registry {
            tools: BTreeMap::new(),
            documents: Vec::new(),
            sources: Vec::new(),
        };
        registry.add_document::<NodeFile>();
        registry.add_document::<Policy>();
        registry
    }

    /// Adds a tool written as an `async fn(&Ctx, Args) -> Result<Out, ToolError>`.
    ///
    /// The tool's input schema is generated from `Args`, whose doc comments become the argument
    /// descriptions. Arguments are parsed before the function runs, refusing fields `Args` does not
    /// have; `Out` must serialize to a JSON object.
    ///
    /// ```
    /// use legion_core::proto::{Effect, ErrorBody, ErrorCode, EventKind};
    /// use legion_core::{Access, Ctx, Registry, ToolError};
    /// use schemars::JsonSchema;
    /// use serde::{Deserialize, Serialize};
    /// use serde_json::json;
    ///
    /// #[derive(Deserialize, JsonSchema)]
    /// struct Args {
    ///     /// The file to measure. (This comment is the argument's description in the schema.)
    ///     path: String,
    /// }
    ///
    /// #[derive(Serialize)]
    /// struct Size {
    ///     bytes: u64,
    /// }
    ///
    /// async fn size(ctx: &Ctx, args: Args) -> Result<Size, ToolError> {
    ///     // Every path a tool is given goes through the grant before it is touched.
    ///     let path = ctx.grant().check_path(&args.path, Access::Read)?;
    ///     ctx.emit(EventKind::Log, json!(format!("measuring {}", path.display())));
    ///     let meta = std::fs::metadata(&path).map_err(|error| {
    ///         ErrorBody::new(ErrorCode::NotFound, format!("{} cannot be read: {error}.", path.display()))
    ///     })?;
    ///     Ok(Size { bytes: meta.len() })
    /// }
    ///
    /// /// What a tool family's module exposes.
    /// pub fn register(registry: &mut Registry) {
    ///     registry.add_fn("demo.size", Effect::Read, "Say how big a file is.", size);
    /// }
    ///
    /// let mut registry = Registry::new();
    /// register(&mut registry);
    /// let spec = registry.get("demo.size").map(|tool| tool.spec().clone());
    /// assert_eq!(spec.map(|spec| spec.input_schema["required"].clone()), Some(json!(["path"])));
    /// ```
    ///
    /// # Panics
    ///
    /// When `name` is not `family.verb` in lower case, or is already taken: both are mistakes in
    /// the code that registers it, which the first test building the registry catches.
    pub fn add_fn<A, O, F>(&mut self, name: &str, effect: Effect, description: &str, handler: F)
    where
        A: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + 'static,
        F: for<'a> Handler<'a, A, O>,
    {
        self.add(FnTool::new(name, effect, description, handler));
    }

    /// Adds a tool that implements [`Tool`] itself.
    ///
    /// # Panics
    ///
    /// As [`Registry::add_fn`]. Tools defined at run time go through [`Registry::try_add`].
    #[allow(
        clippy::panic,
        reason = "a built-in tool's name is fixed in the code that adds it"
    )]
    pub fn add(&mut self, tool: impl Tool) {
        if let Err(error) = self.try_add(tool) {
            panic!("{}", error.message);
        }
    }

    /// Adds a tool, or says why it cannot be added: its name is not `family.verb` in lower case,
    /// or a tool already has it.
    pub fn try_add(&mut self, tool: impl Tool) -> Result<(), ErrorBody> {
        let name = tool.spec().name.clone();
        if !is_tool_name(&name) {
            return Err(ErrorBody::new(
                legion_proto::ErrorCode::InvalidArgs,
                format!(
                    "{name:?} is not a tool name: write family.verb in lower case, such as fs.read."
                ),
            ));
        }
        if self.tools.contains_key(&name) {
            return Err(ErrorBody::new(
                legion_proto::ErrorCode::Exists,
                format!("A tool called {name} is already registered."),
            ));
        }
        self.tools.insert(name, Arc::new(tool));
        Ok(())
    }

    /// Makes a configuration document known, so `meta.doctor` checks that its file parses. Adding
    /// one twice has no further effect.
    pub fn add_document<D: Document>(&mut self) {
        if self.documents.iter().all(|known| known.file != D::FILE) {
            self.documents.push(DocumentInfo {
                file: D::FILE,
                check: |store| store.get::<D>().map(|_| ()),
            });
        }
    }

    /// The tool with this name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Whether any registered built-in tool belongs to `family` (the segment before the first `.`).
    pub fn has_family(&self, family: &str) -> bool {
        self.tools
            .keys()
            .any(|name| name.split('.').next() == Some(family))
    }

    /// Every tool's spec, sorted by name.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| tool.spec().clone())
            .collect()
    }

    /// Every tool's name, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// The configuration documents this node reads.
    pub fn documents(&self) -> &[DocumentInfo] {
        &self.documents
    }

    /// Registers a reloadable source of tools consulted after built-ins.
    pub fn add_source(&mut self, source: impl ToolSource) {
        self.sources.push(Arc::new(source));
    }

    /// The reloadable tool sources registered on this node.
    pub fn sources(&self) -> &[Arc<dyn ToolSource>] {
        &self.sources
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("documents", &self.documents)
            .finish()
    }
}

/// A configuration document a node reads, and how to check that its file parses.
#[derive(Clone, Copy)]
pub struct DocumentInfo {
    file: &'static str,
    check: fn(&ConfigStore) -> Result<(), ErrorBody>,
}

impl DocumentInfo {
    /// The file's name, such as `policy.toml`.
    pub fn file(&self) -> &'static str {
        self.file
    }

    /// Reads the file as the document it is, as a call that needed it would.
    pub fn check(&self, store: &ConfigStore) -> Result<(), ErrorBody> {
        (self.check)(store)
    }
}

impl fmt::Debug for DocumentInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.file)
    }
}

/// `family.verb`, lower case, each part starting with a letter: `fs.read`, `job.start`.
pub fn is_tool_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let part_fits = |part: &str| {
        part.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    let first = parts.next().is_some_and(part_fits);
    let mut rest = 0;
    for part in parts {
        if !part_fits(part) {
            return false;
        }
        rest += 1;
    }
    first && rest >= 1
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{Ctx, NoArgs, ToolError};

    #[derive(Serialize)]
    struct Pong {
        pong: bool,
    }

    async fn ping(_ctx: &Ctx, _args: NoArgs) -> Result<Pong, ToolError> {
        Ok(Pong { pong: true })
    }

    #[test]
    fn tools_are_listed_by_name_with_a_generated_schema() {
        let mut registry = Registry::new();
        registry.add_fn("test.ping", Effect::Read, "Answers.", ping);
        registry.add_fn("a.ping", Effect::Write, "Answers too.", ping);
        let names: Vec<&str> = registry.names().collect();
        assert_eq!(names, ["a.ping", "test.ping"]);
        let spec = &registry.specs()[1];
        assert_eq!(spec.effect, Effect::Read);
        assert_eq!(spec.input_schema["type"], json!("object"));
    }

    #[test]
    fn a_name_is_family_dot_verb_and_is_taken_once() {
        let mut registry = Registry::new();
        assert!(
            registry
                .try_add(FnTool::new("test.ping", Effect::Read, "", ping))
                .is_ok()
        );
        let again = registry.try_add(FnTool::new("test.ping", Effect::Read, "", ping));
        assert_eq!(
            again.map_err(|error| error.code),
            Err(legion_proto::ErrorCode::Exists)
        );
        for wrong in [
            "ping",
            "Test.ping",
            "test.",
            ".ping",
            "test.Ping",
            "test.p-ing",
            "1.x",
        ] {
            assert!(!is_tool_name(wrong), "{wrong} was taken for a tool name");
        }
        for right in ["fs.read", "job.status", "ext.my_tool", "a.b.c", "fs2.read"] {
            assert!(is_tool_name(right), "{right} was refused");
        }
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn adding_a_built_in_twice_is_a_mistake_that_stops_the_program() {
        let mut registry = Registry::new();
        registry.add_fn("test.ping", Effect::Read, "", ping);
        registry.add_fn("test.ping", Effect::Read, "", ping);
    }

    #[test]
    fn every_registry_knows_node_and_policy() {
        let mut registry = Registry::new();
        registry.add_document::<Policy>();
        let files: Vec<&str> = registry
            .documents()
            .iter()
            .map(DocumentInfo::file)
            .collect();
        assert_eq!(files, ["node.toml", "policy.toml"]);
    }
}
