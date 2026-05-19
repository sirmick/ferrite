//! Corpus-wide cross-reference validator — the single source of truth
//! behind both entry points (Phase 1, `docs/14-fldigi-catalog-variants.md`
//! D-V7): a `cargo test` PR gate (`runtime/tests/flowgraph_corpus.rs`)
//! and an opt-in server boot warn (`ferrited --validate-presets`).
//!
//! Per-flowgraph structural validity is the runtime's job at load time;
//! this is the layer it doesn't have — checks that only make sense
//! across the *whole* preset corpus (global slug uniqueness, dangling
//! asset refs, the variant base==default invariant).

use std::collections::BTreeMap;
use std::path::Path;

use crate::{validate_doc, Environment, FlowgraphDoc};

/// One problem found in the corpus. `slug` is the file stem (or the
/// offending pair, for cross-file collisions).
#[derive(Debug, Clone)]
pub struct CorpusFinding {
    pub slug: String,
    /// `parse` | `name` | `validate` | `asset` | `variant` | `slug`.
    pub kind: &'static str,
    pub message: String,
}

impl std::fmt::Display for CorpusFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}: {}", self.kind, self.slug, self.message)
    }
}

fn resolved_params(
    base: &serde_json::Value,
    patch: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    let mut m = base.as_object().cloned().unwrap_or_default();
    if let Some(p) = patch.as_object() {
        for (k, v) in p {
            m.insert(k.clone(), v.clone());
        }
    }
    m
}

/// Validate every `*.json` directly under `flowgraphs_dir`.
///
/// When `repo_root` is `Some`, declared `sample_path` /
/// `signal_wiki_image` must resolve to real files (a dev/PR-time
/// check — a deployed binary whose `samples/` tree isn't colocated
/// passes `None` to skip it). Returns every finding; an empty vec
/// means the corpus is clean.
#[must_use]
pub fn validate_corpus(flowgraphs_dir: &Path, repo_root: Option<&Path>) -> Vec<CorpusFinding> {
    let mut findings = Vec::new();

    let mut paths: Vec<_> = match std::fs::read_dir(flowgraphs_dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect(),
        Err(e) => {
            return vec![CorpusFinding {
                slug: flowgraphs_dir.display().to_string(),
                kind: "parse",
                message: format!("read_dir failed: {e}"),
            }]
        }
    };
    paths.sort();

    // slug -> owning file stem, for global uniqueness (D-V4).
    let mut slugs: BTreeMap<String, String> = BTreeMap::new();
    let mut claim = |slug: String, owner: &str, out: &mut Vec<CorpusFinding>| {
        if let Some(prev) = slugs.insert(slug.clone(), owner.to_string()) {
            out.push(CorpusFinding {
                slug,
                kind: "slug",
                message: format!("duplicate catalog slug — claimed by both {prev} and {owner}"),
            });
        }
    };

    for path in &paths {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                findings.push(CorpusFinding {
                    slug: stem.clone(),
                    kind: "parse",
                    message: format!("read failed: {e}"),
                });
                continue;
            }
        };
        let doc = match FlowgraphDoc::from_json(&bytes) {
            Ok(d) => d,
            Err(e) => {
                findings.push(CorpusFinding {
                    slug: stem.clone(),
                    kind: "parse",
                    message: format!("FlowgraphDoc contract: {e}"),
                });
                continue;
            }
        };

        // The addressing slug must match the filename so
        // `${name}-${variantId}` and `/api/preset` agree.
        if doc.name != stem {
            findings.push(CorpusFinding {
                slug: stem.clone(),
                kind: "name",
                message: format!("doc.name {:?} != filename slug {stem:?}", doc.name),
            });
        }

        // A catalog-visible doc (browser-runnable, not opted out) must
        // carry a `category` — it is the 2-level grouping key and has
        // no sensible default (D-V6, Phase 2). Node-only / dev /
        // canary docs are exempt.
        let catalog_visible =
            doc.environments.contains(&Environment::Browser) && doc.catalog_visible != Some(false);
        if catalog_visible && doc.category.as_deref().unwrap_or("").trim().is_empty() {
            findings.push(CorpusFinding {
                slug: stem.clone(),
                kind: "category",
                message: "catalog-visible preset has no `category` (2-level grouping key)"
                    .to_string(),
            });
        }

        if let Err(e) = validate_doc(&doc) {
            findings.push(CorpusFinding {
                slug: stem.clone(),
                kind: "validate",
                message: format!("{e:?}"),
            });
        }

        if let Some(root) = repo_root {
            for (field, val) in [
                ("sample_path", &doc.sample_path),
                ("signal_wiki_image", &doc.signal_wiki_image),
            ] {
                if let Some(rel) = val {
                    if !root.join(rel).is_file() {
                        findings.push(CorpusFinding {
                            slug: stem.clone(),
                            kind: "asset",
                            message: format!("{field} {rel:?} does not resolve to a file"),
                        });
                    }
                }
            }
        }

        // Variant invariants (D-V4/D-V5).
        if doc.variants.is_empty() {
            claim(doc.name.clone(), &stem, &mut findings);
            continue;
        }
        let mut ids: BTreeMap<String, ()> = BTreeMap::new();
        let mut default_ids = Vec::new();
        for v in &doc.variants {
            if ids.insert(v.id.clone(), ()).is_some() {
                findings.push(CorpusFinding {
                    slug: stem.clone(),
                    kind: "variant",
                    message: format!("duplicate variant id {:?}", v.id),
                });
            }
            if v.default {
                default_ids.push(v.id.clone());
            }
            claim(format!("{}-{}", doc.name, v.id), &stem, &mut findings);
        }
        if default_ids.len() != 1 {
            findings.push(CorpusFinding {
                slug: stem.clone(),
                kind: "variant",
                message: format!("exactly one variant must be default, got {default_ids:?}"),
            });
        }
        if let Some(def) = doc.variants.iter().find(|v| v.default) {
            for (block_id, patch) in &def.patch {
                let Some(block) = doc.blocks.get(block_id) else {
                    findings.push(CorpusFinding {
                        slug: stem.clone(),
                        kind: "variant",
                        message: format!(
                            "default variant {:?} patches unknown block {block_id:?}",
                            def.id
                        ),
                    });
                    continue;
                };
                let base = block.params.clone().unwrap_or(serde_json::Value::Null);
                let resolved = serde_json::Value::Object(resolved_params(&base, patch));
                let base_obj =
                    serde_json::Value::Object(base.as_object().cloned().unwrap_or_default());
                if resolved != base_obj {
                    findings.push(CorpusFinding {
                        slug: stem.clone(),
                        kind: "variant",
                        message: format!(
                            "default variant {:?} resolves block {block_id:?} to params \
                             differing from the base — base and default must not drift (D-V5)",
                            def.id
                        ),
                    });
                }
            }
        }
    }

    findings
}
