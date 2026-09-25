use super::*;
use axum::extract::Query;

#[derive(Deserialize)]
pub(super) struct LogQuery {
    #[serde(default = "default_source")]
    source: String,
    #[serde(default = "default_lines")]
    lines: usize,
}
fn default_source() -> String {
    "auto".into()
}
fn default_lines() -> usize {
    100
}

pub(super) async fn logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> impl IntoResponse {
    if let Err(response) = authorized(&headers, &state) {
        return response.into_response();
    }
    let result = blocking(move || -> Result<serde_json::Value, RuntimeError> {
        let id = Id::new("instance", id)?;
        let root = state.workspace.lock().map_err(|_| RuntimeError::Process("workspace lock poisoned".into()))?
            .root().join("instances").join(id.as_str());
        let names: &[&str] = match query.source.as_str() {
            "serial" => &["serial.log"], "stderr" => &["qemu.stderr"], "stdout" => &["qemu.stdout"],
            "auto" => &["serial.log", "qemu.stderr", "qemu.stdout"],
            _ => return Err(RuntimeError::Process("source must be auto, serial, stderr, or stdout".into())),
        };
        let path = names.iter().map(|name| root.join(name)).find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.len() > 0))
            .or_else(|| names.iter().map(|name| root.join(name)).find(|path| path.is_file()))
            .ok_or_else(|| RuntimeError::NotFound { kind: "instance log", id: id.as_str().into() })?;
        let bytes = fs::read(&path).map_err(|source| RuntimeError::Io { path: path.clone(), source })?;
        let content = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(query.lines.min(100_000));
        Ok(serde_json::json!({"source": path.file_name().and_then(|n| n.to_str()),
            "content": lines[start..].join("\n") + if content.ends_with('\n') { "\n" } else { "" }}))
    }).await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error @ RuntimeError::NotFound { .. }) => (
            StatusCode::NOT_FOUND,
            axum::Json(ErrorBody {
                error: error.to_string(),
            }),
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
