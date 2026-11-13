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

/// Most extra request headers one call may carry.
pub const MAX_EXTRA_HEADERS: usize = 4;
const MAX_EXTRA_HEADER_NAME_BYTES: usize = 64;
const MAX_EXTRA_HEADER_VALUE_BYTES: usize = 256;
/// Headers the transport owns. An extra header can never replace them, so a
/// configuration value cannot swap the credential or the content negotiation.
const RESERVED_HEADERS: [&str; 9] = [
    "content-type",
    "accept",
    "authorization",
    "proxy-authorization",
    "content-length",
    "host",
    "transfer-encoding",
    "connection",
    "cookie",
];

/// A bounded, validated set of operator-chosen request headers (for example
/// the OpenRouter `HTTP-Referer` / `X-Title` attribution pair).
///
/// Validation happens at construction, so a value that exists is safe to put
/// on the wire: at most [`MAX_EXTRA_HEADERS`] entries, ASCII token names,
/// printable-ASCII values (hence no CR/LF), no duplicates, and no reserved
/// name. `Debug` prints names only; values are never logged.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ExtraHeaders(Vec<(String, String)>);

impl ExtraHeaders {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new(pairs: &[(&str, &str)]) -> Result<Self, String> {
        if pairs.len() > MAX_EXTRA_HEADERS {
            return Err(format!(
                "at most {MAX_EXTRA_HEADERS} extra headers are allowed"
            ));
        }
        let mut headers: Vec<(String, String)> = Vec::with_capacity(pairs.len());
        for (name, value) in pairs {
            let valid_name = !name.is_empty()
                && name.len() <= MAX_EXTRA_HEADER_NAME_BYTES
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
            if !valid_name {
                return Err(
                    "extra header name must be 1-64 ASCII letters, digits, '-' or '_'".to_owned(),
                );
            }
            if RESERVED_HEADERS
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
            {
                return Err(format!(
                    "extra header {name:?} is reserved by the transport"
                ));
            }
            if headers
                .iter()
                .any(|(seen, _)| seen.eq_ignore_ascii_case(name))
            {
                return Err(format!("extra header {name:?} is given twice"));
            }
            let valid_value = !value.is_empty()
                && value.len() <= MAX_EXTRA_HEADER_VALUE_BYTES
                && value.bytes().all(|b| (0x20..=0x7e).contains(&b))
                && value.trim() == *value;
            if !valid_value {
                return Err(format!(
                    "extra header {name:?} value must be 1-{MAX_EXTRA_HEADER_VALUE_BYTES} printable ASCII bytes without surrounding spaces"
                ));
            }
            headers.push(((*name).to_owned(), (*value).to_owned()));
        }
        Ok(Self(headers))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }
}

impl fmt::Debug for ExtraHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(n, _)| n))
            .finish()
    }
}

fn headers_unsupported() -> HttpTransportError {
    HttpTransportError::Unavailable("transport does not support extra request headers".to_owned())
}

pub trait JsonHttpTransport: Send + Sync {
    fn post_json(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError>;

    /// [`post_json`](Self::post_json) plus bounded extra headers. The default
    /// forwards an empty set and refuses a non-empty one, so a transport that
    /// predates headers fails closed instead of silently dropping them.
    fn post_json_with_headers(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        extra_headers: &ExtraHeaders,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError> {
        if extra_headers.is_empty() {
            self.post_json(endpoint, bearer_token, body, timeout)
        } else {
            Err(headers_unsupported())
        }
    }
}

/// A transport that can issue a JSON request with any HTTP method.
///
/// [`JsonHttpTransport`] only speaks POST, which is all a generation provider
/// needs. A memory sidecar also reads, updates and deletes, so the method
/// travels with the request here. The endpoint policy is the same one the
/// POST path already applies: TLS for remote hosts, plain HTTP only for
/// loopback, and never a redirect.
pub trait JsonHttpRequestTransport: Send + Sync {
    fn request_json(
        &self,
        method: &str,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError>;
}

/// Response whose body is read incrementally (server-sent events).
pub struct HttpStreamResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Box<dyn Read + Send>,
}

/// A transport that returns the response body as a bounded reader instead of
/// buffering it (issue #83 S2). The endpoint policy is identical to
/// [`JsonHttpTransport`]; `timeout` bounds the whole exchange including the
/// body, and `max_body_bytes` bounds the decoded stream.
pub trait StreamingHttpTransport: Send + Sync {
    fn post_json_stream(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
        max_body_bytes: u64,
    ) -> Result<HttpStreamResponse, HttpTransportError>;

    /// Streaming counterpart of [`JsonHttpTransport::post_json_with_headers`],
    /// with the same fail-closed default.
    fn post_json_stream_with_headers(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        extra_headers: &ExtraHeaders,
        body: &[u8],
        timeout: Duration,
        max_body_bytes: u64,
    ) -> Result<HttpStreamResponse, HttpTransportError> {
        if extra_headers.is_empty() {
            self.post_json_stream(endpoint, bearer_token, body, timeout, max_body_bytes)
        } else {
            Err(headers_unsupported())
        }
    }
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
        self.request_json("POST", endpoint, bearer_token, body, timeout)
    }

    fn post_json_with_headers(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        extra_headers: &ExtraHeaders,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.send_json("POST", endpoint, bearer_token, extra_headers, body, timeout)
    }
}

impl JsonHttpRequestTransport for UreqJsonTransport {
    fn request_json(
        &self,
        method: &str,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.send_json(
            method,
            endpoint,
            bearer_token,
            &ExtraHeaders::empty(),
            body,
            timeout,
        )
    }
}

impl UreqJsonTransport {
    fn send_json(
        &self,
        method: &str,
        endpoint: &str,
        bearer_token: Option<&str>,
        extra_headers: &ExtraHeaders,
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
        // `force_send_body` lets the bodyless methods share one send path: an
        // empty body is not a body, so GET and DELETE still carry no content.
        let config = match method {
            "GET" => self.agent.get(endpoint).force_send_body().config(),
            "DELETE" => self.agent.delete(endpoint).force_send_body().config(),
            "POST" => self.agent.post(endpoint).config(),
            "PUT" => self.agent.put(endpoint).config(),
            "PATCH" => self.agent.patch(endpoint).config(),
            other => {
                return Err(HttpTransportError::Unavailable(format!(
                    "unsupported HTTP method {other}"
                )));
            }
        };
        let mut config = config
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

        if !body.is_empty() {
            request = request.header("content-type", "application/json");
        }
        if let Some(token) = bearer_token {
            request = request.header("authorization", &format!("Bearer {token}"));
        }
        for (name, value) in extra_headers.iter() {
            request = request.header(name, value);
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

impl StreamingHttpTransport for UreqJsonTransport {
    fn post_json_stream(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        body: &[u8],
        timeout: Duration,
        max_body_bytes: u64,
    ) -> Result<HttpStreamResponse, HttpTransportError> {
        self.post_json_stream_with_headers(
            endpoint,
            bearer_token,
            &ExtraHeaders::empty(),
            body,
            timeout,
            max_body_bytes,
        )
    }

    fn post_json_stream_with_headers(
        &self,
        endpoint: &str,
        bearer_token: Option<&str>,
        extra_headers: &ExtraHeaders,
        body: &[u8],
        timeout: Duration,
        max_body_bytes: u64,
    ) -> Result<HttpStreamResponse, HttpTransportError> {
        if !is_secure_provider_endpoint(endpoint) {
            return Err(HttpTransportError::Unavailable(
                "provider endpoint must use HTTPS or loopback HTTP without embedded credentials"
                    .to_owned(),
            ));
        }
        let mut config = self
            .agent
            .post(endpoint)
            .config()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0);
        if url::Url::parse(endpoint).is_ok_and(|url| is_loopback_host(&url)) {
            config = config.proxy(None);
        }
        let mut request = config.build();
        request = request
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        if let Some(token) = bearer_token {
            request = request.header("authorization", &format!("Bearer {token}"));
        }
        for (name, value) in extra_headers.iter() {
            request = request.header(name, value);
        }
        let response = request.send(body).map_err(map_ureq_error)?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let reader = response
            .into_body()
            .into_with_config()
            .limit(max_body_bytes)
            .reader();
        Ok(HttpStreamResponse {
            status,
            content_type,
            body: Box::new(reader),
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
mod extra_header_tests {
    use super::*;

    #[test]
    fn accepts_a_small_bounded_set_and_hides_values_in_debug() {
        let headers = ExtraHeaders::new(&[
            ("HTTP-Referer", "https://example.invalid/app"),
            ("X-Title", "aivtuber"),
        ])
        .expect("valid");
        assert_eq!(headers.len(), 2);
        let debug = format!("{headers:?}");
        assert!(debug.contains("X-Title") && !debug.contains("aivtuber"));
        assert!(ExtraHeaders::empty().is_empty());
        let names: Vec<String> = (0..MAX_EXTRA_HEADERS).map(|i| format!("X-H{i}")).collect();
        let max: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "v")).collect();
        assert!(ExtraHeaders::new(&max).is_ok());
    }

    #[test]
    fn rejects_injection_overflow_reserved_duplicate_and_non_ascii() {
        let five = [("A", "1"), ("B", "1"), ("C", "1"), ("D", "1"), ("E", "1")];
        assert!(ExtraHeaders::new(&five).is_err(), "more than four");
        for bad_value in [
            "ok\r\nAuthorization: Bearer stolen",
            "line\nbreak",
            "tab\tvalue",
            "caf\u{e9}",
            "",
            " padded ",
        ] {
            assert!(
                ExtraHeaders::new(&[("X-Title", bad_value)]).is_err(),
                "value accepted: {bad_value:?}"
            );
        }
        let long = "v".repeat(MAX_EXTRA_HEADER_VALUE_BYTES + 1);
        assert!(ExtraHeaders::new(&[("X-Title", long.as_str())]).is_err());
        for bad_name in ["", "Bad Name", "X:Title", "X-Ti\r\ntle", "X-T\u{e9}"] {
            assert!(
                ExtraHeaders::new(&[(bad_name, "v")]).is_err(),
                "name accepted: {bad_name:?}"
            );
        }
        for reserved in ["Content-Type", "ACCEPT", "authorization", "Host", "Cookie"] {
            assert!(
                ExtraHeaders::new(&[(reserved, "v")]).is_err(),
                "reserved accepted: {reserved}"
            );
        }
        assert!(ExtraHeaders::new(&[("X-Title", "a"), ("x-title", "b")]).is_err());
    }

    struct PlainTransport;
    impl JsonHttpTransport for PlainTransport {
        fn post_json(
            &self,
            _: &str,
            _: Option<&str>,
            _: &[u8],
            _: Duration,
        ) -> Result<HttpResponse, HttpTransportError> {
            Ok(HttpResponse {
                status: 200,
                body: Vec::new(),
                content_type: None,
            })
        }
    }

    #[test]
    fn a_transport_without_header_support_fails_closed() {
        let headers = ExtraHeaders::new(&[("X-Title", "t")]).expect("valid");
        let timeout = Duration::from_secs(1);
        assert!(
            PlainTransport
                .post_json_with_headers("https://x.invalid/", None, &headers, b"{}", timeout)
                .is_err()
        );
        assert!(
            PlainTransport
                .post_json_with_headers(
                    "https://x.invalid/",
                    None,
                    &ExtraHeaders::empty(),
                    b"{}",
                    timeout
                )
                .is_ok()
        );
    }

    #[test]
    fn extra_headers_reach_the_wire_and_do_not_displace_the_credential() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut received = Vec::new();
            let mut buffer = [0_u8; 2048];
            while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buffer).expect("read");
                received.extend_from_slice(&buffer[..n]);
            }
            // Let the small body arrive so closing does not reset the socket.
            std::thread::sleep(Duration::from_millis(50));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            String::from_utf8_lossy(&received).to_ascii_lowercase()
        });
        let headers = ExtraHeaders::new(&[
            ("HTTP-Referer", "https://example.invalid"),
            ("X-Title", "t"),
        ])
        .expect("valid");
        let response = UreqJsonTransport::default()
            .post_json_with_headers(
                &endpoint,
                Some("tok"),
                &headers,
                b"{}",
                Duration::from_secs(5),
            )
            .expect("response");
        assert_eq!(response.status, 200);
        let head = server.join().expect("server");
        assert!(
            head.contains("http-referer: https://example.invalid"),
            "{head}"
        );
        assert!(head.contains("x-title: t"), "{head}");
        assert!(head.contains("authorization: bearer tok"), "{head}");
        assert!(head.contains("content-type: application/json"), "{head}");
    }
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
        pub headers: Vec<(String, String)>,
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
            self.post_json_with_headers(
                endpoint,
                bearer_token,
                &ExtraHeaders::empty(),
                body,
                timeout,
            )
        }

        fn post_json_with_headers(
            &self,
            endpoint: &str,
            bearer_token: Option<&str>,
            extra_headers: &ExtraHeaders,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, HttpTransportError> {
            self.requests
                .lock()
                .expect("request lock")
                .push(CapturedHttpRequest {
                    endpoint: endpoint.to_owned(),
                    bearer_token: bearer_token.map(str::to_owned),
                    headers: extra_headers
                        .iter()
                        .map(|(n, v)| (n.to_owned(), v.to_owned()))
                        .collect(),
                    body: body.to_vec(),
                    timeout,
                });
            self.response.lock().expect("response lock").clone()
        }
    }
}
