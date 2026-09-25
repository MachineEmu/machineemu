use super::*;
use axum::body::Body;
use axum::http::Request;
use futures_util::StreamExt;
use tower::ServiceExt;

#[cfg(unix)]
#[tokio::test]
async fn intent_create_publishes_self_contained_domain_before_source_removal() {
    let root = tempfile::tempdir().unwrap();
    let workspace_root = root.path().join("workspace");
    std::fs::create_dir_all(workspace_root.join("profiles")).unwrap();
    std::fs::create_dir_all(workspace_root.join("images/source")).unwrap();
    std::fs::create_dir_all(workspace_root.join("generated-engines/qemu-test")).unwrap();
    std::fs::write(
        workspace_root.join("profiles/source.yaml"),
        "api_version: machineemu.io/v1\nkind: Profile\nmetadata:\n  name: source\n  revision: 1\nspec:\n  architecture: x86_64\n  machine:\n    type: pc-q35-8.2\n  resources:\n    memory: 512MiB\n    vcpus: 1\n  network:\n    type: disabled\n",
    )
    .unwrap();
    std::fs::write(
        workspace_root.join("images/source/manifest.yaml"),
        "api_version: machineemu.io/v1\nkind: Image\nmetadata:\n  name: source\n  revision: 1\nspec:\n  image_id: source\n  engine_track: qemu-test\n  supported_engine_tracks: [qemu-test]\n  target: x86_64-softmmu\n  components: {}\n  disk_sha256: \"\"\n",
    )
    .unwrap();
    std::fs::write(
        workspace_root.join("generated-engines/qemu-test/engine-build.json"),
        serde_json::json!({
            "schema_version": 1,
            "track_id": "qemu-test",
            "build_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "executables": {"x86_64-softmmu": "/run/current-system/sw/bin/qemu-system-x86_64"}
        })
        .to_string(),
    )
    .unwrap();
    let state = AppState {
        workspace: Arc::new(Mutex::new(Workspace::open(&workspace_root).unwrap())),
        bearer_token: Arc::from("secret"),
        supervisors: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),
        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
        image_imports: Arc::new(Mutex::new(BTreeMap::new())),
        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(HelperConfig::default()),
        local_unix: false,
    };
    let app = router(state.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/api/v2/instances")
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "instance_id": "domain01",
                "profile_id": "source",
                "image_id": "source",
                "profile": "source"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    if response.status() != StatusCode::CREATED {
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        panic!("create failed: {}", String::from_utf8_lossy(&body));
    }
    let domain_path = workspace_root.join("instances/domain01/instance.yaml");
    let envelope: serde_json::Value = machineemu_core::engine::load_document(&domain_path).unwrap();
    assert_eq!(envelope["domain_document"]["kind"], "Instance");
    assert_eq!(envelope["domain_document"]["metadata"]["name"], "domain01");

    // The source documents are deliberately removed before lifecycle use.
    std::fs::remove_file(workspace_root.join("profiles/source.yaml")).unwrap();
    std::fs::remove_file(workspace_root.join("images/source/manifest.yaml")).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/instances/domain01/start")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    if response.status() != StatusCode::OK {
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        panic!("start failed: {}", String::from_utf8_lossy(&body));
    }
    let stopped = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/instances/domain01/stop")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        stopped.status(),
        StatusCode::OK | StatusCode::NO_CONTENT
    ));
}

#[tokio::test]
async fn daemon_engine_import_registers_bundle_and_replays_completion() {
    let root = tempfile::tempdir().unwrap();
    let source = crate::engine_import::tests::fixture(root.path(), b"engine", false);
    let workspace = root.path().join("workspace");
    let state = AppState {
        workspace: Arc::new(Mutex::new(Workspace::open(&workspace).unwrap())),
        bearer_token: Arc::from("secret"),
        supervisors: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),
        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
        image_imports: Arc::new(Mutex::new(BTreeMap::new())),
        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(HelperConfig::default()),
        local_unix: false,
    };
    let app = router(state);
    for authorized in [false, true] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/v2/engine-imports")
            .header("content-type", "application/json");
        if authorized {
            request = request.header("authorization", "Bearer secret");
        }
        let response = app
            .clone()
            .oneshot(
                request
                    .body(Body::from(serde_json::json!({"source":source}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        if !authorized {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            continue;
        }
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let job: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mut complete = false;
        for _ in 0..100 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(job["status_url"].as_str().unwrap())
                        .header("authorization", "Bearer secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_ne!(status["status"], "failed", "{status}");
            if status["status"] == "complete" {
                complete = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(complete);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(job["events_url"].as_str().unwrap())
                    .header("authorization", "Bearer secret")
                    .header("last-event-id", "0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let mut complete = false;
        for _ in 0..20 {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if String::from_utf8_lossy(&chunk).contains("event: complete") {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert!(
            crate::engine_import::registered(&workspace)
                .unwrap()
                .contains_key("qemu-10.2-analysis")
        );
    }
}

#[test]
fn create_plan_validation_does_not_make_runtime_directories() {
    let root =
        std::env::temp_dir().join(format!("machineemu-plan-validation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let plan = LaunchSpec {
        argv: vec![
            "qemu-system-x86_64".into(),
            "-pidfile".into(),
            root.join("instances/test01/control/qemu.pid")
                .to_string_lossy()
                .into_owned(),
            format!(
                "unix:{}/instances/test01/sockets/serial.sock",
                root.display()
            ),
        ],
        vnc_auto: false,
        qmp_socket: PathBuf::from("instances/test01/sockets/qmp.sock"),
        stdout: Some(PathBuf::from("instances/test01/serial.log")),
        stderr: None,
        preparation: None,
        helper_argv: None,
        helpers: Vec::new(),
    };
    launch::validate_plan_paths(&root, &plan).unwrap();
    assert!(!root.join("instances/test01").exists());
    launch::plan_paths(&root, &plan).unwrap();
    assert!(root.join("instances/test01/sockets").is_dir());
    assert!(root.join("instances/test01/control").is_dir());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bearer_auth_requires_exact_token() {
    let root = std::env::temp_dir().join(format!("machineemu-daemon-auth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let state = AppState {
        workspace: Arc::new(Mutex::new(Workspace::open(&root).unwrap())),
        bearer_token: Arc::from("secret"),
        supervisors: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),

        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
        image_imports: Arc::new(Mutex::new(BTreeMap::new())),

        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(HelperConfig::default()),
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
        supervisors: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),

        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
        image_imports: Arc::new(Mutex::new(BTreeMap::new())),

        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(HelperConfig::default()),
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
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/openapi.json")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(document["paths"]["/ws/v2/instances/{id}/{kind}"]["get"].is_object());
    assert_eq!(
        document["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
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
    for authenticated in [false, true] {
        let mut request = Request::builder().uri("/api/v2/images");
        if authenticated {
            request = request.header("authorization", "Bearer secret");
        }
        let response = router(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        if authenticated {
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let images: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(images.as_array().unwrap().len(), 1);
            assert_eq!(images[0]["image_id"], "debian13-cloud");
        } else {
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }
    let instance = serde_json::json!({
        "instance_id": "lab01",
        "image_id": "debian13-cloud",
        "profile_id": "debian13-cloud",
        "profile": {
            "api_version": "machineemu.io/v1",
            "kind": "Profile",
            "metadata": {"name": "debian13-cloud", "revision": 1},
            "spec": {"architecture": "x86_64", "machine": {"type": "q35"}, "resources": {"memory": "1GiB", "vcpus": 1}}
        },
        "image": {
            "api_version": "machineemu.io/v1",
            "kind": "Image",
            "metadata": {"name": "debian13-cloud", "revision": 1},
            "spec": {"architecture": "x86_64", "compatible_engines": ["unifi-10-2"], "components": {}}
        },
        "context": {"engines": [{"track": "unifi-10-2", "build_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "executable": "/bin/true", "machines": ["q35"]}]}
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
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/lab01/guest-agent")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let guest_agent: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(guest_agent["available"], false);
    assert_eq!(guest_agent["reason"], "not_running");
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/instances/lab01/guest-executions")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"command":"uname -a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/lab01/events")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame = String::from_utf8(first.to_vec()).unwrap();
    assert!(frame.contains("event: snapshot"));
    assert!(frame.contains("\"instance_id\":\"lab01\""));
    let cursor = frame
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
        .to_owned();
    drop(stream);
    let instance_id = Id::new("instance", "lab01").unwrap();
    let instance_record = state
        .workspace
        .lock()
        .unwrap()
        .instance(&instance_id)
        .unwrap();
    events::publish_state(&state, &instance_record, None, "test");
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/lab01/events")
                .header("authorization", "Bearer secret")
                .header("last-event-id", &cursor)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut replay = response.into_body().into_data_stream();
    let replayed = tokio::time::timeout(std::time::Duration::from_secs(2), replay.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(replayed.to_vec())
            .unwrap()
            .contains("event: state")
    );
    drop(replay);
    for _ in 0..257 {
        events::publish_state(&state, &instance_record, None, "test");
    }
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/lab01/events")
                .header("authorization", "Bearer secret")
                .header("last-event-id", &cursor)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut expired = response.into_body().into_data_stream();
    let resync = tokio::time::timeout(std::time::Duration::from_secs(2), expired.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(resync.to_vec())
            .unwrap()
            .contains("event: snapshot")
    );
    drop(expired);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/lab01/events")
                .header("authorization", "Bearer secret")
                .header("last-event-id", "bad-cursor")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/lab01/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut local = state.clone();
    local.local_unix = true;
    let response = router(local)
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/lab01/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for (path, status) in [
        ("/api/v2/instances/missing/events", StatusCode::NOT_FOUND),
        ("/api/v2/instances/lab01/events", StatusCode::OK),
        ("/api/v2/instances/INVALID/events", StatusCode::BAD_REQUEST),
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
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

#[tokio::test]
async fn daemon_import_job_copies_vmmanager_base() {
    let root =
        std::env::temp_dir().join(format!("machineemu-daemon-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let source = root.join("base");
    std::fs::create_dir_all(source.join("tpm")).unwrap();
    std::fs::write(source.join("disk.qcow2"), b"base disk").unwrap();
    std::fs::write(source.join("OVMF_VARS.fd"), b"vars").unwrap();
    std::fs::write(source.join("tpm/tpm2-00.permall"), b"tpm state").unwrap();
    let workspace_root = root.join("workspace");
    let state = AppState {
        workspace: Arc::new(Mutex::new(Workspace::open(&workspace_root).unwrap())),
        bearer_token: Arc::from("secret"),
        supervisors: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),

        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
        image_imports: Arc::new(Mutex::new(BTreeMap::new())),

        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(HelperConfig::default()),
        local_unix: false,
    };
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/image-imports/vmmanager-base")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "source": source,
                        "image_id": "win11-dev",
                        "engine_track": "qemu-10.2-analysis",
                        "target": "x86_64-softmmu"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let status_url = body["status_url"].as_str().unwrap();
    let mut status = serde_json::Value::Null;
    for _ in 0..50 {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri(status_url)
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        status = serde_json::from_slice(&body).unwrap();
        if status["status"] == "complete" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(status["status"], "complete");
    assert_eq!(
        std::fs::read(workspace_root.join("images/win11-dev/components/disk.qcow2")).unwrap(),
        b"base disk"
    );
    assert!(!workspace_root.join("blobs/sha256").exists());
    drop(state);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn checked_in_rust_openapi_matches_generated_document() {
    let generated = serde_json::to_value(super::openapi::document()).unwrap();
    let checked_in: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/openapi-v2.json"
    )))
    .unwrap();
    assert_eq!(checked_in, generated);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "legacy saved launch-plan lifecycle removed by the v1 domain cutover"]
async fn saved_plan_supports_restart_and_disposable_cleanup() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let root = std::env::temp_dir().join(format!("machineemu-disposable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = Workspace::open(&root).unwrap();
    let image = ImageManifest {
        image_id: Id::new("image", "image01").unwrap(),
        engine_track: Id::new("track", "track01").unwrap(),
        supported_engine_tracks: vec![],
        target: "x86_64-softmmu".into(),
        components: std::collections::BTreeMap::new(),
        disk_sha256: "a".repeat(64),
        firmware_sha256: None,
        tpm_state_sha256: None,
    };
    workspace.register_image(&image).unwrap();
    let state = AppState {
        workspace: Arc::new(Mutex::new(workspace)),
        bearer_token: Arc::from("secret"),
        supervisors: Arc::new(Mutex::new(BTreeMap::new())),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),

        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
        image_imports: Arc::new(Mutex::new(BTreeMap::new())),

        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(HelperConfig::default()),
        local_unix: false,
    };
    let socket = root.join("qmp.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut stream = tokio::io::BufReader::new(stream);
                stream
                    .get_mut()
                    .write_all(b"{\"QMP\":{}}\r\n")
                    .await
                    .unwrap();
                let mut line = String::new();
                while stream.read_line(&mut line).await.unwrap() != 0 {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if request["execute"] == "screendump" {
                        tokio::fs::write(
                            request["arguments"]["filename"].as_str().unwrap(),
                            b"\x89PNG\r\n\x1a\n",
                        )
                        .await
                        .unwrap();
                    }
                    let result = if request["execute"] == "query-status" {
                        serde_json::json!({"status":"running"})
                    } else {
                        serde_json::json!({})
                    };
                    stream
                        .get_mut()
                        .write_all(
                            format!(
                                "{}\r\n",
                                serde_json::json!({"return":result,"id":request["id"]})
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    if request["execute"] == "quit" {
                        break;
                    }
                    line.clear();
                }
            });
        }
    });
    let request = |path: &str, body: serde_json::Value| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let stale = root.join("staging/instances/temp01/old");
    let sibling = root.join("staging/instances/temp01-other/old");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let created = router(state.clone()).oneshot(request("/api/v2/instances", serde_json::json!({"instance_id":"temp01","image_id":"image01","profile_id":"profile01","auto_remove":true,"launch_plan":{"argv":["/bin/sh","-c","sleep 2"],"qmp_socket":"qmp.sock"}}))).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert!(!stale.exists());
    assert!(sibling.exists());
    let details = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/temp01")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let details: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(details.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(details["configured"], true);
    assert_eq!(details["auto_remove"], true);
    let snapshot = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/temp01/snapshots",
            serde_json::json!({"snapshot_id":"snap01"}),
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.status(), StatusCode::CONFLICT);
    let restart = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/temp01/restart",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(restart.status(), StatusCode::CONFLICT);
    let started = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/temp01/start",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let stopped = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/temp01/stop",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(stopped.status(), StatusCode::OK);
    let id = Id::new("instance", "temp01").unwrap();
    assert!(state.workspace.lock().unwrap().instance(&id).is_err());
    assert_eq!(
        state
            .workspace
            .lock()
            .unwrap()
            .instance_tombstone(&id)
            .unwrap()
            .unwrap()
            .1,
        "operator_stop"
    );
    let tombstone = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/temp01/tombstone")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tombstone.status(), StatusCode::OK);
    let created = router(state.clone()).oneshot(request("/api/v2/instances", serde_json::json!({"instance_id":"failed01","image_id":"image01","profile_id":"profile01","auto_remove":true,"launch_plan":{"argv":["/this-program-does-not-exist"],"qmp_socket":"qmp.sock"}}))).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let failed_start = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/failed01/start",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(failed_start.status(), StatusCode::CONFLICT);
    let failed_id = Id::new("instance", "failed01").unwrap();
    assert!(
        state
            .workspace
            .lock()
            .unwrap()
            .instance(&failed_id)
            .is_err()
    );
    assert_eq!(
        state
            .workspace
            .lock()
            .unwrap()
            .instance_tombstone(&failed_id)
            .unwrap()
            .unwrap()
            .1,
        "start_failed"
    );
    let created = router(state.clone()).oneshot(request("/api/v2/instances", serde_json::json!({"instance_id":"persist01","image_id":"image01","profile_id":"profile01","profile":{"id":"profile01","resources":{"memory":"1G"}},"launch_plan":{"argv":["/bin/sh","-c","sleep 2"],"qmp_socket":"qmp.sock"}}))).await.unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let stored_profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("instances/persist01/profile.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored_profile["id"], "profile01");
    let started = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/start",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let persistent_id = Id::new("instance", "persist01").unwrap();
    let sent = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/send-key",
            serde_json::json!({"keys":["ctrl","alt","delete"]}),
        ))
        .await
        .unwrap();
    assert_eq!(sent.status(), StatusCode::NO_CONTENT);
    let screenshot = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/screenshot",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(screenshot.status(), StatusCode::OK);
    assert_eq!(screenshot.headers()["content-type"], "image/png");
    assert_eq!(
        axum::body::to_bytes(screenshot.into_body(), 1024)
            .await
            .unwrap(),
        b"\x89PNG\r\n\x1a\n".as_slice()
    );
    let config_response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/instances/persist01/config")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(config_response.status(), StatusCode::OK);
    let mut config: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(config_response.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    config["profile"]["resources"]["memory"] = serde_json::json!("2G");
    config["launch_plan"]["argv"][2] = serde_json::json!("sleep 3");
    let put_config = |body: String| {
        Request::builder()
            .method("PUT")
            .uri("/api/v2/instances/persist01/config")
            .header("authorization", "Bearer secret")
            .header("content-type", "application/yaml")
            .body(Body::from(body))
            .unwrap()
    };
    let yaml = serde_yaml::to_string(&config).unwrap();
    let rejected = router(state.clone())
        .oneshot(put_config(yaml.clone()))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let first_run = state
        .workspace
        .lock()
        .unwrap()
        .active_run(&persistent_id)
        .unwrap()
        .unwrap()
        .run_id;
    let started_body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(started.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(started_body["run_id"], first_run.as_str());
    assert!(started_body["operation_id"].as_str().is_some());
    let restarted = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/restart",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(restarted.status(), StatusCode::OK);
    let second_run = state
        .workspace
        .lock()
        .unwrap()
        .active_run(&persistent_id)
        .unwrap()
        .unwrap()
        .run_id;
    assert_ne!(first_run, second_run);
    assert_eq!(
        state
            .workspace
            .lock()
            .unwrap()
            .run(&first_run)
            .unwrap()
            .status,
        "exited"
    );
    let stopped = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/stop",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(stopped.status(), StatusCode::OK);
    assert_eq!(
        state
            .workspace
            .lock()
            .unwrap()
            .instance(&persistent_id)
            .unwrap()
            .state,
        "stopped"
    );
    // Direct file edits invalidate an older API editor without a lifecycle change.
    let document_path = root.join("instances/persist01/instance.json");
    let mut direct: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&document_path).unwrap()).unwrap();
    direct["launch_plan"]["argv"][2] = "sleep 4".into();
    std::fs::write(&document_path, serde_json::to_vec(&direct).unwrap()).unwrap();
    // No request-side plan can bypass the file.
    let override_start = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/start",
            serde_json::json!({"launch_plan": {}}),
        ))
        .await
        .unwrap();
    assert_eq!(override_start.status(), StatusCode::UNPROCESSABLE_ENTITY);
    direct["launch_plan"]["argv"][2] = "printf from-instance-file; sleep 2".into();
    direct["launch_plan"]["stdout"] = "instances/persist01/from-file.stdout".into();
    std::fs::write(&document_path, serde_json::to_vec(&direct).unwrap()).unwrap();
    let from_file = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/start",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(from_file.status(), StatusCode::OK);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if std::fs::read(root.join("instances/persist01/from-file.stdout")).unwrap_or_default()
                == b"from-instance-file"
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let stopped_again = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/stop",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(stopped_again.status(), StatusCode::OK);
    let serial_ticket = router(state.clone())
        .oneshot(request(
            "/api/v2/instances/persist01/streams/serial/ticket",
            serde_json::json!({"control":true}),
        ))
        .await
        .unwrap();
    assert_eq!(serial_ticket.status(), StatusCode::OK);
    let serial_ticket = axum::body::to_bytes(serial_ticket.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&serial_ticket).unwrap()["ticket"].is_string()
    );
    let stale = router(state.clone())
        .oneshot(put_config(yaml))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::BAD_REQUEST);
    config["revision"] = serde_json::json!(
        state
            .workspace
            .lock()
            .unwrap()
            .instance_document(&persistent_id)
            .unwrap()
            .revision()
            .unwrap()
    );
    let updated = router(state.clone())
        .oneshot(put_config(serde_yaml::to_string(&config).unwrap()))
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    let (saved, _) = state
        .workspace
        .lock()
        .unwrap()
        .instance_launch(&persistent_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&saved).unwrap()["argv"][2],
        "sleep 3"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(root.join("instances/persist01/profile.json")).unwrap()
        )
        .unwrap()["resources"]["memory"],
        "2G"
    );
    let shared_profile = router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v2/profiles/shared01")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/yaml")
                .body(Body::from("id: shared01\nname: Shared profile\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(shared_profile.status(), StatusCode::OK);
    let shared_profile = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/profiles/shared01")
                .header("authorization", "Bearer secret")
                .header("accept", "application/yaml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(shared_profile.headers()["content-type"], "application/yaml");
    let profile_text = axum::body::to_bytes(shared_profile.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&profile_text)
            .unwrap()
            .contains("Shared profile")
    );
    let updated_image = router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v2/images/image01")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"image_id":"image01","engine_track":"track01","supported_engine_tracks":["track02"],"target":"x86_64-softmmu","disk_sha256":"a".repeat(64),"firmware_sha256":null,"tpm_state_sha256":null}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(updated_image.status(), StatusCode::OK);
    assert_eq!(
        state
            .workspace
            .lock()
            .unwrap()
            .image(&Id::new("image", "image01").unwrap())
            .unwrap()
            .supported_engine_tracks[0]
            .as_str(),
        "track02"
    );
    let image_yaml = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v2/images/image01")
                .header("authorization", "Bearer secret")
                .header("accept", "application/yaml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(image_yaml.headers()["content-type"], "application/yaml");
    let image_bytes = axum::body::to_bytes(image_yaml.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&image_bytes)
            .unwrap()
            .contains("track02")
    );
    server.abort();
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
    let mut workspace = Workspace::open(&root).unwrap();
    let image = ImageManifest {
        image_id: Id::new("image", "image01").unwrap(),
        engine_track: Id::new("track", "track01").unwrap(),
        supported_engine_tracks: vec![],
        target: "x86_64-softmmu".into(),
        components: std::collections::BTreeMap::new(),
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
        line.clear();
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(request["execute"], "query-status");
        stream
            .write_all(
                format!(
                    "{{\"return\":{{\"status\":\"running\"}},\"id\":{}}}\r\n",
                    request["id"]
                )
                .as_bytes(),
            )
            .unwrap();
    });
    let running = workspace
        .start_instance_async(machineemu_core::runtime::StartRequest {
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
            on_operation: None,
            on_state: None,
            complete_operation: true,
        })
        .await
        .unwrap();
    workspace
        .transition_instance(&instance_id, "error")
        .unwrap();
    let mut running_map = BTreeMap::new();
    let mut owner = supervisor::RunSupervisor::new(running.run_id.clone());
    owner.running = Some(Arc::new(tokio::sync::Mutex::new(running)));
    running_map.insert("lab01".into(), owner);
    let state = AppState {
        workspace: Arc::new(Mutex::new(workspace)),
        bearer_token: Arc::from("secret"),
        supervisors: Arc::new(Mutex::new(running_map)),
        instance_locks: Arc::new(Mutex::new(BTreeMap::new())),

        display_stream: Arc::new(PathBuf::from("display-stream")),
        stream_tickets: Arc::new(Mutex::new(BTreeMap::new())),
        audio_sessions: Arc::new(Mutex::new(BTreeMap::new())),
        control_streams: Arc::new(Mutex::new(BTreeMap::new())),
        events: Arc::new(Mutex::new(events::EventHub::new().unwrap())),
        image_imports: Arc::new(Mutex::new(BTreeMap::new())),

        guest_executions: Arc::new(Mutex::new(BTreeMap::new())),
        helpers: Arc::new(HelperConfig::default()),
        local_unix: false,
    };
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer secret".parse().unwrap());
    let response = lifecycle_action(state.clone(), headers, "lab01".into(), "stop").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(supervisor::connection(&state, "lab01").unwrap().is_some());
    server.join().unwrap();
    drop(state);
    let _ = std::fs::remove_dir_all(root);
}
