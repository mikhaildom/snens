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
            "SNENS_CONFIG".to_string(),
            crate::cfg::config_path().display().to_string(),
        ),
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
    if std::env::var_os("SNENS_ASKPASS_DISABLED").is_some() {
        // Keep both the policy marker and explicit config in each child; askpass client
        // rejects before socket connect, even when another TUI owns the default path.
        v.push(("SNENS_ASKPASS_DISABLED".into(), "1".into()));
    }
    if let Ok(shim) = askpass::ensure_shims(cfg) {
        v.extend(askpass::child_env(
            &askpass::sock_path(&cfg.sessions_path()),
            &shim,
        ));
    }
    v.push((
        "SNENS_CONTEXT_PATH".into(),
        sess.path()
            .with_extension("out")
            .join("LIVE_CONTEXT.json")
            .display()
            .to_string(),
    ));
    v
}

/// Вшитый инструмент. Всё остальное — процессы из реестра.
pub fn builtin_defs() -> Vec<Value> {
    vec![
        json!({
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
        }),
        json!({
            "type": "function",
            "function": {
                "name": "ctx",
                "description": "Управление рабочим контекстом. commit — применить правку LIVE_CONTEXT.json (после всех результатов шага; статус придёт перед следующим запросом). status — путь и бюджет. clear — хирургически вырезать результаты инструментов, не трогая диалог: либо call_ids (точечно), либо before (всё строго до указанного), либо keep_last (все старше последних N), с масками tool и min_bytes.",
                "parameters": {"type":"object", "properties":{
                    "action":{"type":"string","enum":["commit","status","clear"]},
                    "call_ids":{"type":"array","items":{"type":"string"},"description":"clear: какие результаты вырезать"},
                    "before":{"type":"string","description":"clear: вырезать всё строго до этого call_id"},
                    "keep_last":{"type":"integer","description":"clear: оставить N последних результатов"},
                    "tool":{"type":"string","description":"clear: только результаты этого инструмента"},
                    "min_bytes":{"type":"integer","description":"clear: только результаты не меньше N байт"}
                }}
            }
        }),
    ]
}

/// Определения тулов для модели + сами тулы + PATH дочерних процессов.
pub async fn prepare(cfg: &Config) -> R<(Vec<Value>, Vec<registry::Tool>, String)> {
    let tools = registry::discover(cfg).await;
    let bin = registry::sync_bin_dir(cfg, &tools)?;
    // Keep the shim even for headless API sessions: it routes sudo through askpass,
    // whose explicit disabled guard fails before touching a possibly stale TUI socket.
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
    if call.name == "ctx" {
        let action = a.get("action").and_then(Value::as_str).unwrap_or("commit");
        if action == "clear" {
            return ctx_clear(sess, &a);
        }
        let text = match action {
            "commit" => {
                "Draft отправлен. Следующее системное сообщение [Harness context status] содержит окончательный результат; не вызывай commit повторно ради подтверждения."
                    .to_string()
            }
            "status" => format!(
                "Зеркало: {}\nБюджет: {} токенов; цель уборки: {}\nПоследняя принятая ревизия: {}",
                out_dir.join("LIVE_CONTEXT.json").display(),
                cfg.context_budget(),
                cfg.context_target(),
                sess.read()
                    .ok()
                    .and_then(|rs| rs.into_iter().rev().find_map(|r| match r {
                        crate::log::Rec::ContextRevision { anchor, .. } => Some(anchor.to_string()),
                        _ => None,
                    }))
                    .unwrap_or_else(|| "нет".into())
            ),
            _ => return fail(format!("ctx: неизвестное действие {action}")),
        };
        return exec::Out {
            bytes: text.len(),
            text,
            exit: 0,
            ms: 0,
            timed_out: false,
            parts: Vec::new(),
        };
    }
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
/// Хирургическая уборка: вырезать выбранные результаты тулов, не трогая
/// диалог. Отбор — по `call_id` (голые), `before` (всё до указанного),
/// `keep_last` (все старше N последних), `tool` и `min_bytes` (маски).
///
/// Каждое вырезание пишется записью `Rec::Ctx` с картой заглушек: проекция
/// накладывает её поверх, исходный результат остаётся в логе, resume сохраняет
/// уборку. Диалог (user/assistant) не трогается вовсе — жёсткое правило.
fn ctx_clear(sess: &crate::log::Session, a: &Value) -> exec::Out {
    use std::collections::BTreeMap;
    let fail = |msg: String| exec::Out {
        exit: -1,
        text: msg,
        bytes: 0,
        ms: 0,
        timed_out: false,
        parts: Vec::new(),
    };
    let log = match sess.read() {
        Ok(log) => log,
        Err(e) => return fail(format!("ctx clear: не читается лог: {e}")),
    };
    let already = crate::project::cleared_map(&log);
    let results: Vec<&crate::log::Rec> = log
        .iter()
        .filter(|r| matches!(r, crate::log::Rec::ToolResult { .. }))
        .collect();

    let ids: Vec<&str> = a
        .get("call_ids")
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let before = a.get("before").and_then(Value::as_str);
    let keep_last = a.get("keep_last").and_then(Value::as_u64);
    let tool = a.get("tool").and_then(Value::as_str);
    let min_bytes = a.get("min_bytes").and_then(Value::as_u64);

    let mut cleared: BTreeMap<String, String> = BTreeMap::new();
    let mut freed = 0usize;
    let keep_from = keep_last
        .map(|n| results.len().saturating_sub(n as usize))
        .unwrap_or(0);
    let mut seen_before = false;
    for (idx, rec) in results.iter().enumerate() {
        let crate::log::Rec::ToolResult {
            call_id,
            tool: name,
            argv,
            bytes,
            exit,
            ..
        } = rec
        else {
            continue;
        };
        // Позицию "--before" отмечаем до проверки на уже вырезанное: точка
        // отсечения — это сам call_id, независимо от того, чистили его раньше.
        // "--before c" вырезает строго до c: сам c остаётся.
        let is_before_target = before.is_some_and(|b| b == call_id);
        let selected = if before.is_some() {
            !seen_before
        } else if !ids.is_empty() {
            ids.contains(&call_id.as_str())
        } else {
            idx < keep_from
        };
        if is_before_target {
            seen_before = true;
        }
        let selected = selected && !is_before_target;
        if already.contains_key(call_id) || !selected {
            continue;
        }
        if tool.is_some_and(|t| t != name) {
            continue;
        }
        if min_bytes.is_some_and(|min| (*bytes as u64) < min) {
            continue;
        }
        let placeholder = format!(
            "[результат вырезан уборкой контекста: {name}, {bytes} Б, exit {exit} — {}]",
            crate::project::command_hint(argv)
        );
        freed += bytes;
        cleared.insert(call_id.clone(), placeholder);
    }
    if cleared.is_empty() {
        return exec::Out {
            bytes: 0,
            text: "ctx clear: нечего вырезать (нет подходящих результатов)".into(),
            exit: 0,
            ms: 0,
            timed_out: false,
            parts: Vec::new(),
        };
    }
    let elided = cleared.len();
    if let Err(e) = sess.append(crate::log::Rec::Ctx {
        ts: crate::log::now(),
        action: "clear".into(),
        elided,
        freed_bytes: freed,
        cleared: cleared.clone(),
    }) {
        return fail(format!("ctx clear: не записалась уборка: {e}"));
    }
    let text = format!(
        "ctx clear: вырезано результатов {elided}, освобождено {} Б. Диалог не тронут.",
        freed
    );
    exec::Out {
        bytes: text.len(),
        text,
        exit: 0,
        ms: 0,
        timed_out: false,
        parts: Vec::new(),
    }
}#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Body, Call, Rec, Session};

    fn fixture() -> (std::path::PathBuf, Session) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "snens-dispatch-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let sess = Session::open(&dir, "test").unwrap();
        sess.append(Rec::System {
            ts: 0,
            text: "sys".into(),
        })
        .unwrap();
        for id in ["a", "b", "c"] {
            sess.append(Rec::Assistant {
                ts: 0,
                text: None,
                tool_calls: vec![Call {
                    id: id.into(),
                    name: "run".into(),
                    args: "{}".into(),
                }],
                reasoning: None,
            })
            .unwrap();
            sess.append(Rec::ToolResult {
                ts: 0,
                call_id: id.into(),
                tool: "run".into(),
                argv: vec![json!({"command": format!("echo {id}")}).to_string()],
                exit: 0,
                bytes: 100,
                ms: 1,
                body: Body::Inline {
                    text: format!("output-{id}"),
                },
                parts: vec![],
            })
            .unwrap();
        }
        (dir, sess)
    }

    fn kept(sess: &Session) -> usize {
        let log = sess.read().unwrap();
        crate::project::msgs(&log, &Config::default())
            .iter()
            .filter(|m| m["role"] == "tool")
            .count()
    }

    #[test]
    fn clear_keep_last_keeps_dialog_and_recent_results() {
        let (dir, sess) = fixture();
        let out = ctx_clear(&sess, &json!({"keep_last": 1}));
        assert_eq!(out.exit, 0, "{}", out.text);
        let log = sess.read().unwrap();
        let cleared = crate::project::cleared_map(&log);
        assert_eq!(cleared.len(), 2, "должны уйти a и b");
        let wire = serde_json::to_string(&crate::project::msgs(
            &log,
            &Config::default(),
        ))
        .unwrap();
        assert!(!wire.contains("output-a"), "старый результат остался");
        assert!(wire.contains("output-c"), "свежий результат вырезан");
        // Диалог и вызовы не тронуты.
        assert!(wire.contains("\"tool_calls\""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clear_by_call_id_and_filters() {
        let (dir, sess) = fixture();
        assert_eq!(ctx_clear(&sess, &json!({"call_ids": ["b"]})).exit, 0);
        let cleared = crate::project::cleared_map(&sess.read().unwrap());
        assert_eq!(cleared.keys().collect::<Vec<_>>(), vec!["b"]);
        // Повторный вызов по тому же id не находит ничего.
        let again = ctx_clear(&sess, &json!({"call_ids": ["b"]}));
        assert_eq!(again.exit, 0);
        assert!(again.text.contains("нечего"));
        // --before вырезает строго до указанного, диалог остаётся.
        assert_eq!(ctx_clear(&sess, &json!({"before": "c"})).exit, 0);
        let cleared = crate::project::cleared_map(&sess.read().unwrap());
        assert_eq!(cleared.len(), 2);
        assert!(cleared.contains_key("a"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clear_min_bytes_skips_small_results() {
        let (dir, sess) = fixture();
        assert_eq!(ctx_clear(&sess, &json!({"min_bytes": 200})).exit, 0);
        let cleared = crate::project::cleared_map(&sess.read().unwrap());
        assert!(cleared.is_empty(), "маленькие результаты не должны вырезаться");
        let _ = std::fs::remove_dir_all(dir);
        let _ = kept(&sess);
    }
}