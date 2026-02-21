use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser, TreeCursor};

use crate::model::{Definition, FileExtraction, Import, LanguageKind, Reference, ReferenceKind};

pub fn detect_language(path: &Path) -> Option<LanguageKind> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => Some(LanguageKind::Rust),
        Some("py") => Some(LanguageKind::Python),
        _ => None,
    }
}

pub fn parse_file(path: &Path, source: &str) -> Result<Option<FileExtraction>> {
    let Some(language) = detect_language(path) else {
        return Ok(None);
    };

    let mut parser = Parser::new();
    match language {
        LanguageKind::Rust => parser
            .set_language(&tree_sitter_rust::language())
            .context("failed to load rust grammar")?,
        LanguageKind::Python => parser
            .set_language(&tree_sitter_python::language())
            .context("failed to load python grammar")?,
    }

    let tree = parser
        .parse(source, None)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let mut definitions = Vec::new();
    let mut references = Vec::new();
    let mut imports = Vec::new();

    let mut def_dedupe = HashSet::new();
    let mut ref_dedupe = HashSet::new();
    let mut import_dedupe = HashSet::new();

    let mut scopes = Vec::new();
    walk_tree(
        tree.root_node(),
        source,
        language,
        &mut scopes,
        &mut definitions,
        &mut references,
        &mut imports,
        &mut def_dedupe,
        &mut ref_dedupe,
        &mut import_dedupe,
    );

    Ok(Some(FileExtraction {
        language,
        definitions,
        references,
        imports,
    }))
}

#[allow(clippy::too_many_arguments)]
fn walk_tree(
    node: Node<'_>,
    source: &str,
    language: LanguageKind,
    scopes: &mut Vec<String>,
    definitions: &mut Vec<Definition>,
    references: &mut Vec<Reference>,
    imports: &mut Vec<Import>,
    def_dedupe: &mut HashSet<String>,
    ref_dedupe: &mut HashSet<String>,
    import_dedupe: &mut HashSet<String>,
) {
    if let Some(def) = maybe_extract_definition(node, source, language, scopes) {
        let key = format!("{}:{}:{}:{}", def.qualname, def.kind, def.line, def.col);
        if def_dedupe.insert(key) {
            scopes.push(def.name.clone());
            definitions.push(def);

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_tree(
                    child,
                    source,
                    language,
                    scopes,
                    definitions,
                    references,
                    imports,
                    def_dedupe,
                    ref_dedupe,
                    import_dedupe,
                );
            }
            scopes.pop();
            return;
        }
    }

    if let Some(reference) = maybe_extract_call(node, source, language) {
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
    } else if let Some(reference) = maybe_extract_reference(node, source, language) {
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

    if let Some(import_item) = maybe_extract_import(node, source, language) {
        let key = format!(
            "{}:{}:{}",
            import_item.module, import_item.line, import_item.col
        );
        if import_dedupe.insert(key) {
            imports.push(import_item);
        }
    }

    let mut cursor: TreeCursor<'_> = node.walk();
    for child in node.children(&mut cursor) {
        walk_tree(
            child,
            source,
            language,
            scopes,
            definitions,
            references,
            imports,
            def_dedupe,
            ref_dedupe,
            import_dedupe,
        );
    }
}

fn maybe_extract_definition(
    node: Node<'_>,
    source: &str,
    language: LanguageKind,
    scopes: &[String],
) -> Option<Definition> {
    let kind = node.kind();
    let matches = match language {
        LanguageKind::Rust => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
                | "const_item"
                | "type_item"
                | "macro_definition"
        ),
        LanguageKind::Python => {
            matches!(kind, "function_definition" | "class_definition" | "module")
        }
    };

    if !matches {
        return None;
    }

    let name = if kind == "module" {
        "<module>".to_string()
    } else {
        let name_node = node.child_by_field_name("name")?;
        node_text(name_node, source)?
    };

    let qualname = if scopes.is_empty() {
        name.clone()
    } else {
        format!("{}::{}", scopes.join("::"), name)
    };

    let start = node.start_position();
    let end = node.end_position();

    Some(Definition {
        name,
        qualname,
        kind: kind.to_string(),
        line: start.row as i64 + 1,
        col: start.column as i64 + 1,
        end_line: end.row as i64 + 1,
        end_col: end.column as i64 + 1,
    })
}

fn maybe_extract_call(node: Node<'_>, source: &str, language: LanguageKind) -> Option<Reference> {
    let call_kind = match language {
        LanguageKind::Rust => "call_expression",
        LanguageKind::Python => "call",
    };

    if node.kind() != call_kind {
        return None;
    }

    let function_node = node.child_by_field_name("function")?;
    let name = extract_terminal_identifier(function_node, source)?;
    let start = function_node.start_position();
    let end = function_node.end_position();

    Some(Reference {
        name,
        kind: ReferenceKind::Call,
        line: start.row as i64 + 1,
        col: start.column as i64 + 1,
        end_line: end.row as i64 + 1,
        end_col: end.column as i64 + 1,
    })
}

fn maybe_extract_reference(
    node: Node<'_>,
    source: &str,
    language: LanguageKind,
) -> Option<Reference> {
    let kind = node.kind();

    let should_capture = match language {
        LanguageKind::Rust => matches!(kind, "identifier" | "type_identifier"),
        LanguageKind::Python => kind == "identifier",
    };

    if !should_capture {
        return None;
    }

    let parent_kind = node.parent().map(|n| n.kind()).unwrap_or_default();
    if matches!(
        parent_kind,
        "function_item"
            | "function_definition"
            | "class_definition"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "mod_item"
            | "import_statement"
            | "import_from_statement"
            | "use_declaration"
    ) {
        return None;
    }

    let name = node_text(node, source)?;
    if name == "self" || name == "super" {
        return None;
    }

    let start = node.start_position();
    let end = node.end_position();

    Some(Reference {
        name,
        kind: ReferenceKind::Ref,
        line: start.row as i64 + 1,
        col: start.column as i64 + 1,
        end_line: end.row as i64 + 1,
        end_col: end.column as i64 + 1,
    })
}

fn maybe_extract_import(node: Node<'_>, source: &str, language: LanguageKind) -> Option<Import> {
    let is_import = match language {
        LanguageKind::Rust => node.kind() == "use_declaration",
        LanguageKind::Python => matches!(node.kind(), "import_statement" | "import_from_statement"),
    };

    if !is_import {
        return None;
    }

    let raw = node_text(node, source)?;
    let module = normalize_import(&raw, language);
    if module.is_empty() {
        return None;
    }

    let pos = node.start_position();
    Some(Import {
        module,
        line: pos.row as i64 + 1,
        col: pos.column as i64 + 1,
    })
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
    }
}

fn extract_terminal_identifier(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => node_text(node, source),
        _ => {
            let mut cursor = node.walk();
            let mut last_ident = None;
            for child in node.children(&mut cursor) {
                if let Some(candidate) = extract_terminal_identifier(child, source) {
                    last_ident = Some(candidate);
                }
            }
            last_ident
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
