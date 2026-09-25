use super::*;
use axum::response::{
    Response,
    sse::{Event, KeepAlive, Sse},
};
use futures_util::stream;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path as FsPath, PathBuf},
    sync::mpsc as std_mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

const IMPORT_SYNC_INTERVAL_BYTES: u64 = 512 * 1024 * 1024;

fn image_components_from_digests(
    disk: Option<&String>,
    firmware: Option<&String>,
    tpm_state: Option<&String>,
) -> BTreeMap<String, machineemu_core::domain::ImageBundleComponent> {
    let mut components = BTreeMap::new();
    let mut insert = |name: &str, path: &str, digest: Option<&String>| {
        if let Some(digest) = digest {
            components.insert(
                name.to_owned(),
                machineemu_core::domain::ImageBundleComponent {
                    path: path.to_owned(),
                    sha256: format!(
                        "sha256:{}",
                        digest.strip_prefix("sha256:").unwrap_or(digest)
                    ),
                },
            );
        }
    };
    insert("disk", "components/disk.qcow2", disk);
    insert("firmware", "components/firmware.fd", firmware);
    insert("tpm_state", "components/tpm-state", tpm_state);
    components
}

#[derive(Clone)]
pub(super) struct ImageImportEvent {
    sequence: u64,
    kind: &'static str,
    data: Value,
}

impl ImageImportEvent {
    fn sse(self, import_id: &str) -> Event {
        Event::default()
            .event(self.kind)
            .id(format!("{import_id}:{}", self.sequence))
            .data(self.data.to_string())
    }
}

pub(super) struct ImageImportJob {
    status: String,
    phase: String,
    source: PathBuf,
    image_id: String,
    engine_track: String,
    target: String,
    current_component: Option<String>,
    bytes_done: u64,
    bytes_total: u64,
    manifest: Option<Value>,
    error: Option<String>,
    next_sequence: u64,
    next_subscriber: u64,
    events: VecDeque<ImageImportEvent>,
    subscribers: BTreeMap<u64, mpsc::Sender<ImageImportEvent>>,
}

impl ImageImportJob {
    pub(super) fn engine(source: PathBuf) -> Self {
        Self::new(&ImportVmmanagerBase {
            source: source.to_string_lossy().into_owned(),
            image_id: String::new(),
            engine_track: String::new(),
            target: String::new(),
        })
    }
    fn new(input: &ImportVmmanagerBase) -> Self {
        Self {
            status: "queued".into(),
            phase: "queued".into(),
            source: PathBuf::from(&input.source),
            image_id: input.image_id.clone(),
            engine_track: input.engine_track.clone(),
            target: input.target.clone(),
            current_component: None,
            bytes_done: 0,
            bytes_total: 0,
            manifest: None,
            error: None,
            next_sequence: 0,
            next_subscriber: 0,
            events: VecDeque::new(),
            subscribers: BTreeMap::new(),
        }
    }

    fn status_json(&self, import_id: &str) -> Value {
        json!({
            "import_id": import_id,
            "status": self.status,
            "phase": self.phase,
            "source": self.source,
            "image_id": self.image_id,
            "engine_track": self.engine_track,
            "target": self.target,
            "current_component": self.current_component,
            "bytes_done": self.bytes_done,
            "bytes_total": self.bytes_total,
            "manifest": self.manifest,
            "error": self.error
        })
    }
}

pub(super) fn image_import_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("imgimp-{}-{now:x}", std::process::id())
}

pub(super) fn publish_import_event(
    state: &AppState,
    import_id: &str,
    kind: &'static str,
    data: Value,
) -> Result<(), RuntimeError> {
    let mut imports = state
        .image_imports
        .lock()
        .map_err(|_| RuntimeError::Process("image import lock poisoned".into()))?;
    let Some(job) = imports.get_mut(import_id) else {
        return Ok(());
    };
    job.next_sequence += 1;
    let event = ImageImportEvent {
        sequence: job.next_sequence,
        kind,
        data,
    };
    job.subscribers
        .retain(|_, sender| sender.try_send(event.clone()).is_ok());
    job.events.push_back(event);
    while job.events.len() > 512 {
        job.events.pop_front();
    }
    Ok(())
}

pub(super) fn set_import_status(
    state: &AppState,
    import_id: &str,
    status: &str,
    phase: &str,
    current_component: Option<String>,
    bytes_done: u64,
    bytes_total: u64,
) -> Result<(), RuntimeError> {
    let mut imports = state
        .image_imports
        .lock()
        .map_err(|_| RuntimeError::Process("image import lock poisoned".into()))?;
    if let Some(job) = imports.get_mut(import_id) {
        job.status = status.into();
        job.phase = phase.into();
        job.current_component = current_component;
        job.bytes_done = bytes_done;
        job.bytes_total = bytes_total;
    }
    Ok(())
}

pub(super) fn finish_import(
    state: &AppState,
    import_id: &str,
    manifest: impl Serialize,
) -> Result<(), RuntimeError> {
    let mut imports = state
        .image_imports
        .lock()
        .map_err(|_| RuntimeError::Process("image import lock poisoned".into()))?;
    if let Some(job) = imports.get_mut(import_id) {
        job.status = "complete".into();
        job.phase = "complete".into();
        job.current_component = None;
        job.bytes_done = job.bytes_total;
        job.manifest = Some(
            serde_json::to_value(manifest)
                .map_err(|error| RuntimeError::Process(error.to_string()))?,
        );
    }
    Ok(())
}

pub(super) fn fail_import(state: &AppState, import_id: &str, error: String) {
    if let Ok(mut imports) = state.image_imports.lock()
        && let Some(job) = imports.get_mut(import_id)
    {
        job.status = "failed".into();
        job.phase = "failed".into();
        job.error = Some(error);
        job.current_component = None;
    }
}
pub(super) async fn register_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(input): axum::Json<RegisterImage>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let disk_sha256 = input.disk_sha256;
        let firmware_sha256 = input.firmware_sha256;
        let tpm_state_sha256 = input.tpm_state_sha256;
        let components = image_components_from_digests(
            Some(&disk_sha256),
            firmware_sha256.as_ref(),
            tpm_state_sha256.as_ref(),
        );
        let manifest = ImageManifest {
            image_id: Id::new("image", input.image_id)?,
            engine_track: Id::new("engine track", input.engine_track)?,
            supported_engine_tracks: input
                .supported_engine_tracks
                .into_iter()
                .map(|track| Id::new("engine track", track))
                .collect::<Result<_, _>>()?,
            target: input.target,
            components,
            disk_sha256,
            firmware_sha256,
            tpm_state_sha256,
        };
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let digest = workspace.register_image(&manifest)?;
        Ok((manifest, digest))
    })
    .await;
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

pub(super) async fn start_vmmanager_base_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(input): axum::Json<ImportVmmanagerBase>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    if let Err(error) = Id::new("image", input.image_id.clone())
        .and_then(|_| Id::new("engine track", input.engine_track.clone()).map(|_| ()))
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response();
    }
    let import_id = image_import_id();
    {
        let mut imports = match state.image_imports.lock() {
            Ok(imports) => imports,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(ErrorBody {
                        error: "image import lock poisoned".into(),
                    }),
                )
                    .into_response();
            }
        };
        imports.insert(import_id.clone(), ImageImportJob::new(&input));
    }
    let worker_state = state.clone();
    let worker_import_id = import_id.clone();
    tokio::spawn(async move {
        let result = blocking({
            let worker_state = worker_state.clone();
            let worker_import_id = worker_import_id.clone();
            move || import_vmmanager_base_job(worker_state, worker_import_id, input)
        })
        .await;
        if let Err(error) = result {
            fail_import(&worker_state, &worker_import_id, error.to_string());
            let _ = publish_import_event(
                &worker_state,
                &worker_import_id,
                "failed",
                json!({"error": error.to_string()}),
            );
        }
    });
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({
            "import_id": import_id,
            "status_url": format!("/api/v2/image-imports/{import_id}"),
            "events_url": format!("/api/v2/image-imports/{import_id}/events")
        })),
    )
        .into_response()
}

pub(super) async fn get_image_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let imports = match state.image_imports.lock() {
        Ok(imports) => imports,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorBody {
                    error: "image import lock poisoned".into(),
                }),
            )
                .into_response();
        }
    };
    match imports.get(&id) {
        Some(job) => axum::Json(job.status_json(&id)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: "image import not found".into(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn stream_image_import_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(':').next())
        .and_then(|value| value.parse::<u64>().ok());
    let prepared = {
        let mut imports = match state.image_imports.lock() {
            Ok(imports) => imports,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let Some(job) = imports.get_mut(&id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let replay = match cursor {
            Some(sequence) => job
                .events
                .iter()
                .filter(|event| event.sequence > sequence)
                .cloned()
                .collect::<VecDeque<_>>(),
            None => {
                let mut initial = VecDeque::new();
                job.next_sequence += 1;
                initial.push_back(ImageImportEvent {
                    sequence: job.next_sequence,
                    kind: "snapshot",
                    data: job.status_json(&id),
                });
                initial
            }
        };
        let (sender, receiver) = mpsc::channel(64);
        job.next_subscriber += 1;
        let subscriber = job.next_subscriber;
        job.subscribers.insert(subscriber, sender);
        (replay, receiver, subscriber)
    };
    let (initial, receiver, subscriber) = prepared;
    let stream = stream::unfold(
        (
            id.clone(),
            state.image_imports.clone(),
            initial,
            subscriber,
            receiver,
        ),
        move |(id, imports, mut initial, subscriber, mut receiver)| async move {
            if let Some(event) = initial.pop_front() {
                return Some((
                    Ok::<_, std::convert::Infallible>(event.sse(&id)),
                    (id, imports, initial, subscriber, receiver),
                ));
            }
            let event = receiver.recv().await;
            match event {
                Some(event) => Some((
                    Ok::<_, std::convert::Infallible>(event.sse(&id)),
                    (id, imports, initial, subscriber, receiver),
                )),
                None => {
                    if let Ok(mut imports) = imports.lock()
                        && let Some(job) = imports.get_mut(&id)
                    {
                        job.subscribers.remove(&subscriber);
                    }
                    None
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn import_vmmanager_base_job(
    state: AppState,
    import_id: String,
    input: ImportVmmanagerBase,
) -> Result<(), RuntimeError> {
    let source = PathBuf::from(&input.source);
    let disk = source.join("disk.qcow2");
    let firmware = source.join("OVMF_VARS.fd");
    let tpm = source.join("tpm/tpm2-00.permall");
    if !disk.is_file() {
        return Err(RuntimeError::InvalidBundlePath(disk.display().to_string()));
    }
    let mut components = vec![("disk", disk, "disk.qcow2")];
    if firmware.is_file() {
        components.push(("firmware", firmware, "firmware.fd"));
    }
    if tpm.is_file() {
        components.push(("tpm_state", tpm, "tpm-state"));
    }
    let total = components
        .iter()
        .map(|(_, path, _)| path.metadata().map(|m| m.len()).unwrap_or(0))
        .sum::<u64>();
    set_import_status(&state, &import_id, "running", "queued", None, 0, total)?;
    publish_import_event(
        &state,
        &import_id,
        "queued",
        json!({"status":"running","phase":"queued","bytes_done":0,"bytes_total":total}),
    )?;
    let image_id = Id::new("image", input.image_id)?;
    let engine_track = Id::new("engine track", input.engine_track)?;
    let workspace = state
        .workspace
        .lock()
        .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
    let image_root = workspace
        .root()
        .join("images")
        .join(image_id.as_str())
        .join("components");
    fs::create_dir_all(&image_root).map_err(|source| RuntimeError::Io {
        path: image_root.clone(),
        source,
    })?;
    let mut done = 0;
    let mut disk_sha256 = None;
    let mut firmware_sha256 = None;
    let mut tpm_state_sha256 = None;
    for (component, source, destination_name) in components {
        let size = source.metadata().map(|m| m.len()).unwrap_or(0);
        publish_import_event(
            &state,
            &import_id,
            "component-start",
            json!({"component":component,"phase":"copying+hashing","bytes_done":done,"bytes_total":total,"component_bytes":size}),
        )?;
        set_import_status(
            &state,
            &import_id,
            "running",
            "copying+hashing",
            Some(component.into()),
            done,
            total,
        )?;
        let digest = copy_component_with_progress(
            &source,
            &image_root.join(destination_name),
            |update| {
                let aggregate = done.saturating_add(update.bytes_done);
                let _ = set_import_status(
                    &state,
                    &import_id,
                    "running",
                    update.phase,
                    Some(component.into()),
                    aggregate,
                    total,
                );
                let _ = publish_import_event(
                    &state,
                    &import_id,
                    "progress",
                    json!({"component":component,"phase":update.phase,"bytes_done":aggregate,"bytes_total":total,"component_done":update.bytes_done,"component_bytes":size}),
                );
            },
        )?;
        done = done.saturating_add(size);
        publish_import_event(
            &state,
            &import_id,
            "component-complete",
            json!({"component":component,"phase":"verified","sha256":digest,"bytes_done":done,"bytes_total":total}),
        )?;
        match component {
            "disk" => disk_sha256 = Some(digest),
            "firmware" => firmware_sha256 = Some(digest),
            "tpm_state" => tpm_state_sha256 = Some(digest),
            _ => {}
        }
    }
    let disk_sha256 = disk_sha256.expect("disk component is required");
    let components = image_components_from_digests(
        Some(&disk_sha256),
        firmware_sha256.as_ref(),
        tpm_state_sha256.as_ref(),
    );
    let manifest = ImageManifest {
        image_id,
        engine_track,
        supported_engine_tracks: Vec::new(),
        target: input.target,
        components,
        disk_sha256,
        firmware_sha256,
        tpm_state_sha256,
    };
    set_import_status(
        &state,
        &import_id,
        "registering",
        "registering",
        None,
        total,
        total,
    )?;
    publish_import_event(
        &state,
        &import_id,
        "progress",
        json!({"phase":"registering","bytes_done":total,"bytes_total":total}),
    )?;
    workspace.register_image(&manifest)?;
    finish_import(&state, &import_id, manifest.clone())?;
    publish_import_event(
        &state,
        &import_id,
        "complete",
        json!({"status":"complete","phase":"complete","bytes_done":total,"bytes_total":total,"manifest":manifest}),
    )?;
    Ok(())
}

struct ComponentCopyUpdate {
    phase: &'static str,
    bytes_done: u64,
}

fn copy_component_with_progress(
    source: &FsPath,
    destination: &FsPath,
    mut progress: impl FnMut(ComponentCopyUpdate),
) -> Result<String, RuntimeError> {
    let temporary = destination.with_file_name(format!(
        ".{}.{}.importing",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("component"),
        std::process::id()
    ));
    let mut input = File::open(source).map_err(|source_error| RuntimeError::Io {
        path: source.to_owned(),
        source: source_error,
    })?;
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|source_error| RuntimeError::Io {
            path: temporary.clone(),
            source: source_error,
        })?;
    let sync_file = output
        .try_clone()
        .map_err(|source_error| RuntimeError::Io {
            path: temporary.clone(),
            source: source_error,
        })?;
    let (sync_tx, sync_rx) = std_mpsc::sync_channel::<u64>(1);
    let sync_path = temporary.clone();
    let sync_worker = std::thread::spawn(move || -> Result<(), std::io::Error> {
        while sync_rx.recv().is_ok() {
            while sync_rx.try_recv().is_ok() {}
            sync_file.sync_data()?;
        }
        Ok(())
    });
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut last_sync = 0u64;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|source_error| RuntimeError::Io {
                path: source.to_owned(),
                source: source_error,
            })?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|source_error| RuntimeError::Io {
                path: temporary.clone(),
                source: source_error,
            })?;
        hasher.update(&buffer[..count]);
        copied = copied.saturating_add(count as u64);
        progress(ComponentCopyUpdate {
            phase: "copying+hashing",
            bytes_done: copied,
        });
        if copied.saturating_sub(last_sync) >= IMPORT_SYNC_INTERVAL_BYTES {
            progress(ComponentCopyUpdate {
                phase: "sync queued",
                bytes_done: copied,
            });
            let _ = sync_tx.try_send(copied);
            last_sync = copied;
        }
    }
    drop(sync_tx);
    progress(ComponentCopyUpdate {
        phase: "syncing",
        bytes_done: copied,
    });
    match sync_worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(source)) => {
            return Err(RuntimeError::Io {
                path: sync_path,
                source,
            });
        }
        Err(_) => {
            return Err(RuntimeError::Process(
                "background image import sync worker panicked".into(),
            ));
        }
    }
    output.sync_all().map_err(|source_error| RuntimeError::Io {
        path: temporary.clone(),
        source: source_error,
    })?;
    progress(ComponentCopyUpdate {
        phase: "publishing",
        bytes_done: copied,
    });
    fs::rename(&temporary, destination).map_err(|source_error| RuntimeError::Io {
        path: destination.to_owned(),
        source: source_error,
    })?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub(super) async fn list_images(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let root = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .root()
            .to_owned();
        Workspace::list_images(root)
    })
    .await;
    match result {
        Ok(images) => documents::render_document(&headers, images),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}

pub(super) async fn get_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<_, RuntimeError> {
        let image_id = Id::new("image", id)?;
        let workspace = state
            .workspace
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        workspace.image(&image_id)
    })
    .await;
    match result {
        Ok(image) => documents::render_document(&headers, image),
        Err(error) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
        )
            .into_response(),
    }
}
