//! Verified, resumable, bounded HTTPS download.
//!
//! * HTTPS only. Plain HTTP is accepted only for a loopback host and only when
//!   [`DownloadConfig::allow_loopback_http`] is set, which production code never
//!   does (tests use it against a fake server).
//! * Redirects are followed by hand (at most [`DownloadConfig::max_redirects`])
//!   so every hop is checked against the same scheme rule: an HTTPS URL can not
//!   redirect to plain HTTP.
//! * The file arrives in `Range` segments appended to `<dest>.part`. A segment
//!   has its own total timeout, so a stalled connection fails fast and a retry
//!   continues from the bytes already on disk; the manifest `size` is an exact
//!   cap (a body past it is refused, a body short of it is a failure).
//! * Only a file whose SHA-256 equals the pin is ever renamed to its final name.
//!   A digest mismatch deletes the partial file; if the partial pre-existed the
//!   download restarts once from zero, otherwise the mismatch is final.

use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// Test hook: allow `http://` to a loopback host. Never set in production.
    pub allow_loopback_http: bool,
    pub connect_timeout: Duration,
    /// Total time allowed for one `Range` segment (headers and body).
    pub segment_timeout: Duration,
    pub segment_bytes: u64,
    /// Consecutive attempts without progress before giving up.
    pub retries: u32,
    pub retry_backoff: Duration,
    pub max_redirects: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            allow_loopback_http: false,
            connect_timeout: Duration::from_secs(15),
            segment_timeout: Duration::from_secs(120),
            segment_bytes: 32 * 1024 * 1024,
            retries: 4,
            retry_backoff: Duration::from_secs(2),
            max_redirects: 5,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DownloadError {
    InsecureUrl(String),
    Http {
        url: String,
        status: u16,
    },
    Timeout(String),
    Network(String),
    SizeExceeded {
        url: String,
        limit: u64,
    },
    SizeMismatch {
        url: String,
        expected: u64,
        actual: u64,
    },
    RangeUnsupported(String),
    ChecksumMismatch {
        expected: String,
        actual: String,
    },
    Io(String),
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsecureUrl(url) => write!(f, "refusing {url}: only https URLs are allowed"),
            Self::Http { url, status } => write!(f, "{url} answered HTTP {status}"),
            Self::Timeout(url) => write!(f, "timed out talking to {url}"),
            Self::Network(message) => write!(f, "network error: {message}"),
            Self::SizeExceeded { url, limit } => write!(
                f,
                "{url} sent more than the pinned size of {limit} bytes; download refused"
            ),
            Self::SizeMismatch {
                url,
                expected,
                actual,
            } => write!(
                f,
                "{url} is {actual} bytes, the pin says {expected}; download refused"
            ),
            Self::RangeUnsupported(url) => write!(
                f,
                "{url} ignored the Range request for a file larger than one segment"
            ),
            Self::ChecksumMismatch { expected, actual } => {
                write!(f, "sha256 mismatch: got {actual}, pinned {expected}")
            }
            Self::Io(message) => write!(f, "file error: {message}"),
        }
    }
}

impl std::error::Error for DownloadError {}

impl DownloadError {
    fn retryable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::Network(_) => true,
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    AlreadyPresent,
    Downloaded { resumed_from: u64 },
}

/// The URL rule: https always; http only to loopback under the test hook; no
/// embedded credentials.
pub fn check_url(url: &str, cfg: &DownloadConfig) -> Result<url::Url, DownloadError> {
    let parsed = url::Url::parse(url).map_err(|_| DownloadError::InsecureUrl(url.to_owned()))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(DownloadError::InsecureUrl(url.to_owned()));
    }
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if cfg.allow_loopback_http && is_loopback(&parsed) => Ok(parsed),
        _ => Err(DownloadError::InsecureUrl(url.to_owned())),
    }
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// True when `path` is a file of exactly `size` bytes with SHA-256 `sha256`.
pub fn matches_pin(path: &Path, sha256: &str, size: u64) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == size)
        && sha256_file(path).is_ok_and(|actual| actual == sha256)
}

fn io(e: std::io::Error) -> DownloadError {
    DownloadError::Io(e.to_string())
}

/// Downloads `url` to `dest`, verified against `sha256` and `size`.
/// `progress` receives (bytes on disk, total) after each segment.
pub fn fetch_verified(
    cfg: &DownloadConfig,
    url: &str,
    dest: &Path,
    sha256: &str,
    size: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Outcome, DownloadError> {
    check_url(url, cfg)?;
    if matches_pin(dest, sha256, size) {
        return Ok(Outcome::AlreadyPresent);
    }
    if dest.exists() {
        fs::remove_file(dest).map_err(io)?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let part = part_path(dest);
    let mut resumed_from = fs::metadata(&part).map_or(0, |m| m.len());
    if resumed_from > size {
        let _ = fs::remove_file(&part);
        resumed_from = 0;
    }
    let started_from_partial = resumed_from > 0;
    let first_resume = resumed_from;

    let mut restarted = false;
    loop {
        fill_part(cfg, url, &part, size, progress)?;
        let actual = sha256_file(&part).map_err(io)?;
        if actual == sha256 {
            fs::rename(&part, dest).map_err(io)?;
            return Ok(Outcome::Downloaded {
                resumed_from: first_resume,
            });
        }
        let _ = fs::remove_file(&part);
        if started_from_partial && !restarted {
            // The partial file from an earlier run may be the bad part.
            restarted = true;
            continue;
        }
        return Err(DownloadError::ChecksumMismatch {
            expected: sha256.to_owned(),
            actual,
        });
    }
}

/// Appends segments to `part` until it holds exactly `size` bytes.
fn fill_part(
    cfg: &DownloadConfig,
    url: &str,
    part: &Path,
    size: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), DownloadError> {
    let mut failures = 0u32;
    loop {
        let have = fs::metadata(part).map_or(0, |m| m.len());
        if have == size {
            return Ok(());
        }
        if have > size {
            let _ = fs::remove_file(part);
            continue;
        }
        let end = (have + cfg.segment_bytes).min(size) - 1;
        match fetch_segment(cfg, url, part, have, end, size) {
            Ok(wrote) => {
                let now = have + wrote;
                progress(now, size);
                if wrote > 0 {
                    failures = 0;
                } else {
                    failures += 1;
                }
            }
            Err(error) => {
                let progressed = fs::metadata(part).map_or(0, |m| m.len()) > have;
                if matches!(error, DownloadError::Http { status: 416, .. }) {
                    // Our partial does not fit the remote file: start over.
                    let _ = fs::remove_file(part);
                    failures += 1;
                } else if error.retryable() {
                    if progressed {
                        failures = 0;
                    } else {
                        failures += 1;
                    }
                } else {
                    return Err(error);
                }
                if failures > cfg.retries {
                    return Err(error);
                }
                std::thread::sleep(cfg.retry_backoff);
            }
        }
        if failures > cfg.retries {
            return Err(DownloadError::Network("server returned no data".to_owned()));
        }
    }
}

/// One GET for bytes `start..=end`, appended to `part`. Returns bytes written.
fn fetch_segment(
    cfg: &DownloadConfig,
    url: &str,
    part: &Path,
    start: u64,
    end: u64,
    size: u64,
) -> Result<u64, DownloadError> {
    let mut current = url.to_owned();
    for _hop in 0..=cfg.max_redirects {
        let parsed = check_url(&current, cfg)?;
        let mut builder = ureq::Agent::config_builder()
            .timeout_connect(Some(cfg.connect_timeout))
            .timeout_global(Some(cfg.segment_timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .accept_encoding("identity");
        if is_loopback(&parsed) {
            // Loopback must stay direct whatever HTTP_PROXY says.
            builder = builder.proxy(None);
        }
        let agent: ureq::Agent = builder.build().into();
        let mut response = agent
            .get(parsed.as_str())
            .header("Range", format!("bytes={start}-{end}"))
            .call()
            .map_err(|e| map_ureq(e, &current))?;
        let status = response.status().as_u16();
        match status {
            301 | 302 | 303 | 307 | 308 => {
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| DownloadError::Network("redirect without Location".into()))?;
                current = parsed
                    .join(location)
                    .map_err(|_| DownloadError::InsecureUrl(location.to_owned()))?
                    .to_string();
                continue;
            }
            206 => {
                let range = response
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range);
                let Some((first, last, total)) = range else {
                    return Err(DownloadError::Network("206 without a Content-Range".into()));
                };
                if let Some(total) = total.filter(|t| *t != size) {
                    return Err(if total > size {
                        DownloadError::SizeExceeded {
                            url: current,
                            limit: size,
                        }
                    } else {
                        DownloadError::SizeMismatch {
                            url: current,
                            expected: size,
                            actual: total,
                        }
                    });
                }
                if first != start || last < start || last > end {
                    return Err(DownloadError::Network(format!(
                        "Content-Range {first}-{last}/{total:?} does not match the request {start}-{end}/{size}"
                    )));
                }
                let expected = last - start + 1;
                return append(&mut response, part, start, expected, &current);
            }
            200 => {
                // The server ignored Range and is sending the whole file.
                if start != 0 {
                    return Err(DownloadError::RangeUnsupported(current));
                }
                if size > cfg.segment_bytes {
                    return Err(DownloadError::RangeUnsupported(current));
                }
                return append(&mut response, part, 0, size, &current);
            }
            status => {
                return Err(DownloadError::Http {
                    url: current,
                    status,
                });
            }
        }
    }
    Err(DownloadError::Network(format!(
        "more than {} redirects",
        cfg.max_redirects
    )))
}

/// Copies at most `expected` bytes of the body to `part` at offset `start`.
/// Any byte beyond `expected` is a size violation and the part is removed.
fn append(
    response: &mut ureq::http::Response<ureq::Body>,
    part: &Path,
    start: u64,
    expected: u64,
    url: &str,
) -> Result<u64, DownloadError> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)
        .map_err(io)?;
    file.set_len(start).map_err(io)?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).map_err(io)?;
    let mut reader = response.body_mut().as_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                return Err(if e.kind() == std::io::ErrorKind::TimedOut {
                    DownloadError::Timeout(url.to_owned())
                } else if let Some(inner) =
                    e.get_ref().and_then(|i| i.downcast_ref::<ureq::Error>())
                    && matches!(inner, ureq::Error::Timeout(_))
                {
                    DownloadError::Timeout(url.to_owned())
                } else {
                    DownloadError::Network(e.to_string())
                });
            }
        };
        if written + n as u64 > expected {
            drop(file);
            let _ = fs::remove_file(part);
            return Err(DownloadError::SizeExceeded {
                url: url.to_owned(),
                limit: start + expected,
            });
        }
        file.write_all(&buf[..n]).map_err(io)?;
        written += n as u64;
    }
    file.flush().map_err(io)?;
    if written < expected {
        // Keep what arrived: the next attempt resumes after it.
        return Err(DownloadError::Network(format!(
            "connection ended after {written} of {expected} bytes"
        )));
    }
    Ok(written)
}

fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (first, last) = range.split_once('-')?;
    let total = if total == "*" {
        None
    } else {
        Some(total.parse().ok()?)
    };
    Some((first.parse().ok()?, last.parse().ok()?, total))
}

fn map_ureq(error: ureq::Error, url: &str) -> DownloadError {
    match error {
        ureq::Error::Timeout(_) => DownloadError::Timeout(url.to_owned()),
        other => DownloadError::Network(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_only_and_loopback_http_only_under_the_test_hook() {
        let prod = DownloadConfig::default();
        assert!(check_url("https://example.com/a", &prod).is_ok());
        assert!(check_url("http://example.com/a", &prod).is_err());
        assert!(check_url("http://127.0.0.1:1/a", &prod).is_err());
        assert!(check_url("ftp://example.com/a", &prod).is_err());
        assert!(check_url("https://user:pw@example.com/a", &prod).is_err());
        let test = DownloadConfig {
            allow_loopback_http: true,
            ..DownloadConfig::default()
        };
        assert!(check_url("http://127.0.0.1:1/a", &test).is_ok());
        assert!(check_url("http://localhost:1/a", &test).is_ok());
        assert!(check_url("http://example.com/a", &test).is_err());
        assert!(check_url("http://127.0.0.1.example.com/a", &test).is_err());
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 0-9/100"),
            Some((0, 9, Some(100)))
        );
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, 9, None)));
        assert_eq!(parse_content_range("items 0-9/100"), None);
    }
}
