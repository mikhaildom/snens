use crate::{
    cfg::Config,
    surface_runtime::{self as rt, Session},
};
use std::{fs, path::Path};

/// Merge durable delivery sessions with JSONL transcripts. JSONL-only legacy
/// files never get rewritten as a side effect of discovery.
pub fn sessions(data_dir: &Path, store: &rt::Store) -> crate::R<Vec<Session>> {
    let mut sessions = store.sessions.clone();
    let dir = data_dir.join("sessions");
    if dir.is_dir() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") || !path.is_file() {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|x| x.to_str()) else {
                continue;
            };
            if !rt::valid_id(id) {
                continue;
            }
            if sessions.contains_key(id) {
                continue;
            }
            let surface = crate::log::read_file(&path)
                .ok()
                .and_then(|records| {
                    records.into_iter().find_map(|record| match record {
                        crate::log::Rec::Meta {
                            surface: Some(surface),
                            ..
                        } => Some(surface),
                        _ => None,
                    })
                })
                .unwrap_or_else(|| "legacy".into());
            sessions.insert(
                id.to_string(),
                Session {
                    id: id.to_string(),
                    surface,
                    state: "idle".into(),
                    run_id: None,
                },
            );
        }
    }
    Ok(sessions.into_values().collect())
}

/// Resolve only canonical UUID JSONL paths; caller still checks file existence.
pub fn path(cfg: &Config, id: &str) -> Option<std::path::PathBuf> {
    if !rt::valid_id(id) {
        return None;
    }
    Some(cfg.sessions_path().join(format!("{id}.jsonl")))
}

pub fn load_meta_surface(path: &Path) -> Option<String> {
    crate::log::read_file(path)
        .ok()?
        .into_iter()
        .find_map(|record| match record {
            crate::log::Rec::Meta { surface, .. } => surface,
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("surface-discovery-{}", uuid::Uuid::new_v4()))
    }
    #[test]
    fn finds_legacy_jsonl_without_changing_bytes_and_ignores_non_uuid() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(&file, "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n").unwrap();
        fs::write(dir.join("not-a-session.jsonl"), "bad\n").unwrap();
        let before = fs::read(&file).unwrap();
        let found = sessions(&root, &rt::Store::default()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, id);
        assert_eq!(found[0].surface, "legacy");
        assert_eq!(before, fs::read(&file).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
