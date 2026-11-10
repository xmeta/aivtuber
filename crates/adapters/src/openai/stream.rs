//! Bounded server-sent-event streaming for the OpenAI-compatible Thinking
//! adapter (issue #83 S2).
//!
//! Provider wire DTOs stop here: the rest of the system sees only plain text
//! deltas through [`ReplyDeltaSink`]. Every resource is bounded: bytes per
//! event, bytes per stream, characters per reply, idle time between chunks and
//! the overall deadline. The HTTP exchange runs on a short-lived reader thread
//! so that cancellation, the idle timeout and the deadline are all observed
//! even while the provider is silent (a blocking socket read cannot be
//! interrupted); an abandoned reader ends at the transport deadline at the
//! latest.

use super::{OpenAiResponsesAdapter, compact_generation_input, map_status, map_transport_error};
use crate::StreamingHttpTransport;
use aivtuber_domain::{
    DeltaFlow, EngineError, EngineErrorKind, GeneratedReply, ReplyDeltaSink, ThinkingRequest,
};
use serde_json::{Value, json};
use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Bounds of one streamed reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamLimits {
    /// Largest single SSE event (all `data:` lines of one frame).
    pub max_event_bytes: usize,
    /// Largest decoded stream body.
    pub max_stream_bytes: u64,
    /// Largest accumulated reply, in characters.
    pub max_reply_chars: usize,
    /// Longest silence between two chunks of the body.
    pub idle_timeout: Duration,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: 16 * 1024,
            max_stream_bytes: 1024 * 1024,
            max_reply_chars: 4096,
            idle_timeout: Duration::from_secs(10),
        }
    }
}

impl StreamLimits {
    pub fn validate(&self) -> Result<(), EngineError> {
        if self.max_event_bytes == 0
            || self.max_stream_bytes == 0
            || self.max_reply_chars == 0
            || self.idle_timeout.is_zero()
        {
            return Err(EngineError::new(
                EngineErrorKind::InvalidRequest,
                "stream limits must all be greater than zero",
            ));
        }
        Ok(())
    }
}

/// How often a silent stream re-checks cancellation and the clocks.
const POLL: Duration = Duration::from_millis(20);
const READ_CHUNK: usize = 4096;
/// Chunks the reader may run ahead of the consumer; beyond this the socket
/// applies backpressure.
const READ_AHEAD: usize = 8;

// ---------------------------------------------------------------------------
// SSE framing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseError {
    EventTooLarge,
    InvalidUtf8,
}

/// Incremental SSE frame parser. Accepts `\n`, `\r\n` and `\r` line ends, a
/// frame split across any number of reads, multi-line `data:` fields (joined
/// with `\n`), and ignores comments and fields other than `data`. An event
/// still open when the stream ends is discarded, as the SSE specification
/// requires.
pub(crate) struct SseParser {
    max_event_bytes: usize,
    line: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    pending_cr: bool,
}

impl SseParser {
    pub(crate) fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            line: Vec::new(),
            data: Vec::new(),
            has_data: false,
            pending_cr: false,
        }
    }

    /// Feed bytes; completed `data` payloads are appended to `out`.
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) -> Result<(), SseError> {
        for &byte in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue; // second half of CRLF
                }
            }
            match byte {
                b'\n' => self.end_line(out)?,
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(out)?;
                }
                other => {
                    self.line.push(other);
                    if self.line.len() + self.data.len() > self.max_event_bytes {
                        return Err(SseError::EventTooLarge);
                    }
                }
            }
        }
        Ok(())
    }

    fn end_line(&mut self, out: &mut Vec<String>) -> Result<(), SseError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.has_data {
                let data = std::mem::take(&mut self.data);
                self.has_data = false;
                out.push(String::from_utf8(data).map_err(|_| SseError::InvalidUtf8)?);
            }
            return Ok(());
        }
        let value = if let Some(rest) = line.strip_prefix(b"data:") {
            rest.strip_prefix(b" ").unwrap_or(rest)
        } else if line == b"data" {
            &[][..]
        } else {
            return Ok(()); // comment, event:, id:, retry:
        };
        if self.has_data {
            self.data.push(b'\n');
        }
        self.has_data = true;
        self.data.extend_from_slice(value);
        if self.data.len() > self.max_event_bytes {
            return Err(SseError::EventTooLarge);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Provider DTO (private to this adapter)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamItem {
    Delta(String),
    Done,
    Ignore,
}

/// Interpret one `data:` payload. Understands the Responses API events
/// (`response.output_text.delta`, `response.completed`, `response.incomplete`,
/// `response.failed`, `error`) and the Chat Completions chunk shape
/// (`choices[0].delta.content`, `[DONE]`), because OpenAI-compatible servers,
/// llama-server among them, speak either.
pub(crate) fn parse_stream_event(data: &str) -> Result<StreamItem, EngineError> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(StreamItem::Done);
    }
    let bad = |what: &str| EngineError::new(EngineErrorKind::Backend, what.to_owned());
    let value: Value = serde_json::from_str(data).map_err(|_| bad("malformed stream event"))?;
    if let Some(kind) = value.get("type").and_then(Value::as_str) {
        return match kind {
            "response.output_text.delta" => Ok(value
                .get("delta")
                .and_then(Value::as_str)
                .map_or(StreamItem::Ignore, |text| {
                    StreamItem::Delta(text.to_owned())
                })),
            // `incomplete` is a reply cut at the token budget: the text so far
            // is the reply, exactly as in the buffered path.
            "response.completed" | "response.incomplete" => Ok(StreamItem::Done),
            "response.failed" | "error" => Err(bad("provider reported a stream failure")),
            _ => Ok(StreamItem::Ignore),
        };
    }
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(bad("provider reported a stream failure"));
    }
    if let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) {
        return Ok(choice
            .get("delta")
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map_or(StreamItem::Ignore, |text| {
                StreamItem::Delta(text.to_owned())
            }));
    }
    Ok(StreamItem::Ignore)
}

// ---------------------------------------------------------------------------
// The streaming call
// ---------------------------------------------------------------------------

enum Message {
    Status(u16),
    Chunk(Vec<u8>),
    End,
    Failed(EngineError),
}

fn read_error(error: &std::io::Error) -> EngineError {
    let text = error.to_string().to_ascii_lowercase();
    if error.kind() == std::io::ErrorKind::TimedOut || text.contains("timeout") {
        EngineError::new(EngineErrorKind::Timeout, "stream read timed out")
    } else {
        EngineError::new(EngineErrorKind::Unavailable, "stream read failed")
    }
}

fn spawn_reader(
    transport: Arc<dyn StreamingHttpTransport>,
    endpoint: String,
    bearer: Option<String>,
    body: Vec<u8>,
    timeout: Duration,
    max_stream_bytes: u64,
) -> Result<mpsc::Receiver<Message>, EngineError> {
    let (tx, rx) = mpsc::sync_channel(READ_AHEAD);
    std::thread::Builder::new()
        .name("thinking-sse-reader".to_owned())
        .spawn(move || {
            let response = match transport.post_json_stream(
                &endpoint,
                bearer.as_deref(),
                &body,
                timeout,
                max_stream_bytes,
            ) {
                Ok(response) => response,
                Err(error) => {
                    let _ = tx.send(Message::Failed(map_transport_error(error)));
                    return;
                }
            };
            if tx.send(Message::Status(response.status)).is_err() || response.status != 200 {
                return;
            }
            let mut reader = response.body;
            let mut buffer = [0_u8; READ_CHUNK];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = tx.send(Message::End);
                        return;
                    }
                    Ok(n) => {
                        if tx.send(Message::Chunk(buffer[..n].to_vec())).is_err() {
                            return; // consumer gone (cancelled or finished)
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Message::Failed(read_error(&error)));
                        return;
                    }
                }
            }
        })
        .map_err(|_| {
            EngineError::new(
                EngineErrorKind::Unavailable,
                "could not start stream reader",
            )
        })?;
    Ok(rx)
}

fn stopped() -> EngineError {
    EngineError::new(EngineErrorKind::Backend, "stream stopped by the consumer")
}

impl OpenAiResponsesAdapter {
    pub(super) fn stream_reply(
        &self,
        request: &ThinkingRequest,
        timeout: Duration,
        sink: &mut dyn ReplyDeltaSink,
    ) -> Option<Result<GeneratedReply, EngineError>> {
        let (transport, limits) = self.stream.as_ref()?;
        Some(self.stream_reply_inner(transport, *limits, request, timeout, sink))
    }

    fn stream_reply_inner(
        &self,
        transport: &Arc<dyn StreamingHttpTransport>,
        limits: StreamLimits,
        request: &ThinkingRequest,
        timeout: Duration,
        sink: &mut dyn ReplyDeltaSink,
    ) -> Result<GeneratedReply, EngineError> {
        if timeout.is_zero() {
            return Err(EngineError::new(
                EngineErrorKind::Timeout,
                "OpenAI-compatible call has no remaining interaction budget",
            ));
        }
        let started = Instant::now();
        let input = compact_generation_input(request)?;
        let body = serde_json::to_vec(&json!({
            "model": self.config.model_alias,
            "input": input,
            "max_output_tokens": self.config.max_output_tokens,
            "stream": true,
        }))
        .map_err(|error| EngineError::new(EngineErrorKind::InvalidRequest, error.to_string()))?;
        let receiver = spawn_reader(
            Arc::clone(transport),
            self.config.endpoint.clone(),
            self.api_key.as_ref().map(|key| key.expose().to_owned()),
            body,
            timeout,
            limits.max_stream_bytes,
        )?;

        let mut parser = SseParser::new(limits.max_event_bytes);
        let mut reply = String::new();
        let mut chars = 0_usize;
        let mut last_activity = Instant::now();
        let mut events = Vec::new();
        loop {
            if sink.is_cancelled() {
                return Err(stopped());
            }
            let now = Instant::now();
            if now.duration_since(started) >= timeout {
                return Err(EngineError::new(
                    EngineErrorKind::Timeout,
                    "stream exceeded its overall deadline",
                ));
            }
            if now.duration_since(last_activity) >= limits.idle_timeout {
                return Err(EngineError::new(
                    EngineErrorKind::Timeout,
                    "stream idle timeout",
                ));
            }
            let message = match receiver.recv_timeout(POLL) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(EngineError::new(
                        EngineErrorKind::Unavailable,
                        "stream reader ended unexpectedly",
                    ));
                }
            };
            last_activity = Instant::now();
            match message {
                Message::Status(status) if status != 200 => return Err(map_status(status)),
                Message::Status(_) => {}
                Message::Failed(error) => return Err(error),
                Message::End => {
                    return Err(EngineError::new(
                        EngineErrorKind::Unavailable,
                        "stream ended before the completion marker",
                    ));
                }
                Message::Chunk(bytes) => {
                    events.clear();
                    parser.feed(&bytes, &mut events).map_err(|error| {
                        EngineError::new(
                            EngineErrorKind::Backend,
                            match error {
                                SseError::EventTooLarge => "stream event exceeded its byte limit",
                                SseError::InvalidUtf8 => "stream event was not valid UTF-8",
                            },
                        )
                    })?;
                    for data in events.drain(..) {
                        match parse_stream_event(&data)? {
                            StreamItem::Ignore => {}
                            StreamItem::Done => {
                                if reply.trim().is_empty() {
                                    return Err(EngineError::new(
                                        EngineErrorKind::Backend,
                                        "OpenAI-compatible stream produced no text",
                                    ));
                                }
                                return Ok(GeneratedReply { text: reply });
                            }
                            StreamItem::Delta(text) => {
                                chars += text.chars().count();
                                if chars > limits.max_reply_chars {
                                    return Err(EngineError::new(
                                        EngineErrorKind::Backend,
                                        "streamed reply exceeded its character limit",
                                    ));
                                }
                                reply.push_str(&text);
                                if sink.push(&text) == DeltaFlow::Stop {
                                    return Err(stopped());
                                }
                            }
                        }
                    }
                    // The sink may have blocked (synthesis, a lookahead wait):
                    // that time is not provider silence.
                    last_activity = Instant::now();
                }
            }
        }
    }
}
