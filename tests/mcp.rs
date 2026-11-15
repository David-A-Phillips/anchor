//! The MCP server end to end: a client speaks JSON-RPC over a pipe, lists the tools, asks a
//! question, sees the shallow flag, deepens the source and asks again.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use anchor::base::SourceKind;
use anchor::config::{AnchorConfig, Depth, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::query::{DepthControl, QueryDeps, QueryEngine, ShallowPolicy};
use anchor::engine::router::ProviderKind;
use anchor::mcp::{McpServer, PROTOCOL_VERSIONS};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

fn server(dir: &std::path::Path) -> McpServer {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("CODEOWNERS"), "* @acme/payments-platform\n").unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "/// Charges a card.\npub fn charge(amount: u64) -> u64 { validate(amount) }\nfn validate(amount: u64) -> u64 { amount }\n",
    )
    .unwrap();
    let mut config = AnchorConfig {
        data_dir: Some(dir.join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    config.query.on_shallow = ShallowPolicy::Manual;
    let mut source = SourceConfig::new("payments-api", SourceKind::Repo, repo.to_string_lossy());
    source.depth = Depth::PhoneBook;
    config.sources.push(source);
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    let depths: Arc<dyn DepthControl> = engine.clone();
    let query = QueryEngine::new(QueryDeps {
        store: engine.store().clone(),
        graph: engine.graph(),
        vectors: None,
        embedder: None,
        router: engine.router(),
        backlog: Some(engine.backlog()),
        graph_config: engine.config().graph.clone(),
        query_config: engine.config().query.clone(),
    })
    .with_depths(depths);
    McpServer::new(engine, query)
}

struct Client {
    writer: DuplexStream,
    reader: tokio::io::Lines<BufReader<DuplexStream>>,
    next_id: u64,
}

impl Client {
    fn connect(server: McpServer) -> Self {
        let (client_writer, server_reader) = tokio::io::duplex(1 << 16);
        let (server_writer, client_reader) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            server
                .serve(BufReader::new(server_reader), server_writer)
                .await
                .unwrap();
        });
        Self {
            writer: client_writer,
            reader: BufReader::new(client_reader).lines(),
            next_id: 0,
        }
    }

    async fn send(&mut self, message: &Value) {
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        let line = self.reader.next_line().await.unwrap().unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        response
    }

    /// Calls a tool and parses the JSON it returns as text.
    async fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        let result = &response["result"];
        assert_eq!(result["isError"], false, "{response}");
        assert_eq!(result["content"][0]["type"], "text");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_can_ask_see_shallow_areas_deepen_and_ask_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::connect(server(dir.path()));

    let init = client
        .request(
            "initialize",
            json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "anchor");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    // A notification gets no response: the next line read belongs to the next request.
    client
        .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;

    let listed = client.request("tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["ask", "search", "get", "path", "deepen", "status"]);
    for tool in listed["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(!tool["description"].as_str().unwrap().is_empty());
    }

    let owner = client
        .tool(
            "ask",
            json!({ "question": "Who owns the payments-api service?" }),
        )
        .await;
    assert!(
        owner["answer"]
            .as_str()
            .unwrap()
            .contains("payments-platform")
    );
    assert_eq!(owner["evidence"][0]["role"], "answer");
    assert_eq!(owner["evidence"][0]["trust"], "source_of_truth");
    assert_eq!(owner["shallow"][0]["source"], "payments-api");
    assert_eq!(owner["shallow"][0]["depth"], "phone_book");

    let missing = client
        .tool("search", json!({ "query": "charge", "kind": "symbol" }))
        .await;
    assert_eq!(missing["results"].as_array().unwrap().len(), 0);

    let deepened = client
        .tool("deepen", json!({ "source": "payments-api" }))
        .await;
    assert_eq!(deepened["depth"], "full");

    let found = client
        .tool("search", json!({ "query": "charge", "kind": "symbol" }))
        .await;
    let key = found["results"][0]["key"].as_str().unwrap().to_owned();
    assert!(key.contains("charge"), "{found}");
    let again = client
        .tool(
            "ask",
            json!({ "question": "Who owns the payments-api service?" }),
        )
        .await;
    assert_eq!(again["shallow"].as_array().unwrap().len(), 0);

    let detail = client
        .tool("get", json!({ "entity": "payments-api" }))
        .await;
    assert_eq!(detail["kind"], "service");
    assert!(
        detail["relationships"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["relation"] == "owns" && r["entity"]["kind"] == "team")
    );

    let path = client
        .tool("path", json!({ "from": key, "to": "payments-platform" }))
        .await;
    assert_eq!(path["found"], true, "{path}");
    assert_eq!(path["reference_only_elements"], 0);
    assert!(!path["steps"].as_array().unwrap().is_empty());

    let status = client.tool("status", json!({})).await;
    assert_eq!(status["sources"][0]["id"], "payments-api");
    assert_eq!(status["sources"][0]["depth"], "full");
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_errors_are_reported_without_breaking_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(dir.path());

    // An unknown protocol revision is answered with the newest one the server speaks.
    let init = server
        .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "1999-01-01" } }))
        .await
        .unwrap();
    assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);

    let garbage = server.handle_line("{not json").await.unwrap();
    assert!(garbage.contains("-32700"));

    let unknown = server
        .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }))
        .await
        .unwrap();
    assert_eq!(unknown["error"]["code"], -32601);

    let no_tool = server
        .handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "nope" } }))
        .await
        .unwrap();
    assert_eq!(no_tool["error"]["code"], -32602);

    // A tool that fails is a successful JSON-RPC response carrying a tool error.
    let failed = server
        .handle(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "get", "arguments": { "entity": "no-such-thing-anywhere" } } }))
        .await
        .unwrap();
    assert_eq!(failed["result"]["isError"], true);
    let missing = server
        .handle(&json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "ask", "arguments": {} } }))
        .await
        .unwrap();
    assert_eq!(missing["result"]["isError"], true);

    assert!(
        server
            .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" }))
            .await
            .is_none()
    );
    let batch = server
        .handle_line(r#"[{"jsonrpc":"2.0","id":6,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#)
        .await
        .unwrap();
    let batch: Value = serde_json::from_str(&batch).unwrap();
    assert_eq!(batch.as_array().unwrap().len(), 1);
    assert_eq!(batch[0]["id"], 6);
}
