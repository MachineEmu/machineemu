use crate::{Error, Result, runtime::process_identity_matches};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

pub(super) fn acquire_workspace_lock(path: &Path) -> Result<File> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            #[cfg(target_os = "linux")]
            {
                let stale = fs::read_to_string(path)
                    .ok()
                    .and_then(|value| {
                        let mut fields = value.split_whitespace();
                        let pid = fields.next()?.parse::<u32>().ok()?;
                        let start = fields.next()?.parse::<u64>().ok()?;
                        Some(!process_identity_matches(pid, start))
                    })
                    .unwrap_or(false);
                if stale {
                    fs::remove_file(path).map_err(|source| Error::Io {
                        path: path.to_owned(),
                        source,
                    })?;
                    return OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)
                        .map_err(|source| Error::Io {
                            path: path.to_owned(),
                            source,
                        });
                }
            }
            Err(Error::WorkspaceLocked(path.to_owned()))
        }
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}
