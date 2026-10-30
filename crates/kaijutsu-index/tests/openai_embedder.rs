use std::time::Duration;

use kaijutsu_index::{Embedder, EmbeddingProfile, EmbeddingPurpose, OpenAiEmbedder, OpenAiEmbedderConfig};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Request {
    line: String,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

fn reply(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}")
}

fn ok(vectors: &[Vec<f64>]) -> String {
    let data: Vec<_> = vectors.iter().enumerate()
        .map(|(index, embedding)| serde_json::json!({"object": "embedding", "index": index, "embedding": embedding}))
        .collect();
    reply("200 OK", &serde_json::json!({"object": "list", "data": data, "model": "test-model"}).to_string())
}

async fn mock_service(replies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<Request>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut header = String::new();
                stream.read_line(&mut header).await.unwrap();
                if header == "\r\n" { break; }
                if let Some((key, value)) = header.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") { length = value.trim().parse().unwrap(); }
                    headers.push((key.trim().to_owned(), value.trim().to_owned()));
                }
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            let body = if body.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&body).unwrap() };
            requests.push(Request { line, headers, body });
            stream.write_all(reply.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        requests
    });
    (base_url, task)
}

fn config(base_url: &str, dimensions: usize, profile: EmbeddingProfile) -> OpenAiEmbedderConfig {
    OpenAiEmbedderConfig {
        base_url: base_url.to_owned(),
        api_key: Some("sk-test".to_owned()),
        model: "qwen/qwen3-embedding-8b".to_owned(),
        dimensions,
        profile,
        timeout: Duration::from_secs(2),
        max_in_flight: 2,
    }
}

/// A client whose boot probe has already been answered.
async fn client(base_url: &str, dimensions: usize, profile: EmbeddingProfile) -> OpenAiEmbedder {
    OpenAiEmbedder::connect(config(base_url, dimensions, profile)).await.unwrap()
}

fn probe_reply(dimensions: usize) -> String {
    ok(&[vec![1.0; dimensions]])
}

#[tokio::test]
async fn a_request_names_the_model_and_dimensions_and_carries_the_key() {
    let (base_url, server) = mock_service(vec![probe_reply(3), ok(&[vec![3.0, 4.0, 0.0], vec![0.0, 0.0, 2.0]])]).await;
    let client = client(&base_url, 3, EmbeddingProfile::Plain).await;
    let vectors = client.embed_batch(&["first", "second"], EmbeddingPurpose::Document).await.unwrap();
    assert_eq!(vectors, vec![vec![0.6, 0.8, 0.0], vec![0.0, 0.0, 1.0]], "vectors come back L2-normalized");

    let requests = server.await.unwrap();
    let request = &requests[1];
    assert!(request.line.starts_with("POST /api/v1/embeddings "), "{}", request.line);
    assert_eq!(request.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(request.body, serde_json::json!({
        "model": "qwen/qwen3-embedding-8b",
        "input": ["first", "second"],
        "dimensions": 3,
        "encoding_format": "float",
    }));
}

#[tokio::test]
async fn results_are_returned_in_input_order_whatever_order_the_provider_sends() {
    let shuffled = reply("200 OK", &serde_json::json!({"data": [
        {"index": 1, "embedding": [0.0, 1.0]},
        {"index": 0, "embedding": [1.0, 0.0]},
    ]}).to_string());
    let (base_url, server) = mock_service(vec![probe_reply(2), shuffled]).await;
    let client = client(&base_url, 2, EmbeddingProfile::Plain).await;
    let vectors = client.embed_batch(&["a", "b"], EmbeddingPurpose::Query).await.unwrap();
    assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    server.await.unwrap();
}

#[tokio::test]
async fn a_profile_prefixes_queries_and_documents_the_way_its_model_expects() {
    let cases = [
        (EmbeddingProfile::Plain, EmbeddingPurpose::Query, "text"),
        (EmbeddingProfile::Plain, EmbeddingPurpose::Document, "text"),
        (EmbeddingProfile::Qwen3, EmbeddingPurpose::Query,
            "Instruct: Given a search query, retrieve relevant passages that answer the query\nQuery: text"),
        (EmbeddingProfile::Qwen3, EmbeddingPurpose::Document, "text"),
        (EmbeddingProfile::EmbeddingGemma, EmbeddingPurpose::Query, "task: search result | query: text"),
        (EmbeddingProfile::EmbeddingGemma, EmbeddingPurpose::Document, "title: none | text: text"),
    ];
    for (profile, purpose, expected) in cases {
        let (base_url, server) = mock_service(vec![probe_reply(2), ok(&[vec![1.0, 0.0]])]).await;
        let client = client(&base_url, 2, profile).await;
        client.embed("text", purpose).await.unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests[1].body["input"], serde_json::json!([expected]), "{profile:?} {purpose:?}");
    }
}

#[tokio::test]
async fn the_cache_identity_changes_with_the_model_dimensions_and_profile() {
    let mut identities = std::collections::HashSet::new();
    for (model, dimensions, profile) in [
        ("qwen/qwen3-embedding-8b", 2, EmbeddingProfile::Plain),
        ("qwen/qwen3-embedding-8b", 2, EmbeddingProfile::Qwen3),
        ("qwen/qwen3-embedding-8b", 3, EmbeddingProfile::Plain),
        ("google/gemini-embedding-001", 2, EmbeddingProfile::Plain),
    ] {
        let (base_url, server) = mock_service(vec![probe_reply(dimensions)]).await;
        let mut config = config(&base_url, dimensions, profile);
        config.model = model.to_owned();
        let client = OpenAiEmbedder::connect(config).await.unwrap();
        server.await.unwrap();
        assert!(identities.insert(client.cache_identity()), "{model} {dimensions} {profile:?} repeats an identity");
    }
}

#[tokio::test]
async fn a_response_that_does_not_match_the_request_is_refused() {
    let wrong_index = reply("200 OK", r#"{"data":[{"index":1,"embedding":[1.0,0.0]}]}"#);
    let duplicate = reply("200 OK", r#"{"data":[{"index":0,"embedding":[1.0,0.0]},{"index":0,"embedding":[1.0,0.0]}]}"#);
    let no_data = reply("200 OK", r#"{"error":{"message":"no capacity"}}"#);
    let not_json = reply("200 OK", "<html>gateway</html>");
    for (name, response) in [
        ("empty data", ok(&[])),
        ("too many vectors", ok(&[vec![1.0, 0.0], vec![0.0, 1.0]])),
        ("wrong dimensions", ok(&[vec![1.0, 0.0, 0.0]])),
        ("zero vector", ok(&[vec![0.0, 0.0]])),
        ("overflowing component", reply("200 OK", r#"{"data":[{"index":0,"embedding":[1e100,0.0]}]}"#)),
        ("index out of range", wrong_index),
        ("duplicate index", duplicate),
        ("no data field", no_data),
        ("not json", not_json),
    ] {
        let (base_url, server) = mock_service(vec![probe_reply(2), response]).await;
        let client = client(&base_url, 2, EmbeddingProfile::Plain).await;
        let result = client.embed("text", EmbeddingPurpose::Document).await;
        assert!(result.is_err(), "accepted: {name}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn an_http_error_names_the_status_and_what_the_provider_said() {
    let (base_url, server) = mock_service(vec![
        probe_reply(2),
        reply("401 Unauthorized", r#"{"error":{"message":"invalid api key"}}"#),
    ]).await;
    let client = client(&base_url, 2, EmbeddingProfile::Plain).await;
    let error = client.embed("text", EmbeddingPurpose::Document).await.unwrap_err().to_string();
    assert!(error.contains("401"), "{error}");
    assert!(error.contains("invalid api key"), "{error}");
    assert!(!error.contains("sk-test"), "the key never reaches an error: {error}");
    server.await.unwrap();
}

#[tokio::test]
async fn batches_are_chunked_at_32_and_keep_input_order() {
    let first: Vec<Vec<f64>> = vec![vec![1.0, 0.0]; 32];
    let (base_url, server) = mock_service(vec![probe_reply(2), ok(&first), ok(&[vec![0.0, 1.0]])]).await;
    let client = client(&base_url, 2, EmbeddingProfile::Plain).await;
    let inputs: Vec<String> = (0..33).map(|i| format!("input {i}")).collect();
    let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let vectors = client.embed_batch(&refs, EmbeddingPurpose::Document).await.unwrap();
    assert_eq!(vectors.len(), 33);
    assert_eq!(vectors[32], vec![0.0, 1.0]);
    let requests = server.await.unwrap();
    assert_eq!(requests[1].body["input"], serde_json::json!(&inputs[..32]));
    assert_eq!(requests[2].body["input"], serde_json::json!(&inputs[32..]));
}

#[tokio::test]
async fn connect_refuses_a_model_that_does_not_return_the_configured_dimensions() {
    let (base_url, server) = mock_service(vec![probe_reply(4)]).await;
    let error = match OpenAiEmbedder::connect(config(&base_url, 3, EmbeddingProfile::Plain)).await {
        Ok(_) => panic!("a dimension mismatch must fail the boot probe"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("embedding service: "), "{error}");
    assert!(error.contains('3') && error.contains('4'), "names both sizes: {error}");
    server.await.unwrap();
}

#[tokio::test]
async fn connect_refuses_an_unreachable_provider() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    drop(listener);
    let mut config = config(&base_url, 2, EmbeddingProfile::Plain);
    config.timeout = Duration::from_millis(200);
    assert!(OpenAiEmbedder::connect(config).await.is_err());
}

#[tokio::test]
async fn a_bad_configuration_is_refused_before_any_request() {
    let base = "http://127.0.0.1:9/api/v1";
    let cases: Vec<(&str, OpenAiEmbedderConfig)> = vec![
        ("empty key", OpenAiEmbedderConfig { api_key: Some(" ".into()), ..config(base, 2, EmbeddingProfile::Plain) }),
        ("empty model", OpenAiEmbedderConfig { model: String::new(), ..config(base, 2, EmbeddingProfile::Plain) }),
        ("zero dimensions", config(base, 0, EmbeddingProfile::Plain)),
        ("zero concurrency", OpenAiEmbedderConfig { max_in_flight: 0, ..config(base, 2, EmbeddingProfile::Plain) }),
        ("zero timeout", OpenAiEmbedderConfig { timeout: Duration::ZERO, ..config(base, 2, EmbeddingProfile::Plain) }),
        ("not http", config("ftp://example.com/v1", 2, EmbeddingProfile::Plain)),
        ("credentials in the url", config("http://user:secret@example.com/v1", 2, EmbeddingProfile::Plain)),
        ("query string", config("http://example.com/v1?x=1", 2, EmbeddingProfile::Plain)),
    ];
    for (name, config) in cases {
        let error = match OpenAiEmbedder::connect(config).await {
            Ok(_) => panic!("accepted: {name}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("embedding service: "), "{name}: {error}");
    }
}

#[tokio::test]
async fn the_request_deadline_includes_waiting_for_capacity() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        let _ = stream.read(&mut buffer).await.unwrap();
        stream.write_all(probe_reply(2).as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let (first, _) = listener.accept().await.unwrap();
        // With one permit, the queued call must time out without opening a
        // second request while the first response is stalled.
        let second = tokio::time::timeout(Duration::from_millis(40), listener.accept()).await;
        assert!(second.is_err(), "concurrency limit allowed a second in-flight request");
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(first);
    });
    let mut config = config(&base_url, 2, EmbeddingProfile::Plain);
    config.timeout = Duration::from_millis(100);
    config.max_in_flight = 1;
    let client = std::sync::Arc::new(OpenAiEmbedder::connect(config).await.unwrap());
    let (a, b) = tokio::time::timeout(Duration::from_millis(170), async {
        tokio::join!(client.embed("first", EmbeddingPurpose::Document), client.embed("second", EmbeddingPurpose::Document))
    }).await.expect("queue time must be covered by the request deadline");
    assert!(a.is_err());
    assert!(b.is_err());
    server.await.unwrap();
}

#[test]
fn profile_tokens_round_trip_and_unknown_tokens_are_refused() {
    for profile in [EmbeddingProfile::Plain, EmbeddingProfile::Qwen3, EmbeddingProfile::EmbeddingGemma] {
        assert_eq!(profile.as_str().parse::<EmbeddingProfile>().unwrap(), profile);
    }
    let error = "gemini".parse::<EmbeddingProfile>().unwrap_err();
    assert!(error.contains("plain") && error.contains("qwen3") && error.contains("embeddinggemma"), "{error}");
}

/// A local server with no key gets no `Authorization` header at all.
#[tokio::test]
async fn a_server_with_no_key_gets_no_authorization_header() {
    let (base_url, server) = mock_service(vec![probe_reply(2), ok(&[vec![1.0, 0.0]])]).await;
    let client = OpenAiEmbedder::connect(OpenAiEmbedderConfig { api_key: None, ..config(&base_url, 2, EmbeddingProfile::Plain) })
        .await.unwrap();
    client.embed("text", EmbeddingPurpose::Document).await.unwrap();
    let requests = server.await.unwrap();
    for request in &requests {
        assert_eq!(request.header("authorization"), None, "{}", request.line);
    }
}

/// The same model id at another endpoint is another vector space.
#[tokio::test]
async fn the_cache_identity_changes_with_the_endpoint() {
    let (first, a) = mock_service(vec![probe_reply(2)]).await;
    let (second, b) = mock_service(vec![probe_reply(2)]).await;
    let one = client(&first, 2, EmbeddingProfile::Plain).await.cache_identity();
    let two = client(&second, 2, EmbeddingProfile::Plain).await.cache_identity();
    a.await.unwrap();
    b.await.unwrap();
    assert_ne!(one, two);
}
