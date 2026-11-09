use crate::{AudioOutput, AvatarOutput, StreamOutput};
use aivtuber_adapters::{
    AudioPlayRequest, AudioPlayer, AudioStopOutcome, DropReason, GazeTarget, LocalAvatarHandle,
    LocalAvatarHealth, LocalAvatarServer, LocalAvatarState, ObsWebSocketAdapter, PresetKind,
    PresetMapping, VTubeStudioAdapter, WireCommand, map_preset,
};
use aivtuber_domain::{
    AuthenticatedControl, AuthorizedStreamAction, AvatarAction, EngineError, EngineErrorKind,
    Viseme, authorize_avatar_action,
};
use aivtuber_runtime::{AudioPlaybackCommand, AvatarPlaybackCommand};
use std::collections::BTreeMap;

impl<T> AudioOutput for T
where
    T: AudioPlayer,
{
    fn execute(&mut self, command: &AudioPlaybackCommand) -> Result<(), EngineError> {
        self.play(&AudioPlayRequest {
            audio_ref: command.audio_ref.clone(),
            speed_factor: command.speed_factor,
        })
    }

    fn stop(&mut self, fade_ms: u32) -> Result<AudioStopOutcome, EngineError> {
        AudioPlayer::stop(self, fade_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtsPlaybackConfig {
    pub expression_hotkey_prefix: String,
    pub gesture_hotkey_prefix: String,
    pub gaze_hotkey_prefix: String,
    /// Prefix of the VTube Studio input parameters that carry the mouth
    /// shapes; the suffix per canonical viseme is adapter-private
    /// ([`vts_viseme_suffix`]).
    pub viseme_parameter_prefix: String,
    /// Hotkey a model author binds to "clear all expressions / return to
    /// neutral". VTube Studio has no generic clear call, so `Reset` triggers
    /// this hotkey when set. `None` means `Reset` only zeroes the mouth
    /// parameters, because triggering an absent hotkey would fail.
    pub reset_hotkey: Option<String>,
}

impl Default for VtsPlaybackConfig {
    fn default() -> Self {
        Self {
            expression_hotkey_prefix: String::new(),
            gesture_hotkey_prefix: String::new(),
            gaze_hotkey_prefix: "gaze.".to_owned(),
            viseme_parameter_prefix: "Viseme".to_owned(),
            reset_hotkey: None,
        }
    }
}

/// VTube Studio parameter suffix for a canonical viseme. This is the only
/// place VTS names exist; `Neutral` has no parameter (all mouth shapes zero).
fn vts_viseme_suffix(viseme: Viseme) -> Option<&'static str> {
    match viseme {
        Viseme::Aa => Some("A"),
        Viseme::Ih => Some("I"),
        Viseme::Ou => Some("U"),
        Viseme::Ee => Some("E"),
        Viseme::Oh => Some("O"),
        Viseme::Neutral => None,
    }
}

#[derive(Debug)]
pub struct VtsAvatarOutput {
    adapter: VTubeStudioAdapter,
    authority: AuthenticatedControl,
    config: VtsPlaybackConfig,
}

impl VtsAvatarOutput {
    pub fn new(
        adapter: VTubeStudioAdapter,
        authority: AuthenticatedControl,
        config: VtsPlaybackConfig,
    ) -> Self {
        Self {
            adapter,
            authority,
            config,
        }
    }

    fn authorize(
        &self,
        action: AvatarAction,
    ) -> Result<aivtuber_domain::AuthorizedAvatarAction, EngineError> {
        authorize_avatar_action(&self.authority, action)
    }

    /// The authorized VTS actions one playback command expands to, in order.
    fn actions_for(
        &self,
        command: &AvatarPlaybackCommand,
    ) -> Result<Vec<aivtuber_domain::AuthorizedAvatarAction>, EngineError> {
        let actions = match command {
            AvatarPlaybackCommand::Expression { preset, .. } => {
                vec![self.hotkey_action(format!(
                    "{}{}",
                    self.config.expression_hotkey_prefix, preset
                ))?]
            }
            AvatarPlaybackCommand::Gesture { preset, .. } => {
                vec![
                    self.hotkey_action(format!("{}{}", self.config.gesture_hotkey_prefix, preset))?,
                ]
            }
            AvatarPlaybackCommand::Gaze { target, .. } => {
                vec![self.hotkey_action(format!("{}{}", self.config.gaze_hotkey_prefix, target))?]
            }
            AvatarPlaybackCommand::Viseme { viseme, weight, .. } => {
                vec![self.viseme_action(*viseme, *weight)?]
            }
            AvatarPlaybackCommand::Reset { .. } => {
                let mut actions = vec![self.viseme_action(Viseme::Neutral, 0.0)?];
                if let Some(hotkey) = &self.config.reset_hotkey {
                    actions.push(self.hotkey_action(hotkey.clone())?);
                }
                actions
            }
        };
        Ok(actions)
    }

    /// One parameter injection that zeroes every mouth shape, then sets the
    /// addressed one (`Neutral` addresses none).
    fn viseme_action(
        &self,
        viseme: Viseme,
        weight: f64,
    ) -> Result<aivtuber_domain::AuthorizedAvatarAction, EngineError> {
        let prefix = &self.config.viseme_parameter_prefix;
        let mut parameters = Viseme::ALL
            .into_iter()
            .filter_map(vts_viseme_suffix)
            .map(|suffix| (format!("{prefix}{suffix}"), 0.0))
            .collect::<BTreeMap<_, _>>();
        if let Some(suffix) = vts_viseme_suffix(viseme) {
            parameters.insert(format!("{prefix}{suffix}"), weight);
        }
        self.authorize(AvatarAction {
            action: "parameters.inject".to_owned(),
            parameters,
        })
    }

    fn hotkey_action(
        &self,
        id: String,
    ) -> Result<aivtuber_domain::AuthorizedAvatarAction, EngineError> {
        self.authorize(AvatarAction {
            action: format!("hotkey.trigger:{id}"),
            parameters: BTreeMap::new(),
        })
    }
}

impl AvatarOutput for VtsAvatarOutput {
    fn name(&self) -> &'static str {
        "VTube Studio"
    }

    fn connect(&mut self) -> Result<(), EngineError> {
        self.adapter.connect()
    }

    fn execute(&mut self, command: &AvatarPlaybackCommand) -> Result<(), EngineError> {
        let actions = self.actions_for(command)?;
        for action in &actions {
            self.adapter.execute_sync(action)?;
        }
        Ok(())
    }
}

/// Local VRM renderer output (ADR-0003, #219). `execute` only authorizes and
/// enqueues into the adapter's bounded queue: an absent, restarting or slow
/// renderer drops and counts commands and never fails or blocks the
/// dispatcher. Renderer availability is reported through `connect()`, which
/// `maintain_adapters` polls into `AdapterHealth::avatar_error`.
#[derive(Debug)]
pub struct LocalAvatarOutput {
    // Owns the listener threads; dropping the output stops the server.
    server: LocalAvatarServer,
    handle: LocalAvatarHandle,
    authority: AuthenticatedControl,
}

impl LocalAvatarOutput {
    pub fn new(server: LocalAvatarServer, authority: AuthenticatedControl) -> Self {
        let handle = server.handle();
        Self {
            server,
            handle,
            authority,
        }
    }

    pub fn health(&self) -> LocalAvatarHealth {
        self.handle.health()
    }

    /// Operator-facing URL; contains the per-launch secret, never log it.
    pub fn launch_url(&self) -> String {
        self.server.launch_url()
    }

    /// Pack preset -> renderer wire command. Values are validated against the
    /// protocol ranges *before* scaling (a bad input must not be rescued by
    /// attenuation) and clamped after.
    fn wire_for(command: &AvatarPlaybackCommand) -> Result<WireCommand, Unsent> {
        let wire = match command {
            AvatarPlaybackCommand::Expression {
                preset, intensity, ..
            } => {
                let (target, scale) = Self::resolve(PresetKind::Expression, preset)?;
                let raw = WireCommand::Expression {
                    preset: target.to_owned(),
                    intensity: *intensity,
                };
                raw.validate().map_err(|_| Unsent::Invalid)?;
                WireCommand::Expression {
                    preset: target.to_owned(),
                    intensity: (intensity * scale).clamp(0.0, 1.0),
                }
            }
            AvatarPlaybackCommand::Gesture {
                preset, amplitude, ..
            } => {
                let (target, scale) = Self::resolve(PresetKind::Gesture, preset)?;
                let raw = WireCommand::Gesture {
                    preset: target.to_owned(),
                    amplitude: *amplitude,
                };
                raw.validate().map_err(|_| Unsent::Invalid)?;
                WireCommand::Gesture {
                    preset: target.to_owned(),
                    amplitude: (amplitude * scale).clamp(0.0, 2.0),
                }
            }
            AvatarPlaybackCommand::Gaze { target, .. } => WireCommand::Gaze {
                target: GazeTarget::parse(target).ok_or(Unsent::Invalid)?,
            },
            AvatarPlaybackCommand::Viseme { viseme, weight, .. } => WireCommand::Viseme {
                viseme: *viseme,
                weight: *weight,
            },
            AvatarPlaybackCommand::Reset { .. } => WireCommand::Reset {},
        };
        wire.validate().map_err(|_| Unsent::Invalid)?;
        Ok(wire)
    }

    fn resolve(kind: PresetKind, preset: &str) -> Result<(&'static str, f64), Unsent> {
        match map_preset(kind, preset) {
            PresetMapping::Mapped { target, scale } => Ok((target, scale)),
            PresetMapping::Unsupported { .. } | PresetMapping::Unknown => {
                Err(Unsent::Preset(preset.to_owned()))
            }
        }
    }
}

/// Why a playback command has no wire form. Neither is an error for the
/// caller: both are counted in the adapter health.
#[derive(Debug, PartialEq, Eq)]
enum Unsent {
    /// Preset with no renderer equivalent (unsupported or unknown).
    Preset(String),
    /// Out-of-range value or unknown gaze target.
    Invalid,
}

impl AvatarOutput for LocalAvatarOutput {
    fn name(&self) -> &'static str {
        "local avatar renderer"
    }

    fn connect(&mut self) -> Result<(), EngineError> {
        match self.handle.health().state {
            LocalAvatarState::Ready => Ok(()),
            state => Err(EngineError::new(
                EngineErrorKind::Unavailable,
                format!("local avatar renderer is {state:?}"),
            )),
        }
    }

    fn execute(&mut self, command: &AvatarPlaybackCommand) -> Result<(), EngineError> {
        let wire = match Self::wire_for(command) {
            Ok(wire) => wire,
            Err(Unsent::Preset(name)) => {
                self.handle.record_unmapped_preset(&name);
                return Ok(());
            }
            Err(Unsent::Invalid) => {
                self.handle.record_drop(DropReason::Unmapped);
                return Ok(());
            }
        };
        // Same authority gate as every other avatar output. A failure here is
        // a wiring bug, not renderer weather, so it is a real error.
        authorize_avatar_action(
            &self.authority,
            AvatarAction {
                action: format!("avatar.local:{}", wire.kind()),
                parameters: BTreeMap::new(),
            },
        )?;
        // Drop-and-count is already recorded by the adapter.
        let _ = self.handle.enqueue(wire);
        Ok(())
    }
}

#[derive(Debug)]
pub struct ObsStreamOutput {
    adapter: ObsWebSocketAdapter,
}

impl ObsStreamOutput {
    pub fn new(adapter: ObsWebSocketAdapter) -> Self {
        Self { adapter }
    }
}

impl StreamOutput for ObsStreamOutput {
    fn connect(&mut self) -> Result<(), EngineError> {
        self.adapter.connect()
    }

    fn execute(&mut self, action: &AuthorizedStreamAction) -> Result<(), EngineError> {
        self.adapter.execute_sync(action)
    }
}

#[derive(Debug, Default)]
pub struct NoopAvatarOutput;

impl AvatarOutput for NoopAvatarOutput {
    fn name(&self) -> &'static str {
        "no avatar output"
    }

    fn execute(&mut self, _command: &AvatarPlaybackCommand) -> Result<(), EngineError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct NoopStreamOutput;

impl StreamOutput for NoopStreamOutput {
    fn execute(&mut self, _action: &AuthorizedStreamAction) -> Result<(), EngineError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aivtuber_adapters::VTubeStudioConfig;
    use aivtuber_domain::{
        AuthorizationMethod, Capability, ControlSecret, LocalControlIngress, OperatorCommandInput,
    };
    use std::collections::BTreeSet;

    fn authority() -> AuthenticatedControl {
        let secret = [9_u8; 32];
        let ingress = LocalControlIngress::new(
            "local-runtime",
            "runtime:motor",
            AuthorizationMethod::SignedLocalApi,
            BTreeSet::from([Capability::AvatarControl]),
            ControlSecret::new(secret),
        )
        .expect("ingress");
        let command = ingress
            .authenticate(
                OperatorCommandInput {
                    event_id: "test-avatar-authority".to_owned(),
                    correlation_id: "test".to_owned(),
                    sequence: 0,
                    observed_at: "1970-01-01T00:00:00Z".to_owned(),
                    action: "avatar.control".to_owned(),
                    payload: BTreeMap::new(),
                },
                &secret,
            )
            .expect("authenticate");
        command.authority().clone()
    }

    fn output(reset_hotkey: Option<&str>) -> VtsAvatarOutput {
        VtsAvatarOutput::new(
            VTubeStudioAdapter::new(VTubeStudioConfig::default()).expect("adapter"),
            authority(),
            VtsPlaybackConfig {
                reset_hotkey: reset_hotkey.map(str::to_owned),
                ..VtsPlaybackConfig::default()
            },
        )
    }

    fn local_output() -> LocalAvatarOutput {
        let server = LocalAvatarServer::start(aivtuber_adapters::LocalAvatarConfig {
            port: 0,
            ..aivtuber_adapters::LocalAvatarConfig::default()
        })
        .expect("server");
        LocalAvatarOutput::new(server, authority())
    }

    #[test]
    fn local_output_never_fails_or_blocks_when_the_renderer_is_absent() {
        let mut out = local_output();
        // Availability is reported through connect(), not through execute().
        assert!(out.connect().is_err());
        let started = std::time::Instant::now();
        for generation in 0..1000 {
            let viseme = AvatarPlaybackCommand::Viseme {
                generation,
                at_ms: 0,
                viseme: Viseme::Aa,
                weight: 0.5,
            };
            assert!(out.execute(&viseme).is_ok());
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // Unknown gaze target and out-of-range values have no wire form.
        let gaze = AvatarPlaybackCommand::Gaze {
            generation: 1,
            at_ms: 0,
            target: "ceiling".to_owned(),
        };
        assert!(out.execute(&gaze).is_ok());
        let bad = AvatarPlaybackCommand::Viseme {
            generation: 1,
            at_ms: 0,
            viseme: Viseme::Aa,
            weight: 7.0,
        };
        assert!(out.execute(&bad).is_ok());
        let health = out.health();
        assert_eq!(health.dropped_count(DropReason::NotReady), 1000);
        assert_eq!(health.dropped_count(DropReason::Unmapped), 2);
        assert_eq!(health.sent, 0);
    }

    #[test]
    fn pack_presets_are_scaled_by_level_and_clamped() {
        let expr = |preset: &str, intensity: f64| AvatarPlaybackCommand::Expression {
            generation: 1,
            at_ms: 0,
            preset: preset.to_owned(),
            intensity,
        };
        let gesture = |preset: &str, amplitude: f64| AvatarPlaybackCommand::Gesture {
            generation: 1,
            at_ms: 0,
            preset: preset.to_owned(),
            amplitude,
        };
        assert_eq!(
            LocalAvatarOutput::wire_for(&expr("surprise_medium", 1.0)),
            Ok(WireCommand::Expression {
                preset: "surprise".to_owned(),
                intensity: 0.75
            })
        );
        assert_eq!(
            LocalAvatarOutput::wire_for(&gesture("nod_small", 2.0)),
            Ok(WireCommand::Gesture {
                preset: "nod".to_owned(),
                amplitude: 1.0
            })
        );
        // Out-of-range input is not rescued by attenuation.
        assert_eq!(
            LocalAvatarOutput::wire_for(&expr("smile_soft", 1.5)),
            Err(Unsent::Invalid)
        );
        assert_eq!(
            LocalAvatarOutput::wire_for(&gesture("recoil_small", 1.0)),
            Err(Unsent::Preset("recoil_small".to_owned()))
        );
    }

    #[test]
    fn unmapped_presets_never_fail_and_are_counted_by_name() {
        let mut out = local_output();
        for preset in ["thinking_soft", "recoil_small", "recoil_small"] {
            let command = AvatarPlaybackCommand::Expression {
                generation: 1,
                at_ms: 0,
                preset: preset.to_owned(),
                intensity: 0.5,
            };
            assert!(out.execute(&command).is_ok());
        }
        // Hostile names stay bounded.
        for n in 0..100 {
            let command = AvatarPlaybackCommand::Gesture {
                generation: 1,
                at_ms: 0,
                preset: format!("made_up_{n}"),
                amplitude: 1.0,
            };
            assert!(out.execute(&command).is_ok());
        }
        let health = out.health();
        assert_eq!(health.unmapped_presets["thinking_soft"], 1);
        assert_eq!(health.unmapped_presets["recoil_small"], 2);
        assert_eq!(
            health.unmapped_presets.len(),
            aivtuber_adapters::MAX_TRACKED_UNMAPPED_PRESETS
        );
        assert_eq!(health.unmapped_preset_overflow, 70);
        assert_eq!(health.dropped_count(DropReason::UnmappedPreset), 103);
        assert_eq!(health.sent, 0);
    }

    #[test]
    fn avatar_outputs_name_themselves_for_health_text() {
        assert_eq!(local_output().name(), "local avatar renderer");
        assert_eq!(output(None).name(), "VTube Studio");
        assert_eq!(NoopAvatarOutput.name(), "no avatar output");
    }

    #[test]
    fn local_output_maps_every_command_to_the_wire_form() {
        let cases = [
            (
                AvatarPlaybackCommand::Reset {
                    generation: 1,
                    at_ms: 0,
                },
                Ok(WireCommand::Reset {}),
            ),
            (
                AvatarPlaybackCommand::Gaze {
                    generation: 1,
                    at_ms: 0,
                    target: "chat".to_owned(),
                },
                Ok(WireCommand::Gaze {
                    target: GazeTarget::Chat,
                }),
            ),
            (
                AvatarPlaybackCommand::Expression {
                    generation: 1,
                    at_ms: 0,
                    preset: "joy".to_owned(),
                    intensity: 0.4,
                },
                Ok(WireCommand::Expression {
                    preset: "joy".to_owned(),
                    intensity: 0.4,
                }),
            ),
        ];
        for (command, expected) in cases {
            assert_eq!(LocalAvatarOutput::wire_for(&command), expected);
        }
    }

    fn parameters(action: &aivtuber_domain::AuthorizedAvatarAction) -> BTreeMap<String, f64> {
        assert_eq!(action.action().action, "parameters.inject");
        action.action().parameters.clone()
    }

    #[test]
    fn every_canonical_viseme_maps_to_one_vts_parameter_or_none() {
        let out = output(None);
        for (viseme, active) in [
            (Viseme::Aa, Some("VisemeA")),
            (Viseme::Ih, Some("VisemeI")),
            (Viseme::Ou, Some("VisemeU")),
            (Viseme::Ee, Some("VisemeE")),
            (Viseme::Oh, Some("VisemeO")),
            (Viseme::Neutral, None),
        ] {
            let command = AvatarPlaybackCommand::Viseme {
                generation: 1,
                at_ms: 0,
                viseme,
                weight: 0.5,
            };
            let actions = out.actions_for(&command).expect("actions");
            assert_eq!(actions.len(), 1);
            let parameters = parameters(&actions[0]);
            assert_eq!(parameters.len(), 5, "all five mouth shapes are addressed");
            for (name, value) in &parameters {
                let expected = if Some(name.as_str()) == active {
                    0.5
                } else {
                    0.0
                };
                assert_eq!(*value, expected, "{viseme:?} {name}");
            }
        }
    }

    #[test]
    fn reset_zeroes_mouth_and_triggers_the_configured_hotkey_only_when_set() {
        let reset = AvatarPlaybackCommand::Reset {
            generation: 3,
            at_ms: 10,
        };

        let without = output(None).actions_for(&reset).expect("actions");
        assert_eq!(without.len(), 1);
        assert!(parameters(&without[0]).values().all(|v| *v == 0.0));

        let with = output(Some("ClearExpressions"))
            .actions_for(&reset)
            .expect("actions");
        assert_eq!(with.len(), 2);
        assert!(parameters(&with[0]).values().all(|v| *v == 0.0));
        assert_eq!(with[1].action().action, "hotkey.trigger:ClearExpressions");
    }
}
