use machineemu_core::{config::EngineConfig, domain::Id};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct CountedReader<'a> {
    file: fs::File,
    count: &'a Cell<u64>,
}

impl Read for CountedReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.file.read(buffer)?;
        self.count.set(self.count.get() + count as u64);
        Ok(count)
    }
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    track_id: String,
    build_digest: String,
    dirty_source: bool,
    executables: BTreeMap<String, PathBuf>,
    executable_sha256: BTreeMap<String, String>,
    qemu_version: String,
}

fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn registered(workspace: &Path) -> Result<BTreeMap<String, EngineConfig>> {
    let path = workspace.join("engines/registry.json");
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}

/// Extract into private staging, verify every payload file, then publish the registry last.
pub(crate) fn import(
    workspace: &Path,
    source: &Path,
    mut progress: impl FnMut(&str, u64, u64),
) -> Result<serde_json::Value> {
    let root = workspace.join("engines");
    fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    let staging = tempfile::tempdir_in(&root)?;
    let file = fs::File::open(source)?;
    let compressed_total = file.metadata()?.len();
    let compressed_done = Cell::new(0);
    progress("extracting", 0, compressed_total);
    let decoder = flate2::read::GzDecoder::new(CountedReader {
        file,
        count: &compressed_done,
    });
    let mut archive = tar::Archive::new(decoder);
    let mut files = Vec::new();
    let mut bundle_name = None;
    let mut extracted = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !relative(&path) {
            return Err(format!("invalid archive path: {}", path.display()).into());
        }
        let first = path.components().next().unwrap().as_os_str().to_owned();
        if bundle_name.as_ref().is_some_and(|name| name != &first) {
            return Err("archive must contain exactly one engine directory".into());
        }
        bundle_name = Some(first);
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(format!(
                "archive links and special files are unsupported: {}",
                path.display()
            )
            .into());
        }
        if kind.is_file() {
            files.push(path.clone());
        }
        let destination = staging.path().join(&path);
        if destination.exists() && kind.is_file() {
            return Err("duplicate archive file".into());
        }
        if !entry.unpack_in(staging.path())? {
            return Err("archive entry escaped staging".into());
        }
        extracted += entry.size();
        progress("extracting", compressed_done.get(), compressed_total);
    }
    let bundle = staging
        .path()
        .join(bundle_name.ok_or("empty engine archive")?);
    let sums = fs::read_to_string(bundle.join("SHA256SUMS"))?;
    let mut expected = BTreeMap::new();
    for line in sums.lines() {
        let (digest, path) = line.split_once("  ").ok_or("invalid SHA256SUMS entry")?;
        if digest.len() != 64
            || !digest.bytes().all(|c| c.is_ascii_hexdigit())
            || !relative(Path::new(path))
        {
            return Err("invalid SHA256SUMS entry".into());
        }
        if expected
            .insert(PathBuf::from(path), digest.to_owned())
            .is_some()
        {
            return Err("duplicate checksum entry".into());
        }
    }
    let total = extracted;
    let mut verified = 0;
    for path in &files {
        let file = staging.path().join(path);
        let relative = file.strip_prefix(&bundle)?;
        if relative == Path::new("SHA256SUMS") {
            continue;
        }
        let digest = expected
            .remove(relative)
            .ok_or_else(|| format!("missing checksum: {}", relative.display()))?;
        if hash(&file)? != digest {
            return Err(format!("checksum mismatch: {}", relative.display()).into());
        }
        verified += file.metadata()?.len();
        progress("verifying", verified, total);
    }
    if !expected.is_empty() {
        return Err("checksum file names missing payload files".into());
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(bundle.join("engine-build.json"))?)?;
    Id::new("engine track", manifest.track_id.clone())?;
    if manifest.schema_version != 1 || manifest.dirty_source || manifest.executables.is_empty() {
        return Err("engine requires a clean schema-version-1 manifest with executables".into());
    }
    if manifest.build_digest.len() != 64
        || !manifest.build_digest.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err("invalid engine build digest".into());
    }
    for (target, executable) in &manifest.executables {
        let arch = target
            .strip_suffix("-softmmu")
            .ok_or("invalid engine target")?;
        if executable != &PathBuf::from(format!("bin/qemu-system-{arch}")) || !relative(executable)
        {
            return Err("engine executable must be bin/qemu-system-<architecture>".into());
        }
        let expected = manifest
            .executable_sha256
            .get(target)
            .ok_or("missing executable hash")?;
        if &hash(&bundle.join(executable))? != expected {
            return Err("executable checksum mismatch".into());
        }
    }
    let version = manifest
        .qemu_version
        .split_whitespace()
        .skip_while(|s| *s != "version")
        .nth(1)
        .ok_or("missing QEMU version")?
        .to_owned();
    // Include the payload checksums so two differently packaged builds never share a directory.
    let identity = format!("{:x}", Sha256::digest(sums.as_bytes()));
    let destination = root.join(&manifest.track_id).join(identity);
    fs::create_dir_all(destination.parent().unwrap())?;
    if destination.exists() {
        // Re-import is allowed only if the previously installed payload is still intact.
        for path in &files {
            let relative = staging.path().join(path).strip_prefix(&bundle)?.to_owned();
            if hash(&destination.join(&relative))? != hash(&bundle.join(relative))? {
                return Err("installed engine was modified; refusing to reuse it".into());
            }
        }
    } else {
        fs::rename(&bundle, &destination)?;
    }
    let mut registry = registered(workspace)?;
    registry.insert(
        manifest.track_id.clone(),
        EngineConfig {
            path: destination.clone(),
            version: Some(version.clone()),
            build_digest: Some(manifest.build_digest.clone()),
            target: if manifest.executables.len() == 1 {
                manifest.executables.keys().next().cloned()
            } else {
                None
            },
        },
    );
    let mut temporary = tempfile::NamedTempFile::new_in(&root)?;
    temporary.write_all(&serde_json::to_vec_pretty(&registry)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(root.join("registry.json"))?;
    progress("complete", total, total);
    Ok(
        serde_json::json!({"track_id":manifest.track_id,"path":destination,"version":version,"build_digest":manifest.build_digest}),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture(root: &Path, payload: &[u8], corrupt: bool) -> PathBuf {
        let manifest = serde_json::json!({
            "schema_version":1,"track_id":"qemu-10.2-analysis","build_digest":"a".repeat(64),
            "dirty_source":false,"qemu_version":"QEMU emulator version 10.2.4",
            "executables":{"x86_64-softmmu":"bin/qemu-system-x86_64"},
            "executable_sha256":{"x86_64-softmmu":format!("{:x}",Sha256::digest(payload))}
        })
        .to_string();
        let mut files = vec![
            ("bin/qemu-system-x86_64", payload.to_vec()),
            ("engine-build.json", manifest.into_bytes()),
        ];
        let sums = files
            .iter()
            .map(|(path, data)| format!("{:x}  {path}\n", Sha256::digest(data)))
            .collect::<String>();
        files.push(("SHA256SUMS", sums.into_bytes()));
        if corrupt {
            files[0].1.push(b'!');
        }
        let path = root.join("fixture.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut archive = tar::Builder::new(encoder);
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, format!("engine/{name}"), &data[..])
                .unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn engine_import_verifies_and_preserves_old_installations() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let source = fixture(root.path(), b"first", false);
        let first = import(&workspace, &source, |_, _, _| {}).unwrap();
        assert_eq!(first, import(&workspace, &source, |_, _, _| {}).unwrap());
        let source = fixture(root.path(), b"second", false);
        let second = import(&workspace, &source, |_, _, _| {}).unwrap();
        assert_ne!(first["path"], second["path"]);
        assert_eq!(
            fs::read(Path::new(first["path"].as_str().unwrap()).join("bin/qemu-system-x86_64"))
                .unwrap(),
            b"first"
        );
        assert_eq!(
            registered(&workspace).unwrap()["qemu-10.2-analysis"].path,
            PathBuf::from(second["path"].as_str().unwrap())
        );
        let source = fixture(root.path(), b"broken", true);
        assert!(
            import(&workspace, &source, |_, _, _| {})
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
        assert_eq!(
            registered(&workspace).unwrap()["qemu-10.2-analysis"].path,
            PathBuf::from(second["path"].as_str().unwrap())
        );
    }

    #[test]
    fn engine_import_rejects_links_and_unsafe_paths() {
        for path in ["../escape", "/absolute", "a/../../escape", ""] {
            assert!(!relative(Path::new(path)));
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("link.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name("/tmp").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "engine/link", std::io::empty())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        assert!(
            import(&root.path().join("workspace"), &path, |_, _, _| {})
                .unwrap_err()
                .to_string()
                .contains("links and special")
        );
        assert!(!root.path().join("workspace/engines/registry.json").exists());
    }
}
