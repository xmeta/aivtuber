//! Download behavior against a loopback fake server (issue #294 S2).

mod common;

use aivtuber_setup::download::{DownloadConfig, DownloadError, Outcome, fetch_verified, part_path};
use common::*;
use std::fs;
use std::time::Duration;

fn cfg() -> DownloadConfig {
    DownloadConfig {
        allow_loopback_http: true,
        connect_timeout: Duration::from_secs(2),
        segment_timeout: Duration::from_secs(5),
        segment_bytes: 64 * 1024,
        retries: 2,
        retry_backoff: Duration::from_millis(10),
        max_redirects: 3,
    }
}

fn run(
    cfg: &DownloadConfig,
    server: &Server,
    path: &str,
    dest: &std::path::Path,
    data: &[u8],
) -> Result<Outcome, DownloadError> {
    fetch_verified(
        cfg,
        &server.url(path),
        dest,
        &sha256_hex(data),
        data.len() as u64,
        &mut |_, _| {},
    )
}

fn one_file(data: &[u8]) -> Behavior {
    let mut b = Behavior::default();
    b.files.insert("/f.bin".to_owned(), data.to_vec());
    b
}

#[test]
fn happy_path_downloads_in_range_segments_and_verifies() {
    let data = pattern(200_000, 1);
    let server = serve(one_file(&data));
    let dir = scratch("happy");
    let dest = dir.join("sub").join("f.bin");
    let outcome = run(&cfg(), &server, "/f.bin", &dest, &data).unwrap();
    assert_eq!(outcome, Outcome::Downloaded { resumed_from: 0 });
    assert_eq!(fs::read(&dest).unwrap(), data);
    assert!(!part_path(&dest).exists());
    let log = server.requests.lock().unwrap();
    assert!(
        log.len() >= 4,
        "200 KB in 64 KiB segments needs several requests: {}",
        log.len()
    );
    assert!(
        log.iter()
            .all(|(_, r)| r.as_deref().is_some_and(|r| r.starts_with("bytes=")))
    );
    drop(log);
    // A second run keeps the verified file and makes no request.
    let before = server.count();
    assert_eq!(
        run(&cfg(), &server, "/f.bin", &dest, &data).unwrap(),
        Outcome::AlreadyPresent
    );
    assert_eq!(server.count(), before);
}

#[test]
fn a_checksum_mismatch_deletes_the_partial_and_never_creates_the_file() {
    let served = pattern(50_000, 2);
    let mut wrong = served.clone();
    wrong[10] ^= 0xff;
    let server = serve(one_file(&wrong));
    let dir = scratch("mismatch");
    let dest = dir.join("f.bin");
    let err = run(&cfg(), &server, "/f.bin", &dest, &served).unwrap_err();
    assert!(
        matches!(err, DownloadError::ChecksumMismatch { .. }),
        "{err:?}"
    );
    assert!(!dest.exists());
    assert!(!part_path(&dest).exists());
}

#[test]
fn a_body_larger_than_the_pinned_size_is_refused() {
    // The server's file is bigger than the pin: Content-Range reports the real total.
    let big = pattern(100_000, 3);
    let pinned = &big[..40_000];
    let server = serve(one_file(&big));
    let dir = scratch("cap-range");
    let dest = dir.join("f.bin");
    let err = fetch_verified(
        &cfg(),
        &server.url("/f.bin"),
        &dest,
        &sha256_hex(pinned),
        40_000,
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(
        matches!(err, DownloadError::SizeExceeded { limit: 40_000, .. }),
        "{err:?}"
    );
    assert!(!dest.exists());

    // A server that ignores Range and streams the whole oversized body.
    let mut b = one_file(&big[..60_000]);
    b.ignore_range = true;
    let server = serve(b);
    let dest = dir.join("g.bin");
    let err = fetch_verified(
        &cfg(),
        &server.url("/f.bin"),
        &dest,
        &sha256_hex(&big[..40_000]),
        40_000,
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(matches!(err, DownloadError::SizeExceeded { .. }), "{err:?}");
    assert!(!dest.exists());
    assert!(
        !part_path(&dest).exists(),
        "an oversized partial must not be kept"
    );
}

#[test]
fn a_partial_file_is_resumed_with_a_range_request() {
    let data = pattern(150_000, 4);
    let server = serve(one_file(&data));
    let dir = scratch("resume");
    let dest = dir.join("f.bin");
    fs::write(part_path(&dest), &data[..100_000]).unwrap();
    let outcome = run(&cfg(), &server, "/f.bin", &dest, &data).unwrap();
    assert_eq!(
        outcome,
        Outcome::Downloaded {
            resumed_from: 100_000
        }
    );
    assert_eq!(fs::read(&dest).unwrap(), data);
    let log = server.requests.lock().unwrap();
    assert_eq!(
        log[0].1.as_deref(),
        Some("bytes=100000-149999"),
        "the first request starts after the partial"
    );
}

#[test]
fn a_corrupt_partial_restarts_cleanly_from_zero() {
    let data = pattern(150_000, 5);
    let server = serve(one_file(&data));
    let dir = scratch("bad-partial");
    let dest = dir.join("f.bin");
    fs::write(part_path(&dest), vec![0xAAu8; 100_000]).unwrap();
    let outcome = run(&cfg(), &server, "/f.bin", &dest, &data).unwrap();
    assert!(matches!(
        outcome,
        Outcome::Downloaded {
            resumed_from: 100_000
        }
    ));
    assert_eq!(fs::read(&dest).unwrap(), data);
    let log = server.requests.lock().unwrap();
    assert!(
        log.iter()
            .any(|(_, r)| r.as_deref().is_some_and(|r| r.starts_with("bytes=0-"))),
        "restarted at zero: {log:?}"
    );
}

#[test]
fn an_interrupted_connection_is_retried_from_the_bytes_already_on_disk() {
    let data = pattern(120_000, 6);
    let mut b = one_file(&data);
    b.cut_first_after = Some(1000);
    let server = serve(b);
    let dir = scratch("cut");
    let dest = dir.join("f.bin");
    run(&cfg(), &server, "/f.bin", &dest, &data).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), data);
    let log = server.requests.lock().unwrap();
    assert_eq!(log[0].1.as_deref(), Some("bytes=0-65535"));
    assert_eq!(
        log[1].1.as_deref(),
        Some("bytes=1000-66535"),
        "the retry continues after the 1000 bytes that arrived"
    );
}

#[test]
fn a_stalled_server_times_out_and_the_retries_are_bounded() {
    let data = pattern(10_000, 7);
    let mut b = one_file(&data);
    b.stall = Some(Duration::from_secs(3));
    let server = serve(b);
    let dir = scratch("timeout");
    let dest = dir.join("f.bin");
    let mut c = cfg();
    c.segment_timeout = Duration::from_millis(300);
    c.retries = 1;
    let started = std::time::Instant::now();
    let err = run(&c, &server, "/f.bin", &dest, &data).unwrap_err();
    assert!(matches!(err, DownloadError::Timeout(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(server.count(), 2, "one attempt plus one retry");
    assert!(!dest.exists());
}

#[test]
fn transient_server_errors_are_retried_and_client_errors_are_not() {
    let data = pattern(10_000, 8);
    let mut b = one_file(&data);
    b.fail_first = 2;
    let server = serve(b);
    let dir = scratch("retry503");
    run(&cfg(), &server, "/f.bin", &dir.join("f.bin"), &data).unwrap();
    assert_eq!(server.count(), 3);

    let before = server.count();
    let err = run(&cfg(), &server, "/missing.bin", &dir.join("m.bin"), &data).unwrap_err();
    assert!(
        matches!(err, DownloadError::Http { status: 404, .. }),
        "{err:?}"
    );
    assert_eq!(server.count(), before + 1, "a 404 is final");
}

#[test]
fn redirects_are_followed_but_never_to_plain_http_off_loopback() {
    let data = pattern(10_000, 9);
    let mut b = one_file(&data);
    b.redirects.insert("/old".to_owned(), "/f.bin".to_owned());
    b.redirects
        .insert("/evil".to_owned(), "http://example.com/f.bin".to_owned());
    let server = serve(b);
    let dir = scratch("redirect");
    run(&cfg(), &server, "/old", &dir.join("a.bin"), &data).unwrap();
    let err = run(&cfg(), &server, "/evil", &dir.join("b.bin"), &data).unwrap_err();
    assert!(matches!(err, DownloadError::InsecureUrl(_)), "{err:?}");
}

#[test]
fn production_config_refuses_plain_http_even_on_loopback() {
    let data = pattern(100, 1);
    let server = serve(one_file(&data));
    let dir = scratch("prod");
    let err = run(
        &DownloadConfig::default(),
        &server,
        "/f.bin",
        &dir.join("f.bin"),
        &data,
    )
    .unwrap_err();
    assert!(matches!(err, DownloadError::InsecureUrl(_)));
    assert_eq!(server.count(), 0, "no connection is made");
}
