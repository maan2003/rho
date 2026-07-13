//! Model-facing `exec`/`wait` tool descriptions, the exec source pragma, and
//! JSON-schema-to-TypeScript rendering for nested tool signatures.
//!
//! The description surface and rendering are ported from OpenAI Codex's
//! `codex-code-mode-protocol` crate (Apache-2.0), with the state-contract
//! lines rewritten: rho cells share one persistent V8 REPL scope (notebook
//! semantics) instead of Codex's fresh-isolate-per-cell plus `store`/`load`.

use rho_core::{ToolFormat, ToolGrammarSyntax, ToolName, ToolSpec, ToolType};
use serde_json::Value as JsonValue;

pub const EXEC_TOOL_NAME: &str = "exec";
pub const WAIT_TOOL_NAME: &str = "wait";
pub const CODE_MODE_PRAGMA_PREFIX: &str = "// @exec:";

pub const DEFAULT_YIELD_TIME_MS: u64 = 10_000;
pub const MIN_WAIT_YIELD_TIME_MS: u64 = 30_000;
pub const DEFAULT_WAIT_YIELD_TIME_MS: u64 = 30_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;

const MAX_JS_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;

const EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run JavaScript code to orchestrate/compose tool calls
- Evaluates the provided JavaScript in this session's persistent JavaScript environment (V8 REPL mode).
- Top-level variables, functions, and classes persist across `exec` calls in this session. Redeclaring `let`/`const` in a later `exec` call is allowed.
- All nested tools are available on the global `tools` object, for example `await tools.shell_command(...)`. Tool names are exposed as normalized JavaScript identifiers.
- Nested tool methods take either a string or an object as their input argument and return the documented JavaScript value. Command tools return structured objects; text-only tools return strings.
- Runs raw JavaScript -- no Node, no file system, no network access, no console. All host access goes through the nested tools.
- Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.
- You may optionally start the tool input with a first-line pragma like `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 1000}`.
- `yield_time_ms` asks `exec` to yield early if the script is still running. Defaults to 10000 ms.
- `max_output_tokens` sets the token budget for direct `exec` results. Defaults to 10000 tokens.
- A script that fails or is terminated may have applied only part of its changes to shared session state.
- Unawaited promises are not tracked; `await` everything whose result you need.

- Global helpers:
- `exit()`: Immediately ends the current script successfully (like an early return from the top level).
- `text(value: string | number | boolean | undefined | null)`: Appends a text item to this script's output. Non-string values are stringified with `JSON.stringify(...)` when possible.
- `notify(value: string | number | boolean | undefined | null)`: immediately injects an extra `custom_tool_call_output` for the current `exec` call. Values are stringified like `text(...)`.
- `setTimeout(callback: () => void, delayMs?: number)`: schedules a callback to run later and returns a timeout id. Pending timeouts do not keep `exec` alive by themselves; await an explicit promise if you need to wait for one.
- `clearTimeout(timeoutId?: number)`: cancels a timeout created by `setTimeout`.
- `ALL_TOOLS`: metadata for the enabled nested tools as `{ name, description }` entries.
- `yield_control()`: yields the accumulated output to the model immediately while the script keeps running."#;

const WAIT_DESCRIPTION_TEMPLATE: &str = r#"- Use `wait` only after `exec` returns `Script running with cell ID ...`.
- `cell_id` identifies the running `exec` cell to resume.
- `yield_time_ms` controls how long to wait for more output before yielding again. Use at least 30000 ms and prefer a longer wait over repeated short waits. Defaults to 30000 ms.
- `max_tokens` limits how much new output this wait call returns. Defaults to 10000 tokens.
- `terminate: true` stops the running cell; false or omitted waits for output.
- `wait` returns only the new output since the last yield, or the final completion or termination result for that cell.
- If the cell is still running, `wait` may yield again with the same `cell_id`.
- If the cell has already finished, `wait` returns the completed result and closes the cell."#;

const EXEC_FREEFORM_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

/// One tool exposed to scripts on the `tools` object.
#[derive(Clone, Debug)]
pub struct NestedTool {
    pub name: ToolName,
    pub tool_type: ToolType,
    pub description: String,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
}

impl NestedTool {
    pub fn from_spec(spec: &ToolSpec) -> Self {
        let input_schema = match (&spec.tool_type, &spec.input_schema) {
            (ToolType::Custom, _) | (_, JsonValue::Null) => None,
            (ToolType::Function, schema) => Some(schema.clone()),
        };
        Self {
            name: spec.name.clone(),
            tool_type: spec.tool_type,
            description: spec.description.clone(),
            input_schema,
            output_schema: None,
        }
    }

    pub fn with_output_schema(mut self, schema: JsonValue) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// The identifier scripts use: `tools.<global_name>(...)`.
    pub fn global_name(&self) -> String {
        normalize_code_mode_identifier(self.name.as_str())
    }
}

pub fn exec_tool_spec(nested_tools: &[NestedTool]) -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(EXEC_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Custom,
        description: build_exec_tool_description(nested_tools),
        input_schema: JsonValue::Null,
        format: Some(ToolFormat::Grammar {
            syntax: ToolGrammarSyntax::Lark,
            definition: EXEC_FREEFORM_GRAMMAR.to_owned(),
        }),
    }
}

pub fn wait_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(WAIT_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: format!(
            "Waits on a yielded `{EXEC_TOOL_NAME}` cell and returns new output or completion.\n{}",
            WAIT_DESCRIPTION_TEMPLATE.trim()
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["cell_id"],
            "properties": {
                "cell_id": {
                    "type": "string",
                    "description": "Identifier of the running exec cell."
                },
                "yield_time_ms": {
                    "type": "number",
                    "minimum": MIN_WAIT_YIELD_TIME_MS,
                    "description": "Wait before yielding more output. Use at least 30000 ms and prefer a longer wait over repeated short waits. Defaults to 30000 ms."
                },
                "max_tokens": {
                    "type": "number",
                    "description": "Output token budget for this wait call. Defaults to 10000 tokens."
                },
                "terminate": {
                    "type": "boolean",
                    "description": "True stops the running exec cell; false or omitted waits for output."
                }
            }
        }),
        format: None,
    }
}

pub fn build_exec_tool_description(nested_tools: &[NestedTool]) -> String {
    let mut sections = vec![EXEC_DESCRIPTION_TEMPLATE.to_string()];
    if !nested_tools.is_empty() {
        let tool_sections = nested_tools
            .iter()
            .map(render_nested_tool_section)
            .collect::<Vec<_>>();
        sections.push(tool_sections.join("\n\n"));
    }
    sections.join("\n\n")
}

fn render_nested_tool_section(tool: &NestedTool) -> String {
    let global_name = tool.global_name();
    let raw_name = tool.name.as_str();
    let heading = if global_name == raw_name {
        format!("### `{global_name}`")
    } else {
        format!("### `{global_name}` (`{raw_name}`)")
    };
    let (input_name, input_type) = match tool.tool_type {
        ToolType::Function => (
            "args",
            tool.input_schema
                .as_ref()
                .map(render_json_schema_to_typescript)
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        ToolType::Custom => ("input", "string".to_string()),
    };
    let output_type = tool
        .output_schema
        .as_ref()
        .map(render_json_schema_to_typescript)
        .unwrap_or_else(|| "string".to_owned());
    let declaration = format!(
        "declare const tools: {{ {global_name}({input_name}: {input_type}): Promise<{output_type}>; }};"
    );
    let description = tool.description.trim();
    if description.is_empty() {
        format!("{heading}\nexec tool declaration:\n```ts\n{declaration}\n```")
    } else {
        format!("{heading}\n{description}\n\nexec tool declaration:\n```ts\n{declaration}\n```")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedExecSource {
    pub code: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
}

pub fn parse_exec_source(input: &str) -> Result<ParsedExecSource, String> {
    if input.trim().is_empty() {
        return Err(
            "exec expects raw JavaScript source text (non-empty). Provide JS only, optionally with first-line `// @exec: {\"yield_time_ms\": 10000, \"max_output_tokens\": 1000}`.".to_string(),
        );
    }

    let mut args = ParsedExecSource {
        code: input.to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
    };

    let mut lines = input.splitn(2, '\n');
    let first_line = lines.next().unwrap_or_default();
    let rest = lines.next().unwrap_or_default();
    let Some(pragma) = first_line
        .trim_start()
        .strip_prefix(CODE_MODE_PRAGMA_PREFIX)
    else {
        return Ok(args);
    };

    if rest.trim().is_empty() {
        return Err(
            "exec pragma must be followed by JavaScript source on subsequent lines".to_string(),
        );
    }

    let directive = pragma.trim();
    if directive.is_empty() {
        return Err(
            "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
                .to_string(),
        );
    }

    let value: JsonValue = serde_json::from_str(directive).map_err(|err| {
        format!(
            "exec pragma must be valid JSON with supported fields `yield_time_ms` and `max_output_tokens`: {err}"
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        "exec pragma must be a JSON object with supported fields `yield_time_ms` and `max_output_tokens`"
            .to_string()
    })?;
    for key in object.keys() {
        if key != "yield_time_ms" && key != "max_output_tokens" {
            return Err(format!(
                "exec pragma only supports `yield_time_ms` and `max_output_tokens`; got `{key}`"
            ));
        }
    }

    let field_error = "exec pragma fields `yield_time_ms` and `max_output_tokens` must be non-negative safe integers";
    let read_field = |name: &str| -> Result<Option<u64>, String> {
        match object.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .filter(|value| *value <= MAX_JS_SAFE_INTEGER)
                .map(Some)
                .ok_or_else(|| field_error.to_string()),
        }
    };
    args.yield_time_ms = read_field("yield_time_ms")?;
    args.max_output_tokens = read_field("max_output_tokens")?.map(|value| value as usize);
    args.code = rest.to_string();
    Ok(args)
}

pub fn normalize_code_mode_identifier(tool_key: &str) -> String {
    let mut identifier = String::new();
    for (index, ch) in tool_key.chars().enumerate() {
        let is_valid = if index == 0 {
            ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
        } else {
            ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
        };
        identifier.push(if is_valid { ch } else { '_' });
    }
    if identifier.is_empty() {
        "_".to_string()
    } else {
        identifier
    }
}

pub fn render_json_schema_to_typescript(schema: &JsonValue) -> String {
    match schema {
        JsonValue::Bool(true) => "unknown".to_string(),
        JsonValue::Bool(false) => "never".to_string(),
        JsonValue::Object(map) => {
            if let Some(value) = map.get("const") {
                return render_json_schema_literal(value);
            }

            if let Some(values) = map.get("enum").and_then(JsonValue::as_array) {
                let rendered = values
                    .iter()
                    .map(render_json_schema_literal)
                    .collect::<Vec<_>>();
                if !rendered.is_empty() {
                    return rendered.join(" | ");
                }
            }

            for key in ["anyOf", "oneOf"] {
                if let Some(variants) = map.get(key).and_then(JsonValue::as_array) {
                    let rendered = variants
                        .iter()
                        .map(render_json_schema_to_typescript)
                        .collect::<Vec<_>>();
                    if !rendered.is_empty() {
                        return rendered.join(" | ");
                    }
                }
            }

            if let Some(variants) = map.get("allOf").and_then(JsonValue::as_array) {
                let rendered = variants
                    .iter()
                    .map(render_json_schema_to_typescript)
                    .collect::<Vec<_>>();
                if !rendered.is_empty() {
                    return rendered.join(" & ");
                }
            }

            if let Some(schema_type) = map.get("type") {
                if let Some(types) = schema_type.as_array() {
                    let rendered = types
                        .iter()
                        .filter_map(JsonValue::as_str)
                        .map(|schema_type| render_json_schema_type_keyword(map, schema_type))
                        .collect::<Vec<_>>();
                    if !rendered.is_empty() {
                        return rendered.join(" | ");
                    }
                }
                if let Some(schema_type) = schema_type.as_str() {
                    return render_json_schema_type_keyword(map, schema_type);
                }
            }

            if map.contains_key("properties")
                || map.contains_key("additionalProperties")
                || map.contains_key("required")
            {
                return render_json_schema_object(map);
            }

            if map.contains_key("items") || map.contains_key("prefixItems") {
                return render_json_schema_array(map);
            }

            "unknown".to_string()
        }
        _ => "unknown".to_string(),
    }
}

fn render_json_schema_type_keyword(
    map: &serde_json::Map<String, JsonValue>,
    schema_type: &str,
) -> String {
    match schema_type {
        "string" => "string".to_string(),
        "number" | "integer" => "number".to_string(),
        "boolean" => "boolean".to_string(),
        "null" => "null".to_string(),
        "array" => render_json_schema_array(map),
        "object" => render_json_schema_object(map),
        _ => "unknown".to_string(),
    }
}

fn render_json_schema_array(map: &serde_json::Map<String, JsonValue>) -> String {
    if let Some(items) = map.get("items") {
        let item_type = render_json_schema_to_typescript(items);
        return format!("Array<{item_type}>");
    }

    if let Some(items) = map.get("prefixItems").and_then(JsonValue::as_array) {
        let item_types = items
            .iter()
            .map(render_json_schema_to_typescript)
            .collect::<Vec<_>>();
        if !item_types.is_empty() {
            return format!("[{}]", item_types.join(", "));
        }
    }

    "unknown[]".to_string()
}

fn append_additional_properties_line(
    lines: &mut Vec<String>,
    map: &serde_json::Map<String, JsonValue>,
    properties: &serde_json::Map<String, JsonValue>,
    line_prefix: &str,
) {
    if let Some(additional_properties) = map.get("additionalProperties") {
        let property_type = match additional_properties {
            JsonValue::Bool(true) => Some("unknown".to_string()),
            JsonValue::Bool(false) => None,
            value => Some(render_json_schema_to_typescript(value)),
        };
        if let Some(property_type) = property_type {
            lines.push(format!("{line_prefix}[key: string]: {property_type};"));
        }
    } else if properties.is_empty() {
        lines.push(format!("{line_prefix}[key: string]: unknown;"));
    }
}

fn has_property_description(value: &JsonValue) -> bool {
    value
        .get("description")
        .and_then(JsonValue::as_str)
        .is_some_and(|description| !description.is_empty())
}

fn render_json_schema_object_property(name: &str, value: &JsonValue, required: &[&str]) -> String {
    let optional = if required.contains(&name) { "" } else { "?" };
    let property_name = render_json_schema_property_name(name);
    let property_type = render_json_schema_to_typescript(value);
    format!("{property_name}{optional}: {property_type};")
}

fn render_json_schema_object(map: &serde_json::Map<String, JsonValue>) -> String {
    let required = map
        .get("required")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let properties = map
        .get("properties")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();

    let mut sorted_properties = properties.iter().collect::<Vec<_>>();
    sorted_properties.sort_unstable_by_key(|(name, _)| *name);

    if sorted_properties
        .iter()
        .any(|(_, value)| has_property_description(value))
    {
        let mut lines = vec!["{".to_string()];
        for (name, value) in sorted_properties {
            if let Some(description) = value.get("description").and_then(JsonValue::as_str) {
                for description_line in description
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                {
                    lines.push(format!("  // {description_line}"));
                }
            }
            lines.push(format!(
                "  {}",
                render_json_schema_object_property(name, value, &required)
            ));
        }
        append_additional_properties_line(&mut lines, map, &properties, "  ");
        lines.push("}".to_string());
        return lines.join("\n");
    }

    let mut lines = sorted_properties
        .into_iter()
        .map(|(name, value)| render_json_schema_object_property(name, value, &required))
        .collect::<Vec<_>>();
    append_additional_properties_line(&mut lines, map, &properties, "");

    if lines.is_empty() {
        return "{}".to_string();
    }
    format!("{{ {} }}", lines.join(" "))
}

fn render_json_schema_property_name(name: &str) -> String {
    if normalize_code_mode_identifier(name) == name {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| format!("\"{}\"", name.replace('"', "\\\"")))
    }
}

fn render_json_schema_literal(value: &JsonValue) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn wait_encourages_long_yields_with_a_thirty_second_minimum() {
        let spec = wait_tool_spec();
        assert!(spec.description.contains("prefer a longer wait"));
        assert!(spec.description.contains("Defaults to 30000 ms"));
        assert_eq!(
            spec.input_schema["properties"]["yield_time_ms"]["minimum"],
            MIN_WAIT_YIELD_TIME_MS
        );
        assert_eq!(DEFAULT_WAIT_YIELD_TIME_MS, MIN_WAIT_YIELD_TIME_MS);
    }

    #[test]
    fn pragma_parses_and_strips_first_line() {
        let parsed = parse_exec_source(
            "// @exec: {\"yield_time_ms\": 500, \"max_output_tokens\": 42}\ntext('hi')",
        )
        .unwrap();
        assert_eq!(parsed.code, "text('hi')");
        assert_eq!(parsed.yield_time_ms, Some(500));
        assert_eq!(parsed.max_output_tokens, Some(42));
    }

    #[test]
    fn plain_source_passes_through() {
        let parsed = parse_exec_source("let a = 1;\ntext(a)").unwrap();
        assert_eq!(parsed.code, "let a = 1;\ntext(a)");
        assert_eq!(parsed.yield_time_ms, None);
    }

    #[test]
    fn pragma_rejects_unknown_fields() {
        let err = parse_exec_source("// @exec: {\"nope\": 1}\ntext('hi')").unwrap_err();
        assert!(err.contains("only supports"), "{err}");
    }

    #[test]
    fn empty_source_is_rejected() {
        assert!(parse_exec_source("  \n ").is_err());
    }

    #[test]
    fn identifier_normalization_replaces_invalid_chars() {
        assert_eq!(
            normalize_code_mode_identifier("mcp/tool.name"),
            "mcp_tool_name"
        );
        assert_eq!(normalize_code_mode_identifier("9lives"), "_lives");
        assert_eq!(normalize_code_mode_identifier(""), "_");
    }

    #[test]
    fn schema_renders_object_with_descriptions_as_comments() {
        let schema = json!({
            "type": "object",
            "required": ["command"],
            "additionalProperties": false,
            "properties": {
                "command": { "type": "string", "description": "Command to run" },
                "timeout": { "type": "integer" }
            }
        });
        let rendered = render_json_schema_to_typescript(&schema);
        assert_eq!(
            rendered,
            "{\n  // Command to run\n  command: string;\n  timeout?: number;\n}"
        );
    }

    #[test]
    fn exec_description_includes_nested_tool_declarations() {
        let tool = NestedTool {
            name: ToolName::try_from("shell_command").unwrap(),
            tool_type: ToolType::Function,
            description: "Run a shell command.".to_string(),
            input_schema: Some(json!({
                "type": "object",
                "required": ["command"],
                "properties": { "command": { "type": "string" } }
            })),
            output_schema: None,
        };
        let description = build_exec_tool_description(&[tool]);
        assert!(description.contains("### `shell_command`"), "{description}");
        assert!(
            description.contains("shell_command(args: { command: string; }): Promise<string>;"),
            "{description}"
        );
    }
}
