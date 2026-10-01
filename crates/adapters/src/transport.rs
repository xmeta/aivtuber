use std::error::Error;
use std::fmt;
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Upper bound for a single request/response exchange with VTube Studio or OBS.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-read timeout while waiting for the next websocket message.
const READ_TIMEOUT: Duration = Duration::from_millis(250);
/// Upper bound for a single websocket write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for the TCP connect attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for the websocket handshake exchange.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    message: String,
    timed_out: bool,
}

impl TransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            timed_out: false,
        }
    }

    /// Mark the error as a read timeout so callers can retry within their deadline.
    pub fn timed_out(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            timed_out: true,
        }
    }

    pub fn is_timed_out(&self) -> bool {
        self.timed_out
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for TransportError {}

pub trait WebSocketTransport: Send {
    fn send_text(&mut self, text: &str) -> Result<(), TransportError>;
    fn receive_text(&mut self) -> Result<String, TransportError>;
    fn close(&mut self);
}

pub trait WebSocketConnector: Send + Sync {
    fn connect(&self, endpoint: &str) -> Result<Box<dyn WebSocketTransport>, TransportError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TungsteniteConnector;

impl WebSocketConnector for TungsteniteConnector {
    fn connect(&self, endpoint: &str) -> Result<Box<dyn WebSocketTransport>, TransportError> {
        let request = endpoint
            .into_client_request()
            .map_err(|error| TransportError::new(format!("invalid websocket endpoint: {error}")))?;
        let uri = request.uri();
        let host = uri.host().ok_or_else(|| {
            TransportError::new(format!("websocket endpoint has no host: {endpoint}"))
        })?;
        let default_port = if uri.scheme_str() == Some("wss") {
            443
        } else {
            80
        };
        let port = uri.port_u16().unwrap_or(default_port);
        let address = format!("{host}:{port}")
            .to_socket_addrs()
            .map_err(|error| {
                TransportError::new(format!("websocket endpoint failed to resolve: {error}"))
            })?
            .next()
            .ok_or_else(|| {
                TransportError::new(format!("websocket endpoint did not resolve: {endpoint}"))
            })?;
        let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|error| TransportError::new(format!("websocket connect failed: {error}")))?;
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(|error| {
                TransportError::new(format!("failed to set handshake read timeout: {error}"))
            })?;
        let (mut socket, _) = tungstenite::client(request, MaybeTlsStream::Plain(tcp))
            .map_err(|error| TransportError::new(format!("websocket handshake failed: {error}")))?;
        set_stream_timeouts(socket.get_mut())?;
        Ok(Box::new(TungsteniteTransport { socket }))
    }
}

/// Bound every read and write on the established session so a silent peer can never
/// block the caller forever. `receive_text` reports read timeouts via
/// [`TransportError::is_timed_out`] so callers can retry within their own deadline.
fn set_stream_timeouts(stream: &mut MaybeTlsStream<TcpStream>) -> Result<(), TransportError> {
    if let MaybeTlsStream::Plain(tcp) = stream {
        tcp.set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|error| TransportError::new(format!("failed to set read timeout: {error}")))?;
        tcp.set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(|error| {
                TransportError::new(format!("failed to set write timeout: {error}"))
            })?;
    }
    Ok(())
}

/// Receive the next text message, treating transient read timeouts as "nothing yet"
/// until `timeout` has elapsed. The returned error has [`TransportError::is_timed_out`]
/// set once the deadline passes, so callers can distinguish a silent peer from a
/// hard failure.
pub fn receive_text_with_deadline(
    transport: &mut dyn WebSocketTransport,
    timeout: Duration,
) -> Result<String, TransportError> {
    let deadline = Instant::now() + timeout;
    loop {
        match transport.receive_text() {
            Ok(text) => return Ok(text),
            Err(error) if error.is_timed_out() => {
                if Instant::now() >= deadline {
                    return Err(TransportError::timed_out(
                        "websocket peer did not send a message within the deadline",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

struct TungsteniteTransport {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WebSocketTransport for TungsteniteTransport {
    fn send_text(&mut self, text: &str) -> Result<(), TransportError> {
        self.socket
            .send(Message::Text(text.to_owned().into()))
            .map_err(|error| TransportError::new(format!("websocket send failed: {error}")))
    }

    fn receive_text(&mut self) -> Result<String, TransportError> {
        loop {
            let message = self
                .socket
                .read()
                .map_err(|error| classify_read_error(&error))?;
            match message {
                Message::Text(text) => return Ok(text.to_string()),
                Message::Binary(bytes) => {
                    return String::from_utf8(bytes.to_vec()).map_err(|error| {
                        TransportError::new(format!("websocket binary frame is not UTF-8: {error}"))
                    });
                }
                Message::Ping(payload) => {
                    self.socket.send(Message::Pong(payload)).map_err(|error| {
                        TransportError::new(format!("websocket pong failed: {error}"))
                    })?;
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    return Err(TransportError::new(format!("websocket closed: {frame:?}")));
                }
                Message::Frame(_) => {}
            }
        }
    }

    fn close(&mut self) {
        let _ = self.socket.close(None);
    }
}

/// A read timeout is reported as a distinct error kind: the peer may simply have
/// nothing to say yet, and partial frames stay buffered inside the websocket, so
/// retrying the read is always safe.
fn classify_read_error(error: &tungstenite::Error) -> TransportError {
    if let tungstenite::Error::Io(io_error) = error {
        if io_error.kind() == ErrorKind::TimedOut || io_error.kind() == ErrorKind::WouldBlock {
            return TransportError::timed_out(format!("websocket receive timed out: {io_error}"));
        }
    }
    TransportError::new(format!("websocket receive failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StallingTransport;

    impl WebSocketTransport for StallingTransport {
        fn send_text(&mut self, _text: &str) -> Result<(), TransportError> {
            Ok(())
        }

        fn receive_text(&mut self) -> Result<String, TransportError> {
            Err(TransportError::timed_out("stall"))
        }

        fn close(&mut self) {}
    }

    #[test]
    fn receive_text_with_deadline_enforces_the_deadline() {
        let started = Instant::now();
        let error = receive_text_with_deadline(&mut StallingTransport, Duration::from_millis(100))
            .expect_err("a stalling peer must hit the deadline");
        assert!(error.is_timed_out());
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::AdapterClock;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct ScriptHandle {
        sent: Arc<Mutex<Vec<String>>>,
        closed: Arc<Mutex<usize>>,
    }

    impl ScriptHandle {
        pub fn sent(&self) -> Vec<String> {
            self.sent.lock().expect("sent lock").clone()
        }

        pub fn closed_count(&self) -> usize {
            *self.closed.lock().expect("closed lock")
        }
    }

    pub struct Script {
        pub responses: Vec<Result<String, TransportError>>,
        pub handle: ScriptHandle,
    }

    impl Script {
        pub fn new(responses: Vec<Result<String, TransportError>>) -> Self {
            Self {
                responses,
                handle: ScriptHandle::default(),
            }
        }
    }

    #[derive(Clone, Default)]
    pub struct ScriptedConnector {
        attempts: Arc<Mutex<VecDeque<Result<Script, TransportError>>>>,
    }

    impl ScriptedConnector {
        pub fn new(attempts: Vec<Result<Script, TransportError>>) -> Self {
            Self {
                attempts: Arc::new(Mutex::new(attempts.into())),
            }
        }
    }

    impl WebSocketConnector for ScriptedConnector {
        fn connect(&self, _endpoint: &str) -> Result<Box<dyn WebSocketTransport>, TransportError> {
            let script = self
                .attempts
                .lock()
                .expect("attempt lock")
                .pop_front()
                .ok_or_else(|| TransportError::new("no scripted connection attempt"))??;
            Ok(Box::new(ScriptedTransport {
                responses: script.responses.into(),
                handle: script.handle,
            }))
        }
    }

    struct ScriptedTransport {
        responses: VecDeque<Result<String, TransportError>>,
        handle: ScriptHandle,
    }

    impl WebSocketTransport for ScriptedTransport {
        fn send_text(&mut self, text: &str) -> Result<(), TransportError> {
            self.handle
                .sent
                .lock()
                .expect("sent lock")
                .push(text.to_owned());
            Ok(())
        }

        fn receive_text(&mut self) -> Result<String, TransportError> {
            self.responses
                .pop_front()
                .unwrap_or_else(|| Err(TransportError::new("script exhausted")))
        }

        fn close(&mut self) {
            let mut closed = self.handle.closed.lock().expect("closed lock");
            *closed += 1;
        }
    }

    #[derive(Clone, Default)]
    pub struct TestClock {
        now_ms: Arc<Mutex<u64>>,
    }

    impl TestClock {
        pub fn new(now_ms: u64) -> Self {
            Self {
                now_ms: Arc::new(Mutex::new(now_ms)),
            }
        }

        pub fn set(&self, now_ms: u64) {
            *self.now_ms.lock().expect("clock lock") = now_ms;
        }
    }

    impl AdapterClock for TestClock {
        fn now_ms(&self) -> u64 {
            *self.now_ms.lock().expect("clock lock")
        }
    }
}
