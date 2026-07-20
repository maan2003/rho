use schemars::r#gen::SchemaSettings;
use serde_json::{Map, Value};

use crate::search::SearchCommands;

/// Generates the same draft, option handling, inlining, and retained root keys
/// as Codex's standalone web-search extension.
pub(crate) fn commands_schema() -> Value {
    let schema = SchemaSettings::draft2019_09()
        .with(|settings| {
            settings.inline_subschemas = true;
            settings.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<SearchCommands>();
    let Value::Object(mut schema) =
        serde_json::to_value(schema).expect("search command schema should serialize")
    else {
        unreachable!("search commands schema must be an object");
    };

    let mut tool_schema = Map::new();
    for key in [
        "properties",
        "required",
        "type",
        "additionalProperties",
        "$defs",
        "definitions",
    ] {
        if let Some(value) = schema.remove(key) {
            tool_schema.insert(key.to_owned(), value);
        }
    }
    let mut schema = Value::Object(tool_schema);
    // Codex passes the generated value through its typed tool-schema parser.
    // That representation intentionally retains descriptions but has no
    // fields for numeric `format` or bounds, so they are absent on the wire.
    strip_unsupported_metadata(&mut schema);
    schema
}

fn strip_unsupported_metadata(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                strip_unsupported_metadata(value);
            }
        }
        Value::Object(map) => {
            for key in [
                "format",
                "minimum",
                "maximum",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "multipleOf",
            ] {
                map.remove(key);
            }
            for value in map.values_mut() {
                strip_unsupported_metadata(value);
            }
        }
        _ => {}
    }
}
