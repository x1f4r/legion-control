//! Every tool's schema, as every door shows it, in the plain form strict clients accept.

#![allow(clippy::unwrap_used)]

use legion_core::schema::problems;
use serde_json::json;

#[test]
fn every_tool_schema_is_one_a_strict_client_accepts() {
    let registry = legion_tools::registry();
    let mut found = Vec::new();
    for spec in registry.specs() {
        if spec.input_schema["type"] != json!("object") {
            found.push(format!("{}: the schema is not an object schema", spec.name));
        }
        found.extend(
            problems(&spec.input_schema)
                .into_iter()
                .map(|problem| format!("{}: {problem}", spec.name)),
        );
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}
