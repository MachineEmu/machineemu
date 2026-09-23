use super::Workspace;
use super::blobs::hex_digest;
use crate::domain::{Id, ImageBundleComponent, ImageBundleManifest, ImageManifest};
use crate::{Error, Result};
use rusqlite::params;
use std::{
    fs,
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
            let path = entry.path().join("manifest.json");
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
        let import_named = |path: &Path| -> Result<String> { self.import_blob_computed(path) };
        let disk_sha256 = import_named(&disk)?;
        let firmware_sha256 = firmware
            .is_file()
            .then(|| import_named(&firmware))
            .transpose()?;
        let tpm_state_sha256 = tpm.is_file().then(|| import_named(&tpm)).transpose()?;
        let manifest = ImageManifest {
            image_id,
            engine_track,
            supported_engine_tracks: Vec::new(),
            target: target.into(),
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
        if destination.exists() {
            return Err(Error::BundleExists(destination.to_owned()));
        }
        let image = self.image(image_id)?;
        fs::create_dir_all(destination.join("components")).map_err(|source| Error::Io {
            path: destination.join("components"),
            source,
        })?;
        let mut components = std::collections::BTreeMap::new();
        let mut export_component =
            |name: &str, path: &str, digest: Option<&String>| -> Result<()> {
                let Some(digest) = digest else {
                    return Ok(());
                };
                let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
                let source = self.root.join("blobs/sha256").join(digest);
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
        export_component("disk", "components/disk.qcow2", Some(&image.disk_sha256))?;
        export_component(
            "firmware",
            "components/firmware.fd",
            image.firmware_sha256.as_ref(),
        )?;
        export_component(
            "tpm_state",
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
        let manifest = serde_json::to_vec_pretty(&bundle)?;
        fs::write(destination.join("manifest.json"), manifest).map_err(|source| Error::Io {
            path: destination.join("manifest.json"),
            source,
        })?;
        Ok(bundle)
    }

    pub fn import_image_bundle(&self, source: &Path) -> Result<(ImageManifest, String)> {
        let manifest_path = source.join("manifest.json");
        let manifest: ImageBundleManifest =
            serde_json::from_slice(&fs::read(&manifest_path).map_err(|source_error| {
                Error::Io {
                    path: manifest_path.clone(),
                    source: source_error,
                }
            })?)?;
        if manifest.schema_version != 1 {
            return Err(Error::InvalidBundlePath(
                "unsupported schema_version".into(),
            ));
        }
        let component = |name: &str| -> Result<Option<String>> {
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
            let imported = self.import_blob(&file, &component.sha256)?;
            Ok(imported
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()))
        };
        let disk = component("disk")?
            .ok_or_else(|| Error::InvalidBundlePath("disk component is required".into()))?;
        let image = ImageManifest {
            image_id: manifest.image_id,
            engine_track: manifest.engine_track,
            supported_engine_tracks: manifest.supported_engine_tracks,
            target: manifest.target,
            disk_sha256: disk,
            firmware_sha256: component("firmware")?,
            tpm_state_sha256: component("tpm_state")?,
        };
        let digest = self.register_image(&image)?;
        Ok((image, digest))
    }

    pub fn register_image(&self, manifest: &ImageManifest) -> Result<String> {
        validate_manifest(manifest, manifest.image_id.as_str())?;
        let digest = hex_digest(&serde_json::to_vec(manifest)?);
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

    pub fn image(&self, image_id: &Id) -> Result<ImageManifest> {
        let path = manifest_path(&self.root, image_id)?;
        read_manifest(&path, image_id.as_str())
    }
}

fn manifest_path(root: &Path, id: &Id) -> Result<PathBuf> {
    Id::new("image", id.as_str())?;
    Ok(root.join("images").join(id.as_str()).join("manifest.json"))
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
    for digest in std::iter::once(&image.disk_sha256)
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
    let image = serde_json::from_slice(&bytes)
        .map_err(|error| Error::InvalidBundlePath(format!("{}: {error}", path.display())))?;
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
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        created = true;
        let mut bytes = serde_json::to_vec_pretty(image)?;
        bytes.push(b'\n');
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
