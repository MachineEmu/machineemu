#[cfg(test)]
use machineemu_core::engine::load_document;
use machineemu_core::{domain::ImageManifest, engine::Error};
use serde::Serialize;
#[cfg(test)]
use std::{
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

pub(super) async fn images(daemon: &str, token: &str, json: bool) -> Result<(), Error> {
    let response = super::daemon_request(daemon, token, "GET", "/api/v2/images", None).await?;
    if json {
        return print_json(&response);
    }
    let images: Vec<ImageManifest> = serde_json::from_value(response)
        .map_err(|error| Error::Runtime(format!("invalid image list from daemon: {error}")))?;
    println!("{:<28} {:<24} TARGET", "IMAGE", "ENGINE TRACKS");
    for image in images {
        let mut tracks = vec![image.engine_track.as_str()];
        for track in &image.supported_engine_tracks {
            if !tracks.contains(&track.as_str()) {
                tracks.push(track.as_str());
            }
        }
        println!(
            "{:<28} {:<24} {}",
            image.image_id.as_str(),
            tracks.join(","),
            image.target
        );
    }
    Ok(())
}

#[cfg(test)]
#[derive(Serialize)]
struct ProfileSummary {
    // The filename stem is the name accepted by `machineemu run`.
    id: String,
    name: String,
    target: String,
    path: PathBuf,
}

#[cfg(test)]
fn list_profiles(workspace: &Path) -> Result<Vec<ProfileSummary>, Error> {
    let directory = workspace.join("profiles");
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
    let mut paths = Vec::new();
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
            paths.push((id.to_owned(), path));
        }
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    paths
        .into_iter()
        .map(|(id, path)| {
            let document = load_document(&path)?;
            Ok(ProfileSummary {
                name: document["name"].as_str().unwrap_or(&id).to_owned(),
                target: document["target"].as_str().unwrap_or("-").to_owned(),
                id,
                path,
            })
        })
        .collect()
}

pub(super) async fn profiles(daemon: &str, token: &str, json: bool) -> Result<(), Error> {
    let response = super::daemon_request(daemon, token, "GET", "/api/v2/profiles", None).await?;
    if json {
        return print_json(&response);
    }
    let profiles: Vec<serde_json::Value> = serde_json::from_value(response)
        .map_err(|error| Error::Runtime(format!("invalid profile list from daemon: {error}")))?;
    println!("{:<28} {:<20}", "PROFILE", "TARGET");
    for profile in profiles {
        println!(
            "{:<28} {:<20}",
            profile_id(&profile).unwrap_or("?"),
            profile_target(&profile).unwrap_or("?")
        );
    }
    Ok(())
}

fn profile_id(profile: &serde_json::Value) -> Option<&str> {
    profile
        .get("id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            profile
                .pointer("/metadata/name")
                .and_then(serde_json::Value::as_str)
        })
}

fn profile_target(profile: &serde_json::Value) -> Option<&str> {
    profile
        .get("target")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            profile
                .pointer("/spec/target")
                .and_then(serde_json::Value::as_str)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_list_workspace_documents_only() {
        let root =
            std::env::temp_dir().join(format!("machineemu-inventory-{}", std::process::id()));
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join("profiles")).unwrap();
        fs::write(
            workspace.join("profiles/demo.json"),
            r#"{"id":"different-id","name":"Demo","target":"x86_64-softmmu"}"#,
        )
        .unwrap();
        let profiles = list_profiles(&workspace).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "demo");
        assert_eq!(profiles[0].name, "Demo");
        assert!(list_profiles(&root.join("missing")).unwrap().is_empty());
        fs::write(workspace.join("profiles/broken.json"), "invalid").unwrap();
        assert!(list_profiles(&workspace).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_summary_supports_native_documents() {
        let profile = serde_json::json!({
            "metadata": {"name": "default-uefi"},
            "spec": {"target": "x86_64-softmmu"}
        });
        assert_eq!(profile_id(&profile), Some("default-uefi"));
        assert_eq!(profile_target(&profile), Some("x86_64-softmmu"));
    }
}
