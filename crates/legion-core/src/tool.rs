//! What a tool is, and the helper that makes one from an `async fn`.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::policy::{Grant, Policy};
use crate::{Ctx, Paths};

/// A future a tool returns. Boxed, so [`Tool`] can be used as `dyn Tool` and a registry can hold
/// tools of every type.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The error a tool returns: the same object every door hands to its caller.
pub type ToolError = ErrorBody;

/// A named operation with a schema and an effect.
///
/// Most tools are written as an `async fn` and registered with
/// [`Registry::add_fn`](crate::Registry::add_fn), which implements this trait for them. Implement it
/// by hand only for a tool whose schema is not known at compile time, such as one a user defines.
///
/// When a call is cancelled the node cancels the tool's context and gives it
/// [`CANCEL_GRACE`](crate::CANCEL_GRACE) to return on its own; whatever it returns by then is the
/// call's result, and only after that is its future dropped. So a tool that runs for long, or
/// starts something that outlives a dropped future (a child process tree, a forwarded call), waits
/// on [`Ctx::cancelled`] alongside its work, stops that work, cleans up and returns. Work handed to
/// `spawn_blocking` cannot be stopped from outside at all: it checks a clone of
/// [`Ctx::cancel_token`] between steps itself. A `Drop` guard remains the last line for whatever a
/// tool still holds when the grace runs out.
pub trait Tool: Send + Sync + 'static {
    /// How the tool describes itself.
    fn spec(&self) -> &ToolSpec;

    /// Authorises a call to this tool.
    ///
    /// Most tools use the default implementation, which delegates directly to
    /// [`Policy::authorize`]. Tools with a narrowly scoped exception (such as allowing a
    /// principal to cancel its own transfer job without `effect:exec`) override this method.
    fn authorize(
        &self,
        policy: &Policy,
        principal: &str,
        pairing_id: Option<&str>,
        paths: &Paths,
        _args: &Value,
    ) -> Result<Grant, ErrorBody> {
        let _ = pairing_id;
        policy.authorize(principal, self.spec(), paths)
    }

    /// Runs the tool. `args` is whatever the caller sent; checking it is the tool's job.
    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>>;
}

/// An `async fn(&Ctx, Args) -> Result<Out, ToolError>`, which is what
/// [`Registry::add_fn`](crate::Registry::add_fn) takes.
///
/// It exists because the future such a function returns borrows the context, and a plain `Fn`
/// bound cannot say that its return type depends on the lifetime of its argument. Any `async fn`
/// with that signature implements it; nothing else needs to.
pub trait Handler<'a, A, O>: Send + Sync + 'static {
    /// The future the function returns.
    type Future: Future<Output = Result<O, ToolError>> + Send + 'a;

    /// Calls the function.
    fn run(&self, ctx: &'a Ctx, args: A) -> Self::Future;
}

impl<'a, F, Fut, A, O> Handler<'a, A, O> for F
where
    F: Fn(&'a Ctx, A) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, ToolError>> + Send + 'a,
{
    type Future = Fut;

    fn run(&self, ctx: &'a Ctx, args: A) -> Fut {
        self(ctx, args)
    }
}

/// A tool made from a [`Handler`].
pub(crate) struct FnTool<A, O, F> {
    spec: ToolSpec,
    handler: F,
    types: PhantomData<fn(A) -> O>,
}

impl<A, O, F> FnTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Handler<'a, A, O>,
{
    pub(crate) fn new(name: &str, effect: Effect, description: &str, handler: F) -> Self {
        FnTool {
            spec: ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                effect,
                input_schema: input_schema::<A>(),
            },
            handler,
            types: PhantomData,
        }
    }
}

impl<A, O, F> Tool for FnTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Handler<'a, A, O>,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let args = parse_args::<A>(&self.spec.name, args)?;
            let out = self.handler.run(ctx, args).await?;
            into_object(&self.spec.name, &out)
        })
    }
}

/// For a tool that takes no arguments: refuses any it is given.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

/// Turns a call's arguments into the tool's own type.
///
/// Absent arguments are `{}`. Anything but an object is refused, and so is any field the type does
/// not read, at any depth, so a misspelt option is an error rather than one that silently does
/// nothing. Every error is `invalid_args` and names the field it is about.
pub fn parse_args<A: DeserializeOwned>(tool: &str, args: Value) -> Result<A, ErrorBody> {
    let args = match args {
        Value::Null => Value::Object(Map::new()),
        Value::Object(_) => args,
        other => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{tool} takes a JSON object of arguments, not {}.",
                    kind_of(&other)
                ),
            )
            .with_hint(format!(
                "`legion help {tool}` shows the arguments it takes."
            )));
        }
    };
    let mut unknown = Vec::new();
    let mut note = |path: serde_ignored::Path<'_>| unknown.push(path.to_string());
    let parsed: Result<A, _> =
        serde_path_to_error::deserialize(serde_ignored::Deserializer::new(args, &mut note));
    match parsed {
        Err(error) => {
            let field = error.path().to_string();
            let reason = error.inner().to_string();
            let message = if field == "." {
                format!("{tool}: {reason}.")
            } else {
                format!("{tool}: `{field}`: {reason}.")
            };
            Err(ErrorBody::new(ErrorCode::InvalidArgs, message)
                .with_hint(format!(
                    "`legion help {tool}` shows the arguments it takes."
                ))
                .with_details(json!({"field": field})))
        }
        Ok(_) if !unknown.is_empty() => Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{tool} does not take `{}`.", unknown.join("`, `")),
        )
        .with_hint(format!(
            "`legion help {tool}` shows the arguments it takes."
        ))
        .with_details(json!({"fields": unknown}))),
        Ok(parsed) => Ok(parsed),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A tool's result as the JSON object every door returns. A tool returning anything else is a bug
/// in the tool, reported as such.
fn into_object<O: Serialize>(tool: &str, out: &O) -> Result<Value, ErrorBody> {
    match serde_json::to_value(out) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(other) => Err(ErrorBody::new(
            ErrorCode::Internal,
            format!(
                "{tool} returned {} instead of a JSON object.",
                kind_of(&other)
            ),
        )),
        Err(error) => Err(ErrorBody::new(
            ErrorCode::Internal,
            format!("{tool} returned a result that cannot be written as JSON: {error}."),
        )),
    }
}

/// The JSON Schema of a tool's argument type: inlined, without the meta-schema, the Rust type's
/// name or its doc comment (the tool's description says what it does), with every object that
/// lists its properties closed to others, which is what [`parse_args`] enforces whether or not the
/// type says so, and in the plain form strict clients accept (see [`crate::schema::normalise`]).
pub fn input_schema<A: JsonSchema>() -> Value {
    let generator = SchemaSettings::draft2020_12()
        .with(|settings| {
            settings.meta_schema = None;
            settings.inline_subschemas = true;
        })
        .into_generator();
    let mut schema = generator.into_root_schema_for::<A>().to_value();
    if let Some(root) = schema.as_object_mut() {
        root.remove("title");
        root.remove("description");
    }
    close_objects(&mut schema);
    crate::schema::normalise(&mut schema);
    schema
}

fn close_objects(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            if map.contains_key("properties") && !map.contains_key("additionalProperties") {
                map.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
            map.values_mut().for_each(close_objects);
        }
        Value::Array(items) => items.iter_mut().for_each(close_objects),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    /// Deliberately without `deny_unknown_fields`: the helper refuses unknown fields anyway.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct Args {
        /// Where to look.
        path: String,
        #[serde(default)]
        options: Options,
    }

    #[derive(Debug, Default, Deserialize, JsonSchema)]
    struct Options {
        #[serde(default)]
        depth: u32,
    }

    #[test]
    fn arguments_are_parsed_into_the_tool_type() {
        let args: Args =
            parse_args("fs.list", json!({"path": "/x", "options": {"depth": 2}})).unwrap();
        assert_eq!(args.path, "/x");
        assert_eq!(args.options.depth, 2);
    }

    #[test]
    fn unknown_fields_are_refused_at_any_depth_with_their_path() {
        let error = parse_args::<Args>("fs.list", json!({"path": "/x", "colour": 1})).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs);
        assert!(error.message.contains("`colour`"), "{}", error.message);
        let error = parse_args::<Args>("fs.list", json!({"path": "/x", "options": {"deep": 1}}))
            .unwrap_err();
        assert!(
            error.message.contains("`options.deep`"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_wrong_type_names_the_field() {
        let error = parse_args::<Args>(
            "fs.list",
            json!({"path": "/x", "options": {"depth": "two"}}),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgs);
        assert!(
            error.message.contains("`options.depth`"),
            "{}",
            error.message
        );
        assert_eq!(error.details, Some(json!({"field": "options.depth"})));
    }

    #[test]
    fn a_missing_field_and_a_non_object_are_refused() {
        let error = parse_args::<Args>("fs.list", json!({})).unwrap_err();
        assert!(
            error.message.contains("missing field `path`"),
            "{}",
            error.message
        );
        let error = parse_args::<Args>("fs.list", json!([1])).unwrap_err();
        assert!(error.message.contains("not an array"), "{}", error.message);
        assert!(parse_args::<NoArgs>("meta.version", Value::Null).is_ok());
        assert!(parse_args::<NoArgs>("meta.version", json!({"x": 1})).is_err());
    }

    #[test]
    fn the_schema_is_closed_and_carries_the_doc_comments() {
        let schema = input_schema::<Args>();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["path"]["description"],
            json!("Where to look.")
        );
        assert_eq!(
            schema["properties"]["options"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(schema.get("title"), None);
        assert_eq!(schema.get("$schema"), None);
        assert_eq!(
            input_schema::<NoArgs>()["additionalProperties"],
            json!(false)
        );
    }

    /// How a tool's arguments usually look: optional fields, a documented enum, a sized number.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct Find {
        /// Only names this glob matches.
        #[serde(default)]
        glob: Option<String>,
        /// Only entries of this kind.
        #[serde(default)]
        kind: Option<Kind>,
        /// Which order.
        #[serde(default)]
        order: Order,
        /// At most this many.
        #[serde(default)]
        limit: Option<u32>,
    }

    /// A kind of entry.
    #[derive(Debug, PartialEq, Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    enum Kind {
        /// A regular file.
        File,
        /// A directory.
        Dir,
    }

    #[derive(Debug, Default, PartialEq, Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    enum Order {
        #[default]
        Name,
        Size,
    }

    #[test]
    fn the_schema_is_in_the_plain_form_strict_clients_accept() {
        let schema = input_schema::<Find>();
        assert_eq!(crate::schema::problems(&schema), Vec::<String>::new());
        let properties = &schema["properties"];
        assert_eq!(
            properties["glob"],
            json!({"type": "string", "description": "Only names this glob matches."})
        );
        assert_eq!(
            properties["kind"],
            json!({
                "type": "string",
                "enum": ["file", "dir"],
                "description": "Only entries of this kind.\n`file`: A regular file.\n`dir`: A directory."
            })
        );
        assert_eq!(properties["order"]["enum"], json!(["name", "size"]));
        assert_eq!(properties["limit"]["type"], json!("integer"));
        assert_eq!(properties["limit"].get("format"), None);
        assert_eq!(properties["limit"]["minimum"], json!(0));
        assert_eq!(schema.get("required"), None);
    }

    #[test]
    fn an_optional_field_still_takes_an_explicit_null() {
        let find: Find = parse_args(
            "fs.find",
            json!({"glob": null, "kind": null, "limit": null}),
        )
        .unwrap();
        assert_eq!((find.glob, find.kind, find.limit), (None, None, None));
        assert_eq!(find.order, Order::Name);
        let find: Find = parse_args("fs.find", json!({"kind": "dir"})).unwrap();
        assert_eq!(find.kind, Some(Kind::Dir));
    }

    #[test]
    fn a_result_must_be_an_object() {
        assert!(into_object("t.x", &json!({"a": 1})).is_ok());
        let error = into_object("t.x", &3).unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
    }
}
