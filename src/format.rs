/// Tree-sitter based RON formatter
/// This formatter uses the AST to properly handle formatting
use crate::{
    annotation_parser,
    config::{FormatSettings, InlinePolicy, LineEnding},
    ts_utils::{self, RonParser},
};
use tree_sitter::Node;

/// Format RON content using the default settings.
#[cfg_attr(not(test), allow(dead_code))]
pub fn format_ron(content: &str) -> String {
    format_ron_with(content, &FormatSettings::default())
}

/// Format RON content using tree-sitter AST and the given settings.
pub fn format_ron_with(content: &str, settings: &FormatSettings) -> String {
    let indent_str: String = if settings.use_tabs {
        "\t".to_string()
    } else {
        " ".repeat(settings.indent)
    };

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
        None => return content.to_string(),
    };

    if tree.root_node().has_error() {
        return content.to_string();
    }
    if let Some(main_value) = ts_utils::find_main_value(&tree) {
        let preamble = ron_content[..main_value.start_byte()].trim();
        if !preamble.is_empty() {
            result.push_str(preamble);
            result.push_str("\n\n");
        }
        format_node(
            &main_value,
            ron_content,
            &mut result,
            0,
            &indent_str,
            false,
            settings,
        );
        result.push_str(&ron_content[main_value.end_byte()..]);
    } else {
        return content.to_string();
    }

    // A file ends with a single newline: without it editors and git treat the file
    // as changed after every format.
    let mut formatted = result.trim_end().to_string();
    if settings.final_newline {
        formatted.push('\n');
    }
    apply_line_ending(formatted, content, settings.line_ending)
}

/// Convert the formatter's LF output to the requested line ending.
fn apply_line_ending(mut text: String, source: &str, ending: LineEnding) -> String {
    let crlf = match ending {
        LineEnding::Lf => false,
        LineEnding::Crlf => true,
        LineEnding::Preserve => source.contains("\r\n"),
    };
    if crlf {
        // The preamble keeps the source line endings: unify before converting.
        text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    text
}

/// Format a single node recursively.
fn format_node(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
    settings: &FormatSettings,
) {
    let kind = node.kind();

    // An inline request from a wrapper (`Some([1])`) is honoured only when the
    // container's own policy allows it: `never` always breaks.
    let inline = inline
        && match kind {
            "array" => settings.array_inline.unwrap_or(settings.inline),
            "map" | "tuple" | "struct" => settings.inline,
            _ => InlinePolicy::Never,
        } != InlinePolicy::Never;

    // A container may stay on one line: the policy and the width decide.
    if !inline
        && matches!(kind, "struct" | "array" | "map" | "tuple")
        && try_inline(node, content, output, indent_level, indent_str, settings)
    {
        return;
    }

    match kind {
        "struct" => format_struct(
            node,
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        ),
        "array" => format_array(
            node,
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        ),
        "map" => format_map(
            node,
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        ),
        "tuple" => format_tuple(
            node,
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        ),
        "field" => format_field(
            node,
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        ),
        "string" | "integer" | "float" | "boolean" | "char" | "identifier" | "unit" => {
            if let Some(text) = ts_utils::node_text(node, content) {
                output.push_str(text);
            }
        }
        _ => {
            if let Some(text) = ts_utils::node_text(node, content) {
                output.push_str(text);
            }
        }
    }
}

/// Format a struct node.
fn format_struct(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
    settings: &FormatSettings,
) {
    if let Some(name) = ts_utils::struct_name(node, content) {
        output.push_str(name);
    }

    if ts_utils::is_empty_structure(node, content) {
        output.push_str("()");
        return;
    }

    let fields = ts_utils::struct_fields(node);
    let values = ts_utils::struct_values(node, content);

    if inline {
        output.push('(');
        if !fields.is_empty() {
            for (index, field) in fields
                .iter()
                .filter(|field| !ts_utils::is_trivia(field.kind()))
                .enumerate()
            {
                if index > 0 {
                    output.push_str(", ");
                }
                format_field(
                    field,
                    content,
                    output,
                    indent_level,
                    indent_str,
                    true,
                    settings,
                );
            }
        } else {
            let mut first = true;
            for value in values
                .iter()
                .filter(|value| !ts_utils::is_trivia(value.kind()))
            {
                if !first {
                    output.push_str(", ");
                }
                first = false;
                format_node(
                    value,
                    content,
                    output,
                    indent_level,
                    indent_str,
                    true,
                    settings,
                );
            }
        }
        output.push(')');
        return;
    }

    output.push('(');

    if !fields.is_empty() {
        output.push('\n');
        let children = ts_utils::named_children(node);
        let last = children
            .iter()
            .rposition(|child| !ts_utils::is_trivia(child.kind()));
        for (index, child) in children.iter().enumerate() {
            if ts_utils::is_trivia(child.kind()) {
                output.push_str(&indent_str.repeat(indent_level + 1));
                format_node(
                    child,
                    content,
                    output,
                    indent_level + 1,
                    indent_str,
                    false,
                    settings,
                );
                output.push('\n');
                continue;
            }
            if child.kind() != "field" {
                continue;
            }
            output.push_str(&indent_str.repeat(indent_level + 1));
            format_field(
                child,
                content,
                output,
                indent_level + 1,
                indent_str,
                false,
                settings,
            );
            // A trailing comma is configurable; between fields it is always there.
            if index != last.unwrap_or(usize::MAX) || settings.trailing_comma {
                output.push(',');
            }
            output.push('\n');
        }
        output.push_str(&indent_str.repeat(indent_level));
    } else if !values.is_empty() {
        // Tuple-style struct like Some("value"): a single value stays inline.
        let single = values.len() == 1;
        if !single {
            output.push('\n');
        }
        // An inline payload keeps the indent of the value that carries it.
        let payload_level = if single {
            indent_level
        } else {
            indent_level + 1
        };
        let last = values
            .iter()
            .rposition(|value| !ts_utils::is_trivia(value.kind()));
        for (index, child) in values.iter().enumerate() {
            let trivia = ts_utils::is_trivia(child.kind());
            if !single {
                output.push_str(&indent_str.repeat(indent_level + 1));
            }
            format_node(
                child,
                content,
                output,
                payload_level,
                indent_str,
                single,
                settings,
            );
            // A tuple-style struct keeps no trailing comma: `Pair(1, 2)`.
            if !trivia && index != last.unwrap_or(usize::MAX) {
                output.push(',');
            }
            if !single {
                output.push('\n');
            } else if index + 1 < values.len() {
                output.push(' ');
            }
        }
        if !single {
            output.push_str(&indent_str.repeat(indent_level));
        }
    }

    output.push(')');
}

/// Format a `name: value` field.
fn format_field(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
    settings: &FormatSettings,
) {
    if let Some(name) = ts_utils::field_name(node, content) {
        output.push_str(name);
    }
    output.push_str(": ");
    if let Some(value) = ts_utils::field_value(node) {
        format_node(
            &value,
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        );
    }
}

/// Format an array node.
fn format_array(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
    settings: &FormatSettings,
) {
    if ts_utils::is_empty_structure(node, content) {
        output.push_str("[]");
        return;
    }

    let elements = ts_utils::named_children(node);

    if inline {
        output.push('[');
        let mut first = true;
        for element in elements
            .iter()
            .filter(|element| !ts_utils::is_trivia(element.kind()))
        {
            if !first {
                output.push_str(", ");
            }
            first = false;
            format_node(
                element,
                content,
                output,
                indent_level,
                indent_str,
                true,
                settings,
            );
        }
        output.push(']');
        return;
    }

    let last = elements
        .iter()
        .rposition(|element| !ts_utils::is_trivia(element.kind()));
    output.push('[');
    output.push('\n');
    for (index, element) in elements.iter().enumerate() {
        if ts_utils::is_trivia(element.kind()) {
            output.push_str(&indent_str.repeat(indent_level + 1));
            format_node(
                element,
                content,
                output,
                indent_level + 1,
                indent_str,
                false,
                settings,
            );
            output.push('\n');
            continue;
        }
        output.push_str(&indent_str.repeat(indent_level + 1));
        format_node(
            element,
            content,
            output,
            indent_level + 1,
            indent_str,
            false,
            settings,
        );
        if index != last.unwrap_or(usize::MAX) || settings.trailing_comma {
            output.push(',');
        }
        output.push('\n');
    }
    output.push_str(&indent_str.repeat(indent_level));
    output.push(']');
}

/// Format a map node.
fn format_map(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
    settings: &FormatSettings,
) {
    if ts_utils::is_empty_structure(node, content) {
        output.push_str("{}");
        return;
    }

    let entries = ts_utils::named_children(node);

    if inline {
        output.push('{');
        let mut first = true;
        for entry in entries
            .iter()
            .filter(|entry| !ts_utils::is_trivia(entry.kind()))
        {
            if !first {
                output.push_str(", ");
            }
            first = false;
            format_map_entry(
                entry,
                content,
                output,
                indent_level,
                indent_str,
                true,
                settings,
            );
        }
        output.push('}');
        return;
    }

    let last = entries
        .iter()
        .rposition(|entry| !ts_utils::is_trivia(entry.kind()));
    output.push('{');
    output.push('\n');
    for (index, entry) in entries.iter().enumerate() {
        if ts_utils::is_trivia(entry.kind()) {
            output.push_str(&indent_str.repeat(indent_level + 1));
            format_node(
                entry,
                content,
                output,
                indent_level + 1,
                indent_str,
                false,
                settings,
            );
            output.push('\n');
            continue;
        }
        output.push_str(&indent_str.repeat(indent_level + 1));
        format_map_entry(
            entry,
            content,
            output,
            indent_level + 1,
            indent_str,
            false,
            settings,
        );
        if index != last.unwrap_or(usize::MAX) || settings.trailing_comma {
            output.push(',');
        }
        output.push('\n');
    }
    output.push_str(&indent_str.repeat(indent_level));
    output.push('}');
}

/// Format one `key: value` map entry.
fn format_map_entry(
    entry: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
    settings: &FormatSettings,
) {
    let children = ts_utils::named_children(entry);
    if children.len() >= 2 {
        format_node(
            &children[0],
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        );
        output.push_str(": ");
        format_node(
            &children[1],
            content,
            output,
            indent_level,
            indent_str,
            inline,
            settings,
        );
    }
}

/// Format a tuple node.
fn format_tuple(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    inline: bool,
    settings: &FormatSettings,
) {
    let elements = ts_utils::named_children(node);

    if inline {
        output.push('(');
        let mut first = true;
        for element in elements
            .iter()
            .filter(|element| !ts_utils::is_trivia(element.kind()))
        {
            if !first {
                output.push_str(", ");
            }
            first = false;
            format_node(
                element,
                content,
                output,
                indent_level,
                indent_str,
                true,
                settings,
            );
        }
        output.push(')');
        return;
    }

    let last = elements
        .iter()
        .rposition(|element| !ts_utils::is_trivia(element.kind()));
    output.push('(');
    if !elements.is_empty() {
        output.push('\n');
    }
    for (index, element) in elements.iter().enumerate() {
        if ts_utils::is_trivia(element.kind()) {
            output.push_str(&indent_str.repeat(indent_level + 1));
            format_node(
                element,
                content,
                output,
                indent_level + 1,
                indent_str,
                false,
                settings,
            );
            output.push('\n');
            continue;
        }
        output.push_str(&indent_str.repeat(indent_level + 1));
        format_node(
            element,
            content,
            output,
            indent_level + 1,
            indent_str,
            false,
            settings,
        );
        if index != last.unwrap_or(usize::MAX) || settings.trailing_comma {
            output.push(',');
        }
        output.push('\n');
    }
    if !elements.is_empty() {
        output.push_str(&indent_str.repeat(indent_level));
    }
    output.push(')');
}

/// Length of the current output line, in characters.
fn current_column(output: &str) -> usize {
    output
        .rsplit('\n')
        .next()
        .map(|line| line.chars().count())
        .unwrap_or(0)
}

/// Whether the subtree carries comments: a container with comments never inlines.
fn has_comments(node: &Node) -> bool {
    if ts_utils::is_trivia(node.kind()) {
        return true;
    }
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).collect();
    children.iter().any(has_comments)
}

/// Render the node on one line when the policy allows and it fits the width.
fn try_inline(
    node: &Node,
    content: &str,
    output: &mut String,
    indent_level: usize,
    indent_str: &str,
    settings: &FormatSettings,
) -> bool {
    let policy = match node.kind() {
        "array" => settings.array_inline.unwrap_or(settings.inline),
        "map" | "tuple" | "struct" => settings.inline,
        _ => return false,
    };
    if policy == InlinePolicy::Never {
        return false;
    }
    if policy == InlinePolicy::Preserve && node.start_position().row != node.end_position().row {
        return false;
    }
    if has_comments(node) {
        return false;
    }

    let mut candidate = String::new();
    format_node(
        node,
        content,
        &mut candidate,
        indent_level,
        indent_str,
        true,
        settings,
    );
    if candidate.contains('\n') {
        return false;
    }

    if policy == InlinePolicy::Width {
        if let Some(width) = settings.print_width {
            if current_column(output) + candidate.chars().count() > width {
                return false;
            }
        }
    }

    output.push_str(&candidate);
    true
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

    /// `inline = "width"`: a short container stays on one line, a long one breaks.
    #[test]
    fn width_policy_inlines_short_containers() {
        let settings = FormatSettings {
            inline: InlinePolicy::Width,
            print_width: Some(40),
            ..FormatSettings::default()
        };
        assert_eq!(
            format_ron_with("Root(items: [1, 2])", &settings),
            "Root(items: [1, 2])\n"
        );
        let long = format_ron_with("Root(items: [111, 222, 333, 444, 555, 666])", &settings);
        assert!(long.contains("[\n"), "{long}");
    }

    /// `array_inline = "preserve"` keeps the array shape the author wrote.
    #[test]
    fn preserve_policy_keeps_the_array_shape() {
        let settings = FormatSettings {
            array_inline: Some(InlinePolicy::Preserve),
            ..FormatSettings::default()
        };
        assert_eq!(
            // The struct policy stays `never`, so only the array keeps its shape.
            format_ron_with("Root(items: [1, 2])", &settings),
            "Root(\n    items: [1, 2],\n)\n"
        );
        let out = format_ron_with("Root(items: [\n1,\n2,\n])", &settings);
        assert!(out.contains("[\n"), "{out}");
    }

    /// A container without comments never inlines and keeps breaking as before.
    #[test]
    fn never_policy_keeps_breaking() {
        let settings = FormatSettings::default();
        assert_eq!(
            format_ron_with("Root(items: [1, 2])", &settings),
            "Root(\n    items: [\n        1,\n        2,\n    ],\n)\n"
        );
    }

    /// The trailing comma before a closing bracket can be switched off.
    #[test]
    fn trailing_comma_can_be_disabled() {
        let settings = FormatSettings {
            trailing_comma: false,
            ..FormatSettings::default()
        };
        assert_eq!(
            format_ron_with("Root(items: [1])", &settings),
            "Root(\n    items: [\n        1\n    ]\n)\n"
        );
    }

    /// The final newline is a setting too.
    #[test]
    fn final_newline_can_be_disabled() {
        let settings = FormatSettings {
            final_newline: false,
            ..FormatSettings::default()
        };
        assert_eq!(
            format_ron_with("Root(a: 1)", &settings),
            "Root(\n    a: 1,\n)"
        );
    }

    /// Line endings follow the setting; `preserve` mirrors the source.
    #[test]
    fn line_endings_follow_the_setting() {
        let crlf = FormatSettings {
            line_ending: LineEnding::Crlf,
            ..FormatSettings::default()
        };
        let out = format_ron_with("Root(a: 1)\n", &crlf);
        assert!(out.contains("\r\n"), "{out:?}");

        let lf = FormatSettings {
            line_ending: LineEnding::Lf,
            ..FormatSettings::default()
        };
        let out = format_ron_with("Root(a: 1)\r\n", &lf);
        assert!(!out.contains('\r'), "{out:?}");

        let settings = FormatSettings::default();
        let out = format_ron_with("Root(a: 1)\r\n", &settings);
        assert!(out.contains("\r\n"), "preserve keeps CRLF: {out:?}");
    }

    /// Indent width and tabs are settings as well.
    #[test]
    fn indent_is_configurable() {
        let narrow = FormatSettings {
            indent: 2,
            ..FormatSettings::default()
        };
        assert!(format_ron_with("Root(a: 1)", &narrow).contains("\n  a: 1,"));

        let tabs = FormatSettings {
            use_tabs: true,
            ..FormatSettings::default()
        };
        assert!(format_ron_with("Root(a: 1)", &tabs).contains("\n\ta: 1,"));
    }
}
