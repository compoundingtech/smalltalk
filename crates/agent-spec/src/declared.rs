//! Strict, source-located view of canonical Agent Spec KDL.
//!
//! This is the shared declaration boundary for consumers that need more than
//! the runner-normalized [`crate::AgentSpec`]. It preserves every node, entry,
//! duplicate occurrence, and the original bytes while giving policy layers a
//! typed tree. Parsing owns syntax and declaration-shape diagnostics; st2 and
//! downstream consumers retain their separate semantic policies.

use std::fs;
use std::path::{Path, PathBuf};

use kdl::{KdlDocument, KdlNode, KdlValue};
use serde::Serialize;

/// A stable byte and human source location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredSpan {
    pub offset: usize,
    pub length: usize,
    pub line: usize,
    pub column: usize,
}

/// Severity of a declaration-parser diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeclaredSeverity {
    Error,
    Warning,
}

/// Machine-matchable declaration diagnostic identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeclaredDiagnosticCode {
    KdlSyntax,
    UnexpectedTopLevelNode,
    TaskNameMissing,
    UnsupportedSchedule,
    DuplicateRoutingField,
    UnsupportedStreamInterval,
}

impl DeclaredDiagnosticCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KdlSyntax => "kdl-syntax",
            Self::UnexpectedTopLevelNode => "unexpected-top-level-node",
            Self::TaskNameMissing => "task-name-missing",
            Self::UnsupportedSchedule => "unsupported-schedule",
            Self::DuplicateRoutingField => "duplicate-routing-field",
            Self::UnsupportedStreamInterval => "unsupported-stream-interval",
        }
    }
}

impl std::fmt::Display for DeclaredDiagnosticCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A stable parser or declaration-shape diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredDiagnostic {
    pub severity: DeclaredSeverity,
    pub code: DeclaredDiagnosticCode,
    pub source: PathBuf,
    pub span: DeclaredSpan,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// A typed KDL value. The surrounding entry retains its source span.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "value")]
pub enum DeclaredValue {
    String(String),
    Integer(i128),
    Float(f64),
    Bool(bool),
    Null,
}

impl DeclaredValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i128> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

/// One positional argument or named property, preserving duplicates and order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredEntry {
    pub name: Option<String>,
    pub value: DeclaredValue,
    pub span: DeclaredSpan,
}

/// One lossless-semantic KDL node.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredNode {
    pub name: String,
    pub type_name: Option<String>,
    pub entries: Vec<DeclaredEntry>,
    pub children: Vec<DeclaredNode>,
    pub span: DeclaredSpan,
}

impl DeclaredNode {
    pub fn argument(&self, index: usize) -> Option<&DeclaredValue> {
        self.entries
            .iter()
            .filter(|entry| entry.name.is_none())
            .nth(index)
            .map(|entry| &entry.value)
    }

    pub fn arguments(&self) -> impl Iterator<Item = &DeclaredValue> {
        self.entries
            .iter()
            .filter(|entry| entry.name.is_none())
            .map(|entry| &entry.value)
    }

    pub fn property(&self, name: &str) -> Option<&DeclaredValue> {
        self.entries
            .iter()
            .find(|entry| entry.name.as_deref() == Some(name))
            .map(|entry| &entry.value)
    }

    pub fn properties_named<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a DeclaredEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.name.as_deref() == Some(name))
    }

    pub fn child(&self, name: &str) -> Option<&DeclaredNode> {
        self.children.iter().find(|node| node.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a DeclaredNode> {
        self.children.iter().filter(move |node| node.name == name)
    }
}

/// One declared agent with its complete node and exact source slice.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredAgent {
    pub node: DeclaredNode,
    pub span: DeclaredSpan,
    source: String,
}

impl DeclaredAgent {
    pub fn identity(&self) -> Option<&DeclaredValue> {
        self.field("identity")
            .and_then(|node| node.argument(0))
            .or_else(|| self.node.argument(0))
    }

    pub fn field(&self, name: &str) -> Option<&DeclaredNode> {
        self.node.child(name)
    }

    pub fn fields_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a DeclaredNode> {
        self.node.children_named(name)
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

/// A canonical KDL document and every declared agent in source order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredDocument {
    pub source_name: PathBuf,
    pub source: String,
    pub nodes: Vec<DeclaredNode>,
    pub agents: Vec<DeclaredAgent>,
}

/// Parser result. Shape errors retain the typed document; syntax errors do not.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredParse {
    pub document: Option<DeclaredDocument>,
    pub diagnostics: Vec<DeclaredDiagnostic>,
}

impl DeclaredParse {
    pub fn is_valid(&self) -> bool {
        self.document.is_some()
            && !self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == DeclaredSeverity::Error)
    }
}

/// Parse canonical KDL without lowering away declarations needed by policy consumers.
pub fn parse_declared_document(source_name: &Path, source: &str) -> DeclaredParse {
    let parsed = match KdlDocument::parse(source) {
        Ok(document) => document,
        Err(error) => {
            let diagnostics = error
                .diagnostics
                .iter()
                .map(|diagnostic| DeclaredDiagnostic {
                    severity: DeclaredSeverity::Error,
                    code: DeclaredDiagnosticCode::KdlSyntax,
                    source: source_name.to_path_buf(),
                    span: source_span(source, diagnostic.span.offset(), diagnostic.span.len()),
                    message: diagnostic
                        .message
                        .clone()
                        .unwrap_or_else(|| "invalid KDL syntax".to_owned()),
                    help: diagnostic.help.clone(),
                })
                .collect();
            return DeclaredParse {
                document: None,
                diagnostics,
            };
        }
    };

    let nodes = parsed
        .nodes()
        .iter()
        .map(|node| declared_node(source, node))
        .collect::<Vec<_>>();
    let agents = nodes
        .iter()
        .filter(|node| node.name == "agent")
        .cloned()
        .map(|node| {
            let range = node.span.offset..node.span.offset + node.span.length;
            DeclaredAgent {
                span: node.span,
                source: source.get(range).unwrap_or_default().to_owned(),
                node,
            }
        })
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    for node in &nodes {
        if node.name != "agent" {
            diagnostics.push(shape_diagnostic(
                source_name,
                node.span,
                DeclaredDiagnosticCode::UnexpectedTopLevelNode,
                format!(
                    "canonical Agent Spec KDL only accepts top-level `agent` nodes, found `{}`",
                    node.name
                ),
            ));
            continue;
        }
        for field in node.children_named("id").skip(1) {
            diagnostics.push(shape_diagnostic(
                source_name,
                field.span,
                DeclaredDiagnosticCode::DuplicateRoutingField,
                "agent id must be declared exactly once".to_owned(),
            ));
        }
        for field in node.children_named("address").skip(1) {
            diagnostics.push(shape_diagnostic(
                source_name,
                field.span,
                DeclaredDiagnosticCode::DuplicateRoutingField,
                "agent address must be declared exactly once".to_owned(),
            ));
        }
        for field in node.children_named("identity").skip(1) {
            diagnostics.push(shape_diagnostic(
                source_name,
                field.span,
                DeclaredDiagnosticCode::DuplicateRoutingField,
                "agent identity must be declared exactly once".to_owned(),
            ));
        }
        for field in node.children_named("host").skip(1) {
            diagnostics.push(shape_diagnostic(
                source_name,
                field.span,
                DeclaredDiagnosticCode::DuplicateRoutingField,
                "agent host must be declared exactly once".to_owned(),
            ));
        }
        for child in &node.children {
            match child.name.as_str() {
                "pty" | "exec" | "stream"
                    if child.argument(0).and_then(DeclaredValue::as_str).is_none() =>
                {
                    diagnostics.push(shape_diagnostic(
                        source_name,
                        child.span,
                        DeclaredDiagnosticCode::TaskNameMissing,
                        format!("{} task must have one positional string name", child.name),
                    ));
                }
                // A command-bearing stream lowers to a derived exec companion. A command-less
                // stream is an external ingress endpoint. `every` would make either one a schedule,
                // which is the reserved `schedule` node's business.
                "stream" => {
                    for field in child.children_named("every") {
                        diagnostics.push(shape_diagnostic(
                            source_name,
                            field.span,
                            DeclaredDiagnosticCode::UnsupportedStreamInterval,
                            "stream `every` is reserved for the future `schedule` contract; a stream \
                             declares a long-running event source"
                                .to_owned(),
                        ));
                    }
                }
                "schedule" => diagnostics.push(shape_diagnostic(
                    source_name,
                    child.span,
                    DeclaredDiagnosticCode::UnsupportedSchedule,
                    "scheduled work is reserved for a future contract and is not implemented"
                        .to_owned(),
                )),
                _ => {}
            }
        }
    }
    DeclaredParse {
        document: Some(DeclaredDocument {
            source_name: source_name.to_path_buf(),
            source: source.to_owned(),
            nodes,
            agents,
        }),
        diagnostics,
    }
}

/// Read and parse a canonical KDL declaration.
pub fn parse_declared_file(path: &Path) -> std::io::Result<DeclaredParse> {
    fs::read_to_string(path).map(|source| parse_declared_document(path, &source))
}

fn declared_node(source: &str, node: &KdlNode) -> DeclaredNode {
    let raw_span = node.span();
    DeclaredNode {
        name: node.name().value().to_owned(),
        type_name: node.ty().map(|name| name.value().to_owned()),
        entries: node
            .entries()
            .iter()
            .map(|entry| {
                let raw_span = entry.span();
                DeclaredEntry {
                    name: entry.name().map(|name| name.value().to_owned()),
                    value: declared_value(entry.value()),
                    span: source_span(source, raw_span.offset(), raw_span.len()),
                }
            })
            .collect(),
        children: node
            .children()
            .into_iter()
            .flat_map(|children| children.nodes())
            .map(|child| declared_node(source, child))
            .collect(),
        span: source_span(source, raw_span.offset(), raw_span.len()),
    }
}

fn declared_value(value: &KdlValue) -> DeclaredValue {
    match value {
        KdlValue::String(value) => DeclaredValue::String(value.clone()),
        KdlValue::Integer(value) => DeclaredValue::Integer(*value),
        KdlValue::Float(value) => DeclaredValue::Float(*value),
        KdlValue::Bool(value) => DeclaredValue::Bool(*value),
        KdlValue::Null => DeclaredValue::Null,
    }
}

fn source_span(source: &str, offset: usize, length: usize) -> DeclaredSpan {
    let prefix = source.get(..offset).unwrap_or(source);
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.chars().count() + 1, |(_, tail)| {
            tail.chars().count() + 1
        });
    DeclaredSpan {
        offset,
        length,
        line,
        column,
    }
}

fn shape_diagnostic(
    source: &Path,
    span: DeclaredSpan,
    code: DeclaredDiagnosticCode,
    message: String,
) -> DeclaredDiagnostic {
    DeclaredDiagnostic {
        severity: DeclaredSeverity::Error,
        code,
        source: source.to_path_buf(),
        span,
        message,
        help: None,
    }
}
