use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::{collections::BTreeMap, fs};

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use machineemu_core::{
    Error as RuntimeError,
    config::{load_config, resolve_config_path},
    domain::{Id, ImageManifest},
    runtime::ManagedProcess,
    storage::Workspace,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(name = "machineemu-daemon", about = "MachineEmu Rust workspace daemon")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    listen: Option<SocketAddr>,
    #[arg(long)]
    unix_socket: Option<PathBuf>,
    #[arg(long, env = "MACHINEEMU_BEARER_TOKEN")]
    bearer_token: Option<String>,
    /// Path to the display-stream encoder.
    #[arg(long)]
    display_stream: Option<PathBuf>,
    /// Print the generated API v2 OpenAPI document and exit.
    #[arg(long)]
    print_openapi: bool,
}

#[derive(Clone)]
struct AppState {
    workspace: Arc<Mutex<Workspace>>,
    bearer_token: Arc<str>,
    supervisors: Arc<Mutex<BTreeMap<String, supervisor::RunSupervisor>>>,
    instance_locks: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    display_stream: Arc<PathBuf>,
    stream_tickets: Arc<Mutex<BTreeMap<String, streams::StreamTicket>>>,
    audio_sessions: Arc<Mutex<BTreeMap<String, streams::AudioSession>>>,
    control_streams: Arc<Mutex<BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
    events: Arc<Mutex<events::EventHub>>,
    guest_executions: Arc<Mutex<BTreeMap<String, Arc<guest_exec::ExecutionJob>>>>,
    local_unix: bool,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, RuntimeError> + Send + 'static,
) -> Result<T, RuntimeError> {
    static LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);
    let _permit = LIMIT
        .acquire()
        .await
        .map_err(|error| RuntimeError::Process(format!("blocking worker unavailable: {error}")))?;
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RuntimeError::Process(format!("blocking task failed: {error}")))?
}

fn instance_lock(state: &AppState, id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, RuntimeError> {
    Ok(state
        .instance_locks
        .lock()
        .map_err(|_| RuntimeError::Process("instance lock map poisoned".into()))?
        .entry(id.to_owned())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone())
}

/// Log to stderr, filtered by `RUST_LOG` (for example
/// `RUST_LOG=machineemu=debug`). The default shows this crate's
/// informational messages and warnings from dependencies.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,machineemu=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Log every HTTP request by method, path and status. The query string is
/// left out: WebSocket stream tickets travel in it.
async fn log_request(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if status >= 400 {
        tracing::info!(%method, path, status, elapsed_ms, "request");
    } else {
        tracing::debug!(%method, path, status, elapsed_ms, "request");
    }
    response
}

pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.print_openapi {
        println!("{}", openapi::document().to_pretty_json()?);
        return Ok(());
    }
    init_tracing();
    let (config, config_path) = load_config(args.config.as_deref())?;
    let server = config.server.unwrap_or_default();
    let workspace_path = args
        .workspace
        .or(server.workspace)
        .map(|path| resolve_config_path(config_path.as_deref(), path))
        .ok_or("server.workspace is required")?;
    let workspace_path = workspace_path.canonicalize().unwrap_or(workspace_path);
    let unix_socket = args
        .unix_socket
        .or(server.unix_socket)
        .map(|path| resolve_config_path(config_path.as_deref(), path));
    let listen = args
        .listen
        .or_else(|| {
            server
                .listen
                .as_deref()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or_else(|| "127.0.0.1:8787".parse().expect("valid default address"));
    let bearer_token = args
        .bearer_token
        .or(server.bearer_token)
        .unwrap_or_default();
    let mut workspace = Workspace::open(&workspace_path)?;
    let recovered_runs = workspace.reconcile_active_runs()?;
    for run in recovered_runs.iter().filter(|run| run.status == "running") {
        // Query before reconciling operations, so interrupted starts see observed state.
        // A temporarily unavailable QMP socket must not mark a live VM as dead.
        if let Err(error) = workspace.recover_instance_run_async(&run.instance_id).await {
            eprintln!(
                "QMP recovery for {} will retry: {error}",
                run.instance_id.as_str()
            );
        }
    }
    workspace.reconcile_operations()?;
    let state = AppState {
        workspace: Arc::new(Mutex::new(workspace)),
        bearer_token: Arc::from(bearer_token.clone()),
        supervisors: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),

        display_stream: Arc::new(
            args.display_stream
                .unwrap_or_else(|| PathBuf::from("display-stream")),
        ),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new()?)),

        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        local_unix: unix_socket.is_some(),
    };
    tracing::info!(
        workspace = %workspace_path.display(),
        recovered_runs = recovered_runs.len(),
        display_stream = %state.display_stream.display(),
        "daemon starting"
    );
    for run in recovered_runs
        .into_iter()
        .filter(|run| run.status == "running")
    {
        let helpers = state
            .workspace
            .lock()
            .map_err(|_| "workspace lock poisoned")?
            .recover_run_helpers(&run.run_id)?;
        supervisor::install(
            &state,
            run.instance_id.as_str(),
            run.run_id.clone(),
            None,
            helpers,
        )?;
        run_events::spawn(&state, run);
    }
    let app = router(state);
    if let Some(socket) = unix_socket {
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent)?;
        }
        if socket.exists() {
            fs::remove_file(&socket)?;
        }
        let listener = tokio::net::UnixListener::bind(&socket)?;
        #[cfg(unix)]
        std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        tracing::info!(socket = %socket.display(), "listening on Unix socket");
        let served = serve_unix(app, listener).await;
        // The socket belongs to this daemon. Left behind, the next start finds
        // a path that exists but nothing listening on it, and a client sees
        // "connection refused" against what looks like a live daemon.
        if let Err(error) = fs::remove_file(&socket) {
            tracing::warn!(socket = %socket.display(), %error, "could not remove socket");
        }
        served?;
    } else {
        if bearer_token.is_empty() {
            return Err("bearer token is required for TCP server mode".into());
        }
        let listener = tokio::net::TcpListener::bind(listen).await?;
        tracing::info!(%listen, "listening on TCP");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await?;
    }
    Ok(())
}

async fn serve_unix(
    app: Router,
    listener: tokio::net::UnixListener,
) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = shutdown();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let io = TokioIo::new(stream);
                let service = TowerToHyperService::new(app.clone().into_service());
                tokio::spawn(async move {
                    // with_upgrades keeps the connection alive for WebSockets.
                    if let Err(error) = http1::Builder::new().serve_connection(io, service).with_upgrades().await {
                        tracing::debug!(%error, "Unix socket connection ended with an error");
                    }
                });
            }
            _ = &mut shutdown => break,
        }
    }
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v2/health", get(health))
        .route("/api/v2/openapi.json", get(openapi_document))
        .route("/api/v2/images", post(register_image))
        .route(
            "/api/v2/images/:id",
            get(get_image).put(documents::put_image),
        )
        .route(
            "/api/v2/profiles/:id",
            get(documents::get_profile).put(documents::put_profile),
        )
        .route(
            "/api/v2/instances",
            get(list_instances).post(create_instance),
        )
        .route(
            "/api/v2/instances/:id",
            get(get_instance).delete(remove_instance),
        )
        .route(
            "/api/v2/instances/:id/config",
            get(documents::get_instance_config).put(documents::put_instance_config),
        )
        .route("/api/v2/instances/:id/start", post(start_instance))
        .route(
            "/api/v2/instances/:id/tombstone",
            get(instances::get_instance_tombstone),
        )
        .route("/api/v2/instances/:id/stop", post(stop_instance))
        .route("/api/v2/instances/:id/restart", post(restart_instance))
        .route(
            "/api/v2/instances/:id/send-key",
            post(display_control::send_key),
        )
        .route(
            "/api/v2/instances/:id/screenshot",
            post(display_control::screenshot),
        )
        .route("/api/v2/instances/:id/pause", post(pause_instance))
        .route("/api/v2/instances/:id/resume", post(resume_instance))
        .route("/api/v2/instances/:id/reset", post(reset_instance))
        .route("/api/v2/instances/:id/events", get(events::stream_events))
        .route(
            "/api/v2/instances/:id/guest-agent",
            get(guest_agent::get_guest_agent_information),
        )
        .route(
            "/api/v2/instances/:id/guest-executions",
            post(guest_exec::start_execution),
        )
        .route(
            "/api/v2/guest-executions/:id/events",
            get(guest_exec::stream_execution),
        )
        .route("/api/v2/instances/:id/snapshots", post(create_snapshot))
        .route("/api/v2/snapshots/:id", get(get_snapshot))
        .route("/api/v2/snapshots/:id/clone", post(clone_snapshot))
        .route("/api/v2/operations/:id", get(get_operation))
        .route("/api/v2/reconcile", post(reconcile))
        .route(
            "/api/v2/instances/:id/streams/:kind/ticket",
            post(issue_stream_ticket),
        )
        .route(
            "/api/v2/instances/:id/audio/spice/tickets",
            post(issue_spice_tickets),
        )
        .route("/ws/v2/instances/:id/:kind", get(connect_stream))
        .route(
            "/api/v2/instances/:id/helpers/:kind",
            get(helper_control::status).post(helper_control::action),
        )
        .route(
            "/api/v2/instances/:id/devices/:kind",
            get(list_devices).post(attach_device),
        )
        .route(
            "/api/v2/instances/:id/devices/:kind/:device_id",
            axum::routing::delete(detach_device),
        )
        .route(
            "/api/v2/instances/:id/devices/iso/:device_id/change",
            post(change_iso),
        )
        .route(
            "/api/v2/instances/:id/devices/iso/:device_id/eject",
            post(eject_iso),
        )
        .layer(axum::middleware::from_fn(log_request))
        .with_state(state)
}

/// Wait for the signals a supervisor or an operator actually sends. SIGINT
/// alone left `pkill` and systemd stopping the process outright, with no
/// chance to remove the socket it created.
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

mod auth;
mod devices;
mod display_control;
mod documents;
mod dto;
mod events;
mod guest_agent;
mod guest_exec;
mod helper_control;
mod helpers;
mod images;
mod instances;
mod launch;
mod lifecycle;
mod openapi;
mod operations;
mod run_events;
mod snapshots;
mod spice_audio;
mod streams;
mod supervisor;
use auth::*;
use devices::*;
use dto::*;
use images::*;
use instances::*;
use lifecycle::*;
use operations::*;
use snapshots::*;
use streams::*;
#[cfg(test)]
mod tests;

async fn health(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    axum::Json(serde_json::json!({"ok": true, "api": "v2"})).into_response()
}

async fn openapi_document(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    axum::Json(openapi::document()).into_response()
}
