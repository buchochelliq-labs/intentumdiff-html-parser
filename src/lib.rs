//! HTML parser plugin - full-parse mode.
//!
//! Handles `.html`, `.htm`, `.xhtml` files.
//! Parses source with tree-sitter-html directly.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct HtmlParser;

/// HTML void elements that never have end tags and where the self-closing
/// slash (`<br/>` vs `<br>`) is semantically irrelevant.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta",
    "param", "source", "track", "wbr",
];

/// HTML boolean attributes where the presence of the attribute matters, not
/// its value. `disabled`, `disabled="disabled"`, and `disabled=""` are all
/// equivalent truthy spellings per the HTML specification.
const BOOLEAN_ATTRIBUTES: &[&str] = &[
    "allowfullscreen", "async", "autofocus", "autoplay", "checked", "controls",
    "default", "defer", "disabled", "formnovalidate", "hidden", "ismap",
    "itemscope", "loop", "multiple", "muted", "nomodule", "novalidate", "open",
    "playsinline", "readonly", "required", "reversed", "selected", "truespeed",
];

/// Nodes that carry no semantic information and should be dropped.
const TRIVIA: &[&str] = &["comment"];

/// Raw text content in HTML is deliberately excluded from SEMANTIC_TYPES so
/// that prose / copy edits don't generate noise in structural diffs.
/// They will still appear as MODIFICATION changes on the parent `element`
/// node when the text changes, which is the correct granularity for code review.
const SEMANTIC_TYPES: &[&str] = &[
    "document",
    "doctype",
    // Element structure — the core diff units
    "element",
    "start_tag",
    "self_closing_tag",
    "attribute",
    // Embedded code blocks (treated as opaque structural children)
    "script_element",
    "style_element",
    // Error recovery node — preserve so errors are visible in diffs
    "erroneous_end_tag",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        "element" | "script_element" | "style_element" => {
            // Label is the tag name from the start_tag child
            for child in &node.children {
                if child.node_type == "start_tag" || child.node_type == "self_closing_tag" {
                    return tag_name_from_tag(child);
                }
            }
            // Fallback to doctype or direct tag_name
            for child in &node.children {
                if child.node_type == "tag_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "start_tag" | "self_closing_tag" => {
            return tag_name_from_tag(node);
        }
        "attribute" => {
            // "class", "id", "href", etc.
            for child in &node.children {
                if child.node_type == "attribute_name" {
                    return child.text_or_empty().to_string();
                }
                if child.is_leaf() {
                    let t = child.text_or_empty();
                    if !t.is_empty() && t != "=" {
                        return t.to_string();
                    }
                }
            }
        }
        "doctype" => return "doctype".to_string(),
        "erroneous_end_tag" => {
            for child in &node.children {
                if child.is_leaf() {
                    let t = child.text_or_empty();
                    if !t.is_empty() {
                        return format!("/{}", t);
                    }
                }
            }
        }
        _ => {}
    }
    node.node_type.clone()
}

fn tag_name_from_tag(tag_node: &CstNode) -> String {
    for child in &tag_node.children {
        if child.node_type == "tag_name" {
            return child.text_or_empty().to_string();
        }
        if child.is_leaf() {
            let t = child.text_or_empty();
            if !t.is_empty() && t != "<" && t != ">" && t != "/" {
                return t.to_string();
            }
        }
    }
    tag_node.node_type.clone()
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    convert_semantic_strict(
        node,
        id_prefix,
        memo,
        &|t| TRIVIA.contains(&t),
        &is_semantic,
        &label_for,
    )
}



use intentumdiff_plugin_sdk::ts_convert::{convert_semantic_strict, node_to_cst};

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_html::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load HTML grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

/// Extract the tag name from a `start_tag` or `self_closing_tag` CST node.
fn tag_name_of(node: &CstNode) -> Option<String> {
    for child in &node.children {
        if child.node_type == "tag_name" {
            return Some(child.text_or_empty().to_string());
        }
    }
    None
}

/// Extract the attribute name from an `attribute` CST node.
fn attribute_name_of(node: &CstNode) -> Option<String> {
    for child in &node.children {
        if child.node_type == "attribute_name" {
            return Some(child.text_or_empty().to_string());
        }
    }
    None
}

/// Normalize the CST so that semantically-equivalent HTML forms produce
/// identical structural hashes.
///
/// - Void elements: `<br/>` (`self_closing_tag`) and `<br>` (`start_tag`)
///   become the same `start_tag` node type.
/// - Boolean attributes: `disabled="disabled"`, `disabled=""`, and `disabled`
///   all collapse to just the attribute name, stripping value children or
///   normalising leaf text.
fn normalize_cst(node: &CstNode) -> CstNode {
    let mut normalized = node.clone();

    normalized.children = normalized.children.iter().map(normalize_cst).collect();

    match normalized.node_type.as_str() {
        "self_closing_tag" => {
            if let Some(tag) = tag_name_of(&normalized) {
                if VOID_ELEMENTS.contains(&tag.as_str()) {
                    normalized.node_type = "start_tag".to_string();
                }
            }
        }
        "attribute" => {
            let attr_name = if normalized.is_leaf() {
                // tree-sitter-html represents attributes as leaf nodes whose
                // text is the full source spelling: `disabled="disabled"`,
                // `type="checkbox"`, or just `disabled`.
                let raw = normalized.text_or_empty();
                raw.split('=').next().unwrap_or(raw).trim().to_string()
            } else {
                attribute_name_of(&normalized).unwrap_or_default()
            };
            if BOOLEAN_ATTRIBUTES.contains(&attr_name.to_lowercase().as_str()) {
                if normalized.is_leaf() {
                    // Collapse the leaf text to just the attribute name so
                    // all boolean spellings hash identically.
                    normalized.text = Some(attr_name);
                } else {
                    normalized
                        .children
                        .retain(|c| c.node_type == "attribute_name");
                }
            }
        }
        _ => {}
    }

    normalized
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
    };
    let root = normalize_cst(&root);
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for HtmlParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "html".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml") {
            return "html".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "<!DOCTYPE html>\n<html>\n<head>\n  <title>My Page</title>\n</head>\n<body>\n  <h1>Hello World</h1>\n  <p>Welcome to my site.</p>\n</body>\n</html>\n".to_string(),
            new: "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"UTF-8\">\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n  <title>My Page</title>\n</head>\n<body>\n  <header>\n    <h1>Hello World</h1>\n  </header>\n  <main>\n    <p>Welcome to my site.</p>\n  </main>\n</body>\n</html>\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["html".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(HtmlParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!HtmlParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = HtmlParser::grammar_id();
        let ids = HtmlParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_html() {
        assert_eq!(
            HtmlParser::detect_language("index.html".to_string(), "".to_string()),
            "html"
        );
    }

    #[test]
    fn detect_language_htm() {
        assert_eq!(
            HtmlParser::detect_language("page.htm".to_string(), "".to_string()),
            "html"
        );
    }

    #[test]
    fn detect_language_xhtml() {
        assert_eq!(
            HtmlParser::detect_language("app.xhtml".to_string(), "".to_string()),
            "html"
        );
    }

    #[test]
    fn detect_language_unknown() {
        let r = HtmlParser::detect_language("main.py".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    #[test]
    fn void_element_self_closing_and_bare_produce_same_hash() {
        let bare = parse_source("<br>\n").unwrap();
        let self_closing = parse_source("<br />\n").unwrap();
        let bare_norm = normalize_cst(&bare);
        let sc_norm = normalize_cst(&self_closing);
        let bare_hash = structural_hash_with_memo(&bare_norm, &mut std::collections::HashMap::new());
        let sc_hash = structural_hash_with_memo(&sc_norm, &mut std::collections::HashMap::new());
        assert_eq!(bare_hash, sc_hash, "void element <br> and <br/> must hash identically");
    }

    #[test]
    fn void_element_hr_self_closing_and_bare_produce_same_hash() {
        let bare = parse_source("<hr>\n").unwrap();
        let self_closing = parse_source("<hr/>\n").unwrap();
        let bare_hash = structural_hash_with_memo(
            &normalize_cst(&bare),
            &mut std::collections::HashMap::new(),
        );
        let sc_hash = structural_hash_with_memo(
            &normalize_cst(&self_closing),
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(bare_hash, sc_hash);
    }

    #[test]
    fn boolean_attribute_value_variants_produce_same_hash() {
        let with_value = parse_source(r#"<input disabled="disabled">"#).unwrap();
        let bare = parse_source("<input disabled>").unwrap();
        let empty = parse_source(r#"<input disabled="">"#).unwrap();

        let h1 = structural_hash_with_memo(
            &normalize_cst(&with_value),
            &mut std::collections::HashMap::new(),
        );
        let h2 = structural_hash_with_memo(
            &normalize_cst(&bare),
            &mut std::collections::HashMap::new(),
        );
        let h3 = structural_hash_with_memo(
            &normalize_cst(&empty),
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(h1, h2, "disabled='disabled' must equal disabled");
        assert_eq!(h1, h3, "disabled='disabled' must equal disabled=''");
    }

    #[test]
    fn non_void_self_closing_element_keeps_node_type() {
        let root = parse_source("<div/>").unwrap();
        let normalized = normalize_cst(&root);
        let tag = normalized
            .walk()
            .find(|n| n.node_type == "self_closing_tag" || n.node_type == "start_tag");
        assert!(tag.is_some(), "div should have a tag child");
        assert_eq!(
            tag.unwrap().node_type, "self_closing_tag",
            "non-void self-closing elements must keep their node type"
        );
    }
}
