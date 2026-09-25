use machineemu_core::{engine::Error, launch::LaunchSpec};
use std::{fs, io::Write, path::Path};

#[derive(Debug, Default, clap::Args)]
pub(super) struct HardwareArgs {
    /// VNC selection: profile, auto, off, or TCP port 5900-5999.
    #[arg(long)]
    pub vnc: Option<String>,
    /// VNC password (1-8 bytes); stored in an owner-only secret file.
    #[arg(long, conflicts_with_all = ["vnc_password_file", "h264"])]
    pub vnc_password: Option<String>,
    #[arg(long, conflicts_with = "h264")]
    pub vnc_password_file: Option<std::path::PathBuf>,
    /// Enable D-Bus H.264, virtio-vga-gl, and USB tablet; disable VNC.
    #[arg(long)]
    pub h264: bool,
    /// NIC slot 0: bridge:NAME, host (user networking), or off.
    #[arg(long, alias = "network0")]
    pub network: Option<String>,
    #[arg(long)]
    pub network1: Option<String>,
    #[arg(long)]
    pub network2: Option<String>,
    #[arg(long)]
    pub network3: Option<String>,
    /// Repeatable SLOT=PROTO:HOSTADDR:HOSTPORT-GUESTADDR:GUESTPORT.
    #[arg(long)]
    pub portfwd: Vec<String>,
    /// Attach an installer ISO, or off to eject the CLI-managed ISO.
    #[arg(long)]
    pub iso: Option<String>,
    /// Number of virtual CPUs.
    #[arg(long, alias = "cpu-count", value_parser = clap::value_parser!(u16).range(1..))]
    pub cpus: Option<u16>,
    /// Guest RAM, e.g. 2048MiB or 8GiB (bare numbers are MiB).
    #[arg(long)]
    pub memory: Option<String>,
}

fn invalid(s: impl Into<String>) -> Error {
    Error::Invalid(s.into())
}

// QEMU options with values are pairs, but standalone flags and the executable
// remain untouched. Only remove options owned by the requested hardware setting.
fn remove(argv: &mut Vec<String>, predicate: impl Fn(&str, &str) -> bool) {
    let mut i = 1;
    while i + 1 < argv.len() {
        if predicate(&argv[i], &argv[i + 1]) {
            argv.drain(i..i + 2);
        } else {
            i += 1;
        }
    }
}
fn field<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    value.split(',').find_map(|v| v.strip_prefix(name))
}
fn memory(value: &str) -> Result<String, Error> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| invalid("invalid --memory size"))?;
    if number == 0 {
        return Err(invalid("--memory must be positive"));
    }
    let unit = match unit.to_ascii_lowercase().as_str() {
        "" | "m" | "mb" | "mib" => "M",
        "g" | "gb" | "gib" => "G",
        "t" | "tb" | "tib" => "T",
        _ => return Err(invalid("--memory expects MiB, GiB, or TiB")),
    };
    Ok(format!("{number}{unit}"))
}
fn forwarding(value: &str) -> Result<(), Error> {
    let (host, guest) = value
        .split_once('-')
        .ok_or_else(|| invalid("invalid --portfwd rule"))?;
    let host: Vec<_> = host.split(':').collect();
    let guest: Vec<_> = guest.split(':').collect();
    if host.len() != 3 || guest.len() != 2 || !matches!(host[0], "tcp" | "udp") {
        return Err(invalid("--portfwd expects SLOT=tcp:127.0.0.1:2222-:22"));
    }
    for p in [host[2], guest[1]] {
        if p.parse::<u16>().ok().filter(|p| *p > 0).is_none() {
            return Err(invalid("invalid forwarding port"));
        }
    }
    for address in [host[1], guest[0]] {
        if !address.is_empty() && address.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(invalid("forwarding addresses must be IPv4"));
        }
    }
    Ok(())
}

impl HardwareArgs {
    pub fn has_updates(&self) -> bool {
        self.vnc.is_some()
            || self.vnc_password.is_some()
            || self.vnc_password_file.is_some()
            || self.h264
            || self.network.is_some()
            || self.network1.is_some()
            || self.network2.is_some()
            || self.network3.is_some()
            || !self.portfwd.is_empty()
            || self.iso.is_some()
            || self.cpus.is_some()
            || self.memory.is_some()
    }

    pub fn apply(
        &self,
        plan: &mut LaunchSpec,
        workspace: &Path,
        instance: &str,
    ) -> Result<(), Error> {
        // Work on a copy: invalid input cannot leave a partially edited plan.
        let mut next = plan.clone();
        let argv = &mut next.argv;
        let is_q35 = argv
            .windows(2)
            .any(|pair| pair[0] == "-machine" && pair[1].contains("q35"));
        if is_q35 {
            for port in [
                "pcie-root-port,id=pcie-root-port-iso,chassis=16,slot=16",
                "pcie-root-port,id=pcie-root-port-1,chassis=17,slot=17",
                "pcie-root-port,id=pcie-root-port-2,chassis=18,slot=18",
            ] {
                let id = field(port, "id=").expect("root port has an ID");
                if !argv
                    .windows(2)
                    .any(|pair| pair[0] == "-device" && field(&pair[1], "id=") == Some(id))
                {
                    argv.extend(["-device".into(), port.into()]);
                }
            }
        }
        if let Some(cpus) = self.cpus {
            remove(argv, |k, _| k == "-smp");
            argv.extend(["-smp".into(), cpus.to_string()]);
        }
        if let Some(size) = &self.memory {
            let size = memory(size)?;
            remove(argv, |k, _| k == "-m");
            argv.extend(["-m".into(), size]);
        }
        for (slot, selection) in [
            &self.network,
            &self.network1,
            &self.network2,
            &self.network3,
        ]
        .into_iter()
        .enumerate()
        {
            let Some(selection) = selection else { continue };
            let id = format!("net{slot}");
            let previous = argv
                .windows(2)
                .find(|p| p[0] == "-device" && field(&p[1], "netdev=") == Some(&id))
                .map(|p| p[1].clone());
            let backend = match selection.as_str() {
                "host" | "user" => Some(format!("user,id={id}")),
                "off" | "none" => None,
                other => {
                    let bridge = other
                        .strip_prefix("bridge:")
                        .filter(|b| {
                            !b.is_empty()
                                && b.bytes()
                                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                        })
                        .ok_or_else(|| invalid("network expects bridge:NAME, host, or off"))?;
                    let helper = argv
                        .windows(2)
                        .find(|p| p[0] == "-netdev" && field(&p[1], "id=") == Some(&id))
                        .and_then(|p| field(&p[1], "helper="))
                        .or_else(|| {
                            argv.windows(2)
                                .filter(|p| p[0] == "-netdev")
                                .find_map(|p| field(&p[1], "helper="))
                        })
                        .map(|h| format!(",helper={h}"))
                        .unwrap_or_default();
                    Some(format!("bridge,id={id},br={bridge}{helper}"))
                }
            };
            remove(argv, |k, v| {
                (k == "-netdev" && field(v, "id=") == Some(&id))
                    || (k == "-device" && field(v, "netdev=") == Some(&id))
            });
            if let Some(backend) = backend {
                remove(argv, |k, v| k == "-nic" && v == "none");
                let device = previous.unwrap_or_else(|| {
                    use sha2::{Digest, Sha256};
                    let hash = Sha256::digest(format!("{instance}:net{slot}").as_bytes());
                    format!(
                        "virtio-net-pci,netdev={id},mac=02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        hash[0], hash[1], hash[2], hash[3], hash[4]
                    )
                });
                argv.extend(["-netdev".into(), backend, "-device".into(), device]);
            }
        }
        if [
            &self.network,
            &self.network1,
            &self.network2,
            &self.network3,
        ]
        .iter()
        .any(|v| v.is_some())
            && !argv.iter().any(|v| v == "-netdev" || v == "-nic")
        {
            argv.extend(["-nic".into(), "none".into()]);
        }
        for rule in &self.portfwd {
            let (slot, rule) = rule
                .split_once('=')
                .ok_or_else(|| invalid("--portfwd requires SLOT=RULE"))?;
            let slot: usize = slot.parse().map_err(|_| invalid("invalid network slot"))?;
            forwarding(rule)?;
            let id = format!("net{slot}");
            let index = argv
                .windows(2)
                .position(|p| p[0] == "-netdev" && field(&p[1], "id=") == Some(&id))
                .ok_or_else(|| invalid("forwarding requires an enabled host network slot"))?;
            if !argv[index + 1].starts_with("user,") {
                return Err(invalid("forwarding requires host networking"));
            }
            let option = format!("hostfwd={rule}");
            if !argv[index + 1].split(',').any(|v| v == option) {
                argv[index + 1].push_str(&format!(",{option}"));
            }
        }
        if let Some(iso) = &self.iso {
            let drive = if iso == "off" {
                None
            } else {
                let path = Path::new(iso)
                    .canonicalize()
                    .map_err(|e| invalid(format!("cannot open ISO: {e}")))?;
                if !path.is_file() || path.to_string_lossy().contains(',') {
                    return Err(invalid(
                        "ISO must be a regular file with no comma in its path",
                    ));
                }
                Some(format!(
                    "file={},media=cdrom,readonly=on,id=me-iso",
                    path.display()
                ))
            };
            remove(argv, |k, v| {
                k == "-drive" && field(v, "id=") == Some("me-iso")
            });
            if let Some(drive) = drive {
                argv.extend(["-drive".into(), drive]);
            }
        }
        if self.h264 {
            remove(argv, |k, v| {
                matches!(k, "-display" | "-vnc" | "-vga")
                    || (k == "-object" && field(v, "id=") == Some("machineemu-vnc-password"))
                    || (k == "-device"
                        && (field(v, "id=") == Some("me-video")
                            || field(v, "id=") == Some("pc-video")))
            });
            argv.extend([
                "-vga".into(),
                "none".into(),
                "-device".into(),
                // Keep firmware and guests without a working virtio display
                // driver on one primary VGA-compatible scanout.
                "virtio-vga-gl,id=me-video".into(),
                "-display".into(),
                "dbus,p2p=on,gl=on".into(),
            ]);
            let controller = argv
                .windows(2)
                .find(|p| p[0] == "-device" && p[1].starts_with("qemu-xhci,"))
                .and_then(|p| field(&p[1], "id="))
                .map(str::to_owned);
            let controller = controller.unwrap_or_else(|| {
                argv.extend(["-device".into(), "qemu-xhci,id=usb".into()]);
                "usb".into()
            });
            if !argv
                .iter()
                .any(|v| v == "usb-tablet" || v.starts_with("usb-tablet,"))
            {
                argv.extend(["-device".into(), format!("usb-tablet,bus={controller}.0")]);
            }
            next.vnc_auto = false;
        } else if self.vnc.as_deref().is_some_and(|v| v != "profile")
            || self.vnc_password.is_some()
            || self.vnc_password_file.is_some()
        {
            let current = argv
                .windows(2)
                .find(|p| p[0] == "-display" && p[1].starts_with("vnc=127.0.0.1:"));
            let current_port = current
                .and_then(|p| p[1].split(',').next())
                .and_then(|v| v.rsplit(':').next())
                .and_then(|v| v.parse::<u16>().ok())
                .and_then(|v| v.checked_add(5900))
                .map(|v| v.to_string());
            let selection = self
                .vnc
                .as_deref()
                .filter(|v| *v != "profile")
                .or(current_port.as_deref())
                .unwrap_or("none");
            let selection = if selection == "off" {
                "none"
            } else {
                selection
            };
            if self.vnc_password.is_some() && selection == "none" {
                return Err(invalid("--vnc-password requires VNC to be enabled"));
            }
            let existing_secret = argv
                .windows(2)
                .find(|p| {
                    p[0] == "-object" && field(&p[1], "id=") == Some("machineemu-vnc-password")
                })
                .and_then(|p| field(&p[1], "file="))
                .map(std::path::PathBuf::from);
            let password_file = self.vnc_password_file.as_deref().or(
                if self.vnc_password.is_none() && selection != "none" {
                    existing_secret.as_deref()
                } else {
                    None
                },
            );
            let mut display = super::vnc::resolve(
                &serde_json::json!({}),
                Path::new("."),
                selection,
                password_file,
            )?;
            if let Some(password) = &self.vnc_password {
                if password.is_empty() || password.len() > 8 || password.contains(['\n', '\r']) {
                    return Err(invalid(
                        "VNC password must contain 1-8 bytes without a newline",
                    ));
                }
                let root = workspace.join("secrets");
                fs::create_dir_all(&root).map_err(|e| invalid(e.to_string()))?;
                let name = format!(
                    "vnc-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                );
                let path = root.join(name);
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                options
                    .open(&path)
                    .and_then(|mut f| f.write_all(password.as_bytes()))
                    .map_err(|e| invalid(e.to_string()))?;
                display.as_mut().unwrap().password_file = Some(path);
            }
            remove(argv, |k, v| {
                matches!(k, "-display" | "-vnc")
                    || (k == "-object" && field(v, "id=") == Some("machineemu-vnc-password"))
            });
            // Switching back to VNC must remove the GL-only card.
            remove(argv, |k, v| {
                k == "-device"
                    && (v.starts_with("virtio-gpu-gl,") || v.starts_with("virtio-vga-gl,"))
            });
            if display.is_some()
                && !argv.windows(2).any(|p| {
                    p[0] == "-device"
                        && (field(&p[1], "id=") == Some("pc-video")
                            || field(&p[1], "id=") == Some("me-video"))
                })
            {
                remove(argv, |k, _| k == "-vga");
                argv.extend(["-vga".into(), "std".into()]);
            }
            argv.extend(["-display".into(), "none".into()]);
            next.vnc_auto = display.as_ref().is_some_and(|d| d.auto)
                || (display.is_some()
                    && self.vnc.as_deref().is_none_or(|v| v == "profile")
                    && plan.vnc_auto);
            if let Some(display) = display {
                super::vnc::apply(argv, &display)?;
            }
        }
        *plan = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(args: &[&str]) -> LaunchSpec {
        LaunchSpec {
            argv: args.iter().map(|s| s.to_string()).collect(),
            vnc_auto: false,
            qmp_socket: "qmp".into(),
            stdout: None,
            stderr: None,
            preparation: None,
            helper_argv: None,
            helpers: vec![],
        }
    }
    #[test]
    fn h264_disables_vnc_and_always_provides_one_tablet() {
        let mut p = plan(&[
            "qemu",
            "-display",
            "vnc=:0",
            "-vnc",
            "unix:old",
            "-device",
            "VGA,id=pc-video",
        ]);
        let opts = HardwareArgs {
            h264: true,
            ..Default::default()
        };
        opts.apply(&mut p, Path::new("/tmp"), "test").unwrap();
        opts.apply(&mut p, Path::new("/tmp"), "test").unwrap();
        assert!(p.argv.contains(&"virtio-vga-gl,id=me-video".into()));
        assert!(p.argv.contains(&"dbus,p2p=on,gl=on".into()));
        assert!(
            !p.argv
                .iter()
                .any(|s| s.contains("vnc") || s.contains("pc-video"))
        );
        assert_eq!(
            p.argv
                .iter()
                .filter(|s| s.starts_with("usb-tablet,"))
                .count(),
            1
        );
        assert_eq!(
            p.argv
                .iter()
                .filter(|s| s.starts_with("qemu-xhci,"))
                .count(),
            1
        );
    }
    #[test]
    fn network_slots_preserve_identity_and_forward_only_on_host() {
        let mut p = plan(&[
            "qemu",
            "-netdev",
            "user,id=net0",
            "-device",
            "e1000,netdev=net0,mac=02:01:02:03:04:05",
            "-drive",
            "file=disk.qcow2",
        ]);
        let opts = HardwareArgs {
            network: Some("bridge:br0".into()),
            network1: Some("bridge:br2".into()),
            network2: Some("host".into()),
            portfwd: vec!["2=tcp:127.0.0.1:2222-:22".into()],
            ..Default::default()
        };
        opts.apply(&mut p, Path::new("/tmp"), "test").unwrap();
        assert!(
            p.argv
                .contains(&"e1000,netdev=net0,mac=02:01:02:03:04:05".into())
        );
        assert!(
            p.argv
                .contains(&"user,id=net2,hostfwd=tcp:127.0.0.1:2222-:22".into())
        );
        assert!(p.argv.contains(&"file=disk.qcow2".into()));
        let off = HardwareArgs {
            network2: Some("off".into()),
            ..Default::default()
        };
        off.apply(&mut p, Path::new("/tmp"), "test").unwrap();
        assert!(!p.argv.iter().any(|s| s.contains("net2")));
        let before = p.clone();
        let bad = HardwareArgs {
            portfwd: vec!["0=tcp::2222-:22".into()],
            ..Default::default()
        };
        assert!(bad.apply(&mut p, Path::new("/tmp"), "test").is_err());
        assert_eq!(p, before);
    }
    #[test]
    fn removing_last_nic_disables_qemu_default_network() {
        let mut p = plan(&[
            "qemu",
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-pci,netdev=net0",
        ]);
        HardwareArgs {
            network: Some("off".into()),
            ..Default::default()
        }
        .apply(&mut p, Path::new("/tmp"), "test")
        .unwrap();
        assert_eq!(p.argv, ["qemu", "-nic", "none"]);
    }
    #[test]
    fn cpu_and_memory_replace_topology_and_preserve_other_options() {
        let mut p = plan(&[
            "qemu",
            "-smp",
            "2,sockets=1,cores=2",
            "-m",
            "512M",
            "-cpu",
            "host",
        ]);
        HardwareArgs {
            cpus: Some(4),
            memory: Some("8GiB".into()),
            ..Default::default()
        }
        .apply(&mut p, Path::new("/tmp"), "test")
        .unwrap();
        assert_eq!(p.argv, ["qemu", "-cpu", "host", "-smp", "4", "-m", "8G"]);
        for invalid in ["0", "-1G", "8G,slots=4", "hello"] {
            assert!(memory(invalid).is_err());
        }
    }
    #[test]
    fn parser_shares_flags_across_commands() {
        use clap::Parser;
        for command in ["create", "run", "config"] {
            let mut args = vec!["machineemu", command];
            if command != "config" {
                args.push("profile");
            }
            args.extend([
                "test",
                "--cpus",
                "4",
                "--memory",
                "8GiB",
                "--h264",
                "--network",
                "bridge:br0",
                "--network1",
                "bridge:br2",
                "--network2",
                "host",
                "--portfwd",
                "2=tcp::2222-:22",
                "--iso",
                "installer.iso",
            ]);
            assert!(super::super::Cli::try_parse_from(args).is_ok());
        }
        assert!(
            super::super::Cli::try_parse_from([
                "machineemu",
                "create",
                "--file",
                "instance.json",
                "--cpus",
                "4",
                "--h264"
            ])
            .is_ok()
        );
        assert!(
            super::super::Cli::try_parse_from(["machineemu", "config", "test", "--cpus", "0"])
                .is_err()
        );
    }
    #[test]
    fn vnc_secret_is_private_and_survives_port_selection() {
        let root = std::env::temp_dir().join(format!("me-secret-test-{}", std::process::id()));
        let mut p = plan(&["qemu", "-display", "none"]);
        HardwareArgs {
            vnc: Some("auto".into()),
            vnc_password: Some("password".into()),
            ..Default::default()
        }
        .apply(&mut p, &root, "test")
        .unwrap();
        assert!(p.vnc_auto);
        let secret = p.argv.windows(2).find(|p| p[0] == "-object").unwrap()[1].clone();
        let path = Path::new(field(&secret, "file=").unwrap());
        assert_eq!(fs::read(path).unwrap(), b"password");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
        assert!(!p.argv.iter().any(|v| v.contains("data=password")));
        HardwareArgs {
            vnc: Some("auto".into()),
            ..Default::default()
        }
        .apply(&mut p, &root, "test")
        .unwrap();
        assert!(p.argv.contains(&secret));
        HardwareArgs {
            h264: true,
            ..Default::default()
        }
        .apply(&mut p, &root, "test")
        .unwrap();
        assert!(!p.vnc_auto);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn tablet_uses_existing_controller_bus() {
        let mut p = plan(&["qemu", "-device", "qemu-xhci,id=existing"]);
        HardwareArgs {
            h264: true,
            ..Default::default()
        }
        .apply(&mut p, Path::new("/tmp"), "test")
        .unwrap();
        assert!(p.argv.contains(&"usb-tablet,bus=existing.0".into()));
    }
    #[test]
    fn iso_is_replaced_without_ejecting_seed() {
        let root = std::env::temp_dir().join(format!("me-iso-test-{}", std::process::id()));
        fs::write(&root, b"iso").unwrap();
        let mut p = plan(&["qemu", "-drive", "file=seed.iso,media=cdrom,readonly=on"]);
        let opts = HardwareArgs {
            iso: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        opts.apply(&mut p, Path::new("/tmp"), "test").unwrap();
        opts.apply(&mut p, Path::new("/tmp"), "test").unwrap();
        assert_eq!(p.argv.iter().filter(|v| v.contains("id=me-iso")).count(), 1);
        HardwareArgs {
            iso: Some("off".into()),
            ..Default::default()
        }
        .apply(&mut p, Path::new("/tmp"), "test")
        .unwrap();
        assert_eq!(
            p.argv,
            ["qemu", "-drive", "file=seed.iso,media=cdrom,readonly=on"]
        );
        fs::remove_file(root).unwrap();
    }
}
