//! A scripted stand-in for the command classifier the gate's advisory hook
//! consults, served over HTTP on 127.0.0.1.
//!
//! The wire protocol lives in this module alone, so replacing the classifier
//! changes this file and the scenarios keep their `[classifier]` table. Today
//! the hook is `assets/defaults/rc/lib/hooks/lfm2d.kai`, which reads the URL
//! from `[classifier] url` in the gate policy and calls:
//!
//! - `POST /v1/cascade` with `{"clauses": [command]}`, answered with
//!   `{"winner": {"index"}, "clauses": [{"index", "top_severity",
//!   "severity_scores": {label: score}}], "models": [{"model_id", "weight_hash"}]}`.
//! - `GET /v1/models`, answered with `[{"id", "labels": [ladder...]}]`.
//!
//! The hook allows a call only when the winning label is the benign label at
//! ladder index 0; every other answer, an unreachable service, or a malformed
//! reply asks a human.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// What a scenario's `[classifier]` table says the classifier does.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierSpec {
    /// `answer` (the default), `down` (nothing listens at the URL), or
    /// `malformed` (every reply is 200 with a body that is not JSON).
    #[serde(default)]
    pub behavior: Behavior,
    /// The severity ladder, least severe first.
    #[serde(default = "default_labels")]
    pub labels: Vec<String>,
    /// The label every command is scored as. Required for `answer`.
    #[serde(default)]
    pub verdict: Option<String>,
    /// Commands the classifier must have scored, in this order, across the
    /// whole run. Other scored commands may come between them.
    #[serde(default)]
    pub expect_scored: Vec<String>,
    /// Commands the classifier must not have scored.
    #[serde(default)]
    pub expect_unscored: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Behavior {
    #[default]
    Answer,
    Down,
    Malformed,
}

fn default_labels() -> Vec<String> {
    ["informative", "caution", "destructive"].map(String::from).to_vec()
}

/// The model id the mock reports.
const MODEL_ID: &str = "fleet-mock-classifier";

impl ClassifierSpec {
    pub fn check(&self) -> Result<()> {
        if self.behavior == Behavior::Answer {
            let Some(verdict) = &self.verdict else {
                bail!("[classifier]: `behavior = \"answer\"` needs a `verdict` label");
            };
            if !self.labels.contains(verdict) {
                bail!("[classifier]: verdict {verdict:?} is not in labels {:?}", self.labels);
            }
        }
        Ok(())
    }

    /// Every way `scored`, the commands the classifier scored in order,
    /// misses `expect_scored` and `expect_unscored`.
    pub fn judge(&self, scored: &[String]) -> Vec<String> {
        let mut failures = Vec::new();
        let mut rest = scored.iter();
        for want in &self.expect_scored {
            if !rest.any(|got| got == want) {
                failures.push(format!(
                    "classifier: expected it to score {want:?} (in order after {:?}); it scored {scored:?}",
                    self.expect_scored
                ));
                break;
            }
        }
        for unwanted in &self.expect_unscored {
            if scored.contains(unwanted) {
                failures.push(format!("classifier: expected it not to score {unwanted:?}; it scored {scored:?}"));
            }
        }
        failures
    }

    fn cascade_reply(&self) -> Value {
        let verdict = self.verdict.clone().unwrap_or_default();
        json!({
            "winner": {"index": 0},
            "clauses": [{"index": 0, "top_severity": verdict, "severity_scores": {verdict.clone(): 0.9}}],
            "models": [{"model_id": MODEL_ID, "weight_hash": "fleet"}],
        })
    }

    fn models_reply(&self) -> Value {
        json!([{"id": MODEL_ID, "labels": self.labels}])
    }
}

/// A running mock classifier. Stops when dropped.
pub struct MockClassifier {
    url: String,
    scored: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl MockClassifier {
    pub fn start(spec: &ClassifierSpec) -> Result<Self> {
        spec.check()?;
        let listener = TcpListener::bind("127.0.0.1:0").context("bind the mock classifier")?;
        let port = listener.local_addr().context("read the mock classifier's port")?.port();
        let url = format!("http://127.0.0.1:{port}");
        let scored = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        if spec.behavior == Behavior::Down {
            // Nothing listens: the port was free a moment ago and is closed now.
            drop(listener);
            return Ok(Self { url, scored, requests, stop });
        }
        listener.set_nonblocking(true).context("make the mock classifier's listener nonblocking")?;
        let (spec, s, r, halt) = (spec.clone(), Arc::clone(&scored), Arc::clone(&requests), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (spec, s, r) = (spec.clone(), Arc::clone(&s), Arc::clone(&r));
                        std::thread::spawn(move || {
                            let _ = serve(stream, &spec, &s, &r);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
        });
        Ok(Self { url, scored, requests, stop })
    }

    /// The base URL the gate policy's `[classifier] url` names.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The commands scored so far, in arrival order.
    pub fn scored(&self) -> Vec<String> {
        self.scored.lock().expect("classifier log poisoned").clone()
    }

    /// Every request line received, such as `POST /v1/cascade`.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("classifier log poisoned").clone()
    }
}

impl Drop for MockClassifier {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Answer one HTTP/1.1 request and close the connection.
fn serve(stream: TcpStream, spec: &ClassifierSpec, scored: &Mutex<Vec<String>>, requests: &Mutex<Vec<String>>) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().context("parse Content-Length")?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;

    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    requests.lock().expect("classifier log poisoned").push(format!("{method} {path}"));

    let (status, reply) = match (method, path) {
        ("POST", "/v1/cascade") => {
            let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(clauses) = request.get("clauses").and_then(Value::as_array) {
                let mut log = scored.lock().expect("classifier log poisoned");
                log.extend(clauses.iter().filter_map(Value::as_str).map(str::to_string));
            }
            ("200 OK", spec.cascade_reply().to_string())
        }
        ("GET", "/v1/models") => ("200 OK", spec.models_reply().to_string()),
        _ => ("404 Not Found", json!({"error": "no such route"}).to_string()),
    };
    let reply = if spec.behavior == Behavior::Malformed && status == "200 OK" {
        "this is not json".to_string()
    } else {
        reply
    };
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    )?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(toml_text: &str) -> ClassifierSpec {
        toml::from_str(toml_text).unwrap()
    }

    fn http(url: &str, request: &str) -> String {
        let address = url.trim_start_matches("http://");
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    fn body(reply: &str) -> Value {
        serde_json::from_str(reply.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[test]
    fn a_cascade_request_is_scored_and_recorded() {
        let mock = MockClassifier::start(&spec("verdict = \"destructive\"")).unwrap();
        let payload = r#"{"clauses":["rm -rf build"]}"#;
        let reply = http(
            mock.url(),
            &format!("POST /v1/cascade HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{payload}", payload.len()),
        );
        let reply = body(&reply);
        assert_eq!(reply["winner"]["index"], 0);
        assert_eq!(reply["clauses"][0]["top_severity"], "destructive");
        assert_eq!(reply["clauses"][0]["severity_scores"]["destructive"], 0.9);
        assert_eq!(reply["models"][0]["model_id"], MODEL_ID);
        assert_eq!(mock.scored(), vec!["rm -rf build".to_string()]);

        let models = body(&http(mock.url(), "GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert_eq!(models, json!([{"id": MODEL_ID, "labels": ["informative", "caution", "destructive"]}]));
        assert_eq!(mock.requests(), vec!["POST /v1/cascade", "GET /v1/models"]);
    }

    #[test]
    fn a_malformed_classifier_answers_with_a_body_that_is_not_json() {
        let mock = MockClassifier::start(&spec("behavior = \"malformed\"")).unwrap();
        let reply = http(mock.url(), "GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(reply.ends_with("\r\n\r\nthis is not json"), "{reply}");
    }

    #[test]
    fn a_down_classifier_refuses_connections() {
        let mock = MockClassifier::start(&spec("behavior = \"down\"")).unwrap();
        assert!(TcpStream::connect(mock.url().trim_start_matches("http://")).is_err());
    }

    #[test]
    fn scored_commands_are_judged_in_order_and_by_absence() {
        let spec = spec("verdict = \"informative\"\nexpect_scored = [\"a\", \"c\"]\nexpect_unscored = [\"x\"]");
        let scored = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(spec.judge(&scored(&["setup", "a", "b", "c"])).is_empty());
        assert_eq!(spec.judge(&scored(&["c", "a"])).len(), 1, "order matters");
        assert_eq!(spec.judge(&scored(&["a", "c", "x"])).len(), 1, "x must not be scored");
        assert_eq!(spec.judge(&scored(&[])).len(), 1);
    }

    #[test]
    fn an_answer_needs_a_verdict_on_the_ladder() {
        assert!(spec("").check().is_err());
        let err = spec("verdict = \"spicy\"").check().unwrap_err().to_string();
        assert!(err.contains("spicy"), "{err}");
    }
}
