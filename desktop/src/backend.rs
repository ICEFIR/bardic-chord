use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use byteorder::{ByteOrder, LittleEndian};
use chrono::Local;
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serenity::{
    async_trait,
    client::{Context, EventHandler},
    gateway::ShardManager,
    model::{
        gateway::{GatewayIntents, Ready},
        id::{ChannelId, GuildId, UserId},
        voice::VoiceState,
    },
    Client,
};
use songbird::{
    input, CoreEvent as SongbirdCoreEvent, Event as SongbirdEvent,
    EventContext as SongbirdEventContext, EventHandler as SongbirdEventHandler, SerenityInit,
    Songbird, TrackEvent,
};
use std::{
    collections::{HashMap, VecDeque},
    env, fs, io, mem,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Condvar, Mutex as StdMutex},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{oneshot, Mutex},
    task::JoinHandle,
    time::{timeout, Duration, Instant},
};
use tracing::{debug, error, info, warn};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
pub const DISCORD_INVITE_PERMISSIONS: &str = "3146752";
const DISCORD_USER_AGENT: &str = "BardicChord/0.1.0 (+https://github.com/ICEFIR/bardic-chord)";
const LOCAL_STATE_DIR: &str = ".bardic-chord";
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: usize = 2;
const AUDIO_CHANNELS_U32: u32 = 2;
const AUDIO_BUFFER_FRAMES: usize = AUDIO_SAMPLE_RATE as usize;
const AUDIO_PREFILL_FRAMES: usize = 960;
pub const DEFAULT_AUDIO_OUTPUT_NAME: &str = "Bardic_Chord";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub discord_bot_token: String,
    pub tracked_user_id: String,
    pub guild_id: String,
    pub voice_channel_id: String,
    pub audio_output_name: String,
    pub bot_display_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            discord_bot_token: String::new(),
            tracked_user_id: String::new(),
            guild_id: String::new(),
            voice_channel_id: String::new(),
            audio_output_name: DEFAULT_AUDIO_OUTPUT_NAME.into(),
            bot_display_name: "The Amber Minstrel".into(),
        }
    }
}

impl Settings {
    fn normalized(mut self) -> Self {
        self.audio_output_name = audio_output_name_or_default(&self.audio_output_name);
        self.bot_display_name = if self.bot_display_name.trim().is_empty() {
            "The Amber Minstrel".into()
        } else {
            self.bot_display_name.trim().to_string()
        };
        self.discord_bot_token = self.discord_bot_token.trim().to_string();
        self.tracked_user_id = self.tracked_user_id.trim().to_string();
        self.guild_id = self.guild_id.trim().to_string();
        self.voice_channel_id = self.voice_channel_id.trim().to_string();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthTile {
    pub id: String,
    pub label: String,
    pub state: String,
    pub detail: String,
    pub tone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub timestamp: String,
    pub tone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub settings: Settings,
    pub health_tiles: Vec<HealthTile>,
    pub ritual_steps: Vec<String>,
    pub activity: Vec<ActivityEntry>,
    pub last_saved_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioOutputReport {
    pub active: bool,
    pub output_name: Option<String>,
    pub platform: String,
    pub started_at: Option<String>,
    pub message: String,
    pub instruction_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordGuildOption {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordVoiceChannelOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordValidationReport {
    pub bot_username: String,
    pub bot_id: String,
    pub application_name: Option<String>,
    pub application_id: Option<String>,
    pub invite_url: Option<String>,
    pub guild_name: Option<String>,
    pub guild_id: Option<String>,
    pub voice_channel_name: Option<String>,
    pub voice_channel_id: Option<String>,
    pub voice_channel_kind: Option<String>,
    pub guild_options: Vec<DiscordGuildOption>,
    pub voice_channel_options: Vec<DiscordVoiceChannelOption>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordRelayReport {
    pub active: bool,
    pub guild_name: Option<String>,
    pub guild_id: Option<String>,
    pub voice_channel_name: Option<String>,
    pub voice_channel_id: Option<String>,
    pub bot_username: Option<String>,
    pub started_at: Option<String>,
    pub message: String,
    pub instruction_steps: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordCurrentUser {
    id: String,
    username: String,
    global_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DiscordApplication {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct DiscordGuild {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct DiscordGuildSummary {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct DiscordChannel {
    id: String,
    name: String,
    #[serde(rename = "type")]
    channel_type: u8,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize)]
struct PulseClient {
    index: u32,
    #[serde(default)]
    properties: HashMap<String, String>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize)]
struct PulseSink {
    index: u32,
    name: String,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize)]
struct PulseSinkInput {
    index: u32,
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    sink: Option<u32>,
    #[serde(default)]
    properties: HashMap<String, String>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct SpotifyRouteAttempt {
    detected_inputs: usize,
    moved_inputs: usize,
    already_on_target: usize,
}

#[cfg(target_os = "linux")]
impl SpotifyRouteAttempt {
    fn summary(&self, output_name: &str) -> String {
        if self.moved_inputs > 0 {
            format!("Spotify was detected and moved into `{output_name}` automatically.")
        } else if self.already_on_target > 0 {
            format!("Spotify is already routed into `{output_name}`.")
        } else {
            "Spotify was not detected yet. Start playback in Spotify, then Bardic Chord will try again when the party starts."
                .into()
        }
    }
}

#[derive(Clone)]
struct EmittedSink {
    state: Arc<(StdMutex<EmittedSinkState>, Condvar)>,
}

struct EmittedSinkState {
    frames: VecDeque<[f32; 2]>,
}

impl EmittedSink {
    fn new() -> Self {
        Self {
            state: Arc::new((
                StdMutex::new(EmittedSinkState {
                    frames: VecDeque::with_capacity(AUDIO_BUFFER_FRAMES),
                }),
                Condvar::new(),
            )),
        }
    }

    fn push_frames(&self, frames: &[[f32; 2]]) -> bool {
        if frames.is_empty() {
            return true;
        }

        let (lock, available) = &*self.state;
        let mut state = lock.lock().expect("emitted sink poisoned");

        if frames.len() >= AUDIO_BUFFER_FRAMES {
            state.frames.clear();
            state.frames.extend(
                frames[frames.len().saturating_sub(AUDIO_BUFFER_FRAMES)..]
                    .iter()
                    .copied(),
            );
        } else {
            let overflow = state
                .frames
                .len()
                .saturating_add(frames.len())
                .saturating_sub(AUDIO_BUFFER_FRAMES);
            for _ in 0..overflow {
                state.frames.pop_front();
            }
            state.frames.extend(frames.iter().copied());
        }

        available.notify_one();
        true
    }
}

impl io::Read for EmittedSink {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let sample_size = mem::size_of::<f32>() * AUDIO_CHANNELS;

        if buffer.len() < sample_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "EmittedSink read buffer is too small to hold one stereo sample.",
            ));
        }

        let frames_requested = buffer.len() / sample_size;
        let prefill_target = frames_requested.min(AUDIO_PREFILL_FRAMES).max(1);
        let (lock, available) = &*self.state;
        let mut state = lock.lock().expect("emitted sink poisoned");
        let mut bytes_written = 0usize;
        let silence = [0.0_f32, 0.0_f32];

        while bytes_written + (sample_size - 1) < buffer.len() {
            if state.frames.is_empty() && bytes_written == 0 {
                let wait = available
                    .wait_timeout_while(
                        state,
                        Duration::from_millis(40),
                        |state| state.frames.len() < prefill_target,
                    )
                    .expect("emitted sink wait poisoned");
                state = wait.0;
            }

            let sample = state.frames.pop_front().unwrap_or(silence);

            LittleEndian::write_f32_into(
                &sample,
                &mut buffer[bytes_written..(bytes_written + sample_size)],
            );
            bytes_written += sample_size;
        }

        Ok(bytes_written)
    }
}

impl io::Seek for EmittedSink {
    fn seek(&mut self, _pos: io::SeekFrom) -> io::Result<u64> {
        unreachable!("EmittedSink is not seekable")
    }
}

impl songbird::input::core::io::MediaSource for EmittedSink {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

struct LinuxAudioOutputRuntime {
    module_id: String,
    sink_name: String,
    capture_child: Child,
}

enum AudioPlatformRuntime {
    #[cfg(target_os = "linux")]
    Linux(LinuxAudioOutputRuntime),
}

struct AudioOutputRuntime {
    platform: AudioPlatformRuntime,
    task_handle: JoinHandle<()>,
    report: AudioOutputReport,
    emitted_sink: EmittedSink,
}

struct DiscordRelayRuntime {
    manager: Arc<Songbird>,
    shard_manager: Arc<ShardManager>,
    client_handle: JoinHandle<()>,
    guild_id: GuildId,
    report: Arc<StdMutex<DiscordRelayReport>>,
}

struct AppRuntimeState {
    audio_output: Mutex<Option<AudioOutputRuntime>>,
    discord_relay: Mutex<Option<DiscordRelayRuntime>>,
}

struct DiscordReadyHandler {
    startup_channel_signal: Arc<Mutex<Option<oneshot::Sender<Option<ChannelId>>>>>,
    manager: Arc<Songbird>,
    follow_guild_id: GuildId,
    followed_user_id: Option<UserId>,
}

struct RelayTrackLogger {
    report: Arc<StdMutex<DiscordRelayReport>>,
    event: TrackEvent,
    log_path: PathBuf,
}

struct RelayDriverLogger {
    report: Arc<StdMutex<DiscordRelayReport>>,
    event: SongbirdCoreEvent,
    log_path: PathBuf,
}

#[async_trait]
impl EventHandler for DiscordReadyHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(
            bot_user = %ready.user.name,
            bot_id = %ready.user.id,
            "Discord gateway reported the bot as ready"
        );
    }

    async fn cache_ready(&self, ctx: Context, _guilds: Vec<GuildId>) {
        let tracked_user_channel = self.followed_user_id.and_then(|followed_user_id| {
            ctx.cache.guild(self.follow_guild_id).and_then(|guild| {
                guild
                    .voice_states
                    .get(&followed_user_id)
                    .and_then(|state| state.channel_id)
            })
        });

        info!(
            guild_id = %self.follow_guild_id,
            followed_user_id = ?self.followed_user_id.map(|value| value.get()),
            startup_channel_id = ?tracked_user_channel.map(|value| value.get()),
            "Discord guild cache is ready for Bardic Chord relay startup"
        );

        if let Some(signal) = self.startup_channel_signal.lock().await.take() {
            let _ = signal.send(tracked_user_channel);
        }
    }

    async fn voice_state_update(&self, _ctx: Context, _old: Option<VoiceState>, new: VoiceState) {
        let Some(followed_user_id) = self.followed_user_id else {
            return;
        };

        if new.user_id != followed_user_id || new.guild_id != Some(self.follow_guild_id) {
            return;
        }

        let Some(channel_id) = new.channel_id else {
            info!(
                guild_id = %self.follow_guild_id,
                followed_user_id = %followed_user_id,
                "Tracked user left voice; Bardic Chord will stay in the current fallback channel"
            );
            return;
        };

        info!(
            guild_id = %self.follow_guild_id,
            followed_user_id = %followed_user_id,
            channel_id = %channel_id,
            "Tracked user moved voice channels; Bardic Chord is following"
        );

        match self.manager.join(self.follow_guild_id, channel_id).await {
            Ok(_) => info!(
                guild_id = %self.follow_guild_id,
                channel_id = %channel_id,
                "Bardic Chord followed the tracked user into a new voice channel"
            ),
            Err(error) => warn!(
                guild_id = %self.follow_guild_id,
                channel_id = %channel_id,
                %error,
                "Bardic Chord failed to follow the tracked user into a new voice channel"
            ),
        }
    }
}

#[async_trait]
impl SongbirdEventHandler for RelayTrackLogger {
    async fn act(&self, ctx: &SongbirdEventContext<'_>) -> Option<SongbirdEvent> {
        let track_count = match ctx {
            SongbirdEventContext::Track(tracks) => tracks.len(),
            _ => 0,
        };

        match self.event {
            TrackEvent::Playable => {
                info!(
                    track_count,
                    "Discord relay track became playable and is ready to forward desktop audio"
                );
                update_discord_relay_report(&self.report, |report| {
                    report.active = true;
                    report.message =
                        "The party is live. Discord accepted the audio stream and Bardic Chord is forwarding desktop audio."
                            .into();
                });
            }
            TrackEvent::End => {
                warn!(
                    track_count,
                    log_path = %self.log_path.display(),
                    "Discord relay track ended unexpectedly"
                );
                update_discord_relay_report(&self.report, |report| {
                    report.active = false;
                    report.message = format!(
                        "The audio stream stopped unexpectedly. Discord is still connected, but you should start the party again or inspect `{}`.",
                        self.log_path.display()
                    );
                });
            }
            TrackEvent::Error => {
                error!(
                    track_count,
                    log_path = %self.log_path.display(),
                    "Discord relay track hit a runtime error"
                );
                update_discord_relay_report(&self.report, |report| {
                    report.active = false;
                    report.message = format!(
                        "The party hit an audio error after joining Discord. Start the party again or inspect `{}`.",
                        self.log_path.display()
                    );
                });
            }
            other => {
                debug!(event = ?other, track_count, "Discord relay track event fired");
            }
        }

        None
    }
}

#[async_trait]
impl SongbirdEventHandler for RelayDriverLogger {
    async fn act(&self, _ctx: &SongbirdEventContext<'_>) -> Option<SongbirdEvent> {
        match self.event {
            SongbirdCoreEvent::DriverConnect => {
                info!("Discord voice driver connected successfully");
                update_discord_relay_report(&self.report, |report| {
                    report.active = true;
                    report.message =
                        "Bardic Chord is connected to Discord and waiting for desktop audio."
                            .into();
                });
            }
            SongbirdCoreEvent::DriverReconnect => {
                info!("Discord voice driver reconnected successfully");
                update_discord_relay_report(&self.report, |report| {
                    report.active = true;
                    report.message =
                        "Bardic Chord reconnected to Discord and is ready for music again.".into();
                });
            }
            SongbirdCoreEvent::DriverDisconnect => {
                warn!(
                    log_path = %self.log_path.display(),
                    "Discord voice driver disconnected"
                );
                update_discord_relay_report(&self.report, |report| {
                    report.active = false;
                    report.message = format!(
                        "Discord voice disconnected. Relaunch the party or inspect `{}`.",
                        self.log_path.display()
                    );
                });
            }
            other => {
                debug!(event = ?other, "Songbird core event fired for Bardic Chord");
            }
        }

        None
    }
}

#[derive(Clone)]
struct AppPaths {
    state_dir: PathBuf,
}

impl AppPaths {
    fn new() -> Result<Self, String> {
        let state_dir = env::current_dir()
            .map_err(|error| {
                format!("failed to resolve the Bardic Chord working directory: {error}")
            })?
            .join(LOCAL_STATE_DIR);

        Ok(Self { state_dir })
    }

    fn settings_path(&self) -> PathBuf {
        self.state_dir.join("settings.json")
    }

    fn log_path(&self) -> PathBuf {
        self.state_dir.join("bardic-chord.log")
    }
}

pub struct Backend {
    paths: AppPaths,
    runtime_state: AppRuntimeState,
}

pub fn discord_invite_url_from_token(bot_token: &str) -> Option<String> {
    let application_id = discord_application_id_from_token(bot_token)?;
    Some(format!(
        "https://discord.com/api/oauth2/authorize?client_id={application_id}&permissions={DISCORD_INVITE_PERMISSIONS}&scope=bot%20applications.commands"
    ))
}

impl Backend {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            paths: AppPaths::new()?,
            runtime_state: AppRuntimeState {
                audio_output: Mutex::new(None),
                discord_relay: Mutex::new(None),
            },
        })
    }

    pub fn load_app_snapshot(&self) -> Result<AppSnapshot, String> {
        let path = self.paths.settings_path();
        if path.exists() {
            let snapshot = read_snapshot(&path)?;
            Ok(AppSnapshot::from_settings(
                snapshot.settings.normalized(),
                snapshot.last_saved_at,
            ))
        } else {
            Ok(AppSnapshot::from_settings(Settings::default(), None))
        }
    }

    pub fn save_settings(&self, settings: Settings) -> Result<AppSnapshot, String> {
        let path = self.paths.settings_path();
        let parent = path
            .parent()
            .ok_or_else(|| "failed to determine the settings directory".to_string())?;

        fs::create_dir_all(parent).map_err(|error| {
            format!("failed to create Bardic Chord settings directory: {error}")
        })?;

        let snapshot = AppSnapshot::from_settings(settings.normalized(), Some(format_now()));
        let payload = serde_json::to_string_pretty(&snapshot)
            .map_err(|error| format!("failed to serialize Bardic Chord settings: {error}"))?;

        fs::write(path, payload)
            .map_err(|error| format!("failed to write Bardic Chord settings: {error}"))?;

        Ok(snapshot)
    }

    pub async fn validate_discord_setup(
        &self,
        settings: Settings,
    ) -> Result<DiscordValidationReport, String> {
        let bot_token = normalize_discord_bot_token(&settings.discord_bot_token);
        if bot_token.is_empty() {
            return Err("Discord bot token is required before Bardic Chord can connect.".into());
        }

        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| format!("failed to build Discord client: {error}"))?;

        let current_user: DiscordCurrentUser = discord_get(&client, bot_token, "users/@me").await?;
        let application =
            discord_get::<DiscordApplication>(&client, bot_token, "oauth2/applications/@me")
                .await
                .ok();

        let guild_options =
            discord_get::<Vec<DiscordGuildSummary>>(&client, bot_token, "users/@me/guilds")
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|guild| DiscordGuildOption {
                    id: guild.id,
                    name: guild.name,
                })
                .collect::<Vec<_>>();

        let resolved_guild_id = if settings.guild_id.trim().is_empty() {
            guild_options.first().map(|guild| guild.id.clone())
        } else {
            Some(settings.guild_id.trim().to_string())
        };

        let guild = match resolved_guild_id.as_deref() {
            Some(guild_id) => Some(
                discord_get::<DiscordGuild>(&client, bot_token, &format!("guilds/{guild_id}"))
                    .await?,
            ),
            None => None,
        };

        let voice_channel_options = match resolved_guild_id.as_deref() {
            Some(guild_id) => discord_get::<Vec<DiscordChannel>>(
                &client,
                bot_token,
                &format!("guilds/{guild_id}/channels"),
            )
            .await?
            .into_iter()
            .filter(|channel| matches!(channel.channel_type, 2 | 13))
            .map(|channel| DiscordVoiceChannelOption {
                id: channel.id,
                name: channel.name,
                kind: voice_channel_kind_name(channel.channel_type).to_string(),
            })
            .collect::<Vec<_>>(),
            None => Vec::new(),
        };

        let resolved_voice_channel_id = if settings.voice_channel_id.trim().is_empty() {
            voice_channel_options
                .first()
                .map(|channel| channel.id.clone())
        } else {
            Some(settings.voice_channel_id.trim().to_string())
        };

        let voice_channel = match resolved_voice_channel_id.as_deref() {
            Some(channel_id) => {
                if let Some(channel) = voice_channel_options
                    .iter()
                    .find(|channel| channel.id == channel_id)
                {
                    Some(DiscordChannel {
                        id: channel.id.clone(),
                        name: channel.name.clone(),
                        channel_type: match channel.kind.as_str() {
                            "Stage Voice" => 13,
                            _ => 2,
                        },
                    })
                } else {
                    Some(
                        discord_get::<DiscordChannel>(
                            &client,
                            bot_token,
                            &format!("channels/{channel_id}"),
                        )
                        .await?,
                    )
                }
            }
            None => None,
        };

        let application_id = application
            .as_ref()
            .map(|app| app.id.clone())
            .unwrap_or_else(|| current_user.id.clone());
        let application_name = application.as_ref().map(|app| app.name.clone());
        let display_name = current_user
            .global_name
            .clone()
            .unwrap_or_else(|| current_user.username.clone());

        let invite_url = discord_invite_url_from_token(bot_token).or_else(|| {
            Some(format!(
                "https://discord.com/api/oauth2/authorize?client_id={application_id}&permissions={DISCORD_INVITE_PERMISSIONS}&scope=bot%20applications.commands"
            ))
        });

        let message = match (&guild, &voice_channel) {
            (Some(guild), Some(channel)) => format!(
                "{display_name} is connected. Bardic Chord found the server `{}` and the voice channel `{}`.",
                guild.name, channel.name
            ),
            (Some(guild), None) => format!(
                "{display_name} is connected. Bardic Chord found the server `{}`. Choose a voice channel next.",
                guild.name
            ),
            _ => format!(
                "{display_name} is connected. If the bot is not in your server yet, open the invite page and Bardic Chord will check again."
            ),
        };

        Ok(DiscordValidationReport {
            bot_username: current_user.username,
            bot_id: current_user.id,
            application_name,
            application_id: Some(application_id),
            invite_url,
            guild_name: guild.as_ref().map(|value| value.name.clone()),
            guild_id: guild
                .as_ref()
                .map(|value| value.id.clone())
                .or(resolved_guild_id),
            voice_channel_name: voice_channel.as_ref().map(|value| value.name.clone()),
            voice_channel_id: voice_channel
                .as_ref()
                .map(|value| value.id.clone())
                .or(resolved_voice_channel_id),
            voice_channel_kind: voice_channel
                .as_ref()
                .map(|value| voice_channel_kind_name(value.channel_type).to_string()),
            guild_options,
            voice_channel_options,
            message,
        })
    }

    pub async fn get_audio_output_status(&self) -> AudioOutputReport {
        let mut guard = self.runtime_state.audio_output.lock().await;

        if let Some(runtime) = guard.as_mut() {
            if runtime.task_handle.is_finished() && runtime.report.active {
                warn!(
                    log_path = %self.paths.log_path().display(),
                    "local audio capture task finished unexpectedly"
                );
                runtime.report = build_audio_output_report(
                    false,
                    runtime.report.output_name.clone(),
                    platform_name().into(),
                    runtime.report.started_at.clone(),
                    format!(
                        "The desktop audio output stopped unexpectedly. Check `{}` and prepare the output again.",
                        self.paths.log_path().display()
                    ),
                );
            }

            runtime.report.clone()
        } else {
            build_audio_output_report(
                false,
                Some(DEFAULT_AUDIO_OUTPUT_NAME.into()),
                platform_name().into(),
                None,
                default_audio_idle_message(),
            )
        }
    }

    pub async fn prepare_audio_output(
        &self,
        settings: Settings,
    ) -> Result<AudioOutputReport, String> {
        self.stop_relay_state().await?;
        self.stop_audio_output_state().await?;

        #[cfg(target_os = "linux")]
        {
            let output_name = audio_output_name_or_default(&settings.audio_output_name);
            let sink_name = sanitize_audio_output_name(&output_name);
            let emitted_sink = EmittedSink::new();
            let (platform, task_handle) = start_linux_audio_output(
                output_name.clone(),
                emitted_sink.clone(),
                self.paths.log_path(),
            )
            .await?;
            let route_attempt = match try_route_spotify_to_linux_sink(&sink_name, &output_name).await
            {
                Ok(attempt) => Some(attempt),
                Err(error) => {
                    warn!(?error, sink_name, "failed to auto-route Spotify during audio setup");
                    None
                }
            };
            let message = route_attempt
                .as_ref()
                .map(|attempt| {
                    format!(
                        "Desktop audio output is ready on Linux. {}",
                        attempt.summary(&output_name)
                    )
                })
                .unwrap_or_else(|| {
                    "Desktop audio output is ready on Linux. If Spotify does not jump over automatically, move it to the BardicChord output from your sound settings and then start the party."
                        .into()
                });

            let report = build_audio_output_report(
                true,
                Some(output_name),
                platform_name().into(),
                Some(format_now()),
                message,
            );

            let mut guard = self.runtime_state.audio_output.lock().await;
            *guard = Some(AudioOutputRuntime {
                platform,
                task_handle,
                report: report.clone(),
                emitted_sink,
            });

            Ok(report)
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = settings;
            Err(format!(
                "Desktop audio output preparation is not implemented on {} yet. The next backend should use the same guided flow but with a native loopback capture path.",
                platform_name()
            ))
        }
    }

    pub async fn stop_audio_output(&self) -> Result<AudioOutputReport, String> {
        self.stop_relay_state().await?;
        self.stop_audio_output_state().await?;

        Ok(build_audio_output_report(
            false,
            Some(DEFAULT_AUDIO_OUTPUT_NAME.into()),
            platform_name().into(),
            None,
            "Desktop audio output is off. Prepare it again when you want Bardic Chord back in the sound routing list.",
        ))
    }

    pub async fn get_discord_relay_status(&self) -> DiscordRelayReport {
        let mut guard = self.runtime_state.discord_relay.lock().await;

        if let Some(runtime) = guard.as_mut() {
            if runtime.client_handle.is_finished() {
                warn!(
                    log_path = %self.paths.log_path().display(),
                    "Discord relay client task is no longer running"
                );
                update_discord_relay_report(&runtime.report, |report| {
                    if report.active {
                        report.active = false;
                        report.message = format!(
                            "Discord relay stopped unexpectedly. Check `{}` and launch the party again.",
                            self.paths.log_path().display()
                        );
                    }
                });
            }

            clone_discord_relay_report(&runtime.report)
        } else {
            build_relay_report(
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                "Party dormant. Prepare the desktop audio output first, then arm the Discord voice bridge.",
            )
        }
    }

    pub async fn start_discord_relay(
        &self,
        settings: Settings,
    ) -> Result<DiscordRelayReport, String> {
        let bot_token = settings.discord_bot_token.trim();
        if bot_token.is_empty() {
            return Err("Discord bot token is required before the relay can awaken.".into());
        }

        let guild_id = GuildId::new(parse_snowflake(&settings.guild_id, "guild ID")?);
        let channel_id = ChannelId::new(parse_snowflake(
            &settings.voice_channel_id,
            "voice channel ID",
        )?);
        let followed_user_id = if settings.tracked_user_id.trim().is_empty() {
            None
        } else {
            Some(UserId::new(parse_snowflake(
                &settings.tracked_user_id,
                "tracked user ID",
            )?))
        };

        let emitted_sink = {
            let guard = self.runtime_state.audio_output.lock().await;
            let runtime = guard.as_ref().ok_or_else(|| {
                "Prepare the desktop audio output before starting the Discord relay.".to_string()
            })?;
            runtime.emitted_sink.clone()
        };

        self.stop_relay_state().await?;

        info!(
            guild_id = %guild_id,
            configured_channel_id = %channel_id,
            followed_user_id = ?followed_user_id.as_ref().map(|value| value.get()),
            log_path = %self.paths.log_path().display(),
            "starting Discord relay runtime"
        );

        let route = self.validate_discord_setup(settings.clone()).await.ok();
        let follow_enabled = followed_user_id.is_some();
        let output_name = audio_output_name_or_default(&settings.audio_output_name);
        let sink_name = sanitize_audio_output_name(&output_name);

        #[cfg(target_os = "linux")]
        {
            match try_route_spotify_to_linux_sink(&sink_name, &output_name).await {
                Ok(attempt) => {
                    info!(
                        sink_name,
                        output_name,
                        detected_inputs = attempt.detected_inputs,
                        moved_inputs = attempt.moved_inputs,
                        already_on_target = attempt.already_on_target,
                        "checked Spotify routing before starting the Discord relay"
                    );
                }
                Err(error) => {
                    warn!(
                        sink_name,
                        output_name,
                        ?error,
                        "failed to check Spotify routing before starting the Discord relay"
                    );
                }
            }
        }

        let (startup_channel_tx, startup_channel_rx) = oneshot::channel::<Option<ChannelId>>();
        let startup_channel_signal = Arc::new(Mutex::new(Some(startup_channel_tx)));

        let manager = Songbird::serenity();
        let mut client = Client::builder(
            bot_token,
            GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES,
        )
        .event_handler(DiscordReadyHandler {
            startup_channel_signal: startup_channel_signal.clone(),
            manager: manager.clone(),
            follow_guild_id: guild_id,
            followed_user_id,
        })
        .register_songbird_with(manager.clone())
        .await
        .map_err(|error| format!("failed to create Discord client: {error}"))?;

        let shard_manager = client.shard_manager.clone();
        let client_handle = tokio::spawn(async move {
            let _ = client.start().await;
        });

        info!("waiting for Discord guild cache before finalizing relay join");

        let tracked_user_start_channel = timeout(Duration::from_secs(15), startup_channel_rx)
            .await
            .map_err(|_| {
                "Timed out while waiting for the Discord guild cache to become ready.".to_string()
            })?
            .map_err(|_| {
                "Discord gateway closed before the relay finished loading the startup voice state."
                    .to_string()
            })?;
        let tracked_user_start_channel_id =
            tracked_user_start_channel.as_ref().map(|value| value.get());
        let tracked_user_snapped = tracked_user_start_channel.is_some();

        let initial_channel_id = tracked_user_start_channel.unwrap_or(channel_id);

        info!(
            joined_channel_id = %initial_channel_id,
            followed_user_channel = ?tracked_user_start_channel_id,
            "Discord relay resolved its initial voice channel"
        );

        manager
            .join(guild_id, initial_channel_id)
            .await
            .map_err(|error| format!("failed to join the Discord voice channel: {error}"))?;

        info!(
            guild_id = %guild_id,
            channel_id = %initial_channel_id,
            "Discord relay joined the requested voice channel"
        );

        let call = manager.get(guild_id).ok_or_else(|| {
            "Discord voice call did not become available after joining.".to_string()
        })?;

        let raw_input: songbird::input::Input =
            input::RawAdapter::new(emitted_sink, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS_U32).into();
        let raw_input = match raw_input {
            songbird::input::Input::Live(live, _) => {
                let promoted = live
                    .promote(
                        songbird::input::codecs::get_codec_registry(),
                        songbird::input::codecs::get_probe(),
                    )
                    .map_err(|error| {
                        error!(
                            log_path = %self.paths.log_path().display(),
                            ?error,
                            "failed to promote the Discord raw PCM input before playback"
                        );
                        format!(
                            "Bardic Chord could not prepare the music stream for Discord. Check `{}` for details.",
                            self.paths.log_path().display()
                        )
                    })?;
                songbird::input::Input::Live(promoted, None)
            }
            songbird::input::Input::Lazy(_) => {
                return Err(
                    "Bardic Chord created a lazy audio input unexpectedly and could not arm the relay."
                        .into(),
                )
            }
        };

        let actual_voice_channel_id = initial_channel_id.get().to_string();
        let actual_voice_channel_name = route.as_ref().and_then(|value| {
            value
                .voice_channel_options
                .iter()
                .find(|channel| channel.id == actual_voice_channel_id)
                .map(|channel| channel.name.clone())
                .or_else(|| value.voice_channel_name.clone())
        });
        let report = Arc::new(StdMutex::new(build_relay_report(
            true,
            route.as_ref().and_then(|value| value.guild_name.clone()),
            route.as_ref().and_then(|value| value.guild_id.clone()),
            actual_voice_channel_name,
            Some(actual_voice_channel_id),
            route.as_ref().map(|value| value.bot_username.clone()),
            Some(format_now()),
            if follow_enabled && tracked_user_snapped {
                "The party is live. Bardic Chord joined the tracked user's channel on startup and will keep following them."
            } else if follow_enabled {
                "The party is live. Bardic Chord joined the selected channel and will follow the tracked user when they move."
            } else {
                "The party is live. Bardic Chord joined the voice channel and is ready to send music."
            },
        )));
        let log_path = self.paths.log_path();

        {
            let mut handler = call.lock().await;
            handler.add_global_event(
                SongbirdEvent::Core(SongbirdCoreEvent::DriverConnect),
                RelayDriverLogger {
                    report: report.clone(),
                    event: SongbirdCoreEvent::DriverConnect,
                    log_path: log_path.clone(),
                },
            );
            handler.add_global_event(
                SongbirdEvent::Core(SongbirdCoreEvent::DriverReconnect),
                RelayDriverLogger {
                    report: report.clone(),
                    event: SongbirdCoreEvent::DriverReconnect,
                    log_path: log_path.clone(),
                },
            );
            handler.add_global_event(
                SongbirdEvent::Core(SongbirdCoreEvent::DriverDisconnect),
                RelayDriverLogger {
                    report: report.clone(),
                    event: SongbirdCoreEvent::DriverDisconnect,
                    log_path: log_path.clone(),
                },
            );

            let track = handler.play_only_input(raw_input);
            add_relay_track_event(
                &track,
                TrackEvent::Playable,
                report.clone(),
                log_path.clone(),
            );
            add_relay_track_event(&track, TrackEvent::End, report.clone(), log_path.clone());
            add_relay_track_event(&track, TrackEvent::Error, report.clone(), log_path.clone());
        }

        info!("Discord relay validated the raw audio stream format");
        info!("Discord relay attached desktop PCM to the voice call");

        let mut guard = self.runtime_state.discord_relay.lock().await;
        *guard = Some(DiscordRelayRuntime {
            manager,
            shard_manager,
            client_handle,
            guild_id,
            report: report.clone(),
        });

        Ok(clone_discord_relay_report(&report))
    }

    pub async fn stop_discord_relay(&self) -> Result<DiscordRelayReport, String> {
        self.stop_relay_state().await?;

        Ok(build_relay_report(
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            "Party silenced. Start it again when you want Bardic Chord back in voice.",
        ))
    }

    pub async fn can_stop_party(&self) -> bool {
        self.runtime_state.discord_relay.lock().await.is_some()
            || self.runtime_state.audio_output.lock().await.is_some()
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.stop_relay_state().await?;
        self.stop_audio_output_state().await?;
        Ok(())
    }

    pub fn open_url(&self, url: &str) -> Result<(), String> {
        open::that_detached(url).map_err(|error| format!("failed to open URL: {error}"))
    }

    async fn stop_audio_output_state(&self) -> Result<(), String> {
        let existing = {
            let mut guard = self.runtime_state.audio_output.lock().await;
            guard.take()
        };
        stop_existing_audio_output(existing).await
    }

    async fn stop_relay_state(&self) -> Result<(), String> {
        let existing = {
            let mut guard = self.runtime_state.discord_relay.lock().await;
            guard.take()
        };
        let _ = stop_existing_relay(existing).await?;
        Ok(())
    }
}

impl AppSnapshot {
    fn from_settings(settings: Settings, last_saved_at: Option<String>) -> Self {
        let discord_ready = !settings.discord_bot_token.trim().is_empty();
        let audio_name_ready = !settings.audio_output_name.trim().is_empty();
        let routing_ready =
            !settings.guild_id.trim().is_empty() && !settings.voice_channel_id.trim().is_empty();
        let identity_ready = !settings.bot_display_name.trim().is_empty();
        let audio_supported = cfg!(target_os = "linux");
        let ready_to_start = discord_ready && audio_name_ready && routing_ready && identity_ready;

        let health_tiles = vec![
            HealthTile {
                id: "discord".into(),
                label: "Discord".into(),
                state: if discord_ready {
                    "Ready".into()
                } else {
                    "Needed".into()
                },
                detail: if discord_ready {
                    "The bot token is saved and Bardic Chord can connect to Discord.".into()
                } else {
                    "Add the Discord bot token to continue setup.".into()
                },
                tone: if discord_ready {
                    "ready".into()
                } else {
                    "waiting".into()
                },
            },
            HealthTile {
                id: "audio".into(),
                label: "Desktop Audio".into(),
                state: if audio_supported && audio_name_ready {
                    "Ready".into()
                } else if audio_supported {
                    "Needed".into()
                } else {
                    "Later".into()
                },
                detail: if audio_supported && audio_name_ready {
                    "A local desktop output can be prepared for Spotify routing on this machine."
                        .into()
                } else if audio_supported {
                    "Name the local desktop output before preparing audio capture.".into()
                } else {
                    "This build needs a future native loopback backend for the current platform."
                        .into()
                },
                tone: if audio_supported && audio_name_ready {
                    "ready".into()
                } else if audio_supported {
                    "caution".into()
                } else {
                    "waiting".into()
                },
            },
            HealthTile {
                id: "routing".into(),
                label: "Discord Channel".into(),
                state: if routing_ready {
                    "Ready".into()
                } else {
                    "Needed".into()
                },
                detail: if routing_ready {
                    if settings.tracked_user_id.trim().is_empty() {
                        "The server and voice channel are chosen.".into()
                    } else {
                        "The server and voice channel are chosen, and follow mode is on.".into()
                    }
                } else {
                    "Choose the server and voice channel where the music should play.".into()
                },
                tone: if routing_ready {
                    "ready".into()
                } else {
                    "caution".into()
                },
            },
            HealthTile {
                id: "launch".into(),
                label: "Ready to Start".into(),
                state: if ready_to_start {
                    "Ready".into()
                } else {
                    "Waiting".into()
                },
                detail: if ready_to_start {
                    "Setup is complete. Prepare the audio output, route Spotify to it, and start the party."
                        .into()
                } else {
                    "Finish Discord, audio naming, and channel selection before starting the party."
                        .into()
                },
                tone: if ready_to_start {
                    "ready".into()
                } else {
                    "waiting".into()
                },
            },
        ];

        let ritual_steps = vec![
            "Paste the Discord bot token and invite the bot if needed.".into(),
            format!(
                "Prepare the `{}` desktop output from Bardic Chord.",
                audio_output_name_or_default(&settings.audio_output_name)
            ),
            "In your system sound settings, make sure Spotify is routed to that output.".into(),
            format!(
                "Start the party so `{}` joins the chosen Discord voice channel.",
                settings.bot_display_name
            ),
            if settings.tracked_user_id.trim().is_empty() {
                "Follow mode is optional. Leave it off if you want the bot to stay in one channel."
                    .into()
            } else {
                "Follow mode is on, so Bardic Chord will move with that user.".into()
            },
        ];

        let activity = vec![
            ActivityEntry {
                id: "phase".into(),
                title: "Guided desktop relay".into(),
                detail: "Bardic Chord now walks the user through Discord, desktop audio setup, and party launch in order."
                    .into(),
                timestamp: "Now".into(),
                tone: "accent".into(),
            },
            ActivityEntry {
                id: "audio".into(),
                title: if audio_supported {
                    "Local audio capture".into()
                } else {
                    "Platform backend pending".into()
                },
                detail: if audio_supported {
                    "Spotify should be routed through a dedicated local output instead of a Spotify Connect receiver."
                        .into()
                } else {
                    "The current release documents the shared UX, but this platform still needs a native capture backend."
                        .into()
                },
                timestamp: "Setup".into(),
                tone: if audio_supported {
                    "success".into()
                } else {
                    "muted".into()
                },
            },
            ActivityEntry {
                id: "discord".into(),
                title: if discord_ready {
                    "Discord is ready".into()
                } else {
                    "Discord token needed".into()
                },
                detail: if discord_ready {
                    "Next, choose the channel and let Bardic Chord join when the party starts.".into()
                } else {
                    "Add the bot token so Bardic Chord can find your server and voice channel.".into()
                },
                timestamp: "Setup".into(),
                tone: if discord_ready {
                    "success".into()
                } else {
                    "muted".into()
                },
            },
        ];

        Self {
            settings,
            health_tiles,
            ritual_steps,
            activity,
            last_saved_at,
        }
    }
}

fn read_snapshot(path: &PathBuf) -> Result<AppSnapshot, String> {
    let data = fs::read_to_string(path)
        .map_err(|error| format!("failed to read Bardic Chord settings: {error}"))?;
    serde_json::from_str::<AppSnapshot>(&data)
        .map_err(|error| format!("failed to parse Bardic Chord settings: {error}"))
}

fn format_now() -> String {
    Local::now().format("%b %d, %Y at %I:%M %p").to_string()
}

fn voice_channel_kind_name(kind: u8) -> &'static str {
    match kind {
        0 => "Text",
        2 => "Voice",
        4 => "Category",
        5 => "Announcement",
        13 => "Stage Voice",
        15 => "Forum",
        _ => "Other",
    }
}

fn audio_output_instructions(active: bool, output_name: &str) -> Vec<String> {
    if active {
        vec![
            format!("Open your system sound settings and route Spotify to `{output_name}`."),
            format!("Keep Spotify playing through `{output_name}` while the party is live."),
            "If the output disappears, prepare it again from Bardic Chord.".into(),
        ]
    } else {
        vec![
            "Press Prepare Audio Output from Bardic Chord.".into(),
            format!("Route Spotify to `{output_name}` in the system mixer."),
            "Then start the party to send that local audio into Discord.".into(),
        ]
    }
}

fn relay_instructions(active: bool) -> Vec<String> {
    if active {
        vec![
            "Keep Spotify routed to the prepared desktop output.".into(),
            "The bot should stay in the chosen Discord voice channel while the party is running."
                .into(),
            "If the output is recreated, start the party again.".into(),
        ]
    } else {
        vec![
            "Finish Discord setup first.".into(),
            "Prepare the desktop audio output and route Spotify to it.".into(),
            "Then start the party to send the music into Discord.".into(),
        ]
    }
}

fn build_audio_output_report(
    active: bool,
    output_name: Option<String>,
    platform: String,
    started_at: Option<String>,
    message: impl Into<String>,
) -> AudioOutputReport {
    let output_name = output_name.or_else(|| Some(DEFAULT_AUDIO_OUTPUT_NAME.into()));
    let instruction_steps = audio_output_instructions(
        active,
        output_name.as_deref().unwrap_or(DEFAULT_AUDIO_OUTPUT_NAME),
    );

    AudioOutputReport {
        active,
        output_name,
        platform,
        started_at,
        message: message.into(),
        instruction_steps,
    }
}

fn build_relay_report(
    active: bool,
    guild_name: Option<String>,
    guild_id: Option<String>,
    voice_channel_name: Option<String>,
    voice_channel_id: Option<String>,
    bot_username: Option<String>,
    started_at: Option<String>,
    message: impl Into<String>,
) -> DiscordRelayReport {
    DiscordRelayReport {
        active,
        guild_name,
        guild_id,
        voice_channel_name,
        voice_channel_id,
        bot_username,
        started_at,
        message: message.into(),
        instruction_steps: relay_instructions(active),
    }
}

fn clone_discord_relay_report(report: &Arc<StdMutex<DiscordRelayReport>>) -> DiscordRelayReport {
    report
        .lock()
        .expect("discord relay report mutex poisoned")
        .clone()
}

fn update_discord_relay_report<F>(report: &Arc<StdMutex<DiscordRelayReport>>, mutate: F)
where
    F: FnOnce(&mut DiscordRelayReport),
{
    let mut guard = report.lock().expect("discord relay report mutex poisoned");
    mutate(&mut guard);
    guard.instruction_steps = relay_instructions(guard.active);
}

fn add_relay_track_event(
    track: &songbird::tracks::TrackHandle,
    event: TrackEvent,
    report: Arc<StdMutex<DiscordRelayReport>>,
    log_path: PathBuf,
) {
    if let Err(error) = track.add_event(
        SongbirdEvent::Track(event),
        RelayTrackLogger {
            report,
            event,
            log_path,
        },
    ) {
        warn!(
            ?event,
            ?error,
            "failed to register a Discord relay track event handler"
        );
    }
}

fn parse_snowflake(input: &str, label: &str) -> Result<u64, String> {
    input
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("Invalid {label}: {error}"))
}

async fn stop_existing_audio_output(runtime: Option<AudioOutputRuntime>) -> Result<(), String> {
    let Some(mut runtime) = runtime else {
        return Ok(());
    };

    info!("stopping existing desktop audio output runtime");

    #[cfg(target_os = "linux")]
    {
        match &mut runtime.platform {
            AudioPlatformRuntime::Linux(linux) => {
                info!(sink_name = %linux.sink_name, "stopping Linux audio capture child");
                let _ = linux.capture_child.kill().await;
                let _ = linux.capture_child.wait().await;
                unload_pulse_module(&linux.module_id).await?;
            }
        }
    }

    match timeout(Duration::from_secs(5), runtime.task_handle).await {
        Ok(_) => Ok(()),
        Err(_) => Err("Timed out while shutting down the desktop audio capture task.".into()),
    }
}

async fn stop_existing_relay(
    relay: Option<DiscordRelayRuntime>,
) -> Result<Option<DiscordRelayRuntime>, String> {
    if let Some(runtime) = relay {
        info!(guild_id = %runtime.guild_id, "stopping existing Discord relay runtime");
        let _ = runtime.manager.remove(runtime.guild_id).await;
        runtime.shard_manager.shutdown_all().await;

        match timeout(Duration::from_secs(5), runtime.client_handle).await {
            Ok(_) => {}
            Err(_) => return Err("Timed out while shutting down the Discord relay.".into()),
        }
    }

    Ok(None)
}

#[cfg(target_os = "linux")]
async fn start_linux_audio_output(
    output_name: String,
    emitted_sink: EmittedSink,
    log_path: PathBuf,
) -> Result<(AudioPlatformRuntime, JoinHandle<()>), String> {
    let sink_name = sanitize_audio_output_name(&output_name);
    let module_id = load_pulse_null_sink(&sink_name, &output_name).await?;

    let mut child = Command::new("parec")
        .arg(format!("--device={sink_name}.monitor"))
        .arg("--format=s16le")
        .arg("--channels=2")
        .arg(format!("--rate={AUDIO_SAMPLE_RATE}"))
        .arg("--raw")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("failed to start Linux desktop audio capture with parec: {error}")
        })?;

    let mut stdout = child.stdout.take().ok_or_else(|| {
        "parec did not expose a stdout pipe for desktop audio capture.".to_string()
    })?;

    let task_handle = tokio::spawn(async move {
        let mut carry = Vec::<u8>::new();
        let mut buffer = [0_u8; 8192];
        let mut samples_seen = 0usize;
        let mut non_silent_samples = 0usize;
        let mut peak = 0.0_f32;
        let mut last_activity_log = Instant::now();

        loop {
            match stdout.read(&mut buffer).await {
                Ok(0) => {
                    info!("desktop audio capture stream ended");
                    break;
                }
                Ok(read) => {
                    carry.extend_from_slice(&buffer[..read]);
                    let complete_len = carry.len() / 4 * 4;
                    let mut frames = Vec::with_capacity(complete_len / 4);

                    for chunk in carry[..complete_len].chunks_exact(4) {
                        let left =
                            i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / i16::MAX as f32;
                        let right =
                            i16::from_le_bytes([chunk[2], chunk[3]]) as f32 / i16::MAX as f32;
                        let amplitude = left.abs().max(right.abs());
                        samples_seen += 1;
                        if amplitude > 0.0005 {
                            non_silent_samples += 1;
                        }
                        peak = peak.max(amplitude);
                        frames.push([left, right]);
                    }

                    if !emitted_sink.push_frames(&frames) {
                        info!("desktop audio capture stream lost its consumer");
                        return;
                    }

                    if last_activity_log.elapsed() >= Duration::from_secs(2) && samples_seen > 0 {
                        info!(
                            samples_seen,
                            non_silent_samples,
                            peak,
                            "desktop audio capture activity window"
                        );
                        samples_seen = 0;
                        non_silent_samples = 0;
                        peak = 0.0;
                        last_activity_log = Instant::now();
                    }

                    carry.drain(..complete_len);
                }
                Err(error) => {
                    error!(
                        log_path = %log_path.display(),
                        ?error,
                        "desktop audio capture stream hit a read error"
                    );
                    break;
                }
            }
        }
    });

    Ok((
        AudioPlatformRuntime::Linux(LinuxAudioOutputRuntime {
            module_id,
            sink_name,
            capture_child: child,
        }),
        task_handle,
    ))
}

#[cfg(target_os = "linux")]
async fn load_pulse_null_sink(sink_name: &str, output_name: &str) -> Result<String, String> {
    let description = pulse_property_escape(output_name);
    let output = Command::new("pactl")
        .arg("load-module")
        .arg("module-null-sink")
        .arg(format!("sink_name={sink_name}"))
        .arg("format=s16le")
        .arg("channels=2")
        .arg(format!("rate={AUDIO_SAMPLE_RATE}"))
        .arg(format!(
            "sink_properties=device.description='{description}' node.nick='{description}'"
        ))
        .output()
        .await
        .map_err(|error| format!("failed to run pactl load-module: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "Bardic Chord could not create the Linux desktop output. `pactl load-module` failed: {stderr}"
        ));
    }

    let module_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if module_id.is_empty() {
        return Err("Bardic Chord created the Linux desktop output, but `pactl` did not return a module id.".into());
    }

    info!(
        sink_name,
        module_id, "created Linux null sink for desktop capture"
    );
    Ok(module_id)
}

#[cfg(target_os = "linux")]
async fn unload_pulse_module(module_id: &str) -> Result<(), String> {
    let output = Command::new("pactl")
        .arg("unload-module")
        .arg(module_id)
        .output()
        .await
        .map_err(|error| format!("failed to run pactl unload-module: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "Bardic Chord could not unload the Linux desktop output module `{module_id}`: {stderr}"
        ));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
async fn try_route_spotify_to_linux_sink(
    sink_name: &str,
    output_name: &str,
) -> Result<SpotifyRouteAttempt, String> {
    let target_sinks: Vec<PulseSink> = pactl_json(&["--format=json", "list", "sinks"]).await?;
    let target_sink_index = target_sinks
        .iter()
        .find(|sink| sink.name == sink_name)
        .map(|sink| sink.index)
        .ok_or_else(|| {
            format!("Spotify routing could not find the `{output_name}` sink after it was created.")
        })?;

    let clients: Vec<PulseClient> = pactl_json(&["--format=json", "list", "clients"]).await?;
    let client_properties = clients
        .into_iter()
        .map(|client| (client.index.to_string(), client.properties))
        .collect::<HashMap<_, _>>();

    let inputs: Vec<PulseSinkInput> =
        pactl_json(&["--format=json", "list", "sink-inputs"]).await?;

    let mut attempt = SpotifyRouteAttempt::default();

    for input in inputs {
        if !sink_input_matches_spotify(&input, &client_properties) {
            continue;
        }

        attempt.detected_inputs += 1;

        if input.sink == Some(target_sink_index) {
            attempt.already_on_target += 1;
            continue;
        }

        let move_output = Command::new("pactl")
            .arg("move-sink-input")
            .arg(input.index.to_string())
            .arg(sink_name)
            .output()
            .await
            .map_err(|error| {
                format!(
                    "failed to move Spotify stream `{}` into `{output_name}`: {error}",
                    input.index
                )
            })?;

        if !move_output.status.success() {
            let stderr = String::from_utf8_lossy(&move_output.stderr)
                .trim()
                .to_string();
            return Err(format!(
                "pactl could not move Spotify stream `{}` into `{output_name}`: {stderr}",
                input.index
            ));
        }

        attempt.moved_inputs += 1;
        info!(
            sink_input_index = input.index,
            sink_name,
            output_name,
            "moved Spotify stream into the Bardic sink"
        );
    }

    Ok(attempt)
}

#[cfg(target_os = "linux")]
async fn pactl_json<T: DeserializeOwned>(args: &[&str]) -> Result<T, String> {
    let output = Command::new("pactl")
        .args(args)
        .output()
        .await
        .map_err(|error| format!("failed to run pactl {:?}: {error}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("pactl {:?} failed: {stderr}", args));
    }

    serde_json::from_slice::<T>(&output.stdout)
        .map_err(|error| format!("failed to parse pactl {:?} JSON output: {error}", args))
}

#[cfg(target_os = "linux")]
fn sink_input_matches_spotify(
    input: &PulseSinkInput,
    client_properties: &HashMap<String, HashMap<String, String>>,
) -> bool {
    let input_name = pulse_property_value(&input.properties, "application.name");
    let input_binary = pulse_property_value(&input.properties, "application.process.binary");
    let client_properties = input
        .client
        .as_ref()
        .and_then(|client_id| client_properties.get(client_id));
    let client_name = client_properties
        .and_then(|properties| pulse_property_value(properties, "application.name"));
    let client_binary = client_properties
        .and_then(|properties| pulse_property_value(properties, "application.process.binary"));

    [input_name, input_binary, client_name, client_binary]
        .into_iter()
        .flatten()
        .any(property_mentions_spotify)
}

#[cfg(target_os = "linux")]
fn pulse_property_value<'a>(
    properties: &'a HashMap<String, String>,
    key: &str,
) -> Option<&'a str> {
    properties
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn property_mentions_spotify(value: &str) -> bool {
    value.to_ascii_lowercase().contains("spotify")
}

async fn discord_get<T: DeserializeOwned>(
    client: &reqwest::Client,
    bot_token: &str,
    path: &str,
) -> Result<T, String> {
    let url = format!("{DISCORD_API_BASE}/{path}");
    let response = client
        .get(&url)
        .header(AUTHORIZATION, format!("Bot {bot_token}"))
        .header(USER_AGENT, DISCORD_USER_AGENT)
        .send()
        .await
        .map_err(|error| format!("Discord request failed: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Discord did not return a readable error body.".into());
        return Err(discord_error_message(status, &body));
    }

    response
        .json::<T>()
        .await
        .map_err(|error| format!("Discord response could not be parsed: {error}"))
}

fn normalize_discord_bot_token(bot_token: &str) -> &str {
    bot_token.trim().trim_start_matches("Bot ").trim()
}

fn discord_error_message(status: reqwest::StatusCode, body: &str) -> String {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        "Discord rejected that bot token with 401 Unauthorized. Paste the full current bot token from the Discord Developer Portal."
            .into()
    } else {
        format!("Discord returned {status}: {body}")
    }
}

fn discord_application_id_from_token(bot_token: &str) -> Option<String> {
    let encoded_id = normalize_discord_bot_token(bot_token)
        .split('.')
        .next()?
        .trim();
    let decoded = URL_SAFE_NO_PAD.decode(encoded_id).ok()?;
    let application_id = String::from_utf8(decoded).ok()?;
    if application_id
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        Some(application_id)
    } else {
        None
    }
}

fn audio_output_name_or_default(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        DEFAULT_AUDIO_OUTPUT_NAME.into()
    } else {
        trimmed
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_")
    }
}

fn pulse_property_escape(value: &str) -> String {
    value
        .trim()
        .chars()
        .flat_map(|character| match character {
            '\\' => ['\\', '\\'].into_iter().collect::<Vec<_>>(),
            '\'' => ['\\', '\''].into_iter().collect::<Vec<_>>(),
            '"' => Vec::new(),
            other => vec![other],
        })
        .collect()
}

fn sanitize_audio_output_name(value: &str) -> String {
    let mut output = value
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();

    while output.contains("__") {
        output = output.replace("__", "_");
    }
    output = output.trim_matches('_').to_string();

    if output.is_empty() {
        "bardic_chord".into()
    } else if output
        .chars()
        .next()
        .map(|character| character.is_ascii_digit())
        .unwrap_or(false)
    {
        format!("bardic_{output}")
    } else {
        output
    }
}

fn default_audio_idle_message() -> &'static str {
    if cfg!(target_os = "linux") {
        "Desktop audio output is idle. Prepare it from Bardic Chord, route Spotify into it, then start the party."
    } else {
        "This platform still needs a native loopback backend. The shared UX is ready, but the local capture runtime is not implemented here yet."
    }
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "this platform"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_discord_token(application_id: &str) -> String {
        let encoded = URL_SAFE_NO_PAD.encode(application_id.as_bytes());
        format!("{encoded}.payload.signature")
    }

    #[test]
    fn normalize_discord_bot_token_trims_prefix_and_whitespace() {
        assert_eq!(
            normalize_discord_bot_token("  Bot abc.def.ghi  "),
            "abc.def.ghi"
        );
    }

    #[test]
    fn discord_application_id_is_extracted_from_bot_token() {
        let token = fake_discord_token("1494272667946057819");
        assert_eq!(
            discord_application_id_from_token(&token).as_deref(),
            Some("1494272667946057819")
        );
    }

    #[test]
    fn discord_application_id_rejects_invalid_token_payloads() {
        assert_eq!(discord_application_id_from_token("not-a-token"), None);
        assert_eq!(
            discord_application_id_from_token(&fake_discord_token("not-numeric")),
            None
        );
    }

    #[test]
    fn sanitize_audio_output_name_normalizes_non_ascii_delimiters() {
        assert_eq!(
            sanitize_audio_output_name("Bardic Chord Output"),
            "bardic_chord_output"
        );
        assert_eq!(sanitize_audio_output_name("42"), "bardic_42");
        assert_eq!(sanitize_audio_output_name(""), "bardic_chord");
    }

    #[test]
    fn settings_normalized_restores_default_audio_output_name() {
        let settings = Settings {
            audio_output_name: "   ".into(),
            bot_display_name: "   ".into(),
            ..Default::default()
        }
        .normalized();

        assert_eq!(settings.audio_output_name, "Bardic_Chord");
        assert_eq!(settings.bot_display_name, "The Amber Minstrel");
    }

    #[test]
    fn settings_normalized_upgrades_legacy_audio_output_name() {
        let settings = Settings {
            audio_output_name: "Bardic Chord".into(),
            ..Default::default()
        }
        .normalized();

        assert_eq!(settings.audio_output_name, "Bardic_Chord");
    }

    #[test]
    fn pulse_property_escape_keeps_full_display_name() {
        assert_eq!(pulse_property_escape("Bardic Chord"), "Bardic Chord");
        assert_eq!(pulse_property_escape("Bardic's Chord"), "Bardic\\'s Chord");
    }

    #[test]
    fn app_snapshot_marks_desktop_audio_as_ready_on_linux() {
        let snapshot = AppSnapshot::from_settings(
            Settings {
                discord_bot_token: "token".into(),
                guild_id: "guild".into(),
                voice_channel_id: "channel".into(),
                audio_output_name: "Bardic_Chord".into(),
                bot_display_name: "The Amber Minstrel".into(),
                ..Default::default()
            },
            None,
        );

        assert_eq!(snapshot.health_tiles[1].label, "Desktop Audio");
        assert!(!snapshot.ritual_steps.is_empty());
    }

    #[test]
    fn spotify_matcher_detects_product_name_variants() {
        assert!(property_mentions_spotify("spotify"));
        assert!(property_mentions_spotify("Spotify"));
        assert!(property_mentions_spotify("/opt/spotify/spotify"));
        assert!(!property_mentions_spotify("firefox"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sink_input_matcher_can_use_client_properties() {
        let input = PulseSinkInput {
            index: 42,
            client: Some("99".into()),
            sink: Some(7),
            properties: HashMap::new(),
        };
        let client_properties = HashMap::from([(
            "99".into(),
            HashMap::from([("application.process.binary".into(), "spotify".into())]),
        )]);

        assert!(sink_input_matches_spotify(&input, &client_properties));
    }
}
