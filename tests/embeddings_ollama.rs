//! An engine configured for Ollama embeddings uses them: the model's vector length is learned at
//! open, instead of the engine silently falling back to hash vectors.
#![allow(clippy::unwrap_used)]

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use anchor::config::AnchorConfig;
use anchor::engine::Engine;
use anchor::engine::embed::EmbeddingBackend;
use anchor::engine::router::ProviderKind;

/// A fake `/api/embed` answering 8-dimensional vectors.
async fn fake_embedder() -> String {
    let app = Router::new().route(
        "/api/embed",
        post(|Json(body): Json<Value>| async move {
            let inputs = body["input"].as_array().map_or(1, Vec::len);
            let vectors: Vec<Vec<f32>> = (0..inputs)
                .map(|i| (0..8).map(|d| (i + d + 1) as f32).collect())
                .collect();
            Json(json!({ "model": body["model"], "embeddings": vectors }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn config(dir: &std::path::Path, base_url: &str) -> AnchorConfig {
    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    config.embeddings.backend = EmbeddingBackend::Ollama;
    config.ollama.base_url = base_url.to_owned();
    config
}

// Multi-threaded: the fake server must keep answering while `Engine::open` waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ollama_embedding_model_is_used_not_replaced_by_hash_vectors() {
    let url = fake_embedder().await;
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(config(dir.path(), &url)).unwrap();
    assert_eq!(engine.embedder().model_id(), "ollama:nomic-embed-text");
    assert_eq!(engine.embedder().dimensions(), 8);
    assert_eq!(engine.vectors().model_id(), "ollama:nomic-embed-text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_embedding_server_falls_back_to_hash_vectors() {
    // Bind and drop a listener to get a port nothing answers on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(config(dir.path(), &format!("http://127.0.0.1:{port}"))).unwrap();
    assert!(engine.embedder().model_id().starts_with("hash"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_to_an_embedding_model_re_embeds_existing_text() {
    let dir = tempfile::tempdir().unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    for i in 0..5 {
        std::fs::write(
            wiki.join(format!("p{i}.md")),
            format!("# Page {i}\n\nText {i}.\n"),
        )
        .unwrap();
    }
    let with_wiki = |mut config: AnchorConfig| {
        config.sources.push(anchor::config::SourceConfig::new(
            "wiki",
            anchor::base::SourceKind::Wiki,
            wiki.to_string_lossy(),
        ));
        config
    };

    // Indexed with hash vectors first.
    let mut hashed = with_wiki(config(dir.path(), "http://127.0.0.1:9"));
    hashed.embeddings.backend = EmbeddingBackend::Hash;
    let engine = Engine::open(hashed).unwrap();
    engine.scan_all().unwrap();
    let chunks = engine.embed_pending().await.unwrap();
    assert!(chunks >= 5);
    engine.flush().unwrap();
    drop(engine);

    // Reopened with a served model: nothing changed on disk, yet every chunk is embedded again.
    let url = fake_embedder().await;
    let engine = Engine::open(with_wiki(config(dir.path(), &url))).unwrap();
    assert_eq!(
        engine.vectors().len(),
        0,
        "no vectors under the new model yet"
    );
    assert_eq!(engine.embed_pending().await.unwrap(), chunks);
    assert_eq!(engine.vectors().len(), chunks);
    assert_eq!(engine.embed_pending().await.unwrap(), 0, "only once");
}
