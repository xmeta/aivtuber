//! A tiny loopback HTTP/1.1 file server for the download tests (std only).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Behavior {
    /// path -> body
    pub files: BTreeMap<String, Vec<u8>>,
    /// Serve the whole body with 200 whatever the Range header says.
    pub ignore_range: bool,
    /// The first response promises its full length but the connection ends after this many bytes.
    pub cut_first_after: Option<usize>,
    /// Sleep this long before answering anything.
    pub stall: Option<Duration>,
    /// Answer this many requests with 503 before behaving.
    pub fail_first: usize,
    /// path -> redirect target (302)
    pub redirects: BTreeMap<String, String>,
    /// Lie about the total size in Content-Range.
    pub claim_total: Option<usize>,
}

pub type RequestLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

pub struct Server {
    pub base: String,
    pub requests: RequestLog,
}

impl Server {
    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

pub fn serve(behavior: Behavior) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests: RequestLog = Arc::default();
    let behavior = Arc::new(behavior);
    let served = Arc::new(AtomicUsize::new(0));
    let log = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let behavior = behavior.clone();
            let served = served.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &behavior, &served, &log);
            });
        }
    });
    Server { base, requests }
}

fn handle(
    mut stream: TcpStream,
    b: &Behavior,
    served: &AtomicUsize,
    log: &RequestLog,
) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte)? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let range = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("Range: ")
                .or_else(|| l.strip_prefix("range: "))
        })
        .map(|v| v.trim().to_owned());
    log.lock().unwrap().push((path.clone(), range.clone()));
    let nth = served.fetch_add(1, Ordering::SeqCst);
    if let Some(d) = b.stall {
        std::thread::sleep(d);
    }
    if nth < b.fail_first {
        return respond(&mut stream, "503 Service Unavailable", &[], b"busy", None);
    }
    if let Some(target) = b.redirects.get(&path) {
        return respond(
            &mut stream,
            "302 Found",
            &[("Location", target.clone())],
            b"",
            None,
        );
    }
    let Some(body) = b.files.get(&path) else {
        return respond(&mut stream, "404 Not Found", &[], b"no", None);
    };
    let total = body.len();
    let cut = if nth == b.fail_first {
        b.cut_first_after
    } else {
        None
    };
    let parsed = range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, e)| Some((a.parse::<usize>().ok()?, e.parse::<usize>().ok()?)));
    match parsed {
        Some((start, end)) if !b.ignore_range => {
            if start >= total {
                return respond(&mut stream, "416 Range Not Satisfiable", &[], b"", None);
            }
            let end = end.min(total - 1);
            let claimed = b.claim_total.unwrap_or(total);
            respond(
                &mut stream,
                "206 Partial Content",
                &[("Content-Range", format!("bytes {start}-{end}/{claimed}"))],
                &body[start..=end],
                cut,
            )
        }
        _ => respond(&mut stream, "200 OK", &[], body, cut),
    }
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(&str, String)],
    body: &[u8],
    cut_after: Option<usize>,
) -> std::io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())?;
    match cut_after {
        Some(n) => stream.write_all(&body[..n.min(body.len())])?,
        None => stream.write_all(body)?,
    }
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    aivtuber_setup::download::hex(&Sha256::digest(bytes))
}

pub fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aivtuber-setup-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
