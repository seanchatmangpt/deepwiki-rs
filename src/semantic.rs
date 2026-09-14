use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SDOC: &str = "https://ggen.dev/ontology/semantic-documentation#";

#[derive(Debug, Clone)]
pub struct SemanticDocsOptions {
    pub rustdoc_json: PathBuf,
    pub output: PathBuf,
    pub receipt: Option<PathBuf>,
    pub repository: Option<String>,
    pub revision: Option<String>,
    pub open_ontologies_bin: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct SemanticDocumentationReceipt {
    pub schema_version: &'static str,
    pub extractor: &'static str,
    pub rustdoc_format_version: u64,
    pub repository: String,
    pub revision: String,
    pub input: String,
    pub output: String,
    pub item_count: usize,
    pub documented_item_count: usize,
    pub reference_count: usize,
    pub source_span_count: usize,
    pub authority: &'static str,
    pub standing: &'static str,
    pub validation: ValidationReceipt,
    pub falsifiers: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct ValidationReceipt {
    pub validator: String,
    pub requested: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub validated: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct GraphCounts {
    item_count: usize,
    documented_item_count: usize,
    reference_count: usize,
    source_span_count: usize,
}

pub fn run(options: SemanticDocsOptions) -> Result<SemanticDocumentationReceipt> {
    let raw = fs::read_to_string(&options.rustdoc_json)
        .with_context(|| format!("failed to read Rustdoc JSON at {}", options.rustdoc_json.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("invalid Rustdoc JSON at {}", options.rustdoc_json.display()))?;

    let revision = options
        .revision
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .filter(|value| !value.trim().is_empty())
        .context("semantic documentation requires an exact revision via --revision or GITHUB_SHA")?;
    let repository = options
        .repository
        .or_else(|| std::env::var("GITHUB_REPOSITORY").ok())
        .filter(|value| !value.trim().is_empty())
        .context("semantic documentation requires a repository identity via --repository or GITHUB_REPOSITORY")?;

    let (trig, format_version, counts) = compile_rustdoc_value(&value, &repository, &revision)?;
    if let Some(parent) = options.output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&options.output, trig.as_bytes())
        .with_context(|| format!("failed to write semantic graph to {}", options.output.display()))?;

    let validation = if let Some(bin) = &options.open_ontologies_bin {
        validate_with_open_ontologies(bin, &options.output)?
    } else {
        ValidationReceipt {
            validator: "open-ontologies".to_string(),
            requested: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            validated: false,
        }
    };

    // `open-ontologies validate` validates the RDF/ontology document. SHACL is
    // a separate Open Ontologies boundary (`shacl <shapesfile>` / onto_shacl),
    // so successful validation must not be upgraded to semantic admission or
    // ALIVE standing here.
    let receipt = SemanticDocumentationReceipt {
        schema_version: "litho.semantic-documentation.receipt/v1",
        extractor: "rustdoc-json",
        rustdoc_format_version: format_version,
        repository,
        revision,
        input: options.rustdoc_json.display().to_string(),
        output: options.output.display().to_string(),
        item_count: counts.item_count,
        documented_item_count: counts.documented_item_count,
        reference_count: counts.reference_count,
        source_span_count: counts.source_span_count,
        authority: "none",
        standing: "PARTIAL_ALIVE",
        validation,
        falsifiers: vec![
            "repository or exact revision identity is absent",
            "Rustdoc JSON lacks format_version or index",
            "semantic graph cannot be written",
            "requested Open Ontologies validation returns non-zero",
            "same input/repository/revision produces different TriG bytes",
            "emitted semantic subject diverges from semantic-documentation-contract vocabulary",
        ],
    };

    let receipt_path = options
        .receipt
        .unwrap_or_else(|| PathBuf::from(format!("{}.receipt.json", options.output.display())));
    if let Some(parent) = receipt_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)
        .with_context(|| format!("failed to write receipt to {}", receipt_path.display()))?;

    if receipt.validation.requested && !receipt.validation.validated {
        bail!(
            "Open Ontologies validation refused semantic documentation graph (exit {:?}); receipt preserved at {}",
            receipt.validation.exit_code,
            receipt_path.display()
        );
    }

    Ok(receipt)
}

fn validate_with_open_ontologies(bin: &Path, graph: &Path) -> Result<ValidationReceipt> {
    let output = Command::new(bin)
        .arg("validate")
        .arg(graph)
        .output()
        .with_context(|| format!("failed to execute Open Ontologies binary {}", bin.display()))?;

    Ok(ValidationReceipt {
        validator: bin.display().to_string(),
        requested: true,
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        validated: output.status.success(),
    })
}

fn compile_rustdoc_value(
    root: &Value,
    repository: &str,
    revision: &str,
) -> Result<(String, u64, GraphCounts)> {
    let format_version = root
        .get("format_version")
        .and_then(Value::as_u64)
        .context("Rustdoc JSON missing numeric format_version")?;
    let index = root
        .get("index")
        .and_then(Value::as_object)
        .context("Rustdoc JSON missing index object")?;
    let paths = root.get("paths").and_then(Value::as_object);
    let root_id = root.get("root").map(scalar_string).unwrap_or_default();

    let revision_segment = iri_segment(revision);
    let revision_iri = format!("urn:litho:revision:{}", revision_segment);
    let dataset_iri = format!("urn:litho:semantic-documentation:{}", revision_segment);
    let source_graph = format!("urn:litho:graph:source:{}", revision_segment);
    let docs_graph = format!("urn:litho:graph:documentation:{}", revision_segment);

    let mut source = String::new();
    let mut docs = String::new();
    let mut counts = GraphCounts::default();

    source.push_str(&format!(
        "  <{}> a sdoc:RevisionBoundSubject, prov:Entity ;\n    sdoc:repository {} ;\n    sdoc:revision {} ;\n    sdoc:grantsDoAuthority false .\n\n",
        revision_iri,
        literal(repository),
        literal(revision)
    ));
    source.push_str(&format!(
        "  <{}> a sdoc:SemanticDocumentationDataset, prov:Entity ;\n    sdoc:repository {} ;\n    sdoc:revision {} ;\n    sdoc:sourceGraph {} ;\n    sdoc:documentationGraph {} ;\n    sdoc:extractor \"rustdoc-json\" ;\n    sdoc:rustdocFormatVersion {} ;\n    sdoc:standing \"PARTIAL_ALIVE\" ;\n    prov:wasDerivedFrom <{}> ;\n    sdoc:grantsDoAuthority false .\n",
        dataset_iri,
        literal(repository),
        literal(revision),
        literal(&source_graph),
        literal(&docs_graph),
        format_version,
        revision_iri
    ));

    if !root_id.is_empty() {
        source.push_str(&format!(
            "  <{}> sdoc:rootItem <{}> .\n",
            dataset_iri,
            item_iri(revision, &root_id)
        ));
    }

    docs.push_str("  sdoc:RustdocMarkdown a sdoc:DocumentationKind ; sdoc:grantsDoAuthority false .\n");

    let mut ids: Vec<&String> = index.keys().collect();
    ids.sort();

    for id in ids {
        let Some(item) = index.get(id).and_then(Value::as_object) else {
            continue;
        };
        counts.item_count += 1;
        let iri = item_iri(revision, id);
        let name = item.get("name").and_then(Value::as_str);
        let kind = item_kind(item);
        let visibility = item
            .get("visibility")
            .map(canonical_value)
            .unwrap_or_else(|| "unknown".to_string());
        let qualified_path = paths
            .and_then(|p| p.get(id))
            .and_then(|v| v.get("path"))
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("::")
            })
            .filter(|s| !s.is_empty());

        source.push_str(&format!(
            "  <{}> a sdoc:RustItem, sdoc:SourceEvidence, prov:Entity ;\n    sdoc:localId {} ;\n    sdoc:itemKind {} ;\n    sdoc:visibility {} ;\n    dct:source {} ;\n    prov:wasDerivedFrom <{}> ;\n",
            iri,
            literal(id),
            literal(kind),
            literal(&visibility),
            literal(&format!("rustdoc-json:item:{}", id)),
            revision_iri
        ));
        if let Some(name) = name {
            source.push_str(&format!("    sdoc:name {} ;\n", literal(name)));
        }
        if let Some(path) = qualified_path {
            source.push_str(&format!("    sdoc:fullyQualifiedPath {} ;\n", literal(&path)));
        }
        source.push_str("    sdoc:grantsDoAuthority false .\n");
        source.push_str(&format!("  <{}> sdoc:hasEvidence <{}> .\n", dataset_iri, iri));

        if let Some(span) = item.get("span").and_then(Value::as_object) {
            let span_iri = format!("{}/span", iri);
            let filename = span.get("filename").and_then(Value::as_str).unwrap_or("");
            let begin = coordinate(span.get("begin"));
            let end = coordinate(span.get("end"));
            counts.source_span_count += 1;
            source.push_str(&format!(
                "  <{}> a sdoc:SourceSpan, prov:Entity ;\n    sdoc:sourceFile {} ;\n    sdoc:startLine {} ;\n    sdoc:startColumn {} ;\n    sdoc:endLine {} ;\n    sdoc:endColumn {} ;\n    sdoc:grantsDoAuthority false .\n  <{}> sdoc:definedAt <{}> .\n",
                span_iri,
                literal(filename),
                begin.0,
                begin.1,
                end.0,
                end.1,
                iri,
                span_iri
            ));
        }

        if let Some(links) = item.get("links").and_then(Value::as_object) {
            let mut labels: Vec<&String> = links.keys().collect();
            labels.sort();
            for label in labels {
                let target = scalar_string(&links[label]);
                if target.is_empty() {
                    continue;
                }
                counts.reference_count += 1;
                source.push_str(&format!(
                    "  <{}> sdoc:references <{}> .\n",
                    iri,
                    item_iri(revision, &target)
                ));
                let edge = format!("{}/link/{}", iri, iri_segment(label));
                source.push_str(&format!(
                    "  <{}> a sdoc:DocumentationReference, prov:Entity ; sdoc:from <{}> ; sdoc:to <{}> ; sdoc:linkLabel {} ; sdoc:grantsDoAuthority false .\n",
                    edge,
                    iri,
                    item_iri(revision, &target),
                    literal(label)
                ));
            }
        }

        if let Some(docstring) = item.get("docs").and_then(Value::as_str) {
            counts.documented_item_count += 1;
            let doc_iri = format!("{}/doc", iri);
            docs.push_str(&format!(
                "  <{}> a sdoc:DocFragment, sdoc:DocumentationEvidence, prov:Entity ;\n    sdoc:documents <{}> ;\n    sdoc:documentationKind sdoc:RustdocMarkdown ;\n    rdf:value {} ;\n    prov:wasDerivedFrom <{}> ;\n    sdoc:grantsDoAuthority false .\n  <{}> sdoc:hasEvidence <{}> .\n",
                doc_iri,
                iri,
                literal(docstring),
                iri,
                dataset_iri,
                doc_iri
            ));
        }
    }

    let trig = format!(
        "@prefix sdoc: <{}> .\n@prefix prov: <http://www.w3.org/ns/prov#> .\n@prefix dct: <http://purl.org/dc/terms/> .\n@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n<{}> {{\n{}}}\n\n<{}> {{\n{}}}\n",
        SDOC, source_graph, source, docs_graph, docs
    );

    Ok((trig, format_version, counts))
}

fn item_iri(revision: &str, id: &str) -> String {
    format!(
        "urn:litho:item:{}:{}",
        iri_segment(revision),
        iri_segment(id)
    )
}

fn item_kind(item: &serde_json::Map<String, Value>) -> &str {
    item.get("inner")
        .and_then(Value::as_object)
        .and_then(|inner| inner.keys().next())
        .map(String::as_str)
        .unwrap_or("unknown")
}

fn coordinate(value: Option<&Value>) -> (u64, u64) {
    let Some(values) = value.and_then(Value::as_array) else {
        return (0, 0);
    };
    (
        values.first().and_then(Value::as_u64).unwrap_or(0),
        values.get(1).and_then(Value::as_u64).unwrap_or(0),
    )
}

fn scalar_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(v) => v.to_string(),
        _ => String::new(),
    }
}

fn canonical_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "unknown".to_string()),
    }
}

fn literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => escaped.push(' '),
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    escaped
}

fn iri_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    if out.is_empty() {
        "UNKNOWN".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "root": 1,
            "format_version": 61,
            "includes_private": false,
            "index": {
                "2": {
                    "name": "run",
                    "visibility": "public",
                    "docs": "Run \"semantic\" docs.\nNo DO authority.",
                    "span": {"filename": "src/lib.rs", "begin": [8, 1], "end": [11, 2]},
                    "inner": {"function": {}},
                    "links": {}
                },
                "1": {
                    "name": "demo",
                    "visibility": "public",
                    "docs": "Crate docs",
                    "span": {"filename": "src/lib.rs", "begin": [1, 1], "end": [3, 1]},
                    "inner": {"module": {}},
                    "links": {"run": 2}
                }
            },
            "paths": {
                "1": {"path": ["demo"], "kind": "module", "crate_id": 0},
                "2": {"path": ["demo", "run"], "kind": "function", "crate_id": 0}
            }
        })
    }

    #[test]
    fn semantic_graph_is_deterministic_and_preserves_evidence() {
        let (a, version_a, counts_a) = compile_rustdoc_value(&fixture(), "acme/demo", "abc123")
            .expect("fixture should compile");
        let (b, version_b, counts_b) = compile_rustdoc_value(&fixture(), "acme/demo", "abc123")
            .expect("fixture should compile twice");

        assert_eq!(a, b);
        assert_eq!(version_a, 61);
        assert_eq!(version_a, version_b);
        assert_eq!(counts_a, counts_b);
        assert_eq!(counts_a.item_count, 2);
        assert_eq!(counts_a.documented_item_count, 2);
        assert_eq!(counts_a.reference_count, 1);
        assert_eq!(counts_a.source_span_count, 2);
        assert!(a.contains("@prefix sdoc: <https://ggen.dev/ontology/semantic-documentation#>"));
        assert!(a.contains("sdoc:SemanticDocumentationDataset"));
        assert!(a.contains("sdoc:SourceEvidence"));
        assert!(a.contains("sdoc:DocumentationEvidence"));
        assert!(a.contains("demo::run"));
        assert!(a.contains("Run \\\"semantic\\\" docs.\\nNo DO authority."));
        assert!(a.contains("sdoc:startLine 8"));
        assert!(a.contains("sdoc:references <urn:litho:item:abc123:2>"));
        assert!(a.contains("sdoc:grantsDoAuthority false"));
        assert!(!a.contains("prov:wasGeneratedBy"));
    }

    #[test]
    fn malformed_rustdoc_json_is_refused() {
        let err = compile_rustdoc_value(&json!({"index": {}}), "acme/demo", "abc123")
            .expect_err("missing format version must refuse");
        assert!(err.to_string().contains("format_version"));
    }

    #[test]
    fn iri_segments_are_safe_and_stable() {
        assert_eq!(iri_segment("a/b c"), "a%2Fb%20c");
        assert_eq!(iri_segment(""), "UNKNOWN");
    }
}
