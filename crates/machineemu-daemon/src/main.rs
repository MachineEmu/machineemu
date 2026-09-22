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
use machineemu_runtime::{
    Error as RuntimeError, Id, ImageManifest, ManagedProcess, Workspace, load_config,
    resolve_config_path,
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
    /// JSON map of profile ID to a planner-produced launch specification.
    #[arg(long)]
    launch_plans: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    workspace: Arc<Mutex<Workspace>>,
    bearer_token: Arc<str>,
    launch_plans: Arc<BTreeMap<String, LaunchSpec>>,
    running: Arc<Mutex<BTreeMap<String, machineemu_runtime::RunningInstance>>>,
    helpers: Arc<Mutex<BTreeMap<String, ManagedProcess>>>,
    local_unix: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LaunchSpec {
    argv: Vec<String>,
    qmp_socket: PathBuf,
    stdout: Option<PathBuf>,
    stderr: Option<PathBuf>,
    preparation: Option<PreparationSpec>,
    helper_argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PreparationSpec {
    disk_backing: PathBuf,
    backing_format: String,
    disk_size: Option<String>,
    nvram_seed: Option<PathBuf>,
    tpm_seed: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StartInstance {
    operation_id: String,
    run_id: String,
    idempotency_key: String,
    #[serde(default)]
    launch_plan: Option<LaunchSpec>,
}

#[derive(Debug, Deserialize)]
struct CreateSnapshot {
    snapshot_id: String,
}

#[derive(Debug, Deserialize)]
struct CloneSnapshot {
    instance_id: String,
    profile_id: String,
}

#[derive(Debug, Deserialize)]
struct CreateInstance {
    instance_id: String,
    image_id: String,
    profile_id: String,
}

#[derive(Debug, Deserialize)]
struct RegisterImage {
    image_id: String,
    engine_track: String,
    target: String,
    disk_sha256: String,
    firmware_sha256: Option<String>,
    tpm_state_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, Serialize)]
struct InstanceStatus {
    instance: machineemu_runtime::Instance,
    ip: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let (config, config_path) = load_config(args.config.as_deref())?;
    let server = config.server.unwrap_or_default();
    let workspace_path = args
        .workspace
        .or(server.workspace)
        .map(|path| resolve_config_path(config_path.as_deref(), path))
        .ok_or("server.workspace is required")?;
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
    let launch_plans_path = args
        .launch_plans
        .or(server.launch_plans)
        .map(|path| resolve_config_path(config_path.as_deref(), path));
    let workspace = Workspace::open(&workspace_path)?;
    let launch_plans = match launch_plans_path {
        Some(path) => serde_json::from_str(&fs::read_to_string(path)?)?,
        None => BTreeMap::new(),
    };
    let state = AppState {
        workspace: Arc::new(Mutex::new(workspace)),
        bearer_token: Arc::from(bearer_token.clone()),
        launch_plans: Arc::new(launch_plans),
        running: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(Mutex::new(BTreeMap::new())),
        local_unix: unix_socket.is_some(),
    };
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
        serve_unix(app, listener).await?;
    } else {
        if bearer_token.is_empty() {
            return Err("bearer token is required for TCP server mode".into());
        }
        let listener = tokio::net::TcpListener::bind(listen).await?;
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
                    let _ = http1::Builder::new().serve_connection(io, service).await;
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
        .route("/api/v2/images", post(register_image))
        .route("/api/v2/images/:id", get(get_image))
        .route(
            "/api/v2/instances",
            get(list_instances).post(create_instance),
        )
        .route(
            "/api/v2/instances/:id",
            get(get_instance).delete(remove_instance),
        )
        .route("/api/v2/instances/:id/start", post(start_instance))
        .route("/api/v2/instances/:id/stop", post(stop_instance))
        .route("/api/v2/instances/:id/pause", post(pause_instance))
        .route("/api/v2/instances/:id/resume", post(resume_instance))
        .route("/api/v2/instances/:id/reset", post(reset_instance))
        .route("/api/v2/instances/:id/snapshots", post(create_snapshot))
        .route("/api/v2/snapshots/:id", get(get_snapshot))
        .route("/api/v2/snapshots/:id/clone", post(clone_snapshot))
        .route("/api/v2/operations/:id", get(get_operation))
        .route("/api/v2/reconcile", post(reconcile))
        .with_state(state)
}

fn workspace_path(root: &std::path::Path, path: &std::path::Path) -> Result<PathBuf, RuntimeError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(RuntimeError::Process(format!(
            "runtime path is outside workspace: {}",
            path.display()
        )));
    }
    Ok(root.join(path))
}

fn plan_paths(
    root: &std::path::Path,
    plan: &LaunchSpec,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), RuntimeError> {
    if plan.argv.is_empty() {
        return Err(RuntimeError::Process("launch plan argv is empty".into()));
    }
    let qmp = workspace_path(root, &plan.qmp_socket)?;
    let stdout = plan
        .stdout
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    let stderr = plan
        .stderr
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    fs::create_dir_all(qmp.parent().unwrap_or(root)).map_err(|source| RuntimeError::Io {
        path: qmp.parent().unwrap_or(root).to_owned(),
        source,
    })?;
    for path in [stdout.as_deref(), stderr.as_deref()].into_iter().flatten() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
    }
    for argument in plan.argv.iter().skip(1) {
        let candidate = argument
            .strip_prefix("file:")
            .or_else(|| argument.strip_prefix("unix:"))
            .or_else(|| argument.split_once("path=").map(|(_, value)| value))
            .map(|value| value.split(',').next().unwrap_or(value));
        if let Some(candidate) = candidate {
            let path = PathBuf::from(candidate);
            if path.is_absolute() {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
                        path: parent.to_owned(),
                        source,
                    })?;
                }
            }
        }
    }
    for pair in plan.argv.windows(2) {
        if pair[0] == "-pidfile" {
            let path = PathBuf::from(&pair[1]);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| RuntimeError::Io {
                    path: parent.to_owned(),
                    source,
                })?;
            }
        }
    }
    Ok((qmp, stdout, stderr))
}

fn prepare_paths(
    root: &std::path::Path,
    preparation: &PreparationSpec,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), RuntimeError> {
    let backing = workspace_path(root, &preparation.disk_backing)?;
    if !backing.is_file() {
        return Err(RuntimeError::Process(format!(
            "disk backing does not exist: {}",
            backing.display()
        )));
    }
    let nvram = preparation
        .nvram_seed
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    let tpm = preparation
        .tpm_seed
        .as_deref()
        .map(|path| workspace_path(root, path))
        .transpose()?;
    for (kind, path) in [("NVRAM", nvram.as_ref()), ("TPM", tpm.as_ref())] {
        if let Some(path) = path {
            if !path.is_file() {
                return Err(RuntimeError::Process(format!(
                    "{kind} seed does not exist: {}",
                    path.display()
                )));
            }
        }
    }
    Ok((backing, nvram, tpm))
}

async fn start_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<StartInstance>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let input_json = serde_json::to_string(&input)
            .map_err(|error| RuntimeError::Process(error.to_string()))?;
        let instance_id = Id::new("instance", id)?;
        let operation_id = Id::new("operation", input.operation_id)?;
        let run_id = Id::new("run", input.run_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let instance = workspace.instance(&instance_id)?;
        let inline_plan = input.launch_plan.as_ref();
        let stored_plan = state.launch_plans.get(instance.profile_id.as_str());
        let plan = inline_plan.or(stored_plan).ok_or_else(|| {
            RuntimeError::Process(format!(
                "no launch plan for profile {}",
                instance.profile_id.as_str()
            ))
        })?;
        let (qmp, stdout, stderr) = plan_paths(workspace.root(), plan)?;
        if let Some(preparation) = &plan.preparation {
            let (backing, nvram, tpm) = prepare_paths(workspace.root(), preparation)?;
            workspace.prepare_instance_files_sized(
                &instance_id,
                &backing,
                &preparation.backing_format,
                preparation.disk_size.as_deref(),
                nvram.as_deref(),
                tpm.as_deref(),
            )?;
        }
        let mut helper = if let Some(helper_argv) = &plan.helper_argv {
            let helper_id = Id::new("run", format!("{}-helper", run_id.as_str()))?;
            Some(ManagedProcess::spawn(helper_id, helper_argv, None, None)?)
        } else {
            None
        };
        let running = workspace.start_instance(
            operation_id,
            run_id.clone(),
            instance_id.clone(),
            &input.idempotency_key,
            &input_json,
            &plan.argv,
            &qmp,
            stdout.as_deref(),
            stderr.as_deref(),
            std::time::Duration::from_secs(10),
        );
        let running = match running {
            Ok(running) => running,
            Err(error) => {
                if let Some(helper) = helper.as_mut() {
                    let _ = helper.terminate();
                    let _ = helper.wait();
                }
                return Err(error);
            }
        };
        let result = workspace.instance(&instance_id)?;
        drop(workspace);
        state
            .running
            .lock()
            .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
            .insert(instance_id.as_str().into(), running);
        if let Some(helper) = helper {
            state
                .helpers
                .lock()
                .map_err(|_| RuntimeError::Process("helper map lock poisoned".into()))?
                .insert(instance_id.as_str().into(), helper);
        }
        Ok(result)
    })();
    match result {
        Ok(instance) => axum::Json(instance).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn stop_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "stop").await
}

async fn pause_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "pause").await
}

async fn resume_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "resume").await
}

async fn reset_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    lifecycle_action(state, headers, id, "reset").await
}

async fn create_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<CreateSnapshot>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id)?;
        let snapshot_id = Id::new("snapshot", input.snapshot_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.create_instance_snapshot(snapshot_id, instance_id)
    })();
    match result {
        Ok(snapshot) => (StatusCode::CREATED, axum::Json(snapshot)).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn get_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let snapshot_id = Id::new("snapshot", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.snapshot(&snapshot_id)
    })();
    match result {
        Ok(snapshot) => axum::Json(snapshot).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn clone_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(input): axum::Json<CloneSnapshot>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let snapshot_id = Id::new("snapshot", id)?;
        let instance_id = Id::new("instance", input.instance_id)?;
        let profile_id = Id::new("profile", input.profile_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let destination = workspace
            .root()
            .join("instances")
            .join(instance_id.as_str());
        workspace.clone_snapshot(&snapshot_id, instance_id, profile_id, &destination)
    })();
    match result {
        Ok(instance) => (StatusCode::CREATED, axum::Json(instance)).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn lifecycle_action(
    state: AppState,
    headers: HeaderMap,
    id: String,
    action: &str,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id.clone())?;
        let mut running = match state
            .running
            .lock()
            .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
            .remove(&id)
        {
            Some(running) => running,
            None if action == "stop" => {
                let workspace = state
                    .workspace
                    .lock()
                    .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
                let instance = workspace.instance(&Id::new("instance", id.clone())?)?;
                if instance.state == "stopped" || instance.state == "error" {
                    return Ok(instance);
                }
                return Err(RuntimeError::Process(
                    "instance has no daemon-owned run".into(),
                ));
            }
            None => {
                return Err(RuntimeError::Process(
                    "instance has no daemon-owned run".into(),
                ));
            }
        };
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let instance = match action {
            "stop" => workspace.stop_instance(&instance_id, &mut running),
            "pause" => workspace.pause_instance(&instance_id, &mut running),
            "resume" => workspace.resume_instance(&instance_id, &mut running),
            "reset" => workspace
                .reset_instance(&instance_id, &mut running)
                .map(|_| workspace.instance(&instance_id))
                .and_then(|result| result),
            _ => Err(RuntimeError::Process("unknown lifecycle action".into())),
        };
        if action == "stop" {
            if let Some(mut helper) = state
                .helpers
                .lock()
                .map_err(|_| RuntimeError::Process("helper map lock poisoned".into()))?
                .remove(&id)
            {
                let _ = helper.terminate();
                let _ = helper.wait();
            }
        }
        if action != "stop" {
            drop(workspace);
            state
                .running
                .lock()
                .map_err(|_| RuntimeError::Process("running map lock poisoned".into()))?
                .insert(id, running);
        }
        instance
    })();
    match result {
        Ok(instance) => axum::Json(instance).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn register_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(input): axum::Json<RegisterImage>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let manifest = ImageManifest {
            image_id: Id::new("image", input.image_id)?,
            engine_track: Id::new("engine track", input.engine_track)?,
            target: input.target,
            disk_sha256: input.disk_sha256,
            firmware_sha256: input.firmware_sha256,
            tpm_state_sha256: input.tpm_state_sha256,
        };
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let digest = workspace.register_image(&manifest)?;
        Ok((manifest, digest))
    })();
    match result {
        Ok((manifest, digest)) => (
            StatusCode::CREATED,
            axum::Json(serde_json::json!({
                "manifest": manifest,
                "manifest_sha256": digest
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn get_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let image_id = Id::new("image", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.image(&image_id)
    })();
    match result {
        Ok(image) => axum::Json(image).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

fn authorized(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<(), (StatusCode, axum::Json<ErrorBody>)> {
    if state.local_unix {
        return Ok(());
    }
    let expected = format!("Bearer {}", state.bearer_token);
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(expected.as_str())
    {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(ErrorBody {
                error: "unauthorized".into(),
            }),
        ))
    }
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    axum::Json(serde_json::json!({"ok": true, "api": "v2"})).into_response()
}

async fn create_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(input): axum::Json<CreateInstance>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", input.instance_id)?;
        let image_id = Id::new("image", input.image_id)?;
        let profile_id = Id::new("profile", input.profile_id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.create_instance(instance_id, image_id, profile_id)
    })();
    match result {
        Ok(instance) => (StatusCode::CREATED, axum::Json(instance)).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn list_instances(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = state
        .workspace
        .lock()
        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))
        .and_then(|workspace| {
            let instances = workspace.instances()?;
            Ok(instances
                .into_iter()
                .map(|instance| {
                    let socket = workspace
                        .root()
                        .join("instances")
                        .join(instance.instance_id.as_str())
                        .join("qga.sock");
                    let ip = if socket.exists() {
                        #[cfg(unix)]
                        {
                            machineemu_runtime::guest_ipv4(&socket).ok().flatten()
                        }
                        #[cfg(not(unix))]
                        {
                            None
                        }
                    } else {
                        None
                    };
                    InstanceStatus { instance, ip }
                })
                .collect::<Vec<_>>())
        });
    match result {
        Ok(instances) => axum::Json(instances).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn get_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.instance(&instance_id)
    })();
    match result {
        Ok(instance) => axum::Json(instance).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn remove_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let instance_id = Id::new("instance", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.remove_instance(&instance_id)
    })();
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(RuntimeError::NotFound { .. }) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn get_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = (|| -> Result<_, RuntimeError> {
        let operation_id = Id::new("operation", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.operation(&operation_id)
    })();
    match result {
        Ok(operation) => axum::Json(operation).into_response(),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn reconcile(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = state
        .workspace
        .lock()
        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))
        .and_then(|workspace| workspace.reconcile_active_runs());
    match result {
        Ok(runs) => axum::Json(runs).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn bearer_auth_requires_exact_token() {
        let root =
            std::env::temp_dir().join(format!("machineemu-daemon-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = AppState {
            workspace: Arc::new(Mutex::new(Workspace::open(&root).unwrap())),
            bearer_token: Arc::from("secret"),
            launch_plans: Arc::new(BTreeMap::new()),
            running: Arc::new(Mutex::new(BTreeMap::new())),
            helpers: Arc::new(Mutex::new(BTreeMap::new())),
            local_unix: false,
        };
        let missing = HeaderMap::new();
        assert!(authorized(&missing, &state).is_err());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(authorized(&headers, &state).is_ok());
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn api_routes_require_bearer_authentication() {
        let root =
            std::env::temp_dir().join(format!("machineemu-daemon-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = AppState {
            workspace: Arc::new(Mutex::new(Workspace::open(&root).unwrap())),
            bearer_token: Arc::from("secret"),
            launch_plans: Arc::new(BTreeMap::new()),
            running: Arc::new(Mutex::new(BTreeMap::new())),
            helpers: Arc::new(Mutex::new(BTreeMap::new())),
            local_unix: false,
        };
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v2/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v2/health")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let image = serde_json::json!({
            "image_id": "debian13-cloud",
            "engine_track": "unifi-10-2",
            "target": "x86_64-softmmu",
            "disk_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "firmware_sha256": null,
            "tpm_state_sha256": null
        });
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/images")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&image).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let instance = serde_json::json!({
            "instance_id": "lab01",
            "image_id": "debian13-cloud",
            "profile_id": "debian13-cloud"
        });
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/instances")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&instance).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v2/instances/lab01")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = serde_json::json!({"snapshot_id": "snap01"});
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/instances/lab01/snapshots")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&snapshot).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v2/snapshots/snap01")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let clone = serde_json::json!({
            "instance_id": "lab02",
            "profile_id": "debian13-cloud"
        });
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/snapshots/snap01/clone")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&clone).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let start = serde_json::json!({
            "operation_id": "op01",
            "run_id": "run01",
            "idempotency_key": "start-01"
        });
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/instances/lab01/start")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&start).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }
}
