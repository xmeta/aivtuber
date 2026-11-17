//! A fake Irodori-TTS-Server on loopback, for tests (issue #411).
//!
//! It speaks just enough of the server's HTTP API (`POST /v1/audio/speech`) for
//! the bridge and the app's end-to-end tests to run with no server, model or
//! GPU installed, and it records every request so a test can assert what the
//! server was actually sent (the pinning fields, the projected text). A test
//! double like [`super::voicevox_fake`]: the audio is a quiet tone, 40 ms per
//! character at 48 kHz, whose pitch depends on the request's seed, so the same
//! text and seed give identical bytes and another seed gives another "voice".
//! It must never be used as a voice. It binds `127.0.0.1` only.

use super::voicevox_fake::{MAX_RECORDED, RecordedRequest, read_request, reply};
use super::wav;
use serde_json::Value;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct FakeBehaviour {
    /// `0` means 200.
    pub status: u16,
    /// Replaces the WAV body (a 200 answer).
    pub wav_body: Option<Vec<u8>>,
    /// Slept before answering every request.
    pub delay: Duration,
}

pub struct FakeIrodoriServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeIrodoriServer {
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

    /// Origin of the server, e.g. `http://127.0.0.1:41234`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("fake server log").clone()
    }

    /// The `input` of every speech request, in order.
    pub fn spoken_texts(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|request| {
                serde_json::from_slice::<Value>(&request.body)
                    .ok()
                    .and_then(|body| body["input"].as_str().map(str::to_owned))
            })
            .collect()
    }
}

impl Drop for FakeIrodoriServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle(mut stream: TcpStream, behaviour: &FakeBehaviour, log: &Mutex<Vec<RecordedRequest>>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    {
        let mut log = log.lock().expect("fake server log");
        if log.len() >= MAX_RECORDED {
            log.remove(0);
        }
        log.push(request.clone());
    }
    if !behaviour.delay.is_zero() {
        thread::sleep(behaviour.delay);
    }
    if request.method != "POST" || request.route() != "/v1/audio/speech" {
        reply(&mut stream, 404, "application/json", b"{}");
        return;
    }
    if behaviour.status != 0 && behaviour.status != 200 {
        reply(
            &mut stream,
            behaviour.status,
            "application/json",
            b"{\"detail\":\"scripted\"}",
        );
        return;
    }
    let body = behaviour.wav_body.clone().unwrap_or_else(|| {
        let parsed = serde_json::from_slice::<Value>(&request.body).unwrap_or_default();
        let text = parsed["input"].as_str().unwrap_or_default();
        let seed = parsed["irodori"]["seed"].as_u64().unwrap_or(0);
        let step = 0.03 + (seed % 97) as f32 * 0.001;
        let count = text.chars().count().max(1) * 1_920;
        let samples: Vec<f32> = (0..count).map(|i| 0.2 * (i as f32 * step).sin()).collect();
        wav::encode_pcm16(&samples, 48_000).expect("fake wav")
    });
    reply(&mut stream, 200, "audio/wav", &body);
}
