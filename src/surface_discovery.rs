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
            // Прогресс нужен только карточкам подагентов: у пользовательских
            // сессий ходы не показываются, а полный разбор их логов (бывают по
            // несколько мегабайт) удорожает каждый список.
            if session.parent.is_some() {
                let (turns, done) = progress(&path);
                session.turns = Some(turns);
                session.done = done;
            }
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
        // Ходы и закрытие пересчитываем всегда, а не только когда пусто:
        // подагент растёт, пока его карточка висит на экране, и «3 хода» без
        // обновления превращалось бы во враньё.
        if let (Some(path), Some(_)) = (&file, &session.parent) {
            let (turns, done) = progress(path);
            session.turns = Some(turns);
            session.done = done;
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

/// Кэш `progress`: список сессий дёргается на каждое событие SSE и на каждую
/// смену сессии, а полный разбор сотни логов на каждый запрос — это секунды.
/// Ключ — путь + mtime + размер: пока файл не менялся, пересчитывать нечего.
static PROGRESS: std::sync::Mutex<Option<std::collections::HashMap<(std::path::PathBuf, u64, u64), (u64, Option<String>)>>> =
    std::sync::Mutex::new(None);

/// Ходы и признак закрытия из журнала — для карточки подагента: сколько шагов
/// прошло и чем кончилось. Читаем лог целиком: `Done` пишется только в конце.
pub fn progress(path: &Path) -> (u64, Option<String>) {
    let meta = fs::metadata(path).ok();
    let key_extra = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = (path.to_path_buf(), key_extra, meta.map(|m| m.len()).unwrap_or(0));
    if let Ok(guard) = PROGRESS.lock() {
        if let Some(cache) = guard.as_ref() {
            if let Some(hit) = cache.get(&key) {
                return hit.clone();
            }
        }
    }
    let value = progress_uncached(path);
    if let Ok(mut guard) = PROGRESS.lock() {
        let cache = guard.get_or_insert_with(Default::default);
        // Размер кэша ограничен числом сессий с их mtime: старые ключи живут,
        // пока жив процесс. Это микроскопические значения, но подчистим, чтобы
        // не копить их при активной работе: одна запись на путь.
        cache.retain(|(p, _, _), _| p != path);
        cache.insert(key, value.clone());
    }
    value
}

fn progress_uncached(path: &Path) -> (u64, Option<String>) {
    let Ok(records) = crate::log::read_file(path) else {
        return (0, None);
    };
    let mut turns = 0u64;
    let mut done = None;
    for rec in &records {
        match rec {
            crate::log::Rec::Assistant { .. } => turns += 1,
            crate::log::Rec::Done { status, .. } => done = Some(status.clone()),
            _ => {}
        }
    }
    // Перезапущенная сессия: `Done` есть, но после него снова работали —
    // последняя `Assistant` новые ходы добавляет, и закрытие уже не текущее.
    // Порядок в журнале хронологический, поэтому просто смотрим, что было
    // последним из двух событий.
    let last_done = records
        .iter()
        .rposition(|r| matches!(r, crate::log::Rec::Done { .. }));
    let last_assistant = records
        .iter()
        .rposition(|r| matches!(r, crate::log::Rec::Assistant { .. }));
    if let (Some(d), Some(a)) = (last_done, last_assistant) {
        if a > d {
            done = None;
        }
    }
    (turns, done)
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

    #[test]
    fn progress_counts_turns_and_sees_done() {
        let dir = std::env::temp_dir().join(format!("snens-progress-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.jsonl");
        let records = [
            r#"{"kind":"meta","ts":1,"model":"m","parent":"p"}"#,
            r#"{"kind":"user","ts":2,"text":"q","source":"surface"}"#,
            r#"{"kind":"assistant","ts":3,"text":"a","tool_calls":[]}"#,
            r#"{"kind":"assistant","ts":4,"text":"b","tool_calls":[]}"#,
        ];
        fs::write(&path, records.join("\n")).unwrap();
        // Открытая сессия: ходы считаются, закрытия нет.
        assert_eq!(progress(&path), (2, None));
        // После `Done` сессия закрыта — это и есть «подагент закончил».
        let mut text = records.join("\n");
        text.push_str("\n{\"kind\":\"done\",\"ts\":5,\"status\":\"ok\",\"turns\":2}");
        fs::write(&path, &text).unwrap();
        assert_eq!(progress(&path), (2, Some("ok".into())));
        // Новая работа после `Done` (перезапуск) снимает закрытие.
        text.push_str("\n{\"kind\":\"assistant\",\"ts\":6,\"text\":\"c\",\"tool_calls\":[]}");
        fs::write(&path, &text).unwrap();
        assert_eq!(progress(&path), (3, None));
        fs::remove_dir_all(dir).unwrap();
    }
}

/// Одна сессия по id, собранная из журнала: нужно, когда сессия появилась
/// позже старта поверхности (подагент пишет свой лог другим процессом), а
/// `discovery::sessions` наполняет store только целиком и только при старте.
pub fn session_by_id(data_dir: &Path, id: &str) -> Option<Session> {
    let path = path_from(data_dir, id).filter(|p| has_dialogue(p))?;
    let mut session = Session::new(id.to_string(), surface(&path));
    session.title = title(&path);
    session.updated_at = mtime(&path);
    session.parent = parent(&path);
    session.model = model(&path);
    let (turns, done) = progress(&path);
    session.turns = Some(turns);
    session.done = done;
    Some(session)
}
