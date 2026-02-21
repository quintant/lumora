use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LanguageKind {
    Rust,
    Python,
}

impl LanguageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Definition {
    pub name: String,
    pub qualname: String,
    pub kind: String,
    pub line: i64,
    pub col: i64,
    pub end_line: i64,
    pub end_col: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reference {
    pub name: String,
    pub kind: ReferenceKind,
    pub line: i64,
    pub col: i64,
    pub end_line: i64,
    pub end_col: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReferenceKind {
    Call,
    Ref,
}

impl ReferenceKind {
    pub fn as_edge_type(self) -> &'static str {
        match self {
            Self::Call => "calls",
            Self::Ref => "references",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Import {
    pub module: String,
    pub line: i64,
    pub col: i64,
}

#[derive(Debug, Clone)]
pub struct FileExtraction {
    pub language: LanguageKind,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    pub imports: Vec<Import>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entity {
    pub id: i64,
    pub entity_type: String,
    pub key: String,
    pub name: String,
    pub lang: Option<String>,
    pub file_path: Option<String>,
    pub line: Option<i64>,
    pub col: Option<i64>,
    pub end_line: Option<i64>,
    pub end_col: Option<i64>,
    pub meta_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub id: i64,
    pub src_entity_id: i64,
    pub dst_entity_id: i64,
    pub edge_type: String,
    pub file_path: Option<String>,
    pub line: Option<i64>,
    pub col: Option<i64>,
    pub meta_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolLocation {
    pub symbol_name: String,
    pub file_path: String,
    pub line: i64,
    pub col: i64,
    pub kind: String,
    pub qualname: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceLocation {
    pub symbol_name: String,
    pub file_path: String,
    pub line: i64,
    pub col: i64,
    pub edge_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyPath {
    pub found: bool,
    pub hops: Vec<PathHop>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathHop {
    pub entity_key: String,
    pub entity_name: String,
    pub entity_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SliceResult {
    pub anchor: Entity,
    pub neighbors: Vec<RelatedEdge>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedEdge {
    pub edge_type: String,
    pub direction: String,
    pub entity: Entity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloneMatch {
    pub other_file: String,
    pub shared_fingerprints: i64,
    pub similarity: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopFileSummary {
    pub file_path: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectorSuggestion {
    pub entity_type: String,
    pub key: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloneHotspot {
    pub directory: String,
    pub files: i64,
    pub avg_similarity: f64,
    pub max_similarity: f64,
}
