use super::Workspace;
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

impl Workspace {
    pub fn import_blob(&self, source: impl AsRef<Path>, expected_sha256: &str) -> Result<PathBuf> {
        let source = source.as_ref();
        let expected_sha256 = expected_sha256
            .strip_prefix("sha256:")
            .unwrap_or(expected_sha256);
        let staging = self
            .root
            .join("staging")
            .join(format!("import-{}", std::process::id()));
        let destination = self.root.join("blobs/sha256").join(expected_sha256);
        let mut input = File::open(source).map_err(|source_error| Error::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(|source_error| Error::Io {
                path: staging.clone(),
                source: source_error,
            })?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 1024 * 1024];
        loop {
            let count = input.read(&mut buffer).map_err(|source_error| Error::Io {
                path: source.to_owned(),
                source: source_error,
            })?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|source_error| Error::Io {
                    path: staging.clone(),
                    source: source_error,
                })?;
        }
        output.sync_all().map_err(|source_error| Error::Io {
            path: staging.clone(),
            source: source_error,
        })?;
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected_sha256 {
            let _ = fs::remove_file(&staging);
            return Err(Error::DigestMismatch {
                expected: expected_sha256.to_owned(),
                actual,
            });
        }
        if destination.exists() {
            let _ = fs::remove_file(&staging);
            return Ok(destination);
        }
        fs::rename(&staging, &destination).map_err(|source_error| Error::Io {
            path: destination.clone(),
            source: source_error,
        })?;
        Ok(destination)
    }

    pub fn import_file(&self, source: impl AsRef<Path>) -> Result<String> {
        self.import_blob_computed(source.as_ref())
    }

    pub(super) fn import_blob_computed(&self, source: &Path) -> Result<String> {
        let staging = self
            .root
            .join("staging")
            .join(format!("import-computed-{}", std::process::id()));
        let digest = digest_file(source)?;
        fs::copy(source, &staging).map_err(|source_error| Error::Io {
            path: staging.clone(),
            source: source_error,
        })?;
        let destination = self.root.join("blobs/sha256").join(&digest);
        if destination.exists() {
            let _ = fs::remove_file(&staging);
        } else {
            fs::rename(&staging, &destination).map_err(|source_error| Error::Io {
                path: destination.clone(),
                source: source_error,
            })?;
        }
        Ok(digest)
    }
}
pub(super) fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn digest_file(path: &Path) -> Result<String> {
    let mut input = File::open(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
