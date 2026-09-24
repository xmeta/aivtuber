use std::error::Error;
use std::fmt;
use std::net::TcpStream;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    message: String,
}

impl TransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
        let (socket, _) = tungstenite::connect(endpoint)
            .map_err(|error| TransportError::new(format!("websocket connect failed: {error}")))?;
        Ok(Box::new(TungsteniteTransport { socket }))
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
            let message = self.socket.read().map_err(|error| {
                TransportError::new(format!("websocket receive failed: {error}"))
            })?;
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
