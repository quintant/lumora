use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

use crate::languages::{detect_language_from_ext, get_config};
use crate::model::{Definition, FileExtraction, Import, LanguageKind, Reference, ReferenceKind};

pub fn detect_language(path: &Path) -> Option<LanguageKind> {
    let ext = path.extension().and_then(|item| item.to_str())?;
    detect_language_from_ext(ext)
}

pub fn parse_file(path: &Path, source: &str) -> Result<Option<FileExtraction>> {
    let Some(language) = detect_language(path) else {
        return Ok(None);
    };

    let config = get_config(language)
        .ok_or_else(|| anyhow!("no config registered for language {:?}", language))?;

    let mut parser = Parser::new();
    parser
        .set_language(&config.grammar)
        .context("failed to load grammar")?;

    let tree = parser
        .parse(source, None)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let (definitions, references, imports) = extract_with_query(
        &config.grammar,
        config.tags_query,
        tree.root_node(),
        source,
        language,
    )?;

    Ok(Some(FileExtraction {
        language,
        definitions,
        references,
        imports,
    }))
}

#[derive(Clone)]
struct TempDefinition {
    name: String,
    kind: String,
    line: i64,
    col: i64,
    end_line: i64,
    end_col: i64,
    start_byte: usize,
    end_byte: usize,
}

fn extract_with_query(
    grammar: &Language,
    query_str: &str,
    root: Node<'_>,
    source: &str,
    language: LanguageKind,
) -> Result<(Vec<Definition>, Vec<Reference>, Vec<Import>)> {
    if query_str.trim().is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    let query =
        Query::new(grammar, query_str).map_err(|err| anyhow!("query parse error: {err}"))?;
    let capture_names = query.capture_names();

    let mut cursor = QueryCursor::new();
    let mut temp_definitions = Vec::new();
    let mut references = Vec::new();
    let mut imports = Vec::new();
    let mut ref_dedupe = HashSet::new();
    let mut import_dedupe = HashSet::new();

    for query_match in cursor.matches(&query, root, source.as_bytes()) {
        let mut definition_node = None;
        let mut name_nodes = Vec::new();
        let mut call_nodes = Vec::new();
        let mut identifier_nodes = Vec::new();
        let mut import_nodes = Vec::new();

        for capture in query_match.captures {
            let capture_name = capture_names[capture.index as usize];
            let node = capture.node;

            if capture_name == "name" {
                name_nodes.push(node);
                continue;
            }

            if capture_name.starts_with("definition.") {
                if definition_node.is_none() {
                    definition_node = Some(node);
                }
                continue;
            }

            if capture_name == "reference.call" {
                call_nodes.push(node);
                continue;
            }

            if capture_name == "reference.identifier" {
                identifier_nodes.push(node);
                continue;
            }

            if capture_name == "import" {
                import_nodes.push(node);
            }
        }

        for call_node in call_nodes {
            let name_node = name_nodes
                .iter()
                .find(|candidate| node_contains(call_node, **candidate))
                .copied();
            let name = if let Some(name_node) = name_node {
                extract_terminal_identifier(name_node, source)
            } else if let Some(function_node) = call_node.child_by_field_name("function") {
                extract_terminal_identifier(function_node, source)
            } else {
                extract_terminal_identifier(call_node, source)
            };
            let Some(name) = name else {
                continue;
            };
            if should_skip_reference_name(&name) {
                continue;
            }
            let start = call_node.start_position();
            let end = call_node.end_position();
            let reference = Reference {
                name,
                kind: ReferenceKind::Call,
                line: start.row as i64 + 1,
                col: start.column as i64 + 1,
                end_line: end.row as i64 + 1,
                end_col: end.column as i64 + 1,
            };
            let key = format!(
                "{}:{}:{}:{}",
                reference.name,
                reference.kind.as_edge_type(),
                reference.line,
                reference.col
            );
            if ref_dedupe.insert(key) {
                references.push(reference);
            }
        }

        for identifier_node in identifier_nodes {
            if name_nodes
                .iter()
                .any(|name_node| name_node.id() == identifier_node.id())
            {
                continue;
            }
            if should_skip_identifier_reference(identifier_node) {
                continue;
            }
            let Some(name) = node_text(identifier_node, source) else {
                continue;
            };
            if should_skip_reference_name(&name) {
                continue;
            }
            let start = identifier_node.start_position();
            let end = identifier_node.end_position();
            let reference = Reference {
                name,
                kind: ReferenceKind::Ref,
                line: start.row as i64 + 1,
                col: start.column as i64 + 1,
                end_line: end.row as i64 + 1,
                end_col: end.column as i64 + 1,
            };
            let key = format!(
                "{}:{}:{}:{}",
                reference.name,
                reference.kind.as_edge_type(),
                reference.line,
                reference.col
            );
            if ref_dedupe.insert(key) {
                references.push(reference);
            }
        }

        for import_node in import_nodes {
            let Some(raw) = node_text(import_node, source) else {
                continue;
            };
            let module = normalize_import(&raw, language);
            if module.is_empty() {
                continue;
            }
            let start = import_node.start_position();
            let import_item = Import {
                module,
                line: start.row as i64 + 1,
                col: start.column as i64 + 1,
            };
            let key = format!(
                "{}:{}:{}",
                import_item.module, import_item.line, import_item.col
            );
            if import_dedupe.insert(key) {
                imports.push(import_item);
            }
        }

        if let Some(definition_node) = definition_node {
            let definition_name_node = name_nodes
                .iter()
                .find(|candidate| node_contains(definition_node, **candidate))
                .copied();
            let Some(name) = resolve_definition_name(definition_node, definition_name_node, source)
            else {
                continue;
            };
            let start = definition_node.start_position();
            let end = definition_node.end_position();
            temp_definitions.push(TempDefinition {
                name,
                kind: definition_node.kind().to_string(),
                line: start.row as i64 + 1,
                col: start.column as i64 + 1,
                end_line: end.row as i64 + 1,
                end_col: end.column as i64 + 1,
                start_byte: definition_node.start_byte(),
                end_byte: definition_node.end_byte(),
            });
        }
    }

    let definitions = build_qualified_definitions(temp_definitions);
    Ok((definitions, references, imports))
}

fn resolve_definition_name(
    definition_node: Node<'_>,
    definition_name_node: Option<Node<'_>>,
    source: &str,
) -> Option<String> {
    if definition_node.kind() == "module" {
        return Some("<module>".to_string());
    }

    if let Some(name_node) = definition_name_node {
        return extract_terminal_identifier(name_node, source);
    }

    if let Some(name_node) = definition_node.child_by_field_name("name") {
        return extract_terminal_identifier(name_node, source);
    }

    if let Some(type_node) = definition_node.child_by_field_name("type") {
        return extract_terminal_identifier(type_node, source);
    }

    None
}

fn node_contains(container: Node<'_>, candidate: Node<'_>) -> bool {
    container.start_byte() <= candidate.start_byte() && candidate.end_byte() <= container.end_byte()
}

fn build_qualified_definitions(mut temp_definitions: Vec<TempDefinition>) -> Vec<Definition> {
    temp_definitions.sort_by(|left, right| {
        left.start_byte
            .cmp(&right.start_byte)
            .then_with(|| right.end_byte.cmp(&left.end_byte))
    });

    let mut results = Vec::new();
    let mut stack: Vec<(usize, usize, String)> = Vec::new();
    let mut dedupe = HashSet::new();

    for item in temp_definitions {
        while let Some((_, end_byte, _)) = stack.last() {
            if *end_byte <= item.start_byte {
                stack.pop();
            } else {
                break;
            }
        }

        let qualname = if let Some((_, parent_end, parent_qualname)) = stack.last() {
            if *parent_end >= item.end_byte {
                format!("{parent_qualname}::{}", item.name)
            } else {
                item.name.clone()
            }
        } else {
            item.name.clone()
        };

        let definition = Definition {
            name: item.name.clone(),
            qualname: qualname.clone(),
            kind: item.kind,
            line: item.line,
            col: item.col,
            end_line: item.end_line,
            end_col: item.end_col,
        };

        let key = format!(
            "{}:{}:{}:{}",
            definition.qualname, definition.kind, definition.line, definition.col
        );
        if dedupe.insert(key) {
            results.push(definition);
        }

        stack.push((item.start_byte, item.end_byte, qualname));
    }

    results
}

fn should_skip_identifier_reference(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    if let Some(name_field) = parent.child_by_field_name("name") {
        if name_field.id() == node.id() {
            return true;
        }
    }

    let parent_kind = parent.kind();
    if parent_kind.contains("import")
        || parent_kind == "use_declaration"
        || parent_kind == "preproc_include"
    {
        return true;
    }

    matches!(
        parent_kind,
        "function_item"
            | "function_definition"
            | "class_definition"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "mod_item"
            | "function_declaration"
            | "method_declaration"
            | "method_definition"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "constructor_declaration"
            | "type_spec"
            | "struct_specifier"
            | "class_specifier"
            | "namespace_definition"
            | "table"
            | "pair"
            | "block_mapping_pair"
            | "call_expression"
            | "method_invocation"
            | "object_creation_expression"
    )
}

fn should_skip_reference_name(name: &str) -> bool {
    matches!(name, "self" | "super")
}

fn normalize_import(raw: &str, language: LanguageKind) -> String {
    match language {
        LanguageKind::Rust => raw
            .trim()
            .trim_start_matches("use")
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string(),
        LanguageKind::Python => raw
            .trim()
            .replace("import", "")
            .replace("from", "")
            .replace(',', " ")
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
        _ => raw.trim().to_string(),
    }
}

fn extract_terminal_identifier(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "identifier"
        | "type_identifier"
        | "property_identifier"
        | "field_identifier"
        | "namespace_identifier"
        | "simple_identifier"
        | "class_name"
        | "id_name"
        | "attribute_name"
        | "word" => node_text(node, source),
        _ => {
            let mut cursor = node.walk();
            let mut last_ident = None;
            for child in node.children(&mut cursor) {
                if let Some(candidate) = extract_terminal_identifier(child, source) {
                    last_ident = Some(candidate);
                }
            }
            last_ident.or_else(|| node_text(node, source))
        }
    }
}

fn node_text(node: Node<'_>, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes())
        .ok()
        .map(str::trim)
        .filter(|txt| !txt.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn parse_supported(path: &Path, source: &str) -> FileExtraction {
        parse_file(path, source)
            .expect("parse_file should not error")
            .expect("source should be recognized as a supported language")
    }

    fn assert_positions_are_one_indexed(extraction: &FileExtraction) {
        for def in &extraction.definitions {
            assert!(def.line > 0);
            assert!(def.col > 0);
        }
        for reference in &extraction.references {
            assert!(reference.line > 0);
            assert!(reference.col > 0);
        }
        for import_item in &extraction.imports {
            assert!(import_item.line > 0);
            assert!(import_item.col > 0);
        }
    }

    #[test]
    fn detect_language_handles_supported_and_unsupported_extensions() {
        assert_eq!(
            detect_language(Path::new("file.rs")),
            Some(LanguageKind::Rust)
        );
        assert_eq!(
            detect_language(Path::new("script.py")),
            Some(LanguageKind::Python)
        );
        assert_eq!(
            detect_language(Path::new("app.js")),
            Some(LanguageKind::JavaScript)
        );
        assert_eq!(
            detect_language(Path::new("app.tsx")),
            Some(LanguageKind::Tsx)
        );
        assert_eq!(detect_language(Path::new("notes.txt")), None);
    }

    #[test]
    fn parse_file_rust_extracts_definitions_references_imports_and_nested_qualnames() {
        let source = r#"
use std::fmt::Debug;

struct Widget;

enum Choice {
    A,
    B,
}

trait Runner {
    fn run(&self);
}

mod nested {
    fn nested_helper() {}
}

impl Widget {
    fn method(&self, arg: Choice) -> Choice {
        helper(arg)
    }
}

fn helper(value: Choice) -> Choice {
    value
}
"#;

        let extraction = parse_supported(Path::new("sample.rs"), source);
        assert_eq!(extraction.language, LanguageKind::Rust);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper" && item.kind == "function_item"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Widget" && item.kind == "struct_item"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Choice" && item.kind == "enum_item"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Runner" && item.kind == "trait_item"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "method" && item.kind == "function_item"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "nested_helper" && item.qualname == "nested::nested_helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "Choice" && item.kind == ReferenceKind::Ref));
        assert!(extraction
            .imports
            .iter()
            .any(|item| item.module == "std::fmt::Debug"));
        assert_positions_are_one_indexed(&extraction);
    }

    #[test]
    fn parse_file_python_extracts_definitions_calls_and_imports() {
        let source = r#"
import os
from collections import defaultdict

class Greeter:
    def greet(self):
        print("hi")

def helper():
    g = Greeter()
    os.path.join("a", "b")
    return g
"#;

        let extraction = parse_supported(Path::new("sample.py"), source);
        assert_eq!(extraction.language, LanguageKind::Python);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper" && item.kind == "function_definition"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter" && item.kind == "class_definition"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "print" && item.kind == ReferenceKind::Call));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "Greeter" && item.kind == ReferenceKind::Call));
        assert!(extraction.imports.iter().any(|item| item.module == "os"));
        assert!(extraction
            .imports
            .iter()
            .any(|item| item.module == "collections"));
        assert_positions_are_one_indexed(&extraction);
    }

    #[test]
    fn parse_file_empty_supported_file_returns_empty_extraction() {
        let result = parse_file(Path::new("empty.rs"), "").expect("parse_file should not error");
        let extraction = result.expect(".rs should be recognized");
        assert_eq!(extraction.language, LanguageKind::Rust);
        assert!(extraction.definitions.is_empty());
        assert!(extraction.references.is_empty());
        assert!(extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_unsupported_language_returns_none() {
        let txt = parse_file(Path::new("notes.txt"), "fn main() {}")
            .expect("parse_file should not error for unsupported extension");
        assert!(txt.is_none());

        let no_ext =
            parse_file(Path::new("Makefile"), "all:").expect("parse_file should not error");
        assert!(no_ext.is_none());
    }

    #[test]
    fn parse_file_outputs_are_deduplicated_by_name_kind_and_location() {
        let source = r#"
use std::fmt::Debug;

fn helper(value: i32) -> i32 {
    helper(value)
}
"#;
        let extraction = parse_supported(Path::new("dedupe.rs"), source);

        let mut def_keys = HashSet::new();
        for item in &extraction.definitions {
            let key = format!("{}:{}:{}:{}", item.qualname, item.kind, item.line, item.col);
            assert!(def_keys.insert(key));
        }

        let mut ref_keys = HashSet::new();
        for item in &extraction.references {
            let key = format!(
                "{}:{}:{}:{}",
                item.name,
                item.kind.as_edge_type(),
                item.line,
                item.col
            );
            assert!(ref_keys.insert(key));
        }

        let mut import_keys = HashSet::new();
        for item in &extraction.imports {
            let key = format!("{}:{}:{}", item.module, item.line, item.col);
            assert!(import_keys.insert(key));
        }
    }

    #[test]
    fn all_registered_queries_compile() {
        for config in crate::languages::language_configs() {
            if config.tags_query.trim().is_empty() {
                continue;
            }
            Query::new(&config.grammar, config.tags_query).unwrap_or_else(|err| {
                panic!("{} query should compile: {err}", config.kind.as_str())
            });
        }
    }

    #[test]
    fn parse_file_javascript_extracts_basics() {
        let source = r#"
import x from "pkg";
class Greeter { hi() { return helper(); } }
function helper() { return 1; }
const other = () => helper();
"#;
        let extraction = parse_supported(Path::new("sample.js"), source);
        assert_eq!(extraction.language, LanguageKind::JavaScript);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_typescript_extracts_basics() {
        let source = r#"
import { x } from "pkg";
interface User { id: string }
class Greeter { hi(): string { return helper(); } }
function helper(): string { return "ok"; }
"#;
        let extraction = parse_supported(Path::new("sample.ts"), source);
        assert_eq!(extraction.language, LanguageKind::TypeScript);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_go_extracts_basics() {
        let source = r#"
package main
import "fmt"
type User struct{}
func helper() { fmt.Println("x") }
"#;
        let extraction = parse_supported(Path::new("sample.go"), source);
        assert_eq!(extraction.language, LanguageKind::Go);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_java_extracts_basics() {
        let source = r#"
import java.util.List;
class Greeter { void hi() { helper(); } }
class Main { static void helper() {} }
"#;
        let extraction = parse_supported(Path::new("Sample.java"), source);
        assert_eq!(extraction.language, LanguageKind::Java);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_c_extracts_basics() {
        let source = r#"
#include <stdio.h>
int helper() { return 1; }
int main() { return helper(); }
"#;
        let extraction = parse_supported(Path::new("sample.c"), source);
        assert_eq!(extraction.language, LanguageKind::C);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_cpp_extracts_basics() {
        let source = r#"
#include <iostream>
class Greeter {};
int helper() { return 1; }
int main() { return helper(); }
"#;
        let extraction = parse_supported(Path::new("sample.cpp"), source);
        assert_eq!(extraction.language, LanguageKind::Cpp);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_csharp_extracts_basics() {
        let source = r#"
using System;
class Greeter {
    void Hi() { Helper(); }
    static void Helper() {}
}
"#;
        let extraction = parse_supported(Path::new("sample.cs"), source);
        assert_eq!(extraction.language, LanguageKind::CSharp);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction.definitions.iter().any(|item| item.name == "Hi"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "Helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_ruby_extracts_basics() {
        let source = r#"
class Greeter
  def helper
    puts "hi"
  end

  def run
    helper()
  end
end
"#;
        let extraction = parse_supported(Path::new("sample.rb"), source);
        assert_eq!(extraction.language, LanguageKind::Ruby);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
    }

    #[test]
    fn parse_file_bash_extracts_basics() {
        let source = r#"
source ./env.sh
helper() {
  echo hi
}
helper
"#;
        let extraction = parse_supported(Path::new("sample.sh"), source);
        assert_eq!(extraction.language, LanguageKind::Bash);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
    }

    #[test]
    fn parse_file_scala_extracts_basics() {
        let source = r#"
package sample
import scala.util.Try

class Greeter {
  def run(): Int = helper()
}

def helper(): Int = 1
"#;
        let extraction = parse_supported(Path::new("sample.scala"), source);
        assert_eq!(extraction.language, LanguageKind::Scala);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_kotlin_extracts_basics() {
        let source = r#"
import kotlin.collections.List

class Greeter {
    fun run() {
        helper()
    }
}

fun helper() {}
"#;
        let extraction = parse_supported(Path::new("sample.kt"), source);
        assert_eq!(extraction.language, LanguageKind::Kotlin);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_lua_extracts_basics() {
        let source = r#"
obj = {}

function obj.helper()
  return 1
end

obj.helper()
"#;
        let extraction = parse_supported(Path::new("sample.lua"), source);
        assert_eq!(extraction.language, LanguageKind::Lua);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
    }

    #[test]
    fn parse_file_swift_extracts_basics() {
        let source = r#"
import Foundation

class Greeter {
    func run() {
        helper()
    }
}

func helper() {}
"#;
        let extraction = parse_supported(Path::new("sample.swift"), source);
        assert_eq!(extraction.language, LanguageKind::Swift);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Greeter"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
        assert!(!extraction.imports.is_empty());
    }

    #[test]
    fn parse_file_elixir_extracts_basics() {
        let source = r#"
defmodule Demo do
  def helper(x), do: x

  def run do
    helper(1)
  end
end
"#;
        let extraction = parse_supported(Path::new("sample.ex"), source);
        assert_eq!(extraction.language, LanguageKind::Elixir);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "Demo"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper" && item.kind == ReferenceKind::Call));
    }

    #[test]
    fn parse_file_haskell_extracts_basics() {
        let source = r#"
module Main where
import Data.List

data User = User

helper x = x
run = helper 1
"#;
        let extraction = parse_supported(Path::new("sample.hs"), source);
        assert_eq!(extraction.language, LanguageKind::Haskell);
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "helper"));
        assert!(extraction
            .definitions
            .iter()
            .any(|item| item.name == "User"));
        assert!(extraction
            .references
            .iter()
            .any(|item| item.name == "helper"));
        assert!(!extraction.imports.is_empty());
    }
}
