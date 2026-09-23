//! `machineemu inspect`: what an instance is running, what it listens on and
//! what hardware the guest sees.
//!
//! The daemon only records an instance's identity and state, so everything
//! else is read from the live system: the QEMU argv and every process that
//! works on the instance directory (swtpm, helpers) from /proc, their sockets
//! from /proc/<pid>/net, and the guest hardware over QMP. A stopped instance
//! still reports its state directory and daemon record.

use super::client::daemon_request;
use super::console::instance_dir;
use machineemu_core::engine::Error;
use machineemu_core::protocols::qmp::QmpClient;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Serialize)]
struct Report {
    instance: String,
    directory: PathBuf,
    daemon: Option<Value>,
    running: bool,
    pid: Option<u32>,
    executable: Option<String>,
    argv: Vec<String>,
    processes: Vec<Process>,
    sockets: Vec<Socket>,
    hardware: Option<Hardware>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hardware_error: Option<String>,
    files: Vec<FileEntry>,
}

#[derive(Serialize)]
struct Process {
    pid: u32,
    name: String,
}

#[derive(Serialize, Debug, PartialEq)]
struct Socket {
    pid: u32,
    process: String,
    kind: &'static str,
    state: &'static str,
    local: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
    role: String,
}

#[derive(Serialize, Default)]
struct Hardware {
    machine: Option<String>,
    accelerator: Option<String>,
    cpu_count: usize,
    cpu_type: Option<String>,
    cpu_model: Option<String>,
    topology: Option<String>,
    memory_bytes: Option<u64>,
    pci: Vec<PciDevice>,
    block: Vec<BlockDevice>,
    chardevs: Vec<Chardev>,
    nics: Vec<Nic>,
    tpm: Vec<Value>,
    vnc: Option<Value>,
    usb: Vec<String>,
}

#[derive(Serialize)]
struct PciDevice {
    address: String,
    vendor: u64,
    device: u64,
    subsystem_vendor: Option<u64>,
    subsystem: Option<u64>,
    class: String,
    id: Option<String>,
}

#[derive(Serialize)]
struct BlockDevice {
    device: String,
    qdev: Option<String>,
    file: Option<String>,
    format: Option<String>,
    read_only: bool,
    removable: bool,
}

#[derive(Serialize)]
struct Chardev {
    label: String,
    filename: String,
    open: bool,
}

#[derive(Serialize)]
struct Nic {
    name: String,
    mac: Option<String>,
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    kind: &'static str,
    size: u64,
}

pub(super) fn inspect(
    instance: &str,
    workspace: Option<&Path>,
    daemon: &str,
    token: &str,
    json: bool,
) -> Result<(), Error> {
    let directory = instance_dir(instance, workspace)?;
    let directory = fs::canonicalize(&directory).unwrap_or(directory);
    let daemon_record = daemon_request(
        daemon,
        token,
        "GET",
        &format!("/api/v2/instances/{instance}"),
        None,
    )
    .ok();

    let pid = fs::read_to_string(directory.join("control/qemu.pid"))
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|pid| Path::new(&format!("/proc/{pid}")).is_dir());
    let argv = pid.map(read_cmdline).unwrap_or_default();
    let running = pid.is_some() && !argv.is_empty();

    let mut processes = Vec::new();
    if running {
        processes = instance_processes(&directory, pid);
    }
    let mut sockets = Vec::new();
    for process in &processes {
        sockets.extend(process_sockets(process));
    }

    let (mut hardware, mut hardware_error) = (None, None);
    if running {
        match query_hardware(&argv) {
            Ok(value) => hardware = Some(value),
            Err(error) => hardware_error = Some(error),
        }
    }
    let chardevs = hardware.as_ref().map(|h| &h.chardevs[..]).unwrap_or(&[]);
    for socket in &mut sockets {
        socket.role = socket_role(socket, &argv, chardevs);
    }
    sockets.sort_by(|a, b| {
        (a.state != "listen", a.pid, a.kind, &a.local).cmp(&(
            b.state != "listen",
            b.pid,
            b.kind,
            &b.local,
        ))
    });

    let report = Report {
        instance: instance.to_owned(),
        files: state_files(&directory),
        directory,
        daemon: daemon_record,
        running,
        pid,
        executable: argv.first().cloned(),
        argv,
        processes,
        sockets,
        hardware,
        hardware_error,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| Error::Invalid(e.to_string()))?
        );
    } else {
        print_report(&report);
    }
    Ok(())
}

fn read_cmdline(pid: u32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn process_name(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|name| name.trim().to_owned())
        .unwrap_or_else(|_| "?".into())
}

/// QEMU plus every process whose command line names the instance directory:
/// swtpm keeps its state there and the bridge or other helpers take paths in it.
fn instance_processes(directory: &Path, qemu: Option<u32>) -> Vec<Process> {
    let needle = directory.to_string_lossy().into_owned();
    let own = std::process::id();
    let mut pids: Vec<u32> = qemu.into_iter().collect();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == own || pids.contains(&pid) {
                continue;
            }
            if read_cmdline(pid).iter().any(|arg| arg.contains(&needle)) {
                pids.push(pid);
            }
        }
    }
    pids.into_iter()
        .map(|pid| Process {
            pid,
            name: process_name(pid),
        })
        .collect()
}

fn socket_inodes(pid: u32) -> Vec<u64> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        .filter_map(|target| {
            target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect()
}

fn process_sockets(process: &Process) -> Vec<Socket> {
    let inodes = socket_inodes(process.pid);
    if inodes.is_empty() {
        return Vec::new();
    }
    let net = |name: &str| {
        fs::read_to_string(format!("/proc/{}/net/{name}", process.pid)).unwrap_or_default()
    };
    let mut sockets = Vec::new();
    for entry in parse_proc_unix(&net("unix")) {
        // Client-side Unix sockets carry no path; the peer's accepted socket
        // does, and that shows up on the listening process.
        if inodes.contains(&entry.inode) && !entry.path.is_empty() {
            sockets.push(Socket {
                pid: process.pid,
                process: process.name.clone(),
                kind: "unix",
                state: if entry.listening {
                    "listen"
                } else {
                    "connected"
                },
                local: entry.path,
                remote: None,
                role: String::new(),
            });
        }
    }
    for (file, kind) in [("tcp", "tcp"), ("tcp6", "tcp6")] {
        for entry in parse_proc_tcp(&net(file)) {
            if !inodes.contains(&entry.inode) {
                continue;
            }
            let state = match entry.state {
                0x0A => "listen",
                0x01 => "connected",
                _ => continue,
            };
            sockets.push(Socket {
                pid: process.pid,
                process: process.name.clone(),
                kind,
                state,
                local: entry.local,
                remote: (state == "connected").then_some(entry.remote),
                role: String::new(),
            });
        }
    }
    sockets
}

#[derive(Debug, PartialEq)]
struct UnixEntry {
    inode: u64,
    listening: bool,
    path: String,
}

/// /proc/net/unix: `Num RefCount Protocol Flags Type St Inode Path`. Flags
/// carries __SO_ACCEPTCON (0x10000) on a listening socket.
fn parse_proc_unix(text: &str) -> Vec<UnixEntry> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 7 {
                return None;
            }
            let flags = u64::from_str_radix(fields[3], 16).ok()?;
            Some(UnixEntry {
                inode: fields[6].parse().ok()?,
                listening: flags & 0x10000 != 0,
                path: fields.get(7).map(|p| p.to_string()).unwrap_or_default(),
            })
        })
        .collect()
}

#[derive(Debug, PartialEq)]
struct TcpEntry {
    local: String,
    remote: String,
    state: u8,
    inode: u64,
}

/// /proc/net/tcp{,6}: `sl local_address rem_address st ... uid timeout inode`.
fn parse_proc_tcp(text: &str) -> Vec<TcpEntry> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                return None;
            }
            Some(TcpEntry {
                local: decode_address(fields[1])?,
                remote: decode_address(fields[2])?,
                state: u8::from_str_radix(fields[3], 16).ok()?,
                inode: fields[9].parse().ok()?,
            })
        })
        .collect()
}

/// The kernel prints each 32-bit word of the address in host byte order.
fn decode_address(value: &str) -> Option<String> {
    let (address, port) = value.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let words: Vec<u32> = (0..address.len() / 8)
        .map(|i| u32::from_str_radix(&address[i * 8..i * 8 + 8], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    match words.len() {
        1 => Some(format!("{}:{port}", Ipv4Addr::from(words[0].swap_bytes()))),
        4 => {
            let mut bytes = [0u8; 16];
            for (i, word) in words.iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            Some(format!("[{}]:{port}", Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

/// What a socket is for, from the argv option or chardev that names it.
fn socket_role(socket: &Socket, argv: &[String], chardevs: &[Chardev]) -> String {
    if socket.kind != "unix" {
        let port = socket
            .local
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok());
        return if port.is_some() && port == vnc_port(argv) {
            "vnc".into()
        } else if socket.state == "connected" {
            "client".into()
        } else {
            String::new()
        };
    }
    let role = argv_role(&socket.local, argv).or_else(|| {
        chardevs
            .iter()
            .find(|c| c.filename.contains(&socket.local) && !c.label.starts_with("compat_monitor"))
            .map(|chardev| format!("chardev {}", chardev.label))
    });
    // swtpm serves the socket QEMU's TPM chardev connects to.
    match (socket.process == "swtpm", role) {
        (true, Some(role)) => format!("swtpm for {}", role.trim_start_matches("chardev ")),
        (true, None) => "swtpm".into(),
        (false, role) => role.unwrap_or_default(),
    }
}

fn argv_role(path: &str, argv: &[String]) -> Option<String> {
    let index = argv.iter().position(|arg| arg.contains(path))?;
    let option = index
        .checked_sub(1)
        .map(|i| argv[i].trim_start_matches('-'))
        .unwrap_or("");
    let arg = &argv[index];
    Some(match option {
        "qmp" if path.ends_with(".relay.qmp") => "qmp relay".into(),
        "qmp" => "qmp (daemon)".into(),
        "chardev" => arg
            .split(',')
            .find_map(|part| part.strip_prefix("id="))
            .map(|id| format!("chardev {id}"))
            .unwrap_or_else(|| "chardev".into()),
        "" => return None,
        other => other.into(),
    })
}

/// The VNC TCP port, from `-vnc HOST:N` or `-display vnc=HOST:N`.
fn vnc_port(argv: &[String]) -> Option<u16> {
    let display = option_value(argv, "-vnc").or_else(|| {
        argv.windows(2)
            .filter(|pair| pair[0] == "-display")
            .find_map(|pair| pair[1].strip_prefix("vnc="))
    })?;
    let display = display.split(',').next()?;
    let number = display.rsplit(':').next()?.parse::<u16>().ok()?;
    number.checked_add(5900)
}

fn option_value<'a>(argv: &'a [String], option: &str) -> Option<&'a str> {
    argv.iter()
        .position(|arg| arg == option)
        .and_then(|index| argv.get(index + 1))
        .map(String::as_str)
}

fn qmp_sockets(argv: &[String]) -> Vec<PathBuf> {
    let mut sockets: Vec<PathBuf> = argv
        .windows(2)
        .filter(|pair| pair[0] == "-qmp")
        .filter_map(|pair| pair[1].strip_prefix("unix:"))
        .map(|value| PathBuf::from(value.split(',').next().unwrap_or(value)))
        .collect();
    // The relay is meant for outside clients; the daemon may be holding the other.
    sockets.sort_by_key(|path| !path.to_string_lossy().ends_with(".relay.qmp"));
    sockets
}

fn query_hardware(argv: &[String]) -> Result<Hardware, String> {
    let mut last_error = "the QEMU command line has no Unix QMP socket".to_owned();
    for path in qmp_sockets(argv) {
        match QmpClient::connect(&path, Duration::from_secs(2)) {
            Ok(mut client) => return Ok(collect_hardware(&mut client, argv)),
            Err(error) => {
                last_error = format!(
                    "QMP {} unavailable ({error}); another client may hold it",
                    path.display()
                )
            }
        }
    }
    Err(last_error)
}

fn collect_hardware(client: &mut QmpClient, argv: &[String]) -> Hardware {
    let mut query = |command: &str| client.execute(command, Value::Null).ok();
    let mut hardware = Hardware {
        machine: option_value(argv, "-machine")
            .or_else(|| option_value(argv, "-M"))
            .map(|value| {
                value
                    .split(',')
                    .next()
                    .unwrap_or(value)
                    .trim_start_matches("type=")
                    .to_owned()
            }),
        cpu_model: option_value(argv, "-cpu").map(str::to_owned),
        topology: option_value(argv, "-smp").map(str::to_owned),
        ..Default::default()
    };
    hardware.accelerator = query("query-kvm").map(|kvm| {
        if kvm["enabled"].as_bool() == Some(true) {
            "kvm"
        } else {
            "tcg"
        }
        .to_owned()
    });
    if let Some(Value::Array(cpus)) = query("query-cpus-fast") {
        hardware.cpu_count = cpus.len();
        hardware.cpu_type = cpus
            .first()
            .and_then(|cpu| cpu["qom-type"].as_str())
            .map(str::to_owned);
    }
    hardware.memory_bytes = query("query-memory-size-summary").and_then(|summary| {
        Some(summary["base-memory"].as_u64()? + summary["plugged-memory"].as_u64().unwrap_or(0))
    });
    if let Some(Value::Array(buses)) = query("query-pci") {
        for bus in &buses {
            collect_pci(
                bus["bus"].as_u64().unwrap_or(0),
                &bus["devices"],
                &mut hardware.pci,
            );
        }
    }
    if let Some(Value::Array(blocks)) = query("query-block") {
        hardware.block = blocks
            .iter()
            .map(|block| BlockDevice {
                device: block["device"].as_str().unwrap_or("").to_owned(),
                qdev: block["qdev"].as_str().map(str::to_owned),
                file: block["inserted"]["file"].as_str().map(str::to_owned),
                format: block["inserted"]["drv"].as_str().map(str::to_owned),
                read_only: block["inserted"]["ro"].as_bool().unwrap_or(false),
                removable: block["removable"].as_bool().unwrap_or(false),
            })
            .collect();
    }
    if let Some(Value::Array(chardevs)) = query("query-chardev") {
        hardware.chardevs = chardevs
            .iter()
            .map(|chardev| Chardev {
                label: chardev["label"].as_str().unwrap_or("").to_owned(),
                filename: chardev["filename"].as_str().unwrap_or("").to_owned(),
                open: chardev["frontend-open"].as_bool().unwrap_or(false),
            })
            .collect();
    }
    if let Some(Value::Array(filters)) = query("query-rx-filter") {
        hardware.nics = filters
            .iter()
            .map(|nic| Nic {
                name: nic["name"].as_str().unwrap_or("").to_owned(),
                mac: nic["main-mac"].as_str().map(str::to_owned),
            })
            .collect();
    }
    if let Some(Value::Array(tpm)) = query("query-tpm") {
        hardware.tpm = tpm;
    }
    hardware.vnc = query("query-vnc").filter(|vnc| vnc["enabled"].as_bool() == Some(true));
    if let Ok(Value::String(text)) = client.execute(
        "human-monitor-command",
        serde_json::json!({"command-line": "info usb"}),
    ) {
        hardware.usb = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.contains("not enabled"))
            .map(str::to_owned)
            .collect();
    }
    hardware
}

fn collect_pci(bus: u64, devices: &Value, out: &mut Vec<PciDevice>) {
    for device in devices.as_array().into_iter().flatten() {
        let id = &device["id"];
        out.push(PciDevice {
            address: format!(
                "{:02x}:{:02x}.{}",
                device["bus"].as_u64().unwrap_or(bus),
                device["slot"].as_u64().unwrap_or(0),
                device["function"].as_u64().unwrap_or(0)
            ),
            vendor: id["vendor"].as_u64().unwrap_or(0),
            device: id["device"].as_u64().unwrap_or(0),
            subsystem_vendor: id["subsystem-vendor"].as_u64(),
            subsystem: id["subsystem"].as_u64(),
            class: device["class_info"]["desc"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "class {:04x}",
                        device["class_info"]["class"].as_u64().unwrap_or(0)
                    )
                }),
            id: device["qdev_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
        });
        if let Some(bridge) = device.get("pci_bridge") {
            let secondary = bridge["bus"]["secondary"].as_u64().unwrap_or(bus);
            collect_pci(secondary, &bridge["devices"], out);
        }
    }
}

fn state_files(directory: &Path) -> Vec<FileEntry> {
    let mut files: Vec<FileEntry> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let file_type = metadata.file_type();
            #[cfg(unix)]
            let socket = std::os::unix::fs::FileTypeExt::is_socket(&file_type);
            #[cfg(not(unix))]
            let socket = false;
            Some(FileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind: if file_type.is_dir() {
                    "dir"
                } else if socket {
                    "socket"
                } else {
                    "file"
                },
                size: if file_type.is_file() {
                    metadata.len()
                } else {
                    0
                },
            })
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

fn display_path(value: &str) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            Path::new(value)
                .strip_prefix(&cwd)
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| value.to_owned())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn print_report(report: &Report) {
    let field = |key: &str| {
        report
            .daemon
            .as_ref()
            .and_then(|record| record[key].as_str())
            .unwrap_or("?")
            .to_owned()
    };
    println!("instance   {}", report.instance);
    println!(
        "directory  {}",
        display_path(&report.directory.to_string_lossy())
    );
    if report.daemon.is_some() {
        println!(
            "daemon     state {}, profile {}, image {}",
            field("state"),
            field("profile_id"),
            field("image_id")
        );
    } else {
        println!("daemon     unreachable or no record");
    }
    match (report.running, report.pid) {
        (true, Some(pid)) => println!("process    running, pid {pid}"),
        _ => println!("process    not running"),
    }
    if let Some(executable) = &report.executable {
        println!("qemu       {}", display_path(executable));
    }

    if !report.processes.is_empty() {
        println!("\nPROCESSES");
        for process in &report.processes {
            println!("  {:<8} {}", process.pid, process.name);
        }
    }

    let mut groups: BTreeMap<bool, Vec<&Socket>> = BTreeMap::new();
    for socket in &report.sockets {
        groups
            .entry(socket.state != "listen")
            .or_default()
            .push(socket);
    }
    for (connected, title) in [(false, "LISTENERS"), (true, "CONNECTIONS")] {
        let Some(sockets) = groups.get(&connected) else {
            continue;
        };
        println!("\n{title}");
        println!(
            "  {:<16} {:<5} {:<18} {:<52} REMOTE",
            "PROCESS", "KIND", "ROLE", "ADDRESS"
        );
        for socket in sockets {
            println!(
                "  {:<16} {:<5} {:<18} {:<52} {}",
                socket.process,
                socket.kind,
                if socket.role.is_empty() {
                    "-"
                } else {
                    &socket.role
                },
                display_path(&socket.local),
                socket.remote.as_deref().unwrap_or("")
            );
        }
    }

    if let Some(hardware) = &report.hardware {
        println!("\nHARDWARE");
        let opt = |value: &Option<String>| value.clone().unwrap_or_else(|| "?".into());
        println!(
            "  machine  {} ({})",
            opt(&hardware.machine),
            opt(&hardware.accelerator)
        );
        println!(
            "  cpu      {} x {}{}",
            hardware.cpu_count,
            opt(&hardware.cpu_type),
            hardware
                .topology
                .as_ref()
                .map(|t| format!(", smp {t}"))
                .unwrap_or_default()
        );
        if let Some(model) = &hardware.cpu_model {
            println!("           {model}");
        }
        if let Some(memory) = hardware.memory_bytes {
            println!("  memory   {}", human_size(memory));
        }
        for tpm in &hardware.tpm {
            println!(
                "  tpm      {} ({}, {})",
                tpm["model"].as_str().unwrap_or("?"),
                tpm["options"]["type"].as_str().unwrap_or("?"),
                tpm["id"].as_str().unwrap_or("?")
            );
        }
        if let Some(vnc) = &hardware.vnc {
            println!(
                "  vnc      {}:{}, auth {}, {} client(s)",
                vnc["host"].as_str().unwrap_or("?"),
                vnc["service"].as_str().unwrap_or("?"),
                vnc["auth"].as_str().unwrap_or("?"),
                vnc["clients"].as_array().map(Vec::len).unwrap_or(0)
            );
        }
        if !hardware.pci.is_empty() {
            println!("\n  PCI");
            for device in &hardware.pci {
                let subsystem = match (device.subsystem_vendor, device.subsystem) {
                    (Some(vendor), Some(id)) => format!("  sub {vendor:04x}:{id:04x}"),
                    _ => String::new(),
                };
                println!(
                    "    {}  {:04x}:{:04x}  {:<20}{}{}",
                    device.address,
                    device.vendor,
                    device.device,
                    device.class,
                    subsystem,
                    device
                        .id
                        .as_ref()
                        .map(|id| format!("  id {id}"))
                        .unwrap_or_default()
                );
            }
        }
        if !hardware.block.is_empty() {
            println!("\n  BLOCK");
            for block in &hardware.block {
                let mut flags = Vec::new();
                if block.read_only {
                    flags.push("ro");
                }
                if block.removable {
                    flags.push("removable");
                }
                println!(
                    "    {:<10} {:<6} {:<12} {}",
                    block.device,
                    block.format.as_deref().unwrap_or("-"),
                    flags.join(","),
                    block
                        .file
                        .as_deref()
                        .map(display_path)
                        .unwrap_or_else(|| "(empty)".into())
                );
            }
        }
        if !hardware.nics.is_empty() {
            println!("\n  NICS");
            for nic in &hardware.nics {
                println!("    {:<16} {}", nic.name, nic.mac.as_deref().unwrap_or("?"));
            }
        }
        if !hardware.usb.is_empty() {
            println!("\n  USB");
            for line in &hardware.usb {
                println!("    {line}");
            }
        }
        if !hardware.chardevs.is_empty() {
            println!("\n  CHARDEVS");
            for chardev in &hardware.chardevs {
                println!(
                    "    {:<16} {:<6} {}",
                    chardev.label,
                    if chardev.open { "open" } else { "closed" },
                    display_path(&chardev.filename)
                );
            }
        }
    } else if let Some(error) = &report.hardware_error {
        println!("\nHARDWARE\n  unavailable: {error}");
    }

    if !report.files.is_empty() {
        println!("\nSTATE");
        for file in &report.files {
            let size = if file.kind == "file" {
                human_size(file.size)
            } else {
                file.kind.to_owned()
            };
            println!("  {:<24} {size}", file.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_table_marks_listeners_and_keeps_paths() {
        let text = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 4242 /ws/s/abc.relay.qmp\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 4243 /ws/instances/a/sockets/tpm.sock\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 4244\n";
        assert_eq!(
            parse_proc_unix(text),
            vec![
                UnixEntry {
                    inode: 4242,
                    listening: true,
                    path: "/ws/s/abc.relay.qmp".into()
                },
                UnixEntry {
                    inode: 4243,
                    listening: false,
                    path: "/ws/instances/a/sockets/tpm.sock".into()
                },
                UnixEntry {
                    inode: 4244,
                    listening: false,
                    path: String::new()
                },
            ]
        );
    }

    #[test]
    fn tcp_addresses_decode_from_host_order_words() {
        assert_eq!(decode_address("0100007F:170E").unwrap(), "127.0.0.1:5902");
        assert_eq!(
            decode_address("00000000000000000000000001000000:1F90").unwrap(),
            "[::1]:8080"
        );
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
            0: 0100007F:170E 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 555 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(
            parse_proc_tcp(text),
            vec![TcpEntry {
                local: "127.0.0.1:5902".into(),
                remote: "0.0.0.0:0".into(),
                state: 0x0A,
                inode: 555
            }]
        );
    }

    fn socket(process: &str, kind: &'static str, local: &str) -> Socket {
        Socket {
            pid: 1,
            process: process.into(),
            kind,
            state: "listen",
            local: local.into(),
            remote: None,
            role: String::new(),
        }
    }

    #[test]
    fn roles_come_from_the_option_or_chardev_that_names_the_socket() {
        let argv: Vec<String> = [
            "qemu",
            "-qmp",
            "unix:/ws/s/abc.qmp,server=on,wait=off",
            "-chardev",
            "socket,id=uart0,path=/ws/i/serial.sock,server=on",
            "-chardev",
            "socket,id=chrtpm,path=/ws/i/sockets/tpm.sock",
            "-display",
            "vnc=127.0.0.1:2",
            "-qmp",
            "unix:/ws/s/abc.relay.qmp,server=on,wait=off",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            socket_role(&socket("qemu", "unix", "/ws/s/abc.qmp"), &argv, &[]),
            "qmp (daemon)"
        );
        assert_eq!(
            socket_role(&socket("qemu", "unix", "/ws/s/abc.relay.qmp"), &argv, &[]),
            "qmp relay"
        );
        assert_eq!(
            socket_role(&socket("qemu", "unix", "/ws/i/serial.sock"), &argv, &[]),
            "chardev uart0"
        );
        assert_eq!(
            socket_role(&socket("qemu", "tcp", "127.0.0.1:5902"), &argv, &[]),
            "vnc"
        );
        assert_eq!(
            socket_role(
                &socket("swtpm", "unix", "/ws/i/sockets/tpm.sock"),
                &argv,
                &[]
            ),
            "swtpm for chrtpm"
        );
        assert_eq!(
            qmp_sockets(&argv),
            vec![
                PathBuf::from("/ws/s/abc.relay.qmp"),
                PathBuf::from("/ws/s/abc.qmp")
            ]
        );
    }
}
