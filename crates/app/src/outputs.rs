use crate::{AudioOutput, AvatarOutput, StreamOutput};
use aivtuber_adapters::{AudioPlayRequest, AudioPlayer, ObsWebSocketAdapter, VTubeStudioAdapter};
use aivtuber_domain::{
    AuthenticatedControl, AuthorizedStreamAction, AvatarAction, EngineError,
    authorize_avatar_action,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtsPlaybackConfig {
    pub expression_hotkey_prefix: String,
    pub gesture_hotkey_prefix: String,
    pub gaze_hotkey_prefix: String,
    pub viseme_parameter_prefix: String,
    pub known_visemes: Vec<String>,
}

impl Default for VtsPlaybackConfig {
    fn default() -> Self {
        Self {
            expression_hotkey_prefix: String::new(),
            gesture_hotkey_prefix: String::new(),
            gaze_hotkey_prefix: "gaze.".to_owned(),
            viseme_parameter_prefix: "Viseme".to_owned(),
            known_visemes: ["A", "I", "U", "E", "O", "N"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
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
    fn connect(&mut self) -> Result<(), EngineError> {
        self.adapter.connect()
    }

    fn execute(&mut self, command: &AvatarPlaybackCommand) -> Result<(), EngineError> {
        let action = match command {
            AvatarPlaybackCommand::Expression { preset, .. } => self.hotkey_action(format!(
                "{}{}",
                self.config.expression_hotkey_prefix, preset
            ))?,
            AvatarPlaybackCommand::Gesture { preset, .. } => {
                self.hotkey_action(format!("{}{}", self.config.gesture_hotkey_prefix, preset))?
            }
            AvatarPlaybackCommand::Gaze { target, .. } => {
                self.hotkey_action(format!("{}{}", self.config.gaze_hotkey_prefix, target))?
            }
            AvatarPlaybackCommand::Viseme { viseme, weight, .. } => {
                let mut parameters = self
                    .config
                    .known_visemes
                    .iter()
                    .map(|name| {
                        (
                            format!("{}{}", self.config.viseme_parameter_prefix, name),
                            0.0,
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                parameters.insert(
                    format!("{}{}", self.config.viseme_parameter_prefix, viseme),
                    *weight,
                );
                self.authorize(AvatarAction {
                    action: "parameters.inject".to_owned(),
                    parameters,
                })?
            }
        };
        self.adapter.execute_sync(&action)
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
