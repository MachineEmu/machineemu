use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

pub(super) fn copy_and_hash(source: &Path, destination: &Path, mut output: File) -> Result<String> {
    let mut input = File::open(source).map_err(|source_error| Error::Io {
        path: source.to_owned(),
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
        output
            .write_all(&buffer[..count])
            .map_err(|source_error| Error::Io {
                path: destination.to_owned(),
                source: source_error,
            })?;
        hasher.update(&buffer[..count]);
    }
    output.sync_all().map_err(|source_error| Error::Io {
        path: destination.to_owned(),
        source: source_error,
    })?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub(super) fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
