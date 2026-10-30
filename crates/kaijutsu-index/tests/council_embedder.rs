use std::time::Duration;

use gogi_fake::{Config, FakeModel, Server};
use kaijutsu_index::{CouncilEmbedder, CouncilEmbedderConfig, Embedder, EmbeddingPurpose};

const EMBED: &str = "embed-toy-1";

fn server_with(models: Vec<FakeModel>, context_tokens: Option<usize>) -> Server {
    let mut config = Config::default();
    config.models.extend(models);
    if let Some(tokens) = context_tokens {
        config.context_tokens = tokens;
    }
    Server::start(config).unwrap()
}

fn embedder_server() -> Server {
    server_with(vec![FakeModel::embedding(EMBED, 7)], None)
}

fn config(server: &Server, dimensions: usize) -> CouncilEmbedderConfig {
    CouncilEmbedderConfig {
        base_url: server.url(),
        model: EMBED.to_owned(),
        dimensions,
        timeout: Duration::from_secs(5),
        max_in_flight: 2,
    }
}

async fn connect(server: &Server, dimensions: usize) -> CouncilEmbedder {
    CouncilEmbedder::connect(config(server, dimensions)).await.unwrap()
}

fn norm(vector: &[f32]) -> f64 {
    vector.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt()
}

#[tokio::test]
async fn it_embeds_at_the_configured_dimensions_with_unit_vectors_in_input_order() {
    let server = embedder_server();
    let embedder = connect(&server, 32).await;
    assert_eq!(embedder.model_name(), EMBED);
    assert_eq!(embedder.dimensions(), 32);
    let vectors = embedder.embed_batch(&["alpha", "beta beta", "gamma"], EmbeddingPurpose::Document).await.unwrap();
    assert_eq!(vectors.len(), 3);
    for vector in &vectors {
        assert_eq!(vector.len(), 32);
        assert!((norm(vector) - 1.0).abs() < 1e-5);
    }
    assert_ne!(vectors[0], vectors[1], "different inputs have different vectors");
}

/// The server renders the task prompt, so a query and a document of the same
/// text are different inputs to the model.
#[tokio::test]
async fn a_query_and_a_document_of_the_same_text_embed_differently() {
    let server = embedder_server();
    let embedder = connect(&server, 64).await;
    let query = embedder.embed("how does the gate decide?", EmbeddingPurpose::Query).await.unwrap();
    let document = embedder.embed("how does the gate decide?", EmbeddingPurpose::Document).await.unwrap();
    assert_ne!(query, document);
}

/// A vector does not depend on the other inputs of its request, so a long
/// batch (two requests) gives each input the vector it has alone.
#[tokio::test]
async fn a_batch_past_the_servers_limit_keeps_every_inputs_own_vector_in_order() {
    let server = embedder_server();
    let embedder = connect(&server, 16).await;
    let inputs: Vec<String> = (0..70).map(|i| format!("input number {i}")).collect();
    let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let vectors = embedder.embed_batch(&refs, EmbeddingPurpose::Document).await.unwrap();
    assert_eq!(vectors.len(), 70);
    for i in [0, 31, 32, 33, 69] {
        let alone = embedder.embed(&inputs[i], EmbeddingPurpose::Document).await.unwrap();
        assert_eq!(vectors[i], alone, "input {i}");
    }
}

#[tokio::test]
async fn the_cache_identity_follows_the_servers_identity_and_the_dimensions() {
    let first = server_with(vec![FakeModel::embedding(EMBED, 7)], None);
    let second = server_with(vec![FakeModel::embedding(EMBED, 8)], None);
    let a = connect(&first, 32).await.cache_identity();
    assert_eq!(a, connect(&first, 32).await.cache_identity(), "one server, one identity");
    assert_ne!(a, connect(&first, 16).await.cache_identity(), "dimensions are part of it");
    assert_ne!(a, connect(&second, 32).await.cache_identity(), "another weight hash is another vector space");
}

#[tokio::test]
async fn connect_refuses_what_the_server_cannot_embed() {
    let server = server_with(vec![FakeModel::embedding(EMBED, 7)], None);
    let toy = "gogi-toy-1";
    for (what, model, dimensions, needle) in [
        ("an unknown model", "nope", 32, "nope"),
        ("a model that does not embed", toy, 32, "embed"),
        ("dimensions it does not offer", EMBED, 100, "64, 32, 16"),
    ] {
        let mut config = config(&server, dimensions);
        config.model = model.to_owned();
        let error = match CouncilEmbedder::connect(config).await {
            Ok(_) => panic!("accepted: {what}"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("embedding service: "), "{what}: {error}");
        assert!(error.contains(needle), "{what} names {needle}: {error}");
    }
}

#[tokio::test]
async fn connect_refuses_an_unreachable_server_and_a_bad_configuration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let unreachable = CouncilEmbedderConfig {
        base_url,
        model: EMBED.to_owned(),
        dimensions: 32,
        timeout: Duration::from_millis(200),
        max_in_flight: 1,
    };
    assert!(CouncilEmbedder::connect(unreachable).await.is_err());

    let server = embedder_server();
    for (what, config) in [
        ("zero dimensions", CouncilEmbedderConfig { dimensions: 0, ..config(&server, 32) }),
        ("zero concurrency", CouncilEmbedderConfig { max_in_flight: 0, ..config(&server, 32) }),
        ("zero timeout", CouncilEmbedderConfig { timeout: Duration::ZERO, ..config(&server, 32) }),
        ("empty model", CouncilEmbedderConfig { model: " ".into(), ..config(&server, 32) }),
    ] {
        assert!(CouncilEmbedder::connect(config).await.is_err(), "accepted: {what}");
    }
}

/// The server never cuts an input, so an input past its limit is an error
/// that names the limit, and nothing is embedded.
#[tokio::test]
async fn an_input_past_the_servers_limit_is_an_error_naming_it() {
    let server = server_with(vec![FakeModel::embedding(EMBED, 7)], Some(64));
    let embedder = connect(&server, 32).await;
    let long = "x".repeat(10_000);
    let error = embedder.embed(&long, EmbeddingPurpose::Document).await.unwrap_err().to_string();
    assert!(error.contains("embedding service: "), "{error}");
    assert!(error.contains("413") || error.contains("context_tokens"), "{error}");
}

/// A server whose identity changes under a running client (new weights, a
/// new template) must not feed vectors from another space into the index.
#[tokio::test]
async fn a_response_with_another_identity_is_refused() {
    use kaijutsu_mk::test_server::{reply, serve};

    let server = embedder_server();
    let http = reqwest::Client::new();
    let identity = http.get(format!("{}/council/v2/identity", server.url())).send().await.unwrap().text().await.unwrap();
    let embed = |input: &str| {
        let http = http.clone();
        let url = format!("{}/council/v2/embeddings", server.url());
        let body = serde_json::json!({"model": EMBED, "task": "document", "inputs": [input], "dimensions": 32});
        async move { http.post(url).json(&body).send().await.unwrap().text().await.unwrap() }
    };
    let probe = embed("dimension probe").await;
    let later = embed("later").await;
    let mut changed: serde_json::Value = serde_json::from_str(&later).unwrap();
    changed["identity"]["weight_hash"] = serde_json::json!("0".repeat(64));

    let (base_url, _seen) = serve(vec![reply(200, identity), reply(200, probe), reply(200, later), reply(200, changed.to_string())]).await;
    let embedder = CouncilEmbedder::connect(CouncilEmbedderConfig { base_url, ..config(&server, 32) }).await.unwrap();
    embedder.embed("later", EmbeddingPurpose::Document).await.expect("the pinned identity still answers");
    let error = embedder.embed("later", EmbeddingPurpose::Document).await.unwrap_err().to_string();
    assert!(error.contains("identity"), "{error}");
}

/// The identity the fake advertises and the replies it gives, as text, so a
/// test can change one field and serve the rest unchanged.
async fn fake_replies(server: &Server) -> (serde_json::Value, serde_json::Value) {
    let http = reqwest::Client::new();
    let identity = http.get(format!("{}/council/v2/identity", server.url())).send().await.unwrap().text().await.unwrap();
    let body = serde_json::json!({"model": EMBED, "task": "document", "inputs": ["dimension probe"], "dimensions": 32});
    let probe = http.post(format!("{}/council/v2/embeddings", server.url())).json(&body).send().await.unwrap().text().await.unwrap();
    (serde_json::from_str(&identity).unwrap(), serde_json::from_str(&probe).unwrap())
}

/// A server whose identity promises one size and whose probe returns
/// another must fail at connect, not at the first block that needs a vector.
#[tokio::test]
async fn connect_refuses_a_probe_that_returns_the_wrong_size() {
    use kaijutsu_mk::test_server::{reply, serve};

    let server = embedder_server();
    let (identity, mut probe) = fake_replies(&server).await;
    probe["embeddings"][0]["vector"] = serde_json::json!(vec![0.5; 7]);
    let (base_url, _seen) = serve(vec![reply(200, identity.to_string()), reply(200, probe.to_string())]).await;
    let error = match CouncilEmbedder::connect(CouncilEmbedderConfig { base_url, ..config(&server, 32) }).await {
        Ok(_) => panic!("a wrong-size probe must fail the boot probe"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains('7') && error.contains("32"), "names both sizes: {error}");
}

/// A server that takes no inputs per request would make every batch panic
/// in `chunks(0)`; refuse it where the cause is visible.
#[tokio::test]
async fn connect_refuses_a_server_that_takes_no_inputs_per_request() {
    use kaijutsu_mk::test_server::{reply, serve};

    let server = embedder_server();
    let (mut identity, probe) = fake_replies(&server).await;
    let models = identity["models"].as_array_mut().unwrap();
    for model in models.iter_mut().filter(|m| m["model"] == EMBED) {
        model["embeddings"]["inputs_per_request"] = serde_json::json!(0);
    }
    let (base_url, _seen) = serve(vec![reply(200, identity.to_string()), reply(200, probe.to_string())]).await;
    let error = match CouncilEmbedder::connect(CouncilEmbedderConfig { base_url, ..config(&server, 32) }).await {
        Ok(_) => panic!("accepted a server that takes no inputs"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("inputs_per_request"), "{error}");
}
