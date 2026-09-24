use std::error::Error;
use std::fmt;
use std::time::Duration;

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
        let mut request = self
            .agent
            .post(endpoint)
            .header("content-type", "application/json")
            .config()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();

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
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|error| HttpTransportError::Unavailable(error.to_string()))?;
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
