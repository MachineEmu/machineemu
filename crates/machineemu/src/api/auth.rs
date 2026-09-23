use super::*;
pub(super) fn authorized(
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
