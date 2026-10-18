/// Tree-sitter based RON formatter
/// This formatter uses the AST to properly handle formatting
use crate::{
    annotation_parser,
    ts_utils::{self, RonParser},
};
use tree_sitter::Node;

/// Format RON content using tree-sitter AST
pub fn format_ron(content: &str) -> String {
    let indent_str = "    "; // 4 spaces

    // Build the formatted output
    let mut result = String::new();

    // Check for type annotation using the existing parser
    // (tree-sitter grammar has issues with /* @[...] */ vs block comments)
    if let Some(annotation) = annotation_parser::parse_type_annotation(content) {
        result.push_str("/* @[");
        result.push_str(&annotation);
        result.push_str("] */\n\n");
    }

    // Parse the RON content (without annotation) for formatting
    let ron_content = if content.trim_start().starts_with("/*")
        && annotation_parser::parse_type_annotation(content).is_some()
    {
        // Skip past the type annotation
        if let Some(end_idx) = content.find("*/") {
            &content[end_idx + 2..]
        } else {
            content
        }
    } else {
        content
    };

    let mut parser = RonParser::new();
    let tree = match parser.parse(ron_content) {
        Some(t) => t,
        None => {
            // If parsing fails, return original content
            return content.to_string();
        }
    };

    // Format the main value
    if tree.root_node().has_error() {
        return content.to_string();
    }
    if let Some(main_value) = ts_utils::find_main_value(&tree) {
        // Extensions and comments belong to the document, not its main value.
        // Keep their original order and spelling, including unknown extensions.
        let preamble = ron_content[..main_value.start_byte()].trim();
        if !preamble.is_empty() {
            result.push_str(preamble);
            result.push_str("\n\n");
        }
        format_node(&main_value, ron_content, &mut result, 0, indent_str, false);
        result.push_str(&ron_content[main_value.end_byte()..]);
    } else {
        return content.to_string();
    }

    // A file ends with a single newline: without it editors and git treat the file
    // as changed after every format.
    let mut formatted = result.trim_end().to_string();
    formatted.push('\n');
    formatted
}

/// Format a single node recursively
fn format_node(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
) {
    let kind = node.kind();

    match kind {
        "struct" => format_struct(node, content, output, indent_level, indent_str, inline),
        "array" => format_array(node, content, output, indent_level, indent_str, inline),
        "map" => format_map(node, content, output, indent_level, indent_str, inline),
        "tuple" => format_tuple(node, content, output, indent_level, indent_str, inline),
        "field" => format_field(node, content, output, indent_level, indent_str),
        "string" | "integer" | "float" | "boolean" | "char" | "identifier" | "unit" => {
            // Leaf nodes - just output their text
            if let Some(text) = ts_utils::node_text(node, content) {
                output.push_str(text);
            }
        }
        _ => {
            // For other nodes, just output their text as-is
            if let Some(text) = ts_utils::node_text(node, content) {
                output.push_str(text);
            }
        }
    }
}

/// Format a struct node
fn format_struct(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    _inline: bool,
) {
    // Get struct name if it exists
    if let Some(name) = ts_utils::struct_name(node, content) {
        output.push_str(name);
    }

    // Check if empty
    let is_empty = ts_utils::is_empty_structure(node, content);

    if is_empty {
        output.push_str("()");
        return;
    }

    output.push('(');

    // Get all fields and values to determine if this is a tuple-style or field-style struct
    let fields = ts_utils::struct_fields(node);
    let values = ts_utils::struct_values(node, content);

    // If we have fields, use field formatting
    if !fields.is_empty() {
        output.push('\n');

        // Walk children in order so comments between fields stay where they are
        let children = ts_utils::named_children(node);

        for child in children.iter() {
            let kind = child.kind();

            if ts_utils::is_trivia(kind) {
                output.push_str(&indent_str.repeat(indent_level + 1));
                format_node(child, content, output, indent_level + 1, indent_str, false);
                output.push('\n');
                continue;
            }

            // The struct name and anything else that is not a field was handled above
            if kind != "field" {
                continue;
            }

            output.push_str(&indent_str.repeat(indent_level + 1));
            format_field(child, content, output, indent_level + 1, indent_str);
            // Comma after every field, trailing one included
            output.push(',');
            output.push('\n');
        }

        output.push_str(&indent_str.repeat(indent_level));
    } else if !values.is_empty() {
        // Tuple-style struct like Some("value") - these should stay inline if single element
        let should_inline = values.len() == 1;

        if !should_inline {
            output.push('\n');
        }

        // An inline payload keeps the indent of the value that carries it: a list
        // that breaks anyway (`FirstParentOf([A])`) then aligns with its own line,
        // not one level deeper.
        let payload_level = if should_inline {
            indent_level
        } else {
            indent_level + 1
        };

        for (i, child) in values.iter().enumerate() {
            if ts_utils::is_trivia(child.kind()) {
                if !should_inline {
                    output.push_str(&indent_str.repeat(indent_level + 1));
                }
                format_node(
                    child,
                    content,
                    output,
                    payload_level,
                    indent_str,
                    should_inline,
                );
                output.push('\n');
                continue;
            }

            if !should_inline {
                output.push_str(&indent_str.repeat(indent_level + 1));
            }
            format_node(
                child,
                content,
                output,
                payload_level,
                indent_str,
                should_inline,
            );

            if i < values.len() - 1 {
                output.push(',');
                if !should_inline {
                    output.push('\n');
                } else {
                    output.push(' ');
                }
            }
        }

        if !should_inline {
            output.push('\n');
            output.push_str(&indent_str.repeat(indent_level));
        }
    }

    output.push(')');
}

/// Format a field node
fn format_field(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
) {
    // Get field name
    if let Some(name) = ts_utils::field_name(node, content) {
        output.push_str(name);
        output.push_str(": ");
    }

    // Get field value
    if let Some(value) = ts_utils::field_value(node) {
        format_node(&value, content, output, indent_level, indent_str, false);
    }
}

/// Format an array node
fn format_array(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    _inline: bool,
) {
    let is_empty = ts_utils::is_empty_structure(node, content);

    if is_empty {
        output.push_str("[]");
        return;
    }

    output.push('[');

    // Get all array elements (named children that aren't punctuation)
    let elements = ts_utils::named_children(node);

    if !elements.is_empty() {
        output.push('\n');

        for element in elements.iter() {
            output.push_str(&indent_str.repeat(indent_level + 1));
            format_node(
                element,
                content,
                output,
                indent_level + 1,
                indent_str,
                false,
            );

            // Comments are not elements: they take no separator
            if !ts_utils::is_trivia(element.kind()) {
                output.push(',');
            }
            output.push('\n');
        }

        output.push_str(&indent_str.repeat(indent_level));
    }

    output.push(']');
}

/// Format a map node
fn format_map(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    _inline: bool,
) {
    let is_empty = ts_utils::is_empty_structure(node, content);

    if is_empty {
        output.push_str("{}");
        return;
    }

    output.push('{');

    // Walk children in order so comments between entries stay where they are
    let entries = ts_utils::named_children(node);

    if !entries.is_empty() {
        output.push('\n');

        for entry in entries.iter() {
            output.push_str(&indent_str.repeat(indent_level + 1));

            if ts_utils::is_trivia(entry.kind()) {
                format_node(entry, content, output, indent_level + 1, indent_str, false);
                output.push('\n');
                continue;
            }

            // Format map entry (key: value)
            let children = ts_utils::named_children(entry);
            if children.len() >= 2 {
                // Key
                format_node(
                    &children[0],
                    content,
                    output,
                    indent_level + 1,
                    indent_str,
                    false,
                );
                output.push_str(": ");
                // Value
                format_node(
                    &children[1],
                    content,
                    output,
                    indent_level + 1,
                    indent_str,
                    false,
                );
            }

            output.push(',');
            output.push('\n');
        }

        output.push_str(&indent_str.repeat(indent_level));
    }

    output.push('}');
}

/// Format a tuple node
fn format_tuple(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    _inline: bool,
) {
    output.push('(');

    let elements = ts_utils::named_children(node);

    if !elements.is_empty() {
        output.push('\n');

        for element in elements.iter() {
            output.push_str(&indent_str.repeat(indent_level + 1));
            format_node(
                element,
                content,
                output,
                indent_level + 1,
                indent_str,
                false,
            );

            // Comments are not elements: they take no separator
            if !ts_utils::is_trivia(element.kind()) {
                output.push(',');
            }
            output.push('\n');
        }

        output.push_str(&indent_str.repeat(indent_level));
    }

    output.push(')');
}

#[cfg(test)]
mod tests {

    /// Formatting without the trailing newline: layouts are easier to compare that
    /// way, and the newline itself has its own test.
    fn formatted(input: &str) -> String {
        format_ron(input).trim_end().to_string()
    }

    /// A file ends with a single newline: otherwise editors and git treat the file
    /// as changed after every format.
    #[test]
    fn output_ends_with_a_single_newline() {
        let once = format_ron("Config(roles: [1])");
        assert!(once.ends_with('\n'), "trailing: {once:?}");
        assert!(!once.ends_with("\n\n"), "exactly one: {once:?}");
        assert_eq!(format_ron("Config(roles: [1])\n\n\n"), once, "idempotent");
    }

    use super::*;

    #[test]
    fn preserves_extensions_and_deserialization() {
        #[derive(serde::Deserialize)]
        struct Data {
            name: Option<String>,
        }
        for prefix in ["", "/* @[crate::models::Data] */\n\n"] {
            let input = format!("{prefix}#![enable(implicit_some)]\n#![enable(unwrap_newtypes)]\nData(name: \"aoeu\",)");
            let output = formatted(&input);
            assert!(output.contains("#![enable(implicit_some)]"));
            assert!(output.contains("#![enable(unwrap_newtypes)]"));
            assert!(output.contains("    name: \"aoeu\","));
            let data: Data = ron::from_str(&output).unwrap();
            assert_eq!(data.name.as_deref(), Some("aoeu"));
            assert_eq!(formatted(&output), output);
        }
    }

    #[test]
    fn comments_in_arrays_keep_no_comma() {
        let input = "[\n    // first\n    (\n        key: \"a\",\n    ),\n    // second\n    (\n        key: \"b\",\n    ),\n]\n";
        let once = formatted(input);

        assert!(once.contains("// first\n"), "{once}");
        assert!(!once.contains("// first,"), "{once}");
        assert!(once.contains("// second\n"), "{once}");
        // Idempotent: a second pass changes nothing
        assert_eq!(formatted(&once), once);
    }

    #[test]
    fn comments_between_fields_are_kept() {
        let input = "(\n    key: \"a\", // trailing\n    // leading\n    params: [],\n)\n";
        let formatted = formatted(input);

        assert!(formatted.contains("// trailing"), "{formatted}");
        assert!(formatted.contains("// leading"), "{formatted}");
    }

    #[test]
    fn comments_in_maps_are_kept() {
        let input = "{\n    // entry\n    \"a\": 1,\n}\n";
        let formatted = formatted(input);

        assert!(formatted.contains("// entry"), "{formatted}");
        assert!(!formatted.contains("// entry,"), "{formatted}");
    }

    #[test]
    fn preserves_header_comments_and_invalid_input() {
        let input = "/* header */\n#![enable(implicit_some)]\nData(name: \"x\")";
        assert!(formatted(input).starts_with("/* header */"));
        assert_eq!(formatted("Data(name:"), "Data(name:");
    }

    #[test]
    fn test_simple_struct() {
        let input = "User(id: 1, name: \"Alice\")";
        let formatted = formatted(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("User("));
        assert!(formatted.contains("    id: 1,"));
        assert!(formatted.contains("    name: \"Alice\","));
        assert!(formatted.contains(")"));
    }

    #[test]
    fn test_empty_parens() {
        let input = "Unit()";
        let formatted = formatted(input);
        println!("Formatted: '{}'", formatted);
        assert_eq!(formatted, "Unit()");
    }

    #[test]
    fn test_nested_struct() {
        let input = "Post(author: User(id: 1))";
        let formatted = formatted(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("Post("));
        assert!(formatted.contains("    author: User("));
        assert!(formatted.contains("        id: 1,"));
        assert!(formatted.contains("    )"));
        assert!(formatted.trim().ends_with(")"));
    }

    #[test]
    fn test_array() {
        let input = r#"Config(roles: ["admin", "user"])"#;
        let formatted = formatted(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("roles: ["));
        assert!(formatted.contains(r#"        "admin","#));
        assert!(formatted.contains(r#"        "user","#));
        assert!(formatted.contains("    ],") || formatted.contains("    ]\n)"));
    }

    #[test]
    fn test_with_type_annotation() {
        let input = "/* @[crate::User] */\nUser(id: 1)";
        let formatted = formatted(input);
        println!("Formatted:\n{}", formatted);
        assert!(formatted.contains("/* @[crate::User] */"));
        assert!(formatted.contains("User("));
        assert!(formatted.contains("    id: 1,"));
    }

    #[test]
    fn tuple_elements_go_on_their_own_lines() {
        let once = formatted("(1, 2,)");
        let expected = "(
    1,
    2,
)";
        assert_eq!(once, expected);
        // A trailing comma in the source normalizes to one comma per element
        assert_eq!(formatted("(1, 2)"), expected);
        // Idempotent: a second pass changes nothing
        assert_eq!(formatted(&once), once);
    }

    #[test]
    fn nested_tuples_indent_by_one_level() {
        let expected = "(
    (
        1,
        2,
    ),
    3,
)";
        assert_eq!(formatted("((1, 2), 3)"), expected);
    }

    #[test]
    fn tuple_struct_values_go_on_their_own_lines() {
        // Values are separated by commas only between them, so the last one
        // carries no comma (unlike fields, arrays and tuples)
        let expected = "Pair(
    1,
    2
)";
        assert_eq!(formatted("Pair(1, 2)"), expected);
        assert_eq!(formatted("Pair(1, 2,)"), expected);
    }

    #[test]
    fn single_value_tuple_struct_stays_inline() {
        assert_eq!(formatted("Some(\"value\")"), "Some(\"value\")");

        // The wrapper keeps one line, the wrapped struct still breaks, and its fields
        // align with the call that carries them, not one level deeper
        let expected = "Wrap(Inner(\n    a: 1,\n))";
        assert_eq!(formatted("Wrap(Inner(a: 1))"), expected);
    }

    #[test]
    fn inline_wrapper_does_not_inline_container_values() {
        // The inline flag only reaches structs: an array breaks anyway, and it aligns
        // with the call that carries it
        let expected = "Some([\n    1,\n])";
        assert_eq!(formatted("Some([1])"), expected);
    }

    /// A variant payload is a list: it breaks, but stays aligned with the field that
    /// carries the call - not one level deeper.
    #[test]
    fn variant_payload_aligns_with_its_field() {
        let expected = "Rule(\n    target: FirstParentOf([\n        Creature,\n    ]),\n)";
        assert_eq!(
            formatted("Rule(target: FirstParentOf([Creature]))"),
            expected
        );
    }

    #[test]
    fn comments_between_tuple_struct_values_are_kept() {
        // A comment takes a line of its own and no comma; the separator is
        // index based, so the value after it is still the last one
        let expected = "Pair(
    1,
    /* keep */
    2
)";
        assert_eq!(formatted("Pair(1, /* keep */ 2)"), expected);
    }

    #[test]
    fn comments_between_tuple_elements_are_kept() {
        // Every element gets a comma, only the comment is left without one
        let expected = "(
    1,
    // note
    2,
)";
        assert_eq!(formatted("(1, // note\n2)"), expected);
    }

    #[test]
    fn comment_in_array_keeps_its_own_line_without_comma() {
        // Comments before the first and after the last element both stay put
        let expected = "[
    // note
    1,
    /* after */
]";
        let input = "[\n    // note\n    1,\n    /* after */\n]\n";
        assert_eq!(formatted(input), expected);
    }

    #[test]
    fn array_elements_go_on_their_own_lines() {
        let expected = "[\n    1,\n    2,\n]";
        assert_eq!(formatted("[1, 2]"), expected);
        assert_eq!(formatted("[1, 2,]"), expected);
    }

    #[test]
    fn map_entries_go_on_their_own_lines() {
        let expected = r#"{
    "a": 1,
    "b": 2,
}"#;
        assert_eq!(formatted("{\"a\": 1, \"b\": 2}"), expected);
        assert_eq!(formatted("{\"a\": 1, \"b\": 2,}"), expected);
    }

    #[test]
    fn nested_structs_indent_by_one_level() {
        let expected = "A(
    b: B(
        c: 1,
    ),
)";
        assert_eq!(formatted("A(b: B(c: 1))"), expected);
    }

    #[test]
    fn empty_containers_stay_compact() {
        assert_eq!(formatted("[]"), "[]");
        assert_eq!(formatted("{}"), "{}");
        // `()` is the unit value, not an empty container
        assert_eq!(formatted("()"), "()");
    }

    #[test]
    fn leaf_values_keep_their_source_text() {
        for (input, expected) in [
            ("42", "42"),
            ("-1_000", "-1_000"),
            ("1.5", "1.5"),
            ("true", "true"),
            ("'c'", "'c'"),
            ("\"text\"", "\"text\""),
            ("Bare", "Bare"),
        ] {
            assert_eq!(formatted(input), expected, "input: {input}");
        }
    }

    #[test]
    fn input_without_a_main_value_is_returned_verbatim() {
        assert_eq!(formatted(""), "");
        // Comments alone carry no value to format, so nothing is rewritten
        assert_eq!(formatted("// only"), "// only");
        assert_eq!(formatted("/* only */"), "/* only */");
    }
}
