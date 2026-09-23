use super::Error;
use serde_json::Value;
use std::{fs, path::Path};

pub fn load_document(path: &Path) -> Result<Value, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    match path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => serde_json::from_str(&text).map_err(|e| Error::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        }),
        "yaml" | "yml" => serde_yaml::from_str(&text).map_err(|e| Error::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        }),
        suffix => Err(Error::Invalid(format!(
            "unsupported configuration format .{suffix}"
        ))),
    }
}
