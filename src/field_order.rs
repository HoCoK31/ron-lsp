//! Field order for RON structs and enum variants.
//!
//! The formatter itself works on syntax only. This module walks the RON tree
//! together with the matching Rust types and records, for every struct node,
//! the field order declared in the model. Keys are tree-sitter start bytes, so
//! the formatter can look an order up without changing its node handling.

use crate::{
    rust_analyzer::{FieldInfo, RustAnalyzer, TypeInfo, TypeKind},
    ts_utils::{self, RonParser},
};
use std::{collections::HashMap, sync::Arc};
use tree_sitter::Node;

/// Field names by struct-node start byte.
pub type FieldOrders = HashMap<usize, Vec<String>>;

/// Build field orders for the values in `content` whose root type is
/// `root_type`.
///
/// `ron.toml` maps a file to the element type of its top-level array; the
/// root array is therefore walked with `root_type` for each element. Unknown
/// types and fields are left untouched.
pub async fn build_field_orders(
    content: &str,
    root_type: &str,
    analyzer: Arc<RustAnalyzer>,
) -> FieldOrders {
    let mut orders = FieldOrders::new();
    // The formatter strips the type annotation before parsing, so the tree
    // offsets used here must be relative to the same RON text.
    let ron_content = if content.trim_start().starts_with("/*")
        && crate::annotation_parser::parse_type_annotation(content).is_some()
    {
        content
            .find("*/")
            .map_or(content, |end| &content[end + 2..])
    } else {
        content
    };
    // All node text lookups below must use the same text the tree was parsed
    // from; otherwise an annotation shifts every byte offset.
    let content = ron_content;
    let mut parser = RonParser::new();
    let Some(tree) = parser.parse(ron_content) else {
        return orders;
    };
    let Some(main) = ts_utils::find_main_value(&tree) else {
        return orders;
    };

    let mut work = Vec::new();
    if main.kind() == "array" && vector_inner(root_type).is_none() {
        for element in ts_utils::array_value_children(&main) {
            work.push((element, root_type.to_string()));
        }
    } else {
        work.push((main, root_type.to_string()));
    }

    while let Some((node, raw_type)) = work.pop() {
        let mut type_name = raw_type;

        // An `Option<T>` value is either `Some(...)` or an implicit `T`; both
        // describe the payload as `T`.
        if let Some(inner) = option_inner(&type_name) {
            if ts_utils::struct_name(&node, content) == Some("Some") {
                for value in ts_utils::struct_values(&node, content) {
                    work.push((value, inner.clone()));
                }
                continue;
            }
            if node.kind() == "identifier" && ts_utils::node_text(&node, content) == Some("None") {
                continue;
            }
            type_name = inner;
        }

        // Arrays carry `Vec<T>` in the model; each element is a `T`.
        if node.kind() == "array" {
            if let Some(inner) = vector_inner(&type_name) {
                for element in ts_utils::array_value_children(&node) {
                    work.push((element, inner.clone()));
                }
            }
            continue;
        }

        let Some(info) = analyzer.get_type_info(&unwrap_wrappers(&type_name)).await else {
            continue;
        };

        if node.kind() == "struct" {
            let Some(fields) = fields_for(&info, &node, content) else {
                continue;
            };

            let names: Vec<String> = fields
                .iter()
                .filter(|field| !is_positional(&field.name))
                .map(|field| field.name.clone())
                .collect();
            if !names.is_empty() {
                orders.insert(node.start_byte(), names);
            }

            for field in ts_utils::struct_fields(&node) {
                let Some(name) = ts_utils::field_name(&field, content) else {
                    continue;
                };
                let Some(value) = ts_utils::field_value(&field) else {
                    continue;
                };
                if let Some(field_info) = fields.iter().find(|field| field.name == name) {
                    work.push((value, field_info.type_name.clone()));
                }
            }

            // Tuple-style structs and tuple payloads have positional fields.
            if ts_utils::struct_fields(&node).is_empty() {
                for (index, value) in ts_utils::struct_values(&node, content).iter().enumerate() {
                    if let Some(field) = fields.get(index) {
                        work.push((*value, field.type_name.clone()));
                    }
                }
            }
        } else if node.kind() == "tuple" {
            let Some(fields) = fields_for(&info, &node, content) else {
                continue;
            };
            for (index, value) in ts_utils::array_value_children(&node).iter().enumerate() {
                if let Some(field) = fields.get(index) {
                    work.push((*value, field.type_name.clone()));
                }
            }
        }
    }

    orders
}

/// The fields of a Rust struct or of the enum variant named by `node`.
fn fields_for<'a>(info: &'a TypeInfo, node: &Node<'_>, content: &str) -> Option<&'a [FieldInfo]> {
    match &info.kind {
        TypeKind::Struct(fields) => Some(fields),
        TypeKind::Enum(variants) => {
            let name = ts_utils::struct_name(node, content)?;
            variants
                .iter()
                .find(|variant| variant.name == name)
                .map(|variant| variant.fields.as_slice())
        }
    }
}

/// `Vec<T>` -> `T`.
fn vector_inner(type_name: &str) -> Option<String> {
    wrapper_inner(type_name, "Vec<")
}

/// `Option<T>` -> `T`.
fn option_inner(type_name: &str) -> Option<String> {
    wrapper_inner(type_name, "Option<")
}

fn wrapper_inner(type_name: &str, wrapper: &str) -> Option<String> {
    let compact = type_name.replace(' ', "");
    let inner = compact.strip_prefix(wrapper)?.strip_suffix('>')?;
    (!inner.is_empty()).then(|| inner.to_string())
}

/// Strip all wrappers down to the type a field value resolves to.
fn unwrap_wrappers(type_name: &str) -> String {
    let mut current = type_name.replace(' ', "");
    loop {
        let next = ["Vec<", "Option<", "Box<", "Rc<", "Arc<"]
            .iter()
            .find_map(|wrapper| {
                current
                    .strip_prefix(wrapper)
                    .and_then(|value| value.strip_suffix('>'))
                    .map(ToString::to_string)
            });
        match next {
            Some(value) => current = value,
            None => return current,
        }
    }
}

fn is_positional(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_are_unwrapped() {
        assert_eq!(vector_inner("Vec< Foo >").as_deref(), Some("Foo"));
        assert_eq!(
            option_inner("Option<Vec<Foo>>").as_deref(),
            Some("Vec<Foo>")
        );
        assert_eq!(unwrap_wrappers("Option<Vec<Foo>>"), "Foo");
    }

    #[test]
    fn positional_names_are_not_sorted() {
        assert!(is_positional("0"));
        assert!(!is_positional("name"));
    }

    /// Root fields and nested enum-variant payloads follow the Rust declaration
    /// order; arrays keep their element order.
    #[tokio::test]
    async fn nested_fields_follow_model_order() {
        use crate::rust_analyzer::{EnumVariant, FieldInfo, TypeInfo, TypeKind};

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

        let analyzer = Arc::new(RustAnalyzer::new());
        analyzer
            .insert_type_for_test(type_of(
                "Root",
                TypeKind::Struct(vec![
                    field("a", "i32"),
                    field("b", "i32"),
                    field("nested", "Variant"),
                    field("list", "Vec<Variant>"),
                ]),
            ))
            .await;
        analyzer
            .insert_type_for_test(type_of(
                "Variant",
                TypeKind::Enum(vec![EnumVariant {
                    name: "Variant".into(),
                    fields: vec![field("a", "i32"), field("z", "i32")],
                    docs: None,
                    line: None,
                    column: None,
                }]),
            ))
            .await;

        let content = r#"/* @[crate::Root] */
(
    // b comes from the model after a
    b: 2,
    nested: Variant(
        z: 3,
        a: 4,
    ),
    list: [Variant(z: 5, a: 6)],
    // a is first in the model
    a: 1,
)"#;
        let orders = build_field_orders(content, "Root", analyzer).await;
        let formatted = crate::format::format_ron_with_orders(
            content,
            &crate::config::FormatSettings::default(),
            Some(&orders),
        );

        assert!(
            formatted.contains("// a is first in the model\n    a: 1,\n    // b comes from the model after a\n    b: 2,"),
            "comments move with their fields: {formatted}"
        );
        assert!(
            formatted.find("a: 1").unwrap() < formatted.find("b: 2").unwrap(),
            "root fields follow the model: {formatted}"
        );
        assert!(
            formatted.contains("nested: Variant(\n        a: 4,\n        z: 3,"),
            "variant payload follows the model: {formatted}"
        );
        assert!(
            formatted.contains("list: [\n        Variant(\n            a: 6,\n            z: 5,"),
            "array payload keeps its element and sorts fields: {formatted}"
        );
    }
}
