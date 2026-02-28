use tree_sitter::Language;

use crate::model::LanguageKind;

#[derive(Clone)]
pub struct LanguageConfig {
    pub kind: LanguageKind,
    pub extensions: &'static [&'static str],
    pub grammar: Language,
    pub tags_query: &'static str,
}

pub fn language_configs() -> Vec<LanguageConfig> {
    vec![
        LanguageConfig {
            kind: LanguageKind::Rust,
            extensions: &["rs"],
            grammar: tree_sitter_rust::language(),
            tags_query: include_str!("queries/rust.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Python,
            extensions: &["py", "pyi"],
            grammar: tree_sitter_python::language(),
            tags_query: include_str!("queries/python.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::JavaScript,
            extensions: &["js", "jsx", "mjs", "cjs"],
            grammar: tree_sitter_javascript::language(),
            tags_query: include_str!("queries/javascript.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::TypeScript,
            extensions: &["ts", "mts", "cts"],
            grammar: tree_sitter_typescript::language_typescript(),
            tags_query: include_str!("queries/typescript.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Tsx,
            extensions: &["tsx"],
            grammar: tree_sitter_typescript::language_tsx(),
            tags_query: include_str!("queries/tsx.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Go,
            extensions: &["go"],
            grammar: tree_sitter_go::language(),
            tags_query: include_str!("queries/go.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Java,
            extensions: &["java"],
            grammar: tree_sitter_java::language(),
            tags_query: include_str!("queries/java.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::C,
            extensions: &["c", "h"],
            grammar: tree_sitter_c::language(),
            tags_query: include_str!("queries/c.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Cpp,
            extensions: &["cpp", "cc", "cxx", "hpp", "hxx", "hh"],
            grammar: tree_sitter_cpp::language(),
            tags_query: include_str!("queries/cpp.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::CSharp,
            extensions: &["cs"],
            grammar: tree_sitter_c_sharp::language(),
            tags_query: include_str!("queries/csharp.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Ruby,
            extensions: &["rb"],
            grammar: tree_sitter_ruby::language(),
            tags_query: include_str!("queries/ruby.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Bash,
            extensions: &["sh", "bash", "zsh"],
            grammar: tree_sitter_bash::language(),
            tags_query: include_str!("queries/bash.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Css,
            extensions: &["css"],
            grammar: tree_sitter_css::language(),
            tags_query: include_str!("queries/css.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Html,
            extensions: &["html", "htm"],
            grammar: tree_sitter_html::language(),
            tags_query: include_str!("queries/html.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Json,
            extensions: &["json"],
            grammar: tree_sitter_json::language(),
            tags_query: include_str!("queries/json.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Toml,
            extensions: &["toml"],
            grammar: tree_sitter_toml_ng::language(),
            tags_query: include_str!("queries/toml.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Yaml,
            extensions: &["yml", "yaml"],
            grammar: tree_sitter_yaml::language(),
            tags_query: include_str!("queries/yaml.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Scala,
            extensions: &["scala", "sc"],
            grammar: tree_sitter_scala::language(),
            tags_query: include_str!("queries/scala.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Kotlin,
            extensions: &["kt", "kts"],
            grammar: tree_sitter_kotlin::language(),
            tags_query: include_str!("queries/kotlin.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Lua,
            extensions: &["lua"],
            grammar: tree_sitter_lua::language(),
            tags_query: include_str!("queries/lua.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Elixir,
            extensions: &["ex", "exs"],
            grammar: tree_sitter_elixir::language(),
            tags_query: include_str!("queries/elixir.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Haskell,
            extensions: &["hs", "lhs"],
            grammar: tree_sitter_haskell::language(),
            tags_query: include_str!("queries/haskell.scm"),
        },
        LanguageConfig {
            kind: LanguageKind::Swift,
            extensions: &["swift"],
            grammar: tree_sitter_swift::language(),
            tags_query: include_str!("queries/swift.scm"),
        },
    ]
}

pub fn detect_language_from_ext(ext: &str) -> Option<LanguageKind> {
    let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
    for config in language_configs() {
        if config
            .extensions
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&normalized))
        {
            return Some(config.kind);
        }
    }
    None
}

pub fn get_config(kind: LanguageKind) -> Option<LanguageConfig> {
    language_configs()
        .into_iter()
        .find(|config| config.kind == kind)
}
