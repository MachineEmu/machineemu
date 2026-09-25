use super::Workspace;
use super::digests::{copy_and_hash, hex_digest};
use crate::domain::{Id, ImageBundleComponent, ImageBundleManifest, ImageManifest};
use crate::{Error, Result};
use rusqlite::params;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

impl Workspace {
    /// Read editable manifests without opening SQLite or acquiring the writer lock.
    pub fn list_images(root: impl AsRef<Path>) -> Result<Vec<ImageManifest>> {
        let root = root.as_ref();
        let directory = root.join("images");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(Error::Io {
                    path: directory,
                    source,
                });
            }
        };
        let mut images = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::Io {
                path: directory.clone(),
                source,
            })?;
            let path = manifest_path_for_dir(&entry.path());
            if !entry
                .file_type()
                .map_err(|source| Error::Io {
                    path: entry.path(),
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            // Incomplete imports have no published manifest yet.
            match fs::metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(Error::Io { path, source }),
                Ok(_) => {}
            }
            images.push(read_manifest(&path, &id)?);
        }
        images.sort_by(|a, b| a.image_id.as_str().cmp(b.image_id.as_str()));
        Ok(images)
    }

    /// Export legacy metadata before replacing the image table with an ID index.
    pub(super) fn migrate_image_manifests(&self) -> Result<()> {
        let legacy: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('images') WHERE name = 'manifest_json')",
            [],
            |row| row.get(0),
        )?;
        if !legacy {
            return Ok(());
        }
        let mut query = self
            .db
            .prepare("SELECT image_id, manifest_json FROM images ORDER BY image_id")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (id, bytes) = row?;
            let image: ImageManifest = serde_json::from_slice(&bytes)?;
            validate_manifest(&image, &id)?;
            let path = manifest_path(&self.root, &image.image_id)?;
            match fs::metadata(&path) {
                Ok(_) => {
                    read_manifest(&path, &id)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    publish_manifest(&path, &image)?;
                }
                Err(source) => return Err(Error::Io { path, source }),
            }
        }
        drop(query);
        // Keep only IDs for the existing instances.image_id foreign key.
        // Files are durable before removing legacy configuration; a failed
        // migration can safely retry without replacing hand-edited manifests.
        self.db.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let migration = self.db.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE images_file_index (image_id TEXT PRIMARY KEY);
             INSERT INTO images_file_index SELECT image_id FROM images;
             DROP TABLE images;
             ALTER TABLE images_file_index RENAME TO images;
             UPDATE schema_version SET version = 2;
             COMMIT;",
        );
        if migration.is_err() {
            let _ = self.db.execute_batch("ROLLBACK;");
        }
        self.db.execute_batch("PRAGMA foreign_keys = ON;")?;
        migration?;
        Ok(())
    }

    /// Imports a vmmanager-sh base directory without treating an instance
    /// overlay as the reusable image. The TPM lock and PID files are omitted;
    /// only the persistent TPM state file is imported.
    pub fn import_vmmanager_base(
        &self,
        source: impl AsRef<Path>,
        image_id: Id,
        engine_track: Id,
        target: impl Into<String>,
    ) -> Result<ImageManifest> {
        let source = source.as_ref();
        let disk = source.join("disk.qcow2");
        let firmware = source.join("OVMF_VARS.fd");
        let tpm = source.join("tpm/tpm2-00.permall");
        if !disk.is_file() {
            return Err(Error::InvalidBundlePath(disk.display().to_string()));
        }
        let image_root = image_root(&self.root, &image_id)?;
        let components = image_root.join("components");
        fs::create_dir_all(&components).map_err(|source| Error::Io {
            path: components.clone(),
            source,
        })?;
        let disk_sha256 = publish_component(&disk, &components.join("disk.qcow2"), None)?;
        let firmware_sha256 = firmware
            .is_file()
            .then(|| publish_component(&firmware, &components.join("firmware.fd"), None))
            .transpose()?;
        let tpm_state_sha256 = tpm
            .is_file()
            .then(|| publish_component(&tpm, &components.join("tpm-state"), None))
            .transpose()?;
        let manifest = ImageManifest {
            image_id,
            engine_track,
            supported_engine_tracks: Vec::new(),
            target: target.into(),
            components: [(
                "disk".to_owned(),
                ImageBundleComponent {
                    path: "components/disk.qcow2".into(),
                    sha256: format!("sha256:{disk_sha256}"),
                },
            )]
            .into_iter()
            .chain(firmware_sha256.iter().map(|digest| {
                (
                    "firmware".to_owned(),
                    ImageBundleComponent {
                        path: "components/firmware.fd".into(),
                        sha256: format!("sha256:{digest}"),
                    },
                )
            }))
            .chain(tpm_state_sha256.iter().map(|digest| {
                (
                    "tpm_state".to_owned(),
                    ImageBundleComponent {
                        path: "components/tpm-state".into(),
                        sha256: format!("sha256:{digest}"),
                    },
                )
            }))
            .collect(),
            disk_sha256,
            firmware_sha256,
            tpm_state_sha256,
        };
        self.register_image(&manifest)?;
        Ok(manifest)
    }

    pub fn export_image_bundle(
        &self,
        image_id: &Id,
        destination: &Path,
    ) -> Result<ImageBundleManifest> {
        Self::export_image_bundle_from_root(&self.root, image_id, destination)
    }

    pub fn export_image_bundle_from_root(
        root: impl AsRef<Path>,
        image_id: &Id,
        destination: &Path,
    ) -> Result<ImageBundleManifest> {
        let root = root.as_ref();
        if destination.exists() {
            return Err(Error::BundleExists(destination.to_owned()));
        }
        let image = read_manifest(&manifest_path(root, image_id)?, image_id.as_str())?;
        fs::create_dir_all(destination.join("components")).map_err(|source| Error::Io {
            path: destination.join("components"),
            source,
        })?;
        let image_root = image_root(root, &image.image_id)?;
        let mut components = std::collections::BTreeMap::new();
        let mut export_component =
            |name: &str, source_name: &str, path: &str, digest: Option<&String>| -> Result<()> {
                let Some(digest) = digest else {
                    return Ok(());
                };
                let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
                let source = image_root.join("components").join(source_name);
                verify_component(&source, digest)?;
                let target = destination.join(path);
                fs::copy(&source, &target).map_err(|source_error| Error::Io {
                    path: target.clone(),
                    source: source_error,
                })?;
                components.insert(
                    name.to_owned(),
                    ImageBundleComponent {
                        path: path.to_owned(),
                        sha256: format!("sha256:{digest}"),
                    },
                );
                Ok(())
            };
        export_component(
            "disk",
            "disk.qcow2",
            "components/disk.qcow2",
            Some(&image.disk_sha256),
        )?;
        export_component(
            "firmware",
            "firmware.fd",
            "components/firmware.fd",
            image.firmware_sha256.as_ref(),
        )?;
        export_component(
            "tpm_state",
            "tpm-state",
            "components/tpm-state",
            image.tpm_state_sha256.as_ref(),
        )?;
        let bundle = ImageBundleManifest {
            schema_version: 1,
            image_id: image.image_id,
            engine_track: image.engine_track,
            supported_engine_tracks: image.supported_engine_tracks,
            target: image.target,
            components,
        };
        let manifest =
            serde_yaml::to_string(&bundle).map_err(|error| Error::Process(error.to_string()))?;
        fs::write(destination.join("manifest.yaml"), manifest).map_err(|source| Error::Io {
            path: destination.join("manifest.yaml"),
            source,
        })?;
        Ok(bundle)
    }

    pub fn import_image_bundle(&self, source: &Path) -> Result<(ImageManifest, String)> {
        let manifest_path = source.join("manifest.yaml");
        let manifest: ImageBundleManifest =
            serde_yaml::from_slice(&fs::read(&manifest_path).map_err(|source_error| {
                Error::Io {
                    path: manifest_path.clone(),
                    source: source_error,
                }
            })?)
            .map_err(|error| {
                Error::InvalidBundlePath(format!("{}: {error}", manifest_path.display()))
            })?;
        if manifest.schema_version != 1 {
            return Err(Error::InvalidBundlePath(
                "unsupported schema_version".into(),
            ));
        }
        let image_id = manifest.image_id;
        let image_root = image_root(&self.root, &image_id)?;
        let components_dir = image_root.join("components");
        fs::create_dir_all(&components_dir).map_err(|source| Error::Io {
            path: components_dir.clone(),
            source,
        })?;
        let component = |name: &str, destination_name: &str| -> Result<Option<String>> {
            let Some(component) = manifest.components.get(name) else {
                return Ok(None);
            };
            let path = Path::new(&component.path);
            if path.is_absolute()
                || path
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
            {
                return Err(Error::InvalidBundlePath(component.path.clone()));
            }
            let file = source.join(path);
            if !file.is_file() {
                return Err(Error::InvalidBundlePath(component.path.clone()));
            }
            let digest = publish_component(
                &file,
                &components_dir.join(destination_name),
                Some(&component.sha256),
            )?;
            Ok(Some(digest))
        };
        let disk = component("disk", "disk.qcow2")?
            .ok_or_else(|| Error::InvalidBundlePath("disk component is required".into()))?;
        let image = ImageManifest {
            image_id,
            engine_track: manifest.engine_track,
            supported_engine_tracks: manifest.supported_engine_tracks,
            target: manifest.target,
            components: manifest.components.clone(),
            disk_sha256: disk,
            firmware_sha256: component("firmware", "firmware.fd")?,
            tpm_state_sha256: component("tpm_state", "tpm-state")?,
        };
        let digest = self.register_image(&image)?;
        Ok((image, digest))
    }

    pub fn register_image(&self, manifest: &ImageManifest) -> Result<String> {
        validate_manifest(manifest, manifest.image_id.as_str())?;
        let digest = hex_digest(&bytes_for_digest(manifest)?);
        let path = manifest_path(&self.root, &manifest.image_id)?;
        match fs::metadata(&path) {
            Ok(_) => {
                if read_manifest(&path, manifest.image_id.as_str())? != *manifest {
                    return Err(Error::ImageConflict(manifest.image_id.as_str().to_owned()));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                publish_manifest(&path, manifest)?;
            }
            Err(source) => return Err(Error::Io { path, source }),
        }
        self.db.execute(
            "INSERT OR IGNORE INTO images(image_id) VALUES (?1)",
            params![manifest.image_id.as_str()],
        )?;
        Ok(digest)
    }

    pub fn replace_image_manifest(&self, manifest: &ImageManifest) -> Result<String> {
        validate_manifest(manifest, manifest.image_id.as_str())?;
        let path = manifest_path(&self.root, &manifest.image_id)?;
        if !path.is_file() {
            return Err(Error::NotFound {
                kind: "image",
                id: manifest.image_id.as_str().to_owned(),
            });
        }
        let directory = path.parent().expect("manifest has parent");
        let canonical_root = fs::canonicalize(&self.root).map_err(|source| Error::Io {
            path: self.root.clone(),
            source,
        })?;
        let canonical_directory = fs::canonicalize(directory).map_err(|source| Error::Io {
            path: directory.to_owned(),
            source,
        })?;
        if !canonical_directory.starts_with(canonical_root) {
            return Err(Error::InvalidBundlePath(
                "image manifest directory escapes the workspace".into(),
            ));
        }
        let temporary = directory.join(format!(
            ".manifest-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let bytes = serde_yaml::to_string(manifest)
            .map_err(|error| Error::Process(error.to_string()))?
            .into_bytes();
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            fs::File::open(directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok(hex_digest(&bytes_for_digest(manifest)?))
    }

    pub fn image(&self, image_id: &Id) -> Result<ImageManifest> {
        let path = manifest_path(&self.root, image_id)?;
        read_manifest(&path, image_id.as_str())
    }
}

fn bytes_for_digest<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_yaml::to_string(value)
        .map(|text| text.into_bytes())
        .map_err(|error| Error::Process(error.to_string()))
}

fn image_root(root: &Path, id: &Id) -> Result<PathBuf> {
    Id::new("image", id.as_str())?;
    Ok(root.join("images").join(id.as_str()))
}

fn manifest_path(root: &Path, id: &Id) -> Result<PathBuf> {
    Ok(manifest_path_for_dir(&image_root(root, id)?))
}

fn manifest_path_for_dir(directory: &Path) -> PathBuf {
    directory.join("manifest.yaml")
}

fn hash_file(path: &Path) -> Result<String> {
    let staging = path.with_file_name(format!(
        ".hash-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let output = File::create(&staging).map_err(|source| Error::Io {
        path: staging.clone(),
        source,
    })?;
    let result = copy_and_hash(path, &staging, output);
    let _ = fs::remove_file(&staging);
    result
}

fn verify_component(path: &Path, expected_sha256: &str) -> Result<()> {
    let expected_sha256 = expected_sha256
        .strip_prefix("sha256:")
        .unwrap_or(expected_sha256);
    let actual = hash_file(path)?;
    if actual != expected_sha256 {
        return Err(Error::DigestMismatch {
            expected: expected_sha256.to_owned(),
            actual,
        });
    }
    Ok(())
}

fn publish_component(
    source: &Path,
    destination: &Path,
    expected_sha256: Option<&str>,
) -> Result<String> {
    let expected_sha256 =
        expected_sha256.map(|digest| digest.strip_prefix("sha256:").unwrap_or(digest));
    if destination.is_file() {
        let digest = hash_file(destination)?;
        if let Some(expected) = expected_sha256
            && digest != expected
        {
            return Err(Error::DigestMismatch {
                expected: expected.to_owned(),
                actual: digest,
            });
        }
        return Ok(digest);
    }
    let directory = destination.parent().expect("component has parent");
    fs::create_dir_all(directory).map_err(|source| Error::Io {
        path: directory.to_owned(),
        source,
    })?;
    let temporary = directory.join(format!(
        ".component-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| Error::Io {
            path: temporary.clone(),
            source,
        })?;
    let result = (|| -> Result<String> {
        let digest = copy_and_hash(source, &temporary, output)?;
        if let Some(expected) = expected_sha256
            && digest != expected
        {
            return Err(Error::DigestMismatch {
                expected: expected.to_owned(),
                actual: digest,
            });
        }
        fs::rename(&temporary, destination).map_err(|source| Error::Io {
            path: destination.to_owned(),
            source,
        })?;
        Ok(digest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn validate_manifest(image: &ImageManifest, expected_id: &str) -> Result<()> {
    Id::new("image", image.image_id.as_str())?;
    Id::new("engine track", image.engine_track.as_str())?;
    for track in &image.supported_engine_tracks {
        Id::new("engine track", track.as_str())?;
    }
    if image.image_id.as_str() != expected_id {
        return Err(Error::InvalidBundlePath(format!(
            "manifest image_id {:?} must match directory {expected_id:?}",
            image.image_id.as_str()
        )));
    }
    for digest in image
        .components
        .values()
        .map(|component| &component.sha256)
        .chain((!image.disk_sha256.is_empty()).then_some(&image.disk_sha256))
        .chain(image.firmware_sha256.iter())
        .chain(image.tpm_state_sha256.iter())
    {
        let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::InvalidBundlePath(
                "image manifest contains an invalid SHA-256 digest".into(),
            ));
        }
    }
    Ok(())
}

fn read_manifest(path: &Path, id: &str) -> Result<ImageManifest> {
    let bytes = fs::read(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let value: serde_json::Value = serde_yaml::from_slice(&bytes)
        .map_err(|error| Error::InvalidBundlePath(format!("{}: {error}", path.display())))?;
    let value = value.get("spec").cloned().unwrap_or(value);
    let mut image: ImageManifest = serde_json::from_value(value)
        .map_err(|error| Error::InvalidBundlePath(format!("{}: {error}", path.display())))?;
    if !image.components.is_empty() {
        if image.disk_sha256.is_empty()
            && let Some(component) = image.components.get("disk")
        {
            image.disk_sha256 = component
                .sha256
                .strip_prefix("sha256:")
                .unwrap_or(&component.sha256)
                .to_owned();
        }
        if image.firmware_sha256.is_none()
            && let Some(component) = image.components.get("firmware")
        {
            image.firmware_sha256 = Some(
                component
                    .sha256
                    .strip_prefix("sha256:")
                    .unwrap_or(&component.sha256)
                    .to_owned(),
            );
        }
        if image.tpm_state_sha256.is_none()
            && let Some(component) = image.components.get("tpm_state")
        {
            image.tpm_state_sha256 = Some(
                component
                    .sha256
                    .strip_prefix("sha256:")
                    .unwrap_or(&component.sha256)
                    .to_owned(),
            );
        }
    }
    validate_manifest(&image, id)?;
    Ok(image)
}

fn publish_manifest(path: &Path, image: &ImageManifest) -> Result<()> {
    let directory = path.parent().expect("manifest has parent");
    fs::create_dir_all(directory).map_err(|source| Error::Io {
        path: directory.to_owned(),
        source,
    })?;
    let temporary = directory.join(format!(".manifest-{}.tmp", std::process::id()));
    let bytes = serde_yaml::to_string(image)
        .map_err(|error| Error::Process(error.to_string()))?
        .into_bytes();
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        created = true;
        file.write_all(&bytes)?;
        file.sync_all()?;
        // A hard link publishes atomically without overwriting an operator's file.
        fs::hard_link(&temporary, path)?;
        fs::remove_file(&temporary)?;
        fs::File::open(directory)?.sync_all()?;
        if let Some(parent) = directory.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}
