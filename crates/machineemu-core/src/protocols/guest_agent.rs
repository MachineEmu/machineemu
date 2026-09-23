use crate::{Error, Result};
use std::{io::Write, path::Path};

#[cfg(unix)]
pub fn guest_ipv4(socket: &Path) -> Result<Option<String>> {
    use std::io::BufRead;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let stream = UnixStream::connect(socket).map_err(|source| Error::Io {
        path: socket.to_owned(),
        source,
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })?;
    let mut writer = stream.try_clone().map_err(|source| Error::Io {
        path: socket.to_owned(),
        source,
    })?;
    writer
        .write_all(b"{\"execute\":\"guest-sync\",\"arguments\":{\"id\":1}}\n{\"execute\":\"guest-network-get-interfaces\"}\n")
        .map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    for _ in 0..4 {
        line.clear();
        if reader.read_line(&mut line).map_err(|source| Error::Io {
            path: socket.to_owned(),
            source,
        })? == 0
        {
            break;
        }
        let value: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(interfaces) = value.get("return").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for interface in interfaces {
            let Some(addresses) = interface
                .get("ip-addresses")
                .and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            for address in addresses {
                if address
                    .get("ip-address-type")
                    .and_then(serde_json::Value::as_str)
                    == Some("ipv4")
                {
                    let Some(ip) = address
                        .get("ip-address")
                        .and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    if !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                        return Ok(Some(ip.to_owned()));
                    }
                }
            }
        }
    }
    Ok(None)
}
