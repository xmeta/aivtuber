use crate::{
    AdapterClock, AdapterEvent, ReconnectPolicy, ReconnectState, SecretString,
    TungsteniteConnector, WebSocketConnector, WebSocketTransport, default_clock,
};
use aivtuber_domain::{
    AuthorizedStreamAction, EngineError, EngineErrorKind, EngineFuture, StreamAdapter,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ObsWebSocketConfig {
    pub endpoint: String,
    pub password: Option<SecretString>,
    pub rpc_version: u64,
    /// Omit to use obs-websocket's default event subscriptions.
    pub event_subscriptions: Option<u64>,
    pub reconnect: ReconnectPolicy,
}

impl Default for ObsWebSocketConfig {
    fn default() -> Self {
        Self {
            endpoint: "ws://127.0.0.1:4455".to_owned(),
            password: None,
            rpc_version: 1,
            event_subscriptions: None,
            reconnect: ReconnectPolicy::default(),
        }
    }
}

impl fmt::Debug for ObsWebSocketConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObsWebSocketConfig")
            .field("endpoint", &self.endpoint)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("rpc_version", &self.rpc_version)
            .field("event_subscriptions", &self.event_subscriptions)
            .field("reconnect", &self.reconnect)
            .finish()
    }
}

struct ObsState {
    transport: Option<Box<dyn WebSocketTransport>>,
    reconnect: ReconnectState,
    request_sequence: u64,
    event_subscriptions: Option<u64>,
    events: VecDeque<AdapterEvent>,
}

impl ObsState {
    fn new(event_subscriptions: Option<u64>) -> Self {
        Self {
            transport: None,
            reconnect: ReconnectState::default(),
            request_sequence: 0,
            event_subscriptions,
            events: VecDeque::new(),
        }
    }

    fn next_request_id(&mut self) -> String {
        self.request_sequence = self.request_sequence.saturating_add(1);
        format!("aivtuber-obs-{}", self.request_sequence)
    }
}

pub struct ObsWebSocketAdapter {
    config: ObsWebSocketConfig,
    connector: Arc<dyn WebSocketConnector>,
    clock: Arc<dyn AdapterClock>,
    state: Mutex<ObsState>,
}

impl fmt::Debug for ObsWebSocketAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObsWebSocketAdapter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ObsWebSocketAdapter {
    pub fn new(config: ObsWebSocketConfig) -> Result<Self, EngineError> {
        Self::with_dependencies(config, Arc::new(TungsteniteConnector), default_clock())
    }

    pub fn with_dependencies(
        config: ObsWebSocketConfig,
        connector: Arc<dyn WebSocketConnector>,
        clock: Arc<dyn AdapterClock>,
    ) -> Result<Self, EngineError> {
        if config.endpoint.trim().is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "OBS websocket endpoint must not be empty",
            ));
        }
        if config.rpc_version == 0 {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "OBS rpc_version must be greater than zero",
            ));
        }
        let subscriptions = config.event_subscriptions;
        Ok(Self {
            config,
            connector,
            clock,
            state: Mutex::new(ObsState::new(subscriptions)),
        })
    }

    /// Establish the websocket session and complete OBS Hello/Identify/Identified.
    pub fn connect(&self) -> Result<(), EngineError> {
        let mut state = self.lock_state()?;
        self.ensure_connected(&mut state)
    }

    pub fn execute_sync(&self, action: &AuthorizedStreamAction) -> Result<(), EngineError> {
        let (request_type, request_data) = map_stream_action(action)?;
        let mut state = self.lock_state()?;
        self.ensure_connected(&mut state)?;
        self.request(&mut state, request_type, request_data)?;
        Ok(())
    }

    pub fn set_event_subscriptions(&self, subscriptions: u64) -> Result<(), EngineError> {
        let mut state = self.lock_state()?;
        state.event_subscriptions = Some(subscriptions);
        if let Some(transport) = state.transport.as_mut() {
            let text = serde_json::to_string(&json!({
                "op": 3,
                "d": { "eventSubscriptions": subscriptions }
            }))
            .map_err(|error| {
                engine_error(
                    EngineErrorKind::InvalidRequest,
                    format!("failed to encode OBS Reidentify: {error}"),
                )
            })?;
            if let Err(error) = transport.send_text(&text) {
                self.disconnect(&mut state);
                return Err(engine_error(
                    EngineErrorKind::Unavailable,
                    format!("OBS Reidentify send failed: {error}"),
                ));
            }
        }
        Ok(())
    }

    pub fn poll_event(&self) -> Result<Option<AdapterEvent>, EngineError> {
        let mut state = self.lock_state()?;
        if let Some(event) = state.events.pop_front() {
            return Ok(Some(event));
        }
        self.ensure_connected(&mut state)?;

        let text = receive_from_state(&mut state).map_err(|message| {
            self.disconnect(&mut state);
            engine_error(EngineErrorKind::Unavailable, message)
        })?;
        let message: Value = serde_json::from_str(&text).map_err(|error| {
            engine_error(
                EngineErrorKind::Backend,
                format!("invalid OBS websocket event JSON: {error}"),
            )
        })?;
        Ok(parse_obs_event(&message))
    }

    pub fn next_retry_at_ms(&self) -> Result<u64, EngineError> {
        Ok(self.lock_state()?.reconnect.next_retry_at_ms())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, ObsState>, EngineError> {
        self.state.lock().map_err(|_| {
            engine_error(
                EngineErrorKind::Backend,
                "OBS adapter state lock was poisoned",
            )
        })
    }

    fn ensure_connected(&self, state: &mut ObsState) -> Result<(), EngineError> {
        if state.transport.is_some() {
            return Ok(());
        }

        let now_ms = self.clock.now_ms();
        if !state.reconnect.can_attempt(now_ms) {
            return Err(engine_error(
                EngineErrorKind::Unavailable,
                format!(
                    "OBS reconnect backoff active until {}ms",
                    state.reconnect.next_retry_at_ms()
                ),
            ));
        }

        let mut transport = self
            .connector
            .connect(&self.config.endpoint)
            .map_err(|error| {
                state.reconnect.failed(now_ms, self.config.reconnect);
                engine_error(
                    EngineErrorKind::Unavailable,
                    format!("OBS websocket connection failed: {error}"),
                )
            })?;

        if let Err(error) = self.identify(state, transport.as_mut()) {
            transport.close();
            state.reconnect.failed(now_ms, self.config.reconnect);
            return Err(error);
        }

        state.transport = Some(transport);
        state.reconnect.connected();
        Ok(())
    }

    fn identify(
        &self,
        state: &mut ObsState,
        transport: &mut dyn WebSocketTransport,
    ) -> Result<(), EngineError> {
        let hello_text = transport
            .receive_text()
            .map_err(|error| engine_error(EngineErrorKind::Unavailable, error.to_string()))?;
        let hello: Value = serde_json::from_str(&hello_text).map_err(|error| {
            engine_error(
                EngineErrorKind::Backend,
                format!("invalid OBS Hello JSON: {error}"),
            )
        })?;
        if hello.get("op").and_then(Value::as_u64) != Some(0) {
            return Err(engine_error(
                EngineErrorKind::Backend,
                "OBS websocket did not start with Hello (op 0)",
            ));
        }

        let server_rpc = hello
            .pointer("/d/rpcVersion")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                engine_error(
                    EngineErrorKind::Backend,
                    "OBS Hello is missing d.rpcVersion",
                )
            })?;
        let rpc_version = self.config.rpc_version.min(server_rpc);

        let authentication = if let Some(authentication) = hello.pointer("/d/authentication") {
            let challenge = authentication
                .get("challenge")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    engine_error(
                        EngineErrorKind::Backend,
                        "OBS authentication challenge is missing",
                    )
                })?;
            let salt = authentication
                .get("salt")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    engine_error(
                        EngineErrorKind::Backend,
                        "OBS authentication salt is missing",
                    )
                })?;
            let password = self.config.password.as_ref().ok_or_else(|| {
                engine_error(
                    EngineErrorKind::Authentication,
                    "OBS requires authentication but no password is configured",
                )
            })?;
            Some(create_obs_authentication(
                password.expose(),
                salt,
                challenge,
            ))
        } else {
            None
        };

        let mut identify_data = Map::new();
        identify_data.insert("rpcVersion".to_owned(), Value::from(rpc_version));
        if let Some(authentication) = authentication {
            identify_data.insert("authentication".to_owned(), Value::String(authentication));
        }
        if let Some(subscriptions) = state.event_subscriptions {
            identify_data.insert("eventSubscriptions".to_owned(), Value::from(subscriptions));
        }

        let identify = serde_json::to_string(&json!({
            "op": 1,
            "d": Value::Object(identify_data),
        }))
        .map_err(|error| {
            engine_error(
                EngineErrorKind::InvalidRequest,
                format!("failed to encode OBS Identify: {error}"),
            )
        })?;
        transport
            .send_text(&identify)
            .map_err(|error| engine_error(EngineErrorKind::Unavailable, error.to_string()))?;

        for _ in 0..16 {
            let text = transport
                .receive_text()
                .map_err(|error| engine_error(EngineErrorKind::Unavailable, error.to_string()))?;
            let message: Value = serde_json::from_str(&text).map_err(|error| {
                engine_error(
                    EngineErrorKind::Backend,
                    format!("invalid OBS identify response JSON: {error}"),
                )
            })?;
            match message.get("op").and_then(Value::as_u64) {
                Some(2) => return Ok(()),
                Some(5) => {
                    if let Some(event) = parse_obs_event(&message) {
                        state.events.push_back(event);
                    }
                }
                _ => {}
            }
        }

        Err(engine_error(
            EngineErrorKind::Backend,
            "OBS Identified response was not received",
        ))
    }

    fn request(
        &self,
        state: &mut ObsState,
        request_type: &str,
        request_data: Value,
    ) -> Result<Value, EngineError> {
        let mut transport = state.transport.take().ok_or_else(|| {
            engine_error(
                EngineErrorKind::Unavailable,
                "OBS websocket is not connected",
            )
        })?;

        let result = request_with_transport(state, transport.as_mut(), request_type, request_data);
        match result {
            Ok(value) => {
                state.transport = Some(transport);
                Ok(value)
            }
            Err(error) => {
                transport.close();
                state
                    .reconnect
                    .failed(self.clock.now_ms(), self.config.reconnect);
                Err(error)
            }
        }
    }

    fn disconnect(&self, state: &mut ObsState) {
        if let Some(mut transport) = state.transport.take() {
            transport.close();
        }
        state
            .reconnect
            .failed(self.clock.now_ms(), self.config.reconnect);
    }
}

impl StreamAdapter for ObsWebSocketAdapter {
    fn execute<'a>(&'a self, action: &'a AuthorizedStreamAction) -> EngineFuture<'a, ()> {
        Box::pin(async move { self.execute_sync(action) })
    }
}
fn request_with_transport(
    state: &mut ObsState,
    transport: &mut dyn WebSocketTransport,
    request_type: &str,
    request_data: Value,
) -> Result<Value, EngineError> {
    let request_id = state.next_request_id();
    let text = serde_json::to_string(&json!({
        "op": 6,
        "d": {
            "requestType": request_type,
            "requestId": request_id,
            "requestData": request_data,
        }
    }))
    .map_err(|error| {
        engine_error(
            EngineErrorKind::InvalidRequest,
            format!("failed to encode OBS request: {error}"),
        )
    })?;
    transport
        .send_text(&text)
        .map_err(|error| engine_error(EngineErrorKind::Unavailable, error.to_string()))?;

    for _ in 0..64 {
        let text = transport
            .receive_text()
            .map_err(|error| engine_error(EngineErrorKind::Unavailable, error.to_string()))?;
        let response: Value = serde_json::from_str(&text).map_err(|error| {
            engine_error(
                EngineErrorKind::Backend,
                format!("invalid OBS response JSON: {error}"),
            )
        })?;
        match response.get("op").and_then(Value::as_u64) {
            Some(5) => {
                if let Some(event) = parse_obs_event(&response) {
                    state.events.push_back(event);
                }
                continue;
            }
            Some(7) => {}
            _ => continue,
        }

        let response_id = response.pointer("/d/requestId").and_then(Value::as_str);
        if response_id != Some(request_id.as_str()) {
            continue;
        }

        let success = response
            .pointer("/d/requestStatus/result")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !success {
            let code = response
                .pointer("/d/requestStatus/code")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let comment = response
                .pointer("/d/requestStatus/comment")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            return Err(engine_error(
                EngineErrorKind::Backend,
                format!("OBS request {request_type} failed with code {code}: {comment}"),
            ));
        }

        return Ok(response
            .pointer("/d/responseData")
            .cloned()
            .unwrap_or(Value::Null));
    }

    Err(engine_error(
        EngineErrorKind::Backend,
        "OBS response limit exceeded while waiting for request",
    ))
}
fn map_stream_action(
    action: &AuthorizedStreamAction,
) -> Result<(&'static str, Value), EngineError> {
    let stream = action.action();
    match stream.action.as_str() {
        "scene.set" => {
            let scene_name = required_string(&stream.arguments, "scene_name")?;
            Ok(("SetCurrentProgramScene", json!({ "sceneName": scene_name })))
        }
        "source.visibility.set" => {
            let scene_name = required_string(&stream.arguments, "scene_name")?;
            let scene_item_id = required_i64(&stream.arguments, "scene_item_id")?;
            let enabled = required_bool(&stream.arguments, "enabled")?;
            Ok((
                "SetSceneItemEnabled",
                json!({
                    "sceneName": scene_name,
                    "sceneItemId": scene_item_id,
                    "sceneItemEnabled": enabled,
                }),
            ))
        }
        "audio.mute.set" => {
            let input_name = required_string(&stream.arguments, "input_name")?;
            let muted = required_bool(&stream.arguments, "muted")?;
            Ok((
                "SetInputMute",
                json!({
                    "inputName": input_name,
                    "inputMuted": muted,
                }),
            ))
        }
        "audio.volume.set" => {
            let input_name = required_string(&stream.arguments, "input_name")?;
            let volume_mul = required_f64(&stream.arguments, "volume_mul")?;
            if !volume_mul.is_finite() || !(0.0..=20.0).contains(&volume_mul) {
                return Err(engine_error(
                    EngineErrorKind::InvalidRequest,
                    "volume_mul must be a finite value in 0..=20",
                ));
            }
            Ok((
                "SetInputVolume",
                json!({
                    "inputName": input_name,
                    "inputVolumeMul": volume_mul,
                }),
            ))
        }
        other => Err(engine_error(
            EngineErrorKind::InvalidRequest,
            format!("unsupported OBS stream action {other:?}"),
        )),
    }
}
fn required_string<'a>(
    arguments: &'a std::collections::BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, EngineError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            engine_error(
                EngineErrorKind::InvalidRequest,
                format!("OBS action requires non-empty {key}"),
            )
        })
}

fn required_bool(
    arguments: &std::collections::BTreeMap<String, Value>,
    key: &str,
) -> Result<bool, EngineError> {
    arguments.get(key).and_then(Value::as_bool).ok_or_else(|| {
        engine_error(
            EngineErrorKind::InvalidRequest,
            format!("OBS action requires boolean {key}"),
        )
    })
}

fn required_i64(
    arguments: &std::collections::BTreeMap<String, Value>,
    key: &str,
) -> Result<i64, EngineError> {
    arguments.get(key).and_then(Value::as_i64).ok_or_else(|| {
        engine_error(
            EngineErrorKind::InvalidRequest,
            format!("OBS action requires integer {key}"),
        )
    })
}

fn required_f64(
    arguments: &std::collections::BTreeMap<String, Value>,
    key: &str,
) -> Result<f64, EngineError> {
    arguments.get(key).and_then(Value::as_f64).ok_or_else(|| {
        engine_error(
            EngineErrorKind::InvalidRequest,
            format!("OBS action requires numeric {key}"),
        )
    })
}
pub fn create_obs_authentication(password: &str, salt: &str, challenge: &str) -> String {
    let first = Sha256::digest(format!("{password}{salt}").as_bytes());
    let secret = BASE64_STANDARD.encode(first);
    let final_hash = Sha256::digest(format!("{secret}{challenge}").as_bytes());
    BASE64_STANDARD.encode(final_hash)
}

fn parse_obs_event(message: &Value) -> Option<AdapterEvent> {
    if message.get("op").and_then(Value::as_u64) != Some(5) {
        return None;
    }
    Some(AdapterEvent {
        source: "obs",
        event_type: message.pointer("/d/eventType")?.as_str()?.to_owned(),
        data: message
            .pointer("/d/eventData")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

fn receive_from_state(state: &mut ObsState) -> Result<String, String> {
    state
        .transport
        .as_mut()
        .ok_or_else(|| "OBS websocket is not connected".to_owned())?
        .receive_text()
        .map_err(|error| error.to_string())
}

fn engine_error(kind: EngineErrorKind, message: impl Into<String>) -> EngineError {
    EngineError::new(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransportError;
    use crate::transport::test_support::{Script, ScriptedConnector, TestClock};
    use aivtuber_domain::{
        AuthorizationMethod, Capability, ControlSecret, LocalControlIngress, OperatorCommandInput,
        StreamAction, authorize_stream_action,
    };
    use aivtuber_scheduler::{
        BlendChannel, PlannedPerformance, Priority, Scheduler, SchedulerConfig,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn authorized_stream(
        action: &str,
        arguments: BTreeMap<String, Value>,
    ) -> AuthorizedStreamAction {
        let secret = [8_u8; 32];
        let ingress = LocalControlIngress::new(
            "local-ui",
            "operator:local",
            AuthorizationMethod::OperatorUi,
            BTreeSet::from([Capability::ObsControl]),
            ControlSecret::new(secret),
        )
        .expect("ingress");
        let authority = ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "evt-obs-control".to_owned(),
                    correlation_id: "corr-obs".to_owned(),
                    sequence: 1,
                    observed_at: "2026-09-24T00:00:00Z".to_owned(),
                    action: "obs.control".to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated")
            .authority()
            .clone();
        authorize_stream_action(
            &authority,
            StreamAction {
                action: action.to_owned(),
                arguments,
            },
        )
        .expect("authorized")
    }

    fn hello(authentication: Option<(&str, &str)>) -> String {
        let mut data = json!({
            "obsStudioVersion": "32.0.0",
            "obsWebSocketVersion": "5.6.0",
            "rpcVersion": 1
        });
        if let Some((challenge, salt)) = authentication {
            data.as_object_mut().expect("hello data").insert(
                "authentication".to_owned(),
                json!({ "challenge": challenge, "salt": salt }),
            );
        }
        json!({ "op": 0, "d": data }).to_string()
    }

    fn identified() -> String {
        json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } }).to_string()
    }

    fn request_ok(id: &str, request_type: &str) -> String {
        json!({
            "op": 7,
            "d": {
                "requestType": request_type,
                "requestId": id,
                "requestStatus": { "result": true, "code": 100 }
            }
        })
        .to_string()
    }

    fn config() -> ObsWebSocketConfig {
        ObsWebSocketConfig {
            password: Some(SecretString::new("obs-password-secret")),
            event_subscriptions: Some(4),
            reconnect: ReconnectPolicy {
                initial_delay_ms: 100,
                max_delay_ms: 800,
            },
            ..ObsWebSocketConfig::default()
        }
    }

    #[test]
    fn official_authentication_example_matches_protocol_algorithm() {
        let authentication = create_obs_authentication(
            "supersecretpassword",
            "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=",
            "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=",
        );
        assert_eq!(
            authentication,
            "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4="
        );
    }

    #[test]
    fn config_debug_redacts_password() {
        let debug = format!("{:?}", config());
        assert!(!debug.contains("obs-password-secret"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn identifies_with_event_subscriptions_and_sets_scene() {
        let challenge = "challenge";
        let salt = "salt";
        let script = Script::new(vec![
            Ok(hello(Some((challenge, salt)))),
            Ok(identified()),
            Ok(request_ok("aivtuber-obs-1", "SetCurrentProgramScene")),
        ]);
        let handle = script.handle.clone();
        let connector = Arc::new(ScriptedConnector::new(vec![Ok(script)]));
        let clock = Arc::new(TestClock::new(0));
        let adapter =
            ObsWebSocketAdapter::with_dependencies(config(), connector, clock).expect("adapter");

        adapter
            .execute_sync(&authorized_stream(
                "scene.set",
                BTreeMap::from([("scene_name".to_owned(), Value::String("Live".to_owned()))]),
            ))
            .expect("scene set");

        let sent: Vec<Value> = handle
            .sent()
            .iter()
            .map(|text| serde_json::from_str(text).expect("sent JSON"))
            .collect();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].get("op").and_then(Value::as_u64), Some(1));
        assert_eq!(
            sent[0]
                .pointer("/d/eventSubscriptions")
                .and_then(Value::as_u64),
            Some(4)
        );
        assert_eq!(
            sent[0].pointer("/d/authentication").and_then(Value::as_str),
            Some(create_obs_authentication("obs-password-secret", salt, challenge).as_str())
        );
        assert_eq!(sent[1].get("op").and_then(Value::as_u64), Some(6));
        assert_eq!(
            sent[1].pointer("/d/requestType").and_then(Value::as_str),
            Some("SetCurrentProgramScene")
        );
        assert_eq!(
            sent[1]
                .pointer("/d/requestData/sceneName")
                .and_then(Value::as_str),
            Some("Live")
        );
    }

    #[test]
    fn reconnect_does_not_replay_stale_stream_action() {
        let first = Script::new(vec![
            Ok(hello(None)),
            Ok(identified()),
            Err(TransportError::new("socket disconnected")),
        ]);
        let first_handle = first.handle.clone();
        let second = Script::new(vec![
            Ok(hello(None)),
            Ok(identified()),
            Ok(request_ok("aivtuber-obs-2", "SetInputMute")),
        ]);
        let second_handle = second.handle.clone();

        let connector = Arc::new(ScriptedConnector::new(vec![Ok(first), Ok(second)]));
        let clock = Arc::new(TestClock::new(0));
        let mut no_password = config();
        no_password.password = None;
        let adapter = ObsWebSocketAdapter::with_dependencies(no_password, connector, clock.clone())
            .expect("adapter");

        let old = adapter.execute_sync(&authorized_stream(
            "scene.set",
            BTreeMap::from([("scene_name".to_owned(), Value::String("Old".to_owned()))]),
        ));
        assert_eq!(
            old.expect_err("disconnect").kind,
            EngineErrorKind::Unavailable
        );
        assert_eq!(first_handle.closed_count(), 1);

        clock.set(100);
        adapter
            .execute_sync(&authorized_stream(
                "audio.mute.set",
                BTreeMap::from([
                    ("input_name".to_owned(), Value::String("Mic".to_owned())),
                    ("muted".to_owned(), Value::Bool(true)),
                ]),
            ))
            .expect("new action after reconnect");

        let sent = second_handle.sent().join(
            "
",
        );
        assert!(sent.contains("SetInputMute"));
        assert!(!sent.contains("SetCurrentProgramScene"));
        assert!(!sent.contains(r#""Old""#));
    }

    #[test]
    fn source_visibility_and_audio_volume_map_to_obs_v5_requests() {
        let source = authorized_stream(
            "source.visibility.set",
            BTreeMap::from([
                ("scene_name".to_owned(), Value::String("Live".to_owned())),
                ("scene_item_id".to_owned(), Value::from(42)),
                ("enabled".to_owned(), Value::Bool(false)),
            ]),
        );
        let (request_type, data) = map_stream_action(&source).expect("source mapping");
        assert_eq!(request_type, "SetSceneItemEnabled");
        assert_eq!(data["sceneItemId"], 42);
        assert_eq!(data["sceneItemEnabled"], false);

        let volume = authorized_stream(
            "audio.volume.set",
            BTreeMap::from([
                ("input_name".to_owned(), Value::String("Music".to_owned())),
                ("volume_mul".to_owned(), Value::from(0.5)),
            ]),
        );
        let (request_type, data) = map_stream_action(&volume).expect("volume mapping");
        assert_eq!(request_type, "SetInputVolume");
        assert_eq!(data["inputName"], "Music");
        assert_eq!(data["inputVolumeMul"], 0.5);
    }

    #[test]
    fn adapter_disconnect_does_not_mutate_or_stop_scheduler() {
        let connector = Arc::new(ScriptedConnector::new(vec![Err(TransportError::new(
            "offline",
        ))]));
        let clock = Arc::new(TestClock::new(0));
        let mut cfg = config();
        cfg.password = None;
        let adapter =
            ObsWebSocketAdapter::with_dependencies(cfg, connector, clock).expect("adapter");

        let result = adapter.execute_sync(&authorized_stream(
            "scene.set",
            BTreeMap::from([("scene_name".to_owned(), Value::String("Live".to_owned()))]),
        ));
        assert_eq!(
            result.expect_err("adapter offline").kind,
            EngineErrorKind::Unavailable
        );

        let mut scheduler = Scheduler::new(SchedulerConfig {
            min_reaction_spacing_ms: 0,
            ..aivtuber_scheduler::SchedulerConfig::default()
        });
        let scheduled = scheduler
            .schedule(PlannedPerformance {
                event_id: "evt-performance".to_owned(),
                asset_id: "reaction.surprise.01".to_owned(),
                priority: Priority::Conversation,
                interruptible: true,
                interrupt_points_ms: vec![400, 800],
                start_at_ms: 0,
                duration_ms: 800,
                generation: 0,
                exclusive: false,
                channels: BTreeSet::from([BlendChannel::Audio]),
            })
            .expect("scheduler remains operational");
        assert_eq!(scheduled.generation, 1);
    }
}
