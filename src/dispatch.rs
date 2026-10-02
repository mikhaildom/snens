// Диспатч инструментов: вшитый `run` и процессы из реестра. Здесь только то,
// как вызов модели становится процессом; цикл агента об этом не знает.
use std::path::Path;

use serde_json::{json, Value};

use crate::{askpass, cfg::Config, exec, log::Call, log::Session, registry, R};

/// Что тул знает о сессии, в которой он вызван. Через env — как канал вложений:
/// переменной нет — тул просто не знает контекста, ничего не ломается.
fn session_env(sess: &Session, cfg: &Config) -> Vec<(String, String)> {
    let mut v = vec![
        ("SNENS_SESSION".to_string(), sess.id().to_string()),
        (
            "SNENS_MAIL_DIR".to_string(),
            cfg.mail_path().display().to_string(),
        ),
    ];
    if !sess.parent().is_empty() {
        v.push(("SNENS_PARENT".to_string(), sess.parent().to_string()));
    }
    // Канал пароля: процесс-инструмент (в т.ч. шим `sudo`) узнаёт, где спросить
    // пароль у TUI. Каталог шимов создаётся при подготовке окружения; здесь
    // best-effort — без него sudo просто честно упадёт «password required».
    if let Ok(shim) = askpass::ensure_shims(cfg) {
        v.extend(askpass::child_env(
            &askpass::sock_path(&cfg.sessions_path()),
            &shim,
        ));
    }
    v
}

/// Вшитый инструмент. Всё остальное — процессы из реестра.
pub fn builtin_defs() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "run",
            "description": "Выполнить shell-команду (sh -c) и получить вывод: stdout, затем stderr, и код возврата.\nrun <команда> [--cwd каталог]\nОболочка — sh (dash), не bash: нет ${x:0:n}, [[ ]], массивов, $'\\t'; нужен bash — зови `bash -c '...'`. Код возврата при `;`-цепочке — от последней команды, поэтому хвостовой `which`/`grep` может покрасить успешную команду в ошибку (для bash-измов это частая причина).",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Команда для sh -c"},
                    "cwd": {"type": "string", "description": "Рабочий каталог"},
                    "timeout_seconds": {"type": "integer", "description": "Таймаут в секундах"}
                },
                "required": ["command"]
            }
        }
    })]
}

/// Определения тулов для модели + сами тулы + PATH дочерних процессов.
pub async fn prepare(cfg: &Config) -> R<(Vec<Value>, Vec<registry::Tool>, String)> {
    let tools = registry::discover(cfg).await;
    let bin = registry::sync_bin_dir(cfg, &tools)?;
    // Каталог шимов (симлинк `sudo` → `sudo -A`) идёт ПЕРЕД системным PATH: только
    // внутри окружения инструментов агента, обычный sudo пользователя не тронут.
    let shim = askpass::ensure_shims(cfg).ok();
    let mut parts = Vec::new();
    if let Some(s) = &shim {
        parts.push(s.clone());
    }
    let path = registry::child_path_with(&parts, &bin);
    let mut defs = builtin_defs();
    defs.extend(registry::definitions(&tools));
    Ok((defs, tools, path))
}

/// Ошибки инструмента не прерывают ход: они возвращаются как результат,
/// чтобы модель могла исправиться.
pub async fn dispatch(
    call: &Call,
    cfg: &Config,
    tools: &[registry::Tool],
    path: &str,
    out_dir: &Path,
    sess: &Session,
) -> exec::Out {
    let fail = |msg: String| exec::Out {
        exit: -1,
        text: msg,
        bytes: 0,
        ms: 0,
        timed_out: false,
        parts: Vec::new(),
    };
    let a: Value = serde_json::from_str(&call.args).unwrap_or_else(|_| json!({}));
    // Канал вложений — свой каталог на вызов: файлы разных вызовов не смешиваются,
    // а имя каталога (= id вызова) делает сбор детерминированным.
    let attach_dir = cfg
        .attachments
        .then(|| out_dir.join("attach").join(&call.id));

    if call.name == "run" {
        let Some(command) = a.get("command").and_then(|c| c.as_str()) else {
            return fail("run: не передан command".into());
        };
        if command.trim().is_empty() {
            return fail("run: пустая команда".into());
        }
        let t = a
            .get("timeout_seconds")
            .and_then(|t| t.as_u64())
            .unwrap_or(cfg.timeout_seconds);
        let mut sp = exec::Spawn::sh(&cfg.shell, command, t, cfg.max_process_chars);
        sp.cwd = a.get("cwd").and_then(|c| c.as_str());
        sp.path = Some(path);
        sp.reduce = cfg.reduce_output;
        sp.dump_dir = Some(out_dir);
        sp.attach_dir = attach_dir.as_deref();
        sp.max_attachment_bytes = cfg.max_attachment_bytes;
        sp.envs = session_env(sess, cfg);
        return match exec::spawn(sp).await {
            Ok(o) => o,
            Err(e) => fail(format!("run: {e}")),
        };
    }

    let Some(tool) = tools.iter().find(|t| t.name == call.name) else {
        return fail(format!(
            "неизвестный инструмент: {} — его нет в реестре. Список: snens tools",
            call.name
        ));
    };
    let args: Vec<String> = match a.get("args") {
        Some(Value::Array(v)) => v
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Some(Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        None => Vec::new(),
        Some(other) => {
            return fail(format!(
                "args: ожидался массив или строка, получено {other}"
            ))
        }
    };
    let bin = tool.bin.to_string_lossy().into_owned();
    let mut sp = exec::Spawn::argv(&bin, args);
    sp.stdin = a.get("stdin").and_then(|s| s.as_str()).map(str::to_string);
    sp.cwd = a.get("cwd").and_then(|c| c.as_str());
    sp.cap = cfg.max_process_chars;
    sp.timeout_s = a
        .get("timeout_seconds")
        .and_then(|t| t.as_u64())
        .unwrap_or(cfg.timeout_seconds);
    sp.path = Some(path);
    // Тулы отдают точный контракт (файл, JSON-событие) — повторы в них схлопывать
    // нельзя, это исказило бы содержимое. Сокращение живёт только на shell-выводе.
    sp.dump_dir = Some(out_dir);
    sp.attach_dir = attach_dir.as_deref();
    sp.max_attachment_bytes = cfg.max_attachment_bytes;
    sp.envs = session_env(sess, cfg);
    match exec::spawn(sp).await {
        Ok(o) => o,
        Err(e) => fail(format!("{}: {e}", call.name)),
    }
}
