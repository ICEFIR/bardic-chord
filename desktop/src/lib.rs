mod backend;

use backend::{
    discord_invite_url_from_token, ActivityEntry, AppSnapshot, AudioOutputReport, Backend,
    DiscordGuildOption, DiscordRelayReport, DiscordValidationReport, DiscordVoiceChannelOption,
    HealthTile, Settings, DEFAULT_AUDIO_OUTPUT_NAME, DEFAULT_CAPTURE_TARGET,
    DISCORD_INVITE_PERMISSIONS,
};
#[cfg(target_os = "linux")]
use directories::BaseDirs;
use slint::{Model, ModelRc, SharedString, VecModel};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::time::{sleep, Duration};
use tracing::info;
#[cfg(target_os = "linux")]
use tracing::warn;

slint::include_modules!();

const DISCORD_INVITE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const DISCORD_INVITE_POLL_ATTEMPTS: u32 = 18;
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const LOG_MAX_BYTES: u64 = 1_048_576;
#[cfg(target_os = "linux")]
const LINUX_APP_ID: &str = "bardic-chord";
const LOCAL_STATE_DIR: &str = ".bardic-chord";

#[derive(Clone)]
struct SizedLogWriter {
    state: Arc<Mutex<SizedLogWriterState>>,
}

struct SizedLogWriterState {
    file: File,
}

struct ViewState {
    snapshot: AppSnapshot,
    audio_output: AudioOutputReport,
    relay: DiscordRelayReport,
    party_can_stop: bool,
}

pub fn run() {
    install_rustls_crypto_provider();
    configure_desktop_identity();

    if let Err(error) = init_logging() {
        eprintln!("failed to initialize Bardic Chord logging: {error}");
    } else {
        info!("Bardic Chord startup");
    }

    let runtime = build_runtime();
    let backend = Arc::new(Backend::new().expect("failed to initialize Bardic Chord backend"));
    let ui = AppWindow::new().expect("failed to build Bardic Chord window");
    let discord_validation_generation = Arc::new(AtomicU64::new(0));

    ui.set_discord_permissions(DISCORD_INVITE_PERMISSIONS.into());
    ui.set_discord_guild_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_guild_id_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_voice_channel_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_voice_channel_id_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_guild_index(0);
    ui.set_discord_voice_channel_index(0);
    ui.set_follow_user_enabled(false);
    ui.set_discord_connected(false);
    ui.set_discord_route_ready(false);
    ui.set_party_can_stop(false);
    ui.set_audio_prepared(false);

    if let Err(error) = hydrate_ui(&ui, &backend, &runtime) {
        ui.set_save_message(error.into());
    }
    bind_callbacks(
        &ui,
        backend.clone(),
        runtime.handle().clone(),
        discord_validation_generation.clone(),
    );
    start_status_poll(ui.as_weak(), backend.clone(), runtime.handle().clone());

    if !ui.get_discord_bot_token().trim().is_empty() {
        schedule_discord_validation(
            &ui,
            backend.clone(),
            runtime.handle().clone(),
            discord_validation_generation,
            Duration::from_millis(50),
            false,
        );
    }

    ui.run().expect("failed while running Bardic Chord");

    if let Err(error) = runtime.block_on(backend.shutdown()) {
        eprintln!("failed to cleanly stop Bardic Chord runtimes: {error}");
    }
}

fn build_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
}

fn configure_desktop_identity() {
    #[cfg(target_os = "linux")]
    {
        if let Err(error) = slint::BackendSelector::new()
            .backend_name("winit".into())
            .select()
        {
            warn!(
                ?error,
                "failed to explicitly select the Slint winit backend"
            );
        }

        if let Err(error) = slint::set_xdg_app_id(LINUX_APP_ID) {
            warn!(?error, "failed to set the Linux XDG app id");
        }

        if let Err(error) = install_linux_desktop_entry() {
            warn!(
                ?error,
                "failed to install the local Bardic Chord desktop entry"
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn install_linux_desktop_entry() -> Result<(), String> {
    let base_dirs =
        BaseDirs::new().ok_or_else(|| "failed to locate the user base directories".to_string())?;
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| base_dirs.home_dir().join(".local/share"));
    let applications_dir = data_home.join("applications");
    let icon_dir = data_home.join("icons/hicolor/256x256/apps");
    let desktop_file_path = applications_dir.join(format!("{LINUX_APP_ID}.desktop"));
    let icon_target_path = icon_dir.join(format!("{LINUX_APP_ID}.png"));
    let source_icon_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../icon.png");
    let executable = env::current_exe()
        .map_err(|error| format!("failed to locate the Bardic Chord executable: {error}"))?;

    fs::create_dir_all(&applications_dir)
        .map_err(|error| format!("failed to create the applications directory: {error}"))?;
    fs::create_dir_all(&icon_dir)
        .map_err(|error| format!("failed to create the icon directory: {error}"))?;

    if source_icon_path.exists() {
        let _ = fs::copy(&source_icon_path, &icon_target_path)
            .map_err(|error| format!("failed to install the Bardic Chord icon: {error}"))?;
    }

    let desktop_file = format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Bardic Chord\nComment=A mythical desktop relay for routing desktop audio into Discord voice.\nExec={}\nIcon={}\nTerminal=false\nCategories=AudioVideo;Utility;\nStartupWMClass={}\nStartupNotify=true\n",
        executable.display(),
        LINUX_APP_ID,
        LINUX_APP_ID,
    );

    fs::write(&desktop_file_path, desktop_file)
        .map_err(|error| format!("failed to write the desktop entry: {error}"))?;

    Ok(())
}

fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl SizedLogWriter {
    fn new(path: PathBuf) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|error| format!("failed to open Bardic Chord log file: {error}"))?;

        let writer = Self {
            state: Arc::new(Mutex::new(SizedLogWriterState { file })),
        };
        writer.truncate_if_needed()?;
        Ok(writer)
    }

    fn truncate_if_needed(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "log writer mutex was poisoned".to_string())?;

        let size = state
            .file
            .metadata()
            .map_err(|error| format!("failed to inspect Bardic Chord log file: {error}"))?
            .len();

        if size > LOG_MAX_BYTES {
            state
                .file
                .set_len(0)
                .map_err(|error| format!("failed to truncate Bardic Chord log file: {error}"))?;
            state
                .file
                .seek(SeekFrom::Start(0))
                .map_err(|error| format!("failed to seek Bardic Chord log file: {error}"))?;
        }

        Ok(())
    }
}

impl Write for SizedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log writer mutex poisoned"))?;

        let current_size = state.file.metadata()?.len();
        if current_size + buf.len() as u64 > LOG_MAX_BYTES {
            state.file.set_len(0)?;
            state.file.seek(SeekFrom::Start(0))?;
        }

        state.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log writer mutex poisoned"))?;
        state.file.flush()
    }
}

fn init_logging() -> Result<(), String> {
    let log_dir = env::current_dir()
        .map_err(|error| format!("failed to resolve current working directory: {error}"))?
        .join(LOCAL_STATE_DIR);
    fs::create_dir_all(&log_dir)
        .map_err(|error| format!("failed to create Bardic Chord local state directory: {error}"))?;
    let log_path = log_dir.join("bardic-chord.log");
    let log_writer = SizedLogWriter::new(log_path)?;

    let writer = move || log_writer.clone();

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_writer(writer)
        .try_init()
        .map_err(|error| format!("failed to initialize tracing subscriber: {error}"))
}

fn hydrate_ui(ui: &AppWindow, backend: &Backend, runtime: &Runtime) -> Result<(), String> {
    let view = runtime.block_on(collect_view_state(backend))?;
    apply_snapshot(ui, &view.snapshot);
    apply_audio_output_report(ui, &view.audio_output);
    apply_relay_report(ui, &view.relay);
    ui.set_party_can_stop(view.party_can_stop);
    ui.set_discord_status_title("Connect Discord to continue".into());
    ui.set_discord_status_body(
        "Paste the bot token, press Connect Discord, and Bardic Chord will guide the rest.".into(),
    );
    ui.set_discord_route_text("No server or voice channel has been chosen yet.".into());
    refresh_editor_derived(ui);
    refresh_runtime_summary(ui);
    Ok(())
}

async fn collect_view_state(backend: &Backend) -> Result<ViewState, String> {
    Ok(ViewState {
        snapshot: backend.load_app_snapshot()?,
        audio_output: backend.get_audio_output_status().await,
        relay: backend.get_discord_relay_status().await,
        party_can_stop: backend.can_stop_party().await,
    })
}

fn bind_callbacks(
    ui: &AppWindow,
    backend: Arc<Backend>,
    runtime: Handle,
    discord_validation_generation: Arc<AtomicU64>,
) {
    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    let discord_validation_generation_clone = discord_validation_generation.clone();
    ui.on_draft_changed(move || {
        if let Some(ui) = weak.upgrade() {
            sync_discord_picker_ids(&ui);
            normalize_ui_inputs(&ui);
            autosave_current_settings(&ui, &backend_clone);
            refresh_editor_derived(&ui);

            if ui.get_discord_bot_token().trim().is_empty() {
                discord_validation_generation_clone.fetch_add(1, Ordering::SeqCst);
                clear_discord_connection_state(&ui);
            } else {
                schedule_discord_validation(
                    &ui,
                    backend_clone.clone(),
                    runtime_clone.clone(),
                    discord_validation_generation_clone.clone(),
                    Duration::from_millis(800),
                    false,
                );
            }
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    ui.on_request_save(move || {
        if let Some(ui) = weak.upgrade() {
            normalize_ui_inputs(&ui);
            let settings = settings_from_ui(&ui);
            match backend_clone.save_settings(settings) {
                Ok(snapshot) => {
                    apply_snapshot_metadata(&ui, &snapshot);
                    refresh_editor_derived(&ui);
                    refresh_runtime_summary(&ui);
                }
                Err(error) => ui.set_save_message(error.into()),
            }
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    ui.on_request_validate_discord(move || {
        if let Some(ui) = weak.upgrade() {
            set_busy(
                &ui,
                Some("Consulting Discord and checking the chosen route..."),
            );
            normalize_ui_inputs(&ui);
            let settings = settings_from_ui(&ui);
            let ui_weak = ui.as_weak();
            let backend = backend_clone.clone();
            runtime_clone.spawn(async move {
                let result = backend.validate_discord_setup(settings).await;
                queue_ui(ui_weak, move |ui| {
                    clear_busy(&ui);
                    match result {
                        Ok(report) => apply_discord_validation(&ui, &report),
                        Err(error) => {
                            ui.set_discord_status_title("Discord needs attention".into());
                            ui.set_discord_status_body(error.into());
                            ui.set_discord_connected(false);
                            ui.set_discord_route_text(
                                "Check the bot token and try again. If the bot was just invited, give Discord a moment and press Refresh Server List."
                                    .into(),
                            );
                            ui.set_discord_invite_url(
                                discord_invite_url_from_token(ui.get_discord_bot_token().as_str())
                                    .unwrap_or_default()
                                    .into(),
                            );
                        }
                    }
                    refresh_runtime_summary(&ui);
                });
            });
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    let discord_validation_generation_connect = discord_validation_generation.clone();
    ui.on_request_connect_discord(move || {
        if let Some(ui) = weak.upgrade() {
            let url = if ui.get_discord_invite_url().trim().is_empty() {
                discord_invite_url_from_token(ui.get_discord_bot_token().as_str())
                    .unwrap_or_default()
                    .into()
            } else {
                ui.get_discord_invite_url()
            };
            if url.trim().is_empty() {
                ui.set_discord_status_title("Discord invite link missing".into());
                ui.set_discord_status_body(
                    "Bardic Chord could not build an authorize link from that token. Paste the full bot token from the Discord Developer Portal."
                        .into(),
                );
                return;
            }

            if let Err(error) = backend_clone.open_url(url.as_str()) {
                ui.set_discord_status_title("Could not open Discord".into());
                ui.set_discord_status_body(error.into());
            } else {
                ui.set_discord_invite_url(url.clone());
                ui.set_discord_status_title("Finish the Discord invite".into());
                ui.set_discord_status_body(
                    "The browser is open. Invite the bot to your server and come back here. Bardic Chord will keep checking for the server and channel list automatically."
                        .into(),
                );
                start_discord_invite_poll(
                    ui.as_weak(),
                    backend_clone.clone(),
                    settings_from_ui(&ui),
                    runtime_clone.clone(),
                );
                schedule_discord_validation(
                    &ui,
                    backend_clone.clone(),
                    runtime_clone.clone(),
                    discord_validation_generation_connect.clone(),
                    Duration::from_millis(50),
                    false,
                );
            }
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    let discord_validation_generation_clone = discord_validation_generation.clone();
    ui.on_request_disconnect_discord(move || {
        if let Some(ui) = weak.upgrade() {
            discord_validation_generation_clone.fetch_add(1, Ordering::SeqCst);
            set_busy(&ui, Some("Disconnecting the bot token and clearing Discord setup..."));

            let mut cleared = settings_from_ui(&ui);
            cleared.discord_bot_token.clear();
            cleared.guild_id.clear();
            cleared.voice_channel_id.clear();
            cleared.tracked_user_id.clear();
            let ui_weak = ui.as_weak();
            let backend = backend_clone.clone();
            runtime_clone.spawn(async move {
                let relay = backend.stop_discord_relay().await;
                let snapshot = backend.save_settings(cleared);
                let audio_output = backend.get_audio_output_status().await;
                let relay_status = backend.get_discord_relay_status().await;
                let party_can_stop = backend.can_stop_party().await;

                queue_ui(ui_weak, move |ui| {
                    clear_busy(&ui);
                    clear_discord_connection_state(&ui);

                    match snapshot {
                        Ok(snapshot) => apply_snapshot(&ui, &snapshot),
                        Err(error) => ui.set_save_message(error.into()),
                    }
                    if let Err(error) = relay {
                        ui.set_relay_status_title("Party stop needs attention".into());
                        ui.set_relay_status_body(error.into());
                    }
                    apply_audio_output_report(&ui, &audio_output);
                    apply_relay_report(&ui, &relay_status);
                    ui.set_party_can_stop(party_can_stop);
                    ui.set_discord_status_title("Discord has been disconnected".into());
                    ui.set_discord_status_body(
                        "The saved bot token was removed from this computer. Paste a new token whenever you want to reconnect."
                            .into(),
                    );
                    ui.set_discord_route_text("No server or voice channel has been chosen yet.".into());
                    refresh_editor_derived(&ui);
                    refresh_runtime_summary(&ui);
                });
            });
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    ui.on_request_prepare_audio(move || {
        if let Some(ui) = weak.upgrade() {
            set_busy(
                &ui,
                Some("Preparing the local Bardic Chord audio output..."),
            );
            normalize_ui_inputs(&ui);
            let settings = settings_from_ui(&ui);
            let ui_weak = ui.as_weak();
            let backend = backend_clone.clone();
            runtime_clone.spawn(async move {
                let result = backend.prepare_audio_output(settings).await;
                let snapshot = backend.load_app_snapshot();
                let relay = backend.get_discord_relay_status().await;
                let party_can_stop = backend.can_stop_party().await;

                queue_ui(ui_weak, move |ui| {
                    clear_busy(&ui);
                    match result {
                        Ok(report) => {
                            let output_name = report
                                .output_name
                                .clone()
                                .unwrap_or_else(|| DEFAULT_AUDIO_OUTPUT_NAME.into());
                            let capture_target = {
                                let value = ui.get_capture_target();
                                if value.trim().is_empty() {
                                    DEFAULT_CAPTURE_TARGET.to_string()
                                } else {
                                    value.to_string()
                                }
                            };
                            match snapshot {
                                Ok(snapshot) => apply_snapshot(&ui, &snapshot),
                                Err(error) => ui.set_save_message(error.into()),
                            }
                            apply_audio_output_report(&ui, &report);
                            apply_relay_report(&ui, &relay);
                            ui.set_current_page(3);
                            ui.set_save_message(
                                format!(
                                    "Audio is ready. Route or attach `{capture_target}` to `{output_name}` if needed, then press Start Party."
                                )
                                .into(),
                            );
                            if !ui.get_relay_active() {
                                ui.set_relay_status_title("Prepare your app next".into());
                                ui.set_relay_status_body(
                                    format!(
                                        "Bardic Chord prepared `{output_name}` successfully.\n\nIf `{capture_target}` is already playing, Bardic Chord will try to move or attach to it automatically when possible. If not, open your system sound settings or volume mixer and route `{capture_target}` to that output."
                                    )
                                    .into(),
                                );
                            }
                        }
                        Err(error) => {
                            ui.set_audio_status_title("Desktop audio needs attention".into());
                            ui.set_audio_status_body(error.into());
                        }
                    }
                    ui.set_party_can_stop(party_can_stop);
                    refresh_editor_derived(&ui);
                    refresh_runtime_summary(&ui);
                });
            });
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    ui.on_request_stop_audio(move || {
        if let Some(ui) = weak.upgrade() {
            set_busy(
                &ui,
                Some("Turning off the local Bardic Chord audio output..."),
            );
            let ui_weak = ui.as_weak();
            let backend = backend_clone.clone();
            runtime_clone.spawn(async move {
                let result = backend.stop_audio_output().await;
                let relay = backend.get_discord_relay_status().await;
                let party_can_stop = backend.can_stop_party().await;

                queue_ui(ui_weak, move |ui| {
                    clear_busy(&ui);
                    match result {
                        Ok(report) => apply_audio_output_report(&ui, &report),
                        Err(error) => {
                            ui.set_audio_status_title("Desktop audio needs attention".into());
                            ui.set_audio_status_body(error.into());
                        }
                    }
                    apply_relay_report(&ui, &relay);
                    ui.set_party_can_stop(party_can_stop);
                    refresh_runtime_summary(&ui);
                });
            });
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    ui.on_request_stop_party(move || {
        if let Some(ui) = weak.upgrade() {
            set_busy(
                &ui,
                Some("Stopping the live party and silencing desktop audio plus relay..."),
            );
            let ui_weak = ui.as_weak();
            let backend = backend_clone.clone();
            runtime_clone.spawn(async move {
                let result = backend.shutdown().await;
                let audio_output = backend.get_audio_output_status().await;
                let relay = backend.get_discord_relay_status().await;
                let party_can_stop = backend.can_stop_party().await;

                queue_ui(ui_weak, move |ui| {
                    clear_busy(&ui);
                    apply_audio_output_report(&ui, &audio_output);
                    apply_relay_report(&ui, &relay);

                    if let Err(error) = result {
                        ui.set_relay_status_title("Party stop needs attention".into());
                        ui.set_relay_status_body(error.into());
                    }

                    ui.set_party_can_stop(party_can_stop);
                    refresh_runtime_summary(&ui);
                });
            });
        }
    });

    let weak = ui.as_weak();
    let backend_clone = backend.clone();
    let runtime_clone = runtime.clone();
    ui.on_request_launch_party(move || {
        if let Some(ui) = weak.upgrade() {
            let audio_prepared = ui.get_audio_prepared();
            set_busy(
                &ui,
                Some("Launching the party by preparing desktop audio, then arming the relay..."),
            );
            normalize_ui_inputs(&ui);
            let settings = settings_from_ui(&ui);
            let ui_weak = ui.as_weak();
            let backend = backend_clone.clone();
            runtime_clone.spawn(async move {
                let mut audio_error = None::<String>;
                let mut relay_error = None::<String>;

                if !audio_prepared {
                    if let Err(error) = backend.prepare_audio_output(settings.clone()).await {
                        audio_error = Some(error);
                    }
                }
                if relay_error.is_none() && audio_error.is_none() {
                    if let Err(error) = backend.start_discord_relay(settings).await {
                        relay_error = Some(error);
                    }
                }

                let snapshot = backend.load_app_snapshot();
                let audio_output = backend.get_audio_output_status().await;
                let relay = backend.get_discord_relay_status().await;
                let party_can_stop = backend.can_stop_party().await;

                queue_ui(ui_weak, move |ui| {
                    clear_busy(&ui);
                    match snapshot {
                        Ok(snapshot) => apply_snapshot_metadata(&ui, &snapshot),
                        Err(error) => ui.set_save_message(error.into()),
                    }
                    apply_audio_output_report(&ui, &audio_output);
                    apply_relay_report(&ui, &relay);

                    if let Some(error) = audio_error {
                        ui.set_audio_status_title("Desktop audio needs attention".into());
                        ui.set_audio_status_body(error.into());
                    }
                    if let Some(error) = relay_error {
                        ui.set_relay_status_title("Party start needs attention".into());
                        ui.set_relay_status_body(error.into());
                    }

                    ui.set_party_can_stop(party_can_stop);
                    refresh_editor_derived(&ui);
                    refresh_runtime_summary(&ui);
                });
            });
        }
    });
}

fn queue_ui<F>(weak: slint::Weak<AppWindow>, callback: F)
where
    F: FnOnce(AppWindow) + Send + 'static,
{
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            callback(ui);
        }
    });
}

fn start_discord_invite_poll(
    weak: slint::Weak<AppWindow>,
    backend: Arc<Backend>,
    settings: Settings,
    runtime: Handle,
) {
    runtime.spawn(async move {
        for attempt in 1..=DISCORD_INVITE_POLL_ATTEMPTS {
            sleep(DISCORD_INVITE_POLL_INTERVAL).await;

            let result = backend.validate_discord_setup(settings.clone()).await;
            let routes_found = match &result {
                Ok(report) => {
                    !report.guild_options.is_empty() || !report.voice_channel_options.is_empty()
                }
                Err(_) => false,
            };

            let weak_for_ui = weak.clone();
            queue_ui(weak_for_ui, move |ui| {
                match result {
                    Ok(report) if routes_found => {
                        apply_discord_validation(&ui, &report);
                        ui.set_discord_status_body(
                            format!(
                                "{}\n\nDiscord invite confirmed automatically. Bardic Chord loaded the latest server and route choices for you.",
                                report.message
                            )
                            .into(),
                        );
                    }
                    Ok(_) => {
                        ui.set_discord_status_title("Waiting for Discord".into());
                        ui.set_discord_status_body(
                            format!(
                                "Bardic Chord is checking Discord every 5 seconds.\n\nAttempt {attempt}/{DISCORD_INVITE_POLL_ATTEMPTS}. Finish inviting the bot, then return here."
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
                        ui.set_discord_status_title("Waiting for Discord".into());
                        ui.set_discord_status_body(
                            format!(
                                "Bardic Chord is still checking Discord.\n\nAttempt {attempt}/{DISCORD_INVITE_POLL_ATTEMPTS}. Latest check: {error}"
                            )
                            .into(),
                        );
                    }
                }
                refresh_runtime_summary(&ui);
            });

            if routes_found {
                break;
            }

            if attempt == DISCORD_INVITE_POLL_ATTEMPTS {
                queue_ui(weak.clone(), move |ui| {
                    ui.set_discord_status_title("Still waiting for Discord".into());
                    ui.set_discord_status_body(
                        "Bardic Chord still does not see the bot in your servers. Finish the invite in the browser, then press Refresh Server List."
                            .into(),
                    );
                    refresh_runtime_summary(&ui);
                });
            }
        }
    });
}

fn start_status_poll(weak: slint::Weak<AppWindow>, backend: Arc<Backend>, runtime: Handle) {
    runtime.spawn(async move {
        loop {
            sleep(STATUS_POLL_INTERVAL).await;

            let audio_output = backend.get_audio_output_status().await;
            let relay = backend.get_discord_relay_status().await;
            let party_can_stop = backend.can_stop_party().await;

            queue_ui(weak.clone(), move |ui| {
                apply_audio_output_report(&ui, &audio_output);
                apply_relay_report(&ui, &relay);
                ui.set_party_can_stop(party_can_stop);
                refresh_runtime_summary(&ui);
            });
        }
    });
}

fn schedule_discord_validation(
    ui: &AppWindow,
    backend: Arc<Backend>,
    runtime: Handle,
    generation: Arc<AtomicU64>,
    delay: Duration,
    show_busy: bool,
) {
    let settings = settings_from_ui(ui);
    if settings.discord_bot_token.trim().is_empty() {
        return;
    }

    let current_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let ui_weak = ui.as_weak();

    runtime.spawn(async move {
        sleep(delay).await;
        if generation.load(Ordering::SeqCst) != current_generation {
            return;
        }

        let result = backend.validate_discord_setup(settings).await;
        queue_ui(ui_weak, move |ui| {
            if show_busy {
                clear_busy(&ui);
            }

            match result {
                Ok(report) => apply_discord_validation(&ui, &report),
                Err(error) => {
                    ui.set_discord_connected(false);
                    ui.set_discord_status_title("Discord needs attention".into());
                    ui.set_discord_status_body(error.into());
                    ui.set_discord_route_text(
                        "Check the bot token and try again. If the bot was just invited, give Discord a moment and press Refresh Server List."
                            .into(),
                    );
                    ui.set_discord_invite_url(
                        discord_invite_url_from_token(ui.get_discord_bot_token().as_str())
                            .unwrap_or_default()
                            .into(),
                    );
                }
            }
            refresh_runtime_summary(&ui);
        });
    });
}

fn route_ready_from_ui(ui: &AppWindow) -> bool {
    !ui.get_guild_id().trim().is_empty() && !ui.get_voice_channel_id().trim().is_empty()
}

fn settings_from_ui(ui: &AppWindow) -> Settings {
    Settings {
        discord_bot_token: ui.get_discord_bot_token().to_string(),
        tracked_user_id: if ui.get_follow_user_enabled() {
            normalize_discord_user_reference(ui.get_tracked_user_id().as_str())
        } else {
            String::new()
        },
        guild_id: ui.get_guild_id().to_string(),
        voice_channel_id: ui.get_voice_channel_id().to_string(),
        audio_output_name: ui.get_audio_output_name().to_string(),
        capture_target: ui.get_capture_target().to_string(),
        bot_display_name: ui.get_bot_display_name().to_string(),
    }
}

fn apply_snapshot(ui: &AppWindow, snapshot: &AppSnapshot) {
    ui.set_discord_bot_token(snapshot.settings.discord_bot_token.clone().into());
    ui.set_follow_user_enabled(!snapshot.settings.tracked_user_id.trim().is_empty());
    ui.set_tracked_user_id(snapshot.settings.tracked_user_id.clone().into());
    ui.set_guild_id(snapshot.settings.guild_id.clone().into());
    ui.set_voice_channel_id(snapshot.settings.voice_channel_id.clone().into());
    ui.set_audio_output_name(snapshot.settings.audio_output_name.clone().into());
    ui.set_capture_target(snapshot.settings.capture_target.clone().into());
    ui.set_bot_display_name(snapshot.settings.bot_display_name.clone().into());
    apply_snapshot_metadata(ui, snapshot);
    sync_discord_picker_ids(ui);
    refresh_editor_derived(ui);
}

fn apply_snapshot_metadata(ui: &AppWindow, snapshot: &AppSnapshot) {
    ui.set_save_message(
        snapshot
            .last_saved_at
            .as_ref()
            .map(|saved| format!("Saved locally {saved}."))
            .unwrap_or_else(|| "Local settings are ready to be scribed.".into())
            .into(),
    );
    ui.set_overview_health_text(format_health_tiles(&snapshot.health_tiles));
    ui.set_ritual_steps_text(format_steps(&snapshot.ritual_steps));
    ui.set_activity_text(format_activity(&snapshot.activity));
}

fn apply_discord_validation(ui: &AppWindow, report: &DiscordValidationReport) {
    set_discord_guild_models(ui, &report.guild_options, report.guild_id.as_deref());
    set_discord_voice_channel_models(
        ui,
        &report.voice_channel_options,
        report.voice_channel_id.as_deref(),
    );
    if let Some(guild_id) = &report.guild_id {
        ui.set_guild_id(guild_id.clone().into());
    }
    if let Some(channel_id) = &report.voice_channel_id {
        ui.set_voice_channel_id(channel_id.clone().into());
    }
    ui.set_discord_connected(true);

    ui.set_discord_status_title("Discord is ready".into());

    let app_name = report
        .application_name
        .clone()
        .unwrap_or_else(|| "Discord application".into());
    let guild = report
        .guild_name
        .clone()
        .unwrap_or_else(|| "No guild verified yet".into());
    let channel = report
        .voice_channel_name
        .clone()
        .unwrap_or_else(|| "No voice channel verified yet".into());
    let channel_kind = report
        .voice_channel_kind
        .clone()
        .unwrap_or_else(|| "Unknown".into());

    ui.set_discord_status_body(
        format!(
            "{}\n\nBot name: {}\nBot ID: {}\nApp: {}",
            report.message, report.bot_username, report.bot_id, app_name
        )
        .into(),
    );
    ui.set_discord_route_text(
        format!(
            "Selected server: {guild}\nSelected channel: {channel}\nChannel type: {channel_kind}\nServers found: {}\nVoice channels found: {}",
            report.guild_options.len(),
            report.voice_channel_options.len()
        )
        .into(),
    );
    ui.set_discord_invite_url(report.invite_url.clone().unwrap_or_default().into());
    sync_discord_picker_ids(ui);
    refresh_editor_derived(ui);
}

fn apply_audio_output_report(ui: &AppWindow, report: &AudioOutputReport) {
    ui.set_audio_prepared(report.active);
    ui.set_audio_status_title(
        if report.active {
            "Desktop audio is ready"
        } else {
            "Desktop audio is waiting"
        }
        .into(),
    );

    let output_name = report
        .output_name
        .clone()
        .unwrap_or_else(|| DEFAULT_AUDIO_OUTPUT_NAME.into());
    let started_at = report
        .started_at
        .clone()
        .unwrap_or_else(|| "Not running".into());

    ui.set_audio_status_body(
        format!(
            "{}\n\nOutput: {}\nPlatform: {}\nStarted: {}",
            report.message, output_name, report.platform, started_at
        )
        .into(),
    );
    ui.set_audio_instructions_text(format_steps(&report.instruction_steps));
}

fn apply_relay_report(ui: &AppWindow, report: &DiscordRelayReport) {
    ui.set_relay_active(report.active);
    ui.set_relay_status_title(
        if report.active {
            "Party is live"
        } else {
            "Party is off"
        }
        .into(),
    );

    let guild = report
        .guild_name
        .clone()
        .unwrap_or_else(|| "No guild active".into());
    let channel = report
        .voice_channel_name
        .clone()
        .unwrap_or_else(|| "No voice channel active".into());
    let bot = report
        .bot_username
        .clone()
        .unwrap_or_else(|| "No bot identity confirmed".into());
    let started_at = report
        .started_at
        .clone()
        .unwrap_or_else(|| "Not running".into());

    ui.set_relay_status_body(
        format!(
            "{}\n\nGuild: {}\nVoice channel: {}\nBot: {}\nStarted: {}",
            report.message, guild, channel, bot, started_at
        )
        .into(),
    );
    ui.set_relay_instructions_text(format_steps(&report.instruction_steps));
}

fn refresh_editor_derived(ui: &AppWindow) {
    let required_count = 6;
    let completion_count = [
        ui.get_discord_bot_token(),
        if route_ready_from_ui(ui) {
            SharedString::from("ready")
        } else {
            SharedString::default()
        },
        ui.get_audio_output_name(),
        ui.get_capture_target(),
        ui.get_bot_display_name(),
        if ui.get_audio_prepared() {
            SharedString::from("ready")
        } else {
            SharedString::default()
        },
    ]
    .into_iter()
    .filter(|value| !value.trim().is_empty())
    .count();

    let bot_display_name = ui.get_bot_display_name();
    let audio_output_name = ui.get_audio_output_name();
    let capture_target = ui.get_capture_target();
    let voice_channel_id = ui.get_voice_channel_id();

    let companion = non_empty(bot_display_name.as_str()).unwrap_or("the bardic relay");
    let output = non_empty(audio_output_name.as_str()).unwrap_or(DEFAULT_AUDIO_OUTPUT_NAME);
    let target = non_empty(capture_target.as_str()).unwrap_or(DEFAULT_CAPTURE_TARGET);
    let channel = non_empty(voice_channel_id.as_str()).unwrap_or("the chosen voice channel");
    let preview_suffix = if ui.get_follow_user_enabled() {
        let tracked_user_id = ui.get_tracked_user_id();
        let tracked = non_empty(tracked_user_id.as_str()).unwrap_or("the selected user");
        format!(", following {tracked} when they are already in voice or move later")
    } else {
        String::new()
    };

    ui.set_completion_summary(
        format!(
            "{completion_count} of {required_count} setup items are ready. Bardic Chord saves your changes automatically."
        )
        .into(),
    );
    ui.set_preview_line(
        format!(
            "{companion} will send `{target}` through `{output}` into {channel}{preview_suffix}."
        )
        .into(),
    );
    ui.set_discord_route_ready(route_ready_from_ui(ui));
}

fn set_discord_guild_models(
    ui: &AppWindow,
    guilds: &[DiscordGuildOption],
    selected_id: Option<&str>,
) {
    let labels = guilds
        .iter()
        .map(|guild| SharedString::from(format!("{} ({})", guild.name, guild.id)))
        .collect::<Vec<_>>();
    let ids = guilds
        .iter()
        .map(|guild| SharedString::from(guild.id.clone()))
        .collect::<Vec<_>>();
    let selected_index = selected_id
        .and_then(|selected_id| guilds.iter().position(|guild| guild.id == selected_id))
        .unwrap_or(0) as i32;

    ui.set_discord_guild_model(ModelRc::new(VecModel::from(labels)));
    ui.set_discord_guild_id_model(ModelRc::new(VecModel::from(ids)));
    ui.set_discord_guild_index(selected_index);
}

fn set_discord_voice_channel_models(
    ui: &AppWindow,
    channels: &[DiscordVoiceChannelOption],
    selected_id: Option<&str>,
) {
    let labels = channels
        .iter()
        .map(|channel| SharedString::from(format!("{} ({})", channel.name, channel.kind)))
        .collect::<Vec<_>>();
    let ids = channels
        .iter()
        .map(|channel| SharedString::from(channel.id.clone()))
        .collect::<Vec<_>>();
    let selected_index = selected_id
        .and_then(|selected_id| {
            channels
                .iter()
                .position(|channel| channel.id == selected_id)
        })
        .unwrap_or(0) as i32;

    ui.set_discord_voice_channel_model(ModelRc::new(VecModel::from(labels)));
    ui.set_discord_voice_channel_id_model(ModelRc::new(VecModel::from(ids)));
    ui.set_discord_voice_channel_index(selected_index);
}

fn sync_discord_picker_ids(ui: &AppWindow) {
    let guild_index = ui.get_discord_guild_index() as usize;
    let channel_index = ui.get_discord_voice_channel_index() as usize;
    let guild_ids = ui.get_discord_guild_id_model();
    let channel_ids = ui.get_discord_voice_channel_id_model();

    if guild_index < guild_ids.row_count() {
        if let Some(id) = guild_ids.row_data(guild_index) {
            ui.set_guild_id(id);
        }
    }

    if channel_index < channel_ids.row_count() {
        if let Some(id) = channel_ids.row_data(channel_index) {
            ui.set_voice_channel_id(id);
        }
    }
}

fn refresh_runtime_summary(ui: &AppWindow) {
    let discord = if ui.get_discord_connected() {
        "Discord ready"
    } else {
        "Discord waiting"
    };
    let audio = if ui.get_audio_prepared() {
        "audio output ready"
    } else {
        "audio output off"
    };
    let relay = if ui.get_relay_active() {
        "party live"
    } else {
        "party off"
    };

    ui.set_runtime_summary(format!("{discord}, {audio}, {relay}.").into());
}

fn clear_discord_connection_state(ui: &AppWindow) {
    ui.set_discord_connected(false);
    ui.set_discord_route_ready(false);
    ui.set_discord_invite_url(SharedString::default());
    ui.set_discord_guild_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_guild_id_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_voice_channel_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_voice_channel_id_model(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_discord_guild_index(0);
    ui.set_discord_voice_channel_index(0);
    ui.set_guild_id(SharedString::default());
    ui.set_voice_channel_id(SharedString::default());
}

fn normalize_ui_inputs(ui: &AppWindow) {
    if !ui.get_follow_user_enabled() {
        if !ui.get_tracked_user_id().trim().is_empty() {
            ui.set_tracked_user_id(SharedString::default());
        }
        return;
    }

    let normalized = normalize_discord_user_reference(ui.get_tracked_user_id().as_str());
    if normalized != ui.get_tracked_user_id().as_str() {
        ui.set_tracked_user_id(normalized.into());
    }
}

fn autosave_current_settings(ui: &AppWindow, backend: &Backend) {
    match backend.save_settings(settings_from_ui(ui)) {
        Ok(snapshot) => apply_snapshot_metadata(ui, &snapshot),
        Err(error) => ui.set_save_message(error.into()),
    }
}

fn set_busy(ui: &AppWindow, label: Option<&str>) {
    let busy = label.unwrap_or_default();
    ui.set_busy_label(busy.into());
    ui.set_busy_visible(!busy.is_empty());
}

fn clear_busy(ui: &AppWindow) {
    ui.set_busy_label(SharedString::default());
    ui.set_busy_visible(false);
}

fn format_health_tiles(tiles: &[HealthTile]) -> SharedString {
    tiles
        .iter()
        .map(|tile| format!("{} - {}.\n{}", tile.label, tile.state, tile.detail))
        .collect::<Vec<_>>()
        .join("\n\n")
        .into()
}

fn format_steps(steps: &[String]) -> SharedString {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| format!("{}. {}", index + 1, step))
        .collect::<Vec<_>>()
        .join("\n")
        .into()
}

fn format_activity(entries: &[ActivityEntry]) -> SharedString {
    entries
        .iter()
        .map(|entry| format!("{} - {}\n{}", entry.timestamp, entry.title, entry.detail))
        .collect::<Vec<_>>()
        .join("\n\n")
        .into()
}

fn non_empty<'a>(value: &'a str) -> Option<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn normalize_discord_user_reference(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return trimmed.to_string();
    }

    let digits = trimmed
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();

    if (15..=21).contains(&digits.len()) {
        digits
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_discord_user_reference_extracts_digits_from_mentions() {
        assert_eq!(
            normalize_discord_user_reference("<@1494272667946057819>"),
            "1494272667946057819"
        );
        assert_eq!(
            normalize_discord_user_reference("https://discord.com/users/1494272667946057819"),
            "1494272667946057819"
        );
    }

    #[test]
    fn normalize_discord_user_reference_preserves_non_snowflakes() {
        assert_eq!(
            normalize_discord_user_reference("amber minstrel"),
            "amber minstrel"
        );
    }

    #[test]
    fn format_steps_numbers_each_step() {
        let steps = vec!["Prepare the output".into(), "Start the party".into()];
        assert_eq!(
            format_steps(&steps).as_str(),
            "1. Prepare the output\n2. Start the party"
        );
    }

    #[test]
    fn format_health_tiles_keeps_label_state_and_detail() {
        let tiles = vec![HealthTile {
            id: "discord".into(),
            label: "Discord".into(),
            state: "Ready".into(),
            detail: "The bot token is saved.".into(),
            tone: "ready".into(),
        }];

        assert_eq!(
            format_health_tiles(&tiles).as_str(),
            "Discord - Ready.\nThe bot token is saved."
        );
    }

    #[test]
    fn format_activity_joins_entries_in_order() {
        let entries = vec![
            ActivityEntry {
                id: "one".into(),
                title: "Setup".into(),
                detail: "Desktop audio prepared".into(),
                timestamp: "Now".into(),
                tone: "ready".into(),
            },
            ActivityEntry {
                id: "two".into(),
                title: "Launch".into(),
                detail: "Party armed".into(),
                timestamp: "Later".into(),
                tone: "ready".into(),
            },
        ];

        assert_eq!(
            format_activity(&entries).as_str(),
            "Now - Setup\nDesktop audio prepared\n\nLater - Launch\nParty armed"
        );
    }
}
