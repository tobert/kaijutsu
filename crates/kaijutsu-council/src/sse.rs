//! Server-sent event streams: the decoder and the stream of decoded events
//! a streamed council v2 reply is read through.
//!
//! A streamed reply is its events, then one last event. Anything else that
//! ends a stream is an error, never a short reply.

use std::collections::VecDeque;
use std::time::Duration;

use reqwest::Method;

use crate::client::{excerpt, map_reqwest, CouncilClient, CouncilError};

impl CouncilClient {
    /// Sends `body` to `path` and returns its server-sent events. `idle`
    /// bounds the wait for the response and then each read.
    pub(crate) async fn open_stream<T: StreamItem>(
        &self,
        path: &str,
        body: Vec<u8>,
        idle: Duration,
    ) -> Result<EventStream<T>, CouncilError> {
        let send = self.send(Method::POST, path, Some(body), &[], None);
        let resp = tokio::time::timeout(idle, send).await.map_err(|_| CouncilError::Timeout)??;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(CouncilError::Decode {
                what: "status",
                message: format!("expected 200, got {}", resp.status()),
                body: String::new(),
            });
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.starts_with("text/event-stream") {
            return Err(CouncilError::Decode {
                what: "content type",
                message: format!("expected text/event-stream, got {content_type:?}"),
                body: String::new(),
            });
        }
        Ok(EventStream {
            resp,
            idle,
            decoder: SseDecoder::default(),
            frames: VecDeque::new(),
            ended: false,
            item: std::marker::PhantomData,
        })
    }
}

/// One decoded event of a reply stream.
pub trait StreamItem: Sized {
    /// Decodes one server-sent event. An `error` event is an error, and
    /// `None` is an event the stream skips.
    fn decode(frame: &Frame) -> Result<Option<Self>, CouncilError>;
    /// Whether this event ends the stream.
    fn is_last(&self) -> bool;
}

/// A streamed reply. [`next`](Self::next) yields tokens, then the last
/// event, then `None`. An error is yielded once, and then the stream is over.
pub struct EventStream<T> {
    resp: reqwest::Response,
    idle: Duration,
    decoder: SseDecoder,
    frames: VecDeque<Frame>,
    ended: bool,
    item: std::marker::PhantomData<T>,
}

impl<T: StreamItem> EventStream<T> {
    pub async fn next(&mut self) -> Option<Result<T, CouncilError>> {
        loop {
            if self.ended {
                return None;
            }
            if let Some(frame) = self.frames.pop_front() {
                let event = match T::decode(&frame) {
                    Ok(None) => continue,
                    Ok(Some(e)) => Ok(e),
                    Err(e) => Err(e),
                };
                self.ended = match &event {
                    Ok(e) => e.is_last(),
                    Err(_) => true,
                };
                return Some(event);
            }
            let chunk = match tokio::time::timeout(self.idle, self.resp.chunk()).await {
                Err(_) => Err(CouncilError::Timeout),
                Ok(Err(e)) => Err(map_reqwest(e)),
                Ok(Ok(None)) => Err(CouncilError::Truncated(
                    self.decoder.finish().err().unwrap_or_else(|| "the connection closed".into()),
                )),
                Ok(Ok(Some(bytes))) => self.decoder.push(&bytes).map_err(|message| CouncilError::Decode {
                    what: "event stream",
                    message,
                    body: excerpt(&bytes),
                }),
            };
            match chunk {
                Ok(frames) => self.frames.extend(frames),
                Err(e) => {
                    self.ended = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// One server-sent event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The `event` field; `message` when the event named none.
    pub event: String,
    /// The `data` lines, joined with newlines.
    pub data: String,
}

/// Server-sent events from any chunking of the byte stream: LF, CRLF, or lone
/// CR line ends, `:` comment lines, and multi-line `data` joined with
/// newlines.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    /// Bytes of the line not yet ended.
    line: Vec<u8>,
    /// The last byte was CR: an LF next ends nothing new.
    after_cr: bool,
    event: Option<String>,
    data: Vec<String>,
    /// A field line arrived since the last blank line.
    started: bool,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, String> {
        let mut frames = Vec::new();
        for &b in bytes {
            let after_cr = std::mem::replace(&mut self.after_cr, b == b'\r');
            match b {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    let line = std::mem::take(&mut self.line);
                    if let Some(frame) = self.line_ended(&line)? {
                        frames.push(frame);
                    }
                }
                _ => self.line.push(b),
            }
        }
        Ok(frames)
    }

    /// Fails when the bytes so far end inside a line or an event.
    pub(crate) fn finish(&self) -> Result<(), String> {
        if !self.line.is_empty() {
            return Err(format!("it ended inside a line: {:?}", String::from_utf8_lossy(&self.line)));
        }
        if self.started {
            return Err("it ended inside an event".into());
        }
        Ok(())
    }

    fn line_ended(&mut self, line: &[u8]) -> Result<Option<Frame>, String> {
        if line.is_empty() {
            let event = self.event.take();
            let data = std::mem::take(&mut self.data);
            let started = std::mem::replace(&mut self.started, false);
            if !started || data.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Frame { event: event.unwrap_or_else(|| "message".into()), data: data.join("\n") }));
        }
        if line[0] == b':' {
            return Ok(None);
        }
        let line = std::str::from_utf8(line).map_err(|e| format!("a line is not UTF-8: {e}"))?;
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        self.started = true;
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(text: &str, chunk: usize) -> Result<Vec<Frame>, String> {
        let mut d = SseDecoder::default();
        let mut frames = Vec::new();
        for piece in text.as_bytes().chunks(chunk) {
            frames.extend(d.push(piece)?);
        }
        d.finish()?;
        Ok(frames)
    }

    #[test]
    fn any_chunking_and_line_end_gives_the_same_frames() {
        let lf = "event: token\ndata: {\"a\":1}\n\n: a comment\nevent: done\ndata: x\ndata: y\n\n";
        let want = vec![
            Frame { event: "token".into(), data: "{\"a\":1}".into() },
            Frame { event: "done".into(), data: "x\ny".into() },
        ];
        for text in [lf.to_string(), lf.replace('\n', "\r\n"), lf.replace('\n', "\r")] {
            for chunk in [1, 2, 3, 5, 64] {
                assert_eq!(decode_all(&text, chunk).unwrap(), want, "{text:?} in chunks of {chunk}");
            }
        }
    }

    #[test]
    fn a_stream_cut_inside_a_line_or_an_event_is_an_error() {
        assert!(decode_all("event: token\ndata: {\"a\"", 4).unwrap_err().contains("inside a line"));
        assert!(decode_all("event: token\ndata: {}\n", 4).unwrap_err().contains("inside an event"));
        assert_eq!(decode_all("", 4).unwrap(), vec![]);
    }
}
