//! A fake Aivis Cloud API on loopback, for tests (issue #327).
//!
//! It answers `POST /v1/tts/synthesize` with a quiet-tone WAV (40 ms per
//! character of the request's `text`, 44.1 kHz mono) or a scripted status/body,
//! `GET /v1/users/me` (the key check) and `GET /v1/aivm-models/search` (a
//! scripted JSON body), and records every request (headers and body) so a test can assert what the
//! service was actually sent. A test double like [`super::voicevox_fake`]: it
//! speaks nothing and must never be used as a voice. Binds `127.0.0.1` only.

use super::voicevox_fake::{MAX_RECORDED, RecordedRequest, read_request, reply};
use super::wav;
use serde_json::Value;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct FakeAivisBehaviour {
    /// `0` means 200.
    pub status: u16,
    /// Replaces the response body (for a 200 it is served as `audio/wav`).
    pub body: Option<Vec<u8>>,
    /// Slept before answering every request.
    pub delay: Duration,
    /// When set, a request whose `Authorization: Bearer` differs answers 401
    /// (synthesis and the key check).
    pub accept_key: Option<String>,
    /// Status of `GET /v1/users/me` for an accepted key; `0` means 200.
    pub me_status: u16,
    /// Status of `GET /v1/aivm-models/search`; `0` means 200.
    pub search_status: u16,
    /// Body of the search answer; default is an empty page.
    pub search_body: Option<Vec<u8>>,
}

pub struct FakeAivisCloud {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeAivisCloud {
    pub fn start(behaviour: FakeAivisBehaviour) -> Self {
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

    /// Origin of the fake, e.g. `http://127.0.0.1:41234`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("fake api log").clone()
    }

    /// The `text` of every synthesis request, in order.
    pub fn spoken_texts(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter(|request| request.route() == "/v1/tts/synthesize")
            .filter_map(|request| {
                serde_json::from_slice::<Value>(&request.body)
                    .ok()?
                    .get("text")?
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }
}

impl Drop for FakeAivisCloud {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle(
    mut stream: TcpStream,
    behaviour: &FakeAivisBehaviour,
    log: &Mutex<Vec<RecordedRequest>>,
) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    {
        let mut log = log.lock().expect("fake api log");
        if log.len() >= MAX_RECORDED {
            log.remove(0);
        }
        log.push(request.clone());
    }
    if !behaviour.delay.is_zero() {
        thread::sleep(behaviour.delay);
    }
    let key_accepted = behaviour.accept_key.as_ref().is_none_or(|expected| {
        request.header("authorization").as_deref() == Some(format!("Bearer {expected}").as_str())
    });
    match (request.method.as_str(), request.route()) {
        ("GET", "/v1/users/me") => {
            let status = match (key_accepted, behaviour.me_status) {
                (false, _) => 401,
                (true, 0) => 200,
                (true, status) => status,
            };
            reply(
                &mut stream,
                status,
                "application/json",
                b"{\"handle\":\"fake\"}",
            );
            return;
        }
        ("GET", "/v1/aivm-models/search") => {
            let status = if behaviour.search_status == 0 {
                200
            } else {
                behaviour.search_status
            };
            let body = behaviour
                .search_body
                .clone()
                .unwrap_or_else(|| b"{\"total\":0,\"aivm_models\":[]}".to_vec());
            reply(&mut stream, status, "application/json", &body);
            return;
        }
        ("POST", "/v1/tts/synthesize") => {}
        _ => {
            reply(&mut stream, 404, "application/json", b"{}");
            return;
        }
    }
    if !key_accepted {
        reply(
            &mut stream,
            401,
            "application/json",
            b"{\"detail\":\"bad key\"}",
        );
        return;
    }
    let status = if behaviour.status == 0 {
        200
    } else {
        behaviour.status
    };
    if status != 200 {
        let body = behaviour
            .body
            .clone()
            .unwrap_or_else(|| b"{\"detail\":\"scripted\"}".to_vec());
        reply(&mut stream, status, "application/json", &body);
        return;
    }
    let body = behaviour.body.clone().unwrap_or_else(|| {
        let text = serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body["text"].as_str().map(str::to_owned))
            .unwrap_or_default();
        // 40 ms per character at 44.1 kHz: deterministic, text-shaped.
        let count = text.chars().count().max(1) * 1_764;
        let samples: Vec<f32> = (0..count).map(|i| 0.2 * (i as f32 * 0.05).sin()).collect();
        let mut bytes = wav::encode_pcm16(&samples, 44_100).expect("fake wav");
        // Like the live service (streamed from an encoder that cannot seek
        // back): the RIFF and data sizes are the "unknown length" sentinel.
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes
    });
    reply(&mut stream, 200, "audio/wav", &body);
}
