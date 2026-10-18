#[cfg(feature = "cli")]
use crate::diagnostic_reporter;
use crate::rust_analyzer::{EnumVariant, FieldInfo, RustAnalyzer, TypeInfo, TypeKind};
use crate::tree_sitter_parser;
use crate::ts_utils::ParsedEnumVariant;
use ron::Value;
use std::sync::Arc;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tree_sitter::Node;

/// Validate RON syntax only (portable diagnostics).
///
/// Used for files that have no type to validate against: a file still has to be
/// valid RON, and skipping it silently hides real syntax errors.
#[cfg(feature = "cli")]
pub fn validate_ron_syntax_portable(content: &str) -> Vec<diagnostic_reporter::Diagnostic> {
    lsp_diagnostics_to_portable(&validate_ron_syntax(content))
}

/// Validate RON with access to RustAnalyzer for recursive type lookups (returns portable diagnostics)
#[cfg(feature = "cli")]
pub async fn validate_ron_portable(
    content: &str,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<diagnostic_reporter::Diagnostic> {
    let lsp_diagnostics = validate_ron_with_analyzer(content, type_info, analyzer).await;
    lsp_diagnostics_to_portable(&lsp_diagnostics)
}

/// Validate RON with access to RustAnalyzer for recursive type lookups
pub async fn validate_ron_with_analyzer(
    content: &str,
    type_info: &TypeInfo,
    analyzer: Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Parse RON once and check for syntax errors from the result
    // Try to parse the RON content
    let parsed_value = ron::from_str::<Value>(content);

    // If parsing failed, return syntax error
    if let Err(e) = &parsed_value {
        let error_msg = e.to_string();
        let simplified_msg = simplify_ron_error(&error_msg);

        diagnostics.push(Diagnostic {
            range: syntax_error_range(content, &error_msg),
            severity: Some(DiagnosticSeverity::ERROR),
            message: simplified_msg,
            ..Default::default()
        });
        return diagnostics;
    }

    if analyzer.has_custom_deserializer(&type_info.name).await {
        return diagnostics;
    }

    match &type_info.kind {
        TypeKind::Struct(fields) => {
            // The top level is either a single value or an array of values (data
            // registries often hold arrays): validate every element, not just the root.
            match parsed_value.as_ref() {
                Ok(Value::Seq(items)) => {
                    for (index, item) in items.iter().enumerate() {
                        diagnostics.extend(
                            validate_struct_fields(
                                content,
                                fields,
                                Some(item),
                                type_info.has_default,
                                Some(index),
                                Some(&analyzer),
                            )
                            .await,
                        );
                    }
                }
                other => {
                    diagnostics.extend(
                        validate_struct_fields(
                            content,
                            fields,
                            other.ok(),
                            type_info.has_default,
                            None,
                            Some(&analyzer),
                        )
                        .await,
                    );
                }
            }
        }
        TypeKind::Enum(variants) => {
            diagnostics.extend(
                validate_enum_variant_with_fields(content, variants, type_info, &analyzer).await,
            );
        }
    }

    // Check for enum variant fields (scans the whole file, so only call once)
    diagnostics
        .extend(validate_enum_variant_fields_in_structs(content, type_info, &analyzer).await);

    // Deduplicate diagnostics by message and position
    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|d| {
        let key = (
            d.range.start.line,
            d.range.start.character,
            d.message.clone(),
        );
        seen.insert(key)
    });

    diagnostics
}

/// Validate enum variant fields within struct fields using the same logic as goto_definition
async fn validate_enum_variant_fields_in_structs(
    content: &str,
    type_info: &TypeInfo,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut reported_errors = std::collections::HashSet::new();

    // Collect all variant locations and group by variant to check for missing fields
    let variant_locations = tree_sitter_parser::find_all_variant_field_locations(content);
    let mut variant_info: std::collections::HashMap<
        (String, String),
        (usize, std::collections::HashSet<String>),
    > = std::collections::HashMap::new();

    // Cache for variant type lookups: (containing_field_name, variant_name) -> Option<EnumVariant>
    let mut variant_cache: std::collections::HashMap<(String, String), Option<EnumVariant>> =
        std::collections::HashMap::new();

    // First pass: collect all fields present for each variant
    let lines: Vec<&str> = content.lines().collect();
    for location in &variant_locations {
        let key = (
            location.containing_field_name.clone(),
            location.variant_name.clone(),
        );
        let entry = variant_info
            .entry(key)
            .or_insert((location.line_idx, std::collections::HashSet::new()));
        if let Some(ref field_at_pos) = location.field_at_position {
            entry.1.insert(field_at_pos.clone());
        }
    }

    // Second pass: validate unknown fields - do navigation ONCE per unique variant
    for ((containing_field_name, variant_name), (first_line, _present_fields)) in &variant_info {
        // Check if we've already looked up this variant
        if variant_cache.contains_key(&(containing_field_name.clone(), variant_name.clone())) {
            continue;
        }

        // Do the expensive navigation once per unique variant
        let position = Position::new(*first_line as u32, 0);
        let mut contexts = vec![tree_sitter_parser::TypeContext {
            type_name: type_info
                .name
                .split("::")
                .last()
                .unwrap_or(&type_info.name)
                .to_string(),
        }];
        let mut position_contexts =
            tree_sitter_parser::find_type_context_at_position(content, position);
        if !position_contexts.is_empty() {
            position_contexts.pop();
        }
        contexts.extend(position_contexts);

        let mut current_type_info = Some(type_info.clone());
        for context in contexts.iter().skip(1) {
            let info = match current_type_info {
                Some(ref info) => info.clone(),
                None => break,
            };

            if let Some(fields) = info.fields() {
                let context_name = &context.type_name;
                if let Some(field) = fields.iter().find(|f| {
                    let field_type_last = f.type_name.split("::").last().unwrap_or(&f.type_name);
                    let field_type_base =
                        field_type_last.split('<').next().unwrap_or(field_type_last);
                    field_type_base == context_name
                }) {
                    current_type_info = analyzer.get_type_info(&field.type_name).await;
                    continue;
                }
            }

            let direct_lookup = analyzer.get_type_info(&context.type_name).await;
            if direct_lookup.is_some() {
                current_type_info = direct_lookup;
            } else {
                let mut found_via_variant = false;
                if let Some(variant) = info.find_variant(&context.type_name) {
                    if variant.fields.len() == 1 {
                        let field_type = &variant.fields[0].type_name;
                        current_type_info = analyzer.get_type_info(field_type).await;
                        found_via_variant = true;
                    }
                }
                if !found_via_variant {
                    if let Some(fields) = info.fields() {
                        for field in fields {
                            if let Some(field_type_info) =
                                analyzer.get_type_info(&field.type_name).await
                            {
                                if field_type_info.find_variant(&context.type_name).is_some() {
                                    current_type_info = Some(field_type_info);
                                    found_via_variant = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                if !found_via_variant {
                    current_type_info = None;
                }
            }
        }

        // Look up the variant and cache it
        let variant = if let Some(current_type) = current_type_info {
            if let Some(field) = current_type.find_field(containing_field_name) {
                if let Some(field_type_info) = analyzer.get_type_info(&field.type_name).await {
                    field_type_info.find_variant(variant_name).cloned()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        variant_cache.insert(
            (containing_field_name.clone(), variant_name.clone()),
            variant,
        );
    }

    // Third pass: check all field locations using cached variant info
    for location in &variant_locations {
        if let Some(ref field_at_pos) = location.field_at_position {
            let error_key = (location.line_idx, field_at_pos.clone());
            if !reported_errors.contains(&error_key) {
                let cache_key = (
                    location.containing_field_name.clone(),
                    location.variant_name.clone(),
                );
                if let Some(Some(variant)) = variant_cache.get(&cache_key) {
                    if !variant.fields.iter().any(|f| f.name == *field_at_pos) {
                        let line = lines.get(location.line_idx).unwrap_or(&"");
                        if let Some(col) = line
                            .find(&format!("{}:", field_at_pos))
                            .or_else(|| line.find(&format!("{} :", field_at_pos)))
                        {
                            diagnostics.push(Diagnostic {
                                range: Range::new(
                                    Position::new(location.line_idx as u32, col as u32),
                                    Position::new(
                                        location.line_idx as u32,
                                        (col + field_at_pos.len()) as u32,
                                    ),
                                ),
                                severity: Some(DiagnosticSeverity::ERROR),
                                message: format!(
                                    "Unknown field '{}' in variant '{}'",
                                    field_at_pos, location.variant_name
                                ),
                                ..Default::default()
                            });
                            reported_errors.insert(error_key);
                        }
                    }
                }
            }
        }
    }

    // Fourth pass: check for missing required fields using cached variant info
    for ((containing_field_name, variant_name), (first_line, present_fields)) in variant_info {
        let cache_key = (containing_field_name.clone(), variant_name.clone());
        if let Some(Some(variant)) = variant_cache.get(&cache_key) {
            // Check for missing required fields
            for vfield in &variant.fields {
                if !present_fields.contains(&vfield.name) && !vfield.type_name.starts_with("Option")
                {
                    // Note: We don't have has_default info in the cached variant,
                    // but typically enum variants don't have defaults

                    // Find the variant opening line to report the error
                    // Search backwards from first_line to find the line with the variant name
                    let mut variant_line_idx = first_line;
                    let mut variant_col = 0;
                    for i in (0..=first_line).rev() {
                        if let Some(line) = lines.get(i) {
                            if let Some(col) = line.find(&variant_name) {
                                variant_line_idx = i;
                                variant_col = col;
                                break;
                            }
                        }
                    }

                    diagnostics.push(Diagnostic {
                        range: Range::new(
                            Position::new(variant_line_idx as u32, variant_col as u32),
                            Position::new(
                                variant_line_idx as u32,
                                (variant_col + variant_name.len()) as u32,
                            ),
                        ),
                        severity: Some(DiagnosticSeverity::ERROR),
                        message: format!(
                            "Missing required field '{}' in variant '{}'",
                            vfield.name, variant_name
                        ),
                        ..Default::default()
                    });
                }
            }
        }
    }

    diagnostics
}

/// Adjust diagnostic line numbers by an offset
fn adjust_diagnostic_positions(diagnostics: Vec<Diagnostic>, line_offset: u32) -> Vec<Diagnostic> {
    diagnostics
        .into_iter()
        .map(|mut d| {
            d.range.start.line += line_offset;
            d.range.end.line += line_offset;
            d
        })
        .collect()
}

/// Convert LSP diagnostics to portable format
#[cfg(feature = "cli")]
fn lsp_diagnostics_to_portable(diagnostics: &[Diagnostic]) -> Vec<diagnostic_reporter::Diagnostic> {
    diagnostics
        .iter()
        .map(|d| {
            let severity = match d.severity {
                Some(DiagnosticSeverity::ERROR) => diagnostic_reporter::Severity::Error,
                Some(DiagnosticSeverity::WARNING) => diagnostic_reporter::Severity::Warning,
                _ => diagnostic_reporter::Severity::Info,
            };

            let col_start = d.range.start.character;
            let col_end = if d.range.end.line > d.range.start.line
                || d.range.end.character <= d.range.start.character
            {
                // Multi-line range or zero-width: use col_start + 1 as fallback
                col_start + 1
            } else {
                d.range.end.character
            };

            diagnostic_reporter::Diagnostic {
                line: d.range.start.line,
                col_start,
                col_end,
                severity,
                message: d.message.clone(),
            }
        })
        .collect()
}

/// Helper function for struct validation (async version with analyzer)
/// Pick the node to validate: the root value itself, or the `element_index`-th
/// element of a root array.
///
/// A non-struct element inside an array is reported as a type mismatch; a non-struct
/// root outside an array stays unreported (the annotation may name an alias).
fn pick_struct_node<'a>(
    root: Node<'a>,
    element_index: Option<usize>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Node<'a>> {
    let target = match element_index {
        Some(index) => *crate::ts_utils::array_value_children(&root).get(index)?,
        None => root,
    };

    if target.kind() == "struct" {
        return Some(target);
    }

    if element_index.is_some() {
        diagnostics.push(Diagnostic {
            range: crate::ts_utils::node_to_lsp_range(&target),
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!("Type mismatch: expected struct, got {}", target.kind()),
            ..Default::default()
        });
    }

    None
}

async fn validate_struct_fields(
    content: &str,
    fields: &[FieldInfo],
    parsed_value: Option<&Value>,
    has_default: bool,
    element_index: Option<usize>,
    analyzer: Option<&Arc<RustAnalyzer>>,
) -> Vec<Diagnostic> {
    use crate::ts_utils::{self, RonParser};
    let mut diagnostics = Vec::new();

    // The RON-parsed map is used for primitive type checking (it understands typed values).
    let ron_map = parsed_value.and_then(extract_map_from_value);

    // Single tree-sitter parse: drives unknown-field detection, field value node access
    // (for custom-type validation via validate_field_value_node), position reporting,
    // and missing-field detection — no re-parsing or string extraction required.
    let mut parser = RonParser::new();
    if let Some(tree) = parser.parse(content) {
        if let Some(root_value) = ts_utils::find_main_value(&tree) {
            if let Some(main_value) = pick_struct_node(root_value, element_index, &mut diagnostics)
            {
                let mut present_field_names: Vec<&str> = Vec::new();
                let struct_field_nodes = ts_utils::struct_fields(&main_value);

                for field_node in struct_field_nodes {
                    let Some(field_name) = ts_utils::field_name(&field_node, content) else {
                        continue;
                    };

                    match fields.iter().find(|f| f.name == field_name) {
                        None => {
                            // Unknown field — report at the field name node
                            let range = ts_utils::node_to_lsp_range(&field_node.child(0).unwrap());
                            diagnostics.push(Diagnostic {
                                range,
                                severity: Some(DiagnosticSeverity::ERROR),
                                message: format!("Unknown field '{}'", field_name),
                                ..Default::default()
                            });
                        }
                        Some(field_info) => {
                            present_field_names.push(field_name);

                            let Some(value_node) = ts_utils::field_value(&field_node) else {
                                continue;
                            };

                            // Deep validation: Vec<T>, Option<T>, plain custom structs/enums.
                            // validate_field_value_node handles all generic-wrapper cases
                            // uniformly, so there are no per-container special cases here.
                            if let Some(analyzer) = analyzer {
                                let handled = Box::pin(validate_field_value_node(
                                    &value_node,
                                    content,
                                    &field_info.type_name,
                                    analyzer,
                                ))
                                .await;
                                // The node-based checker owns this shape — including the
                                // case where it found nothing — so the RON-value checks
                                // below stay out of it.
                                if let Some(deep_diags) = handled {
                                    diagnostics.extend(deep_diags);
                                    continue;
                                }
                            }

                            // Primitive / surface-level type check (uses RON-parsed typed values).
                            // Positions come directly from the tree-sitter node — no line adjustment.
                            if let Some(map) = ron_map {
                                if let Some(field_value) =
                                    map.get(&Value::String(field_name.to_string()))
                                {
                                    let type_mismatch = if let Some(analyzer) = analyzer {
                                        check_type_mismatch_with_enum_validation(
                                            field_value,
                                            &field_info.type_name,
                                            content,
                                            field_name,
                                            analyzer,
                                        )
                                        .await
                                    } else {
                                        check_type_mismatch_deep(
                                            field_value,
                                            &field_info.type_name,
                                            content,
                                            field_name,
                                        )
                                    };
                                    if let Some(error_msg) = type_mismatch {
                                        let pos = value_node.start_position();
                                        let end_pos = value_node.end_position();
                                        // For multi-line nodes, use end of first line
                                        // to avoid inverted column ranges
                                        let end_col = if end_pos.row > pos.row {
                                            let line = content.lines().nth(pos.row).unwrap_or("");
                                            line.len() as u32
                                        } else {
                                            end_pos.column as u32
                                        };
                                        diagnostics.push(Diagnostic {
                                            range: Range::new(
                                                Position::new(pos.row as u32, pos.column as u32),
                                                Position::new(pos.row as u32, end_col),
                                            ),
                                            severity: Some(DiagnosticSeverity::ERROR),
                                            message: format!("Type mismatch: {}", error_msg),
                                            ..Default::default()
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                // Missing required fields: compare expected fields against those we saw above.
                if !has_default {
                    let missing_fields: Vec<_> = fields
                        .iter()
                        .filter(|f| {
                            !present_field_names.contains(&f.name.as_str()) && !f.is_optional()
                        })
                        .collect();
                    if !missing_fields.is_empty() {
                        let missing_names: Vec<String> =
                            missing_fields.iter().map(|f| f.name.clone()).collect();
                        // Inside an array there is no struct name to point at: use the element start
                        let (line, col_start, col_end) = match element_index {
                            Some(_) => {
                                let start = ts_utils::node_to_lsp_range(&main_value).start;
                                (start.line, start.character, start.character + 1)
                            }
                            None => find_struct_name_position(content),
                        };
                        diagnostics.push(Diagnostic {
                            range: Range::new(
                                Position::new(line, col_start),
                                Position::new(line, col_end),
                            ),
                            severity: Some(DiagnosticSeverity::ERROR),
                            message: format!("Required fields: {}", missing_names.join(", ")),
                            ..Default::default()
                        });
                    }
                }
            }
        }
    }

    diagnostics
}

/// Async version: validate enum variants with field type checking
async fn validate_enum_variant_with_fields(
    content: &str,
    variants: &[EnumVariant],
    type_info: &TypeInfo,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // For enums, we need to parse the variant from the raw text
    let parsed_variant = extract_enum_variant_from_text(content);

    if let Some(variant) = parsed_variant {
        // Check if this variant exists
        if let Some(variant_def) = variants.iter().find(|v| v.name == variant.name) {
            // Variant exists - now validate its fields if it has data
            if let Some(ref data) = variant.data {
                // Validate that the variant can have data
                if variant_def.fields.is_empty() {
                    diagnostics.push(Diagnostic {
                        range: Range::new(
                            Position::new(variant.line, variant.col),
                            Position::new(variant.line, variant.col + variant.name.len() as u32),
                        ),
                        severity: Some(DiagnosticSeverity::ERROR),
                        message: format!(
                            "Variant '{}' is a unit variant and cannot have data",
                            variant.name
                        ),
                        ..Default::default()
                    });
                } else {
                    // Validate the fields
                    let mut field_diagnostics =
                        validate_variant_field_data(data, &variant_def.fields, analyzer).await;
                    // Adjust positions to account for the variant line offset
                    field_diagnostics =
                        adjust_diagnostic_positions(field_diagnostics, variant.line);
                    diagnostics.extend(field_diagnostics);
                }
            }
        } else {
            // Variant doesn't exist
            diagnostics.push(Diagnostic {
                range: Range::new(
                    Position::new(variant.line, variant.col),
                    Position::new(variant.line, variant.col + variant.name.len() as u32),
                ),
                severity: Some(DiagnosticSeverity::ERROR),
                message: format!(
                    "Unknown variant '{}' for enum '{}'",
                    variant.name, type_info.name
                ),
                ..Default::default()
            });
        }
    }

    diagnostics
}

/// A value node checked against a primitive type: the tree-sitter kind is the
/// evidence, since a map value has no field of its own.
fn check_primitive_node(node: &tree_sitter::Node, expected: &str) -> Option<Diagnostic> {
    let kind = node.kind();
    let ok = match expected {
        "String" | "&str" | "str" => matches!(kind, "string" | "raw_string"),
        "f32" | "f64" => matches!(kind, "float" | "integer"),
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128"
        | "usize" => kind == "integer",
        "bool" => kind == "boolean",
        "char" => kind == "char",
        _ => return None,
    };
    if ok {
        None
    } else {
        Some(Diagnostic {
            range: crate::ts_utils::node_to_lsp_range(node),
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!("Type mismatch: expected {expected}, got {kind}"),
            ..Default::default()
        })
    }
}

/// `HashMap<K, V>` split into its two parameters, honouring nested generics.
fn split_map_type(inner: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (index, ch) in inner.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                return Some((inner[..index].trim(), inner[index + 1..].trim()));
            }
            _ => {}
        }
    }
    None
}

/// A node that has to name an enum variant.
///
/// Tuple fields keep no name, so the RON-value checks — which look a value up by
/// field name — cannot judge them; the variant has to be checked on the node.
/// Reports an unknown name, or a value that is not a variant at all.
fn check_enum_variant_node(
    node: &tree_sitter::Node,
    content: &str,
    enum_info: &TypeInfo,
    variants: &[EnumVariant],
) -> Option<Diagnostic> {
    match crate::ts_utils::extract_enum_variant(node, content) {
        Some(variant) if variants.iter().any(|known| known.name == variant.name) => None,
        Some(variant) => Some(Diagnostic {
            range: crate::ts_utils::node_to_lsp_range(node),
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!(
                "Unknown variant '{}' for enum '{}'",
                variant.name, enum_info.name
            ),
            ..Default::default()
        }),
        None => Some(Diagnostic {
            range: crate::ts_utils::node_to_lsp_range(node),
            severity: Some(DiagnosticSeverity::ERROR),
            message: format!("expected {}, got {}", enum_info.name, node.kind()),
            ..Default::default()
        }),
    }
}

/// A value that has to name an enum variant, with its payload checked as well.
///
/// The name is judged first; when the variant takes data, each payload value is
/// checked against the variant's fields by position, so an unknown kind inside
/// `FirstParentOf([ ... ])` is reported too, not just the outer name.
async fn check_enum_variant_with_payload(
    node: &tree_sitter::Node<'_>,
    content: &str,
    enum_info: &TypeInfo,
    variants: &[EnumVariant],
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    if let Some(diag) = check_enum_variant_node(node, content, enum_info, variants) {
        return vec![diag];
    }
    let Some(parsed) = crate::ts_utils::extract_enum_variant(node, content) else {
        return Vec::new();
    };
    let Some(variant) = variants.iter().find(|known| known.name == parsed.name) else {
        return Vec::new();
    };
    if variant.fields.is_empty() {
        return Vec::new();
    }

    let values = crate::ts_utils::struct_values(node, content);
    let mut diagnostics = Vec::new();
    for (value, field) in values.iter().zip(&variant.fields) {
        if let Some(field_diagnostics) = Box::pin(validate_field_value_node(
            value,
            content,
            &field.type_name,
            analyzer,
        ))
        .await
        {
            diagnostics.extend(field_diagnostics);
        }
    }
    diagnostics
}

/// Validate a single field's value node against its declared Rust type.
///
/// This is the single place that decides how to recurse into generic wrappers
/// (Vec<T>, Option<T>, …) and plain custom types. Both `validate_struct_fields`
/// and `validate_node_with_type_info` delegate here so the logic isn't duplicated.
///
/// Returns `Some(diagnostics)` when the node-based checker owns the shape — it
/// walked the value and found exactly what there was to find — and `None` when the
/// shape is not its business: primitives, `Option<T>`/map wrappers, plain enums and
/// custom types without type info stay with the RON-value checks.
///
/// The distinction has to be explicit. A list element has no field text of its own,
/// so the RON-value checks read the whole list by field name and report valid enum
/// variants as "expected Kind, got ()" — a false positive that appeared
/// whenever this function validated a `Vec<Enum>` field successfully and the caller
/// still fell through to them.
async fn validate_field_value_node<'a>(
    value_node: &tree_sitter::Node<'a>,
    content: &str,
    field_type: &str,
    analyzer: &Arc<RustAnalyzer>,
) -> Option<Vec<Diagnostic>> {
    // A type with its own Deserialize impl decides for itself what RON it accepts.
    if analyzer.has_custom_deserializer(field_type).await {
        return Some(Vec::new());
    }
    let field_type_normalized = field_type.replace(" ", "");

    if let Some(inner_type) = extract_inner_type(&field_type_normalized, "Vec<") {
        // Vec<CustomType> — validate every array element against the inner type
        if is_primitive_type(&inner_type) || is_std_generic_type(&inner_type) {
            return None;
        }
        let inner_type_info = analyzer.get_type_info(&inner_type).await?;
        if value_node.kind() != "array" {
            return None;
        }

        let mut diagnostics = Vec::new();
        for elem_node in crate::ts_utils::array_value_children(value_node) {
            if let TypeKind::Enum(variants) = &inner_type_info.kind {
                diagnostics.extend(
                    Box::pin(check_enum_variant_with_payload(
                        &elem_node,
                        content,
                        &inner_type_info,
                        variants,
                        analyzer,
                    ))
                    .await,
                );
                continue;
            }

            let elem_diags = Box::pin(validate_node_with_type_info(
                &elem_node,
                content,
                &inner_type_info,
                analyzer,
            ))
            .await;
            diagnostics.extend(elem_diags);
        }
        return Some(diagnostics);
    }

    // Map values: `HashMap<K, V>` (optionally behind `Option<…>`). A value has no
    // field of its own, so a primitive is judged by its node kind and a custom type
    // by recursing with the value's declared type.
    let unwrapped = field_type_normalized
        .strip_prefix("Option<")
        .filter(|inner| inner.ends_with('>'))
        .map(|inner| &inner[..inner.len() - 1])
        .unwrap_or(field_type_normalized.as_str());
    for wrapper in ["HashMap<", "BTreeMap<"] {
        let Some(inner) = extract_inner_type(unwrapped, wrapper) else {
            continue;
        };
        let (_key_type, value_type) = split_map_type(&inner)?;
        if value_node.kind() != "map" {
            return None;
        }

        let value_custom = analyzer.get_type_info(value_type).await;
        // An enum whose variants carry data takes any RON value inside them; only a
        // unit-only enum can be judged by its variant name.
        if let Some(info) = &value_custom {
            if let TypeKind::Enum(variants) = &info.kind {
                if variants.iter().any(|variant| !variant.fields.is_empty()) {
                    return Some(Vec::new());
                }
            }
        }

        let mut diagnostics = Vec::new();
        for entry in crate::ts_utils::named_children(value_node) {
            let children = crate::ts_utils::named_children(&entry);
            let Some(value) = children.get(1) else {
                continue;
            };
            if is_primitive_type(value_type) {
                if let Some(diag) = check_primitive_node(value, value_type) {
                    diagnostics.push(diag);
                }
                continue;
            }
            if let Some(info) = &value_custom {
                if let TypeKind::Enum(variants) = &info.kind {
                    if let Some(diag) = check_enum_variant_node(value, content, info, variants) {
                        diagnostics.push(diag);
                    }
                    continue;
                }
            }
            if let Some(value_diags) = Box::pin(validate_field_value_node(
                value, content, value_type, analyzer,
            ))
            .await
            {
                diagnostics.extend(value_diags);
            }
        }
        return Some(diagnostics);
    }

    // A primitive is judged by its node kind right here: a nested struct's fields
    // never reach the RON-value fallback, which only runs for top-level fields.
    if is_primitive_type(&field_type_normalized) {
        return Some(
            check_primitive_node(value_node, &field_type_normalized)
                .into_iter()
                .collect(),
        );
    }
    // `Option<T>`/`Box<T>` around a custom type are transparent - the value itself is
    // judged below; containers that carry element checks of their own (maps) are not.
    if is_std_generic_type(&crate::completion::unwrap_wrapper(field_type)) {
        return None;
    }

    // Plain custom type - possibly behind `Option<...>` or another wrapper: the value
    // is judged here when the shape is ours, and the RON-value checks keep the rest.
    let base_type = crate::completion::unwrap_wrapper(field_type);
    let nested_type_info = analyzer.get_type_info(&base_type).await?;
    if let TypeKind::Enum(variants) = &nested_type_info.kind {
        // A variant with a payload (`FirstParentOf([ ... ])`) is checked node by node;
        // a bare unit-variant name stays with the RON-value checks.
        let has_payload = variants.iter().any(|variant| !variant.fields.is_empty());
        if value_node.kind() == "struct" || has_payload {
            return Some(
                Box::pin(check_enum_variant_with_payload(
                    value_node,
                    content,
                    &nested_type_info,
                    variants,
                    analyzer,
                ))
                .await,
            );
        }
        return None;
    }
    if value_node.kind() != "struct" {
        return None;
    }
    let diagnostics = Box::pin(validate_node_with_type_info(
        value_node,
        content,
        &nested_type_info,
        analyzer,
    ))
    .await;
    Some(diagnostics)
}

/// Validate a tree-sitter node representing a value against a TypeInfo
/// This properly handles nested structures by walking the tree directly
async fn validate_node_with_type_info<'a>(
    node: &tree_sitter::Node<'a>,
    content: &str,
    type_info: &TypeInfo,
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    use crate::ts_utils;
    let mut diagnostics = Vec::new();
    if analyzer.has_custom_deserializer(&type_info.name).await {
        return diagnostics;
    }

    match &type_info.kind {
        TypeKind::Struct(fields) if is_tuple_struct(fields) => {
            // A tuple struct keeps its fields positional ("0", "1", …), so the value
            // is a tuple (or an unnamed struct) and each field is matched by position.
            let values = match node.kind() {
                "tuple" => ts_utils::named_children(node),
                "struct" => ts_utils::struct_values(node, content),
                _ => Vec::new(),
            };
            if values.len() != fields.len() {
                diagnostics.push(Diagnostic {
                    range: ts_utils::node_to_lsp_range(node),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: format!(
                        "expected {} values for tuple struct '{}', got {}",
                        fields.len(),
                        type_info.name,
                        values.len()
                    ),
                    ..Default::default()
                });
            }
            for (value, field) in values.iter().zip(fields) {
                if let Some(field_info) = analyzer.get_type_info(&field.type_name).await {
                    if let TypeKind::Enum(variants) = &field_info.kind {
                        if let Some(diag) =
                            check_enum_variant_node(value, content, &field_info, variants)
                        {
                            diagnostics.push(diag);
                        }
                        continue;
                    }
                }
                let field_diags = Box::pin(validate_field_value_node(
                    value,
                    content,
                    &field.type_name,
                    analyzer,
                ))
                .await;
                if let Some(field_diags) = field_diags {
                    diagnostics.extend(field_diags);
                }
            }
        }
        TypeKind::Struct(fields) => {
            // Validate struct fields
            if node.kind() == "struct" {
                let field_nodes = ts_utils::struct_fields(node);
                let mut present_fields = std::collections::HashSet::new();

                // Check each field in the RON
                for field_node in field_nodes {
                    if let Some(field_name) = ts_utils::field_name(&field_node, content) {
                        present_fields.insert(field_name.to_string());

                        // Check if field exists in type
                        if let Some(field_info) = fields.iter().find(|f| f.name == field_name) {
                            // Delegate to the shared helper for all generic-wrapper and custom types
                            if let Some(value_node) = ts_utils::field_value(&field_node) {
                                let field_diags = Box::pin(validate_field_value_node(
                                    &value_node,
                                    content,
                                    &field_info.type_name,
                                    analyzer,
                                ))
                                .await;
                                if let Some(field_diags) = field_diags {
                                    diagnostics.extend(field_diags);
                                }
                            }
                        } else {
                            // Unknown field
                            let range = ts_utils::node_to_lsp_range(&field_node.child(0).unwrap());
                            diagnostics.push(Diagnostic {
                                range,
                                severity: Some(DiagnosticSeverity::ERROR),
                                message: format!("Unknown field '{}'", field_name),
                                ..Default::default()
                            });
                        }
                    }
                }

                // Check for missing required fields
                if !type_info.has_default {
                    for field in fields {
                        if !present_fields.contains(&field.name) && !field.is_optional() {
                            let target_node = node.child(0).unwrap_or(*node);
                            let range = ts_utils::node_to_lsp_range(&target_node);
                            diagnostics.push(Diagnostic {
                                range,
                                severity: Some(DiagnosticSeverity::ERROR),
                                message: format!(
                                    "Required fields: {}",
                                    fields
                                        .iter()
                                        .filter(|f| !present_fields.contains(&f.name)
                                            && !f.type_name.starts_with("Option"))
                                        .map(|f| f.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                                ..Default::default()
                            });
                            break;
                        }
                    }
                }
            }
        }
        TypeKind::Enum(_variants) => {
            // Enum validation - handled separately in validate_enum_variant_with_fields
        }
    }

    diagnostics
}

/// Validate the data inside an enum variant (tuple or struct fields)
/// This recursively validates nested types
async fn validate_variant_field_data(
    data: &str,
    expected_fields: &[FieldInfo],
    analyzer: &Arc<RustAnalyzer>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // For single-field tuple variants with Vec<CustomType>, use tree-sitter validation
    if expected_fields.len() == 1 {
        let field_type = &expected_fields[0].type_name;
        let normalized_type = field_type.replace(" ", "");

        // Check if it's Vec<CustomType>
        if let Some(inner_type) = extract_inner_type(&normalized_type, "Vec<") {
            if !is_primitive_type(&inner_type) && !is_std_generic_type(&inner_type) {
                if let Some(nested_type_info) = analyzer.get_type_info(&inner_type).await {
                    // Parse with tree-sitter
                    use crate::ts_utils::{self, RonParser};
                    let mut parser = RonParser::new();

                    if let Some(tree) = parser.parse(data) {
                        if let Some(array_node) = ts_utils::find_main_value(&tree) {
                            if array_node.kind() == "array" {
                                let mut cursor = array_node.walk();
                                for elem_node in array_node.children(&mut cursor) {
                                    if elem_node.kind() != "["
                                        && elem_node.kind() != "]"
                                        && elem_node.kind() != ","
                                    {
                                        let elem_diags = Box::pin(validate_node_with_type_info(
                                            &elem_node,
                                            data,
                                            &nested_type_info,
                                            analyzer,
                                        ))
                                        .await;
                                        diagnostics.extend(elem_diags);
                                    }
                                }
                            }
                        }
                    }
                    return diagnostics;
                }
            }
        } else if !is_primitive_type(field_type) {
            // Non-generic custom type - recursively validate
            if let Some(nested_type_info) = analyzer.get_type_info(field_type).await {
                let nested_diags = Box::pin(validate_ron_with_analyzer(
                    data,
                    &nested_type_info,
                    analyzer.clone(),
                ))
                .await;
                diagnostics.extend(nested_diags);
                return diagnostics;
            }
        }
    }

    // Try to parse the data as RON
    // For struct variants, the data contains named fields like "field1: val, field2: val"
    // For tuple variants, the data contains unnamed values like "val1, val2"
    let has_named_fields = expected_fields
        .iter()
        .any(|f| f.name.parse::<usize>().is_err());

    let parsed_data = if has_named_fields {
        // Struct-like variant: wrap the named fields in parentheses for RON parsing
        // RON syntax for struct variants is: VariantName( field: value )
        ron::from_str::<Value>(&format!("Temp({})", data))
    } else if data.contains(',') || expected_fields.len() > 1 {
        // Tuple variant with multiple fields
        ron::from_str::<Value>(&format!("({})", data))
    } else {
        // Single unnamed field
        ron::from_str::<Value>(data)
    };

    match parsed_data {
        Ok(value) => {
            // Validate fields based on whether it's named or unnamed
            if expected_fields
                .iter()
                .all(|f| f.name.parse::<usize>().is_err())
            {
                // Named fields (struct-like variant)
                if let Some(map) = extract_map_from_value(&value) {
                    // First, check for unknown fields - extract from the parsed value, not raw data
                    let mut ron_fields = Vec::new();
                    for key in map.keys() {
                        if let Value::String(field_name) = key {
                            ron_fields.push(field_name.clone());
                        }
                    }

                    // Validate field types
                    for field in expected_fields {
                        if let Some(field_value) = map.get(&Value::String(field.name.clone())) {
                            if let Some(error_msg) = check_type_mismatch_with_enum_validation(
                                field_value,
                                &field.type_name,
                                data,
                                &field.name,
                                analyzer,
                            )
                            .await
                            {
                                diagnostics.push(Diagnostic {
                                    range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                                    severity: Some(DiagnosticSeverity::ERROR),
                                    message: format!(
                                        "Type mismatch in variant field: {}",
                                        error_msg
                                    ),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            } else {
                // Unnamed fields (tuple variant)
                // For tuple variants, fields are named "0", "1", "2", etc.
                if let Value::Seq(values) = value {
                    for (i, field) in expected_fields.iter().enumerate() {
                        if let Some(field_value) = values.get(i) {
                            if let Some(error_msg) = check_type_mismatch_with_enum_validation(
                                field_value,
                                &field.type_name,
                                data,
                                &field.name,
                                analyzer,
                            )
                            .await
                            {
                                diagnostics.push(Diagnostic {
                                    range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                                    severity: Some(DiagnosticSeverity::ERROR),
                                    message: format!(
                                        "Type mismatch in variant field {}: {}",
                                        i, error_msg
                                    ),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                } else if expected_fields.len() == 1 {
                    // Single field tuple variant
                    if let Some(error_msg) = check_type_mismatch_with_enum_validation(
                        &value,
                        &expected_fields[0].type_name,
                        data,
                        &expected_fields[0].name,
                        analyzer,
                    )
                    .await
                    {
                        diagnostics.push(Diagnostic {
                            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                            severity: Some(DiagnosticSeverity::ERROR),
                            message: format!("Type mismatch in variant field: {}", error_msg),
                            ..Default::default()
                        });
                    }
                }
            }
        }
        Err(_) => {
            // Failed to parse - could be syntax error
            diagnostics.push(Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Invalid syntax in enum variant data".to_string(),
                ..Default::default()
            });
        }
    }

    diagnostics
}

/// Extract a map from a RON value (handles both raw maps and named struct syntax)
fn extract_map_from_value(value: &Value) -> Option<&ron::Map> {
    match value {
        Value::Map(map) => Some(map),
        _ => None,
    }
}

/// A struct whose fields are positional: their names are the indices, as the
/// analyzer writes them ("0", "1", …).
fn is_tuple_struct(fields: &[FieldInfo]) -> bool {
    !fields.is_empty()
        && fields
            .iter()
            .all(|field| field.name.parse::<usize>().is_ok())
}

/// Check if a type is a primitive type (not a custom enum/struct)
fn is_primitive_type(type_name: &str) -> bool {
    let clean = type_name.replace(" ", "");

    // Primitive types
    let primitives = [
        "bool", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128",
        "usize", "f32", "f64", "char", "String", "&str", "str",
    ];

    if primitives.contains(&clean.as_str()) {
        return true;
    }

    false
}

/// Check if a type is a standard library generic type (Option, Vec, HashMap, etc.)
fn is_std_generic_type(type_name: &str) -> bool {
    let clean = type_name.replace(" ", "");

    clean.starts_with("Option<")
        || clean.starts_with("Vec<")
        || clean.contains("HashMap<")
        || clean.contains("BTreeMap<")
        || clean.contains("HashSet<")
        || clean.contains("BTreeSet<")
        || clean.starts_with("Result<")
        || clean.starts_with("Box<")
        || clean.starts_with("Rc<")
        || clean.starts_with("Arc<")
}

/// Extract the variant name and data from raw RON text using tree-sitter
/// Enums can be: Simple (Long), tuple (Long(...)), or struct-like (Long { ... })
fn extract_enum_variant_from_text(content: &str) -> Option<ParsedEnumVariant> {
    use crate::ts_utils::{self, RonParser};

    // Skip type annotation if present
    let ron_content = if content.trim_start().starts_with("/*") {
        if let Some(end_idx) = content.find("*/") {
            &content[end_idx + 2..]
        } else {
            content
        }
    } else {
        content
    };

    let mut parser = RonParser::new();
    let tree = parser.parse(ron_content)?;

    // Find the main value (should be an identifier or struct representing the variant)
    let main_value = ts_utils::find_main_value(&tree);

    let main_value = main_value?;
    ts_utils::extract_enum_variant(&main_value, ron_content)
}

/// Find the position of an enum variant in the content using tree-sitter
#[allow(dead_code)]
fn find_variant_position(content: &str, variant: &str) -> (u32, u32) {
    use crate::ts_utils::{self, RonParser};

    let mut parser = RonParser::new();
    if let Some(tree) = parser.parse(content) {
        let variants = ts_utils::find_potential_variants(&tree, content);
        for v in variants {
            if let Some(text) = ts_utils::node_text(&v, content) {
                if text == variant {
                    let pos = v.start_position();
                    return (pos.row as u32, pos.column as u32);
                }
            }
        }
    }

    (0, 0)
}

/// Find the position of the struct name in the RON content using tree-sitter
/// Returns (line, col_start, col_end) where col_start == col_end indicates unnamed struct
fn find_struct_name_position(content: &str) -> (u32, u32, u32) {
    use crate::ts_utils::{self, RonParser};

    let mut parser = RonParser::new();
    if let Some(tree) = parser.parse(content) {
        if let Some(main_value) = ts_utils::find_main_value(&tree) {
            if main_value.kind() == "struct" {
                if let Some(name_node) = main_value.child(0) {
                    if name_node.kind() == "identifier" {
                        let pos = name_node.start_position();
                        let end_pos = name_node.end_position();
                        return (pos.row as u32, pos.column as u32, end_pos.column as u32);
                    }
                }
                // Unnamed struct
                let pos = main_value.start_position();
                return (pos.row as u32, pos.column as u32, pos.column as u32);
            }
        }
    }

    (0, 0, 1)
}

/// Find the position of a field's value in the content using tree-sitter
#[allow(dead_code)]
fn find_field_value_position(content: &str, field_name: &str) -> Option<(usize, usize, usize)> {
    use crate::ts_utils::{self, RonParser};

    let mut parser = RonParser::new();
    let tree = parser.parse(content)?;

    if let Some(main_value) = ts_utils::find_main_value(&tree) {
        if main_value.kind() == "struct" {
            let fields = ts_utils::struct_fields(&main_value);
            for field in fields {
                if let Some(name) = ts_utils::field_name(&field, content) {
                    if name == field_name {
                        if let Some(value_node) = ts_utils::field_value(&field) {
                            let pos = value_node.start_position();
                            let end_pos = value_node.end_position();
                            return Some((pos.row, pos.column, end_pos.column));
                        }
                    }
                }
            }
        }
    }

    None
}

/// Type checking with enum variant validation (async, uses analyzer)
async fn check_type_mismatch_with_enum_validation(
    value: &Value,
    expected_type: &str,
    content: &str,
    field_name: &str,
    analyzer: &Arc<RustAnalyzer>,
) -> Option<String> {
    if analyzer.has_custom_deserializer(expected_type).await {
        return None;
    }

    // First do basic type checking
    let basic_result = check_type_mismatch_deep(value, expected_type, content, field_name);

    // If the expected type is a custom type, check if it's an enum and validate the variant
    // before returning any basic mismatch (which may be a false positive for enum variants)
    if !is_primitive_type(expected_type) {
        if let Some(field_value_text) = extract_field_value_text(content, field_name) {
            let trimmed = field_value_text.trim();

            // Check if the type is an enum
            if let Some(type_info) = analyzer.get_type_info(expected_type).await {
                if let TypeKind::Enum(variants) = &type_info.kind {
                    // Extract the variant name from the text
                    let variant_name = trimmed.split('(').next().unwrap_or(trimmed).trim();

                    if variants.iter().any(|v| v.name == variant_name) {
                        // Valid variant — suppress any basic type mismatch
                        return None;
                    } else {
                        return Some(format!(
                            "unknown variant '{}' for enum {}",
                            variant_name, expected_type
                        ));
                    }
                }
            }
        }
    }

    // Return the basic result for non-enum types
    if basic_result.is_some() {
        return basic_result;
    }

    None
}

/// Deep type checking that also validates custom types by looking at raw text
fn check_type_mismatch_deep(
    value: &Value,
    expected_type: &str,
    content: &str,
    field_name: &str,
) -> Option<String> {
    let clean_type = expected_type.replace(" ", "");

    // First check if it's a primitive type or standard library generic type
    if is_primitive_type(expected_type) || is_std_generic_type(expected_type) {
        return check_type_mismatch(value, expected_type);
    }

    // For custom types (structs/enums), we need to check the raw text
    // because Value loses the type information

    // Find the field's value in the raw text
    let field_value_text = extract_field_value_text(content, field_name)?;
    let trimmed = field_value_text.trim();

    // Check if expected type is a custom struct/enum (starts with uppercase)
    if clean_type
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
    {
        // Expected a struct/enum
        // The value should start with TypeName( or be a variant name

        // Check if it looks like a struct instantiation TypeName(...) or unnamed (...)
        if trimmed.contains('(') {
            // Extract the type name before the paren
            let type_in_text = trimmed.split('(').next().unwrap_or("").trim();
            let expected_simple = clean_type.split("::").last().unwrap_or(&clean_type);

            // Allow unnamed struct syntax - empty type_in_text means type is inferred
            if !type_in_text.is_empty() && type_in_text != expected_simple {
                return Some(format!("expected {}, got {}", expected_type, type_in_text));
            }
        } else {
            // It's a bare value - could be an enum variant or a primitive
            // If it's a number, string literal, or bool, that's wrong
            if trimmed.parse::<i64>().is_ok() {
                return Some(format!("expected {}, got integer", expected_type));
            }
            if trimmed.parse::<f64>().is_ok() {
                return Some(format!("expected {}, got float", expected_type));
            }
            if trimmed.starts_with('"') {
                return Some(format!("expected {}, got string", expected_type));
            }
            if trimmed == "true" || trimmed == "false" {
                return Some(format!("expected {}, got bool", expected_type));
            }
            // Otherwise assume it's an enum variant (we'd need more context to validate)
        }
    }

    None
}

/// Extract the raw text value for a field, handling nested structures
fn extract_field_value_text(content: &str, field_name: &str) -> Option<String> {
    use crate::ts_utils::{self, RonParser};

    let mut parser = RonParser::new();
    let tree = parser.parse(content)?;
    let main_value = ts_utils::find_main_value(&tree)?;

    if main_value.kind() == "struct" || main_value.kind() == "ERROR" {
        // For ERROR nodes, find the struct sibling
        let struct_node = if main_value.kind() == "ERROR" {
            let root = tree.root_node();
            let mut cursor = root.walk();
            let result = root.children(&mut cursor).find(|n| n.kind() == "struct");
            result?
        } else {
            main_value
        };

        let field_nodes = ts_utils::struct_fields(&struct_node);
        for field_node in field_nodes {
            if let Some(name) = ts_utils::field_name(&field_node, content) {
                if name == field_name {
                    if let Some(value_node) = ts_utils::field_value(&field_node) {
                        return ts_utils::node_text(&value_node, content).map(|s| s.to_string());
                    }
                }
            }
        }
    }

    None
}

/// Check if a RON value matches the expected Rust type
fn check_type_mismatch(value: &Value, expected_type: &str) -> Option<String> {
    let clean_type = expected_type.replace(" ", "");
    // Use clean_type for error messages to avoid extra spaces
    let display_type = &clean_type;

    // Handle Option types - None is always valid for Option<T>
    if clean_type.starts_with("Option<") {
        if matches!(value, Value::Option(None)) {
            return None;
        }
        // For Some(value), check the inner type
        if let Value::Option(Some(inner)) = value {
            // Extract the inner type from Option<InnerType>
            if let Some(inner_type) = extract_inner_type(&clean_type, "Option<") {
                return check_type_mismatch(inner, &inner_type);
            }
        }
        // Non-Option value for Option type is okay (will be wrapped)
        if let Some(inner_type) = extract_inner_type(&clean_type, "Option<") {
            return check_type_mismatch(value, &inner_type);
        }
    }

    // Handle Box, Rc, Arc - they serialize as just the inner value
    if clean_type.starts_with("Box<")
        || clean_type.starts_with("Rc<")
        || clean_type.starts_with("Arc<")
    {
        let wrapper = if clean_type.starts_with("Box<") {
            "Box<"
        } else if clean_type.starts_with("Rc<") {
            "Rc<"
        } else {
            "Arc<"
        };

        if let Some(inner_type) = extract_inner_type(&clean_type, wrapper) {
            return check_type_mismatch(value, &inner_type);
        }
    }

    match value {
        Value::Bool(_) => {
            if clean_type != "bool" {
                return Some(format!("expected {}, got bool", display_type));
            }
        }
        Value::Number(n) => {
            // Check for integer types
            let integer_types = [
                "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128",
                "usize",
            ];
            let float_types = ["f32", "f64"];

            let is_integer_type = integer_types.contains(&clean_type.as_str());
            let is_float_type = float_types.contains(&clean_type.as_str());

            // Check if it's a float or integer based on the Number variant
            let is_float_value = matches!(n, ron::Number::F32(_) | ron::Number::F64(_));
            let is_int_value = !is_float_value;

            if is_float_value && is_integer_type {
                return Some(format!("expected {}, got float", display_type));
            }
            if is_int_value && is_float_type {
                return Some(format!("expected {}, got integer", display_type));
            }
            // If not a numeric type at all, it's an error
            if !is_integer_type && !is_float_type {
                return Some(format!("expected {}, got number", display_type));
            }
        }
        Value::String(_) => {
            // String is valid for String types
            if clean_type == "String" || clean_type == "&str" || clean_type == "str" {
                return None;
            }
            // Otherwise it's an error
            return Some(format!("expected {}, got string", display_type));
        }
        Value::Seq(seq) => {
            if clean_type.starts_with("Vec<") {
                // Check element types if possible
                if let Some(elem_type) = extract_inner_type(&clean_type, "Vec<") {
                    for elem in seq {
                        if let Some(err) = check_type_mismatch(elem, &elem_type) {
                            return Some(format!("in Vec: {}", err));
                        }
                    }
                }
            } else if clean_type.contains("HashSet<") || clean_type.contains("BTreeSet<") {
                // Sets are serialized as arrays in RON
                return None;
            } else if clean_type.starts_with("Result<") {
                // Result variants like Ok(...) and Err(...) are serialized as tuples/arrays
                return None;
            } else if clean_type.starts_with("[") {
                // Array type
                return None; // Arrays are similar to Vec, accept them
            } else {
                return Some(format!("expected {}, got array", display_type));
            }
        }
        Value::Map(_) => {
            // Maps could be structs or actual maps
            if clean_type.contains("HashMap<") || clean_type.contains("BTreeMap<") {
                // It's a map type, which is fine
                return None;
            }
            // Check if it's a custom struct (starts with uppercase)
            if clean_type
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                // Could be a struct, allow it
                return None;
            }
            return Some(format!("expected {}, got map/struct", display_type));
        }
        Value::Option(Some(_)) => {
            if !clean_type.starts_with("Option<") {
                return Some(format!("expected {}, got Some(...)", display_type));
            }
        }
        Value::Option(None) => {
            if !clean_type.starts_with("Option<") {
                return Some(format!("expected {}, got None", display_type));
            }
        }
        Value::Unit if clean_type != "()" && clean_type != "unit" => {
            return Some(format!("expected {}, got ()", display_type));
        }
        _ => {}
    }

    None
}

/// Extract the inner type from a generic type like Option<T> or Vec<T>
fn extract_inner_type(type_str: &str, wrapper: &str) -> Option<String> {
    if type_str.starts_with(wrapper) && type_str.ends_with('>') {
        let inner = &type_str[wrapper.len()..type_str.len() - 1];
        return Some(inner.to_string());
    }
    None
}

/// Basic RON syntax validation with better error positioning
#[allow(dead_code)]
pub fn validate_ron_syntax(content: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Try to parse as RON
    if let Err(e) = ron::from_str::<ron::Value>(content) {
        let error_msg = e.to_string();

        // Try to extract line and column from error message
        // RON errors often contain "at line X column Y" or similar patterns
        let (line, col) = parse_error_position(&error_msg, content);

        // Create a more helpful error message
        let simplified_msg = simplify_ron_error(&error_msg);

        diagnostics.push(Diagnostic {
            range: Range::new(Position::new(line, col), Position::new(line, col + 1)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: simplified_msg,
            ..Default::default()
        });
    }

    diagnostics
}

/// Parse the leading position of a RON message: `16:20: …` (line and column,
/// 1-indexed as written there, 0-indexed in the returned pair).
fn leading_position(message: &str) -> Option<(u32, u32)> {
    let (line, rest) = message.trim_start().split_once(':')?;
    let (col, _) = rest.split_once(':')?;
    let line: u32 = line.trim().parse().ok()?;
    let col: u32 = col.trim().parse().ok()?;
    Some((line.saturating_sub(1), col.saturating_sub(1)))
}

/// Range of a syntax error: the position is taken from the tree itself — it holds
/// an error node exactly where parsing broke (`duration: 6a0` is `a0`, not the
/// start of the file). The RON error text is the fallback: its `16:20` form is not
/// always parsed, and the diagnostic then used to land on `1:1`.
fn syntax_error_range(content: &str, error_msg: &str) -> Range {
    use crate::ts_utils::{self, RonParser};

    // A RON message starts with the position (`16:20: Expected comma`) — that is
    // the error site itself; the tree, by contrast, recovers after the failure and
    // may point at the next trouble rather than the first one.
    if let Some((line, col)) = leading_position(error_msg) {
        return Range::new(Position::new(line, col), Position::new(line, col + 1));
    }

    if let Some(tree) = RonParser::new().parse(content) {
        let mut errors = Vec::new();
        collect_error_nodes(&tree.root_node(), &mut errors);

        // Outer error nodes merely contain the others: keep the innermost ones and
        // take the leftmost — that is where parsing broke.
        let innermost: Vec<_> = errors
            .iter()
            .copied()
            .filter(|node| {
                !errors.iter().any(|other| {
                    other.id() != node.id()
                        && other.start_byte() >= node.start_byte()
                        && other.end_byte() <= node.end_byte()
                })
            })
            .collect();

        if let Some(node) = innermost.into_iter().min_by_key(|node| node.start_byte()) {
            return ts_utils::node_to_lsp_range(&node);
        }
    }

    let (line, col) = parse_error_position(error_msg, content);
    Range::new(Position::new(line, col), Position::new(line, col + 1))
}

/// Collect every error node of a subtree (missing tokens included).
fn collect_error_nodes<'a>(node: &tree_sitter::Node<'a>, found: &mut Vec<tree_sitter::Node<'a>>) {
    if node.is_error() || node.is_missing() {
        found.push(*node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_error_nodes(&child, found);
    }
}

fn parse_error_position(error_msg: &str, content: &str) -> (u32, u32) {
    // RON error messages often contain position info like "1:5" or "line 1 column 5"

    // Try to find "line X column Y" pattern
    if let Some(line_start) = error_msg.find("line ") {
        let rest = &error_msg[line_start + 5..];
        if let Some(line_end) = rest.find(|c: char| !c.is_numeric()) {
            if let Ok(line) = rest[..line_end].parse::<u32>() {
                if let Some(col_start) = rest.find("column ") {
                    let col_rest = &rest[col_start + 7..];
                    if let Some(col_end) = col_rest.find(|c: char| !c.is_numeric()) {
                        if let Ok(col) = col_rest[..col_end].parse::<u32>() {
                            // RON reports 1-indexed, LSP expects 0-indexed
                            return (line.saturating_sub(1), col.saturating_sub(1));
                        }
                    }
                }
            }
        }
    }

    // Try to find "X:Y" pattern (common in parsers)
    if let Some(colon_pos) = error_msg.find(':') {
        let before = error_msg[..colon_pos].trim();
        // The line number may sit at the very start of the message ("16:20: Expected
        // comma"), and then there is nothing before it to cut off.
        let line_str = match before.rfind(|c: char| !c.is_numeric()) {
            Some(index) => &before[index + 1..],
            None => before,
        };
        {
            if let Ok(line) = line_str.parse::<u32>() {
                let after = &error_msg[colon_pos + 1..];
                if let Some(col_end) = after.find(|c: char| !c.is_numeric()) {
                    if let Ok(col) = after[..col_end].parse::<u32>() {
                        return (line.saturating_sub(1), col.saturating_sub(1));
                    }
                }
            }
        }
    }

    // If we can't parse position, try to find likely error location by looking for common issues
    let lines: Vec<&str> = content.lines().collect();

    // Check for missing commas between fields
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // If a line ends with a value (not comma, not open brace) and next line starts with a field
        if !trimmed.is_empty()
            && !trimmed.ends_with(',')
            && !trimmed.ends_with('(')
            && !trimmed.ends_with('{')
            && !trimmed.ends_with('[')
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && idx + 1 < lines.len()
        {
            let next_line = lines[idx + 1].trim();
            // Next line looks like a field (word followed by colon)
            if next_line.contains(':') && !next_line.starts_with("//") {
                // Likely missing comma
                return (idx as u32, line.len().saturating_sub(1) as u32);
            }
        }
    }

    // Default to start of file
    (0, 0)
}

/// Simplify RON error messages to be more user-friendly
fn simplify_ron_error(error_msg: &str) -> String {
    // Extract the core error without all the implementation details
    if error_msg.contains("expected") {
        if error_msg.contains("`,`") || error_msg.contains("comma") {
            return "Expected comma between fields".to_string();
        }
        if error_msg.contains("`:`") || error_msg.contains("colon") {
            return "Expected colon after field name".to_string();
        }
        if error_msg.contains("`)`") {
            return "Expected closing parenthesis".to_string();
        }
        if error_msg.contains("`}`") {
            return "Expected closing brace".to_string();
        }
        if error_msg.contains("`]`") {
            return "Expected closing bracket".to_string();
        }
    }

    if error_msg.contains("unexpected") {
        return format!(
            "Syntax error: {}",
            error_msg
                .split("unexpected")
                .nth(1)
                .unwrap_or(error_msg)
                .trim()
        );
    }

    // Return simplified version
    format!("RON syntax error: {}", error_msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_analyzer::{EnumVariant, FieldInfo};

    #[tokio::test]
    async fn serde_renamed_fields_are_recognised_in_data() {
        let root = std::env::temp_dir().join(format!("ron-serde-rename-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source = r#"
            #[derive(serde::Deserialize)]
            struct Param { name: String, #[serde(rename = "type")] ty: String }

            #[derive(serde::Deserialize)]
            struct Root { key: String, params: Vec<Param> }
        "#;
        std::fs::write(root.join("src/lib.rs"), source).unwrap();
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer.set_workspace_root(&root).await;
        let info = analyzer.get_type_info("Root").await.unwrap();

        // `type` is the name the field is visible under in RON
        let valid = r#"Root(key: "rule.a", params: [Param(name: "child", type: "value")])"#;
        let diagnostics = validate_ron_with_analyzer(valid, &info, analyzer.clone()).await;
        assert!(
            diagnostics.is_empty(),
            "{:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );

        // The Rust name `ty` is an error in data
        let invalid = r#"Root(key: "rule.a", params: [Param(name: "child", ty: "value")])"#;
        let diagnostics = validate_ron_with_analyzer(invalid, &info, analyzer.clone()).await;
        let messages: Vec<&str> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert!(
            messages.iter().any(|m| m.contains("Unknown field 'ty'")),
            "{messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("Required fields: type")),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn array_elements_skip_comments() {
        let root = std::env::temp_dir().join(format!("ron-array-comments-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source = r#"
            #[derive(serde::Deserialize)]
            struct Entry { key: String }
        "#;
        std::fs::write(root.join("src/lib.rs"), source).unwrap();
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer.set_workspace_root(&root).await;
        let info = analyzer.get_type_info("Entry").await.unwrap();

        // Comments inside an array are not elements: no diagnostics expected
        let content = r#"[
            // first element
            (
                key: "a",
            ),
            /* second */
            (
                key: "b",
            ),
        ]"#;

        let diagnostics = validate_ron_with_analyzer(content, &info, analyzer.clone()).await;
        let messages: Vec<&str> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert!(diagnostics.is_empty(), "{messages:?}");
    }

    #[tokio::test]
    async fn array_root_is_validated_per_element() {
        let root = std::env::temp_dir().join(format!("ron-array-root-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source = r#"
            #[derive(serde::Deserialize)]
            struct Entry { key: String, count: u32 }
        "#;
        std::fs::write(root.join("src/lib.rs"), source).unwrap();
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer.set_workspace_root(&root).await;
        let info = analyzer.get_type_info("Entry").await.unwrap();

        let content = r#"[
            (
                key: "ok",
                count: 1,
            ),
            (
                key: 2,
                kaz: 3,
            ),
        ]"#;

        let diagnostics = validate_ron_with_analyzer(content, &info, analyzer.clone()).await;
        let messages: Vec<String> = diagnostics
            .iter()
            .map(|d| format!("{} @line {}", d.message, d.range.start.line))
            .collect();

        assert!(
            messages.iter().any(|m| m.contains("Unknown field 'kaz'")),
            "{messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.starts_with("Type mismatch")),
            "{messages:?}"
        );
        // Errors belong to the second element (line 5+): the first one is clean
        assert!(
            diagnostics.iter().all(|d| d.range.start.line >= 5),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn custom_deserializers_do_not_require_struct_shaped_values() {
        let root = std::env::temp_dir().join(format!("ron-custom-serde-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source = r#"
            struct Special(String);
            impl<'de> serde::Deserialize<'de> for Special {
                fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                    String::deserialize(d).map(Special)
                }
            }
            #[derive(serde::Deserialize)]
            #[serde(rename = "Renamed", transparent)]
            struct Transparent(String);
            #[derive(serde::Deserialize)]
            #[serde(from = "String")]
            struct Converted { value: String }
            #[derive(serde::Deserialize)]
            #[serde(try_from = "String")]
            enum ConvertedEnum { Value(String) }
            #[derive(serde::Deserialize)]
            struct Ordinary { value: String }
            type Alias = Special;
            struct Data {
                special: Special,
                transparent: Transparent,
                converted: Converted,
                converted_enum: ConvertedEnum,
                optional: Option<Special>,
                values: Vec<Special>,
                alias: Alias,
                count: u32,
                ordinary: Ordinary,
            }
        "#;
        let file = root.join("src/lib.rs");
        std::fs::write(&file, source).unwrap();
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer.set_workspace_root(&root).await;
        let info = analyzer.get_type_info("Data").await.unwrap();
        let content = r#"Data(
            special: "a",
            transparent: "b",
            converted: "c",
            converted_enum: "d",
            optional: Some("e"),
            values: ["f", "g"],
            alias: "h",
            count: 1,
            ordinary: Ordinary(value: "i"),
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, &info, analyzer.clone()).await;
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for (from, to) in [
            ("count: 1", "count: \"wrong\""),
            ("Ordinary(value: \"i\")", "Ordinary()"),
        ] {
            let diagnostics =
                validate_ron_with_analyzer(&content.replace(from, to), &info, analyzer.clone())
                    .await;
            assert!(
                !diagnostics.is_empty(),
                "ordinary type checks must remain enabled"
            );
        }
        let special = analyzer.get_type_info("Special").await.unwrap();
        assert!(
            validate_ron_with_analyzer("\"root\"", &special, analyzer.clone())
                .await
                .is_empty()
        );
        assert!(
            !validate_ron_with_analyzer("\"unterminated", &special, analyzer.clone())
                .await
                .is_empty()
        );
        // Rescanning must forget implementations that were removed.
        std::fs::write(file, "struct Special(String);").unwrap();
        analyzer.scan_workspace().await;
        assert!(!analyzer.has_custom_deserializer("Special").await);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn test_enum_variant_validation() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "PostType".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Short".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                },
                EnumVariant {
                    name: "Long".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Valid enum variant
        let content = "Long";
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Long should be valid. Got errors: {:?}",
            diagnostics
        );

        // Another valid variant
        let content = "Short";
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Short should be valid. Got errors: {:?}",
            diagnostics
        );

        // Invalid enum variant
        let content = "Medium";
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert_eq!(diagnostics.len(), 1, "Medium should be invalid");
        assert!(
            diagnostics[0].message.contains("Unknown variant 'Medium'"),
            "Expected unknown variant error, got: {}",
            diagnostics[0].message
        );

        // Invalid enum variant (typo)
        let content = "Longs";
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert_eq!(diagnostics.len(), 1, "Longs should be invalid");
        assert!(
            diagnostics[0].message.contains("Unknown variant 'Longs'"),
            "Expected unknown variant error, got: {}",
            diagnostics[0].message
        );
    }

    #[tokio::test]
    async fn test_struct_with_enum_field() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "title".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "post_type".to_string(),
                    type_name: "PostType".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Valid struct with enum field
        let content = r#"Post(
            id: 1,
            title: "Test",
            post_type: Long,
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Should have no errors. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_struct_field_expects_struct_not_primitive() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "author".to_string(),
                    type_name: "User".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // WRONG: author should be User(...), not just 1
        let content = r#"Post(
            id: 101,
            author: 1,
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics.is_empty(),
            "Should error on primitive when expecting struct"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expected User")),
            "Should complain about User type. Got: {:?}",
            diagnostics
        );

        // CORRECT: author is User(...)
        let content = r#"Post(
            id: 101,
            author: User(id: 1, name: "John"),
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Should have no errors. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_type_mismatch_primitives() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Type mismatch - string for number
        let content = r#"User(
            id: "not a number",
            name: "John",
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        assert!(!diagnostics.is_empty(), "Should error on type mismatch");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Type mismatch")),
            "Should have type mismatch error. Got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_ron_parsing_collections() {
        // Test what RON actually parses for bad collection values
        let content = r#"GenericTest(
            bad_hashmap: "not a map",
            bad_btreemap: 123,
            bad_hashset: "not a set",
        )"#;

        let parsed = ron::from_str::<Value>(content);
        println!("Parse result: {:?}", parsed);

        if let Ok(Value::Map(map)) = parsed {
            println!("Map has {} entries", map.len());
            for (k, v) in map.iter() {
                if let Value::String(key) = k {
                    println!("  {}: {:?}", key, v);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_invalid_enum_in_struct_field() {
        // This test ensures that invalid enum variants in struct fields are caught
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "post_type".to_string(),
                    type_name: "PostType".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Invalid: "Longs" is not a valid PostType variant
        // This will be caught when the LSP has access to the analyzer
        // In the sync test version, it won't catch this (needs analyzer)
        let content = r#"Post(
            id: 1,
            post_type: Longs,
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        // Without analyzer, this won't be caught - that's expected
        // With analyzer (in real LSP), check_type_mismatch_with_enum_validation will catch it
        println!("Diagnostics for invalid enum variant: {:?}", diagnostics);
    }

    #[tokio::test]
    async fn test_unnamed_struct_syntax() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Unnamed struct syntax should be valid
        let content = r#"(
            id: 1,
            name: "John",
        )"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Unnamed struct syntax should be valid. Got errors: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_enum_with_tuple_variant() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Value".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Int".to_string(),
                    fields: vec![FieldInfo {
                        name: "0".to_string(),
                        type_name: "i32".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                    }],
                    docs: None,
                    line: None,
                    column: None,
                },
                EnumVariant {
                    name: "Str".to_string(),
                    fields: vec![FieldInfo {
                        name: "0".to_string(),
                        type_name: "String".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                    }],
                    docs: None,
                    line: None,
                    column: None,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Valid tuple variant
        let content = "Int(42)";
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Tuple variant should be valid. Got errors: {:?}",
            diagnostics
        );

        // Another valid variant
        let content = r#"Str("hello")"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        assert_eq!(
            diagnostics.len(),
            0,
            "Tuple variant with string should be valid. Got errors: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_enum_with_struct_variant() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Message".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "Text".to_string(),
                fields: vec![
                    FieldInfo {
                        name: "content".to_string(),
                        type_name: "String".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                    },
                    FieldInfo {
                        name: "sender".to_string(),
                        type_name: "String".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                    },
                ],
                docs: None,
                line: None,
                column: None,
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Struct-like variant (this requires parentheses in RON)
        let content = r#"Text { content: "hello", sender: "alice" }"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        // This might not validate correctly without proper struct-variant handling
        // but we're testing that it parses and doesn't crash
        println!("Struct variant diagnostics: {:?}", diagnostics);
    }

    #[test]
    fn test_ron_parsing_enum_variant() {
        // Test if RON can parse a standalone enum variant
        let test_cases = vec![
            "Detailed( length: 1 )",
            "Detailed(length: 1)",
            "Detailed { length: 1 }",
        ];

        for case in test_cases {
            println!("Testing: {}", case);
            let result = ron::from_str::<Value>(case);
            println!("Result: {:?}", result);
        }
    }

    #[test]
    fn test_extract_field_value_for_enum() {
        let content = r#"Post(
    id: 42,
    post_type: Detailed( length: 1 ),
)"#;
        let extracted = extract_field_value_text(content, "post_type");
        println!("Extracted post_type value: {:?}", extracted);
        assert!(extracted.is_some());
        let value = extracted.unwrap();
        assert_eq!(value, "Detailed( length: 1 )");
    }

    #[test]
    fn test_extract_nested_enum_variant() {
        let content = r#"/* @[crate::models::Message] */

PostReference(Post(
    id: 42,
    title: "test",
))"#;
        let variant = extract_enum_variant_from_text(content);
        assert!(variant.is_some());
        let variant = variant.unwrap();
        assert_eq!(variant.name, "PostReference");
        println!("Extracted data: {:?}", variant.data);
        assert!(variant.data.is_some());
        let data = variant.data.unwrap();
        assert!(data.contains("Post("));
        assert!(data.contains("id: 42"));
    }

    #[tokio::test]
    async fn test_unit_variant_with_data_error() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Status".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Active".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                },
                EnumVariant {
                    name: "Inactive".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Unit variant should not have data
        let content = "Active(123)";
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        // Should get error for providing data to unit variant
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("unit variant")
                    || d.message.contains("cannot have data")),
            "Should error on unit variant with data. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_enum_with_vec_of_custom_type_missing_fields() {
        let analyzer = Arc::new(RustAnalyzer::new());

        // Create User type info
        let user_type = TypeInfo {
            name: "User".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "email".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "age".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "is_active".to_string(),
                    type_name: "bool".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "roles".to_string(),
                    type_name: "Vec<String>".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Register User type with the analyzer
        analyzer.insert_type_for_test(user_type).await;

        // Create Message enum with UserTag variant
        let type_info = TypeInfo {
            name: "Message".to_string(),
            kind: TypeKind::Enum(vec![EnumVariant {
                name: "UserTag".to_string(),
                fields: vec![FieldInfo {
                    name: "0".to_string(),
                    type_name: "Vec<User>".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                }],
                docs: None,
                line: None,
                column: None,
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Test with missing required fields in User structs
        let content = r#"UserTag([User(age: 22), User(email: "hello")])"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;

        // We expect errors about missing fields
        // First User(age: 22) is missing: id, name, email, is_active, roles
        // Second User(email: "hello") is missing: id, name, age, is_active, roles
        assert!(
            !diagnostics.is_empty(),
            "Expected diagnostics for missing fields"
        );

        // Should have 2 diagnostics, one for each struct with missing fields
        assert_eq!(
            diagnostics.len(),
            2,
            "Expected 2 diagnostics (one per struct)"
        );

        // Check first struct error
        assert!(
            diagnostics[0].message.contains("Required fields"),
            "First diagnostic should mention required fields"
        );
        assert!(
            diagnostics[0].message.contains("id"),
            "First struct should be missing 'id' field"
        );
        assert!(
            diagnostics[0].message.contains("name"),
            "First struct should be missing 'name' field"
        );
        assert!(
            diagnostics[0].message.contains("email"),
            "First struct should be missing 'email' field"
        );

        // Check second struct error
        assert!(
            diagnostics[1].message.contains("Required fields"),
            "Second diagnostic should mention required fields"
        );
        assert!(
            diagnostics[1].message.contains("id"),
            "Second struct should be missing 'id' field"
        );
        assert!(
            diagnostics[1].message.contains("age"),
            "Second struct should be missing 'age' field"
        );
    }

    /// Regression test for issue #18: missing fields in structs inside a Vec are not reported
    #[tokio::test]
    async fn test_struct_vec_field_missing_nested_fields() {
        let analyzer = Arc::new(RustAnalyzer::new());

        // Vessel type with required fields
        let vessel_type = TypeInfo {
            name: "Vessel".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "name".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "symbol".to_string(),
                    type_name: "char".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "color".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "role".to_string(),
                    type_name: "String".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        analyzer.insert_type_for_test(vessel_type).await;

        // VesselsData struct with a Vec<Vessel> field
        let type_info = TypeInfo {
            name: "VesselsData".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "vessels".to_string(),
                type_name: "Vec<Vessel>".to_string(),
                docs: None,
                line: None,
                column: None,
                has_default: false,
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Each Vessel is missing name, symbol, color
        let content = r#"VesselsData(
    vessels: [
        Vessel(
            role: "General population",
        ),
        (
            role: "Industrial labor",
        )
    ]
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;

        assert!(
            !diagnostics.is_empty(),
            "Expected diagnostics for missing fields in Vec<Vessel> elements, got none"
        );

        // Both Vessel elements are missing name, symbol, and color
        let missing_field_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("Required fields"))
            .collect();
        assert_eq!(
            missing_field_errors.len(),
            2,
            "Expected 2 'Required fields' errors (one per Vessel). Got: {:?}",
            diagnostics
        );

        // Each error should mention the missing fields
        for diag in &missing_field_errors {
            assert!(
                diag.message.contains("name"),
                "Error should mention missing 'name'. Got: {}",
                diag.message
            );
            assert!(
                diag.message.contains("symbol"),
                "Error should mention missing 'symbol'. Got: {}",
                diag.message
            );
            assert!(
                diag.message.contains("color"),
                "Error should mention missing 'color'. Got: {}",
                diag.message
            );
        }

        // Valid case: all fields present
        let valid_content = r#"VesselsData(
    vessels: [
        Vessel(
            name: "Scout",
            symbol: 'S',
            color: "blue",
            role: "General population",
        ),
    ]
)"#;
        let valid_diagnostics =
            validate_ron_with_analyzer(valid_content, &type_info, analyzer).await;
        assert_eq!(
            valid_diagnostics.len(),
            0,
            "Expected no errors for valid VesselsData. Got: {:?}",
            valid_diagnostics
        );
    }

    #[tokio::test]
    async fn test_enum_variant_in_struct_field_not_false_positive() {
        // Bug fix: `Detailed(length: 1)` was falsely reported as
        // "Type mismatch: expected PostType, got Detailed" because
        // check_type_mismatch_deep ran before checking if Detailed
        // is a valid variant of PostType.
        let analyzer = Arc::new(RustAnalyzer::new());

        let post_type_info = TypeInfo {
            name: "PostType".to_string(),
            kind: TypeKind::Enum(vec![
                EnumVariant {
                    name: "Short".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                },
                EnumVariant {
                    name: "Long".to_string(),
                    fields: vec![],
                    docs: None,
                    line: None,
                    column: None,
                },
                EnumVariant {
                    name: "Described".to_string(),
                    fields: vec![FieldInfo {
                        name: "0".to_string(),
                        type_name: "String".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                    }],
                    docs: None,
                    line: None,
                    column: None,
                },
                EnumVariant {
                    name: "Detailed".to_string(),
                    fields: vec![FieldInfo {
                        name: "length".to_string(),
                        type_name: "usize".to_string(),
                        docs: None,
                        line: None,
                        column: None,
                        has_default: false,
                    }],
                    docs: None,
                    line: None,
                    column: None,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };
        analyzer.insert_type_for_test(post_type_info).await;

        let type_info = TypeInfo {
            name: "Post".to_string(),
            kind: TypeKind::Struct(vec![
                FieldInfo {
                    name: "id".to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
                FieldInfo {
                    name: "post_type".to_string(),
                    type_name: "PostType".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                },
            ]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Single-line variant should not be a false positive
        let content = r#"Post(
    id: 1,
    post_type: Detailed( length: 1 ),
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("expected PostType")),
            "Detailed is a valid PostType variant, should not be a type mismatch. Got: {:?}",
            diagnostics
        );

        // Multi-line variant should also not be a false positive
        let content = r#"Post(
    id: 1,
    post_type: Detailed(
        length: 1,
    ),
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("expected PostType")),
            "Multi-line Detailed should not be a type mismatch. Got: {:?}",
            diagnostics
        );

        // Invalid variant should still be caught
        let content = r#"Post(
    id: 1,
    post_type: Bogus( length: 1 ),
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.to_lowercase().contains("unknown variant 'bogus'")),
            "Bogus is not a valid PostType variant. Got: {:?}",
            diagnostics
        );

        // Unit variant should work too
        let content = r#"Post(
    id: 1,
    post_type: Short,
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("expected PostType")),
            "Short is a valid PostType variant. Got: {:?}",
            diagnostics
        );
    }

    #[tokio::test]
    async fn test_multiline_value_node_diagnostic_has_valid_range() {
        // Bug fix: when a value node spans multiple lines, the diagnostic
        // range used end_pos.column from a different row, causing
        // col_start > col_end and a panic in ariadne.
        let analyzer = Arc::new(RustAnalyzer::new());

        let type_info = TypeInfo {
            name: "Wrapper".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "value".to_string(),
                type_name: "CustomType".to_string(),
                docs: None,
                line: None,
                column: None,
                has_default: false,
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        // Multi-line value that would cause inverted range
        let content = r#"Wrapper(
    value: SomeOtherType(
        nested: 1,
    ),
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        // Verify all diagnostics have valid ranges (start <= end)
        for d in &diagnostics {
            assert!(
                d.range.start.line < d.range.end.line
                    || (d.range.start.line == d.range.end.line
                        && d.range.start.character <= d.range.end.character),
                "Diagnostic range is inverted: start={:?} end={:?} msg={}",
                d.range.start,
                d.range.end,
                d.message
            );
        }
    }

    #[cfg(feature = "cli")]
    #[test]
    fn test_lsp_diagnostics_to_portable_multiline_range() {
        // Bug fix: when an LSP diagnostic spans multiple lines,
        // the portable diagnostic's col_end came from end.character
        // on a different line, which could be less than col_start.
        let multiline_diag = Diagnostic {
            range: Range::new(
                Position::new(5, 20), // start: line 5, col 20
                Position::new(8, 5),  // end: line 8, col 5
            ),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "some error".to_string(),
            ..Default::default()
        };

        let result = lsp_diagnostics_to_portable(&[multiline_diag]);
        assert_eq!(result.len(), 1);
        assert!(
            result[0].col_end > result[0].col_start,
            "col_end ({}) should be greater than col_start ({}) for multi-line diagnostics",
            result[0].col_end,
            result[0].col_start,
        );
    }

    #[cfg(feature = "cli")]
    #[test]
    fn test_lsp_diagnostics_to_portable_single_line() {
        // Single-line diagnostics should pass through col_end unchanged
        let single_line_diag = Diagnostic {
            range: Range::new(Position::new(3, 10), Position::new(3, 25)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "some error".to_string(),
            ..Default::default()
        };

        let result = lsp_diagnostics_to_portable(&[single_line_diag]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].col_start, 10);
        assert_eq!(result[0].col_end, 25);
    }

    /// Regression: a `Vec<Enum>` field must not be reported as a type mismatch.
    ///
    /// The node-based validation handles arrays element by element; the RON-value
    /// fallback cannot resolve enum variants inside a list (there is no field text
    /// for an element), so it used to emit "in Vec: expected Kind, got ()".
    #[tokio::test]
    async fn test_vec_of_enum_field_is_not_a_mismatch() {
        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(TypeInfo {
                name: "Kind".to_string(),
                kind: TypeKind::Enum(vec![
                    EnumVariant {
                        name: "item".to_string(),
                        fields: vec![],
                        docs: None,
                        line: None,
                        column: None,
                    },
                    EnumVariant {
                        name: "group".to_string(),
                        fields: vec![],
                        docs: None,
                        line: None,
                        column: None,
                    },
                ]),
                docs: None,
                source_file: None,
                line: None,
                column: None,
                has_default: false,
            })
            .await;

        let type_info = TypeInfo {
            name: "Container".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "kinds".to_string(),
                type_name: "Vec<Kind>".to_string(),
                docs: None,
                line: None,
                column: None,
                has_default: false,
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        let content = r#"Container(
    kinds: [
        item,
        group,
    ],
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer.clone()).await;
        assert!(
            diagnostics.is_empty(),
            "valid enum variants in a Vec must pass, got: {diagnostics:#?}"
        );

        // An unknown variant is still an error, reported once.
        let content = r#"Container(
    kinds: [
        item,
        bogus,
    ],
)"#;
        let diagnostics = validate_ron_with_analyzer(content, &type_info, analyzer).await;
        assert_eq!(
            diagnostics.len(),
            1,
            "exactly one error for the bogus element, got: {diagnostics:#?}"
        );
        assert!(
            diagnostics[0].message.contains("bogus"),
            "the message should name the variant, got: {:?}",
            diagnostics[0].message
        );
    }

    /// Tuple-struct fields are positional: an element of `Vec<Pair>` is a tuple,
    /// its fields have no names, and the enum inside has to be judged on the node.
    #[tokio::test]
    async fn test_tuple_struct_fields_are_validated() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let variant = |name: &str| EnumVariant {
            name: name.to_string(),
            fields: vec![],
            docs: None,
            line: None,
            column: None,
        };
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
                "Kind",
                TypeKind::Enum(vec![
                    variant("Any"),
                    variant("Item"),
                    variant("Group"),
                    variant("Inner"),
                ]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Direction",
                TypeKind::Enum(vec![variant("Up"), variant("Down")]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Pair",
                TypeKind::Struct(vec![field("0", "Kind"), field("1", "Direction")]),
            ))
            .await;
        let type_info = type_of(
            "Container",
            TypeKind::Struct(vec![field("pairs", "Vec<Pair>")]),
        );

        let good = "Container(\n    pairs: [\n        (Item, Up),\n    ],\n)";
        let diagnostics = validate_ron_with_analyzer(good, &type_info, analyzer.clone()).await;
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");

        let typo = "Container(\n    pairs: [\n        (Inneer, Up),\n    ],\n)";
        let diagnostics = validate_ron_with_analyzer(typo, &type_info, analyzer.clone()).await;
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(
            diagnostics[0].message.contains("Inneer"),
            "{:?}",
            diagnostics[0].message
        );

        let short = "Container(\n    pairs: [\n        (Item),\n    ],\n)";
        let diagnostics = validate_ron_with_analyzer(short, &type_info, analyzer).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("expected 2 values")),
            "{diagnostics:#?}"
        );
    }

    /// Map values have no field of their own: a primitive is judged by the node kind,
    /// so `Option<HashMap<String, String>>` has to reject a bare number.
    #[tokio::test]
    async fn test_map_values_are_validated() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Rule".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "values".to_string(),
                type_name: "Option<HashMap<String, String>>".to_string(),
                docs: None,
                line: None,
                column: None,
                has_default: false,
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        let good = "Rule(\n    values: {\n        \"key.a\": \"2.0\",\n    },\n)";
        let diagnostics = validate_ron_with_analyzer(good, &type_info, analyzer.clone()).await;
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");

        let bad = "Rule(\n    values: {\n        \"key.a\": 0.5,\n    },\n)";
        let diagnostics = validate_ron_with_analyzer(bad, &type_info, analyzer).await;
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(
            diagnostics[0].message.contains("expected String"),
            "{:?}",
            diagnostics[0].message
        );
    }

    /// Primitive fields of nested structs: they never reach the RON-value fallback,
    /// so the node kind has to be checked in place — `count: h` is not a u32.
    #[tokio::test]
    async fn test_nested_primitive_fields_are_validated() {
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
                "Row",
                TypeKind::Struct(vec![field("name", "String"), field("count", "u32")]),
            ))
            .await;
        let type_info = type_of("Table", TypeKind::Struct(vec![field("rows", "Vec<Row>")]));

        let good = "Table(rows: [(name: \"row.a\", count: 2)])";
        let diagnostics = validate_ron_with_analyzer(good, &type_info, analyzer.clone()).await;
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");

        let bad = "Table(rows: [(name: \"row.a\", count: h)])";
        let diagnostics = validate_ron_with_analyzer(bad, &type_info, analyzer).await;
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert!(
            diagnostics[0]
                .message
                .contains("expected u32, got identifier"),
            "{:?}",
            diagnostics[0].message
        );
    }

    /// A syntax error lands on the broken token, not on the start of the file.
    #[test]
    fn syntax_error_range_points_at_the_broken_token() {
        let content = "Root(
    duration: 6a0,
    stacks: 3,
)";
        let range = syntax_error_range(content, "Expected comma at 2:20");
        assert_eq!(
            range.start.line, 1,
            "the line with `6a0`, not the first one: {range:?}"
        );

        let line = content.lines().nth(1).unwrap();
        let token = u32::try_from(line.find("6a0").unwrap()).unwrap();
        assert!(
            range.start.character >= token && range.end.character <= token + 3,
            "the range has to cover the broken token: {range:?}"
        );
    }

    /// The position from a RON message is read even when the line number sits at the very start.
    #[test]
    fn ron_error_position_is_parsed() {
        assert_eq!(parse_error_position("16:20: Expected comma", ""), (15, 19));
        assert_eq!(
            parse_error_position("at line 4 column 7: Expected identifier", ""),
            (3, 6)
        );
    }

    /// `adjust_diagnostic_positions` shifts the lines by the variant's offset and
    /// leaves the columns — and the message — alone.
    #[test]
    fn adjust_diagnostic_positions_shifts_lines_only() {
        let diagnostics = vec![Diagnostic {
            range: Range::new(Position::new(2, 3), Position::new(4, 1)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "mismatch".to_string(),
            ..Default::default()
        }];

        let shifted = adjust_diagnostic_positions(diagnostics, 5);

        assert_eq!(shifted.len(), 1);
        assert_eq!(shifted[0].range.start, Position::new(7, 3));
        assert_eq!(shifted[0].range.end, Position::new(9, 1));
        assert_eq!(shifted[0].message, "mismatch");
    }

    /// `leading_position` reads the `16:20: …` prefix of a RON message and converts
    /// both numbers to 0-indexed ones.
    #[test]
    fn leading_position_reads_the_ron_prefix() {
        assert_eq!(leading_position("16:20: Expected comma"), Some((15, 19)));
        assert_eq!(leading_position("  3:1: Expected comma"), Some((2, 0)));
        // 1-indexed input is clamped, never underflowed.
        assert_eq!(leading_position("0:0: Expected comma"), Some((0, 0)));
        // Not a position prefix: either the second colon or the digits are missing.
        assert_eq!(leading_position("Expected comma at 2:20"), None);
        assert_eq!(leading_position("16:20 Expected comma"), None);
    }

    /// `parse_error_position` falls back to the missing-comma heuristic and, when
    /// even that finds nothing, to the start of the file.
    #[test]
    fn parse_error_position_falls_back_line_by_line() {
        let missing_comma = "Root(\n    duration: 6\n    stacks: 3,\n)";
        assert_eq!(
            parse_error_position("unexpected token", missing_comma),
            (1, 14),
            "the line before the field line ends without a comma"
        );

        assert_eq!(parse_error_position("unexpected token", "Root()"), (0, 0));
    }

    /// `simplify_ron_error` maps the punctuation RON complains about onto a short
    /// hint and keeps the rest of the message otherwise.
    #[test]
    fn simplify_ron_error_names_the_missing_token() {
        assert_eq!(
            simplify_ron_error("2:20: expected `,` instead"),
            "Expected comma between fields"
        );
        assert_eq!(
            simplify_ron_error("2:21: expected `:` instead"),
            "Expected colon after field name"
        );
        assert_eq!(
            simplify_ron_error("3:1: expected closing `)`"),
            "Expected closing parenthesis"
        );
        assert_eq!(
            simplify_ron_error("3:1: expected closing `}`"),
            "Expected closing brace"
        );
        assert_eq!(
            simplify_ron_error("3:1: expected closing `]`"),
            "Expected closing bracket"
        );

        // The wording of the input decides which branch matches: "unexpected" starts
        // uppercase in RON's own messages.
        assert_eq!(
            simplify_ron_error("unexpected end of RON"),
            "Syntax error: end of RON"
        );
        assert_eq!(
            simplify_ron_error("Unexpected end of RON"),
            "RON syntax error: Unexpected end of RON"
        );
        assert_eq!(
            simplify_ron_error("bad input"),
            "RON syntax error: bad input"
        );
    }

    /// `validate_ron_syntax` reports one error diagnostic whose range spans a single
    /// column, and stays quiet for valid RON.
    #[test]
    fn validate_ron_syntax_marks_one_column() {
        assert!(validate_ron_syntax("Root(duration: 6, stacks: 3)").is_empty());

        let diagnostics = validate_ron_syntax("Root(\n    duration: 6,\n");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(!diagnostics[0].message.is_empty());
        assert_eq!(
            diagnostics[0].range.end.line, diagnostics[0].range.start.line,
            "the range stays on one line"
        );
        assert_eq!(
            diagnostics[0].range.end.character,
            diagnostics[0].range.start.character + 1
        );
    }

    /// `check_type_mismatch` names the kind of the value it found, and recurses into
    /// a `Vec` element type.
    #[test]
    fn check_type_mismatch_names_the_found_kind() {
        assert_eq!(check_type_mismatch(&Value::Bool(true), "bool"), None);
        assert_eq!(
            check_type_mismatch(&Value::Bool(true), "u32"),
            Some("expected u32, got bool".to_string())
        );

        assert_eq!(
            check_type_mismatch(&Value::String("x".to_string()), "str"),
            None
        );
        assert_eq!(
            check_type_mismatch(&Value::String("x".to_string()), "u32"),
            Some("expected u32, got string".to_string())
        );

        let integer = ron::from_str::<Value>("5").unwrap();
        assert_eq!(check_type_mismatch(&integer, "u32"), None);
        assert_eq!(
            check_type_mismatch(&integer, "f64"),
            Some("expected f64, got integer".to_string())
        );

        let float = ron::from_str::<Value>("1.5").unwrap();
        assert_eq!(check_type_mismatch(&float, "f64"), None);
        assert_eq!(
            check_type_mismatch(&float, "u32"),
            Some("expected u32, got float".to_string())
        );
        assert_eq!(
            check_type_mismatch(&float, "String"),
            Some("expected String, got number".to_string())
        );

        assert_eq!(check_type_mismatch(&Value::Unit, "()"), None);
        assert_eq!(
            check_type_mismatch(&Value::Unit, "u32"),
            Some("expected u32, got ()".to_string())
        );

        assert_eq!(
            check_type_mismatch(&Value::Seq(vec![Value::Bool(true)]), "Vec<bool>"),
            None
        );
        assert_eq!(
            check_type_mismatch(
                &Value::Seq(vec![Value::Number(ron::Number::I32(1))]),
                "Vec<bool>"
            ),
            Some("in Vec: expected bool, got number".to_string())
        );
    }

    /// `check_type_mismatch_deep` reads the field's raw RON text for custom types,
    /// because the parsed value no longer carries the type name.
    #[test]
    fn check_type_mismatch_deep_reads_the_raw_field_text() {
        let content = "Root(value: Other(1))";
        let value = ron::from_str::<Value>(content).unwrap();

        assert_eq!(
            check_type_mismatch_deep(&value, "Root", content, "value"),
            Some("expected Root, got Other".to_string())
        );
        // The type name in the text matches the declared one.
        assert_eq!(
            check_type_mismatch_deep(&value, "Other", content, "value"),
            None
        );
        // A field that is not in the text has nothing to read.
        assert_eq!(
            check_type_mismatch_deep(&value, "Root", content, "missing"),
            None
        );

        let bare = "Root(value: 1)";
        let value = ron::from_str::<Value>(bare).unwrap();
        assert_eq!(
            check_type_mismatch_deep(&value, "Root", bare, "value"),
            Some("expected Root, got integer".to_string())
        );

        let quoted = "Root(value: \"x\")";
        let value = ron::from_str::<Value>(quoted).unwrap();
        assert_eq!(
            check_type_mismatch_deep(&value, "Root", quoted, "value"),
            Some("expected Root, got string".to_string())
        );

        // Primitives are left to the plain type check, which does not need the text.
        assert_eq!(
            check_type_mismatch_deep(&Value::Bool(true), "bool", "", "value"),
            None
        );
        assert_eq!(
            check_type_mismatch_deep(&Value::Bool(true), "u32", "", "value"),
            Some("expected u32, got bool".to_string())
        );
    }

    /// `extract_inner_type` unwraps exactly one generic layer.
    #[test]
    fn extract_inner_type_unwraps_one_layer() {
        assert_eq!(
            extract_inner_type("Vec<u32>", "Vec<"),
            Some("u32".to_string())
        );
        assert_eq!(
            extract_inner_type("Option<Vec<u32>>", "Option<"),
            Some("Vec<u32>".to_string())
        );
        // Wrong wrapper, and a generic that is never closed.
        assert_eq!(extract_inner_type("String", "Vec<"), None);
        assert_eq!(extract_inner_type("Vec<u32", "Vec<"), None);
    }

    /// `split_map_type` splits at the top-level comma only, so nested generics stay
    /// in one piece.
    #[test]
    fn split_map_type_honours_nested_generics() {
        assert_eq!(split_map_type("String, u32"), Some(("String", "u32")));
        assert_eq!(
            split_map_type("String, Vec<HashMap<String, u32>>"),
            Some(("String", "Vec<HashMap<String, u32>>"))
        );
        assert_eq!(split_map_type("String"), None);
        assert_eq!(split_map_type(""), None);
    }

    /// `is_primitive_type` and `is_std_generic_type` ignore the spaces the analyzer
    /// keeps in declared types.
    #[test]
    fn type_classification_ignores_spaces() {
        assert!(is_primitive_type("u32"));
        assert!(is_primitive_type(" String "));
        assert!(!is_primitive_type("Kind"));
        assert!(!is_primitive_type("Vec<u32>"));

        assert!(is_std_generic_type("Vec<Row>"));
        assert!(is_std_generic_type("Option<Item>"));
        assert!(is_std_generic_type("HashMap<String, u32>"));
        assert!(!is_std_generic_type("Row"));
        assert!(!is_std_generic_type("Vec"));
    }

    /// `is_tuple_struct` recognises the positional field names the analyzer writes
    /// for a tuple struct.
    #[test]
    fn is_tuple_struct_reads_positional_field_names() {
        let fields = |names: &[&str]| -> Vec<FieldInfo> {
            names
                .iter()
                .map(|name| FieldInfo {
                    name: (*name).to_string(),
                    type_name: "u32".to_string(),
                    docs: None,
                    line: None,
                    column: None,
                    has_default: false,
                })
                .collect()
        };

        assert!(is_tuple_struct(&fields(&["0", "1"])));
        assert!(is_tuple_struct(&fields(&["0"])));
        assert!(!is_tuple_struct(&fields(&["0", "name"])));
        assert!(!is_tuple_struct(&fields(&[])));
    }

    /// `extract_map_from_value` keeps maps — named-struct RON included — and rejects
    /// every other value.
    #[test]
    fn extract_map_from_value_keeps_only_maps() {
        let map = ron::from_str::<Value>("{ \"a\": 1 }").unwrap();
        assert_eq!(extract_map_from_value(&map).map(|m| m.len()), Some(1));

        let named = ron::from_str::<Value>("Root(a: 1)").unwrap();
        assert_eq!(extract_map_from_value(&named).map(|m| m.len()), Some(1));

        let number = ron::from_str::<Value>("1").unwrap();
        assert!(extract_map_from_value(&number).is_none());
    }

    /// `find_field_value_position` spans the value node of the named field.
    #[test]
    fn find_field_value_position_spans_the_value() {
        assert_eq!(
            find_field_value_position("Root(a: 1)", "a"),
            Some((0, 8, 9))
        );
        assert_eq!(find_field_value_position("Root(a: 1)", "b"), None);
    }

    /// `find_struct_name_position` spans the name of a named struct and falls back
    /// to the `(0, 0, 1)` sentinel when the root is not a struct.
    #[test]
    fn find_struct_name_position_reports_the_name_span() {
        assert_eq!(find_struct_name_position("Root(a: 1)"), (0, 0, 4));
        assert_eq!(
            find_struct_name_position("\nRoot(\n    a: 1,\n)"),
            (1, 0, 4)
        );
        assert_eq!(find_struct_name_position("42"), (0, 0, 1));
    }

    /// `find_variant_position` locates the first standalone uppercase identifier and
    /// reports `(0, 0)` when the text does not occur.
    #[test]
    fn find_variant_position_locates_the_variant_text() {
        let content = "Root(state: Item, count: 1)";
        assert_eq!(find_variant_position(content, "Root"), (0, 0));
        assert_eq!(find_variant_position(content, "Item"), (0, 12));
        assert_eq!(find_variant_position(content, "state"), (0, 0));
        assert_eq!(find_variant_position(content, "Missing"), (0, 0));
    }

    /// `pick_struct_node` reports only the non-struct elements of a root array: a
    /// root that is neither an array nor a struct stays unreported.
    #[test]
    fn pick_struct_node_reports_array_elements_only() {
        use crate::ts_utils::{self, RonParser};

        let mut parser = RonParser::new();
        let tree = parser.parse("[1, Root(a: 1)]").unwrap();
        let root = ts_utils::find_main_value(&tree).unwrap();

        let mut diagnostics = Vec::new();
        let chosen = pick_struct_node(root, Some(1), &mut diagnostics)
            .expect("the second element is a struct");
        assert_eq!(chosen.kind(), "struct");
        assert!(diagnostics.is_empty());

        let mut diagnostics = Vec::new();
        assert!(pick_struct_node(root, Some(0), &mut diagnostics).is_none());
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("expected struct, got integer"),
            "{:?}",
            diagnostics[0].message
        );

        let mut diagnostics = Vec::new();
        assert!(pick_struct_node(root, None, &mut diagnostics).is_none());
        assert!(pick_struct_node(root, Some(9), &mut diagnostics).is_none());
        assert!(diagnostics.is_empty());
    }

    /// `check_primitive_node` judges a value by its tree-sitter kind, since a map
    /// value has no field of its own to read.
    #[test]
    fn check_primitive_node_judges_by_node_kind() {
        use crate::ts_utils::{self, RonParser};

        let mut parser = RonParser::new();

        let tree = parser.parse("true").unwrap();
        let node = ts_utils::find_main_value(&tree).unwrap();
        assert!(check_primitive_node(&node, "bool").is_none());

        let diagnostic = check_primitive_node(&node, "u32").expect("a boolean is not a u32");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            diagnostic.message.contains("expected u32, got boolean"),
            "{:?}",
            diagnostic.message
        );

        // A name this checker does not know is not its business.
        assert!(check_primitive_node(&node, "Kind").is_none());

        // An integer is acceptable where a float is declared.
        let integer_tree = parser.parse("1").unwrap();
        let integer_node = ts_utils::find_main_value(&integer_tree).unwrap();
        assert!(check_primitive_node(&integer_node, "f32").is_none());

        let float_tree = parser.parse("1.5").unwrap();
        let float_node = ts_utils::find_main_value(&float_tree).unwrap();
        let diagnostic = check_primitive_node(&float_node, "u32").expect("a float is not a u32");
        assert!(
            diagnostic.message.contains("expected u32, got float"),
            "{:?}",
            diagnostic.message
        );
    }

    /// `collect_error_nodes` walks into the nested error nodes, so the broken token
    /// ends up in the list.
    #[test]
    fn collect_error_nodes_finds_the_broken_token() {
        use crate::ts_utils::RonParser;

        let content = "Root(\n    duration: 6a0,\n    stacks: 3,\n)";
        let mut parser = RonParser::new();
        let tree = parser.parse(content).unwrap();

        let mut found = Vec::new();
        collect_error_nodes(&tree.root_node(), &mut found);

        assert!(
            !found.is_empty(),
            "the broken token has to leave an error node"
        );
        assert!(found
            .iter()
            .all(|node| node.is_error() || node.is_missing()));

        let token = content.find("6a0").unwrap();
        assert!(
            found
                .iter()
                .any(|node| node.start_byte() <= token + 3 && node.end_byte() >= token),
            "an error node has to cover `6a0`: {found:?}"
        );
    }

    /// The portable wrappers keep the severity and the line of the diagnostics they
    /// convert.
    #[cfg(feature = "cli")]
    #[tokio::test]
    async fn portable_wrappers_map_syntax_errors() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let type_info = TypeInfo {
            name: "Root".to_string(),
            kind: TypeKind::Struct(vec![FieldInfo {
                name: "duration".to_string(),
                type_name: "u32".to_string(),
                docs: None,
                line: None,
                column: None,
                has_default: false,
            }]),
            docs: None,
            source_file: None,
            line: None,
            column: None,
            has_default: false,
        };

        let broken = "Root(\n    duration: 6,\n";
        let lsp = validate_ron_with_analyzer(broken, &type_info, analyzer.clone()).await;
        let portable = validate_ron_portable(broken, &type_info, analyzer).await;

        assert_eq!(lsp.len(), 1, "{lsp:#?}");
        assert_eq!(portable.len(), lsp.len());
        assert_eq!(portable[0].severity, diagnostic_reporter::Severity::Error);
        assert_eq!(portable[0].line, lsp[0].range.start.line);

        assert!(validate_ron_syntax_portable("Root(duration: 6)").is_empty());
    }

    /// A variant payload is checked against the variant's fields: an unknown kind
    /// inside `FirstParentOf([ ... ])` is reported together with the outer name.
    #[tokio::test]
    async fn test_variant_payload_list_validation() {
        let analyzer = Arc::new(RustAnalyzer::new());
        let field = |name: &str, type_name: &str| crate::rust_analyzer::FieldInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            docs: None,
            line: None,
            column: None,
            has_default: false,
        };
        let variant = |name: &str, fields: Vec<crate::rust_analyzer::FieldInfo>| EnumVariant {
            name: name.to_string(),
            fields,
            docs: None,
            line: None,
            column: None,
        };
        analyzer
            .insert_type_for_test(TypeInfo {
                name: "Root".to_string(),
                kind: TypeKind::Struct(vec![field("rules", "Vec<Rule>")]),
                docs: None,
                source_file: None,
                line: None,
                column: None,
                has_default: false,
            })
            .await;
        analyzer
            .insert_type_for_test(TypeInfo {
                name: "Rule".to_string(),
                kind: TypeKind::Enum(vec![
                    variant("FirstParentOf", vec![field("0", "Vec<Kind>")]),
                    variant("AnyChildOf", vec![]),
                ]),
                docs: None,
                source_file: None,
                line: None,
                column: None,
                has_default: false,
            })
            .await;
        analyzer
            .insert_type_for_test(TypeInfo {
                name: "Kind".to_string(),
                kind: TypeKind::Enum(vec![variant("Creature", vec![]), variant("Item", vec![])]),
                docs: None,
                source_file: None,
                line: None,
                column: None,
                has_default: false,
            })
            .await;

        let root = analyzer.get_type_info("Root").await.expect("type info");
        let valid = "Root(\n    rules: [\n        FirstParentOf([Creature, Item]),\n        AnyChildOf,\n    ],\n)";
        assert!(
            validate_ron_with_analyzer(valid, &root, analyzer.clone())
                .await
                .is_empty(),
            "a known variant with known kinds stays clean"
        );

        let bad_kind = valid.replace("Item", "Bogus");
        let diagnostics = validate_ron_with_analyzer(&bad_kind, &root, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown variant 'Bogus'")),
            "an unknown kind is reported: {diagnostics:?}"
        );

        let bad_rule = valid.replace("AnyChildOf", "NopeOf");
        let diagnostics = validate_ron_with_analyzer(&bad_rule, &root, analyzer.clone()).await;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("Unknown variant 'NopeOf'")),
            "an unknown rule is reported: {diagnostics:?}"
        );
    }
}
