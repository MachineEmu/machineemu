//! Native viewer for a machineemu instance's H.264 display stream.
//!
//! It requests a ticket from `machineemu-daemon`, opens the `video` WebSocket,
//! decodes H.264 with GStreamer (hardware first), plays D-Bus guest audio and
//! forwards keyboard, pointer, resize and clipboard input.

mod app;
mod keymap;
mod media;
mod net;
mod protocol;
mod shortcuts;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use gstreamer as gst;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

use app::{App, Options, UiEvent};
use media::{DecoderChoice, Video, VideoCounters};

const DEFAULT_ENDPOINT: &str = "127.0.0.1:8787";
const DEFAULT_TOKEN: &str = "machineemu-dev-token";

#[derive(Parser)]
#[command(
    name = "machineemu-viewer",
    about = "View and control an instance's H.264 display"
)]
struct Args {
    /// Instance ID with a live run and `devices.h264` enabled.
    instance: String,
    /// Daemon endpoint, `host:port` or `unix:/path`. Defaults to the
    /// `client` section of machineemu.yaml, then 127.0.0.1:8787.
    #[arg(long)]
    endpoint: Option<String>,
    /// Daemon bearer token. Defaults to `client.token` in machineemu.yaml.
    #[arg(long)]
    token: Option<String>,
    /// machineemu.yaml to read the client section from.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Watch without sending keyboard, pointer, resize or clipboard input.
    #[arg(long)]
    view_only: bool,
    /// Take display input from the current VNC or video controller.
    #[arg(long, conflicts_with = "view_only")]
    takeover: bool,
    /// Keep the guest resolution when the window is resized.
    #[arg(long)]
    no_resize_guest: bool,
    /// Do not play guest audio.
    #[arg(long)]
    no_audio: bool,
    /// Do not share the clipboard with the guest.
    #[arg(long)]
    no_clipboard: bool,
    /// H.264 decoder: `auto` (hardware first, software if none or if it
    /// fails), `hardware`, `software`, or a GStreamer element name.
    #[arg(long, default_value = "auto")]
    decoder: DecoderChoice,
}

fn client(args: &Args) -> Result<(net::Endpoint, String)> {
    let (config, path) = machineemu_core::config::load_config(args.config.as_deref())
        .map_err(|error| anyhow!("cannot load configuration: {error}"))?;
    let client = config.client.unwrap_or_default();
    let endpoint = args
        .endpoint
        .clone()
        .or_else(|| {
            client.unix_socket.map(|socket| {
                format!(
                    "unix:{}",
                    machineemu_core::config::resolve_config_path(path.as_deref(), socket).display()
                )
            })
        })
        .or(client.endpoint)
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());
    let token = args
        .token
        .clone()
        .or(client.token)
        .unwrap_or_else(|| DEFAULT_TOKEN.to_owned());
    Ok((net::Endpoint::parse(&endpoint), token))
}

/// Log to stderr, filtered by `RUST_LOG` (for example
/// `RUST_LOG=machineemu_viewer=debug`). The default shows this crate's
/// informational messages and warnings from dependencies.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,machineemu_viewer=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn run() -> Result<()> {
    let args = Args::parse();
    init_tracing();
    gst::init().context("cannot initialise GStreamer")?;
    info!(gstreamer = %gst::version_string(), "initialised GStreamer");
    let plan = media::select(&args.decoder)?;
    if !plan.hardware && args.decoder == DecoderChoice::Auto {
        warn!(
            decoder = plan.decoder,
            "no hardware H.264 decoder found; decoding in software"
        );
    }
    info!(
        decoder = plan.decoder,
        postproc = plan.postproc,
        hardware = plan.hardware,
        "selected decoder"
    );
    let (endpoint, token) = client(&args)?;
    info!(
        ?endpoint,
        instance = args.instance,
        view_only = args.view_only,
        takeover = args.takeover,
        "connecting"
    );

    let event_loop = EventLoop::<UiEvent>::with_user_event()
        .build()
        .context("cannot start the window system event loop")?;
    let proxy = event_loop.create_proxy();

    // The decoder thread leaves the newest frame here and wakes the window
    // once; frames that arrive before the window draws replace each other.
    let latest = Arc::new(Mutex::new(None));
    let pending = Arc::new(AtomicBool::new(false));
    let (video_errors, video_error_events) = mpsc::unbounded_channel();
    let counters = Arc::new(VideoCounters::default());
    let video = {
        let latest = latest.clone();
        let pending = pending.clone();
        let proxy = Mutex::new(proxy.clone());
        let superseded = counters.clone();
        Arc::new(Video::new(
            &plan,
            counters,
            move |frame| {
                if latest
                    .lock()
                    .expect("frame lock poisoned")
                    .replace(frame)
                    .is_some()
                {
                    superseded.superseded.fetch_add(1, Ordering::Relaxed);
                }
                if !pending.swap(true, Ordering::AcqRel) {
                    let _ = proxy
                        .lock()
                        .expect("proxy lock poisoned")
                        .send_event(UiEvent::Frame);
                }
            },
            video_errors,
        )?)
    };

    let settings = net::Settings {
        endpoint,
        token,
        instance: args.instance.clone(),
        control: !args.view_only,
        takeover: args.takeover,
        audio: !args.no_audio,
        decoder: args.decoder.clone(),
    };
    let (outbound, outbound_events) = mpsc::unbounded_channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    {
        let video = video.clone();
        let proxy = proxy.clone();
        runtime.spawn(async move {
            let events = {
                let proxy = proxy.clone();
                move |event| {
                    let _ = proxy.send_event(UiEvent::Net(event));
                }
            };
            let result = net::run(settings, video, video_error_events, events, outbound_events)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = proxy.send_event(UiEvent::Closed(result));
        });
    }

    let mut app = App::new(
        Options {
            instance: args.instance,
            control: !args.view_only,
            resize_guest: !args.no_resize_guest,
            clipboard: !args.no_clipboard,
        },
        video,
        latest,
        pending,
        outbound,
    );
    event_loop
        .run_app(&mut app)
        .context("window event loop failed")?;
    let failure = app.failure.take();
    // Dropping the app drops the outbound sender, which closes the stream.
    drop(app);
    runtime.shutdown_timeout(Duration::from_secs(2));
    match failure {
        Some(error) => bail!(error),
        None => Ok(()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("machineemu-viewer: {error:#}");
            ExitCode::FAILURE
        }
    }
}
