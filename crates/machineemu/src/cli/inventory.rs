use machineemu_core::{
    engine::{Error, load_document},
    storage::Workspace,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn print_json(value: &impl Serialize) -> Result<(), Error> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| Error::Invalid(error.to_string()))?
    );
    Ok(())
}

pub(super) fn images(workspace: &Path, json: bool) -> Result<(), Error> {
    let images =
        Workspace::list_images(workspace).map_err(|error| Error::Runtime(error.to_string()))?;
    if json {
        return print_json(&images);
    }
    println!(
        "{:<28} {:<24} {:<20} MANIFEST",
        "IMAGE", "ENGINE TRACKS", "TARGET"
    );
    for image in images {
        let mut tracks = vec![image.engine_track.as_str()];
        for track in &image.supported_engine_tracks {
            if !tracks.contains(&track.as_str()) {
                tracks.push(track.as_str());
            }
        }
        println!(
            "{:<28} {:<24} {:<20} {}",
            image.image_id.as_str(),
            tracks.join(","),
            image.target,
            workspace
                .join("images")
                .join(image.image_id.as_str())
                .join("manifest.json")
                .display()
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct ProfileSummary {
    // The filename stem is the name accepted by `machineemu run`.
    id: String,
    name: String,
    target: String,
    source: &'static str,
    path: PathBuf,
}

fn list_profiles(workspace: &Path, bundled: &Path) -> Result<Vec<ProfileSummary>, Error> {
    let mut paths = BTreeMap::new();
    for (directory, source) in [
        (bundled.to_owned(), "bundled"),
        (workspace.join("profiles"), "workspace"),
    ] {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::Io {
                    path: directory,
                    source,
                });
            }
        };
        for entry in entries {
            let path = entry
                .map_err(|source| Error::Io {
                    path: directory.clone(),
                    source,
                })?
                .path();
            // Match the JSON filename resolution used by named launches.
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") || !path.is_file() {
                continue;
            }
            if let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) {
                paths.insert(id.to_owned(), (path, source));
            }
        }
    }
    paths
        .into_iter()
        .map(|(id, (path, source))| {
            let document = load_document(&path)?;
            Ok(ProfileSummary {
                name: document["name"].as_str().unwrap_or(&id).to_owned(),
                target: document["target"].as_str().unwrap_or("-").to_owned(),
                id,
                source,
                path,
            })
        })
        .collect()
}

pub(super) fn profiles(workspace: &Path, json: bool) -> Result<(), Error> {
    let profiles = list_profiles(workspace, Path::new("profiles"))?;
    if json {
        return print_json(&profiles);
    }
    println!("{:<28} {:<20} {:<10} PATH", "PROFILE", "TARGET", "SOURCE");
    for profile in profiles {
        println!(
            "{:<28} {:<20} {:<10} {}",
            profile.id,
            profile.target,
            profile.source,
            profile.path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_match_launch_precedence_and_ignore_shadowed_documents() {
        let root =
            std::env::temp_dir().join(format!("machineemu-inventory-{}", std::process::id()));
        let workspace = root.join("workspace");
        let bundled = root.join("bundled");
        fs::create_dir_all(workspace.join("profiles")).unwrap();
        fs::create_dir_all(&bundled).unwrap();
        fs::write(bundled.join("demo.json"), "invalid shadowed document").unwrap();
        fs::write(
            bundled.join("zebra.json"),
            r#"{"target":"aarch64-softmmu"}"#,
        )
        .unwrap();
        fs::write(
            workspace.join("profiles/demo.json"),
            r#"{"id":"different-id","name":"Demo","target":"x86_64-softmmu"}"#,
        )
        .unwrap();
        let profiles = list_profiles(&workspace, &bundled).unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, "demo");
        assert_eq!(profiles[0].source, "workspace");
        assert_eq!(profiles[0].name, "Demo");
        assert_eq!(profiles[1].id, "zebra");
        assert_eq!(profiles[1].source, "bundled");
        assert!(
            list_profiles(&root.join("missing"), &root.join("absent"))
                .unwrap()
                .is_empty()
        );
        fs::write(bundled.join("broken.json"), "invalid").unwrap();
        assert!(list_profiles(&workspace, &bundled).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
