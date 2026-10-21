use crate::{
    cfg::Config,
    surface_runtime::{self as rt, Session},
};
use std::{fs, path::Path};

/// Сколько байт головы лога читать для заголовка. Первое сообщение пользователя
/// лежит в начале, а логи бывают по несколько сотен килобайт — целиком их
/// разбирать на каждый `GET /v1/sessions` слишком дорого.
const HEAD_BYTES: u64 = 64 * 1024;

/// Заголовок, собранный из первых слов первого сообщения пользователя.
/// Используется, когда ядро/поверхность заголовок не сгенерировали.
pub fn title_from_transcript(path: &Path) -> Option<String> {
    const MAX_WORDS: usize = 8;
    const MAX_CHARS: usize = 64;
    let head = crate::log::read_head_bytes(path, HEAD_BYTES).ok()?;
    for rec in head {
        let text = match rec {
            crate::log::Rec::User { text, .. } => text,
            crate::log::Rec::Steer { text, .. } => text,
            _ => continue,
        };
        let words: Vec<&str> = text.split_whitespace().filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            continue;
        }
        let mut out = String::new();
        for w in words.iter().take(MAX_WORDS) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(w);
            if out.chars().count() >= MAX_CHARS {
                break;
            }
        }
        let chars: Vec<char> = out.chars().collect();
        if chars.len() > MAX_CHARS {
            out = chars[..MAX_CHARS].iter().collect::<String>();
            out.push('…');
        } else if words.len() > MAX_WORDS {
            out.push('…');
        }
        return Some(out);
    }
    None
}

/// Заголовок сессии: `Meta.title`, иначе первые слова пользователя. Пустая
/// строка не возвращается — `None` значит «заголовка нет».
pub fn title(path: &Path) -> Option<String> {
    let explicit = crate::log::read_head_bytes(path, HEAD_BYTES)
        .ok()?
        .into_iter()
        .find_map(|rec| match rec {
            crate::log::Rec::Meta { title, .. } => title,
            _ => None,
        })
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    explicit.or_else(|| title_from_transcript(path))
}

/// Поверхность сессии: метка `Meta.surface`, иначе `local`.
///
/// Локальные сессии (`tui`, `tui-local`, `run-child`) — не «legacy»: метку они
/// не пишут, и это шум, а не признак устаревания.
pub fn surface(path: &Path) -> String {
    load_meta_surface(path).unwrap_or_else(|| "local".into())
}

/// Merge durable delivery sessions with JSONL transcripts. JSONL-only sessions
/// never get rewritten as a side effect of discovery.
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
            let surface = surface(&path);
            let mut session = Session::new(id.to_string(), surface);
            session.title = title(&path);
            sessions.insert(id.to_string(), session);
        }
    }
    // Заголовок нужен и сессиям, которые уже в store: у старых записей его нет.
    for session in sessions.values_mut() {
        if session.title.is_none() {
            if let Some(path) = path_from(data_dir, &session.id) {
                session.title = title(&path);
            }
        }
    }
    Ok(sessions.into_values().collect())
}

fn path_from(data_dir: &Path, id: &str) -> Option<std::path::PathBuf> {
    let p = data_dir.join("sessions").join(format!("{id}.jsonl"));
    p.is_file().then_some(p)
}

/// Resolve only canonical UUID JSONL paths; caller still checks file existence.
pub fn path(cfg: &Config, id: &str) -> Option<std::path::PathBuf> {
    if !rt::valid_id(id) {
        return None;
    }
    Some(cfg.sessions_path().join(format!("{id}.jsonl")))
}

pub fn load_meta_surface(path: &Path) -> Option<String> {
    crate::log::read_head_bytes(path, HEAD_BYTES)
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
    fn finds_jsonl_without_changing_bytes_and_ignores_non_uuid() {
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
        assert_eq!(found[0].surface, "local");
        assert_eq!(before, fs::read(&file).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn title_falls_back_to_first_user_words() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(
            &file,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"хочу веб-интерфейс snens, из которого потом сделать десктопное приложение и жить долго\",\"source\":\"surface\"}\n"
            ),
        )
        .unwrap();
        let found = sessions(&root, &rt::Store::default()).unwrap();
        let t = found[0].title.clone().unwrap();
        assert!(t.starts_with("хочу веб-интерфейс snens,"), "{t}");
        assert!(t.ends_with('…'), "{t}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_meta_title_wins_over_transcript() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(
            &file,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"title\":\"мок веб-клиента\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"совсем другой текст\",\"source\":\"surface\"}\n"
            ),
        )
        .unwrap();
        let found = sessions(&root, &rt::Store::default()).unwrap();
        assert_eq!(found[0].title.as_deref(), Some("мок веб-клиента"));
        fs::remove_dir_all(root).unwrap();
    }
}
