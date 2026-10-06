//! A scripted HTTP/1.1 server on 127.0.0.1 for the crate's tests. It needs no
//! dependency beyond tokio.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One scripted reply, served on its own connection.
#[derive(Clone)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) content_type: &'static str,
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: String,
    /// Waited before the status line.
    pub(crate) delay: Duration,
    /// When set, the first half of the body is written, then this is waited
    /// before the rest.
    pub(crate) stall: Option<Duration>,
    /// When false, the body has no `content-length` and ends when the
    /// connection closes, as a stream does.
    pub(crate) length: bool,
}

pub(crate) fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        content_type: "application/json",
        headers: vec![],
        body: body.into(),
        delay: Duration::ZERO,
        stall: None,
        length: true,
    }
}

/// A close-delimited `text/event-stream` reply.
pub(crate) fn sse(body: impl Into<String>) -> Reply {
    Reply { content_type: "text/event-stream", length: false, ..reply(200, body) }
}

#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl Captured {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Serves `replies` in order, one connection each, and records each request.
pub(crate) async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for r in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head_end, content_length) = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before sending a request");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    break (i + 4, len);
                }
            };
            while buf.len() < head_end + content_length {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let mut lines = head.lines();
            let mut first = lines.next().unwrap().split(' ');
            let method = first.next().unwrap().to_string();
            let path = first.next().unwrap().to_string();
            let headers = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(n, v)| (n.trim().to_lowercase(), v.trim().to_string()))
                .collect();
            log.lock().unwrap().push(Captured {
                method,
                path,
                headers,
                body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
            });
            tokio::time::sleep(r.delay).await;
            let mut out = format!("HTTP/1.1 {} X\r\ncontent-type: {}\r\nconnection: close\r\n", r.status, r.content_type);
            if r.length {
                out.push_str(&format!("content-length: {}\r\n", r.body.len()));
            }
            for (n, v) in &r.headers {
                out.push_str(&format!("{n}: {v}\r\n"));
            }
            out.push_str("\r\n");
            let body = r.body.as_bytes();
            match r.stall {
                Some(stall) => {
                    let half = body.len() / 2;
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.write_all(&body[..half]).await;
                    let _ = sock.flush().await;
                    tokio::time::sleep(stall).await;
                    let _ = sock.write_all(&body[half..]).await;
                }
                None => {
                    out.push_str(&r.body);
                    let _ = sock.write_all(out.as_bytes()).await;
                }
            }
            let _ = sock.shutdown().await;
        }
    });
    (base, seen)
}
