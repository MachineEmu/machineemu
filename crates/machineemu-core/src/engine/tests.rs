use super::*;
use super::{capabilities::*, plan::*};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[test]
fn memory_is_qemu_compatible() {
    assert_eq!(memory(&Value::from("8GiB")).unwrap(), "8G");
    assert!(memory(&Value::from("8GB")).is_err());
}

#[test]
fn console_uart_creates_interactive_serial_socket() {
    let mut argv = Vec::new();
    append_devices(
        &mut argv,
        Some(&serde_json::json!({"vnc": true})),
        Some(&serde_json::json!({"uart": true})),
        Path::new("/tmp/machineemu-test-instance"),
    )
    .unwrap();
    assert!(argv.iter().any(|arg| arg.contains("serial.sock")));
    assert!(
        argv.windows(2)
            .any(|args| args == ["-serial", "chardev:uart0"])
    );
}

#[test]
fn h264_plan_uses_dbus_display_and_usb_tablet() {
    let mut argv = Vec::new();
    append_devices(
        &mut argv,
        Some(&serde_json::json!({"vnc": false, "h264": true, "usb_tablet": true, "video": {"type":"virtio-vga-gl"}})),
        None,
        Path::new("/tmp/machineemu-test-instance"),
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|args| args == ["-display", "dbus,p2p=on,gl=on"])
    );
    assert!(
        argv.windows(2)
            .any(|args| args == ["-device", "virtio-vga-gl,id=me-video"])
    );
    assert!(
        argv.windows(2)
            .any(|args| args == ["-device", "qemu-xhci,id=usb"])
    );
    assert!(
        argv.windows(2)
            .any(|args| args == ["-device", "usb-tablet,bus=usb.0"])
    );
    assert!(!argv.iter().any(|arg| arg == "-vnc"));
    let mut invalid = Vec::new();
    assert!(
        append_devices(
            &mut invalid,
            Some(&serde_json::json!({"h264":true})),
            None,
            Path::new("/tmp/machineemu-test-instance")
        )
        .is_err()
    );
}

#[test]
fn h264_gpu_plan_uses_dbus_display_and_usb_tablet() {
    let mut argv = Vec::new();
    append_devices(
        &mut argv,
        Some(&serde_json::json!({"vnc": false, "h264": true, "usb_tablet": true, "video": {"type":"virtio-gpu-gl"}})),
        None,
        Path::new("/tmp/machineemu-test-instance"),
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|args| args == ["-display", "dbus,p2p=on,gl=on"])
    );
    assert!(
        argv.windows(2)
            .any(|args| args == ["-device", "virtio-gpu-gl,id=me-video"])
    );
    assert!(
        argv.windows(2)
            .any(|args| args == ["-device", "qemu-xhci,id=usb"])
    );
    assert!(
        argv.windows(2)
            .any(|args| args == ["-device", "usb-tablet,bus=usb.0"])
    );
    assert!(!argv.iter().any(|arg| arg == "-vnc"));
    let mut invalid = Vec::new();
    assert!(
        append_devices(
            &mut invalid,
            Some(&serde_json::json!({"h264":true})),
            None,
            Path::new("/tmp/machineemu-test-instance")
        )
        .is_err()
    );
}

#[test]
fn gl_video_conflicts_with_vnc_for_both_gpu_names() {
    for model in ["virtio-vga-gl", "virtio-gpu-gl"] {
        for vnc in [serde_json::json!(true), serde_json::json!({"port":"auto"})] {
            let devices = serde_json::json!({"vnc":vnc,"h264":true,"video":{"type":model}});
            let error = append_devices(
                &mut Vec::new(),
                Some(&devices),
                None,
                Path::new("/tmp/machineemu-test-instance"),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("vnc conflicts with GL video"),
                "{error}"
            );
        }
    }
}

#[test]
fn usb_pointers_share_one_xhci_controller() {
    for (mouse, tablet, expected) in [
        (true, false, vec!["usb-mouse,bus=usb.0"]),
        (false, true, vec!["usb-tablet,bus=usb.0"]),
        (
            true,
            true,
            vec!["usb-tablet,bus=usb.0", "usb-mouse,bus=usb.0"],
        ),
    ] {
        let mut argv = Vec::new();
        append_devices(
            &mut argv,
            Some(&serde_json::json!({"usb_mouse":mouse,"usb_tablet":tablet})),
            None,
            Path::new("/tmp/machineemu-test-instance"),
        )
        .unwrap();
        let devices: Vec<_> = argv
            .windows(2)
            .filter(|pair| pair[0] == "-device")
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(devices[0], "qemu-xhci,id=usb");
        assert_eq!(&devices[1..], expected);
    }
    assert!(
        append_devices(
            &mut Vec::new(),
            Some(&serde_json::json!({"usb_mouse":"yes"})),
            None,
            Path::new("/tmp/machineemu-test-instance")
        )
        .is_err()
    );
}

#[test]
fn dbus_and_spice_audio_plan_distinct_transports() {
    let runtime = Path::new("/tmp/machineemu-audio");
    let devices = serde_json::json!({"vnc":true});
    let mut dbus = vec!["-display".into(), "vnc=:0".into()];
    append_audio(
        &mut dbus,
        Some(&serde_json::json!({"model":"ich9","backend":{"type":"dbus","id":"pc-audio"}})),
        Some(&devices),
        runtime,
    )
    .unwrap();
    assert!(
        dbus.windows(2)
            .any(|pair| pair == ["-display", "dbus,p2p=on,audiodev=pc-audio"])
    );
    assert!(
        dbus.windows(2)
            .any(|pair| pair == ["-vnc", "unix:/tmp/machineemu-audio/sockets/vnc.sock"])
    );
    assert!(
        dbus.windows(2)
            .any(|pair| pair == ["-audiodev", "dbus,id=pc-audio"])
    );
    let mut spice = vec!["-display".into(), "vnc=:0".into()];
    append_audio(
        &mut spice,
        Some(&serde_json::json!({"model":"ich9","backend":{"type":"spice"}})),
        Some(&devices),
        runtime,
    )
    .unwrap();
    assert!(spice.windows(2).any(|pair| pair
        == [
            "-spice",
            "disable-ticketing=on,unix=on,addr=/tmp/machineemu-audio/sockets/spice.sock"
        ]));
    assert!(
        spice
            .windows(2)
            .any(|pair| pair == ["-audiodev", "spice,id=pc-audio"])
    );
}

#[test]
fn disk_sizes_normalize_for_qemu_img() {
    assert_eq!(disk_size("64GiB").unwrap(), "64G");
    assert_eq!(disk_size("1.5T").unwrap(), "1.5T");
    assert!(disk_size("64").is_err());
}
#[test]
fn qemu_values_are_not_shell_commands() {
    assert_eq!(
        machine_value("pc", Some(&Value::Bool(true)), None).unwrap(),
        "pc,smm=on"
    );
}

#[test]
fn qemu_help_parser_ignores_descriptions_and_keeps_order() {
    let help = "Supported machines are:\npc-q35-10.2  Q35 machine\npc  Standard PC\n\n";
    assert_eq!(parse_named_help(help), vec!["pc-q35-10.2", "pc"]);
}

#[test]
fn qemu_help_parser_handles_option_sections() {
    let help =
        "-cpu cpu select CPU\n\nx86_64  host CPU\nmax  maximum CPU\n\nAccelerators:\nkvm\ntcg\n";
    assert_eq!(parse_named_help(help), vec!["x86_64", "max", "kvm", "tcg"]);
}

#[test]
fn qemu_list_parser_stops_before_display_prose() {
    let help = "Available display backend types:\nnone\ngtk\nsdl\n\nSome display backends support suboptions, which can be set with\n";
    assert_eq!(parse_indented_list(help), vec!["none", "gtk", "sdl"]);
}

#[test]
fn profile_validation_rejects_choices_missing_from_target_qemu() {
    let profile = serde_json::json!({
        "schema_version": 1,
        "id": "fixture",
        "machine": "pc-q35-10.2",
        "cpu": "max",
        "resources": {"accelerator": "kvm"}
    });
    let options = QemuOptions {
        executable: PathBuf::from("qemu-system-x86_64"),
        version: Some("QEMU emulator version 10.2.4".into()),
        machines: vec!["pc".into()],
        cpus: vec!["max".into()],
        accelerators: vec!["tcg".into()],
        devices: vec![],
        display_backends: vec![],
        chardev_backends: vec![],
        tpm_backends: vec![],
        audio_drivers: vec![],
        machine_properties: vec![],
        analysis_machine_properties: vec![],
        device_properties: vec![],
    };
    let error = validate_profile_against_qemu(&profile, &options).unwrap_err();
    assert!(error.to_string().contains("machine"));
}

#[test]
fn profile_validation_checks_optional_qemu_version_pin() {
    let profile = serde_json::json!({
        "schema_version": 1,
        "id": "fixture",
        "engine": {"track": "fixture", "version": "10.2.4"},
        "machine": "pc",
    });
    let options = QemuOptions {
        executable: PathBuf::from("qemu"),
        version: Some("QEMU emulator version 10.2.3".into()),
        machines: vec!["pc".into()],
        cpus: vec![],
        accelerators: vec![],
        devices: vec![],
        display_backends: vec![],
        chardev_backends: vec![],
        tpm_backends: vec![],
        audio_drivers: vec![],
        machine_properties: vec![],
        analysis_machine_properties: vec![],
        device_properties: vec![],
    };
    let error = validate_profile_against_qemu(&profile, &options).unwrap_err();
    assert!(error.to_string().contains("10.2.4"));
}

#[test]
fn profile_validation_checks_declared_devices() {
    let profile = serde_json::json!({
        "schema_version": 1,
        "id": "fixture",
        "machine": "q35",
        "resources": {"accelerator": "kvm"},
        "devices": {"nic": "missing-nic"},
        "tpm": {"model": "tpm-crb"}
    });
    let options = QemuOptions {
        executable: PathBuf::from("qemu-system-x86_64"),
        version: None,
        machines: vec!["q35".into()],
        cpus: vec![],
        accelerators: vec!["kvm".into()],
        devices: vec!["tpm-crb".into()],
        display_backends: vec![],
        chardev_backends: vec![],
        tpm_backends: vec![],
        audio_drivers: vec![],
        machine_properties: vec![],
        analysis_machine_properties: vec![],
        device_properties: vec![],
    };
    let error = validate_profile_against_qemu(&profile, &options).unwrap_err();
    assert!(error.to_string().contains("missing-nic"));
}

#[test]
fn profile_validation_requires_vsock_from_the_selected_qemu() {
    let profile = serde_json::json!({
        "schema_version":1,"id":"vsock","machine":"q35","devices":{"vsock":true}
    });
    let options = QemuOptions {
        executable: PathBuf::from("qemu"),
        version: None,
        machines: vec!["q35".into()],
        cpus: vec![],
        accelerators: vec![],
        devices: vec![],
        display_backends: vec![],
        chardev_backends: vec![],
        tpm_backends: vec![],
        audio_drivers: vec![],
        machine_properties: vec![],
        analysis_machine_properties: vec![],
        device_properties: vec![],
    };
    assert!(
        validate_profile_against_qemu(&profile, &options)
            .unwrap_err()
            .to_string()
            .contains("vhost-vsock-pci")
    );
    let mut supported = options;
    supported.devices.push("vhost-vsock-pci".into());
    assert!(validate_profile_against_qemu(&profile, &supported).is_ok());
}

#[test]
fn legacy_board_validation_checks_devices_and_analysis_properties() {
    let board = serde_json::json!({
        "os": {"type": {"machine": "q35"}},
        "video": {"model": {"type": "missing-video"}},
        "tpm": {"model": "missing-tpm"},
        "analysis": {"enabled": true}
    });
    let options = QemuOptions {
        executable: PathBuf::from("qemu"),
        version: None,
        machines: vec!["q35".into()],
        cpus: vec![],
        accelerators: vec![],
        devices: vec![],
        display_backends: vec![],
        chardev_backends: vec![],
        tpm_backends: vec![],
        audio_drivers: vec![],
        machine_properties: vec![],
        analysis_machine_properties: vec![],
        device_properties: vec![],
    };
    let report = validate_legacy_board(board.as_object().unwrap(), &options, Some("q35".into()));
    assert!(!report.valid);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("video device"))
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("analysis property"))
    );
}

#[cfg(unix)]
#[test]
fn inspect_qemu_executes_the_selected_binary_for_each_capability() {
    use std::os::unix::fs::PermissionsExt;

    let root = test_root("inspect");
    fs::create_dir_all(&root).unwrap();
    let executable = root.join("qemu");
    fs::write(
        &executable,
        r#"#!/bin/sh
case "$1:$2" in
  --version:) echo 'QEMU emulator version 10.2.4';;
  -machine:help) printf 'Supported machines are:\npc-q35-10.2\npc\n';;
  -machine:pc-q35-10.2,help) printf 'smm=on\naccel=tcg\n';;
  -cpu:help) printf 'x86_64\nmax\n';;
  -accel:help) printf 'kvm\ntcg\n';;
  -device:help) printf 'name "virtio-net-pci", bus PCI\nname "ich9-ahci", bus PCI\n';;
  -device:virtio-net-pci,help) printf 'mac=address\nnetdev=id\n';;
  -display:help) printf 'gtk\nvnc\nnone\n';;
  -chardev:help) printf 'socket\nnull\n';;
  -tpmdev:help) printf 'emulator\npassthrough\n';;
  -audiodev:help) printf 'none\npa\n';;
  *) exit 1;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let options = inspect_qemu(&executable, Some("pc-q35-10.2"), Some("virtio-net-pci")).unwrap();
    assert_eq!(
        options.version.as_deref(),
        Some("QEMU emulator version 10.2.4")
    );
    assert_eq!(options.machines, vec!["pc-q35-10.2", "pc"]);
    assert_eq!(options.cpus, vec!["x86_64", "max"]);
    assert_eq!(options.accelerators, vec!["kvm", "tcg"]);
    assert_eq!(options.devices, vec!["virtio-net-pci", "ich9-ahci"]);
    assert_eq!(options.display_backends, vec!["gtk", "vnc", "none"]);
    assert_eq!(options.chardev_backends, vec!["socket", "null"]);
    assert_eq!(options.tpm_backends, vec!["emulator", "passthrough"]);
    assert_eq!(options.audio_drivers, vec!["none", "pa"]);
    assert_eq!(options.machine_properties, vec!["smm", "accel"]);
    assert_eq!(options.analysis_machine_properties, Vec::<String>::new());
    assert_eq!(options.device_properties, vec!["mac", "netdev"]);
    fs::remove_dir_all(root).unwrap();
}

fn tpm_fixture(root: &Path) -> (Value, Value, PathBuf) {
    let bundle = root.join("bundle");
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::write(bundle.join("bin/qemu"), b"fixture").unwrap();
    fs::write(bundle.join("manifest.json"), r#"{
          "schema_version": 1, "track_id": "fixture", "build_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "source_revision": "test", "targets": ["x86_64-softmmu"], "executables": {"x86_64-softmmu": "bin/qemu"}, "dirty_source": false
        }"#).unwrap();
    let release = serde_json::json!({"schema_version":1,"engines":{"fixture":{"manifest":"manifest.json","build_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}});
    let profile = serde_json::json!({"schema_version":1,"id":"fixture","engine":{"track":"fixture"},"machine":"pc-q35-10.2","resources":{"memory":"512MiB","vcpus":2},"network":{"type":"disabled"},"tpm":{"model":"tpm-crb","backend":{"type":"emulator","version":"2.0"}}});
    (profile, release, bundle)
}

#[test]
fn tpm_helper_defaults_to_swtpm_on_path() {
    let root = test_root("tpm-default");
    let (profile, release, bundle) = tpm_fixture(&root);
    let plan = build_plan(PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: None,
        target: "x86_64-softmmu".into(),
        runtime_dir: root.join("runtime"),
        state_dir: None,
        seed: None,
        swtpm: None,
        bridge_helper: None,
        mac: None,
        instance: None,
    })
    .unwrap();
    let helper = plan.helper_argv.expect("a TPM profile needs its helper");
    assert_eq!(helper[0], "swtpm");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tpm_helper_uses_the_configured_swtpm() {
    let root = test_root("tpm-configured");
    let (profile, release, bundle) = tpm_fixture(&root);
    let plan = build_plan(PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: None,
        target: "x86_64-softmmu".into(),
        runtime_dir: root.join("runtime"),
        state_dir: None,
        seed: None,
        swtpm: Some(PathBuf::from("/nix/store/fixture/bin/swtpm")),
        bridge_helper: None,
        mac: None,
        instance: None,
    })
    .unwrap();
    let helper = plan.helper_argv.expect("a TPM profile needs its helper");
    assert_eq!(helper[0], "/nix/store/fixture/bin/swtpm");
    assert!(helper.contains(&"socket".to_string()));
    fs::remove_dir_all(root).unwrap();
}

fn nic_argument(plan: &LaunchPlan) -> String {
    plan.argv
        .iter()
        .find(|argument| argument.starts_with("virtio-net-pci,"))
        .expect("the fixture profile has a NIC")
        .clone()
}

fn plan_with(root: &Path, mac: Option<&str>, instance: Option<&str>) -> Result<LaunchPlan, Error> {
    let (mut profile, release, bundle) = tpm_fixture(root);
    profile["network"] = serde_json::json!({"type":"bridge","bridge":"br0"});
    build_plan(PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: None,
        target: "x86_64-softmmu".into(),
        runtime_dir: root.join("runtime"),
        state_dir: None,
        seed: None,
        swtpm: None,
        bridge_helper: None,
        mac: mac.map(str::to_owned),
        instance: instance.map(str::to_owned),
    })
}

#[test]
fn a_derived_address_is_stable_per_instance_and_differs_between_them() {
    let root = test_root("mac-derived");
    let lab01 = nic_argument(&plan_with(&root, None, Some("lab01")).unwrap());
    let again = nic_argument(&plan_with(&root, None, Some("lab01")).unwrap());
    let lab02 = nic_argument(&plan_with(&root, None, Some("lab02")).unwrap());
    assert_eq!(lab01, again, "an instance keeps its address");
    assert_ne!(lab01, lab02, "two instances must not share one address");
    assert!(
        lab01.contains(",mac=52:54:00:"),
        "unexpected NIC argument: {lab01}"
    );
    assert_eq!(derive_mac("lab01").len(), 17);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_explicit_address_outranks_the_derived_one() {
    let root = test_root("mac-explicit");
    let plan = plan_with(&root, Some("52:54:00:AB:CD:EF"), Some("lab01")).unwrap();
    assert!(
        nic_argument(&plan).ends_with(",mac=52:54:00:ab:cd:ef"),
        "unexpected NIC argument: {}",
        nic_argument(&plan)
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_malformed_or_multicast_address_is_rejected() {
    let root = test_root("mac-invalid");
    for value in ["52:54:00:ab:cd", "52-54-00-ab-cd-ef", "zz:54:00:ab:cd:ef"] {
        assert!(
            plan_with(&root, Some(value), None).is_err(),
            "{value:?} must not be accepted"
        );
    }
    let multicast = plan_with(&root, Some("53:54:00:ab:cd:ef"), None).unwrap_err();
    assert!(
        multicast.to_string().contains("multicast"),
        "unexpected error: {multicast}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn without_an_instance_or_address_qemu_keeps_its_own_default() {
    let root = test_root("mac-absent");
    let plan = plan_with(&root, None, None).unwrap();
    assert_eq!(nic_argument(&plan), "virtio-net-pci,netdev=net0");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bridge_networking_names_the_configured_helper() {
    let root = test_root("bridge-helper");
    let (mut profile, release, bundle) = tpm_fixture(&root);
    profile["network"] = serde_json::json!({"type":"bridge","bridge":"br0"});
    let plan = build_plan(PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: None,
        target: "x86_64-softmmu".into(),
        runtime_dir: root.join("runtime"),
        state_dir: None,
        seed: None,
        swtpm: None,
        bridge_helper: Some(PathBuf::from("/run/wrappers/bin/qemu-bridge-helper")),
        mac: None,
        instance: None,
    })
    .unwrap();
    assert!(
        plan.argv.contains(
            &"bridge,id=net0,br=br0,helper=/run/wrappers/bin/qemu-bridge-helper".to_string()
        ),
        "unexpected argv: {:?}",
        plan.argv
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bridge_networking_without_a_helper_leaves_qemu_its_default() {
    let root = test_root("bridge-default");
    let (mut profile, release, bundle) = tpm_fixture(&root);
    profile["network"] = serde_json::json!({"type":"bridge","bridge":"br0"});
    let plan = build_plan(PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: None,
        target: "x86_64-softmmu".into(),
        runtime_dir: root.join("runtime"),
        state_dir: None,
        seed: None,
        swtpm: None,
        bridge_helper: None,
        mac: None,
        instance: None,
    })
    .unwrap();
    assert!(
        plan.argv.contains(&"bridge,id=net0,br=br0".to_string()),
        "unexpected argv: {:?}",
        plan.argv
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn plan_resolves_a_fixture_engine_without_writing_runtime_state() {
    let root = test_root("plan");
    let bundle = root.join("bundle");
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::write(bundle.join("bin/qemu"), b"fixture").unwrap();
    fs::write(bundle.join("manifest.json"), r#"{
          "schema_version": 1, "track_id": "fixture", "build_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "source_revision": "test", "targets": ["x86_64-softmmu"], "executables": {"x86_64-softmmu": "bin/qemu"}, "dirty_source": false
        }"#).unwrap();
    let release = serde_json::json!({"schema_version":1,"engines":{"fixture":{"manifest":"manifest.json","build_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}});
    let profile = serde_json::json!({"schema_version":1,"id":"fixture","engine":{"track":"fixture"},"machine":"pc-q35-10.2","resources":{"memory":"512MiB","vcpus":2},"network":{"type":"disabled"}});
    let runtime = root.join("runtime");
    let plan = build_plan(PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: None,
        target: "x86_64-softmmu".into(),
        runtime_dir: runtime.clone(),
        state_dir: None,
        seed: None,
        swtpm: None,
        bridge_helper: None,
        mac: None,
        instance: None,
    })
    .unwrap();
    assert!(plan.argv[0].ends_with("bin/qemu"));
    assert!(plan.argv.windows(2).any(|pair| pair == ["-nic", "none"]));
    assert!(
        !runtime.exists(),
        "planning must not create runtime directories"
    );
    fs::remove_dir_all(root).unwrap();
}

fn bundled_input(root: &Path, name: &str) -> PlanInput {
    let (_, release, bundle) = tpm_fixture(root);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../profiles")
        .join(format!("{name}.json"));
    let mut profile: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    profile["engine"] = serde_json::json!({"track":"fixture"});
    let assets = root.join("assets");
    let digest = "b".repeat(64);
    fs::create_dir_all(assets.join("sha256")).unwrap();
    fs::write(assets.join("sha256").join(&digest), b"fixture").unwrap();
    let references = profile["external_assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|asset| {
            (
                asset["id"].as_str().unwrap().to_owned(),
                Value::from(format!("sha256:{digest}")),
            )
        })
        .collect();
    profile["assets"] = Value::Object(references);
    PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: Some(assets),
        target: "x86_64-softmmu".into(),
        runtime_dir: root.join("runtime"),
        state_dir: Some(root.join("state")),
        seed: None,
        swtpm: None,
        bridge_helper: None,
        mac: None,
        instance: Some("fixture01".into()),
    }
}

#[test]
fn win11_profile_renders_boot_storage_and_pointer_devices() {
    let root = test_root("win11-profile");
    let input = bundled_input(&root, "win11-dev");
    let plan = build_plan(input).unwrap();
    assert!(
        plan.argv
            .windows(2)
            .any(|pair| pair == ["-boot", "order=c,menu=off"])
    );
    assert!(
        plan.argv
            .iter()
            .any(|arg| arg.contains("id=pc-disk") && arg.contains("discard=unmap"))
    );
    assert!(
        plan.argv
            .windows(2)
            .any(|pair| pair == ["-device", "qemu-xhci,id=usb"])
    );
    assert!(
        plan.argv
            .windows(2)
            .any(|pair| pair == ["-device", "usb-tablet,bus=usb.0"])
    );
    assert!(
        plan.argv
            .iter()
            .any(|arg| arg.contains("org.qemu.guest_agent.0"))
    );
    assert_eq!(
        plan.preparation.disk_overlay.unwrap().path,
        root.join("state/overlay.qcow2")
    );
    assert_eq!(
        plan.preparation.nvram.unwrap().path,
        root.join("state/OVMF_VARS.fd")
    );
    assert!(plan.helper_argv.is_some());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bundled_profiles_render_vsock_and_boot_settings() {
    let root = test_root("bundled-validation");
    let debian = bundled_input(&root, "debian13-cloud");
    let plan = build_plan(debian.clone()).unwrap();
    assert!(
        plan.argv
            .windows(2)
            .any(|pair| pair == ["-boot", "order=c,menu=off"])
    );
    assert!(plan.argv.iter().any(|arg| arg.contains("discard=unmap")));
    let cid = plan.manifest["vsock"]["guest_cid"].as_u64().unwrap();
    assert!((3..=u64::from(u32::MAX)).contains(&cid));
    assert!(plan.argv.windows(2).any(|pair| pair[0] == "-device"
        && pair[1] == format!("vhost-vsock-pci,id=machineemu-vsock,guest-cid={cid}")));
    assert_eq!(
        build_plan(debian.clone()).unwrap().manifest["vsock"]["guest_cid"],
        cid
    );
    let mut other = debian.clone();
    other.instance = Some("fixture02".into());
    assert_ne!(
        build_plan(other).unwrap().manifest["vsock"]["guest_cid"],
        cid
    );
    let mut disabled = debian.clone();
    disabled.profile["devices"]["vsock"] = Value::Bool(false);
    let plan = build_plan(disabled).unwrap();
    assert!(plan.manifest.get("vsock").is_none());
    assert!(
        !plan
            .argv
            .iter()
            .any(|arg| arg.starts_with("vhost-vsock-pci,"))
    );
    let mut invalid = debian;
    invalid.profile["devices"]["vsock"] = Value::from("yes");
    assert!(
        build_plan(invalid)
            .unwrap_err()
            .to_string()
            .contains("profile.devices.vsock must be a boolean")
    );

    let mut analysis = bundled_input(&root, "malware-analysis-x64");
    let splash = root.join("boot,splash.bmp");
    fs::write(&splash, b"fixture").unwrap();
    analysis.profile["boot"]["splash"] = Value::from(splash.to_str().unwrap());
    let plan = build_plan(analysis.clone()).unwrap();
    let expected = format!(
        "order=c,menu=on,splash={},splash-time=2500",
        splash.to_string_lossy().replace(',', ",,")
    );
    assert!(
        plan.argv
            .windows(2)
            .any(|pair| pair[0] == "-boot" && pair[1] == expected)
    );
    analysis.profile["boot"]
        .as_object_mut()
        .unwrap()
        .remove("splash");
    let without_splash = build_plan(analysis.clone()).unwrap();
    assert!(
        without_splash
            .argv
            .windows(2)
            .any(|pair| pair == ["-boot", "order=c,menu=on,splash-time=2500"])
    );
    for timeout in [
        serde_json::json!(-1),
        serde_json::json!("2500"),
        serde_json::json!(4294967296_u64),
    ] {
        let mut bad = analysis.clone();
        bad.profile["boot"]["timeout"] = timeout;
        assert!(
            build_plan(bad)
                .unwrap_err()
                .to_string()
                .contains("profile.boot.timeout")
        );
    }
    analysis.profile["boot"]["splash"] = Value::from(root.join("missing.bmp").to_str().unwrap());
    assert!(
        build_plan(analysis)
            .unwrap_err()
            .to_string()
            .contains("profile.boot.splash")
    );
    assert!(plan.argv.windows(2).any(|pair| pair == ["-vga", "none"]));
    assert!(
        plan.argv
            .iter()
            .any(|arg| arg.contains("model=SATA SSD,serial=ANSSD-0"))
    );
    assert!(
        plan.argv
            .iter()
            .any(|arg| arg.contains("analysis-profile=on"))
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn planner_rejects_silent_hardware_mismatches() {
    let root = test_root("planner-mismatches");
    let input = bundled_input(&root, "win11-dev");
    let mut vga = input.clone();
    vga.profile["devices"]["video"] = serde_json::json!({"type":"vga"});
    assert!(
        build_plan(vga)
            .unwrap()
            .argv
            .windows(2)
            .any(|pair| pair == ["-vga", "std"])
    );

    for (pointer, value, expected) in [
        (
            "/devices/wifi_hwsim",
            serde_json::json!(true),
            "profile.devices.wifi_hwsim",
        ),
        (
            "/boot/unknown",
            serde_json::json!(true),
            "profile.boot.unknown",
        ),
        (
            "/storage/disk/wwn",
            serde_json::json!("123"),
            "profile.storage.disk.wwn",
        ),
        (
            "/tpm/backend/version",
            serde_json::json!("1.2"),
            "profile.tpm.backend.version",
        ),
    ] {
        let mut broken = input.clone();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        broken
            .profile
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.into(), value);
        assert!(
            build_plan(broken)
                .unwrap_err()
                .to_string()
                .contains(expected),
            "{pointer}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

fn test_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("machineemu-plan-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

#[test]
fn udm_pro_uses_board_cpu_and_disposable_native_storage() {
    let root = test_root("udm-pro");
    let (_, release, bundle) = tpm_fixture(&root);
    let mut profile: Value =
        serde_json::from_str(include_str!("../../../../profiles/udm-pro.json")).unwrap();
    profile["engine"] = serde_json::json!({"track":"fixture"});
    let assets = root.join("assets");
    fs::create_dir_all(assets.join("sha256")).unwrap();
    let digest = "b".repeat(64);
    fs::write(assets.join("sha256").join(&digest), b"fixture").unwrap();
    profile["assets"] = serde_json::json!({"kernel":format!("sha256:{digest}"), "initrd":format!("sha256:{digest}"), "boot":format!("sha256:{digest}"), "spi":format!("sha256:{digest}")});
    let input = PlanInput {
        profile,
        release_set: release,
        bundle_root: bundle,
        asset_root: Some(assets),
        target: "x86_64-softmmu".into(),
        runtime_dir: root.join("runtime"),
        state_dir: None,
        seed: None,
        swtpm: None,
        bridge_helper: None,
        mac: None,
        instance: Some("udm01".into()),
    };
    let plan = build_plan(input.clone()).unwrap();
    assert!(
        !plan
            .argv
            .iter()
            .any(|v| ["-cpu", "-dtb", "-device"].contains(&v.as_str()))
    );
    for flag in ["-kernel", "-initrd", "-append"] {
        assert!(plan.argv.iter().any(|v| v == flag));
    }
    for id in ["udm-boot", "udm-config"] {
        assert!(
            plan.argv
                .iter()
                .any(|v| v.contains(&format!("id={id},snapshot=on")))
        );
    }
    assert!(plan.preparation.disk_overlay.is_none());
    let mut missing = input.clone();
    missing.profile["assets"]
        .as_object_mut()
        .unwrap()
        .remove("spi");
    assert!(
        build_plan(missing)
            .unwrap_err()
            .to_string()
            .contains("not imported: spi")
    );
    let mut lab = input.clone();
    let assets = lab.profile["assets"].clone();
    lab.profile =
        serde_json::from_str(include_str!("../../../../profiles/udm-pro-lab.json")).unwrap();
    lab.profile["engine"] = serde_json::json!({"track":"fixture"});
    lab.profile["assets"] = assets;
    lab.bridge_helper = Some(PathBuf::from("/run/wrappers/bin/qemu-bridge-helper"));
    let plan = build_plan(lab.clone()).unwrap();
    let backends: Vec<_> = plan
        .argv
        .windows(2)
        .filter(|p| p[0] == "-netdev")
        .map(|p| p[1].as_str())
        .collect();
    assert_eq!(
        backends,
        vec![
            "hubport,id=udm-port0,hubid=0",
            "bridge,id=udm-port1,br=br0,helper=/run/wrappers/bin/qemu-bridge-helper",
            "bridge,id=udm-port2,br=br10,helper=/run/wrappers/bin/qemu-bridge-helper",
            "hubport,id=udm-port3,hubid=3",
        ]
    );
    let nics: Vec<_> = plan
        .argv
        .iter()
        .filter(|v| v.starts_with("nic,model=alpine-eth-pci"))
        .collect();
    assert_eq!(nics.len(), 4);
    let addresses: std::collections::BTreeSet<_> = nics
        .iter()
        .map(|v| v.split("macaddr=").nth(1).unwrap())
        .collect();
    assert_eq!(addresses.len(), 4);
    assert!(
        plan.argv
            .iter()
            .any(|v| v == "qemu-xhci,id=udm-usb,bus=pcie-external,addr=9")
    );
    assert!(
        plan.argv
            .iter()
            .any(|v| v == "unifi-lcm,bus=udm-usb.0,events=lcm-events,input=lcm-input,udm-pro=on")
    );
    let serials: Vec<_> = plan
        .argv
        .windows(2)
        .filter(|p| p[0] == "-serial")
        .map(|p| p[1].as_str())
        .collect();
    assert_eq!(serials.len(), 2);
    assert_eq!(serials[0], "chardev:uart0");
    assert!(plan.argv.iter().any(|v| v.starts_with("socket,id=uart0,")
        && v.contains("serial.sock")
        && v.contains("logfile=")
        && v.contains("serial.log")));
    assert_eq!(serials[1], "chardev:btuart");
    let mut bad = lab.clone();
    bad.profile["network"]["ports"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    assert!(
        build_plan(bad)
            .unwrap_err()
            .to_string()
            .contains("four ports")
    );
    let mut bad = lab.clone();
    bad.profile["network"]["ports"][1]["bridge"] = Value::from("br0,id=injected");
    assert!(
        build_plan(bad)
            .unwrap_err()
            .to_string()
            .contains("bridge name")
    );
    lab.profile["devices"]["serial"] = Value::Null;
    assert!(
        build_plan(lab)
            .unwrap_err()
            .to_string()
            .contains("profile.devices.serial")
    );
    let mut networked = input;
    networked.profile["network"] = serde_json::json!({"type":"user"});
    let plan = build_plan(networked).unwrap();
    assert!(
        plan.argv
            .iter()
            .any(|v| v.starts_with("user,model=alpine-eth-pci,mac="))
    );
    assert!(!plan.argv.iter().any(|v| v.contains("virtio-net")));
    fs::remove_dir_all(root).unwrap();
}
