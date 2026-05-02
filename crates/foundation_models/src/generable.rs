// SPDX-License-Identifier: AGPL-3.0-only
//
// MCP-tool-schema → Swift `@Generable` translator (PDX-14).
//
// # What this is
//
// MCP servers advertise tools as `(name, description, inputSchema)` triples
// where `inputSchema` is a JSON Schema object. Apple's Foundation Models
// framework constrains a `LanguageModelSession` to a typed Swift struct
// via the `@Generable` macro. To bridge the two we have to translate JSON
// Schema → Swift `@Generable` types at runtime, since the set of MCP
// tools is not known at compile time (servers register tools dynamically).
//
// This module produces a Swift source string and a stable hash. The
// hash exists so PDX-15 can cache compiled Swift artefacts by content
// and avoid re-`swiftc`-ing the same source on every model call.
//
// # Why a separate module (not a separate crate)
//
// The recommendation in PDX-14 was Option A: live next to the FM bridge
// because `@Generable` is FM-specific. The translator is also small
// enough that a dedicated crate would mostly carry overhead. We can
// always extract it later if a non-FM consumer appears.
//
// # Why no `tokio`, no `rmcp`
//
// The translator is pure types + serde. Wiring directly to `rmcp::model::Tool`
// would couple this crate to the MCP transport layer (which pulls in
// async runtimes and HTTP) for no benefit — the only fields we need are
// `name`, `description`, and `inputSchema`. Callers that already have
// `rmcp::model::Tool` build a [`McpTool`] from it in one line; that
// conversion lives at the integration site (PDX-16), not here.
//
// # What we support today
//
// | JSON Schema type | Swift mapping                        |
// |------------------|--------------------------------------|
// | `string`         | `String`                             |
// | `integer`        | `Int`                                |
// | `number`         | `Double`                             |
// | `boolean`        | `Bool`                               |
// | `array`          | `[T]` (recursing into `items`)        |
// | `object`         | nested `@Generable` struct           |
// | `enum` (string)  | nested `@Generable enum` of cases    |
// | `required: []`   | drives `T` vs `T?` per property      |
//
// # What we deliberately don't support yet
//
// `anyOf`, `oneOf`, `allOf`, `not`, `$ref`, mixed-type arrays, integer
// enums, schema composition. Each of these surfaces as
// [`TranslationError::Unsupported`] with the specific keyword named, so
// PDX-15 can decide whether to skip the tool or translate by hand. The
// goal is not "translate everything" but "translate enough that real
// MCP tools (filesystem, github, sqlite) work".

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Minimal MCP tool descriptor.
///
/// This is intentionally decoupled from `rmcp::model::Tool` so the
/// translator stays a pure-types crate. Callers that already have an
/// `rmcp::model::Tool` (the orchestrator, the agent SDK) construct one
/// of these inline:
///
/// ```ignore
/// McpTool {
///     name: t.name.to_string(),
///     description: t.description.as_deref().unwrap_or_default().to_string(),
///     input_schema: serde_json::Value::Object((*t.input_schema).clone()),
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    /// Tool name as advertised by the MCP server. Must be a valid Swift
    /// identifier after the `to_camel_case` normalisation we apply
    /// (letters, digits, underscores; can't start with a digit).
    pub name: String,
    /// Human-readable description. Used as a Swift doc-comment on the
    /// generated struct so the model has the same context the server
    /// gave the protocol.
    pub description: String,
    /// JSON Schema object describing the tool's input parameters. The
    /// object's `type` field, if present, must be `"object"`.
    pub input_schema: Value,
}

/// Errors produced by [`translate_tools`] and [`translate_tool`].
#[derive(Debug, thiserror::Error)]
pub enum TranslationError {
    /// The tool's `name` is empty or normalises to an empty Swift
    /// identifier.
    #[error("tool {0:?} has invalid name (must yield a non-empty Swift identifier)")]
    InvalidName(String),
    /// The schema's top-level `type` field is missing, not a string, or
    /// not equal to `"object"`. Foundation Models structured-output
    /// expects the top-level to be a struct.
    #[error("tool {tool:?} input schema must be an object (got {got})")]
    NotAnObject {
        /// Name of the offending tool.
        tool: String,
        /// What we found instead of `"object"`.
        got: String,
    },
    /// The schema referenced a JSON Schema feature we don't translate.
    /// The string is a human-readable explanation including the field
    /// path, e.g. `"path/to/field uses anyOf"`.
    #[error("unsupported JSON Schema feature: {0}")]
    Unsupported(String),
    /// A type/typeof field was malformed (e.g. `type: 42`).
    #[error("malformed schema at {path}: {reason}")]
    Malformed {
        /// Dotted path inside the schema.
        path: String,
        /// What was wrong.
        reason: String,
    },
}

/// Output of a successful translation.
#[derive(Debug, Clone)]
pub struct GeneratedSwift {
    /// Swift source text. A single `// MARK:` block per tool, plus the
    /// top-level `ToolChoice` enum at the bottom.
    pub source: String,
    /// SHA-256 of `source`, lower-case hex. PDX-15's compile loop keys
    /// its on-disk cache off this hash so unchanged sets of tools don't
    /// trigger a recompile.
    pub hash: String,
}

/// Translate a slice of MCP tools into a single Swift source string with
/// per-tool `@Generable` structs and a top-level `@Generable` enum
/// `ToolChoice` that wraps every tool.
///
/// The order of `tools` is preserved — both in the order of struct
/// declarations and in the order of `ToolChoice` cases — so callers can
/// rely on stable hashing across runs given a stable input order.
///
/// An empty slice is an error: `ToolChoice` would have no variants and
/// Swift doesn't allow empty enums for `@Generable` (you'd get a
/// compile error inside Swift, not Rust). Callers should detect "no
/// tools" before reaching here.
pub fn translate_tools(tools: &[McpTool]) -> Result<GeneratedSwift, TranslationError> {
    if tools.is_empty() {
        return Err(TranslationError::Unsupported(
            "translate_tools called with zero tools (ToolChoice would be empty)".to_string(),
        ));
    }

    let mut source = String::new();
    writeln!(
        source,
        "// AUTOGENERATED by foundation_models::generable (PDX-14). DO NOT EDIT."
    )
    .unwrap();
    writeln!(source, "// Source: MCP tools/list response.").unwrap();
    writeln!(source).unwrap();
    writeln!(source, "import Foundation").unwrap();
    writeln!(source, "import FoundationModels").unwrap();
    writeln!(source).unwrap();

    // Per-tool struct declarations.
    let mut entries: Vec<ToolChoiceEntry> = Vec::with_capacity(tools.len());
    for tool in tools {
        let entry = translate_tool_inner(tool, &mut source)?;
        entries.push(entry);
    }

    // Top-level `ToolChoice` enum that the model picks from.
    writeln!(
        source,
        "/// Top-level structured output: which tool the model chose to call,"
    )
    .unwrap();
    writeln!(
        source,
        "/// with the typed parameters for that tool. PDX-15's chained execution"
    )
    .unwrap();
    writeln!(
        source,
        "/// loop dispatches on this enum to invoke the corresponding MCP tool."
    )
    .unwrap();
    writeln!(source, "@Generable").unwrap();
    writeln!(source, "public enum ToolChoice {{").unwrap();
    for entry in &entries {
        let doc = escape_doc(&entry.original_description);
        let doc_trim = doc.trim_end_matches('.').trim_end();
        writeln!(source, "    /// {}.", doc_trim).unwrap();
        writeln!(
            source,
            "    case {}({})",
            entry.case_name, entry.params_struct
        )
        .unwrap();
    }
    writeln!(source, "}}").unwrap();

    let hash = format!("{:x}", Sha256::digest(source.as_bytes()));
    Ok(GeneratedSwift { source, hash })
}

/// Translate a single tool. Mostly useful for tests; production callers
/// should use [`translate_tools`] so they get a `ToolChoice` enum too.
pub fn translate_tool(tool: &McpTool) -> Result<GeneratedSwift, TranslationError> {
    let mut source = String::new();
    writeln!(source, "import Foundation").unwrap();
    writeln!(source, "import FoundationModels").unwrap();
    writeln!(source).unwrap();
    let _entry = translate_tool_inner(tool, &mut source)?;
    let hash = format!("{:x}", Sha256::digest(source.as_bytes()));
    Ok(GeneratedSwift { source, hash })
}

/// Internal: translate one tool, append struct decls to `out`, return the
/// metadata needed to add the tool to `ToolChoice`.
fn translate_tool_inner(
    tool: &McpTool,
    out: &mut String,
) -> Result<ToolChoiceEntry, TranslationError> {
    let case_name = to_lower_camel(&tool.name);
    let struct_name = format!("{}Params", to_upper_camel(&tool.name));
    if case_name.is_empty() || struct_name == "Params" {
        return Err(TranslationError::InvalidName(tool.name.clone()));
    }

    // Validate top-level shape.
    let schema = match &tool.input_schema {
        Value::Object(_) => &tool.input_schema,
        other => {
            return Err(TranslationError::NotAnObject {
                tool: tool.name.clone(),
                got: type_label(other).to_string(),
            });
        }
    };
    let ty = schema_type(schema, &tool.name)?;
    if ty != "object" {
        return Err(TranslationError::NotAnObject {
            tool: tool.name.clone(),
            got: ty.to_string(),
        });
    }

    // Doc comment for the struct mirroring the tool description.
    writeln!(out, "// MARK: - {}", struct_name).unwrap();
    if !tool.description.is_empty() {
        for line in tool.description.lines() {
            writeln!(out, "/// {}", escape_doc(line)).unwrap();
        }
    }
    emit_object_struct(&struct_name, schema, &tool.name, "", out)?;
    writeln!(out).unwrap();

    Ok(ToolChoiceEntry {
        case_name,
        params_struct: struct_name,
        original_description: tool.description.clone(),
    })
}

struct ToolChoiceEntry {
    case_name: String,
    params_struct: String,
    original_description: String,
}

/// Emit a `@Generable struct <name>` declaration corresponding to a
/// JSON-Schema object. Nested objects are emitted as nested types inside
/// the struct.
fn emit_object_struct(
    name: &str,
    schema: &Value,
    tool_name: &str,
    path: &str,
    out: &mut String,
) -> Result<(), TranslationError> {
    let obj = schema.as_object().ok_or_else(|| TranslationError::Malformed {
        path: path.to_string(),
        reason: "expected object".to_string(),
    })?;

    reject_unsupported_keywords(obj, path)?;

    let properties = obj.get("properties").and_then(|v| v.as_object());
    let required: Vec<String> = obj
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    writeln!(out, "@Generable").unwrap();
    writeln!(out, "public struct {} {{", name).unwrap();

    // Sort property names for deterministic output. JSON object key order
    // is preserved by serde_json but we want hash-stable Swift regardless
    // of how the server serialised its schema.
    let sorted: BTreeMap<String, &Value> = properties
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    // Pre-emit nested types (objects/enums) before the field decls so
    // Swift's order-independent type resolution still keeps things tidy.
    let mut nested_decls = String::new();
    let mut field_decls = String::new();

    for (prop_name, prop_schema) in &sorted {
        let prop_path = if path.is_empty() {
            prop_name.clone()
        } else {
            format!("{}.{}", path, prop_name)
        };
        let swift_field = to_lower_camel(prop_name);
        if swift_field.is_empty() {
            return Err(TranslationError::Malformed {
                path: prop_path.clone(),
                reason: "property name normalises to empty Swift identifier".to_string(),
            });
        }

        let desc = prop_schema
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !desc.is_empty() {
            writeln!(field_decls, "    /// {}", escape_doc(desc)).unwrap();
        }

        let is_required = required.iter().any(|r| r == prop_name);
        let swift_ty = swift_type_for(
            prop_schema,
            tool_name,
            &prop_path,
            &to_upper_camel(prop_name),
            &mut nested_decls,
        )?;
        let suffix = if is_required { "" } else { "?" };
        writeln!(
            field_decls,
            "    public let {}: {}{}",
            swift_field, swift_ty, suffix
        )
        .unwrap();
    }

    if !nested_decls.is_empty() {
        // Indent nested decls one level so they read as inner types.
        for line in nested_decls.lines() {
            if line.is_empty() {
                writeln!(out).unwrap();
            } else {
                writeln!(out, "    {}", line).unwrap();
            }
        }
    }
    write!(out, "{}", field_decls).unwrap();
    writeln!(out, "}}").unwrap();
    Ok(())
}

/// Return the Swift type expression for a JSON Schema fragment. Emits
/// any nested type declarations into `nested_out`.
fn swift_type_for(
    schema: &Value,
    tool_name: &str,
    path: &str,
    suggested_type_name: &str,
    nested_out: &mut String,
) -> Result<String, TranslationError> {
    let obj = schema
        .as_object()
        .ok_or_else(|| TranslationError::Malformed {
            path: path.to_string(),
            reason: "expected schema object".to_string(),
        })?;
    reject_unsupported_keywords(obj, path)?;

    // String enum → nested `@Generable enum`.
    if let Some(values) = obj.get("enum").and_then(|v| v.as_array()) {
        // Confirm all entries are strings; integer / mixed enums are not
        // supported yet.
        let mut cases: Vec<String> = Vec::with_capacity(values.len());
        for v in values {
            match v.as_str() {
                Some(s) => cases.push(s.to_string()),
                None => {
                    return Err(TranslationError::Unsupported(format!(
                        "{} uses non-string enum (only string enums supported)",
                        path
                    )));
                }
            }
        }
        let enum_name = format!("{}Enum", suggested_type_name);
        writeln!(nested_out).unwrap();
        writeln!(nested_out, "@Generable").unwrap();
        writeln!(nested_out, "public enum {} {{", enum_name).unwrap();
        for case in &cases {
            let ident = to_lower_camel(case);
            if ident.is_empty() {
                return Err(TranslationError::Malformed {
                    path: path.to_string(),
                    reason: format!("enum case {:?} normalises to empty identifier", case),
                });
            }
            writeln!(nested_out, "    case {}", ident).unwrap();
        }
        writeln!(nested_out, "}}").unwrap();
        return Ok(enum_name);
    }

    let ty = schema_type(schema, tool_name)?;
    Ok(match ty {
        "string" => "String".to_string(),
        "integer" => "Int".to_string(),
        "number" => "Double".to_string(),
        "boolean" => "Bool".to_string(),
        "array" => {
            let items = obj
                .get("items")
                .ok_or_else(|| TranslationError::Malformed {
                    path: path.to_string(),
                    reason: "array schema missing `items`".to_string(),
                })?;
            // Mixed-type arrays (`items` as JSON array) aren't supported.
            if items.is_array() {
                return Err(TranslationError::Unsupported(format!(
                    "{} uses tuple-style `items` array (only single-type items supported)",
                    path
                )));
            }
            let inner_path = format!("{}[]", path);
            let inner_ty = swift_type_for(
                items,
                tool_name,
                &inner_path,
                &format!("{}Item", suggested_type_name),
                nested_out,
            )?;
            format!("[{}]", inner_ty)
        }
        "object" => {
            // Nested object → emit a nested struct, return its name.
            let nested_name = format!("{}Object", suggested_type_name);
            writeln!(nested_out).unwrap();
            emit_object_struct(&nested_name, schema, tool_name, path, nested_out)?;
            nested_name
        }
        other => {
            return Err(TranslationError::Unsupported(format!(
                "{} uses unsupported type {:?}",
                path, other
            )));
        }
    })
}

/// Pull the `type` field from a schema object, surfacing the few
/// degenerate cases that show up in real MCP servers (missing,
/// non-string, array of types).
fn schema_type<'a>(schema: &'a Value, _tool_name: &str) -> Result<&'a str, TranslationError> {
    let obj = schema
        .as_object()
        .ok_or_else(|| TranslationError::Malformed {
            path: "<root>".to_string(),
            reason: "expected schema object".to_string(),
        })?;
    match obj.get("type") {
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(Value::Array(_)) => Err(TranslationError::Unsupported(
            "schema uses array-valued `type` (e.g. [\"string\", \"null\"])".to_string(),
        )),
        Some(other) => Err(TranslationError::Malformed {
            path: "type".to_string(),
            reason: format!("expected string, got {}", type_label(other)),
        }),
        None => {
            // Empty object schema → treat as a generic object struct so
            // the model can produce any JSON. We name it `object` so the
            // caller emits a struct.
            if obj.contains_key("properties") {
                Ok("object")
            } else {
                Err(TranslationError::Unsupported(
                    "schema missing `type` field".to_string(),
                ))
            }
        }
    }
}

/// Reject the JSON Schema keywords we explicitly don't translate yet.
/// Returning a typed `Unsupported` error with the offending keyword
/// lets PDX-15 decide whether to skip the tool or hand-translate it.
fn reject_unsupported_keywords(
    obj: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), TranslationError> {
    // TODO(PDX-15): support `anyOf` / `oneOf` as Swift sum types when
    // FoundationModels supports them; for now we error so the chain
    // executor can fall back to a non-FM provider for these tools.
    for kw in ["anyOf", "oneOf", "allOf", "not", "$ref"] {
        if obj.contains_key(kw) {
            return Err(TranslationError::Unsupported(format!(
                "{} uses {}",
                if path.is_empty() { "<root>" } else { path },
                kw
            )));
        }
    }
    Ok(())
}

/// Human-readable JSON value type label for error messages.
fn type_label(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Convert an arbitrary string into a lowerCamelCase Swift identifier.
/// Leading digits get an underscore prefix; non-ASCII / non-alnum
/// characters become word boundaries.
fn to_lower_camel(s: &str) -> String {
    let upper = to_upper_camel(s);
    let mut chars = upper.chars();
    match chars.next() {
        Some(c) => {
            let mut out = String::with_capacity(upper.len());
            out.extend(c.to_lowercase());
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}

fn to_upper_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut new_word = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if new_word {
                for u in c.to_uppercase() {
                    out.push(u);
                }
            } else {
                out.push(c);
            }
            new_word = false;
        } else {
            // Any separator (space, dash, underscore, slash, dot, …)
            // starts a new word.
            new_word = true;
        }
    }
    // Swift identifiers can't start with a digit.
    if out.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        out.insert(0, '_');
    }
    out
}

/// Make a string safe to drop into a Swift `///` doc comment: collapse
/// newlines and strip the comment-closer just in case.
fn escape_doc(s: &str) -> String {
    s.replace('\n', " ").replace("*/", "* /")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_file_tool() -> McpTool {
        McpTool {
            name: "read_file".to_string(),
            description: "Read the contents of a file at the given path.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the file." },
                    "max_bytes": { "type": "integer", "description": "Optional cap." }
                },
                "required": ["path"]
            }),
        }
    }

    fn write_file_tool() -> McpTool {
        McpTool {
            name: "write-file".to_string(),
            description: "Write content to a file.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "mode": {
                        "type": "string",
                        "enum": ["overwrite", "append", "create_new"]
                    },
                    "make_parents": { "type": "boolean" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    /// Single tool with required + optional scalar fields produces a
    /// `@Generable struct` whose `required` fields are non-optional and
    /// the rest are `T?`.
    #[test]
    fn translates_simple_tool() {
        let out = translate_tool(&read_file_tool()).expect("ok");
        assert!(out.source.contains("@Generable"));
        assert!(out.source.contains("public struct ReadFileParams {"));
        // `path` is required → no `?`
        assert!(out.source.contains("public let path: String"));
        assert!(!out.source.contains("public let path: String?"));
        // `max_bytes` is optional → `Int?`
        assert!(out.source.contains("public let maxBytes: Int?"));
        // Property doc-comments are carried over.
        assert!(out.source.contains("/// Absolute path to the file."));
        // Hash is non-empty hex.
        assert_eq!(out.hash.len(), 64);
        assert!(out.hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// String enums produce a nested `@Generable enum` and the field
    /// references that enum by its generated name.
    #[test]
    fn translates_string_enum() {
        let out = translate_tool(&write_file_tool()).expect("ok");
        assert!(out.source.contains("public enum ModeEnum {"));
        assert!(out.source.contains("case overwrite"));
        assert!(out.source.contains("case append"));
        assert!(out.source.contains("case createNew"));
        assert!(out.source.contains("public let mode: ModeEnum?"));
    }

    /// All scalar JSON types map to their canonical Swift equivalents.
    #[test]
    fn translates_all_scalar_types() {
        let tool = McpTool {
            name: "scalars".to_string(),
            description: "all scalar types".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "s": { "type": "string" },
                    "i": { "type": "integer" },
                    "n": { "type": "number" },
                    "b": { "type": "boolean" }
                },
                "required": ["s", "i", "n", "b"]
            }),
        };
        let out = translate_tool(&tool).unwrap();
        assert!(out.source.contains("public let s: String"));
        assert!(out.source.contains("public let i: Int"));
        assert!(out.source.contains("public let n: Double"));
        assert!(out.source.contains("public let b: Bool"));
    }

    /// Arrays of scalars and arrays of objects both translate.
    #[test]
    fn translates_arrays() {
        let tool = McpTool {
            name: "list_things".to_string(),
            description: "lists".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "integer" },
                                "label": { "type": "string" }
                            },
                            "required": ["id"]
                        }
                    }
                },
                "required": ["tags"]
            }),
        };
        let out = translate_tool(&tool).unwrap();
        assert!(out.source.contains("public let tags: [String]"));
        // Nested object array element type is named `<Suggested>Item`.
        assert!(out.source.contains("public let items: [ItemsItemObject]?"));
        assert!(out.source.contains("public struct ItemsItemObject {"));
        assert!(out.source.contains("public let id: Int"));
        assert!(out.source.contains("public let label: String?"));
    }

    /// `translate_tools` produces a top-level `ToolChoice` enum
    /// dispatching to each tool's params struct, in input order.
    #[test]
    fn produces_tool_choice_enum() {
        let out = translate_tools(&[read_file_tool(), write_file_tool()]).unwrap();
        assert!(out.source.contains("public enum ToolChoice {"));
        // Both tools represented; case order matches input order.
        let rf = out.source.find("case readFile(ReadFileParams)").expect("read case");
        let wf = out.source.find("case writeFile(WriteFileParams)").expect("write case");
        assert!(rf < wf, "ToolChoice cases must follow input order");
        // Hash is deterministic.
        let again = translate_tools(&[read_file_tool(), write_file_tool()]).unwrap();
        assert_eq!(out.hash, again.hash);
    }

    /// `anyOf` etc. produce a typed `Unsupported` error with the
    /// offending keyword in the message — that's what PDX-15 keys off
    /// to decide whether to skip the tool.
    #[test]
    fn rejects_anyof_with_typed_error() {
        let tool = McpTool {
            name: "polymorphic".to_string(),
            description: "anyOf isn't supported yet".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "value": { "anyOf": [{ "type": "string" }, { "type": "integer" }] }
                },
                "required": ["value"]
            }),
        };
        let err = translate_tool(&tool).unwrap_err();
        match err {
            TranslationError::Unsupported(msg) => {
                assert!(msg.contains("anyOf"), "message: {}", msg);
            }
            other => panic!("expected Unsupported(anyOf), got {:?}", other),
        }
    }

    /// Non-object top-level schemas are rejected with `NotAnObject`.
    #[test]
    fn rejects_non_object_top_level() {
        let tool = McpTool {
            name: "bad".to_string(),
            description: "scalar at top level".to_string(),
            input_schema: json!({ "type": "string" }),
        };
        let err = translate_tool(&tool).unwrap_err();
        assert!(matches!(err, TranslationError::NotAnObject { .. }));
    }

    /// Empty tool list is an error — Swift can't have an empty
    /// `@Generable enum`.
    #[test]
    fn rejects_empty_tool_list() {
        let err = translate_tools(&[]).unwrap_err();
        assert!(matches!(err, TranslationError::Unsupported(_)));
    }

    /// Mixed-type enums (e.g. `["a", 1]`) are unsupported — call out
    /// the path so PDX-15's logs are useful.
    #[test]
    fn rejects_non_string_enum() {
        let tool = McpTool {
            name: "mixed".to_string(),
            description: "x".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "enum": ["a", 1] }
                },
                "required": ["kind"]
            }),
        };
        let err = translate_tool(&tool).unwrap_err();
        match err {
            TranslationError::Unsupported(msg) => assert!(msg.contains("non-string enum")),
            other => panic!("expected Unsupported, got {:?}", other),
        }
    }

    /// `to_upper_camel` handles all the separator styles we expect from
    /// MCP tool names in the wild.
    #[test]
    fn upper_camel_handles_separators() {
        assert_eq!(to_upper_camel("read_file"), "ReadFile");
        assert_eq!(to_upper_camel("read-file"), "ReadFile");
        assert_eq!(to_upper_camel("read file"), "ReadFile");
        assert_eq!(to_upper_camel("read.file"), "ReadFile");
        assert_eq!(to_upper_camel("readFile"), "ReadFile");
        assert_eq!(to_upper_camel("123start"), "_123start");
    }

    /// Lower-camel mirrors upper-camel with first letter lowered.
    #[test]
    fn lower_camel_basics() {
        assert_eq!(to_lower_camel("read_file"), "readFile");
        assert_eq!(to_lower_camel("WriteFile"), "writeFile");
        assert_eq!(to_lower_camel("create_new"), "createNew");
    }

    /// Visual smoke check: the full generated source for two real-ish
    /// tools includes all the expected headers, structs, enums, and
    /// `ToolChoice` cases. Run with `-- --nocapture` to eyeball the
    /// Swift directly.
    #[test]
    fn full_source_smoke() {
        let out = translate_tools(&[read_file_tool(), write_file_tool()]).unwrap();
        // Header.
        assert!(out.source.starts_with("// AUTOGENERATED"));
        assert!(out.source.contains("import FoundationModels"));
        // Per-tool struct + ToolChoice enum present.
        assert!(out.source.contains("public struct ReadFileParams {"));
        assert!(out.source.contains("public struct WriteFileParams {"));
        assert!(out.source.contains("public enum ToolChoice {"));
        // ToolChoice mirrors the input order and uses the right param types.
        assert!(out
            .source
            .contains("    case readFile(ReadFileParams)\n    "));
        assert!(out.source.contains("    case writeFile(WriteFileParams)\n}"));
        eprintln!("--- generated swift ---\n{}\n--- hash: {} ---", out.source, out.hash);
    }

    /// Determinism: re-running on the same input yields byte-for-byte
    /// the same source (and hash). This is what the FM compile cache
    /// keys on.
    #[test]
    fn output_is_deterministic() {
        let a = translate_tools(&[read_file_tool(), write_file_tool()]).unwrap();
        let b = translate_tools(&[read_file_tool(), write_file_tool()]).unwrap();
        assert_eq!(a.source, b.source);
        assert_eq!(a.hash, b.hash);
    }

    /// Property order inside a struct is sorted alphabetically — same
    /// inputs in different JSON property orders produce the same
    /// output. (We rely on this to avoid spurious cache misses when
    /// upstream MCP servers reshuffle their schemas.)
    #[test]
    fn properties_sort_alphabetically() {
        let a = McpTool {
            name: "x".to_string(),
            description: "".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "zebra": { "type": "string" },
                    "alpha": { "type": "integer" }
                },
                "required": ["alpha", "zebra"]
            }),
        };
        let out = translate_tool(&a).unwrap();
        let alpha_pos = out.source.find("public let alpha:").unwrap();
        let zebra_pos = out.source.find("public let zebra:").unwrap();
        assert!(alpha_pos < zebra_pos, "properties must be sorted");
    }
}
