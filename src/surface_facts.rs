// Всё, что список сессий узнаёт о журнале, — одним проходом и с кэшем.
//
// Раньше каждый элемент списка читал один и тот же файл по 3–6 раз: `has_dialogue`
// целиком (ради одной проверки префикса), `title` и `surface`/`parent` — голову,
// `model` — весь файл обратным разбором, `progress` — весь файл ради счётчика
// ходов. На 500 сессиях это 2.4–2.8 с на каждый `GET /v1/sessions`, синхронно
// в async-обработчике (замер на живом инстансе).
//
// Здесь: голова (мета, заголовок, поверхность, родитель), хвост (последний
// `usage` с моделью), и полный проход только для подагентов — им нужен счётчик
// ходов и признак закрытия. Кэш держит результат по (путь, mtime, размер):
// пока файл не менялся, пересчитывать нечего.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Голова журнала: мета и первое сообщение человека лежат в начале.
pub const HEAD_BYTES: u64 = 64 * 1024;
/// Хвост журнала: последний `usage` с моделью лежит в конце.
const TAIL_BYTES: u64 = 64 * 1024;

/// Факты о журнале, нужные списку сессий.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Есть ли в сессии реплика человека или вставка в ход. Без них это огрызок
    /// (создали и бросили, упал запуск) — в списке ему делать нечего.
    pub has_dialogue: bool,
    pub title: Option<String>,
    pub surface: Option<String>,
    pub parent: Option<String>,
    pub model: Option<String>,
    /// Ходы, признак закрытия и границы работы. Заполняется только для
    /// подагентов: пользовательским сессиям счётчик ходов не показывается, а
    /// полный разбор мегабайтного лога на каждый список слишком дорог.
    pub progress: Option<crate::surface_server::discovery::Progress>,
}

/// Ключ кэша: путь, mtime (сек) и размер файла.
type FactsKey = (PathBuf, u64, u64);

fn cache() -> &'static Mutex<HashMap<FactsKey, Facts>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<FactsKey, Facts>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Факты о журнале с кэшем по (путь, mtime, размер). `want_progress` включает
/// дорогой разбор ради счётчика ходов (нужен только карточке подагента).
pub fn facts(path: &Path, want_progress: bool) -> Facts {
    let meta = std::fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let key = (path.to_path_buf(), mtime, size);
    if let Ok(guard) = cache().lock() {
        if let Some(hit) = guard.get(&key) {
            // Прогресс мог не понадобиться в прошлый раз — досчитываем только его.
            if !want_progress || hit.progress.is_some() {
                return hit.clone();
            }
        }
    }
    let mut value = read_facts(path, want_progress);
    if let Ok(mut guard) = cache().lock() {
        // Старые версии того же пути не нужны: под одним путём живёт один файл,
        // и держать его историю по mtime — значит копить копии фактов.
        guard.retain(|(p, _, _), _| p != path);
        if let Some(prev) = guard.get(&key) {
            value.progress = value.progress.or(prev.progress.clone());
        }
        guard.insert(key, value.clone());
    }
    value
}

fn read_facts(path: &Path, want_progress: bool) -> Facts {
    let mut out = Facts::default();
    let head = crate::log::read_head_bytes(path, HEAD_BYTES).unwrap_or_default();
    for rec in &head {
        match rec {
            crate::log::Rec::Meta {
                title,
                surface,
                parent,
                model,
                ..
            } => {
                if let Some(t) = title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                    out.title.get_or_insert_with(|| t.to_string());
                }
                if let Some(s) = surface.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    out.surface.get_or_insert_with(|| s.to_string());
                }
                if let Some(p) = Some(parent.trim()).filter(|p| !p.is_empty()) {
                    out.parent.get_or_insert_with(|| p.to_string());
                }
                if let Some(m) = Some(model.trim()).filter(|m| !m.is_empty()) {
                    out.model.get_or_insert_with(|| m.to_string());
                }
            }
            crate::log::Rec::User { .. } | crate::log::Rec::Steer { .. } => {
                out.has_dialogue = true;
            }
            _ => {}
        }
    }
    // Диалог может быть и дальше головы (длинные системные вставки в начале):
    // префикс ищем по байтам, не разбирая файл целиком.
    if !out.has_dialogue {
        out.has_dialogue = head_has_dialogue(path);
    }
    if out.title.is_none() {
        out.title = crate::surface_server::discovery::title_from_head(&head);
    }
    if out.model.is_none() {
        out.model = tail_model(path);
    }
    if want_progress {
        out.progress = Some(crate::surface_server::discovery::progress_uncached(path));
    }
    out
}

/// Есть ли в файле строка-реплика человека. Читаем кусками по 64 КБ и ищем
/// префикс в байтах: полный разбор JSON тут не нужен, а `read_to_string` на
/// 400 МБ журналов стоил секунду на список.
fn head_has_dialogue(path: &Path) -> bool {
    const PREFIXES: [&[u8]; 2] = [b"{\"kind\":\"user\"", b"{\"kind\":\"steer\""];
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let Ok(n) = file.read(&mut buf) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        let chunk = &buf[..n];
        if PREFIXES.iter().any(|p| {
            chunk
                .windows(p.len())
                .any(|window| window == *p)
        }) {
            return true;
        }
    }
}

/// Последний `usage` с непустой моделью: у старых логов `Meta.model` пуст, а
/// модель видна только в расходе. Читаем хвост, а не весь файл.
fn tail_model(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        if !line.contains("\"model\"") {
            continue;
        }
        // Читаем как JSON-объект, а не как `Rec::Usage`: нужен один полевой
        // `model`, и лишнее/недостающее поле в старой записи не должно мешать.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if value.get("kind").and_then(|k| k.as_str()) != Some("usage") {
                continue;
            }
            if let Some(model) = value.get("model").and_then(|m| m.as_str()) {
                let model = model.trim();
                if !model.is_empty() {
                    return Some(model.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("discovery-facts-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn facts_take_meta_title_parent_and_dialogue_in_one_pass() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"m/one\",\"surface\":\"web\",\"parent\":\"p1\",\"title\":\"Заголовок\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"привет\"}\n"
            ),
        )
        .unwrap();
        let f = facts(&path, false);
        assert_eq!(f.title.as_deref(), Some("Заголовок"));
        assert_eq!(f.surface.as_deref(), Some("web"));
        assert_eq!(f.parent.as_deref(), Some("p1"));
        assert_eq!(f.model.as_deref(), Some("m/one"));
        assert!(f.has_dialogue);
        assert!(f.progress.is_none(), "прогресс не просили — не считаем");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dialogue_and_model_are_found_beyond_head_and_in_tail() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("long.jsonl");
        let filler = "x".repeat(200);
        let mut body = String::new();
        for i in 0..2000 {
            body.push_str(&format!("{{\"kind\":\"assistant\",\"ts\":{i},\"text\":\"{filler}\"}}\n"));
        }
        body.push_str("{\"kind\":\"user\",\"ts\":9999,\"text\":\"далеко\"}\n");
        body.push_str("{\"kind\":\"usage\",\"ts\":10000,\"model\":\"m/late\",\"in_tok\":1,\"out_tok\":1}\n");
        std::fs::write(&path, body).unwrap();
        let f = facts(&path, false);
        assert!(f.has_dialogue, "реплика дальше головы не найдена");
        assert_eq!(f.model.as_deref(), Some("m/late"), "модель из хвоста не найдена");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_is_keyed_by_mtime_and_size() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.jsonl");
        std::fs::write(&path, "{\"kind\":\"user\",\"ts\":1,\"text\":\"a\"}\n").unwrap();
        assert!(facts(&path, false).has_dialogue);
        std::fs::write(&path, "{\"kind\":\"assistant\",\"ts\":1,\"text\":\"b\"}\n").unwrap();
        // Файл изменился — кэш обязан пересчитать, а не отдать прежний ответ.
        assert!(!facts(&path, false).has_dialogue);
        std::fs::remove_dir_all(&dir).ok();
    }
}
