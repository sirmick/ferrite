//! PR-gate entry point for the corpus validator (Phase 1 of
//! `docs/14-fldigi-catalog-variants.md`, D-V7). The check logic lives
//! in `ferrite_runtime::validate_corpus` so this gate and the opt-in
//! server boot warn (`ferrited --validate-presets`) cannot drift.

use std::path::Path;

use ferrite_runtime::validate_corpus;

#[test]
fn shipped_flowgraph_corpus_is_clean() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("runtime crate has a parent (repo root)");
    let flowgraphs = repo_root.join("flowgraphs");

    let findings = validate_corpus(&flowgraphs, Some(repo_root));
    assert!(
        findings.is_empty(),
        "flowgraph corpus has {} cross-reference problem(s):\n{}",
        findings.len(),
        findings
            .iter()
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
