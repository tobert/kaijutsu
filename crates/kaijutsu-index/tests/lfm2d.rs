use std::time::Duration;

use kaijutsu_index::{Embedder, EmbeddingPurpose, Lfm2dEmbedder};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn service(body: &str, hash: &str) -> (String, tokio::task::JoinHandle<Vec<(String, serde_json::Value)>>) {
    mock_service(vec![discovery(), embedding_response(body, hash)]).await
}

fn discovery() -> String {
    response(&format!(r#"[{{"kind":"embedder","id":"lfm-test","weight_hash":"{HASH}","hidden_size":3}}]"#))
}

fn response(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}")
}

fn embedding_response(body: &str, hash: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Model-Id: lfm-test\r\nX-Model-Weight-Hash: {hash}\r\nConnection: close\r\n\r\n{body}")
}

async fn mock_service(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<(String, serde_json::Value)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request_line = String::new();
            stream.read_line(&mut request_line).await.unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" { break; }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") { length = value.trim().parse().unwrap(); }
                }
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            requests.push((request_line, if body.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&body).unwrap() }));
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        requests
    });
    (endpoint, task)
}

#[tokio::test]
async fn service_preserves_purpose_order_and_normalization() {
    for purpose in [EmbeddingPurpose::Query, EmbeddingPurpose::Document] {
        let (endpoint, server) = service("[[3,4,0],[0,0,2]]", HASH).await;
        let client = Lfm2dEmbedder::connect(&endpoint, Duration::from_secs(2), 2).await.unwrap();
        let vectors = client.embed_batch(&["first", "second"], purpose).await.unwrap();
        assert_eq!(vectors, vec![vec![0.6, 0.8, 0.0], vec![0.0, 0.0, 1.0]]);
        assert_eq!(client.dimensions(), 3);
        assert!(client.cache_identity().contains(HASH));
        let requests = server.await.unwrap();
        assert!(requests[0].0.starts_with("GET /v1/models "));
        assert!(requests[1].0.starts_with("POST /embed "));
        assert_eq!(requests[1].1, serde_json::json!({"inputs": ["first", "second"], "kind": purpose}));
    }
}

#[tokio::test]
async fn service_refuses_malformed_or_changed_profile_results() {
    for (body, hash) in [
        ("[]", HASH), ("[[1,0]]", HASH), ("[[0,0,0]]", HASH),
        ("[[1e100,0,0]]", HASH), ("[[1,0,0],[0,1,0]]", HASH),
        ("[[1,0,0]]", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    ] {
        let (endpoint, server) = service(body, hash).await;
        let client = Lfm2dEmbedder::connect(&endpoint, Duration::from_secs(2), 1).await.unwrap();
        assert!(client.embed("text", EmbeddingPurpose::Document).await.is_err(), "accepted {body} / {hash}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn service_refuses_unavailable_endpoint() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert!(Lfm2dEmbedder::connect(&endpoint, Duration::from_millis(100), 1).await.is_err());
}

#[tokio::test]
async fn service_chunks_batches_and_preserves_input_order() {
    let (endpoint, server) = mock_service(vec![discovery(),
        embedding_response(&serde_json::to_string(&vec![vec![1, 0, 0]; 32]).unwrap(), HASH),
        embedding_response("[[0,1,0]]", HASH),
    ]).await;
    let client = Lfm2dEmbedder::connect(&endpoint, Duration::from_secs(2), 1).await.unwrap();
    let inputs: Vec<String> = (0..33).map(|i| format!("input {i}")).collect();
    let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let vectors = client.embed_batch(&refs, EmbeddingPurpose::Document).await.unwrap();
    assert_eq!(vectors.len(), 33);
    assert_eq!(vectors[32], vec![0.0, 1.0, 0.0]);
    let requests = server.await.unwrap();
    assert_eq!(requests[1].1["inputs"], serde_json::json!(&inputs[..32]));
    assert_eq!(requests[2].1["inputs"], serde_json::json!(&inputs[32..]));
}

#[tokio::test]
async fn service_refuses_ambiguous_discovery_missing_identity_and_http_errors() {
    for body in ["[]".to_owned(), r#"[{"id":"classifier","kind":"classifier"}]"#.into(),
        format!(r#"[{{"id":"a","kind":"embedder","weight_hash":"{HASH}","hidden_size":3}},{{"id":"b","kind":"embedder","weight_hash":"{HASH}","hidden_size":3}}]"#),
        r#"[{"id":"a","kind":"embedder","hidden_size":3}]"#.into(),
    ] {
        let (endpoint, server) = mock_service(vec![response(&body)]).await;
        assert!(Lfm2dEmbedder::connect(&endpoint, Duration::from_secs(2), 1).await.is_err());
        server.await.unwrap();
    }
    for reply in [response("[[1,0,0]]"),
        embedding_response("[[1,0,0]]", HASH).replace("X-Model-Id: lfm-test", "X-Model-Id: swapped"),
        "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n".into(),
    ] {
        let (endpoint, server) = mock_service(vec![discovery(), reply]).await;
        let client = Lfm2dEmbedder::connect(&endpoint, Duration::from_secs(2), 1).await.unwrap();
        assert!(client.embed("input", EmbeddingPurpose::Document).await.is_err());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn service_request_deadline_includes_waiting_for_capacity() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        stream.read(&mut buffer).await.unwrap();
        stream.write_all(discovery().as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let (first, _) = listener.accept().await.unwrap();
        // With one permit, the queued call must time out without opening a
        // second request while the first response is stalled.
        let second = tokio::time::timeout(Duration::from_millis(40), listener.accept()).await;
        assert!(second.is_err(), "concurrency limit allowed a second in-flight request");
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(first);
    });
    let client = std::sync::Arc::new(Lfm2dEmbedder::connect(&endpoint, Duration::from_millis(100), 1).await.unwrap());
    let (a, b) = tokio::time::timeout(Duration::from_millis(170), async {
        tokio::join!(client.embed("first", EmbeddingPurpose::Document), client.embed("second", EmbeddingPurpose::Document))
    }).await.expect("queue time must be covered by the request deadline");
    assert!(a.is_err());
    assert!(b.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn service_refuses_endpoint_prefix_instead_of_silently_discarding_it() {
    let error = match Lfm2dEmbedder::connect("http://127.0.0.1:9/prefix", Duration::from_millis(100), 1).await {
        Ok(_) => panic!("a prefixed endpoint must be rejected"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("root URL"), "reject the path before making a request: {error}");
}
