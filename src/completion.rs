use crate::rust_analyzer::{RustAnalyzer, TypeInfo, TypeKind};
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

/// Determine what we're completing based on cursor position using tree-sitter
fn get_completion_context(content: &str, position: Position) -> CompletionContext {
    use crate::ts_utils::{self, RonParser};

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
            // Get fields already used in the RON file
            let used_fields = tree_sitter_parser::extract_fields_from_ron(content);

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
                    let used_fields = tree_sitter_parser::extract_fields_from_ron(content);
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

/// Разворачивает обёртки `Vec<…>`, `Option<…>`, `Box<…>` до полезного типа.
fn unwrap_wrapper(type_name: &str) -> String {
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

/// Шаг пути от корня файла до места под курсором.
#[derive(Debug, PartialEq)]
enum Step {
    /// Поле структуры по имени.
    Field(String),
    /// Элемент кортежа: у структуры-кортежа поля зовутся "0", "1", …, поэтому
    /// позиция значения и есть имя поля.
    Element(usize),
}

/// Является ли `ancestor` предком `node` (или самим узлом).
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

/// Путь шагов от корня файла до места под курсором.
fn path_at_position(content: &str, position: Position) -> Option<Vec<Step>> {
    use crate::ts_utils::{self, RonParser};

    let mut parser = RonParser::new();
    let tree = parser.parse(content)?;

    // Курсор в редакторе стоит за последним набранным символом, поэтому позиция
    // может попасть на пробел или запятую: тогда узел берётся по байту перед
    // курсором, если он глубже — так `delivery: [ ],` и `(Ef|) ` находят своё поле.
    let offset = ts_utils::position_to_byte_offset(content, position);
    let root = tree.root_node();
    let after = root.descendant_for_byte_range(offset, offset);
    let before_offset = offset.saturating_sub(1);
    let before = root.descendant_for_byte_range(before_offset, before_offset);
    // Значение перед курсором важнее пунктуации под ним: `(Ef|` и `(Ef,|` — это
    // одно и то же место пути, а запятая сама по себе ничего не говорит.
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
        match node.kind() {
            "field" => {
                if let Some(name) = ts_utils::field_name(&node, content) {
                    path.push(Step::Field(name.to_string()));
                }
            }
            "tuple" => {
                // Номер значения, внутри которого стоит курсор: правило цели
                // `(Effect, Up)` — структура-кортеж, её поля позиционные.
                let values = ts_utils::named_children(&node);
                // Курсор может стоять на самом кортеже (`( |` или `Effect, |`):
                // тогда позиция — первый сын, начинающийся не раньше курсора.
                if let Some(index) = values
                    .iter()
                    .position(|value| node_is_ancestor_or_self(*value, cursor))
                    .or_else(|| values.iter().position(|value| value.start_byte() >= offset))
                {
                    path.push(Step::Element(index));
                }
            }
            _ => {}
        }
        current = node.parent();
    }
    path.reverse();
    debug_log(&format!("цепочка узлов: {chain:?}"));

    // Значение ещё не набрано: курсор стоит за `mode:`, и узла-значения в дереве
    // нет. Имя поля берём из текста слева — иначе путь обрывается и подсказки
    // откатываются к списку полей структуры.
    if let Some(name) = field_name_left_of_cursor(content, offset) {
        if path.last() != Some(&Step::Field(name.clone())) {
            path.push(Step::Field(name));
        }
    }

    (!path.is_empty()).then_some(path)
}

/// Имя поля, если курсор стоит сразу за его двоеточием (`mode: |`).
fn field_name_left_of_cursor(content: &str, offset: usize) -> Option<String> {
    let left = content.get(..offset)?;
    let line = left.rsplit('\n').next()?;
    let trimmed = line.trim_end();
    let (name, _) = trimmed.rsplit_once(':')?;
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

/// Тип после шага пути: поле структуры или значение структуры-кортежа.
async fn step_type(
    analyzer: &Arc<RustAnalyzer>,
    current: &TypeInfo,
    step: &Step,
) -> Option<TypeInfo> {
    let type_name = field_type_name(analyzer, current, step).await?;
    analyzer.get_type_info(&unwrap_wrapper(&type_name)).await
}

/// Имя типа, который записан в поле (или в позиции кортежа) текущего типа.
async fn field_type_name(
    analyzer: &Arc<RustAnalyzer>,
    current: &TypeInfo,
    step: &Step,
) -> Option<String> {
    // Массив в RON пишется как есть: `target: [ (Effect, Up) ]` — шаг пути
    // относится к элементу массива, поэтому обёртки (`Vec<T>`, `Option<T>`)
    // снимаются до применения шага.
    let base = analyzer
        .get_type_info(&unwrap_wrapper(&current.name))
        .await
        .unwrap_or_else(|| current.clone());

    match step {
        Step::Field(name) => Some(
            base.fields()?
                .iter()
                .find(|field| &field.name == name)?
                .type_name
                .clone(),
        ),
        Step::Element(index) => Some(base.fields()?.get(*index)?.type_name.clone()),
    }
}

/// Временный журнал для разбора подсказок: включается `RON_LSP_DEBUG_COMPLETION`.
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

/// Подсказки для значения, найденного по пути от курсора вверх.
///
/// Навигация по контекстам аннотаций не спускается ни в `Vec<T>`, ни во вложенные
/// структуры, ни в кортежи: `delivery` живёт в `DamageEntry` внутри `damage: [...]`,
/// а правило цели — в структуре-кортеже `FlagReach`. Здесь путь берётся из дерева
/// (имена полей и номера значений кортежей от корня до курсора), каждый шаг
/// разворачивает обёртки и берёт тип у текущей структуры; на месте значения
/// предлагаются варианты перечисления — и только они.
pub async fn completions_for_field_path(
    content: &str,
    position: Position,
    top_type: &str,
    analyzer: Arc<RustAnalyzer>,
) -> Option<Vec<CompletionItem>> {
    let path = path_at_position(content, position)?;
    debug_log(&format!("путь: {path:?}"));
    let (last, enclosing) = path.split_last()?;

    let mut current = analyzer.get_type_info(top_type).await?;
    debug_log(&format!("корень {top_type} -> {}", current.name));
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
                    "  {step:?} -> РАЗРЫВ (текущий {}: {fields:?})",
                    current.name
                ));
                return None;
            }
        }
    }

    // Тип последнего шага известен: отвечаем сами — «все типы проекта» здесь
    // были бы шумом. Перечисление — варианты, структура — её поля, примитив
    // (`String`, числа) — ничего.
    let Some(type_name) = field_type_name(&analyzer, &current, last).await else {
        debug_log(&format!("  последний {last:?} -> поля нет"));
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
    /// Путь до поля собирается из дерева, а его тип разворачивается через `Vec`.
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
                "DocTest",
                TypeKind::Struct(vec![field("damage", "Vec<DamageEntry>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "DamageEntry",
                TypeKind::Struct(vec![field("delivery", "Vec<Delivery>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Delivery",
                TypeKind::Enum(vec![
                    EnumVariant {
                        name: "Melee".to_string(),
                        fields: vec![],
                        docs: None,
                        line: None,
                        column: None,
                    },
                    EnumVariant {
                        name: "Ranged".to_string(),
                        fields: vec![],
                        docs: None,
                        line: None,
                        column: None,
                    },
                ]),
            ))
            .await;

        let content = "DocTest(
    damage: [
        (
            delivery: [
                
            ],
        ),
    ],
)";
        let path = path_at_position(content, Position::new(4, 16)).expect("путь до поля");
        assert_eq!(
            path,
            vec![Step::Field("damage".into()), Step::Field("delivery".into())]
        );

        let items = completions_for_field_path(content, Position::new(4, 16), "DocTest", analyzer)
            .await
            .expect("подсказки для перечисления");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["Melee", "Ranged"]);
    }

    /// Правило цели — структура-кортеж: курсор внутри значения кортежа, и вариант
    /// перечисления берётся по позиции этого значения.
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
                "EffectTest",
                TypeKind::Struct(vec![field("flags", "Vec<FlagUse>")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "FlagUse",
                TypeKind::Struct(vec![
                    field("key", "String"),
                    field("target", "Vec<FlagReach>"),
                ]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "FlagReach",
                TypeKind::Struct(vec![field("0", "FlagTarget"), field("1", "FlagDirection")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "FlagTarget",
                TypeKind::Enum(vec![
                    variant("Any"),
                    variant("Creature"),
                    variant("Item"),
                    variant("Effect"),
                ]),
            ))
            .await;

        let content = "EffectTest(
    flags: [
        (
            key: \"flag.ability_add\",
            target: [
                (
                    Ef,
                    Up,
                ),
            ],
        ),
    ],
)";
        let path = path_at_position(content, Position::new(6, 21)).expect("путь до правила");
        assert_eq!(
            path,
            vec![
                Step::Field("flags".into()),
                Step::Field("target".into()),
                Step::Element(0),
            ]
        );

        let items =
            completions_for_field_path(content, Position::new(6, 21), "EffectTest", analyzer)
                .await
                .expect("варианты цели");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["Any", "Creature", "Item", "Effect"]);

        // Значение ещё не набрано: курсор стоит на самом кортеже — позиция берётся
        // по первому сыну, который начинается не раньше курсора.
        let on_tuple = path_at_position(content, Position::new(5, 17)).expect("путь до кортежа");
        assert_eq!(
            on_tuple,
            vec![
                Step::Field("flags".into()),
                Step::Field("target".into()),
                Step::Element(0),
            ]
        );
    }
}
