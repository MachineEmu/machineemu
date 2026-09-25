use clap::{Subcommand, ValueEnum};
use machineemu_core::{domain::Id, engine::Error};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

use super::client::daemon_request;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum DeviceKind {
    UsbHost,
    UsbImage,
    Usbredir,
    Iso,
    Network,
}

impl DeviceKind {
    fn api_name(self) -> &'static str {
        match self {
            Self::UsbHost => "usb-host",
            Self::UsbImage => "usb-image",
            Self::Usbredir => "usbredir",
            Self::Iso => "iso",
            Self::Network => "network",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// List live devices of one kind.
    List {
        instance: String,
        #[arg(value_enum)]
        kind: DeviceKind,
    },
    /// Hotplug a managed device.
    Add {
        instance: String,
        #[arg(value_enum)]
        kind: DeviceKind,
        /// Managed QEMU ID: me-usbh-*, me-usbi-*, me-redir-0, me-iso-*, or me-net-*.
        device_id: String,
        /// Host USB bus number (usb-host).
        #[arg(long)]
        hostbus: Option<u16>,
        /// Host USB address (usb-host).
        #[arg(long)]
        hostaddr: Option<u16>,
        /// Local absolute path or path relative to workspace/media (usb-image or iso).
        #[arg(long)]
        path: Option<String>,
        /// Copy media into workspace/media before attaching it.
        #[arg(long, requires = "path")]
        copy_media: bool,
        /// Open a USB image read-only.
        #[arg(long)]
        read_only: bool,
        /// Network model: virtio-net-pci, e1000, or rtl8139.
        #[arg(long)]
        model: Option<String>,
        /// Network card MAC address.
        #[arg(long)]
        mac: Option<String>,
        /// Reserved pcie-root-port-*; ISO defaults to pcie-root-port-iso.
        #[arg(long)]
        bus: Option<String>,
    },
    /// Unplug a managed device and its backend.
    Remove {
        instance: String,
        #[arg(value_enum)]
        kind: DeviceKind,
        device_id: String,
    },
    /// Replace media in a hotplugged ISO drive.
    IsoChange {
        instance: String,
        /// Local absolute ISO path or path relative to workspace/media.
        path: String,
        #[arg(long, default_value = "me-iso-1")]
        device_id: String,
        /// Copy the ISO into workspace/media before changing media.
        #[arg(long)]
        copy_media: bool,
    },
    /// Eject media from a hotplugged ISO drive.
    IsoEject {
        instance: String,
        device_id: String,
        #[arg(long)]
        force: bool,
    },
}

fn checked(kind: &'static str, value: &str) -> Result<(), Error> {
    Id::new(kind, value.to_owned()).map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(())
}

fn print(value: Value) -> Result<(), Error> {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| Error::Invalid(error.to_string()))?
    );
    Ok(())
}

fn same_file_contents(left: &Path, right: &Path) -> std::io::Result<bool> {
    if left.metadata()?.len() != right.metadata()?.len() {
        return Ok(false);
    }
    let mut left = BufReader::new(File::open(left)?);
    let mut right = BufReader::new(File::open(right)?);
    let mut a = [0_u8; 64 * 1024];
    let mut b = [0_u8; 64 * 1024];
    loop {
        let count = left.read(&mut a)?;
        if count != right.read(&mut b)? || a[..count] != b[..count] {
            return Ok(false);
        }
        if count == 0 {
            return Ok(true);
        }
    }
}

fn copy_media(workspace: &Path, value: String) -> Result<String, Error> {
    let source = PathBuf::from(&value);
    if !source.is_file() {
        return Err(Error::Invalid(format!(
            "media file does not exist: {}",
            source.display()
        )));
    }
    let name = source
        .file_name()
        .ok_or_else(|| Error::Invalid("media path has no file name".into()))?;
    let directory = workspace.join("media");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::Invalid(format!("cannot create {}: {error}", directory.display()))
    })?;
    let destination = directory.join(name);
    if destination.exists() {
        if !same_file_contents(&source, &destination)
            .map_err(|error| Error::Invalid(format!("cannot compare existing media: {error}")))?
        {
            return Err(Error::Invalid(format!(
                "{} already exists with different contents",
                destination.display()
            )));
        }
    } else {
        std::fs::copy(&source, &destination).map_err(|error| {
            Error::Invalid(format!(
                "cannot copy {} to {}: {error}",
                source.display(),
                destination.display()
            ))
        })?;
    }
    Ok(name.to_string_lossy().into_owned())
}

fn direct_media(value: String) -> String {
    let path = PathBuf::from(&value);
    if path.is_file() {
        std::fs::canonicalize(path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or(value)
    } else {
        value
    }
}

pub async fn run(
    command: DeviceCommand,
    workspace: &Path,
    daemon: &str,
    token: &str,
) -> Result<(), Error> {
    let (method, path, body) = match command {
        DeviceCommand::List { instance, kind } => {
            checked("instance", &instance)?;
            (
                "GET",
                format!("/api/v2/instances/{instance}/devices/{}", kind.api_name()),
                None,
            )
        }
        DeviceCommand::Add {
            instance,
            kind,
            device_id,
            hostbus,
            hostaddr,
            path,
            copy_media: copy,
            read_only,
            model,
            mac,
            bus,
        } => {
            checked("instance", &instance)?;
            checked("device", &device_id)?;
            let mut body = json!({"device_id":device_id});
            let object = body.as_object_mut().expect("object literal");
            if let Some(value) = hostbus {
                object.insert("hostbus".into(), json!(value));
            }
            if let Some(value) = hostaddr {
                object.insert("hostaddr".into(), json!(value));
            }
            if let Some(value) = path {
                let value = if copy {
                    copy_media(workspace, value)?
                } else {
                    direct_media(value)
                };
                object.insert("path".into(), json!(value));
            }
            if read_only {
                object.insert("read_only".into(), json!(true));
            }
            if let Some(value) = model {
                object.insert("model".into(), json!(value));
            }
            if let Some(value) = mac {
                object.insert("mac".into(), json!(value));
            }
            if let Some(value) = bus {
                object.insert("bus".into(), json!(value));
            }
            (
                "POST",
                format!("/api/v2/instances/{instance}/devices/{}", kind.api_name()),
                Some(body),
            )
        }
        DeviceCommand::Remove {
            instance,
            kind,
            device_id,
        } => {
            checked("instance", &instance)?;
            checked("device", &device_id)?;
            (
                "DELETE",
                format!(
                    "/api/v2/instances/{instance}/devices/{}/{device_id}",
                    kind.api_name()
                ),
                None,
            )
        }
        DeviceCommand::IsoChange {
            instance,
            device_id,
            path: medium,
            copy_media: copy,
        } => {
            checked("instance", &instance)?;
            checked("device", &device_id)?;
            (
                "POST",
                format!("/api/v2/instances/{instance}/devices/iso/{device_id}/change"),
                Some(
                    json!({"path":if copy { copy_media(workspace, medium)? } else { direct_media(medium) }}),
                ),
            )
        }
        DeviceCommand::IsoEject {
            instance,
            device_id,
            force,
        } => {
            checked("instance", &instance)?;
            checked("device", &device_id)?;
            (
                "POST",
                format!("/api/v2/instances/{instance}/devices/iso/{device_id}/eject"),
                Some(json!({"force":force})),
            )
        }
    };
    print(daemon_request(daemon, token, method, &path, body).await?)
}
