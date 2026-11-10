//! A fake VOICEVOX ENGINE on loopback, for tests (issue #295).
//!
//! It speaks just enough of the engine's HTTP API (`POST /audio_query`,
//! `POST /synthesis`) for the bridge and the app's end-to-end tests to run with
//! no engine installed, and it records every request so a test can assert what
//! the engine was actually sent (for example the projected text). It is a test
//! double like [`super::ToneSynthesizer`]: it speaks nothing (the audio is a
//! quiet tone, 40 ms per character of the text) and must never be used as a
//! voice. It binds `127.0.0.1` only and keeps at most [`MAX_RECORDED`] requests.

use super::wav;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Requests kept for inspection; older ones are dropped.
pub const MAX_RECORDED: usize = 256;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct FakeBehaviour {
    /// `0` means 200.
    pub query_status: u16,
    /// `0` means 200.
    pub synthesis_status: u16,
    /// Replaces the `audio_query` JSON body.
    pub query_body: Option<Vec<u8>>,
    /// Replaces the `synthesis` WAV body.
    pub wav_body: Option<Vec<u8>>,
    /// Slept before answering every request.
    pub delay: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub method: String,
    /// Path and query exactly as sent.
    pub target: String,
    /// Lower-cased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn route(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    /// A decoded query parameter.
    pub fn query_param(&self, name: &str) -> Option<String> {
        let url = url::Url::parse(&format!("http://fake.invalid{}", self.target)).ok()?;
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

pub struct FakeVoicevoxEngine {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeVoicevoxEngine {
    pub fn start(behaviour: FakeBehaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let behaviour = Arc::new(behaviour);
        let thread = {
            let requests = requests.clone();
            let shutdown = shutdown.clone();
            thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = requests.clone();
                            let behaviour = behaviour.clone();
                            thread::spawn(move || handle(stream, &behaviour, &requests));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        Self {
            addr,
            requests,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Origin of the engine, e.g. `http://127.0.0.1:41234`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("fake engine log").clone()
    }

    /// The `text` of every `audio_query` request, in order.
    pub fn spoken_texts(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter(|request| request.route() == "/audio_query")
            .filter_map(|request| request.query_param("text"))
            .collect()
    }
}

impl Drop for FakeVoicevoxEngine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_REQUEST_BYTES);
    let mut body = buffer[end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Some(RecordedRequest {
        method,
        target,
        headers,
        body,
    })
}

fn reply(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        307 => "Temporary Redirect",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if status == 307 {
        head.push_str("Location: http://127.0.0.1:1/elsewhere\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn handle(mut stream: TcpStream, behaviour: &FakeBehaviour, log: &Mutex<Vec<RecordedRequest>>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    {
        let mut log = log.lock().expect("fake engine log");
        if log.len() >= MAX_RECORDED {
            log.remove(0);
        }
        log.push(request.clone());
    }
    if !behaviour.delay.is_zero() {
        thread::sleep(behaviour.delay);
    }
    if request.method != "POST" {
        reply(&mut stream, 404, "application/json", b"{}");
        return;
    }
    match request.route() {
        "/audio_query" => {
            let status = if behaviour.query_status == 0 {
                200
            } else {
                behaviour.query_status
            };
            if status != 200 {
                reply(
                    &mut stream,
                    status,
                    "application/json",
                    b"{\"detail\":\"scripted\"}",
                );
                return;
            }
            let body = behaviour.query_body.clone().unwrap_or_else(|| {
                let text = request.query_param("text").unwrap_or_default();
                serde_json::to_vec(&json!({
                    "accent_phrases": [],
                    "speedScale": 1.0,
                    "pitchScale": 0.0,
                    "outputSamplingRate": 24000,
                    "outputStereo": true,
                    "kana": text,
                }))
                .expect("query JSON")
            });
            reply(&mut stream, 200, "application/json", &body);
        }
        "/synthesis" => {
            let status = if behaviour.synthesis_status == 0 {
                200
            } else {
                behaviour.synthesis_status
            };
            if status != 200 {
                reply(
                    &mut stream,
                    status,
                    "application/json",
                    b"{\"detail\":\"scripted\"}",
                );
                return;
            }
            let body = behaviour.wav_body.clone().unwrap_or_else(|| {
                let kana = serde_json::from_slice::<Value>(&request.body)
                    .ok()
                    .and_then(|query| query["kana"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                // 40 ms per character at 24 kHz: deterministic, text-shaped.
                let count = kana.chars().count().max(1) * 960;
                let samples: Vec<f32> = (0..count).map(|i| 0.2 * (i as f32 * 0.05).sin()).collect();
                wav::encode_pcm16(&samples, 24_000).expect("fake wav")
            });
            reply(&mut stream, 200, "audio/wav", &body);
        }
        _ => reply(&mut stream, 404, "application/json", b"{}"),
    }
}
