// Библиотека компонентов генеративного UI: `GET /v1/genui`.
//
// Клиент инжектит её в каждый артефакт инлайном (origin песочницы `null`, сеть
// по умолчанию запрещена — `<script src>` там не загрузится). Поэтому отдаём
// исходники целиком, а не ссылки.
//
// Библиотека живёт в данных инстанса (`~/.local/share/snens/genui`), а не в
// репозитории харнесса: она растёт из тикетов конкретного инстанса, а не из
// релизов. Чтение дублирует `tools/genui/src/library.rs` нарочно: ядро не
// должно зависеть от крейта тула (тул может быть не собран, а клиент ждёт
// библиотеку всегда), а формат — три каталога и два расширения.

use crate::surface_runtime::Runtime;
use crate::surface_server::http::err;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use std::sync::Arc;

/// Каталог библиотеки: `SNENS_GENUI_DIR`, иначе `<данные>/genui`.
fn dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("SNENS_GENUI_DIR") {
        if !d.trim().is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    let base = std::env::var("SNENS_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".local/share/snens")
        });
    base.join("genui")
}

fn read_file(path: &std::path::Path, name: &str) -> Option<serde_json::Value> {
    // Отсутствие файла — норма (библиотеку могли не наполнять), а ошибка чтения —
    // нет: раньше и то и другое давало `None`, и клиент кэшировал библиотеку
    // с молча пропавшим компонентом (хеш переставал меняться).
    match std::fs::read_to_string(path) {
        Ok(source) => Some(serde_json::json!({"name": name, "source": source})),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            eprintln!("[snens] genui: {} не прочитан: {error}", path.display());
            None
        }
    }
}

/// Содержимое библиотеки: `lib.js`, потом компоненты по алфавиту. Порядок важен:
/// компоненты пользуются утилитами из `lib.js`.
fn files() -> Vec<serde_json::Value> {
    let root = dir();
    let mut out = Vec::new();
    if let Some(f) = read_file(&root.join("lib.js"), "lib.js") {
        out.push(f);
    }
    let comps = root.join("components");
    let mut names: Vec<std::path::PathBuf> = match std::fs::read_dir(&comps) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    for p in names {
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => format!("components/{n}"),
            None => continue,
        };
        if let Some(f) = read_file(&p, &name) {
            out.push(f);
        }
    }
    out
}

/// Отпечаток содержимого: клиент решает по нему, что кэш устарел.
fn hash(files: &[serde_json::Value]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for f in files {
        f.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .hash(&mut h);
        f.get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

pub(super) async fn get(State(_s): State<Arc<Runtime>>) -> impl IntoResponse {
    // Чтение библиотеки — блокирующее (`read_dir` + чтение всех компонентов): не
    // на async-воркере, иначе весь рантайм стоит на время обхода.
    let collected = tokio::task::spawn_blocking(files).await;
    let files = match collected {
        Ok(files) => files,
        Err(e) => {
            return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "hash": hash(&files),
            "dir": dir().display().to_string(),
            "files": files,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_changes_with_content() {
        let a = vec![serde_json::json!({"name":"lib.js","source":"1"})];
        let b = vec![serde_json::json!({"name":"lib.js","source":"2"})];
        assert_ne!(hash(&a), hash(&b));
        assert_eq!(hash(&a), hash(&a.clone()));
    }

    #[test]
    fn missing_dir_gives_empty_list() {
        // Каталога нет — пустой список, а не ошибка: клиент рисует артефакты
        // без компонентов.
        //
        // Переменная общая для процесса: ставим под мьютексом (чтобы не
        // пересечься с другими тестами) и возвращаем прежнее значение на любом
        // выходе — `Drop` отработает и при панике в `assert`.
        struct EnvGuard {
            prev: Option<String>,
            _lock: std::sync::MutexGuard<'static, ()>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(prev) => std::env::set_var("SNENS_GENUI_DIR", prev),
                    None => std::env::remove_var("SNENS_GENUI_DIR"),
                }
            }
        }
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _guard = EnvGuard {
            prev: std::env::var("SNENS_GENUI_DIR").ok(),
            _lock: lock,
        };
        std::env::set_var("SNENS_GENUI_DIR", "/nonexistent-genui-dir");
        assert!(files().is_empty());
    }
}
