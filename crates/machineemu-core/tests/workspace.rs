use sha2::{Digest, Sha256};
use std::io::Write;
fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
use machineemu_core::protocols::qmp::QmpClient;
use machineemu_core::{Error, config::*, domain::*, runtime::*, storage::Workspace};
use std::{fs, path::PathBuf, time::Duration};

fn temp_root(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("machineemu-runtime-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    path
}

#[test]
fn saved_launch_and_removal_tombstone_survive_reopen() {
    let root = temp_root("instance-launch");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let id = Id::new("instance", "temporary01").unwrap();
    workspace
        .create_instance(
            id.clone(),
            image.image_id,
            Id::new("profile", "profile01").unwrap(),
        )
        .unwrap();
    workspace
        .save_instance_launch(&id, r#"{"argv":["qemu"],"qmp_socket":"qmp.sock"}"#, true)
        .unwrap();
    let attached = workspace.attach().unwrap();
    assert!(attached.instance_launch(&id).unwrap().unwrap().1);
    attached.remove_instance(&id).unwrap();
    attached
        .record_instance_tombstone(&id, None, "operator_stop")
        .unwrap();
    assert_eq!(
        attached.instance_tombstone(&id).unwrap().unwrap().1,
        "operator_stop"
    );
    drop(attached);
    drop(workspace);
    let reopened = Workspace::open(&root).unwrap();
    assert!(reopened.instance_launch(&id).unwrap().is_none());
    assert_eq!(
        reopened.instance_tombstone(&id).unwrap().unwrap().1,
        "operator_stop"
    );
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prepared_instance_publishes_files_and_database_together() {
    let root = temp_root("prepared-instance");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let id = Id::new("instance", "prepared01").unwrap();
    let orphan = root.join("instances/prepared01");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join(".machineemu-create"), b"staged\n").unwrap();
    fs::write(orphan.join("partial"), b"old").unwrap();
    let staged = root.join("staging/prepared01-new");
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("profile.json"), b"{\"id\":\"profile01\"}").unwrap();
    workspace
        .publish_prepared_instance(
            id.clone(),
            image.image_id,
            Id::new("profile", "profile01").unwrap(),
            &staged,
            "{\"argv\":[]}",
            false,
        )
        .unwrap();
    assert_eq!(
        fs::read(root.join("instances/prepared01/profile.json")).unwrap(),
        b"{\"id\":\"profile01\"}"
    );
    assert!(!orphan.join("partial").exists());
    assert!(!orphan.join(".machineemu-create").exists());
    assert!(!staged.exists());
    assert!(workspace.instance_launch(&id).unwrap().is_some());
    drop(workspace);
    let reopened = Workspace::open(&root).unwrap();
    assert_eq!(reopened.instance(&id).unwrap().state, "created");
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prepared_instance_recovers_empty_plan_directories_only() {
    let root = temp_root("prepared-plan-directories");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let profile_id = Id::new("profile", "profile01").unwrap();
    let destination = root.join("instances/recover01");
    fs::create_dir_all(destination.join("sockets")).unwrap();
    fs::create_dir_all(destination.join("control")).unwrap();
    let staged = root.join("staging/recover01-new");
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("profile.json"), b"{}").unwrap();
    workspace
        .publish_prepared_instance(
            Id::new("instance", "recover01").unwrap(),
            image.image_id.clone(),
            profile_id.clone(),
            &staged,
            "{}",
            false,
        )
        .unwrap();
    assert_eq!(fs::read(destination.join("profile.json")).unwrap(), b"{}");

    let protected = root.join("instances/protected01");
    fs::create_dir_all(protected.join("sockets")).unwrap();
    fs::write(protected.join("sockets/qmp.sock"), b"occupied").unwrap();
    let staged = root.join("staging/protected01-new");
    fs::create_dir(&staged).unwrap();
    assert!(
        workspace
            .publish_prepared_instance(
                Id::new("instance", "protected01").unwrap(),
                image.image_id,
                profile_id,
                &staged,
                "{}",
                false,
            )
            .is_err()
    );
    assert!(protected.join("sockets/qmp.sock").exists());
    assert!(staged.exists());
    drop(workspace);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn yaml_config_supports_server_only_and_relative_paths() {
    let root = temp_root("config");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("machineemu.yaml");
    fs::write(
        &path,
        "server:\n  workspace: ./workspace\n  unix_socket: ./control.sock\n",
    )
    .unwrap();
    let (config, loaded) = load_config(Some(&path)).unwrap();
    assert!(config.client.is_none());
    let server = config.server.unwrap();
    assert_eq!(server.workspace, Some(PathBuf::from("./workspace")));
    assert_eq!(
        resolve_config_path(loaded.as_deref(), server.unix_socket.unwrap()),
        root.join("control.sock")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn yaml_config_supports_engine_registry_and_optional_digest() {
    let root = temp_root("engine-config");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("machineemu.yaml");
    fs::write(
            &path,
            "engines:\n  unifi-10.2:\n    path: ./qemu-build\n    version: 10.2.4\n    build_digest: sha256:abc\n",
        )
        .unwrap();
    let (config, _) = load_config(Some(&path)).unwrap();
    let engine = config.engines.get("unifi-10.2").unwrap();
    assert_eq!(engine.path, PathBuf::from("./qemu-build"));
    assert_eq!(engine.version.as_deref(), Some("10.2.4"));
    assert_eq!(engine.build_digest.as_deref(), Some("sha256:abc"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_missing_helper_is_reported_as_a_missing_executable() {
    let argv = vec!["machineemu-absent-helper".to_string(), "socket".into()];
    let Err(error) = ManagedProcess::spawn(Id::new("run", "run01").unwrap(), &argv, None, None)
    else {
        panic!("an uninstalled helper cannot start");
    };
    assert!(
        matches!(&error, Error::ExecutableNotFound { executable } if executable == "machineemu-absent-helper"),
        "unexpected error: {error}"
    );
    assert_eq!(
        error.to_string(),
        "\"machineemu-absent-helper\" was not found on PATH"
    );
}

#[test]
fn a_helper_path_that_cannot_run_keeps_its_cause() {
    let root = temp_root("spawn-not-executable");
    fs::create_dir_all(&root).unwrap();
    let helper = root.join("swtpm");
    fs::write(&helper, b"not an executable\n").unwrap();
    let argv = vec![helper.to_string_lossy().into_owned()];
    let Err(error) = ManagedProcess::spawn(Id::new("run", "run01").unwrap(), &argv, None, None)
    else {
        panic!("a non-executable file cannot start");
    };
    assert!(
        matches!(&error, Error::Spawn { executable, .. } if executable == &argv[0]),
        "unexpected error: {error}"
    );
    assert!(error.to_string().starts_with("cannot execute "));
    let _ = fs::remove_dir_all(root);
}

fn manifest() -> ImageManifest {
    ImageManifest {
        image_id: Id::new("image", "debian13-cloud").unwrap(),
        engine_track: Id::new("engine track", "unifi-10-2").unwrap(),
        supported_engine_tracks: Vec::new(),
        target: "x86_64-softmmu".into(),
        disk_sha256: "a".repeat(64),
        firmware_sha256: Some("b".repeat(64)),
        tpm_state_sha256: None,
    }
}

#[test]
fn workspace_owns_one_root_and_persists_records() {
    let root = temp_root("records");
    let workspace = Workspace::open(&root).unwrap();
    assert!(matches!(
        Workspace::open(&root),
        Err(Error::WorkspaceLocked(_))
    ));
    let image = manifest();
    let digest = workspace.register_image(&image).unwrap();
    assert_eq!(workspace.image(&image.image_id).unwrap(), image);
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id.clone(),
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    assert_eq!(instance.state, "created");
    drop(workspace);
    let reopened = Workspace::open(&root).unwrap();
    assert_eq!(reopened.image(&image.image_id).unwrap(), image);
    assert_eq!(reopened.instance(&instance.instance_id).unwrap(), instance);
    assert_eq!(digest.len(), 64);
    drop(reopened);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn ids_reject_unsafe_values() {
    assert!(Id::new("instance", "../escape").is_err());
    assert!(Id::new("instance", "UpperCase").is_err());
    assert!(Id::new("instance", "safe-01").is_ok());
    assert!(Id::new("engine track", "unifi-10.2").is_ok());
}

#[cfg(target_os = "linux")]
#[test]
fn stale_workspace_lock_is_reclaimed_but_live_lock_is_rejected() {
    let root = temp_root("stale-lock");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("workspace.lock"), b"999999 1\n").unwrap();
    let workspace = Workspace::open(&root).unwrap();
    assert!(matches!(
        Workspace::open(&root),
        Err(Error::WorkspaceLocked(_))
    ));
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn attached_connection_keeps_workspace_lease() {
    let root = temp_root("attached-lease");
    let owner = Workspace::open(&root).unwrap();
    let attached = owner.attach().unwrap();
    drop(owner);
    assert!(matches!(
        Workspace::open(&root),
        Err(Error::WorkspaceLocked(_))
    ));
    drop(attached);
    assert!(Workspace::open(&root).is_ok());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn operation_retries_are_idempotent_and_transitions_are_guarded() {
    let root = temp_root("operations");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id,
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    assert!(matches!(
        workspace.transition_instance(&instance.instance_id, "running"),
        Err(Error::InvalidTransition { .. })
    ));
    let accepted = workspace
        .begin_operation(
            Id::new("operation", "op01").unwrap(),
            instance.instance_id.clone(),
            "start",
            "request-01",
            r#"{"revision":1}"#,
        )
        .unwrap();
    let retry = workspace
        .begin_operation(
            Id::new("operation", "op02").unwrap(),
            instance.instance_id.clone(),
            "start",
            "request-01",
            r#"{"revision":1}"#,
        )
        .unwrap();
    assert_eq!(retry.operation_id, accepted.operation_id);
    assert!(matches!(
        workspace.begin_operation(
            Id::new("operation", "op03").unwrap(),
            instance.instance_id.clone(),
            "start",
            "request-01",
            r#"{"revision":2}"#,
        ),
        Err(Error::OperationConflict { .. })
    ));
    workspace
        .transition_instance(&instance.instance_id, "starting")
        .unwrap();
    workspace
        .transition_instance(&instance.instance_id, "running")
        .unwrap();
    let completed = workspace
        .complete_operation(&accepted.operation_id, r#"{"ok":true}"#)
        .unwrap();
    assert_eq!(completed.status, "completed");
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn interrupted_start_and_snapshot_operations_reconcile_from_committed_state() {
    let root = temp_root("operation-recovery");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id,
            Id::new("profile", "profile01").unwrap(),
        )
        .unwrap();
    let start = workspace
        .begin_operation(
            Id::new("operation", "start01").unwrap(),
            instance.instance_id.clone(),
            "start",
            "start01",
            "{}",
        )
        .unwrap();
    let snapshot_id = Id::new("snapshot", "snap01").unwrap();
    let snapshot = workspace
        .begin_operation(
            Id::new("operation", "snapshot01").unwrap(),
            instance.instance_id.clone(),
            "snapshot",
            "snapshot:snap01",
            "{\"snapshot_id\":\"snap01\"}",
        )
        .unwrap();
    workspace
        .create_instance_snapshot(snapshot_id, instance.instance_id)
        .unwrap();
    let reconciled = workspace.reconcile_operations().unwrap();
    assert_eq!(reconciled.len(), 2);
    assert_eq!(
        workspace.operation(&start.operation_id).unwrap().status,
        "failed"
    );
    assert_eq!(
        workspace.operation(&snapshot.operation_id).unwrap().status,
        "completed"
    );
    assert!(workspace.reconcile_operations().unwrap().is_empty());
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn stopped_instance_can_be_removed_but_snapshotted_instance_cannot() {
    let root = temp_root("remove-instance");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance_id = Id::new("instance", "lab01").unwrap();
    workspace
        .create_instance(
            instance_id.clone(),
            image.image_id,
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    workspace.remove_instance(&instance_id).unwrap();
    assert!(matches!(
        workspace.instance(&instance_id),
        Err(Error::NotFound {
            kind: "instance",
            ..
        })
    ));
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn blob_import_verifies_before_atomic_publication() {
    let root = temp_root("blobs");
    let workspace = Workspace::open(&root).unwrap();
    let source = root.join("source.bin");
    fs::write(&source, b"immutable input").unwrap();
    let digest = hex_digest(b"immutable input");
    let imported = workspace.import_blob(&source, &digest).unwrap();
    assert_eq!(fs::read(&imported).unwrap(), b"immutable input");
    assert!(workspace.import_blob(&source, &"0".repeat(64)).is_err());
    assert!(!root.join("staging/import-").exists());
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn portable_image_bundle_round_trips_named_components() {
    let root = temp_root("image-bundle");
    let workspace = Workspace::open(&root).unwrap();
    let bundle = root.join("portable-image");
    fs::create_dir_all(bundle.join("components")).unwrap();
    let disk = b"portable disk contents";
    fs::write(bundle.join("components/disk.qcow2"), disk).unwrap();
    let manifest = ImageBundleManifest {
        schema_version: 1,
        image_id: Id::new("image", "portable-test").unwrap(),
        engine_track: Id::new("engine track", "unifi-10-2").unwrap(),
        supported_engine_tracks: vec![Id::new("track", "unifi-10.2-analysis").unwrap()],
        target: "x86_64-softmmu".into(),
        components: [(
            "disk".into(),
            ImageBundleComponent {
                path: "components/disk.qcow2".into(),
                sha256: format!("sha256:{}", hex_digest(disk)),
            },
        )]
        .into_iter()
        .collect(),
    };
    fs::write(
        bundle.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let (image, _) = workspace.import_image_bundle(&bundle).unwrap();
    assert_eq!(
        image.supported_engine_tracks,
        manifest.supported_engine_tracks
    );
    assert_eq!(image.disk_sha256, hex_digest(disk));
    let exported = root.join("exported-image");
    let exported_manifest = workspace
        .export_image_bundle(&image.image_id, &exported)
        .unwrap();
    assert_eq!(
        fs::read(exported.join("components/disk.qcow2")).unwrap(),
        disk
    );
    assert_eq!(exported_manifest.image_id, image.image_id);
    assert_eq!(
        exported_manifest.supported_engine_tracks,
        manifest.supported_engine_tracks
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn vmmanager_base_import_ignores_runtime_lock_files() {
    let root = temp_root("vmmanager-base");
    let source = root.join("source");
    fs::create_dir_all(source.join("tpm")).unwrap();
    fs::write(source.join("disk.qcow2"), b"base disk").unwrap();
    fs::write(source.join("OVMF_VARS.fd"), b"vars").unwrap();
    fs::write(source.join("tpm/tpm2-00.permall"), b"tpm state").unwrap();
    fs::write(source.join("tpm/.lock"), b"").unwrap();
    fs::write(source.join("tpm/swtpm.pid"), b"1234\n").unwrap();
    let workspace = Workspace::open(root.join("workspace")).unwrap();
    let image = workspace
        .import_vmmanager_base(
            &source,
            Id::new("image", "win11-dev").unwrap(),
            Id::new("engine", "qemu-10-2").unwrap(),
            "x86_64-softmmu",
        )
        .unwrap();
    assert_eq!(
        fs::read(
            workspace
                .root()
                .join("blobs/sha256")
                .join(&image.disk_sha256)
        )
        .unwrap(),
        b"base disk"
    );
    assert!(image.firmware_sha256.is_some());
    assert!(image.tpm_state_sha256.is_some());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn instance_preparation_creates_overlay_nvram_and_tpm_state() {
    let root = temp_root("instance-preparation");
    let workspace = Workspace::open(&root).unwrap();
    let image_root = root.join("source");
    fs::create_dir_all(&image_root).unwrap();
    let backing = image_root.join("disk.qcow2");
    let status = std::process::Command::new("qemu-img")
        .args(["create", "-f", "qcow2", backing.to_str().unwrap(), "1M"])
        .status()
        .unwrap();
    assert!(status.success());
    let nvram = image_root.join("vars.fd");
    let tpm = image_root.join("tpm.permall");
    fs::write(&nvram, b"vars").unwrap();
    fs::write(&tpm, b"tpm").unwrap();
    let instance = Id::new("instance", "lab01").unwrap();
    fs::create_dir_all(workspace.root().join("instances/lab01")).unwrap();
    let overlay = workspace
        .prepare_instance_files(&instance, &backing, "qcow2", Some(&nvram), Some(&tpm))
        .unwrap();
    assert!(overlay.is_file());
    assert_eq!(
        fs::read(workspace.root().join("instances/lab01/OVMF_VARS.fd")).unwrap(),
        b"vars"
    );
    assert_eq!(
        fs::read(workspace.root().join("instances/lab01/tpm/tpm2-00.permall")).unwrap(),
        b"tpm"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn qmp_client_negotiates_ignores_events_and_returns_commands() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;

    let root = temp_root("qmp");
    fs::create_dir_all(&root).unwrap();
    let socket = root.join("qmp.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .write_all(b"{\"QMP\":{\"version\":{},\"capabilities\":[]}}\r\n")
            .unwrap();
        let reader_stream = stream.try_clone().unwrap();
        let mut reader = BufReader::new(reader_stream);
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let id = request["id"].clone();
            if request["execute"] == "qmp_capabilities" {
                stream
                    .write_all(format!("{{\"return\":{{}},\"id\":{id}}}\r\n").as_bytes())
                    .unwrap();
            } else {
                stream.write_all(b"{\"event\":\"STOP\"}\r\n").unwrap();
                stream
                    .write_all(
                        format!("{{\"return\":{{\"status\":\"running\"}},\"id\":{id}}}\r\n")
                            .as_bytes(),
                    )
                    .unwrap();
            }
        }
    });
    let mut client = QmpClient::connect(&socket, Duration::from_secs(1)).unwrap();
    let result = client
        .execute("query-status", serde_json::Value::Null)
        .unwrap();
    assert_eq!(result["status"], "running");
    server.join().unwrap();
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn qmp_display_client_passes_socket_fd() {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    use std::io::{BufRead, BufReader, IoSliceMut};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    let root = temp_root("qmp-display-fd");
    fs::create_dir_all(&root).unwrap();
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
        let capabilities: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        stream
            .write_all(format!("{{\"return\":{{}},\"id\":{}}}\r\n", capabilities["id"]).as_bytes())
            .unwrap();

        let mut bytes = [0u8; 256];
        let mut iov = [IoSliceMut::new(&mut bytes)];
        let mut cmsg = nix::cmsg_space!([std::os::fd::RawFd; 1]);
        let message = recvmsg::<()>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::empty(),
        )
        .unwrap();
        let count = message.bytes;
        let rights: Vec<_> = message
            .cmsgs()
            .unwrap()
            .flat_map(|cmsg| match cmsg {
                ControlMessageOwned::ScmRights(fds) => fds,
                _ => vec![],
            })
            .collect();
        assert_eq!(rights.len(), 1);
        let getfd: serde_json::Value = serde_json::from_slice(&bytes[..count]).unwrap();
        assert_eq!(getfd["execute"], "getfd");
        assert_eq!(getfd["arguments"]["fdname"], "me-display-2");
        stream
            .write_all(format!("{{\"return\":{{}},\"id\":{}}}\r\n", getfd["id"]).as_bytes())
            .unwrap();

        line.clear();
        reader.read_line(&mut line).unwrap();
        let add: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(add["execute"], "add_client");
        assert_eq!(add["arguments"]["protocol"], "@dbus-display");
        assert_eq!(add["arguments"]["fdname"], "me-display-2");
        stream
            .write_all(format!("{{\"return\":{{}},\"id\":{}}}\r\n", add["id"]).as_bytes())
            .unwrap();
        for fd in rights {
            nix::unistd::close(fd).unwrap();
        }
    });
    let mut client = QmpClient::connect(&socket, Duration::from_secs(1)).unwrap();
    let (_local, remote) = UnixStream::pair().unwrap();
    client.attach_dbus_display(remote.as_raw_fd()).unwrap();
    server.join().unwrap();
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn display_process_receives_private_bus_as_stdin() {
    use std::os::unix::net::UnixStream;
    let root = temp_root("display-stdin");
    fs::create_dir_all(&root).unwrap();
    let output = root.join("output");
    let (mut bus, child_bus) = UnixStream::pair().unwrap();
    let mut process = ManagedProcess::spawn_with_stdin(
        Id::new("run", "display-1").unwrap(),
        &["cat".into()],
        Some(&output),
        None,
        child_bus,
    )
    .unwrap();
    bus.write_all(b"display bus bytes").unwrap();
    drop(bus);
    assert!(process.wait().unwrap().success);
    assert_eq!(fs::read(&output).unwrap(), b"display bus bytes");
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn process_supervisor_uses_argv_and_reports_exit() {
    let root = temp_root("process");
    fs::create_dir_all(&root).unwrap();
    let stdout = root.join("stdout.log");
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf '%s' \"$1\"".into(),
        "machineemu-test".into(),
        "argv-value".into(),
    ];
    let run_id = Id::new("run", "run01").unwrap();
    let mut process = ManagedProcess::spawn(run_id.clone(), &argv, Some(&stdout), None).unwrap();
    let exit = process.wait().unwrap();
    assert!(exit.success);
    assert_eq!(process.run_id, run_id);
    assert_eq!(fs::read_to_string(stdout).unwrap(), "argv-value");
    drop(process);
    let _ = fs::remove_dir_all(root);
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn run_reconciliation_rejects_a_dead_or_reused_process_identity() {
    let root = temp_root("runs");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id,
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    let argv = vec!["/bin/sh".into(), "-c".into(), "sleep 2".into()];
    let mut process =
        ManagedProcess::spawn(Id::new("run", "run01").unwrap(), &argv, None, None).unwrap();
    let run = workspace
        .record_run(
            process.run_id.clone(),
            instance.instance_id,
            process.pid,
            process.process_start().unwrap(),
            root.join("qmp.sock"),
        )
        .unwrap();
    assert_eq!(
        workspace.reconcile_run(&run.run_id).unwrap().status,
        "running"
    );
    process.terminate().unwrap();
    process.wait().unwrap();
    assert_eq!(
        workspace.reconcile_run(&run.run_id).unwrap().status,
        "uncertain"
    );
    assert_eq!(workspace.reconcile_active_runs().unwrap().len(), 1);
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn start_orchestrates_operation_process_run_and_qmp() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;

    let root = temp_root("start");
    let mut workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id,
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    let socket = root.join("qmp.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        for commands in [6, 3] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"{\"QMP\":{\"version\":{},\"capabilities\":[]}}\r\n")
                .unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let mut reader = BufReader::new(reader_stream);
            for _ in 0..commands {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                let result = if request["execute"] == "query-status" {
                    serde_json::json!({"status":"running"})
                } else {
                    serde_json::json!({})
                };
                stream
                    .write_all(
                        format!("{{\"return\":{result},\"id\":{}}}\r\n", request["id"]).as_bytes(),
                    )
                    .unwrap();
            }
        }
    });
    let argv = vec!["/bin/sh".into(), "-c".into(), "sleep 2".into()];
    let observed_operations = std::sync::Mutex::new(Vec::new());
    let observed_states = std::sync::Mutex::new(Vec::new());
    let on_operation = |operation: &machineemu_core::domain::Operation| {
        observed_operations
            .lock()
            .unwrap()
            .push(operation.status.clone());
    };
    let on_state = |instance: &machineemu_core::domain::Instance| {
        observed_states.lock().unwrap().push(instance.state);
    };
    let mut running = workspace
        .start_instance_async(StartRequest {
            operation_id: Id::new("operation", "op01").unwrap(),
            run_id: Id::new("run", "run01").unwrap(),
            instance_id: instance.instance_id.clone(),
            idempotency_key: "start-01",
            input_json: r#"{"profile":"debian13-cloud"}"#,
            argv: &argv,
            qmp_socket: &socket,
            stdout: None,
            stderr: None,
            qmp_timeout: Duration::from_secs(1),
            on_operation: Some(&on_operation),
            on_state: Some(&on_state),
            complete_operation: true,
        })
        .await
        .unwrap();
    assert_eq!(
        *observed_operations.lock().unwrap(),
        ["accepted", "completed"]
    );
    assert_eq!(*observed_states.lock().unwrap(), ["starting", "running"]);
    assert_eq!(
        workspace.instance(&instance.instance_id).unwrap().state,
        "running"
    );
    assert_eq!(workspace.run(&running.run_id).unwrap().status, "running");
    assert_eq!(
        workspace
            .pause_instance_async(&instance.instance_id, &mut running)
            .await
            .unwrap()
            .state,
        "paused"
    );
    assert_eq!(
        workspace
            .resume_instance_async(&instance.instance_id, &mut running)
            .await
            .unwrap()
            .state,
        "running"
    );
    workspace
        .reset_instance_async(&instance.instance_id, &mut running)
        .await
        .unwrap();
    assert_eq!(
        workspace
            .stop_instance_async(&instance.instance_id, &mut running)
            .await
            .unwrap()
            .state,
        "stopped"
    );
    assert_eq!(workspace.run(&running.run_id).unwrap().status, "exited");
    let mut restarted = workspace
        .start_instance_async(StartRequest {
            operation_id: Id::new("operation", "op02").unwrap(),
            run_id: Id::new("run", "run02").unwrap(),
            instance_id: instance.instance_id.clone(),
            idempotency_key: "start-02",
            input_json: r#"{"profile":"debian13-cloud"}"#,
            argv: &argv,
            qmp_socket: &socket,
            stdout: None,
            stderr: None,
            qmp_timeout: Duration::from_secs(1),
            on_operation: None,
            on_state: None,
            complete_operation: true,
        })
        .await
        .unwrap();
    assert_eq!(
        workspace.instance(&instance.instance_id).unwrap().state,
        "running"
    );
    assert_ne!(running.run_id, restarted.run_id);
    assert_eq!(workspace.run(&running.run_id).unwrap().status, "exited");
    assert_eq!(
        workspace
            .stop_instance_async(&instance.instance_id, &mut restarted)
            .await
            .unwrap()
            .state,
        "stopped"
    );
    server.join().unwrap();
    let _ = fs::remove_dir_all(root);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_start_reaps_child_and_finishes_operation() {
    let root = temp_root("failed-start-cleanup");
    let mut workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id,
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    let operation_id = Id::new("operation", "op01").unwrap();
    let run_id = Id::new("run", "run01").unwrap();
    let result = workspace
        .start_instance_async(StartRequest {
            operation_id: operation_id.clone(),
            run_id: run_id.clone(),
            instance_id: instance.instance_id.clone(),
            idempotency_key: "start-01",
            input_json: "{}",
            argv: &["sleep".into(), "30".into()],
            qmp_socket: &root.join("missing.sock"),
            stdout: None,
            stderr: None,
            qmp_timeout: Duration::from_millis(50),
            on_operation: None,
            on_state: None,
            complete_operation: true,
        })
        .await;
    assert!(result.is_err());
    let run = workspace.run(&run_id).unwrap();
    assert_eq!(run.status, "failed");
    assert!(!std::path::Path::new(&format!("/proc/{}", run.pid)).exists());
    assert_eq!(workspace.operation(&operation_id).unwrap().status, "failed");
    assert_eq!(
        workspace.instance(&instance.instance_id).unwrap().state,
        "error"
    );
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn recovered_run_can_be_stopped_through_qmp() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;

    let root = temp_root("recovered-stop");
    let mut workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id,
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    workspace
        .transition_instance(&instance.instance_id, "starting")
        .unwrap();
    workspace
        .transition_instance(&instance.instance_id, "running")
        .unwrap();
    let run_id = Id::new("run", "run01").unwrap();
    let argv = vec!["sleep".into(), "30".into()];
    let mut process = ManagedProcess::spawn(run_id.clone(), &argv, None, None).unwrap();
    let socket = root.join("qmp.sock");
    workspace
        .record_run(
            run_id.clone(),
            instance.instance_id.clone(),
            process.pid,
            process.process_start().unwrap(),
            socket.clone(),
        )
        .unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .write_all(b"{\"QMP\":{\"version\":{},\"capabilities\":[]}}\r\n")
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for _ in 0..3 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let reply = if request["execute"] == "query-status" {
                serde_json::json!({"status":"running"})
            } else {
                serde_json::json!({})
            };
            stream
                .write_all(
                    format!("{{\"return\":{reply},\"id\":{}}}\r\n", request["id"]).as_bytes(),
                )
                .unwrap();
        }
        process.terminate().unwrap();
        process.wait().unwrap();
    });
    let mut recovered = workspace
        .recover_instance_run_async(&instance.instance_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        workspace
            .stop_instance_async(&instance.instance_id, &mut recovered)
            .await
            .unwrap()
            .state,
        "stopped"
    );
    assert_eq!(workspace.run(&run_id).unwrap().status, "exited");
    server.join().unwrap();
    drop(workspace);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn stopped_snapshot_restores_hashed_components() {
    let root = temp_root("snapshot");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let instance = workspace
        .create_instance(
            Id::new("instance", "lab01").unwrap(),
            image.image_id,
            Id::new("profile", "debian13-cloud").unwrap(),
        )
        .unwrap();
    workspace
        .transition_instance(&instance.instance_id, "starting")
        .unwrap();
    assert!(matches!(
        workspace.create_snapshot(
            Id::new("snapshot", "snap-running").unwrap(),
            instance.instance_id.clone(),
            &[],
        ),
        Err(Error::SnapshotRequiresStopped)
    ));
    workspace
        .transition_instance(&instance.instance_id, "running")
        .unwrap();
    workspace
        .transition_instance(&instance.instance_id, "stopping")
        .unwrap();
    workspace
        .transition_instance(&instance.instance_id, "stopped")
        .unwrap();
    let source = root.join("disk.qcow2");
    fs::write(&source, b"before").unwrap();
    let snapshot = workspace
        .create_snapshot(
            Id::new("snapshot", "snap01").unwrap(),
            instance.instance_id,
            &[("disk.qcow2".into(), source.clone())],
        )
        .unwrap();
    assert_eq!(snapshot.files["disk.qcow2"], hex_digest(b"before"));
    fs::write(&source, b"after").unwrap();
    let restored = root.join("restored");
    workspace
        .restore_snapshot(&snapshot.snapshot_id, &restored)
        .unwrap();
    assert_eq!(fs::read(restored.join("disk.qcow2")).unwrap(), b"before");
    let clone_destination = root.join("clone-files");
    let clone = workspace
        .clone_snapshot(
            &snapshot.snapshot_id,
            Id::new("instance", "lab02").unwrap(),
            Id::new("profile", "debian13-cloud").unwrap(),
            &clone_destination,
        )
        .unwrap();
    assert_eq!(clone.state, "created");
    assert_eq!(
        fs::read(clone_destination.join("disk.qcow2")).unwrap(),
        b"before"
    );
    assert_ne!(clone.instance_id, snapshot.instance_id);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn image_listing_is_sorted_and_read_only_while_workspace_is_locked() {
    let root = temp_root("list-images");
    assert!(Workspace::list_images(&root).unwrap().is_empty());
    assert!(!root.exists());
    let workspace = Workspace::open(&root).unwrap();
    for name in ["zebra", "alpha"] {
        workspace
            .register_image(&ImageManifest {
                image_id: Id::new("image", name).unwrap(),
                engine_track: Id::new("track", "qemu-10.2").unwrap(),
                supported_engine_tracks: Vec::new(),
                target: "x86_64-softmmu".into(),
                disk_sha256: "a".repeat(64),
                firmware_sha256: None,
                tpm_state_sha256: None,
            })
            .unwrap();
    }
    let before = fs::read(root.join("metadata.sqlite3")).unwrap();
    let lock = fs::read(root.join("workspace.lock")).unwrap();
    let images = Workspace::list_images(&root).unwrap();
    assert_eq!(
        images
            .iter()
            .map(|image| image.image_id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zebra"]
    );
    assert_eq!(fs::read(root.join("metadata.sqlite3")).unwrap(), before);
    assert_eq!(fs::read(root.join("workspace.lock")).unwrap(), lock);
    drop(workspace);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_image_manifest_serialization_keeps_its_digest_input() {
    let image = manifest();
    let bytes = serde_json::to_vec(&image).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("supported_engine_tracks"));
    let restored: ImageManifest = serde_json::from_slice(&bytes).unwrap();
    assert!(restored.supported_engine_tracks.is_empty());
    assert_eq!(serde_json::to_vec(&restored).unwrap(), bytes);
}

#[test]
fn editable_image_files_are_authoritative_and_work_without_sqlite() {
    let root = temp_root("image-files");
    let workspace = Workspace::open(&root).unwrap();
    let mut image = manifest();
    workspace.register_image(&image).unwrap();
    let path = root.join("images/debian13-cloud/manifest.json");
    image
        .supported_engine_tracks
        .push(Id::new("track", "analysis").unwrap());
    fs::write(&path, serde_json::to_vec_pretty(&image).unwrap()).unwrap();
    assert_eq!(workspace.image(&image.image_id).unwrap(), image);
    assert_eq!(Workspace::list_images(&root).unwrap(), vec![image.clone()]);
    let mut manual = image.clone();
    manual.image_id = Id::new("image", "manual").unwrap();
    let manual_dir = root.join("images/manual");
    fs::create_dir_all(&manual_dir).unwrap();
    fs::write(
        manual_dir.join("manifest.json"),
        serde_json::to_vec(&manual).unwrap(),
    )
    .unwrap();
    workspace
        .create_instance(
            Id::new("instance", "manual-vm").unwrap(),
            manual.image_id,
            Id::new("profile", "test").unwrap(),
        )
        .unwrap();
    drop(workspace);
    let db = rusqlite::Connection::open(root.join("metadata.sqlite3")).unwrap();
    let columns: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('images')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(columns, 1);
    drop(db);
    fs::remove_file(root.join("metadata.sqlite3")).unwrap();
    assert_eq!(Workspace::list_images(&root).unwrap().len(), 2);
    fs::write(&path, "{broken").unwrap();
    let error = Workspace::list_images(&root).unwrap_err().to_string();
    assert!(error.contains("debian13-cloud/manifest.json"));
    fs::write(&path, serde_json::to_vec(&image).unwrap()).unwrap();
    let mut unsafe_image = serde_json::to_value(image).unwrap();
    unsafe_image["image_id"] = serde_json::json!("../escape");
    fs::write(&path, serde_json::to_vec(&unsafe_image).unwrap()).unwrap();
    assert!(Workspace::list_images(&root).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_image_migration_preserves_edits_and_instance_references() {
    let root = temp_root("legacy-images");
    fs::create_dir_all(&root).unwrap();
    let image = manifest();
    let db = rusqlite::Connection::open(root.join("metadata.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE images (image_id TEXT PRIMARY KEY, manifest_json BLOB NOT NULL, manifest_sha256 TEXT NOT NULL UNIQUE, imported_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
        CREATE TABLE schema_version (version INTEGER NOT NULL);
        INSERT INTO schema_version VALUES (1);
        CREATE TABLE instances (instance_id TEXT PRIMARY KEY, image_id TEXT NOT NULL REFERENCES images(image_id), profile_id TEXT NOT NULL, lifecycle TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);").unwrap();
    let bytes = serde_json::to_vec(&image).unwrap();
    db.execute(
        "INSERT INTO images(image_id,manifest_json,manifest_sha256) VALUES (?1,?2,?3)",
        rusqlite::params![image.image_id.as_str(), bytes, hex_digest(&bytes)],
    )
    .unwrap();
    let mut second = image.clone();
    second.image_id = Id::new("image", "second").unwrap();
    let bytes = serde_json::to_vec(&second).unwrap();
    db.execute(
        "INSERT INTO images(image_id,manifest_json,manifest_sha256) VALUES (?1,?2,?3)",
        rusqlite::params![second.image_id.as_str(), bytes, hex_digest(&bytes)],
    )
    .unwrap();
    db.execute("INSERT INTO instances(instance_id,image_id,profile_id,lifecycle) VALUES ('vm01',?1,'demo','stopped')", [image.image_id.as_str()]).unwrap();
    drop(db);
    let path = root.join("images/debian13-cloud/manifest.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut edited = image.clone();
    edited
        .supported_engine_tracks
        .push(Id::new("track", "analysis").unwrap());
    let edited_bytes = serde_json::to_vec_pretty(&edited).unwrap();
    fs::write(&path, &edited_bytes).unwrap();
    let workspace = Workspace::open(&root).unwrap();
    assert_eq!(workspace.image(&image.image_id).unwrap(), edited);
    assert_eq!(workspace.image(&second.image_id).unwrap(), second);
    assert_eq!(fs::read(&path).unwrap(), edited_bytes);
    assert_eq!(
        workspace
            .instance(&Id::new("instance", "vm01").unwrap())
            .unwrap()
            .image_id,
        image.image_id
    );
    let db = rusqlite::Connection::open(root.join("metadata.sqlite3")).unwrap();
    let violations: i64 = db
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    let columns: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('images') WHERE name='manifest_json'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(columns, 0);
    drop(db);
    drop(workspace);
    fs::remove_file(&path).unwrap();
    let workspace = Workspace::open(&root).unwrap();
    assert!(workspace.image(&image.image_id).is_err());
    assert_eq!(Workspace::list_images(&root).unwrap(), vec![second]);
    drop(workspace);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn configuration_commit_is_atomic_and_rebuilds_profile_cache() {
    let root = temp_root("configuration-atomic");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let id = Id::new("instance", "configured01").unwrap();
    let staged = root.join("staging/configured01");
    fs::create_dir_all(&staged).unwrap();
    let original = serde_json::json!({"id":"profile01", "memory":"1G"});
    fs::write(
        staged.join("profile.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    workspace
        .publish_prepared_instance(
            id.clone(),
            image.image_id,
            Id::new("profile", "profile01").unwrap(),
            &staged,
            "{}",
            false,
        )
        .unwrap();
    let revision = workspace
        .instance_document(&id)
        .unwrap()
        .revision()
        .unwrap();
    let changed = serde_json::json!({"id":"profile01", "memory":"2G"});
    // A stale editor must not replace the file, even after a direct disk edit.
    let path = workspace.instance_document_path(&id).unwrap();
    let original_bytes = fs::read(&path).unwrap();
    let mut edited: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
    edited["profile"]["memory"] = "3G".into();
    fs::write(&path, serde_json::to_vec(&edited).unwrap()).unwrap();
    assert!(
        workspace
            .replace_instance_configuration(&id, revision, "{}", false, Some(&changed))
            .is_err()
    );
    assert_eq!(
        workspace.instance_profile(&id).unwrap().unwrap()["memory"],
        "3G"
    );
    fs::write(&path, original_bytes).unwrap();
    workspace
        .replace_instance_configuration(&id, revision, "{\"changed\":true}", false, Some(&changed))
        .unwrap();
    assert!(
        workspace
            .replace_instance_configuration(&id, revision, "{}", false, None)
            .is_err()
    );
    fs::write(
        root.join("instances/configured01/profile.json"),
        b"interrupted cache write",
    )
    .unwrap();
    drop(workspace);
    let reopened = Workspace::open(&root).unwrap();
    reopened.materialize_instance_profile(&id).unwrap();
    assert_eq!(
        reopened.instance_profile(&id).unwrap(),
        Some(changed.clone())
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(root.join("instances/configured01/profile.json")).unwrap()
        )
        .unwrap(),
        changed
    );
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn deletion_rolls_back_metadata_and_resumes_filesystem_cleanup() {
    let root = temp_root("deletion-atomic");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let id = Id::new("instance", "delete01").unwrap();
    workspace
        .create_instance(
            id.clone(),
            image.image_id,
            Id::new("profile", "profile01").unwrap(),
        )
        .unwrap();
    workspace.save_instance_launch(&id, "{}", false).unwrap();
    let db = rusqlite::Connection::open(root.join("metadata.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_delete BEFORE DELETE ON instances BEGIN SELECT RAISE(ABORT, 'injected delete failure'); END;").unwrap();
    assert!(workspace.remove_instance(&id).is_err());
    assert!(workspace.instance(&id).is_ok());
    assert!(workspace.instance_launch(&id).unwrap().is_some());
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pending_instance_deletions",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER reject_delete").unwrap();
    let directory = root.join("instances/delete01");
    fs::rename(&directory, root.join("staging/deleted-files")).unwrap();
    fs::write(&directory, b"simulate inaccessible instance directory").unwrap();
    assert!(workspace.remove_instance(&id).is_err());
    assert!(workspace.instance(&id).is_err());
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pending_instance_deletions",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    fs::remove_file(&directory).unwrap();
    fs::rename(root.join("staging/deleted-files"), &directory).unwrap();
    drop(workspace);
    let reopened = Workspace::open(&root).unwrap();
    assert!(!directory.exists());
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM pending_instance_deletions",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn instance_documents_export_legacy_configuration_once_without_fallback() {
    let root = temp_root("instance-document-migration");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let id = Id::new("instance", "legacy01").unwrap();
    workspace
        .create_instance(
            id.clone(),
            image.image_id,
            Id::new("profile", "template01").unwrap(),
        )
        .unwrap();
    let path = workspace.instance_document_path(&id).unwrap();
    fs::remove_file(&path).unwrap();
    let db = rusqlite::Connection::open(root.join("metadata.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE instance_launch(instance_id TEXT PRIMARY KEY, plan_json TEXT, auto_remove INTEGER); CREATE TABLE instance_configuration(instance_id TEXT PRIMARY KEY, profile_json TEXT); INSERT INTO instance_launch VALUES ('legacy01', '{\"argv\":[\"original\"]}', 0); INSERT INTO instance_configuration VALUES ('legacy01', '{\"id\":\"template01\",\"memory\":\"4G\"}');").unwrap();
    drop(workspace);
    let workspace = Workspace::open(&root).unwrap();
    let document = workspace.instance_document(&id).unwrap();
    assert_eq!(document.profile.as_ref().unwrap()["memory"], "4G");
    assert_eq!(
        document.launch_plan.as_ref().unwrap()["argv"][0],
        "original"
    );
    assert_eq!(db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('instance_launch', 'instance_configuration')", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    // YAML is equally authoritative, including after restarting the daemon.
    let yaml = path.with_extension("yaml");
    let mut edited = document;
    edited.launch_plan.as_mut().unwrap()["argv"][0] = "edited".into();
    fs::write(&yaml, serde_yaml::to_string(&edited).unwrap()).unwrap();
    assert!(
        workspace
            .instance_document(&id)
            .unwrap_err()
            .to_string()
            .contains("multiple")
    );
    fs::remove_file(&path).unwrap();
    drop(workspace);
    let workspace = Workspace::open(&root).unwrap();
    assert!(
        workspace
            .instance_launch(&id)
            .unwrap()
            .unwrap()
            .0
            .contains("edited")
    );
    let document = workspace.instance_document(&id).unwrap();
    workspace
        .replace_instance_configuration(
            &id,
            document.revision().unwrap(),
            "{\"argv\":[\"updated-yaml\"]}",
            false,
            document.profile.as_ref(),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(fs::read_to_string(&yaml).unwrap().contains("updated-yaml"));
    let mut document = workspace.instance_document(&id).unwrap();
    document.profile = None;
    fs::write(&yaml, serde_yaml::to_string(&document).unwrap()).unwrap();
    workspace.materialize_instance_profile(&id).unwrap();
    assert!(!root.join("instances/legacy01/profile.json").exists());
    fs::write(&yaml, b"invalid: [").unwrap();
    assert!(workspace.instance_launch(&id).is_err());
    fs::remove_file(&yaml).unwrap();
    drop(workspace);
    let workspace = Workspace::open(&root).unwrap();
    assert!(workspace.instance_launch(&id).is_err());
    assert!(!path.exists());
    drop(workspace);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_instance_export_preserves_legacy_configuration() {
    let root = temp_root("instance-document-export-failure");
    let workspace = Workspace::open(&root).unwrap();
    let image = manifest();
    workspace.register_image(&image).unwrap();
    let id = Id::new("instance", "legacy01").unwrap();
    workspace
        .create_instance(
            id.clone(),
            image.image_id,
            Id::new("profile", "template01").unwrap(),
        )
        .unwrap();
    let path = workspace.instance_document_path(&id).unwrap();
    fs::write(&path, b"invalid JSON").unwrap();
    let db = rusqlite::Connection::open(root.join("metadata.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE instance_launch(instance_id TEXT PRIMARY KEY, plan_json TEXT, auto_remove INTEGER); INSERT INTO instance_launch VALUES ('legacy01', '{}', 0);").unwrap();
    drop(workspace);
    assert!(Workspace::open(&root).is_err());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM instance_launch", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(fs::read(path).unwrap(), b"invalid JSON");
    fs::remove_dir_all(root).unwrap();
}
