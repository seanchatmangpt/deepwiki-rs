use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SDOC: &str = "https://ggen.dev/ontology/semantic-documentation#";
const RECEIPT_SCHEMA: &str = "litho.semantic-documentation.receipt/v2";

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
    pub input_sha256: String,
    pub output: String,
    pub output_sha256: String,
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
    pub argv: Vec<String>,
    pub requested: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub reported_ok: Option<bool>,
    pub reported_triples: Option<u64>,
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
    let revision = options
        .revision
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .filter(|value| !value.trim().is_empty())
        .context(
            "semantic documentation requires an exact revision via --revision or GITHUB_SHA",
        )?;
    let revision = exact_revision(&revision)?;
    let repository = options
        .repository
        .or_else(|| std::env::var("GITHUB_REPOSITORY").ok())
        .filter(|value| !value.trim().is_empty())
        .context("semantic documentation requires a repository identity via --repository or GITHUB_REPOSITORY")?;
    let repository = repository_identity(&repository)?;

    let receipt_path = options
        .receipt
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}.receipt.json", options.output.display())));
    refuse_aliasing(&options.rustdoc_json, &options.output, &receipt_path)?;

    let raw = fs::read(&options.rustdoc_json).with_context(|| {
        format!(
            "failed to read Rustdoc JSON at {}",
            options.rustdoc_json.display()
        )
    })?;
    let input_sha256 = sha256_hex(&raw);
    let value: Value = serde_json::from_slice(&raw)
        .with_context(|| format!("invalid Rustdoc JSON at {}", options.rustdoc_json.display()))?;

    let (trig, format_version, counts) = compile_rustdoc_value(&value, &repository, &revision)?;
    if let Some(parent) = options.output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(&options.output, trig.as_bytes()).with_context(|| {
        format!(
            "failed to write semantic graph to {}",
            options.output.display()
        )
    })?;
    let output_sha256 = sha256_hex(trig.as_bytes());

    let validation = if let Some(bin) = &options.open_ontologies_bin {
        validate_with_open_ontologies(bin, &options.output)
    } else {
        ValidationReceipt {
            validator: "open-ontologies".to_string(),
            argv: Vec::new(),
            requested: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            reported_ok: None,
            reported_triples: None,
            validated: false,
        }
    };

    // `open-ontologies validate` validates the RDF/ontology document. SHACL is
    // a separate Open Ontologies boundary (`shacl <shapesfile>` / onto_shacl),
    // so successful validation must not be upgraded to semantic admission or
    // ALIVE standing here.
    let receipt = SemanticDocumentationReceipt {
        schema_version: RECEIPT_SCHEMA,
        extractor: "rustdoc-json",
        rustdoc_format_version: format_version,
        repository,
        revision,
        input: options.rustdoc_json.display().to_string(),
        input_sha256,
        output: options.output.display().to_string(),
        output_sha256,
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
            "revision is not a full 40- or 64-hex commit id (symbolic refs are stale subjects)",
            "output or receipt path aliases the input or each other",
            "requested Open Ontologies validation exits non-zero, cannot start, or reports ok != true",
            "same input/repository/revision produces different TriG bytes",
            "replay: input or output sha256 differs from this receipt",
            "emitted semantic subject diverges from semantic-documentation-contract vocabulary",
        ],
    };

    if let Some(parent) = receipt_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)
        .with_context(|| format!("failed to write receipt to {}", receipt_path.display()))?;

    if receipt.validation.requested && !receipt.validation.validated {
        bail!(
            "Open Ontologies validation refused semantic documentation graph (exit {:?}, reported ok {:?}); receipt preserved at {}",
            receipt.validation.exit_code,
            receipt.validation.reported_ok,
            receipt_path.display()
        );
    }

    Ok(receipt)
}

/// Runs `open-ontologies ontology validate --input <graph>` (the verb shape of
/// open-ontologies `src/cmds/ontology.rs`). That verb exits 0 even when the
/// graph does not parse and reports the verdict as JSON `{"ok":bool,"triples":n}`
/// on stdout, so the exit code alone is a vacuous gate: validation holds only
/// when the process exits 0 AND reports `ok == true` with a non-zero triple
/// count. A validator that cannot be started is recorded as a refusal, never
/// as a missing receipt.
fn validate_with_open_ontologies(bin: &Path, graph: &Path) -> ValidationReceipt {
    let argv = vec![
        "ontology".to_string(),
        "validate".to_string(),
        "--input".to_string(),
        graph.display().to_string(),
    ];
    match Command::new(bin).args(&argv).output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let (reported_ok, reported_triples) = parse_validator_report(&stdout);
            let validated = output.status.success()
                && reported_ok == Some(true)
                && reported_triples.is_some_and(|n| n > 0);
            ValidationReceipt {
                validator: bin.display().to_string(),
                argv,
                requested: true,
                exit_code: output.status.code(),
                stdout,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                reported_ok,
                reported_triples,
                validated,
            }
        }
        Err(err) => ValidationReceipt {
            validator: bin.display().to_string(),
            argv,
            requested: true,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("failed to execute Open Ontologies binary: {err}"),
            reported_ok: None,
            reported_triples: None,
            validated: false,
        },
    }
}

/// Extracts `ok` and `triples` from the last JSON object line on stdout.
fn parse_validator_report(stdout: &str) -> (Option<bool>, Option<u64>) {
    stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(Value::is_object)
        .map(|report| {
            (
                report.get("ok").and_then(Value::as_bool),
                report.get("triples").and_then(Value::as_u64),
            )
        })
        .unwrap_or((None, None))
}

/// A revision must be a full commit object id (SHA-1 40 hex or SHA-256 64 hex).
/// Symbolic refs (`main`, `HEAD`, tags) and abbreviations move or collide, so a
/// graph bound to them is a stale subject and is refused.
fn exact_revision(revision: &str) -> Result<String> {
    let trimmed = revision.trim();
    let is_hex = !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex || !(trimmed.len() == 40 || trimmed.len() == 64) {
        bail!(
            "semantic documentation requires an exact revision (full 40- or 64-hex commit id); refused {:?}",
            revision
        );
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Repository identity must be a single printable token (e.g. `owner/repo`).
fn repository_identity(repository: &str) -> Result<String> {
    let trimmed = repository.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!(
            "semantic documentation requires a repository identity without whitespace or control characters; refused {:?}",
            repository
        );
    }
    Ok(trimmed.to_string())
}

/// Refuses writes that would destroy the input observation or overwrite the
/// graph with its own receipt.
fn refuse_aliasing(input: &Path, output: &Path, receipt: &Path) -> Result<()> {
    let input_id = resolved(input);
    let output_id = resolved(output);
    let receipt_id = resolved(receipt);
    if output_id == input_id {
        bail!(
            "refused: --output {} aliases the Rustdoc JSON input",
            output.display()
        );
    }
    if receipt_id == input_id {
        bail!(
            "refused: --receipt {} aliases the Rustdoc JSON input",
            receipt.display()
        );
    }
    if receipt_id == output_id {
        bail!(
            "refused: --receipt {} aliases the --output graph",
            receipt.display()
        );
    }
    Ok(())
}

/// Resolves a path that may not exist yet: canonicalize the deepest existing
/// ancestor and re-append the remaining components.
fn resolved(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut existing = absolute.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => return absolute,
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

/// Outcome of re-executing a receipt against the files it names.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// Input digest, on-disk output digest and recompiled output digest all match.
    Replayed { output_sha256: String },
}

/// Replays a v2 receipt: the input must still hash to `input_sha256`, the
/// graph on disk must still hash to `output_sha256`, and recompiling the input
/// under the receipt's repository/revision must reproduce `output_sha256`
/// byte-for-byte. Any divergence is a typed refusal (error).
pub fn replay(receipt_path: &Path) -> Result<ReplayVerdict> {
    let receipt: Value = serde_json::from_slice(
        &fs::read(receipt_path)
            .with_context(|| format!("failed to read receipt {}", receipt_path.display()))?,
    )
    .with_context(|| {
        format!(
            "REPLAY_REFUSED[RECEIPT_MALFORMED]: {}",
            receipt_path.display()
        )
    })?;
    let field = |name: &str| -> Result<String> {
        receipt
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| {
                format!("REPLAY_REFUSED[RECEIPT_MALFORMED]: missing string field {name}")
            })
    };
    let schema = field("schema_version")?;
    if schema != RECEIPT_SCHEMA {
        bail!("REPLAY_REFUSED[RECEIPT_SCHEMA]: expected {RECEIPT_SCHEMA}, found {schema}");
    }
    let input = PathBuf::from(field("input")?);
    let output = PathBuf::from(field("output")?);
    let repository = repository_identity(&field("repository")?)?;
    let revision = exact_revision(&field("revision")?)?;
    let expected_input = field("input_sha256")?;
    let expected_output = field("output_sha256")?;

    let raw = fs::read(&input)
        .with_context(|| format!("REPLAY_REFUSED[INPUT_MISSING]: {}", input.display()))?;
    let actual_input = sha256_hex(&raw);
    if actual_input != expected_input {
        bail!(
            "REPLAY_REFUSED[INPUT_DIGEST_MISMATCH]: receipt {expected_input}, disk {actual_input}"
        );
    }
    let on_disk = fs::read(&output)
        .with_context(|| format!("REPLAY_REFUSED[OUTPUT_MISSING]: {}", output.display()))?;
    let actual_output = sha256_hex(&on_disk);
    if actual_output != expected_output {
        bail!(
            "REPLAY_REFUSED[OUTPUT_DIGEST_MISMATCH]: receipt {expected_output}, disk {actual_output}"
        );
    }
    let value: Value = serde_json::from_slice(&raw)
        .with_context(|| format!("REPLAY_REFUSED[INPUT_MALFORMED]: {}", input.display()))?;
    let (trig, _, _) = compile_rustdoc_value(&value, &repository, &revision)?;
    let recompiled = sha256_hex(trig.as_bytes());
    if recompiled != expected_output {
        bail!(
            "REPLAY_REFUSED[REPLAY_MISMATCH]: receipt {expected_output}, recompiled {recompiled}"
        );
    }
    Ok(ReplayVerdict::Replayed {
        output_sha256: recompiled,
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

    docs.push_str(
        "  sdoc:RustdocMarkdown a sdoc:DocumentationKind ; sdoc:grantsDoAuthority false .\n",
    );

    let mut ids: Vec<&String> = index.keys().collect();
    ids.sort();

    for id in ids {
        let Some(item) = index.get(id).and_then(Value::as_object) else {
            bail!(
                "Rustdoc JSON index entry {:?} is not an object; refusing a partial graph",
                id
            );
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
            source.push_str(&format!(
                "    sdoc:fullyQualifiedPath {} ;\n",
                literal(&path)
            ));
        }
        source.push_str("    sdoc:grantsDoAuthority false .\n");
        source.push_str(&format!(
            "  <{}> sdoc:hasEvidence <{}> .\n",
            dataset_iri, iri
        ));

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
            // Turtle UCHAR keeps every other control character lossless.
            c if c.is_control() => escaped.push_str(&format!("\\u{:04X}", c as u32)),
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

/// Boundary, negative and adversarial falsifiers for the semantic-docs command.
/// Chicago style: real files, real process execution, real Rustdoc output
/// (tests/fixtures/semantic, produced by regen.sh), no test doubles. Validator
/// cases use tiny real executables that implement the Open Ontologies report
/// contract; the installed Open Ontologies binary is exercised when present.
#[cfg(test)]
mod hardening {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    const REV: &str = "fc0f287ad5b1313965de2df64771af543dcb8a85";
    const REV2: &str = "27dbd3ed74535213e7c40be8abcdcec4145bebf2";

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("litho-semantic-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/semantic/tiny_semantic.rustdoc.json")
    }

    fn options(input: &Path, output: &Path) -> SemanticDocsOptions {
        SemanticDocsOptions {
            rustdoc_json: input.to_path_buf(),
            output: output.to_path_buf(),
            receipt: None,
            repository: Some("seanchatmangpt/deepwiki-rs".to_string()),
            revision: Some(REV.to_string()),
            open_ontologies_bin: None,
        }
    }

    fn write_input(dir: &Path) -> PathBuf {
        let input = dir.join("in.json");
        fs::copy(fixture_path(), &input).expect("copy fixture");
        input
    }

    fn receipt_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).expect("receipt")).expect("receipt json")
    }

    fn compile(value: &Value) -> String {
        compile_rustdoc_value(value, "acme/demo", REV)
            .expect("compile")
            .0
    }

    // ---- malformed input -------------------------------------------------

    #[test]
    fn control_characters_are_escaped_losslessly() {
        let a = compile(
            &json!({"format_version": 61, "index": {"1": {"docs": "a\u{1}b\u{7f}", "inner": {"module": {}}}}}),
        );
        let b = compile(
            &json!({"format_version": 61, "index": {"1": {"docs": "a b ", "inner": {"module": {}}}}}),
        );
        assert_ne!(a, b, "distinct docstrings must not collide");
        assert!(a.contains(r#"rdf:value "a\u0001b\u007F""#), "{a}");
        assert!(!a.chars().any(|c| c.is_control() && c != '\n'));
    }

    #[test]
    fn non_object_index_entry_is_refused_not_skipped() {
        let err = compile_rustdoc_value(
            &json!({"format_version": 61, "index": {"1": {"inner": {"module": {}}}, "2": "garbage"}}),
            "acme/demo",
            REV,
        )
        .expect_err("partial graph must be refused");
        assert!(err.to_string().contains("\"2\""), "{err}");
    }

    #[test]
    fn missing_index_and_non_numeric_format_version_are_refused() {
        for value in [
            json!({"format_version": 61}),
            json!({"format_version": "61", "index": {}}),
            json!({"format_version": 61, "index": []}),
            json!([]),
        ] {
            assert!(
                compile_rustdoc_value(&value, "acme/demo", REV).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn literal_and_iri_injection_stay_inside_their_terms() {
        let hostile_doc = "x\" . <urn:evil> sdoc:grantsDoAuthority true . \"";
        let trig = compile(&json!({
            "format_version": 61,
            "index": {"a> <b": {"name": "n", "docs": hostile_doc, "inner": {"module": {}},
                                "links": {"l> <x": "a> <b"}}}
        }));
        let iri_token = regex::Regex::new(r"<([^>]*)>").unwrap();
        for line in trig
            .lines()
            .filter(|l| !l.trim_start().starts_with("rdf:value"))
        {
            for cap in iri_token.captures_iter(line) {
                let iri = &cap[1];
                assert!(
                    !iri.contains(' ') && !iri.contains('"') && !iri.contains('<'),
                    "unsafe IRI {iri:?} in {line}"
                );
            }
        }
        let value_line = trig.lines().find(|l| l.contains("rdf:value")).unwrap();
        assert!(
            value_line.contains(r#"x\" . <urn:evil> sdoc:grantsDoAuthority true . \""#),
            "{value_line}"
        );
        assert!(
            !trig
                .lines()
                .any(|l| l.trim_start().starts_with("<urn:evil>"))
        );
    }

    // ---- reordering / duplicate delivery ---------------------------------

    #[test]
    fn json_key_reordering_does_not_change_graph_bytes() {
        let a: Value = serde_json::from_str(r#"{"format_version":61,"paths":{"1":{"path":["c","x"]}},"index":{"2":{"name":"y","inner":{"function":{}}},"1":{"name":"x","visibility":{"restricted":{"parent":0,"path":"crate"}},"links":{"b":"2","a":"2"},"inner":{"module":{}}}}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"index":{"1":{"inner":{"module":{}},"links":{"a":"2","b":"2"},"visibility":{"restricted":{"path":"crate","parent":0}},"name":"x"},"2":{"inner":{"function":{}},"name":"y"}},"paths":{"1":{"path":["c","x"]}},"format_version":61}"#).unwrap();
        assert_eq!(compile(&a), compile(&b));
    }

    #[test]
    fn duplicate_delivery_is_byte_identical_graph_and_receipt() {
        let dir = scratch("dup");
        let input = write_input(&dir);
        let output = dir.join("g.trig");
        run(options(&input, &output)).expect("first");
        let graph1 = fs::read(&output).unwrap();
        let receipt1 = fs::read(dir.join("g.trig.receipt.json")).unwrap();
        run(options(&input, &output)).expect("second");
        assert_eq!(graph1, fs::read(&output).unwrap());
        assert_eq!(receipt1, fs::read(dir.join("g.trig.receipt.json")).unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    // ---- stale subject ----------------------------------------------------

    #[test]
    fn revision_must_be_a_full_commit_id() {
        for bad in [
            "main",
            "HEAD",
            "v26.9.25",
            "abc123",
            &REV[..39],
            &format!("{REV}0"),
            &format!("{}g", &REV[..39]),
            "   ",
        ] {
            assert!(exact_revision(bad).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(
            exact_revision(&format!("  {}  ", REV.to_uppercase())).unwrap(),
            REV
        );
        let sha256_id = "a".repeat(64);
        assert_eq!(exact_revision(&sha256_id).unwrap(), sha256_id);
    }

    #[test]
    fn symbolic_revision_is_refused_before_any_write() {
        let dir = scratch("stale");
        let input = write_input(&dir);
        let output = dir.join("g.trig");
        let mut opts = options(&input, &output);
        opts.revision = Some("main".to_string());
        let err = run(opts).expect_err("symbolic ref is a stale subject");
        assert!(err.to_string().contains("exact revision"), "{err}");
        assert!(!output.exists() && !dir.join("g.trig.receipt.json").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn repository_identity_rejects_whitespace_and_control() {
        for bad in ["owner/ repo", "owner/re\npo", "own\u{0}er/repo"] {
            assert!(repository_identity(bad).is_err(), "{bad:?}");
        }
        assert_eq!(repository_identity(" owner/repo ").unwrap(), "owner/repo");
    }

    // ---- destroyed observation (path aliasing) ----------------------------

    #[test]
    fn output_aliasing_input_is_refused_and_input_preserved() {
        let dir = scratch("alias");
        let input = write_input(&dir);
        let before = fs::read(&input).unwrap();
        let aliased = dir.join(".").join("in.json");
        let err = run(options(&input, &aliased)).expect_err("output == input");
        assert!(
            err.to_string().contains("aliases the Rustdoc JSON input"),
            "{err}"
        );
        assert_eq!(
            before,
            fs::read(&input).unwrap(),
            "input observation destroyed"
        );

        let mut opts = options(&input, &dir.join("g.trig"));
        opts.receipt = Some(dir.join("g.trig"));
        let err = run(opts).expect_err("receipt == output");
        assert!(
            err.to_string().contains("aliases the --output graph"),
            "{err}"
        );

        let mut opts = options(&input, &dir.join("g.trig"));
        opts.receipt = Some(input.clone());
        assert!(run(opts).is_err(), "receipt == input");
        assert_eq!(before, fs::read(&input).unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    // ---- digests + replay -------------------------------------------------

    #[test]
    fn receipt_binds_input_and_output_digests() {
        let dir = scratch("digest");
        let input = write_input(&dir);
        let output = dir.join("g.trig");
        let receipt = run(options(&input, &output)).expect("run");
        assert_eq!(receipt.schema_version, RECEIPT_SCHEMA);
        assert_eq!(receipt.input_sha256, sha256_hex(&fs::read(&input).unwrap()));
        assert_eq!(
            receipt.output_sha256,
            sha256_hex(&fs::read(&output).unwrap())
        );
        assert_eq!(receipt.revision, REV);
        assert_eq!(receipt.authority, "none");
        assert_eq!(receipt.standing, "PARTIAL_ALIVE");
        assert!(!receipt.validation.requested && !receipt.validation.validated);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn replay_accepts_untouched_and_refuses_every_tamper() {
        let dir = scratch("replay");
        let input = write_input(&dir);
        let output = dir.join("g.trig");
        let receipt_path = dir.join("g.trig.receipt.json");
        let receipt = run(options(&input, &output)).expect("run");
        assert_eq!(
            replay(&receipt_path).expect("clean replay"),
            ReplayVerdict::Replayed {
                output_sha256: receipt.output_sha256.clone()
            }
        );

        let original_receipt = fs::read(&receipt_path).unwrap();
        let original_graph = fs::read(&output).unwrap();
        let original_input = fs::read(&input).unwrap();
        let refused = |tag: &str| {
            let err = replay(&receipt_path).expect_err(tag).to_string();
            assert!(err.contains(tag), "expected {tag}, got {err}");
        };

        // wrong digest: input changed after the receipt was written
        fs::write(&input, [original_input.as_slice(), b" "].concat()).unwrap();
        refused("INPUT_DIGEST_MISMATCH");
        fs::write(&input, &original_input).unwrap();

        // wrong digest: graph edited on disk
        fs::write(&output, [original_graph.as_slice(), b"# edit\n"].concat()).unwrap();
        refused("OUTPUT_DIGEST_MISMATCH");
        fs::write(&output, &original_graph).unwrap();

        // replay mismatch: receipt rebound to another exact revision
        let mut forged = receipt_json(&receipt_path);
        forged["revision"] = json!(REV2);
        fs::write(&receipt_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        refused("REPLAY_MISMATCH");

        // replay mismatch: receipt rebound to another repository
        let mut forged: Value = serde_json::from_slice(&original_receipt).unwrap();
        forged["repository"] = json!("someone/else");
        fs::write(&receipt_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        refused("REPLAY_MISMATCH");

        // stale subject inside the receipt
        let mut forged: Value = serde_json::from_slice(&original_receipt).unwrap();
        forged["revision"] = json!("main");
        fs::write(&receipt_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        refused("exact revision");

        // schema downgrade (v1 receipts carry no digests)
        let mut forged: Value = serde_json::from_slice(&original_receipt).unwrap();
        forged["schema_version"] = json!("litho.semantic-documentation.receipt/v1");
        fs::write(&receipt_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        refused("RECEIPT_SCHEMA");

        // digest field removed
        let mut forged: Value = serde_json::from_slice(&original_receipt).unwrap();
        forged.as_object_mut().unwrap().remove("output_sha256");
        fs::write(&receipt_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        refused("RECEIPT_MALFORMED");

        fs::write(&receipt_path, b"{not json").unwrap();
        refused("RECEIPT_MALFORMED");

        fs::write(&receipt_path, &original_receipt).unwrap();
        assert!(
            replay(&receipt_path).is_ok(),
            "restored receipt must replay again"
        );
        let _ = fs::remove_dir_all(dir);
    }

    // ---- validator boundary (fail-closed, anti-vacuity) --------------------

    #[cfg(unix)]
    fn validator(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn validator_report_not_exit_code_decides() {
        let dir = scratch("validator");
        let input = write_input(&dir);
        let cases = [
            (
                "ok.sh",
                r#"[ "$1 $2 $3" = "ontology validate --input" ] || exit 9; echo '{"ok":true,"triples":42}'"#,
                true,
            ),
            (
                "zero-exit-not-ok.sh",
                r#"echo '{"ok":false,"triples":0,"error":"Parser error"}'"#,
                false,
            ),
            (
                "ok-zero-triples.sh",
                r#"echo '{"ok":true,"triples":0}'"#,
                false,
            ),
            ("not-json.sh", "echo validated", false),
            (
                "nonzero-exit.sh",
                r#"echo '{"ok":true,"triples":42}'; exit 3"#,
                false,
            ),
        ];
        for (name, body, expect) in cases {
            let output = dir.join(format!("{name}.trig"));
            let mut opts = options(&input, &output);
            opts.open_ontologies_bin = Some(validator(&dir, name, body));
            let result = run(opts);
            assert_eq!(result.is_ok(), expect, "{name}: {result:?}");
            let receipt = receipt_json(&dir.join(format!("{name}.trig.receipt.json")));
            assert_eq!(receipt["validation"]["validated"], json!(expect), "{name}");
            assert_eq!(receipt["validation"]["requested"], json!(true), "{name}");
            assert_eq!(
                receipt["validation"]["argv"][0],
                json!("ontology"),
                "{name}"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unstartable_validator_is_a_receipted_refusal() {
        let dir = scratch("nobin");
        let input = write_input(&dir);
        let output = dir.join("g.trig");
        let mut opts = options(&input, &output);
        opts.open_ontologies_bin = Some(dir.join("does-not-exist"));
        let err = run(opts).expect_err("unstartable validator must refuse");
        assert!(err.to_string().contains("refused"), "{err}");
        let receipt = receipt_json(&dir.join("g.trig.receipt.json"));
        assert_eq!(receipt["validation"]["validated"], json!(false));
        assert!(
            receipt["validation"]["stderr"]
                .as_str()
                .unwrap()
                .contains("failed to execute")
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn installed_open_ontologies() -> Option<PathBuf> {
        if let Ok(bin) = std::env::var("OPEN_ONTOLOGIES_BIN") {
            return Some(PathBuf::from(bin));
        }
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("open-ontologies"))
                .find(|candidate| candidate.is_file())
        })
    }

    #[test]
    fn installed_open_ontologies_validates_real_graph_and_refuses_corruption() {
        let Some(bin) = installed_open_ontologies() else {
            eprintln!(
                "SKIPPED(open-ontologies not on PATH and OPEN_ONTOLOGIES_BIN unset): real-validator leg not run"
            );
            return;
        };
        let dir = scratch("real-oo");
        let input = write_input(&dir);
        let output = dir.join("g.trig");
        let mut opts = options(&input, &output);
        opts.open_ontologies_bin = Some(bin.clone());
        let receipt = run(opts).expect("real validator must accept the compiled graph");
        assert!(receipt.validation.validated);
        assert!(
            receipt.validation.reported_triples.unwrap() > 100,
            "{:?}",
            receipt.validation
        );
        assert_eq!(
            receipt.standing, "PARTIAL_ALIVE",
            "validate never upgrades standing"
        );

        // Anti-vacuity against the real binary: a corrupted graph is refused.
        let corrupt = dir.join("corrupt.trig");
        fs::write(&corrupt, b"garbage <<< .\n").unwrap();
        let verdict = validate_with_open_ontologies(&bin, &corrupt);
        assert!(!verdict.validated, "{verdict:?}");
        assert_eq!(verdict.reported_ok, Some(false), "{verdict:?}");
        let _ = fs::remove_dir_all(dir);
    }

    // ---- real Rustdoc output ----------------------------------------------

    #[test]
    fn real_rustdoc_fixture_compiles_with_expected_evidence() {
        let value: Value = serde_json::from_slice(&fs::read(fixture_path()).unwrap()).unwrap();
        let index_len = value["index"].as_object().unwrap().len();
        let (trig, version, counts) =
            compile_rustdoc_value(&value, "acme/tiny", REV).expect("real rustdoc");
        assert_eq!(version, value["format_version"].as_u64().unwrap());
        assert_eq!(counts.item_count, index_len);
        let ledger_id = value["index"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, v)| v["name"] == "Ledger")
            .map(|(k, _)| k.clone())
            .unwrap();
        let admit_id = value["index"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, v)| v["name"] == "admit")
            .map(|(k, _)| k.clone())
            .unwrap();
        assert!(trig.contains("sdoc:fullyQualifiedPath \"tiny_semantic::admit\""));
        assert!(trig.contains(&format!(
            "<{}> sdoc:references <{}>",
            item_iri(REV, &admit_id),
            item_iri(REV, &ledger_id)
        )));
        assert!(trig.contains("An append-only ledger of admitted entries."));
        assert!(
            counts.documented_item_count >= 5 && counts.reference_count >= 3,
            "{counts:?}"
        );
    }

    // ---- benchmark + regression bound --------------------------------------

    fn synthetic(n: usize) -> Value {
        let mut index = serde_json::Map::new();
        let mut paths = serde_json::Map::new();
        for i in 0..n {
            let id = i.to_string();
            index.insert(id.clone(), json!({
                "name": format!("item_{i}"),
                "visibility": "public",
                "docs": format!("Item {i} with a link to [`item_{}`] and \"quotes\".\nSecond line.", (i + 1) % n),
                "span": {"filename": format!("src/m{}.rs", i % 50), "begin": [i + 1, 1], "end": [i + 9, 2]},
                "inner": {"function": {}},
                "links": {format!("`item_{}`", (i + 1) % n): (i + 1) % n, "`Self`": i}
            }));
            paths.insert(
                id,
                json!({"path": ["bench", format!("item_{i}")], "kind": "function"}),
            );
        }
        json!({"root": 0, "format_version": 61, "index": index, "paths": paths})
    }

    /// Regression bound for the unoptimized test profile the CI workflow uses:
    /// 5,000 fully-populated items (docs, span, 2 links, path) must compile in
    /// under 2.5 s (measured 218-496 ms across 5 loaded runs on an M-series
    /// host; see tests/fixtures/semantic/BENCH.md). Output size scales linearly.
    #[test]
    fn compile_throughput_regression_bound() {
        let small = synthetic(1_000);
        let large = synthetic(5_000);
        let start = Instant::now();
        let (trig, _, counts) = compile_rustdoc_value(&large, "acme/bench", REV).unwrap();
        let elapsed = start.elapsed();
        let (small_trig, _, _) = compile_rustdoc_value(&small, "acme/bench", REV).unwrap();
        assert_eq!(counts.item_count, 5_000);
        assert_eq!(counts.reference_count, 10_000);
        assert_eq!(counts.source_span_count, 5_000);
        let ratio = trig.len() as f64 / small_trig.len() as f64;
        assert!(
            (4.5..5.5).contains(&ratio),
            "output growth not linear: {ratio}"
        );
        eprintln!(
            "BENCH semantic compile: items=5000 bytes={} elapsed_ms={:.2} items_per_s={:.0}",
            trig.len(),
            elapsed.as_secs_f64() * 1e3,
            5_000.0 / elapsed.as_secs_f64()
        );
        assert!(
            elapsed.as_secs_f64() < 2.5,
            "regression: 5000 items took {elapsed:?}"
        );
    }
}
