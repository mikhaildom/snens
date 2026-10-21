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
            if !has_dialogue(&path) {
                continue;
            }
            let surface = surface(&path);
            let mut session = Session::new(id.to_string(), surface);
            session.title = title(&path);
            session.updated_at = mtime(&path);
            session.parent = parent(&path);
            session.model = model(&path);
            sessions.insert(id.to_string(), session);
        }
    }
    // Сессии из store тоже фильтруются: там могли остаться огрызки, созданные
    // до того, как discovery научился их отбрасывать.
    sessions.retain(|id, _| path_from(data_dir, id).is_some_and(|p| has_dialogue(&p)));
    // Заголовок и поверхность нужны и сессиям, которые уже в store: старые
    // записи хранят "legacy" (устаревшее имя локальной сессии) и не знают
    // заголовка. Метка из `Meta` точнее store — она пишется при создании лога
    // и не переписывается; store достаётся, только если метки в логе нет.
    for session in sessions.values_mut() {
        let file = path_from(data_dir, &session.id);
        if let Some(path) = &file {
            if let Some(marked) = load_meta_surface(path) {
                session.surface = marked;
            }
        }
        if session.surface == "legacy" {
            session.surface = "local".into();
        }
        if session.title.is_none() {
            if let Some(path) = &file {
                session.title = title(path);
            }
        }
        session.updated_at = file.as_ref().and_then(|p| mtime(p));
        if session.parent.is_none() {
            session.parent = file.as_ref().and_then(|p| parent(p));
        }
        if session.model.is_none() {
            session.model = file.as_ref().and_then(|p| model(p));
        }
    }
    // Свежие сверху. Порядок не зависит от поверхности и от формы id
    // (`1234-567` и uuid иначе перемешивались лексикографически).
    let mut out: Vec<Session> = sessions.into_values().collect();
    out.sort_by(|a, b| {
        b.updated_at
            .unwrap_or(0)
            .cmp(&a.updated_at.unwrap_or(0))
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(out)
}

/// Модель сессии: `Meta.model`, иначе последняя запись `usage` с непустым
/// полем (появилось позже `Meta`), иначе `None` — у старых логов её нет.
pub fn model(path: &Path) -> Option<String> {
    let head = crate::log::read_head(path, 4).ok()?;
    if let Some(m) = head.into_iter().find_map(|rec| match rec {
        crate::log::Rec::Meta { model, .. } => Some(model),
        _ => None,
    }) {
        let m = m.trim();
        if !m.is_empty() {
            return Some(m.to_string());
        }
    }
    // Хвост лога: последний `usage` с моделью. Читаем с конца, но простым
    // способом — файл открывается один раз и парсится построчно.
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines().rev() {
        if !line.contains("\"model\"") {
            continue;
        }
        if let Ok(crate::log::Rec::Usage { model, .. }) = serde_json::from_str(line) {
            let model = model.trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }
    None
}

/// Родитель сессии из `Meta` (пусто у пользовательских сессий).
pub fn parent(path: &Path) -> Option<String> {
    crate::log::read_head(path, 4)
        .ok()?
        .into_iter()
        .find_map(|rec| match rec {
            crate::log::Rec::Meta { parent, .. } => Some(parent),
            _ => None,
        })
        .filter(|p| !p.trim().is_empty())
}

/// Была ли в сессии хоть одна реплика человека или вставка в ход. Сессии без
/// них — огрызки (создали и бросили, упал запуск): в списке им делать нечего.
pub fn has_dialogue(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| {
        line.starts_with("{\"kind\":\"user\"") || line.starts_with("{\"kind\":\"steer\"")
    })
}

/// Время последней активности сессии — `mtime` её лога.
pub fn mtime(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
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
        fs::write(
            &file,
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
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
    fn sessions_sort_by_mtime_independently_of_surface_and_id_shape() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let uuid = uuid::Uuid::new_v4().to_string();
        let old = dir.join(format!("{uuid}.jsonl"));
        fs::write(
            &old,
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
        // uuid лексикографически "больше" номера вида 1234-567, но он старее.
        let stamp = dir.join("1790000000-123.jsonl");
        fs::write(
            &stamp,
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        let f = fs::OpenOptions::new().write(true).open(&stamp).unwrap();
        f.set_modified(newer).unwrap();

        let found = sessions(&root, &rt::Store::default()).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "1790000000-123", "свежая — первой");
        assert!(found[0].updated_at.unwrap() > found[1].updated_at.unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_without_dialogue_is_not_listed() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let created = uuid::Uuid::new_v4().to_string();
        let used = uuid::Uuid::new_v4().to_string();
        fs::write(
            dir.join(format!("{created}.jsonl")),
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"system\",\"ts\":2,\"text\":\"prompt\"}\n",
        )
        .unwrap();
        fs::write(
            dir.join(format!("{used}.jsonl")),
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
        )
        .unwrap();
        let found = sessions(&root, &rt::Store::default()).unwrap();
        assert_eq!(found.len(), 1, "пустая сессия в списке не нужна");
        assert_eq!(found[0].id, used);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parent_and_model_are_read_for_subagent_sessions() {
        let root = temp();
        let dir = root.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        fs::write(
            &file,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"1790000000-1\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"задача подагента\",\"source\":\"surface\"}\n",
                "{\"kind\":\"assistant\",\"ts\":3,\"text\":\"ok\",\"tool_calls\":[]}\n",
                "{\"kind\":\"usage\",\"ts\":4,\"model\":\"openai/gpt-6-luna\",\"in_tok\":10,\"cached_tok\":8,\"out_tok\":2,\"cost_micro\":7,\"reason_tok\":0,\"encrypted_reasoning\":false,\"upstream_id\":\"\",\"fingerprint\":\"\",\"finish_reason\":\"stop\",\"est_tok\":10}\n"
            ),
        )
        .unwrap();
        let found = sessions(&root, &rt::Store::default()).unwrap();
        assert_eq!(found[0].parent.as_deref(), Some("1790000000-1"));
        assert_eq!(found[0].model.as_deref(), Some("openai/gpt-6-luna"));
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
