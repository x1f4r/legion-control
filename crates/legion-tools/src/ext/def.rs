//! Extension tool definitions (`tools.d/<name>.toml`): parsing, validation, strict JSON Schema
//! generation, runtime argument checking, environment mapping and TOML serialization.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use legion_core::is_tool_name;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use toml_edit::{Array, DocumentMut, Item, Table};

/// Static family namespaces reserved for Legion (core families plus names kept for Legion's own
/// future use). At load and define time, every family present in the node's built-in registry is
/// also reserved dynamically so newly added built-in families can never be shadowed.
pub const RESERVED_FAMILIES: [&str; 17] = [
    "agent", "audit", "config", "device", "exec", "ext", "fav", "fleet", "fs", "job", "legion",
    "meta", "power", "proj", "svc", "sys", "token",
];

/// Default command timeout in seconds when `timeout_s` is omitted.
pub const DEFAULT_TIMEOUT_S: f64 = 60.0;

/// Maximum command timeout in seconds.
pub const MAX_TIMEOUT_S: f64 = 3600.0;

/// Maximum byte length of a single `LEGION_ARG_<NAME>` environment variable value (32 KiB).
/// Keeps each variable inside Windows's per-variable environment limit and well below Linux's
/// `MAX_ARG_STRLEN`.
pub const MAX_ENV_ARG_BYTES: usize = 32 * 1024;

/// Maximum combined byte length of all `LEGION_ARG_*` environment variable values (96 KiB).
pub const MAX_ENV_TOTAL_BYTES: usize = 96 * 1024;

/// What happens when a string argument exceeds the environment variable size cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LargeArgPolicy {
    /// Fail the call with `invalid_args` before starting the command (default).
    Refuse,
    /// Omit `LEGION_ARG_<NAME>`, set `LEGION_ARG_<NAME>_OMITTED=1`, and pass the full value only
    /// in the JSON object on stdin.
    Stdin,
}

impl LargeArgPolicy {
    /// The TOML/JSON spelling of the policy.
    pub fn as_str(self) -> &'static str {
        match self {
            LargeArgPolicy::Refuse => "refuse",
            LargeArgPolicy::Stdin => "stdin",
        }
    }
}

/// Parameter types an extension tool may declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    /// A UTF-8 string.
    String,
    /// A 64-bit signed integer.
    Integer,
    /// A finite JSON number.
    Number,
    /// A boolean (`true` or `false`).
    Boolean,
}

impl ParamType {
    /// The JSON Schema `type` name.
    pub fn as_str(self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Integer => "integer",
            ParamType::Number => "number",
            ParamType::Boolean => "boolean",
        }
    }
}

/// A default parameter value as stored in TOML and shown in JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    /// A boolean value.
    Bool(bool),
    /// A signed integer value.
    Integer(i64),
    /// A floating-point value.
    Number(f64),
    /// A string value.
    String(String),
}

impl ParamValue {
    /// Converts a JSON value into a [`ParamValue`] matching `kind`, or explains the mismatch.
    pub fn from_json(value: &Value, kind: ParamType) -> Result<ParamValue, String> {
        match (kind, value) {
            (ParamType::Boolean, Value::Bool(b)) => Ok(ParamValue::Bool(*b)),
            (ParamType::Integer, Value::Number(n)) => n
                .as_i64()
                .map(ParamValue::Integer)
                .ok_or_else(|| "default must be a 64-bit integer".to_owned()),
            (ParamType::Number, Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    Ok(ParamValue::Integer(i))
                } else if let Some(f) = n.as_f64()
                    && f.is_finite()
                {
                    Ok(ParamValue::Number(f))
                } else {
                    Err("default must be a finite number".to_owned())
                }
            }
            (ParamType::String, Value::String(s)) => Ok(ParamValue::String(s.clone())),
            _ => Err(format!(
                "default must be {}, not {}",
                article(kind.as_str()),
                json_kind(value)
            )),
        }
    }

    /// Converts to a JSON value for schema generation and runtime argument injection.
    pub fn to_json(&self) -> Value {
        match self {
            ParamValue::Bool(b) => Value::Bool(*b),
            ParamValue::Integer(i) => json!(*i),
            ParamValue::Number(f) => json!(*f),
            ParamValue::String(s) => Value::String(s.clone()),
        }
    }

    /// Converts to a `toml_edit` value for canonical TOML serialization.
    fn to_toml_value(&self) -> toml_edit::Value {
        match self {
            ParamValue::Bool(b) => toml_edit::Value::from(*b),
            ParamValue::Integer(i) => toml_edit::Value::from(*i),
            ParamValue::Number(f) => toml_edit::Value::from(*f),
            ParamValue::String(s) => toml_edit::Value::from(s.as_str()),
        }
    }
}

/// Timeout in seconds that preserves integer spelling (`30`) when whole and float spelling (`0.5`)
/// when fractional.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeoutSeconds(pub f64);

impl TimeoutSeconds {
    /// Returns the timeout as `f64` seconds.
    pub fn as_f64(self) -> f64 {
        self.0
    }

    /// Returns the exact integer value when the timeout is a whole non-negative number of seconds.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn as_whole_u64(self) -> Option<u64> {
        if self.0.is_finite() && self.0 >= 0.0 && self.0.fract() == 0.0 && self.0 <= u64::MAX as f64
        {
            Some(self.0 as u64)
        } else {
            None
        }
    }

    fn to_toml_value(self) -> toml_edit::Value {
        match self.as_whole_u64().and_then(|v| i64::try_from(v).ok()) {
            Some(whole) => toml_edit::Value::from(whole),
            None => toml_edit::Value::from(self.0),
        }
    }
}

impl Serialize for TimeoutSeconds {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.as_whole_u64() {
            Some(whole) => serializer.serialize_u64(whole),
            None => serializer.serialize_f64(self.0),
        }
    }
}

impl<'de> Deserialize<'de> for TimeoutSeconds {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        Ok(TimeoutSeconds(value))
    }
}

/// One parameter declaration inside `[params.<name>]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamDef {
    /// Parameter type: `string`, `integer`, `number` or `boolean`.
    #[serde(rename = "type")]
    pub kind: ParamType,
    /// Human-readable description shown in the generated schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether callers must supply this parameter.
    #[serde(default)]
    pub required: bool,
    /// Default value used when omitted or `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<ParamValue>,
    /// Allowed string values (only valid for `string` parameters).
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    /// How to handle string values larger than [`MAX_ENV_ARG_BYTES`] (only valid for `string`
    /// parameters). Defaults to refusing the call with `invalid_args`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large: Option<LargeArgPolicy>,
}

/// One extension tool definition, as stored in `tools.d/<name>.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDef {
    /// Tool name in `family.verb` lower-case form.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// Effect used by policy and MCP annotations.
    pub effect: Effect,
    /// Exact program and arguments to run without a shell. Mutually exclusive with `cmd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Vec<String>>,
    /// Command line for the platform shell. Mutually exclusive with `run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    /// Command timeout in seconds (default 60, max 3600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<TimeoutSeconds>,
    /// Optional working directory (absolute or starting with `~`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Declared parameters, keyed by parameter name.
    #[serde(default)]
    pub params: BTreeMap<String, ParamDef>,
}

/// Why a definition file failed to parse or validate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseFailure {
    /// Human-readable reason, including the line number when known.
    pub reason: String,
    /// 1-based line number when known.
    pub line: Option<usize>,
}

/// Validation failure naming the relevant field (for error messages and line lookup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    /// Field name or dotted path, such as `name` or `params.index`.
    pub field: String,
    /// Human-readable reason without a trailing period.
    pub message: String,
}

impl ValidationError {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        ValidationError {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Converts into an `invalid_args` [`ErrorBody`] for `ext.define`.
    pub fn into_error_body(self) -> ErrorBody {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("ext.define: `{}`: {}.", self.field, self.message),
        )
        .with_details(json!({"field": self.field}))
    }
}

/// Parses and validates an extension TOML file, returning the reason and line on failure.
pub fn parse_toml(
    bytes: &[u8],
    is_builtin: &dyn Fn(&str) -> bool,
) -> Result<ExtensionDef, ParseFailure> {
    parse_toml_with_reserved(bytes, is_builtin, &|_| false)
}

/// Parses and validates an extension TOML file, also refusing any family for which
/// `is_reserved_family` returns `true` (typically families present in the built-in registry).
pub fn parse_toml_with_reserved(
    bytes: &[u8],
    is_builtin: &dyn Fn(&str) -> bool,
    is_reserved_family: &dyn Fn(&str) -> bool,
) -> Result<ExtensionDef, ParseFailure> {
    let def: ExtensionDef =
        legion_core::config::parse_toml_bytes(bytes).map_err(|(message, line)| {
            let reason = match line {
                Some(line) => format!("line {line}: {message}"),
                None => message,
            };
            ParseFailure { reason, line }
        })?;
    let text = std::str::from_utf8(bytes).unwrap_or_default();
    if let Err(error) = def.validate_with_reserved(is_builtin, is_reserved_family) {
        let line = find_field_line(text, &error.field);
        let reason = match line {
            Some(line) => format!("line {line}: {}", error.message),
            None => error.message,
        };
        return Err(ParseFailure { reason, line });
    }
    Ok(def)
}

impl ExtensionDef {
    /// Validates every field of the definition against built-in tool names and
    /// [`RESERVED_FAMILIES`].
    pub fn validate(&self, is_builtin: &dyn Fn(&str) -> bool) -> Result<(), ValidationError> {
        self.validate_with_reserved(is_builtin, &|_| false)
    }

    /// Validates every field of the definition, also refusing families reported by
    /// `is_reserved_family` (such as every family registered in the node's built-in registry).
    pub fn validate_with_reserved(
        &self,
        is_builtin: &dyn Fn(&str) -> bool,
        is_reserved_family: &dyn Fn(&str) -> bool,
    ) -> Result<(), ValidationError> {
        if !is_tool_name(&self.name) {
            return Err(ValidationError::new(
                "name",
                format!(
                    "{:?} is not a tool name: write family.verb in lower case, such as gpu.free",
                    self.name
                ),
            ));
        }
        let family = self.name.split('.').next().unwrap_or_default();
        if RESERVED_FAMILIES.contains(&family) || is_reserved_family(family) {
            return Err(ValidationError::new(
                "name",
                format!(
                    "family `{family}` is reserved for built-in Legion tools; choose another family"
                ),
            ));
        }
        if is_builtin(&self.name) {
            return Err(ValidationError::new(
                "name",
                format!(
                    "{} is already a built-in tool and cannot be shadowed by an extension",
                    self.name
                ),
            ));
        }
        if self.description.trim().is_empty() {
            return Err(ValidationError::new(
                "description",
                "description must not be empty",
            ));
        }
        match (&self.run, &self.cmd) {
            (Some(_), Some(_)) => {
                return Err(ValidationError::new("run", "give `run` or `cmd`, not both"));
            }
            (None, None) => {
                return Err(ValidationError::new(
                    "run",
                    "give a command to run, as `run` (argv list) or `cmd` (shell command line)",
                ));
            }
            (Some(argv), None) => {
                if argv.is_empty() {
                    return Err(ValidationError::new("run", "`run` must not be empty"));
                }
                if argv[0].is_empty() {
                    return Err(ValidationError::new(
                        "run",
                        "`run` starts with an empty program name",
                    ));
                }
                if argv.iter().any(|arg| arg.contains('\0')) {
                    return Err(ValidationError::new(
                        "run",
                        "an entry in `run` holds a NUL character, which no command line can carry",
                    ));
                }
            }
            (None, Some(cmd)) => {
                if cmd.trim().is_empty() {
                    return Err(ValidationError::new("cmd", "`cmd` must not be empty"));
                }
                if cmd.contains('\0') {
                    return Err(ValidationError::new(
                        "cmd",
                        "`cmd` holds a NUL character, which no command line can carry",
                    ));
                }
            }
        }
        if let Some(timeout) = self.timeout_s {
            let seconds = timeout.as_f64();
            if !(seconds.is_finite() && seconds > 0.0 && seconds <= MAX_TIMEOUT_S) {
                return Err(ValidationError::new(
                    "timeout_s",
                    format!(
                        "`timeout_s` must be greater than 0 and at most {MAX_TIMEOUT_S}, not {seconds}"
                    ),
                ));
            }
        }
        if let Some(cwd) = &self.cwd {
            if cwd.is_empty() {
                return Err(ValidationError::new("cwd", "`cwd` must not be empty"));
            }
            if cwd.contains('\0') {
                return Err(ValidationError::new(
                    "cwd",
                    "`cwd` holds a NUL character, which no path can carry",
                ));
            }
            let from_home = cwd == "~" || cwd.starts_with("~/") || cwd.starts_with("~\\");
            if !from_home && !Path::new(cwd).is_absolute() {
                return Err(ValidationError::new(
                    "cwd",
                    format!("{cwd:?} is a relative path; write an absolute path or start with ~"),
                ));
            }
        }
        for (param_name, param) in &self.params {
            let field = format!("params.{param_name}");
            if !is_param_name(param_name) {
                return Err(ValidationError::new(
                    field,
                    format!(
                        "{param_name:?} is not a parameter name: use lower-case ASCII letters, digits and _, starting with a letter"
                    ),
                ));
            }
            if let Some(values) = &param.enum_values {
                if param.kind != ParamType::String {
                    return Err(ValidationError::new(
                        format!("params.{param_name}.enum"),
                        format!(
                            "`enum` is only allowed for string parameters, not {}",
                            param.kind.as_str()
                        ),
                    ));
                }
                if values.is_empty() {
                    return Err(ValidationError::new(
                        format!("params.{param_name}.enum"),
                        "`enum` must contain at least one allowed value",
                    ));
                }
                let mut seen = HashSet::new();
                for value in values {
                    if value.contains('\0') {
                        return Err(ValidationError::new(
                            format!("params.{param_name}.enum"),
                            "`enum` values cannot contain NUL characters",
                        ));
                    }
                    if !seen.insert(value.as_str()) {
                        return Err(ValidationError::new(
                            format!("params.{param_name}.enum"),
                            format!("`enum` repeats the value {value:?}"),
                        ));
                    }
                }
            }
            if param.large.is_some() && param.kind != ParamType::String {
                return Err(ValidationError::new(
                    format!("params.{param_name}.large"),
                    format!(
                        "`large` is only allowed for string parameters, not {}",
                        param.kind.as_str()
                    ),
                ));
            }
            if let Some(default) = &param.default {
                validate_default(param_name, param, default)?;
            }
        }
        Ok(())
    }

    /// Generates the tool's JSON Schema in the strict normal form built-in tools use.
    pub fn input_schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for (name, param) in &self.params {
            let mut prop = Map::new();
            prop.insert(
                "type".to_owned(),
                Value::String(param.kind.as_str().to_owned()),
            );
            if let Some(description) = &param.description
                && !description.trim().is_empty()
            {
                prop.insert("description".to_owned(), Value::String(description.clone()));
            }
            if let Some(values) = &param.enum_values {
                prop.insert(
                    "enum".to_owned(),
                    Value::Array(values.iter().cloned().map(Value::String).collect()),
                );
            }
            if let Some(default) = &param.default {
                prop.insert("default".to_owned(), default.to_json());
            }
            properties.insert(name.clone(), Value::Object(prop));
            if param.required {
                required.push(Value::String(name.clone()));
            }
        }
        let mut root = Map::new();
        root.insert("type".to_owned(), Value::String("object".to_owned()));
        root.insert("properties".to_owned(), Value::Object(properties));
        if !required.is_empty() {
            root.insert("required".to_owned(), Value::Array(required));
        }
        root.insert("additionalProperties".to_owned(), Value::Bool(false));
        let mut schema = Value::Object(root);
        legion_core::schema::normalise(&mut schema);
        schema
    }

    /// Validates caller arguments against `self.params` and applies defaults for omitted optional
    /// arguments. Unknown arguments, missing required arguments, type mismatches, NUL bytes in
    /// strings, and enum mismatches all fail with `invalid_args` before anything runs.
    pub fn validate_args(&self, args: Value) -> Result<Map<String, Value>, ErrorBody> {
        let tool = &self.name;
        let help_hint = || format!("`legion help {tool}` shows the arguments it takes.");
        let map = match args {
            Value::Null => Map::new(),
            Value::Object(map) => map,
            other => {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "{tool} takes a JSON object of arguments, not {}.",
                        json_kind(&other)
                    ),
                )
                .with_hint(help_hint()));
            }
        };
        let mut unknown: Vec<String> = map
            .keys()
            .filter(|key| !self.params.contains_key(key.as_str()))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            unknown.sort();
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{tool} does not take `{}`.", unknown.join("`, `")),
            )
            .with_hint(help_hint())
            .with_details(json!({"fields": unknown})));
        }

        let mut resolved = Map::new();
        for (name, param) in &self.params {
            match map.get(name) {
                None | Some(Value::Null) => {
                    if param.required {
                        return Err(ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            format!("{tool}: missing required argument `{name}`."),
                        )
                        .with_hint(help_hint())
                        .with_details(json!({"field": name})));
                    }
                    if let Some(default) = &param.default {
                        resolved.insert(name.clone(), default.to_json());
                    }
                }
                Some(value) => {
                    validate_arg_value(tool, name, param, value, &help_hint)?;
                    resolved.insert(name.clone(), value.clone());
                }
            }
        }
        Ok(resolved)
    }

    /// Maps validated arguments to `LEGION_ARG_<NAME>` environment variables, enforcing
    /// [`MAX_ENV_ARG_BYTES`] per argument and [`MAX_ENV_TOTAL_BYTES`] across all arguments.
    ///
    /// By default, an oversized argument is refused with `invalid_args` before the command starts.
    /// When a string parameter sets `large = "stdin"`, an oversized value omits
    /// `LEGION_ARG_<NAME>` and sets `LEGION_ARG_<NAME>_OMITTED=1` instead, while the full JSON
    /// object still arrives on stdin.
    pub fn prepare_arg_env(
        &self,
        resolved: &Map<String, Value>,
    ) -> Result<Vec<(String, String)>, ErrorBody> {
        let tool = &self.name;
        let mut vars = Vec::with_capacity(resolved.len());
        let mut total_bytes: usize = 0;
        for (name, value) in resolved {
            let upper = name.to_ascii_uppercase();
            let env_name = format!("LEGION_ARG_{upper}");
            let env_value = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
                other => other.to_string(),
            };
            let policy = self
                .params
                .get(name)
                .and_then(|param| param.large)
                .unwrap_or(LargeArgPolicy::Refuse);
            let is_string = value.is_string();
            let over_single = env_value.len() > MAX_ENV_ARG_BYTES;
            let over_total = !over_single && total_bytes + env_value.len() > MAX_ENV_TOTAL_BYTES;
            if over_single || over_total {
                if is_string && policy == LargeArgPolicy::Stdin {
                    vars.push((format!("LEGION_ARG_{upper}_OMITTED"), "1".to_owned()));
                    continue;
                }
                let message = if over_single {
                    format!(
                        "{tool}: `{name}` is {} bytes, which exceeds the {MAX_ENV_ARG_BYTES}-byte limit for environment variables; full arguments are also passed as JSON on stdin.",
                        env_value.len()
                    )
                } else {
                    format!(
                        "{tool}: `{name}` pushes total argument environment size past the {MAX_ENV_TOTAL_BYTES}-byte limit; full arguments are also passed as JSON on stdin."
                    )
                };
                return Err(ErrorBody::new(ErrorCode::InvalidArgs, message)
                    .with_hint(format!(
                        "Set `large = \"stdin\"` on `[params.{name}]` to omit {env_name}, set LEGION_ARG_{upper}_OMITTED=1, and read `{name}` from stdin JSON."
                    ))
                    .with_details(json!({
                        "field": name,
                        "bytes": env_value.len(),
                        "limit_bytes": if over_single { MAX_ENV_ARG_BYTES } else { MAX_ENV_TOTAL_BYTES },
                    })));
            }
            total_bytes += env_value.len();
            vars.push((env_name, env_value));
        }
        Ok(vars)
    }

    /// Maps validated arguments to `LEGION_ARG_<NAME>` environment variables without size limits.
    ///
    /// Strings pass verbatim; numbers and booleans use their JSON spelling.
    pub fn arg_env_vars(resolved: &Map<String, Value>) -> Vec<(String, String)> {
        let mut vars = Vec::with_capacity(resolved.len());
        for (name, value) in resolved {
            let env_name = format!("LEGION_ARG_{}", name.to_ascii_uppercase());
            let env_value = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
                other => other.to_string(),
            };
            vars.push((env_name, env_value));
        }
        vars
    }

    /// Effective timeout in seconds.
    pub fn effective_timeout_s(&self) -> f64 {
        self.timeout_s
            .map_or(DEFAULT_TIMEOUT_S, TimeoutSeconds::as_f64)
    }

    /// Serializes the definition as clean TOML suitable for `tools.d/<name>.toml`.
    pub fn to_toml(&self) -> String {
        let mut doc = DocumentMut::new();
        doc["name"] = toml_edit::value(self.name.as_str());
        doc["description"] = toml_edit::value(self.description.as_str());
        doc["effect"] = toml_edit::value(self.effect.as_str());
        if let Some(run) = &self.run {
            let mut array = Array::new();
            for item in run {
                array.push(item.as_str());
            }
            doc["run"] = Item::Value(toml_edit::Value::Array(array));
        }
        if let Some(cmd) = &self.cmd {
            doc["cmd"] = toml_edit::value(cmd.as_str());
        }
        if let Some(timeout_s) = self.timeout_s {
            doc["timeout_s"] = Item::Value(timeout_s.to_toml_value());
        }
        if let Some(cwd) = &self.cwd {
            doc["cwd"] = toml_edit::value(cwd.as_str());
        }
        if !self.params.is_empty() {
            let mut params_table = Table::new();
            params_table.set_implicit(true);
            for (name, param) in &self.params {
                let mut table = Table::new();
                table.insert("type", toml_edit::value(param.kind.as_str()));
                if let Some(description) = &param.description {
                    table.insert("description", toml_edit::value(description.as_str()));
                }
                if param.required {
                    table.insert("required", toml_edit::value(true));
                }
                if let Some(default) = &param.default {
                    table.insert("default", Item::Value(default.to_toml_value()));
                }
                if let Some(values) = &param.enum_values {
                    let mut array = Array::new();
                    for item in values {
                        array.push(item.as_str());
                    }
                    table.insert("enum", Item::Value(toml_edit::Value::Array(array)));
                }
                if let Some(large) = param.large {
                    table.insert("large", toml_edit::value(large.as_str()));
                }
                params_table.insert(name, Item::Table(table));
            }
            doc["params"] = Item::Table(params_table);
        }
        doc.to_string()
    }
}

/// Checks whether `name` is a valid extension parameter name (`[a-z][a-z0-9_]*`), which ensures
/// upper-casing into `LEGION_ARG_<NAME>` is collision-free and valid in POSIX and PowerShell.
pub fn is_param_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn validate_default(
    param_name: &str,
    param: &ParamDef,
    default: &ParamValue,
) -> Result<(), ValidationError> {
    let field = format!("params.{param_name}.default");
    match (param.kind, default) {
        (ParamType::Boolean, ParamValue::Bool(_)) => Ok(()),
        (ParamType::Integer, ParamValue::Integer(_)) => Ok(()),
        (ParamType::Number, ParamValue::Integer(_)) => Ok(()),
        (ParamType::Number, ParamValue::Number(f)) => {
            if f.is_finite() {
                Ok(())
            } else {
                Err(ValidationError::new(
                    field,
                    "default must be a finite number",
                ))
            }
        }
        (ParamType::String, ParamValue::String(s)) => {
            if s.contains('\0') {
                return Err(ValidationError::new(
                    field,
                    "default string cannot contain a NUL character",
                ));
            }
            if let Some(values) = &param.enum_values
                && !values.contains(s)
            {
                return Err(ValidationError::new(
                    field,
                    format!(
                        "default {s:?} is not one of the allowed enum values ({})",
                        values.join(", ")
                    ),
                ));
            }
            Ok(())
        }
        _ => Err(ValidationError::new(
            field,
            format!(
                "default does not match parameter type {}",
                param.kind.as_str()
            ),
        )),
    }
}

fn validate_arg_value(
    tool: &str,
    name: &str,
    param: &ParamDef,
    value: &Value,
    help_hint: &dyn Fn() -> String,
) -> Result<(), ErrorBody> {
    let bad = |message: String| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{tool}: `{name}`: {message}."),
        )
        .with_hint(help_hint())
        .with_details(json!({"field": name}))
    };
    match param.kind {
        ParamType::String => {
            let Some(text) = value.as_str() else {
                return Err(bad(format!("must be a string, not {}", json_kind(value))));
            };
            if text.contains('\0') {
                return Err(bad(
                    "holds a NUL character, which cannot be passed in an environment variable"
                        .to_owned(),
                ));
            }
            if let Some(allowed) = &param.enum_values
                && !allowed.iter().any(|item| item == text)
            {
                return Err(bad(format!(
                    "must be one of {}, not {text:?}",
                    allowed.join(", ")
                )));
            }
            Ok(())
        }
        ParamType::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(()),
            other => Err(bad(format!("must be an integer, not {}", json_kind(other)))),
        },
        ParamType::Number => match value {
            Value::Number(_) => Ok(()),
            other => Err(bad(format!("must be a number, not {}", json_kind(other)))),
        },
        ParamType::Boolean => match value {
            Value::Bool(_) => Ok(()),
            other => Err(bad(format!("must be a boolean, not {}", json_kind(other)))),
        },
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "an integer",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn article(kind: &str) -> String {
    if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
        format!("an {kind}")
    } else {
        format!("a {kind}")
    }
}

/// Best-effort 1-based line lookup in TOML source for a validation error's field path.
fn find_field_line(text: &str, field: &str) -> Option<usize> {
    if let Some(rest) = field.strip_prefix("params.") {
        let (param_name, subkey) = match rest.split_once('.') {
            Some((name, key)) => (name, Some(key)),
            None => (rest, None),
        };
        let header = format!("[params.{param_name}]");
        let quoted_header = format!("[params.\"{param_name}\"]");
        let mut in_section = false;
        let mut section_line = None;
        for (idx, raw) in text.lines().enumerate() {
            let trimmed = raw.trim();
            if trimmed.starts_with('[') {
                if trimmed == header || trimmed == quoted_header {
                    in_section = true;
                    section_line = Some(idx + 1);
                } else {
                    in_section = false;
                }
            } else if in_section
                && let Some(key) = subkey
                && is_key_assignment(trimmed, key)
            {
                return Some(idx + 1);
            }
        }
        return section_line;
    }
    for (idx, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            break;
        }
        if is_key_assignment(trimmed, field) {
            return Some(idx + 1);
        }
    }
    None
}

fn is_key_assignment(line: &str, key: &str) -> bool {
    let Some(rest) = line.strip_prefix(key) else {
        return false;
    };
    rest.trim_start().starts_with('=')
}

#[cfg(test)]
mod tests {
    use legion_core::schema::problems;

    use super::*;

    fn none_builtin(_name: &str) -> bool {
        false
    }

    const SAMPLE: &str = r#"
name = "gpu.free"
description = "Free video memory in MiB, per GPU."
effect = "read"
run = ["nvidia-smi", "--query-gpu=memory.free", "--format=csv,noheader,nounits"]
timeout_s = 30
cwd = "~"

[params.index]
type = "integer"
description = "Which GPU; all when omitted."
required = false
default = 0

[params.unit]
type = "string"
description = "Unit to report."
enum = ["mib", "gib"]
default = "mib"
"#;

    #[test]
    fn parses_sample_definition_and_round_trips_toml() {
        let def = parse_toml(SAMPLE.as_bytes(), &none_builtin).unwrap();
        assert_eq!(def.name, "gpu.free");
        assert_eq!(def.effect, Effect::Read);
        assert_eq!(def.timeout_s, Some(TimeoutSeconds(30.0)));
        assert_eq!(def.cwd.as_deref(), Some("~"));
        assert_eq!(def.params.len(), 2);
        assert_eq!(def.params["index"].kind, ParamType::Integer);
        assert_eq!(def.params["index"].default, Some(ParamValue::Integer(0)));
        assert_eq!(
            def.params["unit"].enum_values,
            Some(vec!["mib".to_owned(), "gib".to_owned()])
        );

        let rendered = def.to_toml();
        let reparsed = parse_toml(rendered.as_bytes(), &none_builtin).unwrap();
        assert_eq!(reparsed, def);
    }

    #[test]
    fn rejects_invalid_names_reserved_families_and_builtins() {
        let base = |name: &str| {
            format!(
                "name = \"{name}\"\ndescription = \"Test.\"\neffect = \"read\"\nrun = [\"echo\"]\n"
            )
        };
        for bad in [
            "gpu", "Gpu.free", "gpu.Free", "gpu.a-b", "1.a", "gpu.", ".a",
        ] {
            let err = parse_toml(base(bad).as_bytes(), &none_builtin).unwrap_err();
            assert!(err.reason.contains("not a tool name"), "{bad}: {err:?}");
            assert_eq!(err.line, Some(1), "{bad}: {err:?}");
        }
        for family in RESERVED_FAMILIES {
            let name = format!("{family}.custom");
            let err = parse_toml(base(&name).as_bytes(), &none_builtin).unwrap_err();
            assert!(err.reason.contains("reserved"), "{name}: {err:?}");
            assert_eq!(err.line, Some(1));
        }
        let err = parse_toml(base("gpu.free").as_bytes(), &|n| n == "gpu.free").unwrap_err();
        assert!(err.reason.contains("built-in"), "{err:?}");
    }

    #[test]
    fn rejects_invalid_effects_and_unknown_fields_with_line_numbers() {
        let bad_effect =
            "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"root\"\nrun = [\"x\"]\n";
        let err = parse_toml(bad_effect.as_bytes(), &none_builtin).unwrap_err();
        assert_eq!(err.line, Some(3), "{err:?}");

        let unknown = "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\nrun = [\"x\"]\nextra = 1\n";
        let err = parse_toml(unknown.as_bytes(), &none_builtin).unwrap_err();
        assert_eq!(err.line, Some(5), "{err:?}");
        assert!(err.reason.contains("extra"), "{err:?}");

        for effect in ["read", "write", "exec", "destroy", "power", "admin"] {
            let ok = format!(
                "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"{effect}\"\ncmd = \"echo ok\"\n"
            );
            assert!(parse_toml(ok.as_bytes(), &none_builtin).is_ok(), "{effect}");
        }
    }

    #[test]
    fn requires_exactly_one_of_run_or_cmd_and_checks_timeout_and_cwd() {
        let neither = "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\n";
        assert!(parse_toml(neither.as_bytes(), &none_builtin).is_err());

        let both = "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\nrun = [\"a\"]\ncmd = \"b\"\n";
        assert!(parse_toml(both.as_bytes(), &none_builtin).is_err());

        let empty_run = "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\nrun = []\n";
        assert!(parse_toml(empty_run.as_bytes(), &none_builtin).is_err());

        let empty_prog =
            "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\nrun = [\"\"]\n";
        assert!(parse_toml(empty_prog.as_bytes(), &none_builtin).is_err());

        let empty_cmd =
            "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\ncmd = \"   \"\n";
        assert!(parse_toml(empty_cmd.as_bytes(), &none_builtin).is_err());

        for bad_timeout in ["0", "-5", "3601"] {
            let text = format!(
                "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\nrun = [\"x\"]\ntimeout_s = {bad_timeout}\n"
            );
            let err = parse_toml(text.as_bytes(), &none_builtin).unwrap_err();
            assert_eq!(err.line, Some(5), "{bad_timeout}: {err:?}");
        }

        let rel_cwd = "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\nrun = [\"x\"]\ncwd = \"relative/path\"\n";
        let err = parse_toml(rel_cwd.as_bytes(), &none_builtin).unwrap_err();
        assert_eq!(err.line, Some(5), "{err:?}");
        assert!(err.reason.contains("relative"), "{err:?}");
    }

    #[test]
    fn validates_param_types_defaults_and_enums() {
        let header =
            "name = \"gpu.free\"\ndescription = \"D.\"\neffect = \"read\"\nrun = [\"x\"]\n";

        let bad_int_default = format!("{header}[params.n]\ntype = \"integer\"\ndefault = 1.5\n");
        let err = parse_toml(bad_int_default.as_bytes(), &none_builtin).unwrap_err();
        assert!(err.reason.contains("default"), "{err:?}");

        let bad_int_str = format!("{header}[params.n]\ntype = \"integer\"\ndefault = \"0\"\n");
        assert!(parse_toml(bad_int_str.as_bytes(), &none_builtin).is_err());

        let ok_num_int = format!("{header}[params.x]\ntype = \"number\"\ndefault = 2\n");
        assert!(parse_toml(ok_num_int.as_bytes(), &none_builtin).is_ok());
        let ok_num_float = format!("{header}[params.x]\ntype = \"number\"\ndefault = 2.5\n");
        assert!(parse_toml(ok_num_float.as_bytes(), &none_builtin).is_ok());

        let bad_bool = format!("{header}[params.flag]\ntype = \"boolean\"\ndefault = \"true\"\n");
        assert!(parse_toml(bad_bool.as_bytes(), &none_builtin).is_err());

        let bad_enum_type =
            format!("{header}[params.n]\ntype = \"integer\"\nenum = [\"1\", \"2\"]\n");
        assert!(parse_toml(bad_enum_type.as_bytes(), &none_builtin).is_err());

        let empty_enum = format!("{header}[params.s]\ntype = \"string\"\nenum = []\n");
        assert!(parse_toml(empty_enum.as_bytes(), &none_builtin).is_err());

        let dup_enum = format!("{header}[params.s]\ntype = \"string\"\nenum = [\"a\", \"a\"]\n");
        assert!(parse_toml(dup_enum.as_bytes(), &none_builtin).is_err());

        let bad_enum_default = format!(
            "{header}[params.s]\ntype = \"string\"\nenum = [\"a\", \"b\"]\ndefault = \"c\"\n"
        );
        let err = parse_toml(bad_enum_default.as_bytes(), &none_builtin).unwrap_err();
        assert!(err.reason.contains("enum"), "{err:?}");

        let bad_name = format!("{header}[params.BadName]\ntype = \"string\"\n");
        assert!(parse_toml(bad_name.as_bytes(), &none_builtin).is_err());
    }

    #[test]
    fn generated_schema_passes_strict_checks() {
        let def = parse_toml(SAMPLE.as_bytes(), &none_builtin).unwrap();
        let schema = def.input_schema();
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema.get("required"), None);
        assert_eq!(schema["properties"]["index"]["type"], json!("integer"));
        assert_eq!(schema["properties"]["index"]["default"], json!(0));
        assert_eq!(schema["properties"]["unit"]["enum"], json!(["mib", "gib"]));
        assert_eq!(problems(&schema), Vec::<String>::new());

        let with_required = "name = \"gpu.set\"\ndescription = \"Set.\"\neffect = \"write\"\nrun = [\"x\"]\n[params.mode]\ntype = \"string\"\nrequired = true\n";
        let def = parse_toml(with_required.as_bytes(), &none_builtin).unwrap();
        let schema = def.input_schema();
        assert_eq!(schema["required"], json!(["mode"]));
        assert_eq!(problems(&schema), Vec::<String>::new());
    }

    #[test]
    fn validates_runtime_arguments_and_maps_env_vars() {
        let def = parse_toml(
            r#"
name = "demo.check"
description = "Check."
effect = "read"
run = ["echo"]

[params.name]
type = "string"
required = true

[params.count]
type = "integer"
default = 3

[params.ratio]
type = "number"

[params.dry_run]
type = "boolean"
default = false

[params.mode]
type = "string"
enum = ["fast", "safe"]
"#
            .as_bytes(),
            &none_builtin,
        )
        .unwrap();

        let resolved = def
            .validate_args(json!({"name": "a b; $HOME", "ratio": 1.5, "mode": "fast"}))
            .unwrap();
        assert_eq!(
            Value::Object(resolved.clone()),
            json!({
                "count": 3,
                "dry_run": false,
                "mode": "fast",
                "name": "a b; $HOME",
                "ratio": 1.5,
            })
        );
        let env: BTreeMap<String, String> =
            ExtensionDef::arg_env_vars(&resolved).into_iter().collect();
        assert_eq!(env["LEGION_ARG_NAME"], "a b; $HOME");
        assert_eq!(env["LEGION_ARG_COUNT"], "3");
        assert_eq!(env["LEGION_ARG_RATIO"], "1.5");
        assert_eq!(env["LEGION_ARG_DRY_RUN"], "false");
        assert_eq!(env["LEGION_ARG_MODE"], "fast");

        let err = def
            .validate_args(json!({"name": "ok", "extra": 1}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("`extra`"), "{}", err.message);

        let err = def.validate_args(json!({})).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("`name`"), "{}", err.message);

        let err = def
            .validate_args(json!({"name": "ok", "count": 1.5}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        let err = def
            .validate_args(json!({"name": "ok", "mode": "turbo"}))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("fast, safe"), "{}", err.message);

        let err = def.validate_args(json!({"name": "bad\0val"})).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
    }

    #[test]
    fn enforces_env_arg_size_cap_and_large_stdin_policy() {
        let bad_non_string = r#"
name = "demo.bad"
description = "Bad."
effect = "read"
run = ["echo"]

[params.n]
type = "integer"
large = "stdin"
"#;
        let err = parse_toml(bad_non_string.as_bytes(), &none_builtin).unwrap_err();
        assert!(err.reason.contains("large"), "{err:?}");

        let def = parse_toml(
            r#"
name = "demo.big"
description = "Big."
effect = "read"
run = ["echo"]

[params.a]
type = "string"

[params.b]
type = "string"
large = "stdin"

[params.c]
type = "string"

[params.d]
type = "string"

[params.e]
type = "string"
"#
            .as_bytes(),
            &none_builtin,
        )
        .unwrap();
        let reparsed = parse_toml(def.to_toml().as_bytes(), &none_builtin).unwrap();
        assert_eq!(reparsed, def);

        // Oversized default parameter is refused with invalid_args naming the parameter and stdin.
        let big = "x".repeat(MAX_ENV_ARG_BYTES + 1);
        let resolved = def.validate_args(json!({"a": big})).unwrap();
        let err = def.prepare_arg_env(&resolved).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("`a`"), "{}", err.message);
        assert!(err.message.contains("stdin"), "{}", err.message);

        // Oversized parameter with large = "stdin" omits LEGION_ARG_B and sets LEGION_ARG_B_OMITTED=1.
        let resolved = def.validate_args(json!({"b": big})).unwrap();
        let env: BTreeMap<String, String> = def
            .prepare_arg_env(&resolved)
            .unwrap()
            .into_iter()
            .collect();
        assert!(!env.contains_key("LEGION_ARG_B"));
        assert_eq!(
            env.get("LEGION_ARG_B_OMITTED").map(String::as_str),
            Some("1")
        );

        // Small value on a large = "stdin" parameter still passes via env normally.
        let resolved = def.validate_args(json!({"b": "small"})).unwrap();
        let env: BTreeMap<String, String> = def
            .prepare_arg_env(&resolved)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(env.get("LEGION_ARG_B").map(String::as_str), Some("small"));
        assert!(!env.contains_key("LEGION_ARG_B_OMITTED"));

        // Combined total across multiple under-cap params exceeds MAX_ENV_TOTAL_BYTES -> refused.
        let chunk = "y".repeat(MAX_ENV_ARG_BYTES);
        let resolved = def
            .validate_args(json!({"a": chunk, "c": chunk, "d": chunk, "e": "z"}))
            .unwrap();
        let err = def.prepare_arg_env(&resolved).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("`e`"), "{}", err.message);
        assert!(err.message.contains("stdin"), "{}", err.message);
    }
}
