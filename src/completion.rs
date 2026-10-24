use crate::rust_analyzer::{EnumVariant, FieldInfo, RustAnalyzer, TypeInfo, TypeKind};
use crate::tree_sitter_parser;
use std::sync::Arc;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Documentation, InsertTextFormat, MarkupContent, MarkupKind,
    Position,
};

#[derive(Debug, PartialEq)]
enum CompletionContext {
    FieldName,  // Completing field names (e.g., after comma or opening paren)
    FieldValue, // Completing values after colon
    StructType, // Completing struct type name for nested types
}

/// Field name on the current line when the cursor sits after its colon and no
/// value has been typed yet.
///
/// Tree-sitter cannot represent a field without a value: on `apply_rule:` it
/// swallows the next field name as the value, so the cursor is no longer inside
/// a `field` node and completion falls back to field names. The lexical check
/// keeps the value context tied to the field the user is typing.
fn empty_field_before_cursor(content: &str, position: Position) -> Option<String> {
    let line = content.lines().nth(position.line as usize)?;
    let byte_offset = crate::ts_utils::line_column_to_byte_offset(line, position.character);
    let prefix = line.get(..byte_offset)?;
    let trimmed = prefix.trim_start();
    let (name, rest) = trimmed.split_once(':')?;
    if !rest.trim().is_empty() {
        return None;
    }
    let name = name.trim_end();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_string())
}

/// Whether the cursor sits where a named field may start: after a comma or an
/// opening paren of a struct, with only whitespace between it and the cursor.
///
/// The tree cannot express a field without a name: a trailing comma makes the
/// previous field (`falloff: "0.5",`) the node under the cursor, and a path built
/// from that node would point at the previous field's value. Named structs are
/// told apart from arrays, maps and tuples by the nearest enclosing container.
pub(crate) fn is_field_name_position(content: &str, position: Position) -> bool {
    use crate::ts_utils::{self, RonParser};

    if cursor_inside_string(content, position) {
        return false;
    }

    let before = content
        .get(..ts_utils::position_to_byte_offset(content, position))
        .unwrap_or_default()
        .trim_end();
    if !before.ends_with(',') && !before.ends_with('(') {
        return false;
    }

    let mut parser = RonParser::new();
    let Some(tree) = parser.parse(content) else {
        return false;
    };
    let Some(node) = ts_utils::node_at_position(&tree, content, position) else {
        return false;
    };

    // The nearest value container decides: an array has no named fields; a bare
    // record is recognized by its field names (`projectile: (key: …)`), otherwise
    // it is positional and has nothing to offer.
    let mut current = Some(node);
    while let Some(node) = current {
        match node.kind() {
            "array" | "map" => return false,
            "tuple" => {
                let mut cursor = node.walk();
                return node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "field");
            }
            "struct" => {
                let mut cursor = node.walk();
                if node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "field")
                {
                    return true;
                }
                // A payload with no field yet (`MeleeSimple(|)`) looks like a
                // tuple to the tree; the variant name in front of the paren is
                // the only signal. A tuple payload is rejected later, when the
                // variant is resolved against its Rust type.
                return node
                    .named_child(0)
                    .is_some_and(|child| child.kind() == "identifier");
            }
            _ => current = node.parent(),
        }
    }
    false
}

/// Whether the cursor is in an anonymous record that belongs to a field,
/// possibly through an array (`effects: [(key: …, |)]`), rather than in a
/// top-level record or a named struct variant (`Ball(…)`).
///
/// Such a record has no type name of its own; its owner is the type of the
/// enclosing field, so completion must use the field's type instead of walking
/// up to the document type.
///
/// Returns that enclosing field's name. When the cursor is already on an inner
/// field (`scope: (delivery: …)`), the name differs from the field under the
/// cursor and the record's own type stays the owner.
pub(crate) fn anonymous_record_field_name(content: &str, position: Position) -> Option<String> {
    use crate::ts_utils::{self, RonParser};

    let mut parser = RonParser::new();
    let tree = parser.parse(content)?;
    let node = ts_utils::node_at_position(&tree, content, position)?;

    let mut record_seen = false;
    let mut current = Some(node);
    while let Some(node) = current {
        match node.kind() {
            "array" | "map" => {
                if !record_seen {
                    return None;
                }
            }
            "tuple" => {
                let mut cursor = node.walk();
                let has_field = node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "field");
                // `scope: ()` is an empty record, not a positional tuple: name
                // the enclosing field so its type can provide the fields.
                if node.named_child_count() == 0 {
                    record_seen = true;
                } else if !has_field && !record_seen {
                    return None;
                }
            }
            "struct" => {
                let mut cursor = node.walk();
                if !node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "field")
                {
                    return None;
                }
                // A named struct (`Ball(…)`) is an enum variant, not a bare
                // record: its first child is the identifier.
                let named = node
                    .named_child(0)
                    .is_some_and(|child| child.kind() == "identifier");
                if named {
                    return None;
                }
                record_seen = true;
            }
            "field" => {
                // The first field is the one under the cursor (or its value):
                // skip it and keep walking to the record that contains it.
                if record_seen {
                    return ts_utils::field_name(&node, content).map(str::to_string);
                }
            }
            "source_file" => return None,
            _ => {}
        }
        current = node.parent();
    }
    None
}

/// Determine what we're completing based on cursor position using tree-sitter
fn get_completion_context(content: &str, position: Position) -> CompletionContext {
    use crate::ts_utils::{self, RonParser};

    if is_field_name_position(content, position) {
        return CompletionContext::FieldName;
    }

    if empty_field_before_cursor(content, position).is_some() {
        return CompletionContext::FieldValue;
    }

    let mut parser = RonParser::new();
    let tree = match parser.parse(content) {
        Some(t) => t,
        None => return CompletionContext::FieldName,
    };

    let node = match ts_utils::node_at_position(&tree, content, position) {
        Some(n) => n,
        None => return CompletionContext::FieldName,
    };

    // Check if we're inside a field node
    if let Some(field_node) = ts_utils::find_ancestor_by_kind(node, "field") {
        // Check if we're after the colon (in the value position)
        let field_name_node = field_node.child(0);
        if let (Some(field_name), Some(value_node)) =
            (field_name_node, ts_utils::field_value(&field_node))
        {
            // If cursor is after the field name, we're completing a value
            let name_end = field_name.end_position();
            if position.line > name_end.row as u32
                || (position.line == name_end.row as u32
                    && position.character > name_end.column as u32)
            {
                // Check if there's already some text (might be completing a type)
                if let Some(val_text) = ts_utils::node_text(&value_node, content) {
                    if val_text
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
                    {
                        return CompletionContext::StructType;
                    }
                }

                return CompletionContext::FieldValue;
            }
        }
    }

    // Default to field name completion
    CompletionContext::FieldName
}

/// Find the type we're currently inside (e.g., if typing "User(" we return "User")
#[allow(dead_code)]
fn find_current_type_context(content: &str, position: Position) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();

    if position.line as usize >= lines.len() {
        return None;
    }

    let line = lines[position.line as usize];
    let before_cursor =
        &line[..crate::ts_utils::line_column_to_byte_offset(line, position.character)];

    // Look for pattern like "TypeName(" right before cursor
    // This handles cases like: author: User(
    if let Some(paren_pos) = before_cursor.rfind('(') {
        let before_paren = before_cursor[..paren_pos].trim();

        // Extract the type name (last word before the paren)
        if let Some(word_start) = before_paren.rfind(|c: char| !c.is_alphanumeric() && c != '_') {
            let type_name = &before_paren[word_start + 1..];
            if !type_name.is_empty() && type_name.chars().next().unwrap().is_uppercase() {
                return Some(type_name.to_string());
            }
        } else if !before_paren.is_empty() && before_paren.chars().next().unwrap().is_uppercase() {
            return Some(before_paren.to_string());
        }
    }

    None
}

/// Generate completions for a given type (already navigated to the innermost type)
/// Type context navigation is now done in main.rs using Backend::navigate_to_innermost_type
pub async fn generate_completions_for_type(
    content: &str,
    position: Position,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    let effective_type = type_info;

    let context = get_completion_context(content, position);

    match context {
        CompletionContext::FieldName => {
            generate_field_completions(content, position, effective_type)
        }
        CompletionContext::FieldValue => {
            // Find the field we're completing the value for
            if let Some(field_name) = find_current_field(content, position) {
                let mut completions = generate_value_completions_for_field(
                    field_name,
                    effective_type,
                    analyzer.clone(),
                )
                .await;

                // Also add all workspace symbols as potential completions
                completions.extend(get_all_workspace_types(analyzer).await);

                completions
            } else {
                get_all_workspace_types(analyzer).await
            }
        }
        CompletionContext::StructType => {
            // Find the field type and provide struct completions
            if let Some(field_name) = find_current_field(content, position) {
                generate_type_completions_for_field(field_name, effective_type, analyzer).await
            } else {
                Vec::new()
            }
        }
    }
}

/// Get all types from the workspace as completion items
async fn get_all_workspace_types(analyzer: Arc<RustAnalyzer>) -> Vec<CompletionItem> {
    analyzer
        .get_all_types()
        .await
        .into_iter()
        .map(|type_info| create_type_completion(&type_info))
        .collect()
}

fn generate_field_completions(
    content: &str,
    position: Position,
    type_info: &TypeInfo,
) -> Vec<CompletionItem> {
    match &type_info.kind {
        TypeKind::Struct(fields) => {
            // Get fields already used in this struct payload
            let used_fields = tree_sitter_parser::extract_fields_at_position(content, position);

            // Generate completions for unused fields
            fields
                .iter()
                .filter(|field| !used_fields.contains(&field.name))
                .map(|field| {
                    let documentation = if let Some(docs) = &field.docs {
                        Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format!(
                                "```rust\n{}: {}\n```\n\n{}",
                                field.name, field.type_name, docs
                            ),
                        }))
                    } else {
                        Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format!("```rust\n{}: {}\n```", field.name, field.type_name),
                        }))
                    };

                    CompletionItem {
                        label: field.name.clone(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(field.type_name.clone()),
                        documentation,
                        insert_text: Some(format!("{}: ", field.name)),
                        ..Default::default()
                    }
                })
                .collect()
        }
        TypeKind::Enum(variants) => {
            // Check if we're inside a specific variant's fields
            if let Some(variant_name) =
                tree_sitter_parser::find_current_variant_context(content, position)
            {
                if let Some(variant) = variants.iter().find(|v| v.name == variant_name) {
                    // Complete the variant's fields
                    let used_fields =
                        tree_sitter_parser::extract_fields_at_position(content, position);
                    return variant
                        .fields
                        .iter()
                        .filter(|field| !used_fields.contains(&field.name))
                        .map(|field| {
                            let documentation = if let Some(docs) = &field.docs {
                                Some(Documentation::MarkupContent(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: format!(
                                        "```rust\n{}: {}\n```\n\n{}",
                                        field.name, field.type_name, docs
                                    ),
                                }))
                            } else {
                                Some(Documentation::MarkupContent(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: format!(
                                        "```rust\n{}: {}\n```",
                                        field.name, field.type_name
                                    ),
                                }))
                            };

                            CompletionItem {
                                label: field.name.clone(),
                                kind: Some(CompletionItemKind::FIELD),
                                detail: Some(field.type_name.clone()),
                                documentation,
                                insert_text: Some(format!("{}: ", field.name)),
                                ..Default::default()
                            }
                        })
                        .collect();
                }
            }

            // Otherwise, complete variant names
            // Generate completions for enum variants
            variants
                .iter()
                .map(|variant| {
                    let documentation = if let Some(docs) = &variant.docs {
                        Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format!("```rust\n{}\n```\n\n{}", variant.name, docs),
                        }))
                    } else {
                        Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format!("```rust\n{}\n```", variant.name),
                        }))
                    };

                    let insert_text = if variant.fields.is_empty() {
                        variant.name.clone()
                    } else if variant
                        .fields
                        .iter()
                        .all(|f| f.name.chars().all(|c| c.is_numeric()))
                    {
                        // Tuple variant
                        format!("{}($0)", variant.name)
                    } else {
                        // Struct variant
                        format!("{}($0)", variant.name)
                    };

                    CompletionItem {
                        label: variant.name.clone(),
                        kind: Some(CompletionItemKind::ENUM_MEMBER),
                        detail: Some(format!("Variant of {}", type_info.name)),
                        documentation,
                        insert_text: Some(insert_text),
                        ..Default::default()
                    }
                })
                .collect()
        }
    }
}

/// Find the field name for the current cursor position using tree-sitter
fn find_current_field(content: &str, position: Position) -> Option<String> {
    if let Some(field) = empty_field_before_cursor(content, position) {
        return Some(field);
    }
    tree_sitter_parser::get_field_at_position(content, position)
}

/// Generate value completions for a specific field
async fn generate_value_completions_for_field(
    field_name: String,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    // Find the field in the type info
    if let TypeKind::Struct(fields) = &type_info.kind {
        if let Some(field) = fields.iter().find(|f| f.name == field_name) {
            return generate_value_completions_by_type(&field.type_name, analyzer).await;
        }
    }

    Vec::new()
}

/// Generate type completions for a field that expects a custom type
async fn generate_type_completions_for_field(
    field_name: String,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    // Find the field in the type info
    if let TypeKind::Struct(fields) = &type_info.kind {
        if let Some(field) = fields.iter().find(|f| f.name == field_name) {
            // Get the inner type if it's a generic
            let inner_type = extract_inner_type(&field.type_name);

            // Try to get type info for this type
            if let Some(nested_type) = analyzer.get_type_info(&inner_type).await {
                return vec![create_type_completion(&nested_type)];
            }
        }
    }

    Vec::new()
}

/// Create a completion item for a type (struct or enum)
fn create_type_completion(type_info: &TypeInfo) -> CompletionItem {
    let type_name = type_info.name.split("::").last().unwrap_or(&type_info.name);

    match &type_info.kind {
        TypeKind::Struct(fields) => {
            // Generate a snippet for the struct with all fields
            let field_snippets: Vec<String> = fields
                .iter()
                .enumerate()
                .map(|(i, f)| format!("    {}: ${{{}}}", f.name, i + 1))
                .collect();

            let snippet = if field_snippets.is_empty() {
                format!("{}()", type_name)
            } else {
                format!("{}(\n{},\n)", type_name, field_snippets.join(",\n"))
            };

            CompletionItem {
                label: type_name.to_string(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(format!("struct {}", type_info.name)),
                documentation: type_info.docs.as_ref().map(|docs| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: docs.clone(),
                    })
                }),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        }
        TypeKind::Enum(_variants) => {
            // For enums, just provide the type name - variants will be suggested separately
            CompletionItem {
                label: type_name.to_string(),
                kind: Some(CompletionItemKind::ENUM),
                detail: Some(format!("enum {}", type_info.name)),
                documentation: type_info.docs.as_ref().map(|docs| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: docs.clone(),
                    })
                }),
                insert_text: Some(format!("{}($0)", type_name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        }
    }
}

/// Extract inner type from generics (e.g., Option<T> -> T, Vec<T> -> T)
fn extract_inner_type(type_string: &str) -> String {
    let clean = type_string.replace(" ", "");

    if let Some(start) = clean.find('<') {
        if let Some(end) = clean.rfind('>') {
            return clean[start + 1..end].to_string();
        }
    }

    type_string.to_string()
}

/// Generate value completions based on field type
async fn generate_value_completions_by_type(
    field_type: &str,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<CompletionItem> {
    let mut completions = Vec::new();

    // Clean up the type string (remove spaces)
    let clean_type = field_type.replace(" ", "");

    // First check if this is a custom type (struct or enum) in the workspace
    if let Some(type_info) = analyzer.get_type_info(field_type).await {
        match &type_info.kind {
            TypeKind::Enum(variants) => {
                // For enums, provide completions for each variant
                for variant in variants {
                    let completion = CompletionItem {
                        label: variant.name.clone(),
                        kind: Some(CompletionItemKind::ENUM_MEMBER),
                        detail: Some(format!("Variant of {}", type_info.name)),
                        documentation: variant.docs.as_ref().map(|docs| {
                            Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: docs.clone(),
                            })
                        }),
                        insert_text: Some(variant.name.clone()),
                        ..Default::default()
                    };
                    completions.push(completion);
                }
                return completions;
            }
            TypeKind::Struct(_) => {
                // For structs, provide the type with snippet
                completions.push(create_type_completion(&type_info));
                return completions;
            }
        }
    }

    // Check for generic types and try to provide completions for the inner type
    if clean_type.starts_with("Option<") {
        let inner = extract_inner_type(&clean_type);
        if let Some(type_info) = analyzer.get_type_info(&inner).await {
            completions.push(CompletionItem {
                label: format!(
                    "Some({})",
                    type_info.name.split("::").last().unwrap_or(&type_info.name)
                ),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some("Some variant with nested type".to_string()),
                insert_text: Some(format!(
                    "Some({}($0))",
                    type_info.name.split("::").last().unwrap_or(&type_info.name)
                )),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        } else {
            completions.push(CompletionItem {
                label: "Some()".to_string(),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some("Some variant".to_string()),
                insert_text: Some("Some($0)".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
        completions.push(CompletionItem {
            label: "None".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("None variant".to_string()),
            insert_text: Some("None".to_string()),
            ..Default::default()
        });
        return completions;
    }

    // Handle primitive types
    if clean_type == "bool" {
        completions.push(CompletionItem {
            label: "true".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("Boolean value".to_string()),
            insert_text: Some("true".to_string()),
            ..Default::default()
        });
        completions.push(CompletionItem {
            label: "false".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("Boolean value".to_string()),
            insert_text: Some("false".to_string()),
            ..Default::default()
        });
    } else if clean_type.starts_with("Option<") {
        completions.push(CompletionItem {
            label: "Some()".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("Some variant".to_string()),
            insert_text: Some("Some($0)".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });
        completions.push(CompletionItem {
            label: "None".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("None variant".to_string()),
            insert_text: Some("None".to_string()),
            ..Default::default()
        });
    } else if clean_type.starts_with("Vec<") || clean_type.starts_with("[") {
        completions.push(CompletionItem {
            label: "[]".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("Empty vector/array".to_string()),
            insert_text: Some("[]".to_string()),
            ..Default::default()
        });
        completions.push(CompletionItem {
            label: "[...]".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("Vector/array with elements".to_string()),
            insert_text: Some("[$0]".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    } else if clean_type.starts_with("HashMap<") || clean_type.starts_with("BTreeMap<") {
        completions.push(CompletionItem {
            label: "{}".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("Empty map".to_string()),
            insert_text: Some("{}".to_string()),
            ..Default::default()
        });
        completions.push(CompletionItem {
            label: "{...}".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("Map with entries".to_string()),
            insert_text: Some("{$0}".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    } else if clean_type == "String" || clean_type == "&str" {
        completions.push(CompletionItem {
            label: "\"\"".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("String value".to_string()),
            insert_text: Some("\"$0\"".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    } else if clean_type.starts_with("i")
        || clean_type.starts_with("u")
        || clean_type.starts_with("f")
    {
        // Numeric types (i8, i16, i32, i64, u8, u16, u32, u64, f32, f64)
        completions.push(CompletionItem {
            label: "0".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format!("{} value", field_type)),
            insert_text: Some("0".to_string()),
            ..Default::default()
        });
    }

    completions
}

/// Unwraps `Vec<…>`, `Option<…>`, `Box<…>` wrappers down to the payload type.
pub(crate) fn unwrap_wrapper(type_name: &str) -> String {
    let mut current = type_name.replace(' ', "");
    loop {
        let mut unwrapped = None;
        for wrapper in ["Vec<", "Option<", "Box<", "Rc<", "Arc<"] {
            if current.starts_with(wrapper) && current.ends_with('>') {
                unwrapped = Some(current[wrapper.len()..current.len() - 1].to_string());
                break;
            }
        }
        match unwrapped {
            Some(inner) => current = inner,
            None => return current,
        }
    }
}

/// Resolves a field's model type, unwrapping containers: `delivery: Vec<Delivery>`
/// points at `Delivery`, not at a type literally named `Vec<Delivery>`.
pub(crate) async fn field_type_info(
    analyzer: &Arc<RustAnalyzer>,
    field: &FieldInfo,
) -> Option<TypeInfo> {
    analyzer
        .get_type_info(&unwrap_wrapper(&field.type_name))
        .await
}

/// One step of the path from the file root down to the cursor.
#[derive(Debug, PartialEq)]
enum Step {
    /// A struct field by name.
    Field(String),
    /// A tuple element: a tuple struct names its fields "0", "1", …, so the
    /// position of the value is the field name.
    Element(usize),
    /// A value of an enum variant: `FirstParentOf([Filter])` — the identifier names
    /// the variant and the payload values are its fields, "0", "1", ….
    Variant { name: String, index: usize },
    /// The field list of a named struct variant beyond its last field:
    /// `MeleeSimple(range: "…", |)` — the field being typed has no value yet, so
    /// no `Variant` slot names it. The variant's payload is the owner of the list.
    VariantFields { name: String },
}

/// Whether a variant field is positional (`0`, `1`, …) rather than named.
///
/// Tuple payloads use positional names; named struct variants (`Dodge(key: …)`)
/// keep Rust identifiers. The distinction decides whether a `Variant` step
/// selects a payload value or is only a context for the following field step.
fn is_tuple_slot_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_digit())
}

/// A named struct variant's payload as a struct.
///
/// RON writes the payload as fields (`MeleeSimple(range: …)`), while Rust keeps
/// it as a variant that has no type name of its own. Callers that need the list
/// (completion, step resolution) take it as this synthetic struct.
fn struct_variant_type(base: &TypeInfo, variant: &EnumVariant) -> TypeInfo {
    TypeInfo {
        name: format!("{}::{}", base.name, variant.name),
        kind: TypeKind::Struct(variant.fields.clone()),
        docs: variant.docs.clone(),
        source_file: base.source_file.clone(),
        line: variant.line,
        column: variant.column,
        has_default: false,
    }
}

/// Whether the variant's payload is a named field list rather than a tuple.
///
/// Tuple payloads name their fields "0", "1", …; only named payloads give a
/// meaning to a field-name position inside them.
fn is_named_struct_variant(variant: &EnumVariant) -> bool {
    variant
        .fields
        .first()
        .is_some_and(|field| !is_tuple_slot_name(&field.name))
}

/// Whether `ancestor` is an ancestor of `node` (or the node itself).
fn node_is_ancestor_or_self(ancestor: tree_sitter::Node<'_>, node: tree_sitter::Node<'_>) -> bool {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.id() == ancestor.id() {
            return true;
        }
        current = candidate.parent();
    }
    false
}

/// The text of a node taken from the source.
fn node_text<'a>(node: &tree_sitter::Node<'_>, content: &'a str) -> &'a str {
    content
        .get(node.start_byte()..node.end_byte())
        .unwrap_or("")
}

/// Values of an unclosed `(` group inside the node: the fields of the pair are
/// positional, and while the first value is being typed the tuple may still be an `ERROR`.
fn values_in_open_paren<'t>(
    node: &tree_sitter::Node<'t>,
    offset: usize,
) -> Option<Vec<tree_sitter::Node<'t>>> {
    // A struct in parentheses (`( key: …, items: [ … ] )`) is not a tuple, and a
    // variant call (`FirstParentOf([ … ])`) is a `struct` node too — the named-field
    // check below tells them apart, so neither is rejected by the node kind.

    let mut open = None;
    let mut closed = false;
    for index in 0..node.child_count() {
        let child = node.child(index)?;
        match child.kind() {
            "(" if child.start_byte() <= offset => {
                open = Some(child);
                closed = false;
            }
            ")" if open.is_some() && child.start_byte() < offset => closed = true,
            _ => {}
        }
    }
    let open = open?;
    if closed {
        return None;
    }

    let values: Vec<tree_sitter::Node<'t>> = node
        .named_children(&mut node.walk())
        .filter(|child| child.start_byte() >= open.end_byte())
        .collect();

    // Named fields are returned too: a named struct variant (`Dodge(key: …)`)
    // needs its variant step, and the caller ignores named values when the node
    // has no variant name (a bare record `(key: …)`).
    Some(values)
}

/// The enum variant a `struct` node stands for: `FirstParentOf([ … ])` begins with
/// the variant name, while a bare tuple struct `( … )` does not.
fn variant_name(node: &tree_sitter::Node<'_>, content: &str) -> Option<String> {
    // Only a `struct` node names a variant: a bare tuple is a `tuple` node.
    if node.kind() != "struct" {
        return None;
    }
    let first = node.named_child(0)?;
    if first.kind() != "identifier" {
        return None;
    }
    let name = node_text(&first, content);
    (!name.is_empty()).then(|| name.to_string())
}

/// The path of steps from the file root down to the cursor.
fn path_at_position(content: &str, position: Position) -> Option<Vec<Step>> {
    use crate::ts_utils::{self, RonParser};

    let mut parser = RonParser::new();
    let tree = parser.parse(content)?;

    // The editor cursor sits after the last typed character, so the position may
    // land on a space or a comma: the node is then taken from the byte before the
    // cursor when that one is deeper — this is how `items: [ ],` and `(Kind|) ` find their field.
    let offset = ts_utils::position_to_byte_offset(content, position);
    let root = tree.root_node();
    let after = root.descendant_for_byte_range(offset, offset);
    let before_offset = offset.saturating_sub(1);
    let before = root.descendant_for_byte_range(before_offset, before_offset);
    // The value before the cursor matters more than the punctuation under it:
    // `(Kind|` and `(Kind,|` are the same path position, and a comma alone says nothing.
    let is_value = |kind: &str| {
        matches!(
            kind,
            "identifier" | "integer" | "float" | "string" | "raw_string" | "boolean" | "char"
        )
    };
    let cursor = match (after, before) {
        (_, Some(before)) if is_value(before.kind()) => before,
        (Some(after), _) => after,
        (None, Some(before)) => before,
        (None, None) => return None,
    };

    let mut path = Vec::new();
    let mut current = Some(cursor);
    let mut chain = Vec::new();
    while let Some(node) = current {
        chain.push(node.kind().to_string());
        if node.kind() == "field" {
            if let Some(name) = ts_utils::field_name(&node, content) {
                path.push(Step::Field(name.to_string()));
            }
        }

        // A position inside the pair: a group in parentheses is not necessarily a `tuple`.
        // Without a comma the parser does not treat `(k` as a tuple yet and keeps it in an
        // ERROR, so the `(` itself is searched for and the values after it are numbered.
        if let Some(values) = values_in_open_paren(&node, offset) {
            if let Some(index) = values
                .iter()
                .position(|value| node_is_ancestor_or_self(*value, cursor))
                .or_else(|| values.iter().position(|value| value.start_byte() >= offset))
            {
                // `FirstParentOf([ … ])` names the enum variant in front of the payload.
                match variant_name(&node, content) {
                    // `Root(…)` at the document root is the mapped type itself,
                    // not a variant of it; variants live in arrays and fields.
                    Some(_)
                        if node
                            .parent()
                            .is_some_and(|parent| parent.kind() == "source_file") => {}
                    Some(name) => path.push(Step::Variant { name, index }),
                    // A bare record `(key: …)` is not a tuple element: its fields
                    // are named, and the field step already names the one at the cursor.
                    None if !values.iter().any(|value| value.kind() == "field") => {
                        path.push(Step::Element(index))
                    }
                    None => {}
                }
            } else {
                let before = content.get(..offset).unwrap_or_default().trim_end();
                let starts_field_list = (values.is_empty() && before.ends_with('('))
                    || (values.iter().any(|value| value.kind() == "field")
                        && before.ends_with(','));
                if starts_field_list {
                    // The cursor is in a named struct variant's field list but
                    // not on a value: `MeleeSimple(|)` (nothing typed yet) or
                    // `MeleeSimple(range: "…", |)` (past the last field). The
                    // variant itself owns the list; a tuple payload is rejected
                    // when the variant kind is resolved.
                    if let Some(name) = variant_name(&node, content) {
                        if !node
                            .parent()
                            .is_some_and(|parent| parent.kind() == "source_file")
                        {
                            path.push(Step::VariantFields { name });
                        }
                    }
                }
            }
        }

        // Broken field: the name is left as a neighbouring identifier (`choices`), while
        // `: [ …` goes into an ERROR — the name is taken from the left neighbour.
        if node_text(&node, content).trim_start().starts_with(':') {
            if let Some(previous) = node.prev_named_sibling() {
                if previous.kind() == "identifier" {
                    path.push(Step::Field(node_text(&previous, content).to_string()));
                }
            }
        }
        current = node.parent();
    }
    path.reverse();
    debug_log(&format!("node chain: {chain:?}"));

    // The value is not typed yet: the cursor sits after `kind:`, and there is no value
    // node in the tree. The field name is taken from the text on the left — otherwise
    // the path breaks and the completions fall back to the struct's field list.
    if let Some(name) = field_name_left_of_cursor(content, offset) {
        // The name may already stand in the path: inside a payload written on the same
        // line the text left of the cursor names the outer field (`target: First...([…`),
        // and a second step would break the resolution.
        let known = path
            .iter()
            .any(|step| matches!(step, Step::Field(field) if *field == name));
        if !known {
            path.push(Step::Field(name));
        }
    }

    (!path.is_empty()).then_some(path)
}

/// The field name when the cursor sits right after its colon (`kind: |`).
fn field_name_left_of_cursor(content: &str, offset: usize) -> Option<String> {
    let left = content.get(..offset)?;
    let line = left.rsplit('\n').next()?;
    let trimmed = line.trim_end();
    let (name, rest) = trimmed.rsplit_once(':')?;
    // Only an empty value position needs the synthetic field step: in
    // `formula: "1"` the tree already has the formula field and a value.
    if !rest.trim().is_empty() {
        return None;
    }
    let name = name.trim_end();
    let name = name.rsplit(|c: char| c.is_whitespace()).next()?;

    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_string())
}

/// The type after a path step: a struct field or an element of a tuple struct.
async fn step_type(
    analyzer: &Arc<RustAnalyzer>,
    current: &TypeInfo,
    step: &Step,
) -> Option<TypeInfo> {
    // A named struct variant (`Dodge(key: …, evasion: …)`) is a context, not a
    // value: the following `Field` step names the field inside it. Expose the
    // variant's own fields as a struct so that step resolves against them.
    // Tuple payloads (`FirstParentOf([ … ])`) keep the old value semantics.
    if let Step::Variant { .. } | Step::VariantFields { .. } = step {
        let base = analyzer
            .get_type_info(&unwrap_wrapper(&current.name))
            .await
            .unwrap_or_else(|| current.clone());
        if let TypeKind::Enum(variants) = &base.kind {
            let variant = match step {
                Step::Variant { name, index } => variants
                    .iter()
                    .find(|variant| &variant.name == name)
                    .filter(|variant| {
                        variant
                            .fields
                            .get(*index)
                            .is_some_and(|field| !is_tuple_slot_name(&field.name))
                    }),
                Step::VariantFields { name } => variants
                    .iter()
                    .find(|variant| &variant.name == name)
                    .filter(|variant| is_named_struct_variant(variant)),
                _ => None,
            };
            if let Some(variant) = variant {
                return Some(struct_variant_type(&base, variant));
            }
        }
    }

    let type_name = field_type_name(analyzer, current, step).await?;
    analyzer.get_type_info(&unwrap_wrapper(&type_name)).await
}

/// The name of the type written in a field (or in a tuple position) of the current type.
async fn field_type_name(
    analyzer: &Arc<RustAnalyzer>,
    current: &TypeInfo,
    step: &Step,
) -> Option<String> {
    // An array is written in RON as is: `choices: [ (Kind, Direction) ]` — the path
    // step refers to an array element, so the wrappers (`Vec<T>`, `Option<T>`) are
    // unwrapped before the step is applied.
    let base = analyzer
        .get_type_info(&unwrap_wrapper(&current.name))
        .await
        .unwrap_or_else(|| current.clone());

    match step {
        // `find_field` also searches the fields of enum variants: a named
        // struct variant exposes them without a separate type of its own.
        Step::Field(name) => Some(base.find_field(name)?.type_name.clone()),
        Step::Element(index) => Some(base.fields()?.get(*index)?.type_name.clone()),
        Step::Variant { name, index } => {
            let TypeKind::Enum(variants) = &base.kind else {
                return None;
            };
            let variant = variants.iter().find(|variant| &variant.name == name)?;
            Some(variant.fields.get(*index)?.type_name.clone())
        }
        // A field list has no single type: only `step_type` gives it shape.
        Step::VariantFields { .. } => None,
    }
}

/// Temporary log for debugging completions: enabled by `RON_LSP_DEBUG_COMPLETION`.
fn debug_log(message: &str) {
    if std::env::var_os("RON_LSP_DEBUG_COMPLETION").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("ron-lsp-completion.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{message}");
    }
}

/// Whether the cursor sits inside a string (`"…"`).
fn cursor_inside_string(content: &str, position: Position) -> bool {
    use crate::ts_utils::{self, RonParser};

    let offset = ts_utils::position_to_byte_offset(content, position);
    let Some(tree) = RonParser::new().parse(content) else {
        return false;
    };
    let mut node = tree.root_node().descendant_for_byte_range(offset, offset);
    while let Some(current) = node {
        match current.kind() {
            "string" | "raw_string" => return true,
            "field" | "struct" | "array" | "map" | "tuple" | "source_file" => return false,
            _ => node = current.parent(),
        }
    }
    false
}

/// A field under the cursor, found through the syntax tree.
///
/// The path is collected upwards from the cursor, as for completions, and resolved
/// downwards from the document type, so a field inside arrays and tuples is found
/// too — the annotation contexts never descend there.
pub(crate) struct FieldAtCursor {
    /// The owning type, by full name.
    pub owner: String,
    /// The owning type itself: a named struct variant has no resolvable name of
    /// its own, so callers that need the fields (completion) take them from here.
    pub owner_type: TypeInfo,
    /// The field name; a tuple position keeps the name the scanner gave it (`0`, `1`).
    pub name: String,
    /// The field type as declared, as a string.
    pub type_name: String,
    /// The field's doc comment from the model.
    pub docs: Option<String>,
}

/// A top-level RON array is described by its element type in `ron.toml`
/// (`abilities.ron` -> `AbilityDef`), while the tree path starts with `Element(0)`.
/// Drop those outer element steps: they belong to the file, not to the mapped type.
fn strip_outer_elements(path: &mut Vec<Step>, top_type: &str) {
    let top_type = top_type.trim();
    if top_type.starts_with("Vec<")
        || top_type.starts_with('[')
        || top_type.starts_with('(')
        || top_type.starts_with("&[")
    {
        return;
    }
    while matches!(path.first(), Some(Step::Element(_))) {
        path.remove(0);
    }
}

/// Finds the field under the cursor through the tree (see [`FieldAtCursor`]).
pub(crate) async fn field_at_position(
    content: &str,
    position: Position,
    top_type: &str,
    analyzer: Arc<RustAnalyzer>,
) -> Option<FieldAtCursor> {
    let mut path = path_at_position(content, position)?;
    strip_outer_elements(&mut path, top_type);
    let (last, enclosing) = path.split_last()?;

    let mut current = analyzer.get_type_info(top_type).await?;
    for step in enclosing {
        current = step_type(&analyzer, &current, step).await?;
    }

    // Wrappers (`Vec<T>`) are stripped: a tuple position lives in the struct
    // itself, not in its vector.
    let owner = analyzer
        .get_type_info(&unwrap_wrapper(&current.name))
        .await
        .unwrap_or(current);

    let field = match last {
        // `find_field` searches enum variants too: a named struct variant is
        // exposed as its own field list by `step_type`.
        Step::Field(name) => owner.find_field(name)?.clone(),
        Step::Element(index) => owner.fields()?.get(*index)?.clone(),
        Step::Variant { name, index } => {
            let TypeKind::Enum(variants) = &owner.kind else {
                return None;
            };
            let variant = variants.iter().find(|variant| &variant.name == name)?;
            variant.fields.get(*index)?.clone()
        }
        // The cursor is in a named struct variant's field list but after its
        // last field: there is no value to name, so the variant owns the list.
        Step::VariantFields { name } => {
            let TypeKind::Enum(variants) = &owner.kind else {
                return None;
            };
            let variant = variants.iter().find(|variant| &variant.name == name)?;
            if !is_named_struct_variant(variant) {
                return None;
            }
            let owner_type = struct_variant_type(&owner, variant);
            return Some(FieldAtCursor {
                owner: owner_type.name.clone(),
                owner_type,
                name: String::new(),
                type_name: String::new(),
                docs: variant.docs.clone(),
            });
        }
    };

    Some(FieldAtCursor {
        owner: owner.name.clone(),
        owner_type: owner,
        name: field.name.clone(),
        type_name: field.type_name.clone(),
        docs: field.docs.clone(),
    })
}

/// Completions for the value found by the path from the cursor upwards.
///
/// Annotation-context navigation descends neither into `Vec<T>`, nor into nested
/// structs, nor into tuples: `choices` lives in `Item` inside `items: [...]`, and the
/// pair is the tuple struct `Pair`. Here the path comes from the tree (field names and
/// tuple element indices from the root down to the cursor), every step unwraps the
/// wrappers and takes the type from the current struct; at the value position the enum
/// variants are offered — and only they.
pub async fn completions_for_field_path(
    content: &str,
    position: Position,
    top_type: &str,
    analyzer: Arc<RustAnalyzer>,
) -> Option<Vec<CompletionItem>> {
    // At a field-name position the path still names the previous field; its value
    // type would answer the wrong question, so field-name completion takes over.
    if is_field_name_position(content, position) {
        return None;
    }
    // There is nothing to suggest inside a string: an unclosed quote swallows the rest
    // of the file, and any list would be made up.
    if cursor_inside_string(content, position) {
        return Some(Vec::new());
    }

    let mut path = path_at_position(content, position)?;
    strip_outer_elements(&mut path, top_type);
    debug_log(&format!("path: {path:?}"));
    let (last, enclosing) = path.split_last()?;

    let mut current = analyzer.get_type_info(top_type).await?;
    debug_log(&format!("root {top_type} -> {}", current.name));
    for step in enclosing {
        match step_type(&analyzer, &current, step).await {
            Some(next) => {
                debug_log(&format!("  {step:?} -> {}", next.name));
                current = next;
            }
            None => {
                let fields: Vec<String> = current
                    .fields()
                    .map(|list| list.iter().map(|field| field.name.clone()).collect())
                    .unwrap_or_default();
                debug_log(&format!(
                    "  {step:?} -> BREAK (current {}: {fields:?})",
                    current.name
                ));
                return None;
            }
        }
    }

    // The type of the last step is known: the answer is produced here — "all types of
    // the project" would only be noise. An enum gives its variants, a struct gives its
    // fields, a primitive (`String`, numbers) gives nothing.
    let Some(type_name) = field_type_name(&analyzer, &current, last).await else {
        debug_log(&format!("  last {last:?} -> no such field"));
        return None;
    };
    let Some(inner) = analyzer.get_type_info(&unwrap_wrapper(&type_name)).await else {
        return Some(Vec::new());
    };

    match &inner.kind {
        TypeKind::Enum(variants) => Some(
            variants
                .iter()
                .map(|variant| CompletionItem {
                    label: variant.name.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    ..Default::default()
                })
                .collect(),
        ),
        TypeKind::Struct(fields) => Some(
            fields
                .iter()
                .map(|field| CompletionItem {
                    label: field.name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    ..Default::default()
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_analyzer::{EnumVariant, FieldInfo, TypeInfo, TypeKind};

    /// A field with only a name and a type: docs, position and default are irrelevant here.
    fn field(name: &str, type_name: &str) -> FieldInfo {
        FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
        }
    }

    /// A type with the given name and shape, without docs or a source position.
    fn type_of(name: &str, kind: TypeKind) -> TypeInfo {
        TypeInfo {
            name: name.to_string(),
            kind,
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        }
    }

    /// A unit enum variant.
    fn variant(name: &str) -> EnumVariant {
        EnumVariant {
            name: name.to_string(),
            fields: vec![],
            docs: None,
            line: None,
            column: None,
        }
    }

    #[tokio::test]
    async fn test_enum_variant_field_completion() {
        // Create a mock enum with a struct variant
        let variant = EnumVariant {
            name: "StructVariant".to_string(),
            fields: vec![
                FieldInfo {
                    name: "field_a".to_string(),
                    type_name: "String".to_string(),
                    docs: Some("Field A documentation".to_string()),
                    line: Some(10),
                    column: Some(8),
                    has_default: false,
                },
                FieldInfo {
                    name: "field_b".to_string(),
                    type_name: "i32".to_string(),
                    docs: None,
                    line: Some(11),
                    column: Some(8),
                    has_default: false,
                },
            ],
            docs: Some("A struct variant".to_string()),
            line: Some(9),
            column: Some(4),
        };

        let type_info = TypeInfo {
            name: "MyEnum".to_string(),
            kind: TypeKind::Enum(vec![variant]),
            docs: None,
            source_file: None,
            line: Some(8),
            column: Some(0),
            has_default: false,
        };

        // Test content with a struct variant (RON uses parentheses)
        let content = "MyEnum::StructVariant(\n    \n)";
        let position = Position::new(1, 4); // Inside the parens

        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let completions =
            generate_completions_for_type(content, position, &type_info, analyzer).await;

        // Should complete with variant fields
        assert!(!completions.is_empty());
        let field_labels: Vec<String> = completions.iter().map(|c| c.label.clone()).collect();
        assert!(field_labels.contains(&"field_a".to_string()));
        assert!(field_labels.contains(&"field_b".to_string()));

        // Check that field_a has documentation
        let field_a = completions.iter().find(|c| c.label == "field_a").unwrap();
        assert!(field_a.documentation.is_some());
        if let Some(Documentation::MarkupContent(content)) = &field_a.documentation {
            assert!(content.value.contains("Field A documentation"));
        }
    }

    #[tokio::test]
    async fn test_enum_variant_completion() {
        // Create a mock enum with multiple variants
        let variant1 = EnumVariant {
            name: "UnitVariant".to_string(),
            fields: vec![],
            docs: Some("A unit variant".to_string()),
            line: Some(9),
            column: Some(4),
        };

        let variant2 = EnumVariant {
            name: "TupleVariant".to_string(),
            fields: vec![FieldInfo {
                name: "0".to_string(),
                type_name: "i32".to_string(),
                docs: None,
                line: None,
                column: None,
                has_default: false,
            }],
            docs: Some("A tuple variant".to_string()),
            line: Some(10),
            column: Some(4),
        };

        let type_info = TypeInfo {
            name: "MyEnum".to_string(),
            kind: TypeKind::Enum(vec![variant1, variant2]),
            docs: None,
            source_file: None,
            line: Some(8),
            column: Some(0),
            has_default: false,
        };

        // Test in FieldName context - should get variant completions
        let content = "";
        let position = Position::new(0, 0);

        let analyzer = std::sync::Arc::new(crate::rust_analyzer::RustAnalyzer::new());
        let completions =
            generate_completions_for_type(content, position, &type_info, analyzer).await;

        // Should complete with variant names
        assert!(!completions.is_empty());

        let variant_labels: Vec<String> = completions.iter().map(|c| c.label.clone()).collect();
        assert!(variant_labels.contains(&"UnitVariant".to_string()));
        assert!(variant_labels.contains(&"TupleVariant".to_string()));
    }
    /// The path to the field is collected from the tree, and its type is unwrapped through `Vec`.
    #[tokio::test]
    async fn nested_field_offers_enum_variants() {
        use crate::rust_analyzer::{EnumVariant, FieldInfo};

        let analyzer = Arc::new(RustAnalyzer::new());
        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
        };
        let type_of = |name: &str, kind: TypeKind| TypeInfo {
            name: name.to_string(),
            kind,
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };
        analyzer
            .insert_type_for_test(type_of(
                "Root",
                TypeKind::Struct(vec![field("items", "Vec<Item>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Item",
                TypeKind::Struct(vec![field("choices", "Vec<Choice>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Choice",
                TypeKind::Enum(vec![
                    EnumVariant {
                        name: "Alpha".to_string(),
                        fields: vec![],
                        docs: None,
                        line: None,
                        column: None,
                    },
                    EnumVariant {
                        name: "Beta".to_string(),
                        fields: vec![],
                        docs: None,
                        line: None,
                        column: None,
                    },
                ]),
            ))
            .await;

        let content = "Root(
    items: [
        (
            choices: [

            ],
        ),
    ],
)";
        let path = path_at_position(content, Position::new(3, 22)).expect("path to the field");
        assert_eq!(
            path,
            vec![Step::Field("items".into()), Step::Field("choices".into())]
        );

        let items = completions_for_field_path(content, Position::new(3, 22), "Root", analyzer)
            .await
            .expect("completions for the enum");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["Alpha", "Beta"]);
    }

    /// A payload written on one line: the cursor inside it is a variant field, not a
    /// field of the document type - the path must not gain a step from the text left
    /// of the cursor.
    #[test]
    fn single_line_payload_path_resolves() {
        let content = "Effect(
                target: FirstParentOf([Cre]),
)";
        let path = path_at_position(content, Position::new(1, 42)).expect("path");
        assert_eq!(
            path,
            vec![
                Step::Field("target".into()),
                Step::Variant {
                    name: "FirstParentOf".into(),
                    index: 0,
                },
            ]
        );
    }

    /// A variant payload is a list of values: the cursor inside `Variant([ ... ])` is a
    /// field of that variant, and the enum variants of its element type are offered.
    #[tokio::test]
    async fn variant_payload_offers_its_element_variants() {
        use crate::rust_analyzer::{EnumVariant, FieldInfo};

        let analyzer = Arc::new(RustAnalyzer::new());
        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
        };
        let type_of = |name: &str, kind: TypeKind| TypeInfo {
            name: name.to_string(),
            kind,
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };
        let variant = |name: &str, fields: Vec<FieldInfo>| EnumVariant {
            name: name.to_string(),
            fields,
            docs: None,
            line: None,
            column: None,
        };
        analyzer
            .insert_type_for_test(type_of(
                "Root",
                TypeKind::Struct(vec![field("rules", "Vec<Rule>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Rule",
                TypeKind::Enum(vec![
                    variant("FirstParentOf", vec![field("0", "Vec<Kind>")]),
                    variant("AnyChildOf", vec![]),
                ]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Kind",
                TypeKind::Enum(vec![variant("Creature", vec![]), variant("Item", vec![])]),
            ))
            .await;

        let content = "Root(\n    rules: [\n        FirstParentOf([\n        ]),\n    ],\n)";
        let path = path_at_position(content, Position::new(2, 23)).expect("path to the payload");
        assert_eq!(
            path,
            vec![
                Step::Field("rules".into()),
                Step::Variant {
                    name: "FirstParentOf".into(),
                    index: 0,
                },
            ]
        );

        let items = completions_for_field_path(content, Position::new(2, 23), "Root", analyzer)
            .await
            .expect("completions for the payload element");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["Creature", "Item"]);
    }

    /// The pair is a tuple struct: the cursor sits inside a tuple value, and the enum
    /// variant is taken by the position of that value.
    #[tokio::test]
    async fn tuple_element_offers_enum_variants() {
        use crate::rust_analyzer::{EnumVariant, FieldInfo};

        let analyzer = Arc::new(RustAnalyzer::new());
        let field = |name: &str, type_name: &str| FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
        };
        let type_of = |name: &str, kind: TypeKind| TypeInfo {
            name: name.to_string(),
            kind,
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };
        let variant = |name: &str| EnumVariant {
            name: name.to_string(),
            fields: vec![],
            docs: None,
            line: None,
            column: None,
        };
        analyzer
            .insert_type_for_test(type_of(
                "Root",
                TypeKind::Struct(vec![field("items", "Vec<Item>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Item",
                TypeKind::Struct(vec![field("key", "String"), field("choices", "Vec<Pair>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Pair",
                TypeKind::Struct(vec![field("0", "Kind"), field("1", "Direction")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Kind",
                TypeKind::Enum(vec![
                    variant("Any"),
                    variant("Creature"),
                    variant("Item"),
                    variant("Object"),
                ]),
            ))
            .await;

        let content = "Root(
    items: [
        (
            key: \"pair.alpha\",
            choices: [
                (
                    Kind,
                    Direction,
                ),
            ],
        ),
    ],
)";
        let path = path_at_position(content, Position::new(6, 21)).expect("path to the pair");
        assert_eq!(
            path,
            vec![
                Step::Field("items".into()),
                Step::Field("choices".into()),
                Step::Element(0),
            ]
        );

        let items = completions_for_field_path(content, Position::new(6, 21), "Root", analyzer)
            .await
            .expect("variants of the kind");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["Any", "Creature", "Item", "Object"]);

        // The value is not typed yet: the cursor sits on the tuple itself — the position
        // is taken from the first child that starts no earlier than the cursor.
        let on_tuple = path_at_position(content, Position::new(5, 17)).expect("path to the tuple");
        assert_eq!(
            on_tuple,
            vec![
                Step::Field("items".into()),
                Step::Field("choices".into()),
                Step::Element(0),
            ]
        );
    }

    /// A formula field inside an array record must not gain a synthetic field
    /// step from the previous `delivery:` on the same RON line.
    #[test]
    fn formula_field_path_has_no_spurious_field() {
        let content = r#"EffectDef(
    damage: [(damage_type_key: "damage_type.elemental.poison", delivery: [Dot], formula: "1.0")],
)"#;
        let line = content.lines().nth(1).unwrap();
        let pos = Position::new(1, (line.find("formula").unwrap() + 1) as u32);
        assert_eq!(
            path_at_position(content, pos),
            Some(vec![
                Step::Field("damage".into()),
                Step::Field("formula".into()),
            ])
        );
    }

    /// A field inside a nested tuple (`projectile: (key: …)`) follows its own
    /// owner, not the outer ability: the path keeps both field steps.
    #[test]
    fn nested_tuple_field_path_resolves() {
        let content = "AbilityDef(
    projectile: (key: \"item.arrow\", consumable: true),
)";
        let path = path_at_position(content, Position::new(1, 18)).expect("path to key");
        assert_eq!(
            path,
            vec![Step::Field("projectile".into()), Step::Field("key".into()),]
        );
    }

    /// The first value of the pair is typed without a comma: there is no tuple in the
    /// tree yet (the parser keeps `( k` in an ERROR), but the position is still read.
    #[test]
    fn tuple_element_is_found_before_the_comma() {
        let content = "Root(
    items: [
        (
            key: \"pair.alpha\",
            choices: [
                (
                    k
                ),
            ],
        ),
    ],
)";
        let path = path_at_position(content, Position::new(6, 21)).expect("path to the pair");
        assert_eq!(
            path,
            vec![
                Step::Field("items".into()),
                Step::Field("choices".into()),
                Step::Element(0),
            ]
        );
    }

    /// Fields already written in the RON file are not offered again.
    #[test]
    fn field_completions_skip_fields_already_used() {
        let owner = type_of(
            "Root",
            TypeKind::Struct(vec![field("items", "Vec<Item>"), field("key", "String")]),
        );

        let items =
            generate_field_completions("Root(\n    items: [],\n)", Position::new(1, 4), &owner);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["key"]);
        assert_eq!(items[0].insert_text.as_deref(), Some("key: "));
    }

    /// An anonymous record inside an array is still owned by the array's field:
    /// `effects: [(key: …, |)]` must complete EffectApplication fields.
    #[test]
    fn array_record_is_owned_by_its_field() {
        let content = "AbilityDef(
    effects: [
        (key: \"effect.poisoned\", ),
    ],
)";
        let line = content.lines().nth(2).unwrap();
        let position = Position::new(2, (line.find(", )").unwrap() + 1) as u32);

        assert!(is_field_name_position(content, position));
        assert!(anonymous_record_field_name(content, position).is_some());
        assert_eq!(
            crate::tree_sitter_parser::extract_fields_at_position(content, position),
            vec!["key".to_string()]
        );
    }

    /// A record nested under a field names that field as its owner, whether the
    /// cursor is on an inner field or in an empty record.
    #[test]
    fn nested_record_owner_is_the_field_holding_it() {
        let content = "DefenseDef(
    Block(
        scope: (
            delivery: [Melee],
        ),
    ),
)";
        let line = content.lines().nth(3).unwrap();
        let position = Position::new(3, line.find("delivery").unwrap() as u32 + 1);
        assert_eq!(
            anonymous_record_field_name(content, position).as_deref(),
            Some("scope")
        );
    }

    /// An anonymous record (`projectile: (key: …)`) is a field-name context too:
    /// only the missing ProjectileSpec field is offered, not the outer AbilityDef
    /// fields.
    #[test]
    fn tuple_record_after_comma_completes_missing_fields() {
        let spec = type_of(
            "ProjectileSpec",
            TypeKind::Struct(vec![field("key", "String"), field("consumable", "bool")]),
        );
        let content = "AbilityDef(
    projectile: (key: \"item.arrow\", ),
)";
        let line = content.lines().nth(1).unwrap();
        let position = Position::new(1, (line.find(", )").unwrap() + 1) as u32);

        assert!(is_field_name_position(content, position));
        assert!(anonymous_record_field_name(content, position).is_some());
        assert_eq!(
            get_completion_context(content, position),
            CompletionContext::FieldName
        );
        let items = generate_field_completions(content, position, &spec);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["consumable"]);
        assert_eq!(items[0].insert_text.as_deref(), Some("consumable: "));

        // Both fields are present: the record owns the answer, so it is empty
        // rather than the outer AbilityDef field list.
        let all = "AbilityDef(
    projectile: (key: \"item.arrow\", consumable: true, ),
)";
        let all_line = all.lines().nth(1).unwrap();
        let all_position = Position::new(1, (all_line.find(", )").unwrap() + 1) as u32);
        assert!(generate_field_completions(all, all_position, &spec).is_empty());
    }

    /// A trailing comma in a struct variant payload is a field-name position, not
    /// the value of the field before it: only the missing Ball field is offered.
    #[test]
    fn variant_payload_after_comma_completes_missing_fields() {
        let shape = type_of(
            "Shape",
            TypeKind::Enum(vec![EnumVariant {
                name: "Ball".to_string(),
                fields: vec![
                    field("range", "String"),
                    field("speed", "String"),
                    field("radius", "String"),
                    field("blast_speed", "String"),
                    field("falloff", "String"),
                ],
                docs: None,
                line: None,
                column: None,
            }]),
        );
        let content = "Root(
    shape: Ball(range: \"14\", speed: \"12\", radius: \"2\", falloff: \"0.5\", ),
)";
        let line = content.lines().nth(1).unwrap();
        let position = Position::new(1, (line.find(", )").unwrap() + 1) as u32);

        assert_eq!(
            get_completion_context(content, position),
            CompletionContext::FieldName
        );
        let items = generate_field_completions(content, position, &shape);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["blast_speed"]);
        assert_eq!(items[0].insert_text.as_deref(), Some("blast_speed: "));
    }

    /// A trailing comma inside a named struct variant's payload belongs to the
    /// variant, not to the field that holds it: `MeleeSimple`'s own fields are
    /// completed, not the `AbilityDef` fields around `shape`.
    #[tokio::test]
    async fn named_variant_payload_after_comma_offers_its_own_fields() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "AbilityDef",
                TypeKind::Struct(vec![field("key", "String"), field("shape", "Shape")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Shape",
                TypeKind::Enum(vec![EnumVariant {
                    name: "MeleeSimple".to_string(),
                    fields: vec![field("range", "String"), field("min_windup", "String")],
                    docs: None,
                    line: None,
                    column: None,
                }]),
            ))
            .await;

        let content = "[\n    (\n        key: \"ability.sting\",\n        shape: MeleeSimple(range: \"stat.combat.reach\", ),\n    ),\n]";
        let line = content.lines().nth(3).unwrap();
        let position = Position::new(3, (line.find(", )").unwrap() + 1) as u32);

        assert_eq!(
            path_at_position(content, position),
            Some(vec![
                Step::Field("shape".into()),
                Step::VariantFields {
                    name: "MeleeSimple".into()
                },
            ])
        );

        let found = field_at_position(content, position, "AbilityDef", analyzer.clone())
            .await
            .expect("owner of the field list");
        assert_eq!(found.owner_type.name, "Shape::MeleeSimple");

        let items =
            generate_completions_for_type(content, position, &found.owner_type, analyzer).await;
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["min_windup"]);
    }

    /// An empty named struct variant payload (`MeleeSimple(|)`) already sits in
    /// the variant's field list: its fields are offered before anything is typed.
    #[tokio::test]
    async fn named_variant_empty_payload_offers_its_own_fields() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "AbilityDef",
                TypeKind::Struct(vec![field("key", "String"), field("shape", "Shape")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Shape",
                TypeKind::Enum(vec![EnumVariant {
                    name: "MeleeSimple".to_string(),
                    fields: vec![field("range", "String"), field("min_windup", "String")],
                    docs: None,
                    line: None,
                    column: None,
                }]),
            ))
            .await;

        let content =
            "[\n    (\n        key: \"ability.sting\",\n        shape: MeleeSimple(),\n    ),\n]";
        let line = content.lines().nth(3).unwrap();
        let position = Position::new(
            3,
            (line.find("MeleeSimple(").unwrap() + "MeleeSimple(".len()) as u32,
        );

        assert!(is_field_name_position(content, position));
        assert_eq!(
            path_at_position(content, position),
            Some(vec![
                Step::Field("shape".into()),
                Step::VariantFields {
                    name: "MeleeSimple".into()
                },
            ])
        );

        let found = field_at_position(content, position, "AbilityDef", analyzer.clone())
            .await
            .expect("owner of the field list");
        assert_eq!(found.owner_type.name, "Shape::MeleeSimple");

        let items =
            generate_completions_for_type(content, position, &found.owner_type, analyzer).await;
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["range", "min_windup"]);
    }

    /// An empty tuple variant payload (`FirstParentOf(|)`) reaches the variant
    /// through the tree but has no named field list to complete.
    #[tokio::test]
    async fn empty_tuple_variant_payload_has_no_field_list() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "Root",
                TypeKind::Struct(vec![field("apply_rule", "ApplyRule")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "ApplyRule",
                TypeKind::Enum(vec![EnumVariant {
                    name: "FirstParentOf".to_string(),
                    fields: vec![field("0", "Vec<Species>")],
                    docs: None,
                    line: None,
                    column: None,
                }]),
            ))
            .await;

        let content = "Root(\n    apply_rule: FirstParentOf(),\n)";
        let line = content.lines().nth(1).unwrap();
        let position = Position::new(
            1,
            (line.find("FirstParentOf(").unwrap() + "FirstParentOf(".len()) as u32,
        );

        assert!(is_field_name_position(content, position));
        assert!(field_at_position(content, position, "Root", analyzer)
            .await
            .is_none());
    }

    /// The cursor context: a type in the value position, a value, or a field name.
    #[test]
    fn completion_context_follows_the_cursor() {
        let value = "Root(\n    kind: Va,\n)";
        assert_eq!(
            get_completion_context(value, Position::new(1, 11)),
            CompletionContext::StructType
        );
        // The same line with the cursor inside the field name, and right on its colon.
        assert_eq!(
            get_completion_context(value, Position::new(1, 5)),
            CompletionContext::FieldName
        );
        assert_eq!(
            get_completion_context(value, Position::new(1, 8)),
            CompletionContext::FieldName
        );

        // An array value is not a type name.
        let array = "Root(\n    kind: [\n    ],\n)";
        assert_eq!(
            get_completion_context(array, Position::new(2, 2)),
            CompletionContext::FieldValue
        );

        // No field under the cursor at all.
        assert_eq!(
            get_completion_context("", Position::new(0, 0)),
            CompletionContext::FieldName
        );
    }

    /// A field with an empty value after a nested array keeps the value context.
    ///
    /// Tree-sitter parses `apply_rule:` by taking the next field name as its
    /// value; completion must still resolve the field from the line under the
    /// cursor, not fall back to the outer struct's field list.
    #[test]
    fn empty_field_after_nested_array_is_value_context() {
        let content = r#"[
    (
        key: "ability.sting",
        damage: [
            (
                damage_type_key: "x",
                delivery: [Melee],
                formula: "1",
            ),
        ],
        apply_rule:
        effects: [(key: "effect.poisoned")],
    ),
]"#;
        let position = Position::new(10, 19);
        assert_eq!(
            get_completion_context(content, position),
            CompletionContext::FieldValue
        );
        assert_eq!(
            find_current_field(content, position),
            Some("apply_rule".to_string())
        );
    }

    /// The same empty value is recognized after a simple array field.
    #[test]
    fn empty_field_after_simple_array_is_value_context() {
        let content = r#"[
    (
        key: "ability.melee",
        usable_slots: ["slot.limb.hand"],
        apply_rule:
        delivery: [Melee],
    ),
]"#;
        let position = Position::new(4, 19);
        assert_eq!(
            get_completion_context(content, position),
            CompletionContext::FieldValue
        );
        assert_eq!(
            find_current_field(content, position),
            Some("apply_rule".to_string())
        );
    }

    /// The value completion for `apply_rule:` is the enum variants, not fields.
    #[tokio::test]
    async fn empty_apply_rule_completion_offers_enum_variants() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "ApplyRule",
                TypeKind::Enum(vec![variant("AnyChildOf"), variant("FirstParentOf")]),
            ))
            .await;
        let owner = type_of(
            "Root",
            TypeKind::Struct(vec![field("apply_rule", "Option<ApplyRule>")]),
        );
        analyzer.insert_type_for_test(owner).await;
        let content = r#"[
    (
        damage: [],
        apply_rule:
        effects: [(key: "effect.poisoned")],
    ),
]"#;

        let position = Position::new(3, 18);
        let items = completions_for_field_path(content, position, "Root", analyzer)
            .await
            .unwrap();
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["AnyChildOf", "FirstParentOf"]);
    }

    /// The type name before an opening paren is read from the line under the cursor.
    #[test]
    fn current_type_context_reads_the_name_before_the_paren() {
        assert_eq!(
            find_current_type_context("Root(\n    items: Item(\n", Position::new(1, 16)),
            Some("Item".to_string())
        );
        assert_eq!(
            find_current_type_context("Root(\n    Item(\n", Position::new(1, 9)),
            Some("Item".to_string())
        );
        // A lowercase name is not a type, and the line may not exist at all.
        assert_eq!(
            find_current_type_context("Root(\n    items: item(\n", Position::new(1, 16)),
            None
        );
        assert_eq!(
            find_current_type_context("Root(\n", Position::new(9, 0)),
            None
        );
    }

    /// The field under the cursor is taken from the enclosing `field` node.
    #[test]
    fn current_field_is_taken_from_the_enclosing_field_node() {
        let content = "Root(\n    kind: Va,\n)";
        assert_eq!(
            find_current_field(content, Position::new(1, 11)),
            Some("kind".to_string())
        );
        // Inside the field name the same `field` node is found.
        assert_eq!(
            find_current_field(content, Position::new(1, 5)),
            Some("kind".to_string())
        );
        // The value is not typed yet: `kind:` stays in an ERROR and there is no `field` node.
        assert_eq!(
            find_current_field("Root(\n    kind: \n)", Position::new(1, 8)),
            None
        );
        assert_eq!(find_current_field("", Position::new(0, 0)), None);
    }

    /// A string is detected by the node the cursor sits in.
    #[test]
    fn cursor_inside_string_matches_the_string_node() {
        let content = "Root(\n    kind: \"abc\",\n)";
        assert!(cursor_inside_string(content, Position::new(1, 12)));
        // After the closing quote the cursor is back in the field.
        assert!(!cursor_inside_string(content, Position::new(1, 15)));
        assert!(!cursor_inside_string(
            "Root(\n    kind: Va,\n)",
            Position::new(1, 11)
        ));
    }

    /// The field name is read from the text before the cursor.
    #[test]
    fn field_name_left_of_cursor_reads_the_identifier_before_the_colon() {
        assert_eq!(
            field_name_left_of_cursor("    kind: ", 10),
            Some("kind".to_string())
        );
        assert_eq!(
            field_name_left_of_cursor("    kind:", 9),
            Some("kind".to_string())
        );
        assert_eq!(
            field_name_left_of_cursor("items: [\n    kind: Va", 20),
            None,
            "a non-empty value is already in the tree"
        );
        // No colon, a name that is not an identifier, nothing before the cursor, or an
        // offset past the end of the text.
        assert_eq!(field_name_left_of_cursor("    items", 9), None);
        assert_eq!(field_name_left_of_cursor("    1: ", 7), None);
        assert_eq!(field_name_left_of_cursor("    a-b: ", 9), None);
        assert_eq!(field_name_left_of_cursor("kind: ", 0), None);
        assert_eq!(field_name_left_of_cursor("kind: ", 100), None);
    }

    /// Wrappers are stripped down to the payload type.
    #[test]
    fn unwrap_wrapper_strips_known_wrappers() {
        assert_eq!(unwrap_wrapper("Vec<Kind>"), "Kind");
        assert_eq!(unwrap_wrapper("Option<Vec<Direction>>"), "Direction");
        assert_eq!(unwrap_wrapper("Box< Option< Inner > >"), "Inner");
        assert_eq!(unwrap_wrapper("Rc<Arc<Kind>>"), "Kind");
        assert_eq!(unwrap_wrapper("Kind"), "Kind");
        assert_eq!(unwrap_wrapper(""), "");
        // No closing `>` — nothing to unwrap.
        assert_eq!(unwrap_wrapper("Vec<Kind"), "Vec<Kind");
        // Spaces are dropped even for a type that is not a wrapper.
        assert_eq!(
            unwrap_wrapper("HashMap<String, Kind>"),
            "HashMap<String,Kind>"
        );
    }

    /// A field declared as `Vec<Delivery>` resolves to `Delivery`: goto
    /// definition on `delivery: [Melee]` must find the `Melee` variant.
    #[tokio::test]
    async fn field_type_info_unwraps_list_elements() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "Delivery",
                TypeKind::Enum(vec![variant("Melee"), variant("Ranged")]),
            ))
            .await;

        let info = field_type_info(&analyzer, &field("delivery", "Vec<Delivery>"))
            .await
            .expect("element type");
        assert_eq!(info.name, "Delivery");
        assert!(info.find_variant("Melee").is_some());
    }

    /// The inner type of a generic is cut out of the text of the type.
    #[test]
    fn extract_inner_type_unwraps_one_level() {
        assert_eq!(extract_inner_type("Vec<Kind>"), "Kind");
        assert_eq!(extract_inner_type("Option< Vec< Kind > >"), "Vec<Kind>");
        assert_eq!(extract_inner_type("HashMap<String, Kind>"), "String,Kind");
        assert_eq!(extract_inner_type("Kind"), "Kind");
        // Without a closing `>` the original text is returned unchanged.
        assert_eq!(extract_inner_type("Option< Kind"), "Option< Kind");
    }

    /// A type completion carries the snippet of the struct or the enum it describes.
    #[test]
    fn create_type_completion_builds_a_snippet() {
        let mut owner = type_of(
            "module::Root",
            TypeKind::Struct(vec![field("items", "Vec<Item>"), field("key", "String")]),
        );
        owner.docs = Some("Root docs".to_string());

        let item = create_type_completion(&owner);
        assert_eq!(item.label, "Root");
        assert_eq!(item.kind, Some(CompletionItemKind::STRUCT));
        assert_eq!(item.detail.as_deref(), Some("struct module::Root"));
        assert_eq!(
            item.insert_text.as_deref(),
            Some("Root(\n    items: ${1},\n    key: ${2},\n)")
        );
        assert_eq!(item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        match item.documentation {
            Some(Documentation::MarkupContent(markup)) => assert_eq!(markup.value, "Root docs"),
            other => panic!("expected markdown documentation, got {other:?}"),
        }

        // An enum offers its name only: the variants are suggested separately.
        let kind = create_type_completion(&type_of(
            "module::Kind",
            TypeKind::Enum(vec![variant("Any")]),
        ));
        assert_eq!(kind.label, "Kind");
        assert_eq!(kind.kind, Some(CompletionItemKind::ENUM));
        assert_eq!(kind.insert_text.as_deref(), Some("Kind($0)"));
    }

    /// Tree helpers: text slicing, the ancestor walk, and the values of an open paren.
    #[test]
    fn tree_helpers_read_the_parsed_ron() {
        use crate::ts_utils::{self, RonParser};

        let content = "Root(\n    items: [ (Kind, Direction,) ],\n)";
        let mut parser = RonParser::new();
        let tree = parser.parse(content).expect("tree");
        let root = tree.root_node();

        assert_eq!(node_text(&root, content), content);

        let offset = ts_utils::position_to_byte_offset(content, Position::new(1, 17));
        let element = root
            .descendant_for_byte_range(offset, offset)
            .expect("node at the cursor");
        assert_eq!(node_text(&element, content), "Kind");

        let mut tuple = element;
        while tuple.kind() != "tuple" {
            tuple = tuple.parent().expect("enclosing tuple");
        }
        assert_eq!(node_text(&tuple, content), "(Kind, Direction,)");

        // The ancestor walk goes upwards only, and a node is its own ancestor.
        assert!(node_is_ancestor_or_self(root, element));
        assert!(node_is_ancestor_or_self(tuple, element));
        assert!(node_is_ancestor_or_self(element, element));
        assert!(!node_is_ancestor_or_self(element, tuple));
        assert!(!node_is_ancestor_or_self(element, root));
        let second = tuple.named_child(1).expect("second tuple element");
        assert!(!node_is_ancestor_or_self(second, element));

        // Both positional values of the open paren are listed in order.
        let values = values_in_open_paren(&tuple, offset).expect("values in the open paren");
        let texts: Vec<&str> = values
            .iter()
            .map(|value| node_text(value, content))
            .collect();
        assert_eq!(texts, vec!["Kind", "Direction"]);

        // Once the closing paren is behind the offset the group is complete.
        let after = ts_utils::position_to_byte_offset(content, Position::new(1, 31));
        assert!(values_in_open_paren(&tuple, after).is_none());

        // A struct in parentheses is not a tuple: its values are named fields,
        // returned so a named variant can keep its variant step.
        let mut group = element;
        while group.kind() != "struct" {
            group = group.parent().expect("enclosing struct");
        }
        let named = values_in_open_paren(&group, offset).expect("named fields");
        assert!(named.iter().all(|value| value.kind() == "field"));
    }

    /// Value completions of a field come from the type written in it.
    #[tokio::test]
    async fn value_completions_follow_the_field_type() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "Kind",
                TypeKind::Enum(vec![variant("Any"), variant("Object")]),
            ))
            .await;

        let owner = type_of("Root", TypeKind::Struct(vec![field("kind", "Kind")]));
        let items =
            generate_value_completions_for_field("kind".to_string(), &owner, analyzer.clone())
                .await;
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["Any", "Object"]);

        // A field the type does not have gives nothing.
        assert!(
            generate_value_completions_for_field("missing".to_string(), &owner, analyzer)
                .await
                .is_empty()
        );
    }

    /// Value completions of primitive types and containers.
    #[tokio::test]
    async fn value_completions_cover_primitive_types() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let labels = |items: Vec<CompletionItem>| {
            items
                .into_iter()
                .map(|item| item.label)
                .collect::<Vec<String>>()
        };

        assert_eq!(
            labels(generate_value_completions_by_type("bool", analyzer.clone()).await),
            vec!["true", "false"]
        );
        assert_eq!(
            labels(generate_value_completions_by_type("String", analyzer.clone()).await),
            vec!["\"\""]
        );
        assert_eq!(
            labels(generate_value_completions_by_type("u32", analyzer).await),
            vec!["0"]
        );
    }

    /// A field whose type is a custom type offers that type.
    #[tokio::test]
    async fn type_completions_follow_the_field_type() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "Item",
                TypeKind::Struct(vec![field("key", "String")]),
            ))
            .await;

        let owner = type_of("Root", TypeKind::Struct(vec![field("items", "Vec<Item>")]));
        let items =
            generate_type_completions_for_field("items".to_string(), &owner, analyzer.clone())
                .await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Item");
        assert_eq!(items[0].kind, Some(CompletionItemKind::STRUCT));

        assert!(
            generate_type_completions_for_field("missing".to_string(), &owner, analyzer)
                .await
                .is_empty()
        );
    }

    /// Every type of the workspace is offered as a type completion.
    #[tokio::test]
    async fn workspace_types_are_offered() {
        let analyzer = Arc::new(RustAnalyzer::new());
        assert!(get_all_workspace_types(analyzer.clone()).await.is_empty());

        analyzer
            .insert_type_for_test(type_of(
                "Root",
                TypeKind::Struct(vec![field("key", "String")]),
            ))
            .await;

        let items = get_all_workspace_types(analyzer).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Root");
        assert_eq!(items[0].kind, Some(CompletionItemKind::STRUCT));
    }

    /// A named struct variant's fields are reachable through the tree path:
    /// `Dodge(…, evasion: …)` names the field directly instead of a payload slot.
    #[test]
    fn named_variant_field_path_keeps_the_variant_step() {
        let content = "[\n    Dodge(key: \"defense.dodge.base\", evasion: \"holder.stat.combat.evasion\"),\n]";
        let character = content.lines().nth(1).unwrap().find("evasion").unwrap() as u32 + 1;
        let path = path_at_position(content, Position::new(1, character)).expect("path");
        assert_eq!(
            path,
            vec![
                Step::Variant {
                    name: "Dodge".into(),
                    index: 1,
                },
                Step::Field("evasion".into()),
            ]
        );
    }

    /// The step into a named struct variant exposes that variant's fields; the
    /// following field step resolves against them instead of the enum itself.
    #[tokio::test]
    async fn named_variant_step_exposes_its_fields() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let dodge = EnumVariant {
            name: "Dodge".to_string(),
            fields: vec![field("key", "String"), field("evasion", "String")],
            docs: Some("Уворот".to_string()),
            line: Some(4),
            column: Some(0),
        };
        let top = type_of("crate::models::DefenseDef", TypeKind::Enum(vec![dodge]));
        analyzer.insert_type_for_test(top.clone()).await;

        let step = Step::Variant {
            name: "Dodge".into(),
            index: 1,
        };
        let next = step_type(&analyzer, &top, &step)
            .await
            .expect("variant type");
        assert_eq!(next.name, "crate::models::DefenseDef::Dodge");
        assert_eq!(next.fields().map(Vec::len), Some(2));
        assert_eq!(
            field_type_name(&analyzer, &top, &Step::Field("evasion".into()))
                .await
                .as_deref(),
            Some("String")
        );
    }

    /// The whole tree path resolves a named variant field, and the returned
    /// owner carries the variant fields for field-name completion.
    #[tokio::test]
    async fn named_variant_field_resolves_through_tree() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let dodge = EnumVariant {
            name: "Dodge".to_string(),
            fields: vec![
                field("key", "String"),
                FieldInfo {
                    name: "evasion".to_string(),
                    type_name: "String".to_string(),
                    docs: Some("Формула рейтинга уворота".to_string()),
                    line: None,
                    column: None,
                    has_default: false,
                },
            ],
            docs: Some("Уворот".to_string()),
            line: Some(4),
            column: Some(0),
        };
        let top = type_of("crate::models::DefenseDef", TypeKind::Enum(vec![dodge]));
        analyzer.insert_type_for_test(top).await;

        let content = "[\n    Dodge(key: \"defense.dodge.base\", evasion: \"holder.stat.combat.evasion\"),\n]";
        let character = content.lines().nth(1).unwrap().find("evasion").unwrap() as u32 + 1;
        let found = field_at_position(
            content,
            Position::new(1, character),
            "crate::models::DefenseDef",
            analyzer,
        )
        .await
        .expect("field under cursor");

        assert_eq!(found.name, "evasion");
        assert_eq!(found.docs.as_deref(), Some("Формула рейтинга уворота"));
        assert_eq!(found.owner_type.name, "crate::models::DefenseDef::Dodge");
        assert_eq!(found.owner_type.fields().map(Vec::len), Some(2));
    }

    /// A record inside `damage: [ ( … ) ]` belongs to the element type:
    /// its fields resolve to `DamageEntry`, not to the document type.
    #[tokio::test]
    async fn nested_record_fields_resolve_through_the_list_field() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of("Delivery", TypeKind::Enum(vec![variant("Melee")])))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "DamageEntry",
                TypeKind::Struct(vec![
                    field("damage_type_key", "String"),
                    field("delivery", "Vec<Delivery>"),
                    field("formula", "String"),
                ]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "AbilityDef",
                TypeKind::Struct(vec![
                    field("key", "String"),
                    field("damage", "Vec<DamageEntry>"),
                ]),
            ))
            .await;

        let content = "[\n    (\n        damage: [\n            (\n                damage_type_key: \"damage_type.physic.pierce\",\n                delivery: [Melee],\n                formula: \"1\",\n            ),\n        ],\n    ),\n]";
        let line_of = |needle: &str| {
            content
                .lines()
                .position(|line| line.contains(needle))
                .unwrap() as u32
        };
        let char_of = |needle: &str| {
            content
                .lines()
                .nth(line_of(needle) as usize)
                .unwrap()
                .find(needle)
                .unwrap() as u32
                + 1
        };

        let field = field_at_position(
            content,
            Position::new(line_of("damage_type_key"), char_of("damage_type_key")),
            "AbilityDef",
            analyzer.clone(),
        )
        .await
        .expect("damage_type_key field");
        assert_eq!(field.name, "damage_type_key");
        assert_eq!(field.owner_type.name, "DamageEntry");

        let delivery = field_at_position(
            content,
            Position::new(line_of("delivery"), char_of("delivery")),
            "AbilityDef",
            analyzer,
        )
        .await
        .expect("delivery field");
        assert_eq!(delivery.name, "delivery");
        assert_eq!(delivery.owner_type.name, "DamageEntry");
        assert_eq!(delivery.type_name, "Vec<Delivery>");
    }
}
