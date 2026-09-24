use crate::{
    AdapterClock, AdapterEvent, ReconnectPolicy, ReconnectState, SecretString,
    TungsteniteConnector, WebSocketConnector, WebSocketTransport, default_clock,
};
use aivtuber_domain::{
    AuthorizedAvatarAction, AvatarAdapter, EngineError, EngineErrorKind, EngineFuture,
};
use serde_json::{Value, json};
use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

const VTS_API_NAME: &str = "VTubeStudioPublicAPI";
const VTS_API_VERSION: &str = "1.0";

#[derive(Clone)]
pub struct VTubeStudioConfig {
    pub endpoint: String,
    pub plugin_name: String,
    pub plugin_developer: String,
    pub authentication_token: Option<SecretString>,
    pub reconnect: ReconnectPolicy,
}

impl Default for VTubeStudioConfig {
    fn default() -> Self {
        Self {
            endpoint: "ws://127.0.0.1:8001".to_owned(),
            plugin_name: "AI VTuber".to_owned(),
            plugin_developer: "xmeta".to_owned(),
            authentication_token: None,
            reconnect: ReconnectPolicy::default(),
        }
    }
}

impl fmt::Debug for VTubeStudioConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VTubeStudioConfig")
            .field("endpoint", &self.endpoint)
            .field("plugin_name", &self.plugin_name)
            .field("plugin_developer", &self.plugin_developer)
            .field(
                "authentication_token",
                &self.authentication_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("reconnect", &self.reconnect)
            .finish()
    }
}

struct VtsState {
    transport: Option<Box<dyn WebSocketTransport>>,
    reconnect: ReconnectState,
    request_sequence: u64,
    token: Option<SecretString>,
    subscriptions: BTreeSet<String>,
    events: VecDeque<AdapterEvent>,
}

impl VtsState {
    fn new(token: Option<SecretString>) -> Self {
        Self {
            transport: None,
            reconnect: ReconnectState::default(),
            request_sequence: 0,
            token,
            subscriptions: BTreeSet::new(),
            events: VecDeque::new(),
        }
    }

    fn next_request_id(&mut self) -> String {
        self.request_sequence = self.request_sequence.saturating_add(1);
        format!("aivtuber-vts-{}", self.request_sequence)
    }
}

pub struct VTubeStudioAdapter {
    config: VTubeStudioConfig,
    connector: Arc<dyn WebSocketConnector>,
    clock: Arc<dyn AdapterClock>,
    state: Mutex<VtsState>,
}

impl fmt::Debug for VTubeStudioAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VTubeStudioAdapter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl VTubeStudioAdapter {
    pub fn new(config: VTubeStudioConfig) -> Result<Self, EngineError> {
        Self::with_dependencies(config, Arc::new(TungsteniteConnector), default_clock())
    }

    pub fn with_dependencies(
        config: VTubeStudioConfig,
        connector: Arc<dyn WebSocketConnector>,
        clock: Arc<dyn AdapterClock>,
    ) -> Result<Self, EngineError> {
        validate_vts_config(&config)?;
        let token = config.authentication_token.clone();
        Ok(Self {
            config,
            connector,
            clock,
            state: Mutex::new(VtsState::new(token)),
        })
    }

    /// Establish the websocket session, authenticate, and restore subscriptions.
    pub fn connect(&self) -> Result<(), EngineError> {
        let mut state = self.lock_state()?;
        self.ensure_connected(&mut state)
    }

    pub fn execute_sync(&self, action: &AuthorizedAvatarAction) -> Result<(), EngineError> {
        let mut state = self.lock_state()?;
        self.ensure_connected(&mut state)?;

        let avatar = action.action();
        if let Some(hotkey_id) = avatar.action.strip_prefix("hotkey.trigger:") {
            if hotkey_id.trim().is_empty() {
                return Err(engine_error(
                    EngineErrorKind::InvalidRequest,
                    "hotkey.trigger requires a non-empty hotkey id/name",
                ));
            }
            let data = json!({ "hotkeyID": hotkey_id });
            self.request(&mut state, "HotkeyTriggerRequest", data)?;
            return Ok(());
        }

        if avatar.action == "parameters.inject" {
            let mut parameter_values = Vec::with_capacity(avatar.parameters.len());
            for (id, value) in &avatar.parameters {
                if !value.is_finite() || !(-1_000_000.0..=1_000_000.0).contains(value) {
                    return Err(engine_error(
                        EngineErrorKind::InvalidRequest,
                        format!("parameter {id:?} is outside the VTube Studio API range"),
                    ));
                }
                parameter_values.push(json!({ "id": id, "value": value }));
            }
            self.request(
                &mut state,
                "InjectParameterDataRequest",
                json!({
                    "faceFound": false,
                    "mode": "set",
                    "parameterValues": parameter_values,
                }),
            )?;
            return Ok(());
        }

        Err(engine_error(
            EngineErrorKind::InvalidRequest,
            format!("unsupported VTube Studio avatar action {:?}", avatar.action),
        ))
    }

    pub fn subscribe_event(&self, event_name: &str) -> Result<(), EngineError> {
        let event_name = event_name.trim();
        if event_name.is_empty() {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                "event name must not be empty",
            ));
        }

        let mut state = self.lock_state()?;
        state.subscriptions.insert(event_name.to_owned());
        if state.transport.is_some() {
            self.request(
                &mut state,
                "EventSubscriptionRequest",
                json!({
                    "eventName": event_name,
                    "subscribe": true,
                    "config": {},
                }),
            )?;
        }
        Ok(())
    }

    pub fn poll_event(&self) -> Result<Option<AdapterEvent>, EngineError> {
        let mut state = self.lock_state()?;
        if let Some(event) = state.events.pop_front() {
            return Ok(Some(event));
        }
        self.ensure_connected(&mut state)?;
        let text = receive_from_state(&mut state).map_err(|error| {
            self.disconnect(&mut state);
            engine_error(EngineErrorKind::Unavailable, error)
        })?;
        let message: Value = serde_json::from_str(&text).map_err(|error| {
            engine_error(
                EngineErrorKind::Backend,
                format!("invalid VTube Studio event JSON: {error}"),
            )
        })?;
        Ok(parse_vts_event(&message))
    }

    pub fn next_retry_at_ms(&self) -> Result<u64, EngineError> {
        Ok(self.lock_state()?.reconnect.next_retry_at_ms())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, VtsState>, EngineError> {
        self.state.lock().map_err(|_| {
            engine_error(
                EngineErrorKind::Backend,
                "VTube Studio adapter state lock was poisoned",
            )
        })
    }

    fn ensure_connected(&self, state: &mut VtsState) -> Result<(), EngineError> {
        if state.transport.is_some() {
            return Ok(());
        }

        let now_ms = self.clock.now_ms();
        if !state.reconnect.can_attempt(now_ms) {
            return Err(engine_error(
                EngineErrorKind::Unavailable,
                format!(
                    "VTube Studio reconnect backoff active until {}ms",
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
                    format!("VTube Studio connection failed: {error}"),
                )
            })?;

        let authenticate = self.authenticate(state, transport.as_mut());
        if let Err(error) = authenticate {
            transport.close();
            state.reconnect.failed(now_ms, self.config.reconnect);
            return Err(error);
        }

        let subscriptions: Vec<String> = state.subscriptions.iter().cloned().collect();
        for event_name in subscriptions {
            if let Err(error) = request_with_transport(
                state,
                transport.as_mut(),
                "EventSubscriptionRequest",
                json!({
                    "eventName": event_name,
                    "subscribe": true,
                    "config": {},
                }),
            ) {
                transport.close();
                state.reconnect.failed(now_ms, self.config.reconnect);
                return Err(error);
            }
        }

        state.transport = Some(transport);
        state.reconnect.connected();
        Ok(())
    }

    fn authenticate(
        &self,
        state: &mut VtsState,
        transport: &mut dyn WebSocketTransport,
    ) -> Result<(), EngineError> {
        if state.token.is_none() {
            let data = request_with_transport(
                state,
                transport,
                "AuthenticationTokenRequest",
                json!({
                    "pluginName": self.config.plugin_name,
                    "pluginDeveloper": self.config.plugin_developer,
                }),
            )?;
            let token = data
                .get("authenticationToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| {
                    engine_error(
                        EngineErrorKind::Authentication,
                        "VTube Studio did not return an authentication token",
                    )
                })?;
            state.token = Some(SecretString::new(token));
        }

        let token = state
            .token
            .as_ref()
            .expect("token is populated before AuthenticationRequest")
            .expose()
            .to_owned();

        let data = request_with_transport(
            state,
            transport,
            "AuthenticationRequest",
            json!({
                "pluginName": self.config.plugin_name,
                "pluginDeveloper": self.config.plugin_developer,
                "authenticationToken": token,
            }),
        )?;

        if data.get("authenticated").and_then(Value::as_bool) != Some(true) {
            return Err(engine_error(
                EngineErrorKind::Authentication,
                "VTube Studio authentication was rejected",
            ));
        }
        Ok(())
    }

    fn request(
        &self,
        state: &mut VtsState,
        message_type: &str,
        data: Value,
    ) -> Result<Value, EngineError> {
        let mut transport = state.transport.take().ok_or_else(|| {
            engine_error(
                EngineErrorKind::Unavailable,
                "VTube Studio is not connected",
            )
        })?;

        let result = request_with_transport(state, transport.as_mut(), message_type, data);
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

    fn disconnect(&self, state: &mut VtsState) {
        if let Some(mut transport) = state.transport.take() {
            transport.close();
        }
        state
            .reconnect
            .failed(self.clock.now_ms(), self.config.reconnect);
    }
}

impl AvatarAdapter for VTubeStudioAdapter {
    fn execute<'a>(&'a self, action: &'a AuthorizedAvatarAction) -> EngineFuture<'a, ()> {
        Box::pin(async move { self.execute_sync(action) })
    }
}

fn request_with_transport(
    state: &mut VtsState,
    transport: &mut dyn WebSocketTransport,
    message_type: &str,
    data: Value,
) -> Result<Value, EngineError> {
    let request_id = state.next_request_id();
    let request = json!({
        "apiName": VTS_API_NAME,
        "apiVersion": VTS_API_VERSION,
        "requestID": request_id,
        "messageType": message_type,
        "data": data,
    });

    let text = serde_json::to_string(&request).map_err(|error| {
        engine_error(
            EngineErrorKind::InvalidRequest,
            format!("failed to encode VTube Studio request: {error}"),
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
                format!("invalid VTube Studio response JSON: {error}"),
            )
        })?;

        if let Some(event) = parse_vts_event(&response) {
            state.events.push_back(event);
            continue;
        }

        let response_request_id = response.get("requestID").and_then(Value::as_str);
        if response_request_id != Some(request_id.as_str()) {
            continue;
        }

        if response.get("messageType").and_then(Value::as_str) == Some("APIError") {
            let error_id = response
                .pointer("/data/errorID")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let kind = if error_id == 50 {
                EngineErrorKind::Authentication
            } else {
                EngineErrorKind::Backend
            };
            return Err(engine_error(
                kind,
                format!("VTube Studio API error {error_id}"),
            ));
        }

        return Ok(response.get("data").cloned().unwrap_or(Value::Null));
    }

    Err(engine_error(
        EngineErrorKind::Backend,
        "VTube Studio response limit exceeded while waiting for request",
    ))
}

fn parse_vts_event(message: &Value) -> Option<AdapterEvent> {
    let message_type = message.get("messageType")?.as_str()?;
    if !message_type.ends_with("Event") {
        return None;
    }
    Some(AdapterEvent {
        source: "vtube_studio",
        event_type: message_type.to_owned(),
        data: message.get("data").cloned().unwrap_or(Value::Null),
    })
}

fn receive_from_state(state: &mut VtsState) -> Result<String, String> {
    state
        .transport
        .as_mut()
        .ok_or_else(|| "VTube Studio is not connected".to_owned())?
        .receive_text()
        .map_err(|error| error.to_string())
}

fn validate_vts_config(config: &VTubeStudioConfig) -> Result<(), EngineError> {
    if config.endpoint.trim().is_empty() {
        return Err(engine_error(
            EngineErrorKind::InvalidRequest,
            "VTube Studio endpoint must not be empty",
        ));
    }
    for (field, value) in [
        ("plugin_name", config.plugin_name.as_str()),
        ("plugin_developer", config.plugin_developer.as_str()),
    ] {
        if !(3..=32).contains(&value.chars().count()) {
            return Err(engine_error(
                EngineErrorKind::InvalidRequest,
                format!("{field} must contain 3..=32 characters"),
            ));
        }
    }
    Ok(())
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
        AuthorizationMethod, AvatarAction, Capability, ControlSecret, LocalControlIngress,
        OperatorCommandInput, authorize_avatar_action,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn authorized_avatar(action: &str) -> AuthorizedAvatarAction {
        let secret = [9_u8; 32];
        let ingress = LocalControlIngress::new(
            "local-ui",
            "operator:local",
            AuthorizationMethod::OperatorUi,
            BTreeSet::from([Capability::AvatarControl]),
            ControlSecret::new(secret),
        )
        .expect("ingress");
        let authority = ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "evt-vts-control".to_owned(),
                    correlation_id: "corr-vts".to_owned(),
                    sequence: 1,
                    observed_at: "2026-09-24T00:00:00Z".to_owned(),
                    action: "avatar.control".to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated")
            .authority()
            .clone();
        authorize_avatar_action(
            &authority,
            AvatarAction {
                action: action.to_owned(),
                parameters: BTreeMap::new(),
            },
        )
        .expect("authorized")
    }

    fn auth_response(id: &str) -> String {
        json!({
            "apiName": VTS_API_NAME,
            "apiVersion": VTS_API_VERSION,
            "requestID": id,
            "messageType": "AuthenticationResponse",
            "data": { "authenticated": true, "reason": "ok" }
        })
        .to_string()
    }

    fn ok_response(id: &str, message_type: &str) -> String {
        json!({
            "apiName": VTS_API_NAME,
            "apiVersion": VTS_API_VERSION,
            "requestID": id,
            "messageType": message_type,
            "data": {}
        })
        .to_string()
    }

    fn config() -> VTubeStudioConfig {
        VTubeStudioConfig {
            authentication_token: Some(SecretString::new("vts-token-secret")),
            reconnect: ReconnectPolicy {
                initial_delay_ms: 100,
                max_delay_ms: 800,
            },
            ..VTubeStudioConfig::default()
        }
    }

    #[test]
    fn config_debug_redacts_authentication_token() {
        let debug = format!("{:?}", config());
        assert!(!debug.contains("vts-token-secret"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn authenticates_subscribes_and_triggers_hotkey_over_mock_transport() {
        let script = Script::new(vec![
            Ok(auth_response("aivtuber-vts-1")),
            Ok(ok_response("aivtuber-vts-2", "EventSubscriptionResponse")),
            Ok(ok_response("aivtuber-vts-3", "HotkeyTriggerResponse")),
        ]);
        let handle = script.handle.clone();
        let connector = Arc::new(ScriptedConnector::new(vec![Ok(script)]));
        let clock = Arc::new(TestClock::new(0));
        let adapter =
            VTubeStudioAdapter::with_dependencies(config(), connector, clock).expect("adapter");

        adapter
            .subscribe_event("ModelLoadedEvent")
            .expect("subscribe");
        adapter
            .execute_sync(&authorized_avatar("hotkey.trigger:Wave"))
            .expect("hotkey");

        let sent: Vec<Value> = handle
            .sent()
            .iter()
            .map(|text| serde_json::from_str(text).expect("sent JSON"))
            .collect();
        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent[0].get("messageType").and_then(Value::as_str),
            Some("AuthenticationRequest")
        );
        assert_eq!(
            sent[1].get("messageType").and_then(Value::as_str),
            Some("EventSubscriptionRequest")
        );
        assert_eq!(
            sent[2].pointer("/data/hotkeyID").and_then(Value::as_str),
            Some("Wave")
        );
    }

    #[test]
    fn disconnect_backoff_drops_old_action_instead_of_replaying_it() {
        let second = Script::new(vec![
            Ok(auth_response("aivtuber-vts-1")),
            Ok(ok_response("aivtuber-vts-2", "HotkeyTriggerResponse")),
        ]);
        let second_handle = second.handle.clone();
        let connector = Arc::new(ScriptedConnector::new(vec![
            Err(TransportError::new("offline")),
            Ok(second),
        ]));
        let clock = Arc::new(TestClock::new(0));
        let adapter = VTubeStudioAdapter::with_dependencies(config(), connector, clock.clone())
            .expect("adapter");

        let first = adapter.execute_sync(&authorized_avatar("hotkey.trigger:Old"));
        assert_eq!(
            first.expect_err("offline").kind,
            EngineErrorKind::Unavailable
        );

        let during_backoff = adapter.execute_sync(&authorized_avatar("hotkey.trigger:Old"));
        assert_eq!(
            during_backoff.expect_err("backoff").kind,
            EngineErrorKind::Unavailable
        );

        clock.set(100);
        adapter
            .execute_sync(&authorized_avatar("hotkey.trigger:New"))
            .expect("reconnected action");

        let sent = second_handle.sent().join(
            "
",
        );
        assert!(sent.contains(r#""hotkeyID":"New""#));
        assert!(!sent.contains(r#""hotkeyID":"Old""#));
    }

    #[test]
    fn parameter_injection_uses_vtube_studio_wire_shape() {
        let script = Script::new(vec![
            Ok(auth_response("aivtuber-vts-1")),
            Ok(ok_response("aivtuber-vts-2", "InjectParameterDataResponse")),
        ]);
        let handle = script.handle.clone();
        let connector = Arc::new(ScriptedConnector::new(vec![Ok(script)]));
        let clock = Arc::new(TestClock::new(0));
        let adapter =
            VTubeStudioAdapter::with_dependencies(config(), connector, clock).expect("adapter");

        let secret = [9_u8; 32];
        let ingress = LocalControlIngress::new(
            "local-ui",
            "operator:local",
            AuthorizationMethod::OperatorUi,
            BTreeSet::from([Capability::AvatarControl]),
            ControlSecret::new(secret),
        )
        .expect("ingress");
        let authority = ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "evt-vts-params".to_owned(),
                    correlation_id: "corr-vts".to_owned(),
                    sequence: 2,
                    observed_at: "2026-09-24T00:00:00Z".to_owned(),
                    action: "avatar.control".to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticated")
            .authority()
            .clone();
        let action = authorize_avatar_action(
            &authority,
            AvatarAction {
                action: "parameters.inject".to_owned(),
                parameters: BTreeMap::from([
                    ("FaceAngleX".to_owned(), 12.5),
                    ("MouthOpen".to_owned(), 0.8),
                ]),
            },
        )
        .expect("authorized");

        adapter.execute_sync(&action).expect("inject parameters");

        let sent: Vec<Value> = handle
            .sent()
            .iter()
            .map(|text| serde_json::from_str(text).expect("sent JSON"))
            .collect();
        assert_eq!(
            sent[1].get("messageType").and_then(Value::as_str),
            Some("InjectParameterDataRequest")
        );
        assert_eq!(
            sent[1].pointer("/data/mode").and_then(Value::as_str),
            Some("set")
        );
        assert_eq!(
            sent[1]
                .pointer("/data/parameterValues")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
    }
}
