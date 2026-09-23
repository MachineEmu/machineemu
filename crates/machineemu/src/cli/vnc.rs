use machineemu_core::engine::Error;
use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
};

pub(super) struct Display {
    pub port: u16,
    pub password_file: Option<PathBuf>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn port(value: &Value) -> Result<u16, Error> {
    if value == "auto" {
        return (5900..=5999)
            .find(|port| available(*port))
            .ok_or_else(|| invalid("no free VNC port in 5900-5999"));
    }
    let value = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .ok_or_else(|| invalid("VNC port must be auto or a TCP port from 5900 to 5999"))?;
    if !(5900..=5999).contains(&value) {
        return Err(invalid("VNC port must be 5900-5999"));
    }
    let port = value as u16;
    if !available(port) {
        return Err(invalid(format!("VNC port {port} is already in use")));
    }
    Ok(port)
}

pub(super) fn resolve(
    profile: &Value,
    profile_path: &Path,
    selection: &str,
    password_override: Option<&Path>,
) -> Result<Option<Display>, Error> {
    let declared = profile.pointer("/devices/vnc");
    let settings = declared.and_then(Value::as_object);
    if let Some(declared) = declared
        && !declared.is_boolean()
        && settings.is_none()
    {
        return Err(invalid(
            "profile.devices.vnc must be a boolean or an object",
        ));
    }
    if let Some(settings) = settings {
        for key in settings.keys() {
            if !["port", "password_file"].contains(&key.as_str()) {
                return Err(invalid(format!("profile.devices.vnc.{key} is unsupported")));
            }
        }
    }
    let enabled = declared == Some(&Value::Bool(true)) || settings.is_some();
    let selected = match selection {
        "profile" if enabled => Some(
            settings
                .and_then(|s| s.get("port"))
                .cloned()
                .unwrap_or(json!("auto")),
        ),
        "profile" | "none" => None,
        "auto" => Some(json!("auto")),
        other => Some(json!(other)),
    };
    let Some(selected) = selected else {
        if password_override.is_some() {
            return Err(invalid("--vnc-password-file requires VNC to be enabled"));
        }
        return Ok(None);
    };
    let port = port(&selected)?;
    let password_file = password_override.map(Path::to_owned).or_else(|| {
        settings
            .and_then(|s| s.get("password_file"))
            .and_then(Value::as_str)
            .map(|value| {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    path
                } else {
                    profile_path.parent().unwrap_or(Path::new(".")).join(path)
                }
            })
    });
    if let Some(path) = &password_file {
        let metadata = fs::metadata(path).map_err(|error| {
            invalid(format!(
                "cannot read VNC password file {}: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(invalid("VNC password path must name a regular file"));
        }
        let bytes = fs::read(path).map_err(|error| {
            invalid(format!(
                "cannot read VNC password file {}: {error}",
                path.display()
            ))
        })?;
        if bytes.is_empty() || bytes.len() > 8 || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
            return Err(invalid(
                "VNC password file must contain 1-8 bytes without a newline",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(invalid(format!(
                    "VNC password file {} must be readable only by its owner (chmod 600)",
                    path.display()
                )));
            }
        }
    }
    Ok(Some(Display {
        port,
        password_file,
    }))
}

pub(super) fn normalize_profile(profile: &mut Value, enabled: bool) -> Result<(), Error> {
    let object = profile
        .as_object_mut()
        .ok_or_else(|| invalid("profile must be a mapping"))?;
    let devices = object
        .entry("devices")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("profile.devices must be a mapping"))?;
    devices.insert("vnc".into(), json!(enabled));
    Ok(())
}

pub(super) fn apply(argv: &mut Vec<String>, display: &Display) -> Result<(), Error> {
    let index = argv
        .iter()
        .position(|arg| arg == "-display")
        .ok_or_else(|| invalid("launch plan has no display option"))?;
    let mut setting = format!("vnc=127.0.0.1:{}", display.port - 5900);
    if let Some(path) = &display.password_file {
        let path = path
            .canonicalize()
            .map_err(|error| invalid(format!("cannot resolve VNC password file: {error}")))?;
        let path = path.to_string_lossy();
        if path.contains(',') {
            return Err(invalid("VNC password file path cannot contain a comma"));
        }
        argv.extend([
            "-object".into(),
            format!("secret,id=machineemu-vnc-password,file={path}"),
        ]);
        setting.push_str(",password-secret=machineemu-vnc-password");
    }
    argv[index + 1] = setting;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_port_and_password_are_applied_without_exposing_secret() {
        let root = std::env::temp_dir().join(format!("machineemu-vnc-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let secret = root.join("vnc.pass");
        fs::write(&secret, b"hunter42").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let available_port = (5900..=5999).rev().find(|port| available(*port)).unwrap();
        let profile = json!({"devices":{"vnc":{"port":available_port,"password_file":"vnc.pass"}}});
        let display = resolve(&profile, &root.join("profile.json"), "profile", None)
            .unwrap()
            .unwrap();
        assert_eq!(display.port, available_port);
        let mut args = vec!["-display".into(), "vnc=:0".into()];
        apply(&mut args, &display).unwrap();
        assert_eq!(
            args[1],
            format!(
                "vnc=127.0.0.1:{},password-secret=machineemu-vnc-password",
                available_port - 5900
            )
        );
        assert!(!args.join(" ").contains("hunter42"));
        assert!(
            resolve(
                &profile,
                &root.join("profile.json"),
                &available_port.to_string(),
                None
            )
            .is_ok()
        );
        assert!(resolve(&profile, &root.join("profile.json"), "59000", None).is_err());
        assert!(
            resolve(
                &json!({"devices":{"vnc":false}}),
                &root.join("profile.json"),
                "none",
                Some(&secret)
            )
            .is_err()
        );
        fs::write(&secret, b"too-long!\n").unwrap();
        assert!(resolve(&profile, &root.join("profile.json"), "profile", None).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
