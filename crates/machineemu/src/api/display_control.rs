use super::*;
use axum::{Json, body::Body, http::header};
use machineemu_core::protocols::async_qmp::AsyncQmp;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SCREENSHOT: AtomicU64 = AtomicU64::new(1);

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SendKey {
    keys: Vec<String>,
    #[serde(default)]
    hold_time_ms: Option<u32>,
}

async fn qmp_session(
    state: &AppState,
    id: String,
) -> Result<(tokio::sync::OwnedMutexGuard<()>, AsyncQmp, PathBuf), RuntimeError> {
    let instance_id = Id::new("instance", id.clone())?;
    let guard = instance_lock(state, &id)?.lock_owned().await;
    let owner = state.workspace.clone();
    let (socket, directory) = blocking(move || {
        let workspace = owner
            .lock()
            .map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?;
        let run = workspace
            .live_run(&instance_id)?
            .ok_or_else(|| RuntimeError::Process("instance has no live run".into()))?;
        Ok((
            run.qmp_socket,
            workspace
                .root()
                .join("instances")
                .join(instance_id.as_str()),
        ))
    })
    .await?;
    Ok((guard, AsyncQmp::connect(&socket).await?, directory))
}

fn failure(error: RuntimeError) -> axum::response::Response {
    (
        StatusCode::CONFLICT,
        Json(ErrorBody {
            error: error.to_string(),
        }),
    )
        .into_response()
}

pub(super) async fn send_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<SendKey>,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    if input.keys.is_empty()
        || input.keys.len() > 16
        || input.keys.iter().any(|key| {
            key.is_empty()
                || key.len() > 32
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
        || input.hold_time_ms.is_some_and(|time| time > 5000)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "keys must contain 1–16 QEMU qcodes; hold_time_ms must be at most 5000"
                    .into(),
            }),
        )
            .into_response();
    }
    let result = async {
        let (_guard, mut qmp, _) = qmp_session(&state, id).await?;
        let keys = input
            .keys
            .iter()
            .map(|key| json!({"type":"qcode","data":key}))
            .collect::<Vec<_>>();
        let mut arguments = json!({"keys":keys});
        if let Some(time) = input.hold_time_ms {
            arguments["hold-time"] = json!(time);
        }
        qmp.execute("send-key", arguments).await?;
        Ok::<_, RuntimeError>(())
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(error),
    }
}

pub(super) async fn screenshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = async {
        let (_guard, mut qmp, directory) = qmp_session(&state, id).await?;
        let name = format!(
            "screenshot-{}-{}.png",
            std::process::id(),
            NEXT_SCREENSHOT.fetch_add(1, Ordering::Relaxed)
        );
        let path = directory.join(name);
        let result = async {
            qmp.execute(
                "screendump",
                json!({"filename":path.to_string_lossy(),"format":"png"}),
            )
            .await?;
            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|source| RuntimeError::Io {
                    path: path.clone(),
                    source,
                })?;
            if metadata.len() > 16 * 1024 * 1024 {
                return Err(RuntimeError::Process("screenshot exceeds 16 MiB".into()));
            }
            tokio::fs::read(&path)
                .await
                .map_err(|source| RuntimeError::Io {
                    path: path.clone(),
                    source,
                })
        }
        .await;
        let _ = tokio::fs::remove_file(&path).await;
        result
    }
    .await;
    match result {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            Body::from(bytes),
        )
            .into_response(),
        Err(error) => failure(error),
    }
}
