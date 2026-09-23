use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

#[test]
fn bearer_auth_requires_exact_token() {
    let root = std::env::temp_dir().join(format!("machineemu-daemon-auth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let state = AppState {
        workspace: Arc::new(Mutex::new(Workspace::open(&root).unwrap())),
        bearer_token: Arc::from("secret"),
        launch_plans: Arc::new(BTreeMap::new()),
        running: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(Mutex::new(BTreeMap::new())),
        display_streams: Arc::new(Mutex::new(BTreeMap::new())),
        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
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
    let root = std::env::temp_dir().join(format!("machineemu-daemon-route-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let state = AppState {
        workspace: Arc::new(Mutex::new(Workspace::open(&root).unwrap())),
        bearer_token: Arc::from("secret"),
        launch_plans: Arc::new(BTreeMap::new()),
        running: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(Mutex::new(BTreeMap::new())),
        display_streams: Arc::new(Mutex::new(BTreeMap::new())),
        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
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

#[cfg(unix)]
#[tokio::test]
async fn failed_stop_keeps_owned_process_in_running_map() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    let root = std::env::temp_dir().join(format!("machineemu-daemon-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = Workspace::open(&root).unwrap();
    let image = ImageManifest {
        image_id: Id::new("image", "image01").unwrap(),
        engine_track: Id::new("track", "track01").unwrap(),
        supported_engine_tracks: vec![],
        target: "x86_64-softmmu".into(),
        disk_sha256: "a".repeat(64),
        firmware_sha256: None,
        tpm_state_sha256: None,
    };
    workspace.register_image(&image).unwrap();
    let instance_id = Id::new("instance", "lab01").unwrap();
    workspace
        .create_instance(
            instance_id.clone(),
            image.image_id,
            Id::new("profile", "profile01").unwrap(),
        )
        .unwrap();
    let socket = root.join("qmp.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .write_all(b"{\"QMP\":{\"version\":{},\"capabilities\":[]}}\r\n")
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        stream
            .write_all(format!("{{\"return\":{{}},\"id\":{}}}\r\n", request["id"]).as_bytes())
            .unwrap();
    });
    let running = workspace
        .start_instance(machineemu_core::runtime::StartRequest {
            operation_id: Id::new("operation", "op01").unwrap(),
            run_id: Id::new("run", "run01").unwrap(),
            instance_id: instance_id.clone(),
            idempotency_key: "start-01",
            input_json: "{}",
            argv: &["sleep".into(), "30".into()],
            qmp_socket: &socket,
            stdout: None,
            stderr: None,
            qmp_timeout: std::time::Duration::from_secs(1),
        })
        .unwrap();
    workspace
        .transition_instance(&instance_id, "error")
        .unwrap();
    let mut running_map = BTreeMap::new();
    running_map.insert("lab01".into(), Arc::new(Mutex::new(running)));
    let state = AppState {
        workspace: Arc::new(Mutex::new(workspace)),
        bearer_token: Arc::from("secret"),
        launch_plans: Arc::new(BTreeMap::new()),
        running: Arc::new(Mutex::new(running_map)),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(Mutex::new(BTreeMap::new())),
        display_streams: Arc::new(Mutex::new(BTreeMap::new())),
        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
        local_unix: false,
    };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer secret".parse().unwrap());
    let response = lifecycle_action(state.clone(), headers, "lab01".into(), "stop").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(state.running.lock().unwrap().contains_key("lab01"));
    server.join().unwrap();
    drop(state);
    let _ = std::fs::remove_dir_all(root);
}
