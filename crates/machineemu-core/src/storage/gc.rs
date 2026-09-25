//! Reference-aware cleanup for the workspace content-addressed object store.

use super::Workspace;
use crate::{Error, Result};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GarbageCollectionReport {
    pub dry_run: bool,
    pub retained: Vec<String>,
    pub candidates: Vec<String>,
    pub removed: Vec<String>,
}

pub type GarbageCollectionResult = GarbageCollectionReport;

pub(super) fn collect(workspace: &Workspace, dry_run: bool) -> Result<GarbageCollectionResult> {
    let object_root = workspace.root().join("objects").join("sha256");
    let mut objects = Vec::new();
    let entries = match fs::read_dir(&object_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GarbageCollectionReport {
                dry_run,
                retained: Vec::new(),
                candidates: Vec::new(),
                removed: Vec::new(),
            });
        }
        Err(source) => {
            return Err(Error::Io {
                path: object_root,
                source,
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: object_root.clone(),
            source,
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_digest(&name) {
                objects.push((name, path));
            }
        }
    }
    objects.sort_by(|a, b| a.0.cmp(&b.0));

    let retained = referenced_digests(workspace.root(), &objects)?;
    let mut report = GarbageCollectionReport {
        dry_run,
        retained: objects
            .iter()
            .filter(|(digest, _)| retained.contains(digest))
            .map(|(digest, _)| digest.clone())
            .collect(),
        candidates: objects
            .iter()
            .filter(|(digest, _)| !retained.contains(digest))
            .map(|(digest, _)| digest.clone())
            .collect(),
        removed: Vec::new(),
    };
    if !dry_run {
        for digest in &report.candidates {
            let path = object_root.join(digest);
            match fs::remove_file(&path) {
                Ok(()) => report.removed.push(digest.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        if !report.removed.is_empty() {
            fs::File::open(&object_root)
                .and_then(|file| file.sync_all())
                .map_err(|source| Error::Io {
                    path: object_root,
                    source,
                })?;
        }
    }
    Ok(report)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn referenced_digests(root: &Path, objects: &[(String, PathBuf)]) -> Result<BTreeSet<String>> {
    let mut retained = BTreeSet::new();
    scan_metadata(root, root, objects, &mut retained)?;
    Ok(retained)
}

fn scan_metadata(
    root: &Path,
    directory: &Path,
    objects: &[(String, PathBuf)],
    retained: &mut BTreeSet<String>,
) -> Result<()> {
    let entries = fs::read_dir(directory).map_err(|source| Error::Io {
        path: directory.to_owned(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            if path == root.join("objects") {
                continue;
            }
            scan_metadata(root, &path, objects, retained)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let metadata = entry.metadata().map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.len() > MAX_METADATA_BYTES {
            continue;
        }
        let bytes = fs::read(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let text = String::from_utf8_lossy(&bytes);
        for (digest, _) in objects {
            if text.contains(digest) {
                retained.insert(digest.clone());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn workspace() -> (Workspace, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "machineemu-gc-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let workspace = Workspace::open(&path).expect("workspace");
        (workspace, path)
    }

    #[test]
    fn dry_run_retains_metadata_and_removes_only_unreferenced_objects() {
        let (workspace, path) = workspace();
        let retained = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let dead = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let object_dir = path.join("objects/sha256");
        fs::create_dir_all(&object_dir).expect("object dir");
        fs::write(object_dir.join(retained), b"live").expect("live");
        fs::write(object_dir.join(dead), b"dead").expect("dead");
        fs::create_dir_all(path.join("staging/create")).expect("staging");
        fs::write(
            path.join("staging/create/domain_document.yaml"),
            format!("artifact: objects/sha256/{retained}\n"),
        )
        .expect("metadata");

        let report = workspace.collect_garbage(true).expect("dry run");
        assert_eq!(report.retained, vec![retained]);
        assert_eq!(report.candidates, vec![dead]);
        assert!(report.removed.is_empty());
        assert!(object_dir.join(dead).is_file());

        let report = workspace.collect_garbage(false).expect("collect");
        assert_eq!(report.removed, vec![dead]);
        assert!(object_dir.join(retained).is_file());
        assert!(!object_dir.join(dead).exists());
        drop(workspace);
        let _ = fs::remove_dir_all(path);
    }
}
