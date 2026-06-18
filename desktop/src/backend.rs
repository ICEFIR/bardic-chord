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
#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::VecDeque,
    env, fs, io, mem,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex as StdMutex},
};
#[cfg(target_os = "windows")]
use std::{process::Command as StdCommand, thread};
#[cfg(target_os = "linux")]
use tokio::io::AsyncReadExt;
#[cfg(target_os = "linux")]
use tokio::process::{Child, Command};
use tokio::{
    sync::{oneshot, Mutex},
    task::JoinHandle,
    time::{timeout, Duration, Instant},
};
use tracing::{debug, error, info, warn};
#[cfg(target_os = "windows")]
use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};
#[cfg(target_os = "windows")]
use windows::{
    core::{Interface, PWSTR},
    Win32::{
        Media::Audio::Endpoints::IAudioMeterInformation,
        Media::Audio::{
            eConsole, eRender, AudioSessionStateActive, AudioSessionStateExpired,
            AudioSessionStateInactive, IAudioSessionControl2, IAudioSessionManager2,
            IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
        },
        System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL},
    },
};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
pub const DISCORD_INVITE_PERMISSIONS: &str = "3146752";
const DISCORD_USER_AGENT: &str = "BardicChord/0.1.0 (+https://github.com/ICEFIR/bardic-chord)";
const LOCAL_STATE_DIR: &str = ".bardic-chord";
#[cfg(any(target_os = "windows", test))]
const WINDOWS_TASKLIST_ARGS: [&str; 3] = ["/fo", "csv", "/nh"];
const AUDIO_SAMPLE_RATE: u32 = 48_000;
const AUDIO_CHANNELS: usize = 2;
const AUDIO_CHANNELS_U32: u32 = 2;
const AUDIO_BUFFER_FRAMES: usize = AUDIO_SAMPLE_RATE as usize * 2;
const AUDIO_PREFILL_FRAMES: usize = 1_920;
#[cfg(target_os = "windows")]
const WINDOWS_SILENCE_WARN_WINDOWS: u32 = 5;
#[cfg(target_os = "windows")]
const WINDOWS_CAPTURE_RESCAN_SILENT_WINDOWS: u32 = 8;
#[cfg(target_os = "windows")]
const WINDOWS_MONITOR_INTERVAL: Duration = Duration::from_secs(3);
#[cfg(target_os = "windows")]
const WINDOWS_PROBE_DURATION: Duration = Duration::from_millis(1_200);
#[cfg(target_os = "windows")]
const WINDOWS_ACTIVE_AUDIO_THRESHOLD: f32 = 0.0005;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const DEFAULT_AUDIO_OUTPUT_NAME: &str = "Bardic_Chord";
pub const DEFAULT_CAPTURE_TARGET: &str = "spotify";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct Settings {
    pub discord_bot_token: String,
    pub tracked_user_id: String,
    pub guild_id: String,
    pub voice_channel_id: String,
    pub audio_output_name: String,
    pub capture_target: String,
    pub capture_target_pid: u32,
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
            capture_target: DEFAULT_CAPTURE_TARGET.into(),
            capture_target_pid: 0,
            bot_display_name: "The Amber Minstrel".into(),
        }
    }
}

impl Settings {
    fn normalized(mut self) -> Self {
        self.audio_output_name = audio_output_name_or_default(&self.audio_output_name);
        self.capture_target = capture_target_or_default(&self.capture_target);
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
    pub capture_session_options: Vec<WindowsAudioSessionOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WindowsAudioSessionOption {
    pub pid: u32,
    pub process_name: String,
    pub display_name: String,
    pub state: String,
    pub peak: f32,
    pub volume: f32,
    pub muted: bool,
    pub audible: bool,
    pub label: String,
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
struct TargetRouteAttempt {
    detected_inputs: usize,
    moved_inputs: usize,
    already_on_target: usize,
}

#[cfg(target_os = "linux")]
impl TargetRouteAttempt {
    fn summary(&self, output_name: &str, capture_target: &str) -> String {
        if self.moved_inputs > 0 {
            format!("`{capture_target}` was detected and moved into `{output_name}` automatically.")
        } else if self.already_on_target > 0 {
            format!("`{capture_target}` is already routed into `{output_name}`.")
        } else {
            format!(
                "`{capture_target}` was not detected yet. Start playback in that app, then Bardic Chord will try again when the party starts."
            )
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
                    .wait_timeout_while(state, Duration::from_millis(40), |state| {
                        state.frames.len() < prefill_target
                    })
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

#[cfg(target_os = "linux")]
struct LinuxAudioOutputRuntime {
    module_id: String,
    sink_name: String,
    capture_child: Child,
}

#[cfg(target_os = "windows")]
struct WindowsAudioOutputRuntime {
    stop_signal: Arc<AtomicBool>,
    capture_target: String,
    last_error: Arc<StdMutex<Option<String>>>,
    target_pid: Arc<StdMutex<Option<u32>>>,
    status: Arc<StdMutex<WindowsCaptureStatus>>,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowsProcessCandidate {
    image_name: String,
    pid: u32,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
struct WindowsCaptureStatus {
    message: String,
    target_pid: Option<u32>,
    candidate_count: usize,
    audible_count: usize,
    peak: f32,
    updated_at: String,
}

#[cfg(target_os = "windows")]
impl WindowsCaptureStatus {
    fn watching(capture_target: &str) -> Self {
        Self {
            message: format!(
                "Windows capture is watching for `{capture_target}`. Open the app and start playback; Bardic Chord will attach automatically."
            ),
            target_pid: None,
            candidate_count: 0,
            audible_count: 0,
            peak: 0.0,
            updated_at: format_now(),
        }
    }
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
struct WindowsProbeResult {
    candidate: WindowsProcessCandidate,
    samples_seen: usize,
    non_silent_samples: usize,
    zero_value_samples: usize,
    packets_read: usize,
    frames_read_total: usize,
    silent_flag_packets: usize,
    silent_flag_frames: usize,
    wait_timeouts: usize,
    peak: f32,
    error: Option<String>,
}

#[cfg(target_os = "windows")]
impl WindowsProbeResult {
    fn new(candidate: WindowsProcessCandidate) -> Self {
        Self {
            candidate,
            samples_seen: 0,
            non_silent_samples: 0,
            zero_value_samples: 0,
            packets_read: 0,
            frames_read_total: 0,
            silent_flag_packets: 0,
            silent_flag_frames: 0,
            wait_timeouts: 0,
            peak: 0.0,
            error: None,
        }
    }

    fn audible(&self) -> bool {
        self.error.is_none()
            && self.non_silent_samples > 0
            && self.peak > WINDOWS_ACTIVE_AUDIO_THRESHOLD
    }
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsCaptureRunResult {
    Stopped,
    Rescan,
}

enum AudioPlatformRuntime {
    #[cfg(target_os = "linux")]
    Linux(LinuxAudioOutputRuntime),
    #[cfg(target_os = "windows")]
    Windows(WindowsAudioOutputRuntime),
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
            #[cfg(target_os = "windows")]
            if runtime.report.active {
                runtime.report.capture_session_options = windows_audio_session_options_for_report();
                let AudioPlatformRuntime::Windows(windows) = &runtime.platform;
                if let Ok(status) = windows.status.lock() {
                    let active_pid = status
                        .target_pid
                        .map(|pid| format!("pid {pid}"))
                        .unwrap_or_else(|| "none".into());
                    runtime.report.message = format!(
                        "{}\nProbe updated: {}\nActive PID: {}\nCandidates: {}; audible: {}; peak: {:.3}",
                        status.message,
                        status.updated_at,
                        active_pid,
                        status.candidate_count,
                        status.audible_count,
                        status.peak
                    );
                }
            }

            if runtime.task_handle.is_finished() && runtime.report.active {
                let (capture_target, failure_message) = match &runtime.platform {
                    #[cfg(target_os = "linux")]
                    AudioPlatformRuntime::Linux(_) => (
                        DEFAULT_CAPTURE_TARGET.to_string(),
                        format!(
                            "The desktop audio output stopped unexpectedly. Check `{}` and prepare the output again.",
                            self.paths.log_path().display()
                        ),
                    ),
                    #[cfg(target_os = "windows")]
                    AudioPlatformRuntime::Windows(windows) => {
                        let detail = windows
                            .last_error
                            .lock()
                            .ok()
                            .and_then(|error| error.clone());
                        let target_pid = windows.target_pid.lock().ok().and_then(|pid| *pid);
                        let message = match detail {
                            Some(detail) => format!(
                                "Windows loopback capture for `{}` ({}) stopped: {} Check `{}` and prepare the output again.",
                                windows.capture_target,
                                target_pid
                                    .map(|pid| format!("pid {pid}"))
                                    .unwrap_or_else(|| "no active pid".into()),
                                detail,
                                self.paths.log_path().display()
                            ),
                            None => format!(
                                "Windows loopback capture for `{}` ({}) stopped unexpectedly. Check `{}` and prepare the output again.",
                                windows.capture_target,
                                target_pid
                                    .map(|pid| format!("pid {pid}"))
                                    .unwrap_or_else(|| "no active pid".into()),
                                self.paths.log_path().display()
                            ),
                        };
                        (windows.capture_target.clone(), message)
                    }
                };
                warn!(
                    log_path = %self.paths.log_path().display(),
                    "local audio capture task finished unexpectedly"
                );
                runtime.report = build_audio_output_report(
                    false,
                    runtime.report.output_name.clone(),
                    Some(capture_target),
                    platform_name().into(),
                    runtime.report.started_at.clone(),
                    failure_message,
                );
            }

            runtime.report.clone()
        } else {
            build_audio_output_report(
                false,
                Some(DEFAULT_AUDIO_OUTPUT_NAME.into()),
                Some(DEFAULT_CAPTURE_TARGET.into()),
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
            let capture_target = capture_target_or_default(&settings.capture_target);
            let emitted_sink = EmittedSink::new();
            let (platform, task_handle) = start_linux_audio_output(
                output_name.clone(),
                emitted_sink.clone(),
                self.paths.log_path(),
            )
            .await?;
            let route_attempt = match try_route_application_to_linux_sink(
                &sink_name,
                &output_name,
                &capture_target,
            )
            .await
            {
                Ok(attempt) => Some(attempt),
                Err(error) => {
                    warn!(
                        ?error,
                        sink_name,
                        capture_target,
                        "failed to auto-route the target app during audio setup"
                    );
                    None
                }
            };
            let message = route_attempt
                .as_ref()
                .map(|attempt| {
                    format!(
                        "Desktop audio output is ready on Linux. {}",
                        attempt.summary(&output_name, &capture_target)
                    )
                })
                .unwrap_or_else(|| {
                    format!(
                        "Desktop audio output is ready on Linux. If `{capture_target}` does not jump over automatically, move it to `{output_name}` from your sound settings and then start the party."
                    )
                });

            let report = build_audio_output_report(
                true,
                Some(output_name),
                Some(capture_target),
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

        #[cfg(target_os = "windows")]
        {
            let output_name = audio_output_name_or_default(&settings.audio_output_name);
            let capture_target = capture_target_or_default(&settings.capture_target);
            let preferred_pid =
                (settings.capture_target_pid != 0).then_some(settings.capture_target_pid);
            let emitted_sink = EmittedSink::new();
            info!(
                capture_target,
                preferred_pid,
                log_path = %self.paths.log_path().display(),
                "preparing Windows audio output"
            );
            let (platform, task_handle) = start_windows_audio_output(
                emitted_sink.clone(),
                self.paths.log_path(),
                capture_target.clone(),
                preferred_pid,
            )
            .await?;

            let report = build_audio_output_report(
                true,
                Some(output_name),
                Some(capture_target.clone()),
                platform_name().into(),
                Some(format_now()),
                format!(
                    "Windows capture is watching for `{capture_target}`. Start playback whenever you are ready; Bardic Chord will probe matching app processes and attach to the one producing audio."
                ),
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

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
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
            Some(DEFAULT_CAPTURE_TARGET.into()),
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

        #[cfg(target_os = "linux")]
        let output_name = audio_output_name_or_default(&settings.audio_output_name);

        #[cfg(target_os = "linux")]
        let capture_target = capture_target_or_default(&settings.capture_target);

        #[cfg(target_os = "linux")]
        let sink_name = sanitize_audio_output_name(&output_name);

        #[cfg(target_os = "linux")]
        {
            match try_route_application_to_linux_sink(&sink_name, &output_name, &capture_target)
                .await
            {
                Ok(attempt) => {
                    info!(
                        sink_name,
                        output_name,
                        capture_target,
                        detected_inputs = attempt.detected_inputs,
                        moved_inputs = attempt.moved_inputs,
                        already_on_target = attempt.already_on_target,
                        "checked target app routing before starting the Discord relay"
                    );
                }
                Err(error) => {
                    warn!(
                        sink_name,
                        output_name,
                        capture_target,
                        ?error,
                        "failed to check target app routing before starting the Discord relay"
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
            songbird::input::Input::Lazy(_) => return Err(
                "Bardic Chord created a lazy audio input unexpectedly and could not arm the relay."
                    .into(),
            ),
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
        let capture_target = capture_target_or_default(&settings.capture_target);
        let routing_ready =
            !settings.guild_id.trim().is_empty() && !settings.voice_channel_id.trim().is_empty();
        let identity_ready = !settings.bot_display_name.trim().is_empty();
        let audio_supported = cfg!(any(target_os = "linux", target_os = "windows"));
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
                    if cfg!(target_os = "windows") {
                        format!(
                            "`{capture_target}` can be captured directly through Windows process loopback on this machine."
                        )
                    } else {
                        format!(
                            "A local desktop output can be prepared for routing `{capture_target}` on this machine."
                        )
                    }
                } else if audio_supported {
                    if cfg!(target_os = "windows") {
                        "Prepare app loopback capture before starting the relay.".into()
                    } else {
                        "Name the local desktop output before preparing audio capture.".into()
                    }
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
                    format!(
                        "Setup is complete. Prepare the audio path for `{capture_target}` and start the party."
                    )
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
            if cfg!(target_os = "windows") {
                format!("Prepare loopback capture for `{capture_target}` from Bardic Chord.")
            } else {
                format!(
                    "Prepare the `{}` desktop output from Bardic Chord.",
                    audio_output_name_or_default(&settings.audio_output_name)
                )
            },
            if cfg!(target_os = "windows") {
                format!(
                    "Start playback in `{capture_target}` whenever you are ready; Bardic Chord will attach when it hears audio."
                )
            } else {
                format!(
                    "In your system sound settings, make sure `{capture_target}` is routed to that output."
                )
            },
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
                    format!(
                        "`{capture_target}` should be captured locally instead of relying on a Spotify Connect receiver."
                    )
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

fn audio_output_instructions(active: bool, output_name: &str, capture_target: &str) -> Vec<String> {
    if cfg!(target_os = "windows") {
        if active {
            vec![
                format!(
                    "Keep `{capture_target}` open when you want music, but playback can start later."
                ),
                format!(
                    "Bardic Chord keeps probing matching Windows processes and attaches to the one producing audio."
                ),
                format!("Then start or restart the party to send `{capture_target}` into Discord."),
            ]
        } else {
            vec![
                format!("Open `{capture_target}` before or after preparing audio."),
                format!(
                    "Press Prepare Audio Output so Bardic Chord starts its Windows loopback watcher."
                ),
                format!("Then start the party to send `{capture_target}` into Discord."),
            ]
        }
    } else if active {
        vec![
            format!(
                "Open your system sound settings and route `{capture_target}` to `{output_name}`."
            ),
            format!(
                "Keep `{capture_target}` playing through `{output_name}` while the party is live."
            ),
            "If the output disappears, prepare it again from Bardic Chord.".into(),
        ]
    } else {
        vec![
            "Press Prepare Audio Output from Bardic Chord.".into(),
            format!("Route `{capture_target}` to `{output_name}` in the system mixer."),
            "Then start the party to send that local audio into Discord.".into(),
        ]
    }
}

fn relay_instructions(active: bool) -> Vec<String> {
    if active {
        vec![
            if cfg!(target_os = "windows") {
                "Keep the selected capture app open while Bardic Chord is running.".into()
            } else {
                "Keep the selected capture app routed to the prepared desktop output.".into()
            },
            "The bot should stay in the chosen Discord voice channel while the party is running."
                .into(),
            "If the output is recreated, start the party again.".into(),
        ]
    } else {
        vec![
            "Finish Discord setup first.".into(),
            "Prepare the desktop audio path for the selected capture app.".into(),
            "Then start the party to send the music into Discord.".into(),
        ]
    }
}

fn build_audio_output_report(
    active: bool,
    output_name: Option<String>,
    capture_target: Option<String>,
    platform: String,
    started_at: Option<String>,
    message: impl Into<String>,
) -> AudioOutputReport {
    let output_name = output_name.or_else(|| Some(DEFAULT_AUDIO_OUTPUT_NAME.into()));
    let capture_target = capture_target.unwrap_or_else(|| DEFAULT_CAPTURE_TARGET.into());
    let instruction_steps = audio_output_instructions(
        active,
        output_name.as_deref().unwrap_or(DEFAULT_AUDIO_OUTPUT_NAME),
        &capture_target,
    );

    AudioOutputReport {
        active,
        output_name,
        platform,
        started_at,
        message: message.into(),
        instruction_steps,
        capture_session_options: windows_audio_session_options_for_report(),
    }
}

fn windows_audio_session_options_for_report() -> Vec<WindowsAudioSessionOption> {
    #[cfg(target_os = "windows")]
    {
        match enumerate_windows_render_audio_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                warn!(
                    ?error,
                    "failed to enumerate Windows render audio sessions for the UI"
                );
                Vec::new()
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
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
    let Some(runtime) = runtime else {
        return Ok(());
    };

    info!("stopping existing desktop audio output runtime");

    #[cfg(target_os = "linux")]
    {
        let mut runtime = runtime;
        match &mut runtime.platform {
            AudioPlatformRuntime::Linux(linux) => {
                info!(sink_name = %linux.sink_name, "stopping Linux audio capture child");
                let _ = linux.capture_child.kill().await;
                let _ = linux.capture_child.wait().await;
                unload_pulse_module(&linux.module_id).await?;
            }
        }

        return match timeout(Duration::from_secs(5), runtime.task_handle).await {
            Ok(_) => Ok(()),
            Err(_) => Err("Timed out while shutting down the desktop audio capture task.".into()),
        };
    }

    #[cfg(target_os = "windows")]
    {
        match &runtime.platform {
            AudioPlatformRuntime::Windows(windows) => {
                info!("stopping Windows target loopback capture");
                windows.stop_signal.store(true, Ordering::SeqCst);
            }
        }

        return match timeout(Duration::from_secs(5), runtime.task_handle).await {
            Ok(_) => Ok(()),
            Err(_) => Err("Timed out while shutting down the desktop audio capture task.".into()),
        };
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = runtime;
        Ok(())
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
                            non_silent_samples, peak, "desktop audio capture activity window"
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

#[cfg(target_os = "windows")]
async fn start_windows_audio_output(
    emitted_sink: EmittedSink,
    log_path: PathBuf,
    capture_target: String,
    preferred_pid: Option<u32>,
) -> Result<(AudioPlatformRuntime, JoinHandle<()>), String> {
    info!(
        capture_target,
        preferred_pid,
        log_path = %log_path.display(),
        "searching for Windows loopback capture target"
    );
    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_for_task = stop_signal.clone();
    let last_error = Arc::new(StdMutex::new(None::<String>));
    let last_error_for_task = last_error.clone();
    let capture_target_for_runtime = capture_target.clone();
    let target_pid = Arc::new(StdMutex::new(None::<u32>));
    let target_pid_for_task = target_pid.clone();
    let status = Arc::new(StdMutex::new(WindowsCaptureStatus::watching(
        &capture_target,
    )));
    let status_for_task = status.clone();

    let task_handle = tokio::task::spawn_blocking(move || {
        if let Err(error) = run_windows_capture_monitor(
            capture_target,
            emitted_sink,
            stop_for_task,
            log_path.clone(),
            status_for_task,
            target_pid_for_task,
            preferred_pid,
        ) {
            if let Ok(mut last_error) = last_error_for_task.lock() {
                *last_error = Some(error.clone());
            }
            error!(
                log_path = %log_path.display(),
                ?error,
                "windows target loopback capture failed"
            );
        }
    });

    Ok((
        AudioPlatformRuntime::Windows(WindowsAudioOutputRuntime {
            stop_signal,
            capture_target: capture_target_for_runtime,
            last_error,
            target_pid,
            status,
        }),
        task_handle,
    ))
}

#[cfg(target_os = "windows")]
fn run_windows_capture_monitor(
    capture_target: String,
    emitted_sink: EmittedSink,
    stop_signal: Arc<AtomicBool>,
    log_path: PathBuf,
    status: Arc<StdMutex<WindowsCaptureStatus>>,
    target_pid: Arc<StdMutex<Option<u32>>>,
    preferred_pid: Option<u32>,
) -> Result<(), String> {
    info!(
        capture_target,
        log_path = %log_path.display(),
        "Windows loopback capture monitor started"
    );

    while !stop_signal.load(Ordering::SeqCst) {
        let mut candidates = match find_windows_target_process_candidates_sync(&capture_target) {
            Ok(candidates) => candidates,
            Err(error) => {
                warn!(
                    capture_target,
                    ?error,
                    "Windows capture monitor could not enumerate target processes"
                );
                update_windows_capture_status(
                    &status,
                    WindowsCaptureStatus {
                        message: format!(
                            "Windows capture is still watching for `{capture_target}`, but process discovery failed. Check `{}` for details.",
                            log_path.display()
                        ),
                        target_pid: None,
                        candidate_count: 0,
                        audible_count: 0,
                        peak: 0.0,
                        updated_at: format_now(),
                    },
                );
                thread::sleep(WINDOWS_MONITOR_INTERVAL);
                continue;
            }
        };

        if let Some(preferred_pid) = preferred_pid {
            let has_preferred = candidates
                .iter()
                .any(|candidate| candidate.pid == preferred_pid);
            if !has_preferred {
                match find_windows_process_candidate_by_pid_sync(preferred_pid) {
                    Ok(Some(candidate)) => candidates.insert(0, candidate),
                    Ok(None) => {
                        warn!(
                            capture_target,
                            preferred_pid,
                            "selected Windows audio session process is no longer present"
                        );
                    }
                    Err(error) => {
                        warn!(
                            capture_target,
                            preferred_pid,
                            ?error,
                            "failed to resolve selected Windows audio session process"
                        );
                    }
                }
            }
        }

        if candidates.is_empty() {
            if let Ok(mut pid) = target_pid.lock() {
                *pid = None;
            }
            update_windows_capture_status(
                &status,
                WindowsCaptureStatus {
                    message: format!(
                        "Windows capture is watching for `{capture_target}`. Open the app and start playback; Bardic Chord will attach automatically."
                    ),
                    target_pid: None,
                    candidate_count: 0,
                    audible_count: 0,
                    peak: 0.0,
                    updated_at: format_now(),
                },
            );
            info!(
                capture_target,
                "Windows capture monitor found no matching target processes"
            );
            thread::sleep(WINDOWS_MONITOR_INTERVAL);
            continue;
        }

        let probe_candidates = preferred_pid
            .and_then(|preferred_pid| {
                candidates
                    .iter()
                    .find(|candidate| candidate.pid == preferred_pid)
                    .cloned()
                    .map(|candidate| vec![candidate])
            })
            .unwrap_or_else(|| candidates.clone());
        let probe_results = probe_windows_capture_candidates(
            &probe_candidates,
            &capture_target,
            stop_signal.clone(),
        );
        let audible = probe_results
            .iter()
            .filter(|result| result.audible())
            .collect::<Vec<_>>();
        let best = audible
            .iter()
            .max_by(|left, right| left.peak.total_cmp(&right.peak))
            .copied();

        log_windows_probe_results(&capture_target, &probe_results, best);

        let Some(best) = best else {
            let total_samples_seen = probe_results
                .iter()
                .map(|result| result.samples_seen)
                .sum::<usize>();
            let total_non_silent_samples = probe_results
                .iter()
                .map(|result| result.non_silent_samples)
                .sum::<usize>();
            let failed_probe_count = probe_results
                .iter()
                .filter(|result| result.error.is_some())
                .count();
            let message = if total_samples_seen > 0 && total_non_silent_samples == 0 {
                format!(
                    "`{capture_target}` is open, but every loopback probe is silent. Read {total_samples_seen} samples from {} selected/matching process{} with peak 0.000. Check Windows Volume Mixer for muted app audio or an unexpected output device; Bardic Chord will keep watching.",
                    probe_candidates.len(),
                    plural_suffix(probe_candidates.len())
                )
            } else if failed_probe_count > 0 {
                format!(
                    "`{capture_target}` is open, but {failed_probe_count} of {} loopback probe{} failed. Check `{}` for details; Bardic Chord will keep watching.",
                    probe_results.len(),
                    plural_suffix(failed_probe_count),
                    log_path.display()
                )
            } else {
                format!(
                    "`{capture_target}` is open, but Bardic Chord does not hear app audio yet. Watching {} selected/matching process{}.",
                    probe_candidates.len(),
                    plural_suffix(probe_candidates.len())
                )
            };
            if let Ok(mut pid) = target_pid.lock() {
                *pid = None;
            }
            update_windows_capture_status(
                &status,
                WindowsCaptureStatus {
                    message,
                    target_pid: None,
                    candidate_count: probe_candidates.len(),
                    audible_count: 0,
                    peak: 0.0,
                    updated_at: format_now(),
                },
            );
            thread::sleep(WINDOWS_MONITOR_INTERVAL);
            continue;
        };

        let selected = best.candidate.clone();
        if let Ok(mut pid) = target_pid.lock() {
            *pid = Some(selected.pid);
        }
        update_windows_capture_status(
            &status,
            WindowsCaptureStatus {
                message: format!(
                    "Capturing `{capture_target}` from {} pid {}. Audible matches: {} of {}; peak {:.3}.",
                    selected.image_name,
                    selected.pid,
                    audible.len(),
                    probe_candidates.len(),
                    best.peak
                ),
                target_pid: Some(selected.pid),
                candidate_count: probe_candidates.len(),
                audible_count: audible.len(),
                peak: best.peak,
                updated_at: format_now(),
            },
        );

        match run_windows_target_loopback(
            selected.pid,
            &capture_target,
            emitted_sink.clone(),
            stop_signal.clone(),
            log_path.clone(),
            status.clone(),
        ) {
            Ok(WindowsCaptureRunResult::Stopped) => break,
            Ok(WindowsCaptureRunResult::Rescan) => {
                if let Ok(mut pid) = target_pid.lock() {
                    *pid = None;
                }
                update_windows_capture_status(
                    &status,
                    WindowsCaptureStatus {
                        message: format!(
                            "`{capture_target}` went quiet. Bardic Chord is scanning matching processes again."
                        ),
                        target_pid: None,
                        candidate_count: candidates.len(),
                        audible_count: 0,
                        peak: 0.0,
                        updated_at: format_now(),
                    },
                );
            }
            Err(error) => {
                warn!(
                    selected_pid = selected.pid,
                    capture_target,
                    ?error,
                    "Windows selected loopback capture failed; returning to process monitor"
                );
                update_windows_capture_status(
                    &status,
                    WindowsCaptureStatus {
                        message: format!(
                            "Capture from `{capture_target}` pid {} stopped. Bardic Chord is scanning again.",
                            selected.pid
                        ),
                        target_pid: None,
                        candidate_count: candidates.len(),
                        audible_count: audible.len(),
                        peak: best.peak,
                        updated_at: format_now(),
                    },
                );
                thread::sleep(WINDOWS_MONITOR_INTERVAL);
            }
        }
    }

    if let Ok(mut pid) = target_pid.lock() {
        *pid = None;
    }
    update_windows_capture_status(
        &status,
        WindowsCaptureStatus {
            message: format!("Windows capture for `{capture_target}` is off."),
            target_pid: None,
            candidate_count: 0,
            audible_count: 0,
            peak: 0.0,
            updated_at: format_now(),
        },
    );
    info!(capture_target, "Windows loopback capture monitor stopped");
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_windows_target_loopback(
    target_pid: u32,
    capture_target: &str,
    emitted_sink: EmittedSink,
    stop_signal: Arc<AtomicBool>,
    log_path: PathBuf,
    status: Arc<StdMutex<WindowsCaptureStatus>>,
) -> Result<WindowsCaptureRunResult, String> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|error| format!("failed to initialize Windows audio COM apartment: {error}"))?;

    let desired_format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        AUDIO_SAMPLE_RATE as usize,
        2,
        None,
    );
    let mut audio_client = AudioClient::new_application_loopback_client(target_pid, true)
        .map_err(|error| {
            format!(
                "failed to create a Windows loopback client for `{capture_target}` pid {target_pid}: {error}"
            )
        })?;
    let stream_mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    };

    audio_client
        .initialize_client(&desired_format, &Direction::Capture, &stream_mode)
        .map_err(|error| format!("failed to initialize the Windows loopback client: {error}"))?;

    let event_handle = audio_client
        .set_get_eventhandle()
        .map_err(|error| format!("failed to create the Windows loopback event handle: {error}"))?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .map_err(|error| format!("failed to open the Windows loopback capture client: {error}"))?;

    audio_client.start_stream().map_err(|error| {
        format!("failed to start the Windows loopback stream for `{capture_target}`: {error}")
    })?;

    info!(
        target_pid,
        capture_target,
        sample_rate = AUDIO_SAMPLE_RATE,
        channels = AUDIO_CHANNELS,
        sample_bits = 32,
        buffer_duration_hns = 200_000,
        log_path = %log_path.display(),
        "Windows target loopback capture started"
    );

    let mut packet_buffer = vec![0_u8; 65_536];
    let mut samples_seen = 0usize;
    let mut non_silent_samples = 0usize;
    let mut zero_value_samples = 0usize;
    let mut packets_read = 0usize;
    let mut frames_read_total = 0usize;
    let mut silent_flag_packets = 0usize;
    let mut silent_flag_frames = 0usize;
    let mut wait_timeouts = 0usize;
    let mut empty_packet_polls = 0usize;
    let mut peak = 0.0_f32;
    let mut last_activity_log = Instant::now();
    let mut consecutive_silent_windows = 0u32;
    let mut first_audio_logged = false;

    loop {
        if stop_signal.load(Ordering::SeqCst) {
            break;
        }

        if let Err(error) = event_handle.wait_for_event(250) {
            let message = error.to_string();
            if message.to_ascii_lowercase().contains("timeout") {
                wait_timeouts += 1;
            } else {
                warn!(
                    ?error,
                    "Windows loopback event wait returned a non-timeout warning"
                );
            }
        }

        let mut read_packet_this_cycle = false;
        loop {
            let Some(next_frames) = capture_client.get_next_packet_size().map_err(|error| {
                format!("failed to query the Windows loopback packet size: {error}")
            })?
            else {
                break;
            };

            if next_frames == 0 {
                break;
            }

            let needed_bytes = next_frames as usize * mem::size_of::<f32>() * AUDIO_CHANNELS;
            if packet_buffer.len() < needed_bytes {
                packet_buffer.resize(needed_bytes, 0);
            }

            let (frames_read, buffer_info) = capture_client
                .read_from_device(&mut packet_buffer[..needed_bytes])
                .map_err(|error| format!("failed to read a Windows loopback packet: {error}"))?;

            if frames_read == 0 {
                break;
            }

            read_packet_this_cycle = true;
            packets_read += 1;
            frames_read_total += frames_read as usize;
            let usable_bytes = frames_read as usize * mem::size_of::<f32>() * AUDIO_CHANNELS;
            let mut frames = Vec::with_capacity(frames_read as usize);

            if buffer_info.flags.silent {
                samples_seen += frames_read as usize;
                silent_flag_packets += 1;
                silent_flag_frames += frames_read as usize;
                frames.resize(frames_read as usize, [0.0, 0.0]);
            } else {
                for chunk in packet_buffer[..usable_bytes].chunks_exact(8) {
                    let left = LittleEndian::read_f32(&chunk[0..4]);
                    let right = LittleEndian::read_f32(&chunk[4..8]);
                    let amplitude = left.abs().max(right.abs());
                    samples_seen += 1;
                    if amplitude == 0.0 {
                        zero_value_samples += 1;
                    }
                    if amplitude > WINDOWS_ACTIVE_AUDIO_THRESHOLD {
                        non_silent_samples += 1;
                        if !first_audio_logged {
                            info!(
                                target_pid,
                                capture_target,
                                amplitude,
                                "Windows target loopback detected first non-silent sample"
                            );
                            first_audio_logged = true;
                        }
                    }
                    peak = peak.max(amplitude);
                    frames.push([left, right]);
                }
            }

            if !emitted_sink.push_frames(&frames) {
                info!("windows target loopback capture lost its consumer");
                let _ = audio_client.stop_stream();
                return Ok(WindowsCaptureRunResult::Stopped);
            }
        }

        if !read_packet_this_cycle {
            empty_packet_polls += 1;
        }

        if last_activity_log.elapsed() >= Duration::from_secs(2)
            && (samples_seen > 0 || wait_timeouts > 0 || empty_packet_polls > 0)
        {
            let silent_window = samples_seen > 0 && non_silent_samples == 0;
            if silent_window {
                consecutive_silent_windows += 1;
            } else {
                consecutive_silent_windows = 0;
            }

            info!(
                samples_seen,
                non_silent_samples,
                zero_value_samples,
                packets_read,
                frames_read_total,
                silent_flag_packets,
                silent_flag_frames,
                wait_timeouts,
                empty_packet_polls,
                peak,
                consecutive_silent_windows,
                target_pid,
                capture_target,
                "windows target loopback activity window"
            );

            if consecutive_silent_windows == WINDOWS_SILENCE_WARN_WINDOWS
                || (consecutive_silent_windows > WINDOWS_SILENCE_WARN_WINDOWS
                    && consecutive_silent_windows % (WINDOWS_SILENCE_WARN_WINDOWS * 3) == 0)
            {
                warn!(
                    target_pid,
                    capture_target,
                    consecutive_silent_windows,
                    log_path = %log_path.display(),
                    "Windows target loopback has only produced silence; confirm the selected app process is the one playing audio and that the app is not muted in Windows volume mixer"
                );
            }

            update_windows_capture_status(
                &status,
                WindowsCaptureStatus {
                    message: if silent_window {
                        format!(
                            "`{capture_target}` pid {target_pid} is attached but currently silent. Bardic Chord will rescan if playback moved to another process."
                        )
                    } else {
                        format!(
                            "Capturing `{capture_target}` from pid {target_pid}. Peak {:.3}.",
                            peak
                        )
                    },
                    target_pid: Some(target_pid),
                    candidate_count: 1,
                    audible_count: usize::from(!silent_window),
                    peak,
                    updated_at: format_now(),
                },
            );

            if consecutive_silent_windows >= WINDOWS_CAPTURE_RESCAN_SILENT_WINDOWS {
                info!(
                    target_pid,
                    capture_target,
                    consecutive_silent_windows,
                    "Windows target loopback stayed silent long enough to trigger process rescan"
                );
                let _ = audio_client.stop_stream();
                return Ok(WindowsCaptureRunResult::Rescan);
            }

            samples_seen = 0;
            non_silent_samples = 0;
            zero_value_samples = 0;
            packets_read = 0;
            frames_read_total = 0;
            silent_flag_packets = 0;
            silent_flag_frames = 0;
            wait_timeouts = 0;
            empty_packet_polls = 0;
            peak = 0.0;
            last_activity_log = Instant::now();
        }
    }

    let _ = audio_client.stop_stream();
    info!(capture_target, "Windows target loopback capture stopped");
    Ok(WindowsCaptureRunResult::Stopped)
}

#[cfg(target_os = "windows")]
fn update_windows_capture_status(
    status: &Arc<StdMutex<WindowsCaptureStatus>>,
    next: WindowsCaptureStatus,
) {
    if let Ok(mut status) = status.lock() {
        *status = next;
    }
}

#[cfg(target_os = "windows")]
fn find_windows_target_process_candidates_sync(
    capture_target: &str,
) -> Result<Vec<WindowsProcessCandidate>, String> {
    let output = StdCommand::new("tasklist")
        .args(WINDOWS_TASKLIST_ARGS)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| {
            format!("failed to run tasklist while looking for `{capture_target}`: {error}")
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        error!(
            capture_target,
            status = %output.status,
            stderr,
            "tasklist failed while looking for the Windows capture target"
        );
        return Err(format!(
            "tasklist failed while looking for `{capture_target}`: {stderr}"
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_windows_tasklist_for_target_candidates(
        stdout.as_ref(),
        capture_target,
    ))
}

#[cfg(target_os = "windows")]
fn find_windows_process_candidate_by_pid_sync(
    target_pid: u32,
) -> Result<Option<WindowsProcessCandidate>, String> {
    let output = StdCommand::new("tasklist")
        .args(WINDOWS_TASKLIST_ARGS)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| {
            format!("failed to run tasklist while looking for pid {target_pid}: {error}")
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "tasklist failed while looking for pid {target_pid}: {stderr}"
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_windows_tasklist_for_pid(stdout.as_ref(), target_pid))
}

#[cfg(target_os = "windows")]
fn probe_windows_capture_candidates(
    candidates: &[WindowsProcessCandidate],
    capture_target: &str,
    stop_signal: Arc<AtomicBool>,
) -> Vec<WindowsProbeResult> {
    let handles = candidates
        .iter()
        .cloned()
        .map(|candidate| {
            let capture_target = capture_target.to_string();
            let stop_signal = stop_signal.clone();
            thread::spawn(move || {
                probe_windows_capture_candidate(candidate, &capture_target, stop_signal)
            })
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .filter_map(|handle| handle.join().ok())
        .collect()
}

#[cfg(target_os = "windows")]
fn probe_windows_capture_candidate(
    candidate: WindowsProcessCandidate,
    capture_target: &str,
    stop_signal: Arc<AtomicBool>,
) -> WindowsProbeResult {
    let mut result = WindowsProbeResult::new(candidate.clone());
    if stop_signal.load(Ordering::SeqCst) {
        return result;
    }

    if let Err(error) = wasapi::initialize_mta().ok() {
        result.error = Some(format!(
            "failed to initialize Windows audio COM apartment: {error}"
        ));
        return result;
    }

    let desired_format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        AUDIO_SAMPLE_RATE as usize,
        2,
        None,
    );
    let mut audio_client = match AudioClient::new_application_loopback_client(candidate.pid, true) {
        Ok(client) => client,
        Err(error) => {
            result.error = Some(format!(
                "failed to create loopback probe for `{capture_target}` pid {}: {error}",
                candidate.pid
            ));
            return result;
        }
    };
    let stream_mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 100_000,
    };

    if let Err(error) =
        audio_client.initialize_client(&desired_format, &Direction::Capture, &stream_mode)
    {
        result.error = Some(format!(
            "failed to initialize loopback probe for `{capture_target}` pid {}: {error}",
            candidate.pid
        ));
        return result;
    }

    let event_handle = match audio_client.set_get_eventhandle() {
        Ok(handle) => handle,
        Err(error) => {
            result.error = Some(format!(
                "failed to create loopback probe event for `{capture_target}` pid {}: {error}",
                candidate.pid
            ));
            return result;
        }
    };
    let capture_client = match audio_client.get_audiocaptureclient() {
        Ok(client) => client,
        Err(error) => {
            result.error = Some(format!(
                "failed to open loopback probe client for `{capture_target}` pid {}: {error}",
                candidate.pid
            ));
            return result;
        }
    };

    if let Err(error) = audio_client.start_stream() {
        result.error = Some(format!(
            "failed to start loopback probe for `{capture_target}` pid {}: {error}",
            candidate.pid
        ));
        return result;
    }

    let started = Instant::now();
    let mut packet_buffer = vec![0_u8; 65_536];
    while started.elapsed() < WINDOWS_PROBE_DURATION && !stop_signal.load(Ordering::SeqCst) {
        if let Err(error) = event_handle.wait_for_event(150) {
            let message = error.to_string();
            if message.to_ascii_lowercase().contains("timeout") {
                result.wait_timeouts += 1;
            } else {
                result.error = Some(format!(
                    "loopback probe wait failed for `{capture_target}` pid {}: {error}",
                    candidate.pid
                ));
                break;
            }
        }

        loop {
            let next_frames = match capture_client.get_next_packet_size() {
                Ok(Some(frames)) => frames,
                Ok(None) => break,
                Err(error) => {
                    result.error = Some(format!(
                        "loopback probe packet query failed for `{capture_target}` pid {}: {error}",
                        candidate.pid
                    ));
                    break;
                }
            };
            if next_frames == 0 {
                break;
            }

            let needed_bytes = next_frames as usize * mem::size_of::<f32>() * AUDIO_CHANNELS;
            if packet_buffer.len() < needed_bytes {
                packet_buffer.resize(needed_bytes, 0);
            }

            let (frames_read, buffer_info) =
                match capture_client.read_from_device(&mut packet_buffer[..needed_bytes]) {
                    Ok(read) => read,
                    Err(error) => {
                        result.error = Some(format!(
                            "loopback probe read failed for `{capture_target}` pid {}: {error}",
                            candidate.pid
                        ));
                        break;
                    }
                };

            if frames_read == 0 {
                break;
            }

            result.packets_read += 1;
            result.frames_read_total += frames_read as usize;

            if buffer_info.flags.silent {
                result.samples_seen += frames_read as usize;
                result.silent_flag_packets += 1;
                result.silent_flag_frames += frames_read as usize;
            } else {
                let usable_bytes = frames_read as usize * mem::size_of::<f32>() * AUDIO_CHANNELS;
                for chunk in packet_buffer[..usable_bytes].chunks_exact(8) {
                    let left = LittleEndian::read_f32(&chunk[0..4]);
                    let right = LittleEndian::read_f32(&chunk[4..8]);
                    let amplitude = left.abs().max(right.abs());
                    result.samples_seen += 1;
                    if amplitude == 0.0 {
                        result.zero_value_samples += 1;
                    }
                    if amplitude > WINDOWS_ACTIVE_AUDIO_THRESHOLD {
                        result.non_silent_samples += 1;
                    }
                    result.peak = result.peak.max(amplitude);
                }
            }

            if result.error.is_some() {
                break;
            }
        }

        if result.error.is_some() {
            break;
        }
    }

    let _ = audio_client.stop_stream();
    result
}

#[cfg(target_os = "windows")]
fn log_windows_probe_results(
    capture_target: &str,
    results: &[WindowsProbeResult],
    best: Option<&WindowsProbeResult>,
) {
    let audible_count = results.iter().filter(|result| result.audible()).count();
    let best_pid = best.map(|result| result.candidate.pid);
    let best_peak = best.map(|result| result.peak).unwrap_or(0.0);

    info!(
        capture_target,
        candidate_count = results.len(),
        audible_count,
        best_pid,
        best_peak,
        "Windows loopback probe summary"
    );

    for result in results {
        info!(
            capture_target,
            target_pid = result.candidate.pid,
            image_name = %result.candidate.image_name,
            audible = result.audible(),
            samples_seen = result.samples_seen,
            non_silent_samples = result.non_silent_samples,
            zero_value_samples = result.zero_value_samples,
            packets_read = result.packets_read,
            frames_read_total = result.frames_read_total,
            silent_flag_packets = result.silent_flag_packets,
            silent_flag_frames = result.silent_flag_frames,
            wait_timeouts = result.wait_timeouts,
            peak = result.peak,
            error = ?result.error,
            "Windows loopback probe candidate result"
        );
    }
}

#[cfg(target_os = "windows")]
fn plural_suffix(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "es"
    }
}

#[cfg(test)]
fn parse_windows_tasklist_for_target_pid(stdout: &str, capture_target: &str) -> Option<u32> {
    parse_windows_tasklist_for_target_candidates(stdout, capture_target)
        .into_iter()
        .next()
        .map(|candidate| candidate.pid)
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_tasklist_for_target_candidates(
    stdout: &str,
    capture_target: &str,
) -> Vec<WindowsProcessCandidate> {
    let normalized_target = capture_target.trim().to_ascii_lowercase();
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let columns = line.split("\",\"").collect::<Vec<_>>();
            if columns.len() < 2 {
                return None;
            }

            let image_name = columns[0].trim_matches('"');
            let pid = columns[1].trim_matches('"').parse::<u32>().ok()?;
            process_name_matches_target(image_name, &normalized_target).then(|| {
                WindowsProcessCandidate {
                    image_name: image_name.to_string(),
                    pid,
                }
            })
        })
        .collect()
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_tasklist_for_pid(
    stdout: &str,
    target_pid: u32,
) -> Option<WindowsProcessCandidate> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(parse_windows_tasklist_process_line)
        .find(|candidate| candidate.pid == target_pid)
}

#[cfg(any(target_os = "windows", test))]
fn parse_windows_tasklist_process_line(line: &str) -> Option<WindowsProcessCandidate> {
    let columns = line.split("\",\"").collect::<Vec<_>>();
    if columns.len() < 2 {
        return None;
    }
    let image_name = columns[0].trim_matches('"').to_string();
    let pid = columns[1].trim_matches('"').parse::<u32>().ok()?;
    Some(WindowsProcessCandidate { image_name, pid })
}

#[cfg(target_os = "windows")]
fn enumerate_windows_render_audio_sessions() -> Result<Vec<WindowsAudioSessionOption>, String> {
    wasapi::initialize_mta().ok().map_err(|error| {
        format!("failed to initialize COM for audio session discovery: {error}")
    })?;

    let process_map = windows_process_map_sync().unwrap_or_default();
    let sessions = unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|error| format!("failed to create Windows device enumerator: {error}"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|error| format!("failed to open default render endpoint: {error}"))?;
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| format!("failed to activate audio session manager: {error}"))?;
        let session_enumerator = manager
            .GetSessionEnumerator()
            .map_err(|error| format!("failed to enumerate audio sessions: {error}"))?;
        let count = session_enumerator
            .GetCount()
            .map_err(|error| format!("failed to count audio sessions: {error}"))?;
        let mut sessions = Vec::new();

        for index in 0..count {
            let control = match session_enumerator.GetSession(index) {
                Ok(control) => control,
                Err(error) => {
                    warn!(index, ?error, "failed to read Windows audio session");
                    continue;
                }
            };
            let state = match control.GetState() {
                Ok(state) if state == AudioSessionStateActive => "active",
                Ok(state) if state == AudioSessionStateInactive => "inactive",
                Ok(state) if state == AudioSessionStateExpired => "expired",
                Ok(_) => "unknown",
                Err(error) => {
                    warn!(index, ?error, "failed to read Windows audio session state");
                    "unknown"
                }
            }
            .to_string();
            let display_name = pwstr_to_string(control.GetDisplayName().ok());
            let control2 = control.cast::<IAudioSessionControl2>().ok();
            let pid = control2
                .as_ref()
                .and_then(|control| control.GetProcessId().ok())
                .unwrap_or(0);
            let process_name = process_map.get(&pid).cloned().unwrap_or_default();
            let meter = control.cast::<IAudioMeterInformation>().ok();
            let peak = meter
                .as_ref()
                .and_then(|meter| meter.GetPeakValue().ok())
                .unwrap_or(0.0);
            let volume = control
                .cast::<ISimpleAudioVolume>()
                .ok()
                .and_then(|volume| volume.GetMasterVolume().ok())
                .unwrap_or(0.0);
            let muted = control
                .cast::<ISimpleAudioVolume>()
                .ok()
                .and_then(|volume| volume.GetMute().ok())
                .map(|muted| muted.as_bool())
                .unwrap_or(false);
            let audible = peak >= WINDOWS_ACTIVE_AUDIO_THRESHOLD && !muted && state == "active";
            let label_name = non_empty(&process_name)
                .or_else(|| non_empty(&display_name))
                .unwrap_or("Unknown session");
            let label = format!(
                "{} pid {} | {} | peak {:.3} | vol {:.0}%{}",
                label_name,
                pid,
                state,
                peak,
                volume * 100.0,
                if muted { " | muted" } else { "" }
            );

            sessions.push(WindowsAudioSessionOption {
                pid,
                process_name,
                display_name,
                state,
                peak,
                volume,
                muted,
                audible,
                label,
            });
        }

        sessions
    };

    let mut sessions = sessions
        .into_iter()
        .filter(|session| session.pid != 0)
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .audible
            .cmp(&left.audible)
            .then_with(|| right.peak.total_cmp(&left.peak))
            .then_with(|| left.process_name.cmp(&right.process_name))
    });

    info!(
        session_count = sessions.len(),
        audible_count = sessions.iter().filter(|session| session.audible).count(),
        "Windows render audio session discovery"
    );
    for session in &sessions {
        info!(
            pid = session.pid,
            process_name = %session.process_name,
            display_name = %session.display_name,
            state = %session.state,
            peak = session.peak,
            volume = session.volume,
            muted = session.muted,
            audible = session.audible,
            "Windows render audio session"
        );
    }

    Ok(sessions)
}

#[cfg(target_os = "windows")]
fn windows_process_map_sync() -> Result<std::collections::HashMap<u32, String>, String> {
    let output = StdCommand::new("tasklist")
        .args(WINDOWS_TASKLIST_ARGS)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("failed to run tasklist while mapping audio sessions: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "tasklist failed while mapping audio sessions: {stderr}"
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(parse_windows_tasklist_process_line)
        .map(|candidate| (candidate.pid, candidate.image_name))
        .collect())
}

#[cfg(target_os = "windows")]
fn pwstr_to_string(value: Option<PWSTR>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let text = unsafe { value.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    text
}

#[cfg(target_os = "windows")]
fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
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
async fn try_route_application_to_linux_sink(
    sink_name: &str,
    output_name: &str,
    capture_target: &str,
) -> Result<TargetRouteAttempt, String> {
    let target_sinks: Vec<PulseSink> = pactl_json(&["--format=json", "list", "sinks"]).await?;
    let target_sink_index = target_sinks
        .iter()
        .find(|sink| sink.name == sink_name)
        .map(|sink| sink.index)
        .ok_or_else(|| {
            format!(
                "Target app routing could not find the `{output_name}` sink after it was created."
            )
        })?;

    let clients: Vec<PulseClient> = pactl_json(&["--format=json", "list", "clients"]).await?;
    let client_properties = clients
        .into_iter()
        .map(|client| (client.index.to_string(), client.properties))
        .collect::<HashMap<_, _>>();

    let inputs: Vec<PulseSinkInput> = pactl_json(&["--format=json", "list", "sink-inputs"]).await?;

    let mut attempt = TargetRouteAttempt::default();

    for input in inputs {
        if !sink_input_matches_target(&input, &client_properties, capture_target) {
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
                    "failed to move `{capture_target}` stream `{}` into `{output_name}`: {error}",
                    input.index
                )
            })?;

        if !move_output.status.success() {
            let stderr = String::from_utf8_lossy(&move_output.stderr)
                .trim()
                .to_string();
            return Err(format!(
                "pactl could not move `{capture_target}` stream `{}` into `{output_name}`: {stderr}",
                input.index
            ));
        }

        attempt.moved_inputs += 1;
        info!(
            sink_input_index = input.index,
            sink_name, output_name, capture_target, "moved target app stream into the Bardic sink"
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
fn sink_input_matches_target(
    input: &PulseSinkInput,
    client_properties: &HashMap<String, HashMap<String, String>>,
    capture_target: &str,
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
        .any(|value| property_matches_capture_target(value, capture_target))
}

#[cfg(target_os = "linux")]
fn pulse_property_value<'a>(properties: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    properties
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn property_matches_capture_target(value: &str, capture_target: &str) -> bool {
    let normalized_target = capture_target.trim().to_ascii_lowercase();
    if normalized_target.is_empty() {
        return false;
    }

    let normalized_value = value.trim().to_ascii_lowercase();
    normalized_value.contains(&normalized_target)
        || normalized_value.contains(&format!("{normalized_target}.exe"))
        || normalized_value
            .trim_end_matches(".exe")
            .contains(normalized_target.trim_end_matches(".exe"))
}

#[cfg(any(target_os = "windows", test))]
fn process_name_matches_target(process_name: &str, capture_target: &str) -> bool {
    property_matches_capture_target(process_name, capture_target)
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
        trimmed.split_whitespace().collect::<Vec<_>>().join("_")
    }
}

fn capture_target_or_default(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        DEFAULT_CAPTURE_TARGET.into()
    } else {
        trimmed.to_string()
    }
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
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
        "Desktop audio output is idle. Prepare it from Bardic Chord, route your selected app into it, then start the party."
    } else if cfg!(target_os = "windows") {
        "Desktop audio is idle. Prepare it from Bardic Chord and it will capture the selected app directly through Windows loopback."
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
            capture_target: "   ".into(),
            bot_display_name: "   ".into(),
            ..Default::default()
        }
        .normalized();

        assert_eq!(settings.audio_output_name, "Bardic_Chord");
        assert_eq!(settings.capture_target, "spotify");
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
    fn capture_target_matcher_detects_product_name_variants() {
        assert!(property_matches_capture_target("spotify", "spotify"));
        assert!(property_matches_capture_target("Spotify", "spotify"));
        assert!(property_matches_capture_target(
            "/opt/spotify/spotify",
            "spotify"
        ));
        assert!(property_matches_capture_target("firefox.exe", "firefox"));
        assert!(!property_matches_capture_target("firefox", "spotify"));
    }

    #[test]
    fn windows_tasklist_args_are_well_formed() {
        assert_eq!(WINDOWS_TASKLIST_ARGS, ["/fo", "csv", "/nh"]);
    }

    #[test]
    fn windows_tasklist_parser_detects_spotify_process() {
        let stdout = "\"Spotify.exe\",\"1337\",\"Console\",\"1\",\"123,456 K\"\n\
\"Discord.exe\",\"7331\",\"Console\",\"1\",\"654,321 K\"";

        assert_eq!(
            parse_windows_tasklist_for_target_pid(stdout, "spotify"),
            Some(1337)
        );
    }

    #[test]
    fn windows_tasklist_parser_keeps_all_spotify_candidates() {
        let stdout = "\"Spotify.exe\",\"1337\",\"Console\",\"1\",\"123,456 K\"\n\
\"Spotify.exe\",\"1338\",\"Console\",\"1\",\"116,000 K\"\n\
\"SpotifyWidgetProvider.exe\",\"1339\",\"Console\",\"1\",\"28,000 K\"\n\
\"crashpad_handler.exe\",\"1340\",\"Console\",\"1\",\"1,000 K\"\n\
\"RuntimeBroker.exe\",\"1341\",\"Console\",\"1\",\"1,000 K\"";

        let candidates = parse_windows_tasklist_for_target_candidates(stdout, "spotify");

        assert_eq!(
            candidates,
            vec![
                WindowsProcessCandidate {
                    image_name: "Spotify.exe".into(),
                    pid: 1337,
                },
                WindowsProcessCandidate {
                    image_name: "Spotify.exe".into(),
                    pid: 1338,
                },
                WindowsProcessCandidate {
                    image_name: "SpotifyWidgetProvider.exe".into(),
                    pid: 1339,
                },
            ]
        );
    }

    #[test]
    fn windows_tasklist_parser_finds_candidate_by_pid() {
        let stdout = "\"SpotifyLauncher.exe\",\"9908\",\"Console\",\"1\",\"682,420 K\"\n\
\"Spotify.exe\",\"26740\",\"Console\",\"1\",\"179,000 K\"";

        assert_eq!(
            parse_windows_tasklist_for_pid(stdout, 26740),
            Some(WindowsProcessCandidate {
                image_name: "Spotify.exe".into(),
                pid: 26740,
            })
        );
        assert_eq!(parse_windows_tasklist_for_pid(stdout, 123), None);
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

        assert!(sink_input_matches_target(
            &input,
            &client_properties,
            "spotify"
        ));
    }

    #[test]
    fn capture_target_normalization_restores_default() {
        assert_eq!(capture_target_or_default(""), "spotify");
        assert_eq!(capture_target_or_default("   firefox  "), "firefox");
    }
}
