use aivtuber_adapters::{
    ObsWebSocketAdapter, ObsWebSocketConfig, SecretString, VTubeStudioAdapter, VTubeStudioConfig,
};
use aivtuber_domain::{
    AuthorizationMethod, AvatarAction, Capability, ControlSecret, LocalControlIngress,
    OperatorCommandInput, StreamAction, authorize_avatar_action, authorize_stream_action,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn authority(capability: Capability, action: &str) -> aivtuber_domain::AuthenticatedControl {
    let secret = [5_u8; 32];
    let ingress = LocalControlIngress::new(
        "smoke-test",
        "operator:smoke-test",
        AuthorizationMethod::OperatorUi,
        BTreeSet::from([capability]),
        ControlSecret::new(secret),
    )
    .expect("trusted smoke ingress");
    ingress
        .authenticate(
            OperatorCommandInput {
                event_id: "evt-live-smoke".to_owned(),
                correlation_id: "corr-live-smoke".to_owned(),
                sequence: 1,
                observed_at: "2026-09-24T00:00:00Z".to_owned(),
                action: action.to_owned(),
                payload: BTreeMap::new(),
            },
            &secret,
        )
        .expect("authenticated smoke authority")
        .authority()
        .clone()
}

#[test]
#[ignore = "requires a running local VTube Studio instance with Plugin API enabled"]
fn vtube_studio_live_smoke() {
    let endpoint =
        std::env::var("AIVTUBER_VTS_ENDPOINT").unwrap_or_else(|_| "ws://127.0.0.1:8001".to_owned());
    let token = std::env::var("AIVTUBER_VTS_TOKEN")
        .ok()
        .map(SecretString::new);
    let adapter = VTubeStudioAdapter::new(VTubeStudioConfig {
        endpoint,
        authentication_token: token,
        ..VTubeStudioConfig::default()
    })
    .expect("valid VTube Studio config");

    adapter
        .subscribe_event("ModelLoadedEvent")
        .expect("register event subscription");
    adapter
        .connect()
        .expect("connect/authenticate VTube Studio");

    if let Ok(hotkey) = std::env::var("AIVTUBER_VTS_HOTKEY") {
        let authority = authority(Capability::AvatarControl, "avatar.control");
        let action = authorize_avatar_action(
            &authority,
            AvatarAction {
                action: format!("hotkey.trigger:{hotkey}"),
                parameters: BTreeMap::new(),
            },
        )
        .expect("authorize VTS smoke hotkey");
        adapter
            .execute_sync(&action)
            .expect("trigger VTS smoke hotkey");
    }
}

#[test]
#[ignore = "requires a running local OBS instance with obs-websocket 5.x enabled"]
fn obs_live_smoke() {
    let endpoint =
        std::env::var("AIVTUBER_OBS_ENDPOINT").unwrap_or_else(|_| "ws://127.0.0.1:4455".to_owned());
    let password = std::env::var("AIVTUBER_OBS_PASSWORD")
        .ok()
        .map(SecretString::new);
    let adapter = ObsWebSocketAdapter::new(ObsWebSocketConfig {
        endpoint,
        password,
        ..ObsWebSocketConfig::default()
    })
    .expect("valid OBS config");

    adapter.connect().expect("connect/authenticate OBS");

    if let Ok(scene) = std::env::var("AIVTUBER_OBS_SCENE") {
        let authority = authority(Capability::ObsControl, "obs.control");
        let action = authorize_stream_action(
            &authority,
            StreamAction {
                action: "scene.set".to_owned(),
                arguments: BTreeMap::from([("scene_name".to_owned(), Value::String(scene))]),
            },
        )
        .expect("authorize OBS smoke scene change");
        adapter.execute_sync(&action).expect("set OBS smoke scene");
    }
}
