// Диспатч инструментов: вшитый `run` и процессы из реестра. Здесь только то,
// как вызов модели становится процессом; цикл агента об этом не знает.
use std::path::Path;

use serde_json::{json, Value};

use crate::{cfg::Config, exec, log::Call, registry, R};

/// Вшитый инструмент. Всё остальное — процессы из реестра.
pub fn builtin_defs() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "run",
            "description": "Выполнить shell-команду (sh -c) и получить вывод: stdout, затем stderr, и код возврата.\nrun <команда> [--cwd каталог]",
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
    let path = registry::child_path(&bin);
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
    match exec::spawn(sp).await {
        Ok(o) => o,
        Err(e) => fail(format!("{}: {e}", call.name)),
    }
}