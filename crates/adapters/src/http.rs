use std::error::Error;
use std::fmt;
use std::io::Read;
use std::time::Duration;

/// Maximum decoded JSON response payload accepted from a generation provider.
/// The Responses API and normalized TTS return small structured metadata, not
/// media bytes; a 1 MiB bound leaves ample room for errors and metadata.
const MAX_PROVIDER_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpTransportError {
    Timeout,
    Unavailable(String),
}

impl fmt::Display for HttpTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("HTTP transport timeout"),
            Self::Unavailable(message) => write!(f, "HTTP transport unavailable: {message}"),
        }
    }
}

impl Error for HttpTransportError {}

pub trait JsonHttpTransport: Send + Sync {
    fn post_json(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError>;
}

#[derive(Clone)]
pub struct UreqJsonTransport {
    agent: ureq::Agent,
}

impl Default for UreqJsonTransport {
    fn default() -> Self {
        Self {
            agent: ureq::Agent::new_with_defaults(),
        }
    }
}

impl fmt::Debug for UreqJsonTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UreqJsonTransport")
    }
}

impl JsonHttpTransport for UreqJsonTransport {
    fn post_json(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError> {
        // Validate at the actual network boundary as well as in adapter
        // configuration. This transport is public and may be called directly
        // by future providers with private JSON or bearer credentials.
        if !is_secure_provider_endpoint(endpoint) {
            return Err(HttpTransportError::Unavailable(
                "provider endpoint must use HTTPS or loopback HTTP without embedded credentials"
                    .to_owned(),
            ));
        }
        let mut config = self
            .agent
            .post(endpoint)
            .header("content-type", "application/json")
            .config()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            // Never follow provider redirects: a 307/308 could otherwise
            // forward curated private input to a different endpoint, even
            // when the Authorization header is stripped by ureq.
            .max_redirects(0);

        // ureq's agent inherits proxy environment variables by default.
        // Loopback HTTP(S) endpoints must connect directly, regardless of
        // HTTP_PROXY/ALL_PROXY/NO_PROXY, to keep local prompts and keys local.
        // Keep the configured proxy for non-loopback HTTPS providers.
        if url::Url::parse(endpoint).is_ok_and(|url| is_loopback_host(&url)) {
            config = config.proxy(None);
        }
        let mut request = config.build();

        if let Some(token) = bearer_token {
            request = request.header("authorization", &format!("Bearer {token}"));
        }

        let mut response = request.send(body).map_err(map_ureq_error)?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // ureq applies Body::limit() before content decompression. Also cap
        // the outer decoded reader: a tiny gzip response must not expand
        // into an unbounded allocation. Read cap+1 to detect overflow.
        let reader = response
            .body_mut()
            .with_config()
            .limit(MAX_PROVIDER_RESPONSE_BYTES + 1)
            .reader();
        let mut body = Vec::new();
        reader
            .take(MAX_PROVIDER_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|error| HttpTransportError::Unavailable(error.to_string()))?;
        if body.len() as u64 > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(HttpTransportError::Unavailable(format!(
                "provider HTTP response exceeded {MAX_PROVIDER_RESPONSE_BYTES} byte limit"
            )));
        }
        Ok(HttpResponse {
            status,
            body,
            content_type,
        })
    }
}

fn map_ureq_error(error: ureq::Error) -> HttpTransportError {
    match error {
        ureq::Error::Timeout(_) => HttpTransportError::Timeout,
        other => HttpTransportError::Unavailable(other.to_string()),
    }
}

/// TLS is required for remote provider requests, since both thinking and TTS
/// can attach bearer credentials and private curated content. Parse the actual
/// URL authority instead of trusting a string prefix such as "localhost".
pub(crate) fn is_secure_provider_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.has_host(),
        "http" => is_loopback_host(&url),
        _ => false,
    }
}

fn is_loopback_host(url: &url::Url) -> bool {
    url.host().is_some_and(|host| match host {
        url::Host::Domain(domain) => domain == "localhost",
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    })
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn validates_actual_scheme_and_host_with_no_embedded_credentials() {
        for valid in [
            "https://api.example.invalid/v1/responses",
            "http://localhost:1234/v1/responses",
            "http://127.0.0.1:1234/v1/responses",
            "http://[::1]:1234/v1/responses",
        ] {
            assert!(is_secure_provider_endpoint(valid), "rejected: {valid}");
        }
        for invalid in [
            "http://localhost.evil.invalid/v1/responses",
            "http://127.0.0.1.evil.invalid/v1/responses",
            "http://localhost@evil.invalid/v1/responses",
            "http://127.0.0.1@evil.invalid/v1/responses",
            "http://192.168.0.5/v1/responses",
            "ftp://localhost/v1/responses",
            "https://",
            "https://user:pass@api.example.invalid/v1/responses",
            "not a url",
        ] {
            assert!(!is_secure_provider_endpoint(invalid), "allowed: {invalid}");
        }
    }
}

#[cfg(test)]
mod transport_limits_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    #[test]
    fn direct_transport_rejects_insecure_endpoints_before_network_io() {
        // UreqJsonTransport is exported; callers must not be able to bypass
        // the URL policy used by the Thinking and TTS config constructors.
        for endpoint in [
            "http://192.0.2.1:9/private",
            "http://localhost.attacker.invalid/private",
            "http://user:pass@127.0.0.1:9/private",
            "https://user:pass@api.example.invalid/private",
            "ftp://localhost:9/private",
            "https://",
        ] {
            let error = UreqJsonTransport::default()
                .post_json(
                    endpoint,
                    Some("sensitive-test-token"),
                    b"{\"private\":true}",
                    Duration::from_millis(50),
                )
                .expect_err("insecure endpoint must fail without network I/O");
            assert!(
                error.to_string().contains("HTTPS or loopback HTTP"),
                "unexpected response for {endpoint}: {error}"
            );
        }
    }

    fn serve_once(
        response: Vec<u8>,
        status: &str,
        extra_headers: &str,
    ) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let endpoint = format!(
            "http://{}/v1/responses",
            listener.local_addr().expect("address")
        );
        let status = status.to_owned();
        let headers = extra_headers.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accepted request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            // Consume the complete request before closing the socket: leaving
            // unread POST bytes would trigger TCP reset and produce a false
            // "Peer disconnected" failure for otherwise valid responses.
            let mut received = Vec::new();
            let mut buffer = [0_u8; 2048];
            loop {
                let n = stream.read(&mut buffer).expect("read request");
                assert!(n > 0, "request closed before body was received");
                received.extend_from_slice(&buffer[..n]);
                assert!(
                    received.len() < 16 * 1024,
                    "unexpectedly large test request"
                );
                let Some(headers_end) = received.windows(4).position(|chunk| chunk == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&received[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .map(str::to_owned)
                    })
                    .and_then(|value| value.parse::<usize>().ok())
                    .expect("request content length");
                if received.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                response.len()
            );
            stream.write_all(head.as_bytes()).expect("response headers");
            // When the client refuses an oversized response it may close early.
            let _ = stream.write_all(&response);
        });
        (endpoint, handle)
    }

    #[test]
    fn loopback_provider_bypasses_even_an_explicit_http_proxy() {
        // A default ureq agent inherits HTTP_PROXY/ALL_PROXY from the environment.
        // A loopback inference request must not leak a bearer token or input
        // through such a proxy, including when NO_PROXY is absent.
        let proxy = ureq::Proxy::new("http://127.0.0.1:9").expect("proxy URI");
        let agent = ureq::Agent::config_builder()
            .proxy(Some(proxy))
            .build()
            .into();
        let transport = UreqJsonTransport { agent };
        let (endpoint, server) = serve_once(
            b"{}".to_vec(),
            "200 OK",
            "Content-Type: application/json\r\n",
        );
        let response = transport
            .post_json(
                &endpoint,
                Some("loopback-only-secret"),
                b"{\"input\":\"private\"}",
                Duration::from_secs(2),
            )
            .expect("loopback must connect directly even with an HTTP proxy configured");
        server.join().expect("server");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{}");
    }

    #[test]
    fn rejects_responses_above_the_provider_byte_budget() {
        let payload = vec![b'x'; MAX_PROVIDER_RESPONSE_BYTES as usize + 1];
        let (endpoint, server) =
            serve_once(payload, "200 OK", "Content-Type: application/json\r\n");
        let result =
            UreqJsonTransport::default().post_json(&endpoint, None, b"{}", Duration::from_secs(5));
        server.join().expect("server");
        let error = result.expect_err("oversized response must fail closed");
        assert!(
            error.to_string().contains("limit"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn compressed_provider_response_cannot_exceed_decoded_byte_budget() {
        // 1,051 compressed bytes expand to 1 MiB + 1. The ureq input
        // limiter alone cannot bound the decompressed Vec allocation.
        let compressed = include_bytes!("../tests/fixtures/gzip-over-decoded-budget.gz");
        assert!(compressed.len() < MAX_PROVIDER_RESPONSE_BYTES as usize);
        let (endpoint, server) = serve_once(
            compressed.to_vec(),
            "200 OK",
            "Content-Encoding: gzip\r\nContent-Type: application/json\r\n",
        );
        let result =
            UreqJsonTransport::default().post_json(&endpoint, None, b"{}", Duration::from_secs(5));
        server.join().expect("server");
        let error = result.expect_err("decompressed response must fail closed");
        assert!(
            error.to_string().contains("exceeded"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn accepts_exactly_the_provider_byte_budget() {
        let payload = vec![b'x'; MAX_PROVIDER_RESPONSE_BYTES as usize];
        let (endpoint, server) =
            serve_once(payload, "200 OK", "Content-Type: application/json\r\n");
        let result =
            UreqJsonTransport::default().post_json(&endpoint, None, b"{}", Duration::from_secs(5));
        server.join().expect("server");
        assert_eq!(
            result.expect("at-budget response").body.len(),
            MAX_PROVIDER_RESPONSE_BYTES as usize
        );
    }

    #[test]
    fn does_not_follow_provider_redirects_with_private_request_body() {
        let (endpoint, server) = serve_once(
            Vec::new(),
            "307 Temporary Redirect",
            "Location: http://127.0.0.1:9/should-not-be-followed\r\n",
        );
        let result = UreqJsonTransport::default().post_json(
            &endpoint,
            Some("private-test-token"),
            b"{\"private\":true}",
            Duration::from_secs(3),
        );
        server.join().expect("server");
        assert_eq!(result.expect("do not follow redirect").status, 307);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CapturedHttpRequest {
        pub endpoint: String,
        pub bearer_token: Option<String>,
        pub body: Vec<u8>,
        pub timeout: Duration,
    }

    #[derive(Clone)]
    pub struct MockHttpTransport {
        response: Arc<Mutex<Result<HttpResponse, HttpTransportError>>>,
        requests: Arc<Mutex<Vec<CapturedHttpRequest>>>,
    }

    impl MockHttpTransport {
        pub fn new(response: Result<HttpResponse, HttpTransportError>) -> Self {
            Self {
                response: Arc::new(Mutex::new(response)),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn requests(&self) -> Vec<CapturedHttpRequest> {
            self.requests.lock().expect("request lock").clone()
        }
    }

    impl JsonHttpTransport for MockHttpTransport {
        fn post_json(
            &self,
            endpoint: &str,
            bearer_token: Option<&str>,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, HttpTransportError> {
            self.requests
                .lock()
                .expect("request lock")
                .push(CapturedHttpRequest {
                    endpoint: endpoint.to_owned(),
                    bearer_token: bearer_token.map(str::to_owned),
                    body: body.to_vec(),
                    timeout,
                });
            self.response.lock().expect("response lock").clone()
        }
    }
}
