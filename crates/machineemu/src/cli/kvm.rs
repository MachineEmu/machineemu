//! `machineemu analysis-target`: point the patched `kvm_intel` at an analysis
//! instance's QEMU process.
//!
//! The analysis kernel patch adds an `analysis_target_tgid` parameter to
//! `kvm_intel`. CPUID/hypercall filtering and RDTSC handling only act on the
//! VM whose QEMU thread-group id matches it, so ordinary VMs on the same host
//! are untouched. The parameter lives under `/sys` and only root may write it,
//! so the write goes through root-agent when its socket is present, else sudo.

use super::console::instance_dir;
use machineemu_core::engine::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

fn error(value: impl std::fmt::Display) -> Error {
    Error::Runtime(value.to_string())
}

const PARAM_DIR: &str = "/sys/module/kvm_intel/parameters";

/// The running QEMU thread-group id of one instance.
fn instance_tgid(instance: &str, workspace: Option<&Path>) -> Result<u32, Error> {
    let directory = instance_dir(instance, workspace)?;
    let pid = fs::read_to_string(directory.join("control/qemu.pid"))
        .map_err(|source| error(format!("cannot read {instance} pid file: {source}")))?
        .trim()
        .parse::<u32>()
        .map_err(|_| error(format!("{instance} has no valid QEMU pid; is it running?")))?;
    if !Path::new(&format!("/proc/{pid}")).is_dir() {
        return Err(error(format!(
            "{instance} pid {pid} is not running; start it before targeting kvm_intel"
        )));
    }
    Ok(pid)
}

/// The positive tgids currently in the multi-target array param.
fn current_targets() -> Vec<u32> {
    fs::read_to_string(format!("{PARAM_DIR}/analysis_target_tgids"))
        .map(|text| {
            text.split([',', ' ', '\n', '\t'])
                .filter_map(|token| token.trim().parse::<i64>().ok())
                .filter(|value| *value > 0)
                .map(|value| value as u32)
                .collect()
        })
        .unwrap_or_default()
}

/// Write one `kvm_intel` parameter, escalating because `/sys` is root-owned.
/// root-agent is tried first so an unattended run needs no password, then sudo.
/// A stale root-agent socket fails its attempt, so sudo still gets its turn.
fn write_param(name: &str, value: &str) -> Result<(), Error> {
    let path = format!("{PARAM_DIR}/{name}");
    let mut attempts: Vec<String> = Vec::new();

    let agent_live = std::env::var("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("root-agent.sock"))
        .map(|socket| socket.exists())
        .unwrap_or(false);
    if agent_live && which("root-agent") {
        let mut command = ProcessCommand::new("root-agent");
        command.args([
            "run",
            "--",
            "sh",
            "-c",
            "printf '%s\\n' \"$1\" > \"$2\"",
            "sh",
            value,
            &path,
        ]);
        match command.status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => attempts.push(format!("root-agent exited {status}")),
            Err(source) => attempts.push(format!("root-agent could not run: {source}")),
        }
    }

    if which("sudo") {
        match sudo_tee(value, &path) {
            Ok(()) => return Ok(()),
            Err(reason) => attempts.push(reason),
        }
    } else {
        attempts.push("sudo is not on PATH".into());
    }

    Err(error(format!(
        "could not write {path} as root; tried: {}.\n\
         Run this command in an interactive shell so sudo can prompt, or start root-agent.",
        attempts.join("; ")
    )))
}

fn sudo_tee(value: &str, path: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = ProcessCommand::new("sudo")
        .args(["tee", path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|source| format!("sudo could not run: {source}"))?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(format!("{value}\n").as_bytes())
        .map_err(|source| format!("cannot feed sudo tee: {source}"))?;
    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("sudo tee exited {status}")),
        Err(source) => Err(format!("sudo tee did not complete: {source}")),
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

pub(super) fn analysis_target(
    instances: &[String],
    workspace: Option<&Path>,
    add: bool,
    clear: bool,
) -> Result<(), Error> {
    let single = Path::new(PARAM_DIR).join("analysis_target_tgid");
    if !single.exists() {
        return Err(error(format!(
            "kvm_intel is not the analysis build: {single:?} is missing.\n\
             Load the patched module first, for example:\n  \
             sudo rmmod kvm_intel && sudo insmod <analysis>/kvm-intel.ko\n\
             The module must match the running kernel ({}).",
            kernel_release()
        )));
    }
    // The rebuilt module adds an array param for several pods at once; without
    // it, only a single legacy tgid can be targeted.
    let multi = Path::new(PARAM_DIR).join("analysis_target_tgids").exists();

    if clear {
        write_param("analysis_target_tgid", "-1")?;
        if multi {
            write_param("analysis_target_tgids", "-1")?;
        }
        println!("kvm_intel analysis target cleared");
        return Ok(());
    }

    // Resolve every named pod to a running tgid before writing anything.
    let mut resolved: Vec<(String, u32)> = Vec::new();
    for instance in instances {
        resolved.push((instance.clone(), instance_tgid(instance, workspace)?));
    }

    if !multi {
        if resolved.len() > 1 || add {
            return Err(error(
                "the loaded kvm_intel exposes only analysis_target_tgid (one pod). \
                 Rebuild and reload the patched module to get analysis_target_tgids \
                 for multiple pods."
                    .to_string(),
            ));
        }
        let (instance, pid) = &resolved[0];
        write_param("analysis_target_tgid", &pid.to_string())?;
        verify("analysis_target_tgid", &[*pid])?;
        println!("kvm_intel analysis target set to {instance} (qemu tgid {pid})");
        return Ok(());
    }

    // Union with the current set when adding, otherwise replace it. The single
    // legacy param is cleared so the array is the whole truth.
    let mut targets: std::collections::BTreeSet<u32> = if add {
        current_targets().into_iter().collect()
    } else {
        std::collections::BTreeSet::new()
    };
    for (_, pid) in &resolved {
        targets.insert(*pid);
    }
    let value = targets
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    write_param("analysis_target_tgids", &value)?;
    write_param("analysis_target_tgid", "-1")?;
    verify(
        "analysis_target_tgids",
        &targets.iter().copied().collect::<Vec<_>>(),
    )?;
    let names = resolved
        .iter()
        .map(|(instance, pid)| format!("{instance}={pid}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "kvm_intel analysis targets ({} pod{}): {names}\n  now: {value}",
        targets.len(),
        if targets.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Read a param back and confirm every expected tgid is present. The array
/// param prints its live members, so a dropped write is caught here.
fn verify(name: &str, expected: &[u32]) -> Result<(), Error> {
    let readback = fs::read_to_string(format!("{PARAM_DIR}/{name}"))
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "<unreadable>".into());
    let present: std::collections::BTreeSet<i64> = readback
        .split([',', ' '])
        .filter_map(|token| token.trim().parse::<i64>().ok())
        .collect();
    for pid in expected {
        if !present.contains(&(*pid as i64)) {
            return Err(error(format!(
                "readback of {name} is {readback:?}; it does not include {pid}, so the write did not take"
            )));
        }
    }
    Ok(())
}

fn kernel_release() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "unknown".into())
}
