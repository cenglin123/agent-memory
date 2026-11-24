//! One query vector per search request.
//!
//! Every source's semantic branch depends on the query vector alone, so the
//! request embeds once and every source reads the same result. The case below
//! stands in a helper script that records each invocation: the query must be
//! embedded exactly once however many times the state is asked for, and a
//! missing model must be reported without running anything.

mod support;

use agent_memory::recall::semantic::embed::{EmbeddingPlan, SemanticQuery, SemanticState};
use agent_memory::recall::semantic::model::{
    EmbeddingBackend, EmbeddingModelSpec, Normalization,
};
use std::collections::BTreeMap;
use std::path::Path;

fn python_exe() -> String {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            for name in ["python.exe", "python3.exe"] {
                let candidate = Path::new(dir).join(name);
                if candidate.is_file() {
                    return candidate.to_string_lossy().to_string();
                }
            }
        }
    }
    "python".to_string()
}

/// A helper that records one line per invocation and answers with a fixed
/// 4-dimension vector, following the helper's stdout contract.
fn recording_script(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("stub_embed.py");
    let source = r#"
import json, sys
open(sys.argv[1], "a", encoding="utf-8").write("call\n")
request = json.load(sys.stdin)
vectors = [[0.1, 0.2, 0.3, 0.4] for _ in request["texts"]]
print(json.dumps({"ok": True, "backend": "ollama", "vectors": vectors}))
"#;
    std::fs::write(&script, source).expect("stub script");
    script
}

fn plan_for(script: std::path::PathBuf) -> EmbeddingPlan {
    EmbeddingPlan {
        spec: EmbeddingModelSpec {
            model_id: "stub-embed".to_string(),
            dimension: 4,
            normalization: Normalization::L2,
            backend: EmbeddingBackend::Ollama,
            model_path: None,
            endpoint: None,
        },
        interpreter: python_exe(),
        interpreter_args: Vec::new(),
        interpreter_env: BTreeMap::new(),
        script: Some(script),
        ollama_url: "http://127.0.0.1:11434".to_string(),
    }
}

#[test]
fn the_query_is_embedded_once_however_many_sources_ask() {
    let temp = support::stable_tempdir();
    let script = recording_script(temp.path());
    let calls = temp.path().join("calls.txt");
    // The helper writes to argv[1]; encode it in the script rather than the
    // command line so the plan stays a plain `embed_one` call.
    let source = std::fs::read_to_string(&script).expect("stub script");
    std::fs::write(
        &script,
        source.replace("sys.argv[1]", &format!("{:?}", calls.to_string_lossy())),
    )
    .expect("stub script with counter");

    let semantic = SemanticQuery::new(Some(plan_for(script)), "温度控制曲线怎么写");
    let first = semantic.state();
    let second = semantic.state();
    match (first, second) {
        (
            SemanticState::Ready {
                vector: first_vector,
                norm: first_norm,
            },
            SemanticState::Ready {
                vector: second_vector,
                norm: second_norm,
            },
        ) => {
            assert_eq!(first_vector, second_vector, "the vector is shared");
            assert_eq!(first_norm, second_norm);
            assert_eq!(first_vector.len(), 4);
        }
        _ => panic!("expected a ready vector, got {first:?}"),
    }
    let recorded = std::fs::read_to_string(&calls).expect("counter file");
    assert_eq!(
        recorded.lines().count(),
        1,
        "the helper ran {} times, expected once",
        recorded.lines().count()
    );
}

#[test]
fn concurrent_callers_share_one_embedding() {
    let temp = support::stable_tempdir();
    let script = recording_script(temp.path());
    let calls = temp.path().join("calls.txt");
    let source = std::fs::read_to_string(&script).expect("stub script");
    std::fs::write(
        &script,
        source.replace("sys.argv[1]", &format!("{:?}", calls.to_string_lossy())),
    )
    .expect("stub script with counter");

    let semantic = SemanticQuery::new(Some(plan_for(script)), "缓存失效");
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| match semantic.state() {
                SemanticState::Ready { vector, .. } => assert_eq!(vector.len(), 4),
                other => panic!("expected a ready vector, got {other:?}"),
            });
        }
    });
    let recorded = std::fs::read_to_string(&calls).expect("counter file");
    assert_eq!(recorded.lines().count(), 1);
}

#[test]
fn no_model_is_reported_without_running_a_helper() {
    let semantic = SemanticQuery::new(None, "任意问题");
    assert!(matches!(semantic.state(), SemanticState::NotConfigured));
    assert!(semantic.plan().is_none());
}
