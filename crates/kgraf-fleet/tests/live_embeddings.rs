//! Live check of the embedding kind (decision 0089): one call to OpenAI's
//! `/v1/embeddings`, built the way a run builds it (`build_kind`, the `openai`
//! catalog route, its own credential variable), settled through the ledger.
//!
//! Ignored by default: it spends money (one input token, well under a cent).
//! The key comes from the environment, or from a `KEY=VALUE` file named by
//! `KGRAF_LIVE_ENV_FILE`. Nothing from that file is printed; it is read only
//! inside this process.
//!
//! ```text
//! KGRAF_LIVE_EMBEDDINGS=1 KGRAF_LIVE_ENV_FILE=<repo>/.env \
//!   cargo test -p kgraf-fleet --test live_embeddings -- --ignored --nocapture
//! ```

use kgraf_core::ModelKind;
use kgraf_provider::{Embedder, Ledger, Settlement};
use std::sync::Arc;

const MODEL: &str = "text-embedding-3-small";

/// `KEY=VALUE` lines of the file `KGRAF_LIVE_ENV_FILE` names, if it names one.
/// Read into memory only; no value is printed.
fn env_file() -> std::collections::BTreeMap<String, String> {
    let Ok(path) = std::env::var("KGRAF_LIVE_ENV_FILE") else {
        return Default::default();
    };
    let text = std::fs::read_to_string(&path).expect("KGRAF_LIVE_ENV_FILE is readable");
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            (k.trim().to_owned(), v.to_owned())
        })
        .collect()
}

#[tokio::test]
#[ignore = "spends money: one live embeddings call"]
async fn one_live_embedding_call_settles_through_the_ledger() {
    if std::env::var("KGRAF_LIVE_EMBEDDINGS").as_deref() != Ok("1") {
        eprintln!("skipped: set KGRAF_LIVE_EMBEDDINGS=1");
        return;
    }
    let file = env_file();
    let entry = kgraf_fleet::entry("openai").expect("openai route");
    let credential = kgraf_fleet::credential_from(entry, |name| {
        kgraf_core::legacy::resolve_env(name, |n| {
            std::env::var(n).ok().or_else(|| file.get(n).cloned())
        })
        .value
    })
    .expect("the openai route's credential is set");
    let route = kgraf_fleet::build_kind(ModelKind::EMBEDDING, "openai", &credential, MODEL)
        .expect("embedding route");
    let embedder = route
        .handle::<Arc<dyn Embedder>>(ModelKind::EMBEDDING)
        .expect("embedder");
    let ledger = Ledger::open_in_memory().expect("ledger");
    let token = ledger
        .begin(
            route.adapter(),
            route.protocol(),
            MODEL,
            "live",
            "embeddings",
        )
        .expect("begin");
    let out = embedder
        .embed(MODEL, &["kgraf".to_owned()], &token)
        .await
        .expect("a live embedding");
    let record = ledger.call(token.id()).expect("read").expect("row");
    println!("route: {}", route.name());
    println!("adapter: {}", record.adapter);
    println!("settlement: {}", record.settlement.as_str());
    println!("vectors: {}", out.vectors.len());
    println!("dimensions: {}", out.vectors[0].len());
    println!("usage (raw): {}", out.usage.raw.clone().unwrap_or_default());
    println!("request id present: {}", record.request_id.is_some());
    assert_eq!(record.settlement, Settlement::Counted);
    assert_eq!(out.vectors.len(), 1);
    assert!(!out.vectors[0].is_empty());
    assert!(out.usage.uncached_input > 0);
}
