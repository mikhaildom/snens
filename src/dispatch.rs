// Диспатч инструментов: вшитый `run` и процессы из реестра. Здесь только то,
// как вызов модели становится процессом; цикл агента об этом не знает.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{askpass, cfg::Config, exec, log::Call, log::Session, registry, sesscwd, R};

/// Поля вызова, которые ядро понимает у любого тула (не из schema.json).
const CORE_FIELDS: [&str; 2] = ["cwd", "timeout_seconds"];

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
        (
            "SNENS_SESSIONS_DIR".to_string(),
            cfg.sessions_path().display().to_string(),
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
    // Канал одобрений привилегированных действий. Сокет поднимает сервер
    // поверхности; в живой TUI-сессии его нет, и шим честно уходит в askpass.
    // Нулевой таймаут выключает очередь: переменную не ставим.
    if cfg.approval_timeout_seconds > 0 {
        v.push((
            "SNENS_APPROVAL_SOCK".into(),
            crate::approval::sock_path(&cfg.sessions_path())
                .display()
                .to_string(),
        ));
    }
    // Root-owned broker привилегированных действий: узкий whitelist команд.
    // Одобренный кнопкой шим исполняет команду только через него. Нет файла —
    // переменную не ставим: шим честно откажет (`sudo -n` без правила).
    if let Some(broker) = crate::approval::privileged_broker() {
        v.push(("SNENS_PRIV".into(), broker));
    }
    // Рабочий каталог сессии: тулы, которые сами разрешают пути, должны
    // видеть ту же точку отсчёта, что и ядро. `run`/typed дополняют его явным cwd.
    v.push((
        "SNENS_CWD".into(),
        sesscwd::read(sess).display().to_string(),
    ));
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
                "Зеркало: {}\nБюджет: {} токенов; цель уборки: {}\nТул ctx доступен: да (ты его вызвал — значит он в схеме; commit применит правку)\nПоследняя принятая ревизия: {}",
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
        // Рабочий каталог сессии — точка отсчёта для относительных путей: `cd`
        // внутри run раньше не доезжало до read/edit/write, и они не находили
        // файл. Явный cwd вызова сильнее и разрешается от каталога сессии.
        let session_cwd = sesscwd::read(sess);
        let explicit = a.get("cwd").and_then(|c| c.as_str());
        let cwd_owned = explicit
            .map(|c| sesscwd::resolve(Path::new(c), &session_cwd))
            .unwrap_or_else(|| session_cwd.clone());
        let mut sp = exec::Spawn::sh(&cfg.shell, command, t, cfg.max_process_chars);
        sp.cwd = cwd_owned.to_str();
        sp.path = Some(path);
        sp.reduce = cfg.reduce_output;
        sp.dump_dir = Some(out_dir);
        sp.attach_dir = attach_dir.as_deref();
        sp.max_attachment_bytes = cfg.max_attachment_bytes;
        sp.envs = session_env(sess, cfg);
        sp.envs
            .push(("SNENS_CWD".into(), cwd_owned.display().to_string()));
        return match exec::spawn(sp).await {
            Ok(mut o) => {
                // Каталог записываем только после успешного запуска: битый вызов
                // не должен незаметно сдвинуть точку отсчёта следующих тулов.
                sesscwd::after_run(sess, command, explicit);
                o.text = explain_signal_death(o.exit, o.bytes, &o.text, command);
                o
            }
            Err(e) => fail(format!("run: {e}")),
        };
    }

    let Some(tool) = tools.iter().find(|t| t.name == call.name) else {
        return fail(format!(
            "неизвестный инструмент: {} — его нет в реестре. Список: snens tools",
            call.name
        ));
    };
    // Типизированный тул: поля схемы → argv/stdin. Generic `args`/`stdin` для
    // него не нужны; если модель всё же их прислала — пробуем спасти вызов.
    if let Some(schema) = &tool.schema {
        return dispatch_typed(call, &tool.bin, schema, &a, cfg, path, out_dir, sess).await;
    }

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
    sp.stdin = stdin_of(&a);
    // cwd сессии — как у `run`: тул видит тот же каталог, откуда шли cd.
    let session_cwd = sesscwd::read(sess);
    let cwd_owned = a
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(|c| sesscwd::resolve(Path::new(c), &session_cwd))
        .unwrap_or(session_cwd);
    sp.cwd = cwd_owned.to_str();
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
/// Готовый к запуску вызов типизированного тула: argv, stdin, cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedCall {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    pub cwd: PathBuf,
    pub timeout_seconds: Option<u64>,
}

/// Разбор вызова по схеме — без процессов и без диска. Ошибки до запуска, с
/// именем поля и перечнем допустимых: модель должна суметь исправить вызов.
pub fn typed_call(
    name: &str,
    schema: &registry::ToolSchema,
    a: &Value,
    session_cwd: &Path,
) -> Result<TypedCall, String> {
    let allowed = schema.field_names();
    let (fields, extra) = normalize_fields(a, schema);
    let missing: Vec<String> = schema
        .required()
        .into_iter()
        .filter(|r| !fields.contains_key(r))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{}: не хватает обязательных полей: {}. Поля тула: {}. Пример: {}",
            name,
            missing.join(", "),
            allowed.join(", "),
            example_call(schema),
        ));
    }
    // Неизвестное поле — ошибка со списком допустимых: молча выкинуть значит
    // соврать модели, и она повторит промах.
    let unknown: Vec<&str> = fields
        .keys()
        .filter(|k| !allowed.contains(k) && !CORE_FIELDS.contains(&k.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{}: неизвестные поля: {}. Допустимые: {}.",
            name,
            unknown.join(", "),
            allowed.join(", "),
        ));
    }
    let mut argv = argv_of(schema, &fields)?;
    // Хвост generic-`args`, которому не нашлось поля: это готовые argv-элементы
    // (модель прислала `["/tmp/x", "--offset", "3"]`) — дописываем как есть.
    argv.extend(extra);
    Ok(TypedCall {
        argv,
        stdin: stdin_of_schema(schema, &fields)?,
        cwd: fields
            .get("cwd")
            .and_then(Value::as_str)
            .map(|c| sesscwd::resolve(Path::new(c), session_cwd))
            .unwrap_or_else(|| session_cwd.to_path_buf()),
        timeout_seconds: fields.get("timeout_seconds").and_then(Value::as_u64),
    })
}

/// Типизированный тул: собрать argv и stdin из полей по `x-snens` схемы.
/// Ошибки — до запуска процесса, с именем поля и списком допустимых: модель
/// должна суметь исправить вызов, не гадая о контракте.
async fn dispatch_typed(
    call: &Call,
    bin: &Path,
    schema: &registry::ToolSchema,
    a: &Value,
    cfg: &Config,
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
    // cwd сессии — то, что накопили `cd` в run: относительные пути тулов
    // должны разрешаться оттуда же, откуда их разрешал бы shell.
    let session_cwd = sesscwd::read(sess);
    let tc = match typed_call(&call.name, schema, a, &session_cwd) {
        Ok(tc) => tc,
        Err(e) => return fail(e),
    };
    let bin_s = bin.to_string_lossy().into_owned();
    let mut sp = exec::Spawn::argv(&bin_s, tc.argv);
    sp.stdin = tc.stdin;
    sp.cwd = tc.cwd.to_str();
    sp.cap = cfg.max_process_chars;
    sp.timeout_s = tc.timeout_seconds.unwrap_or(cfg.timeout_seconds);
    sp.path = Some(path);
    sp.dump_dir = Some(out_dir);
    sp.max_attachment_bytes = cfg.max_attachment_bytes;
    sp.envs = session_env(sess, cfg);
    sp.envs
        .push(("SNENS_CWD".into(), tc.cwd.display().to_string()));
    // Канал вложений — как у generic-тулов.
    let attach_dir = cfg
        .attachments
        .then(|| out_dir.join("attach").join(&call.id));
    sp.attach_dir = attach_dir.as_deref();
    match exec::spawn(sp).await {
        Ok(o) => o,
        Err(e) => fail(format!("{}: {e}", call.name)),
    }
}

/// Поля вызова по схеме: всё, кроме мусора. Объект/массив, приехавшие
/// JSON-строкой, распаковываются (модель регулярно сериализует вложенное
/// дважды). Одиночный объект в `edits` принимается как массив из одного —
/// это наблюдавшийся промах, а не другая правка.
fn normalize_fields(
    a: &Value,
    schema: &registry::ToolSchema,
) -> (serde_json::Map<String, Value>, Vec<String>) {
    let mut out = serde_json::Map::new();
    let Some(obj) = a.as_object() else {
        return (out, Vec::new());
    };
    let types: std::collections::BTreeMap<String, String> = schema.fields().into_iter().collect();
    for (k, v) in obj {
        // generic-поля разбираются отдельно: ими модель зовёт тул «по-старому».
        if k == "args" || k == "stdin" {
            continue;
        }
        let ty = types.get(k).cloned().unwrap_or_default();
        let mut v = v.clone();
        // Двойное экранирование: модель прислала JSON-строку там, где схема ждёт
        // object/array. Она это имела в виду — разбираем.
        if (ty.starts_with("object") || ty.starts_with("array")) && v.is_string() {
            if let Ok(parsed) = serde_json::from_str::<Value>(v.as_str().unwrap_or_default()) {
                if parsed.is_object() || parsed.is_array() {
                    v = parsed;
                }
            }
        }
        // Одиночный объект вместо массива (наблюдалось на `edits`): одна правка
        // без обёртки — это массив из одного элемента.
        if ty.starts_with("array") && v.is_object() {
            v = Value::Array(vec![v]);
        }
        // Поля вне схемы оставляем: ошибку про них скажем отдельно, с перечнем
        // допустимых — молча выкинуть значит соврать модели, и она повторит промах.
        out.insert(k.clone(), v);
    }
    legacy_fields(a, schema, &mut out);
    let extra = legacy_args(a, schema, &mut out);
    (out, extra)
}

/// `args` от модели, привыкшей к generic-схеме: первые элементы раскладываем по
/// позиционным полям (`{"field": X}` в порядке схемы), остальное — готовые
/// argv-элементы, их допишем как есть. Флаги угадывать не берёмся.
fn legacy_args(a: &Value, schema: &registry::ToolSchema, out: &mut serde_json::Map<String, Value>) -> Vec<String> {
    let Some(v) = a.get("args") else {
        return Vec::new();
    };
    let items: Vec<String> = match v {
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
        _ => return Vec::new(),
    };
    if items.is_empty() {
        return Vec::new();
    }
    let positional: Vec<String> = schema
        .argv
        .iter()
        .filter_map(|r| match r {
            registry::ArgRule::Field(f) => Some(f.clone()),
            _ => None,
        })
        .collect();
    let mut rest = items;
    for f in positional {
        if rest.is_empty() || out.contains_key(&f) {
            continue;
        }
        let first = rest.remove(0);
        out.insert(f, Value::String(first));
    }
    rest
}

/// Совместимость с generic-контрактом: модель шлёт `{"args": ..., "stdin": ...}`
/// тулу, у которого теперь есть схема. `stdin` разбираем как JSON-объект и
/// раскладываем по полям схемы (не перетирая уже переданные). Спасти не вышло —
/// дальше сработает ошибка про обязательные поля, процесс не запустится.
fn legacy_fields(a: &Value, schema: &registry::ToolSchema, out: &mut serde_json::Map<String, Value>) {
    let Some(stdin) = a.get("stdin") else {
        return;
    };
    let parsed = match stdin {
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        v @ (Value::Object(_) | Value::Array(_)) => Some(v.clone()),
        _ => None,
    };
    let allowed = schema.field_names();
    let merge = |o: serde_json::Map<String, Value>, out: &mut serde_json::Map<String, Value>| {
        for (k, v) in o {
            if allowed.contains(&k) && !out.contains_key(&k) {
                out.insert(k, v);
            }
        }
    };
    match parsed {
        Some(Value::Object(o)) => merge(o, out),
        // `stdin` — массив правок: это ровно форма `edits` из схемы edit.
        Some(Value::Array(items)) if allowed.contains(&"edits".to_string()) => {
            out.entry("edits".to_string())
                .or_insert(Value::Array(items));
        }
        _ => {}
    }
}

/// Значение поля в текст argv: числа без хвоста `.0`, bool как 1/true не
/// выдумываем — берём serde-представление.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Собрать argv по правилам `x-snens.argv`. Поле не задано (нет в вызове или
/// null) — ничего не эмитим; bool-поле с `field` и истиной — `1`, с ложью —
/// ничего (флаг присутствует только когда включён).
fn argv_of(
    schema: &registry::ToolSchema,
    fields: &serde_json::Map<String, Value>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for rule in &schema.argv {
        match rule {
            registry::ArgRule::Const(c) => out.push(c.clone()),
            registry::ArgRule::Field(f) => {
                if let Some(v) = fields.get(f) {
                    if let Some(t) = scalar_text(v) {
                        out.push(t);
                    }
                }
            }
            registry::ArgRule::Flag {
                flag,
                field,
                when_field,
            } => {
                if let Some(b) = when_field {
                    if !fields.get(b).and_then(Value::as_bool).unwrap_or(false) {
                        continue;
                    }
                }
                match field {
                    None => out.push(flag.clone()),
                    Some(f) => {
                        let Some(v) = fields.get(f) else { continue };
                        let Some(t) = scalar_text(v) else { continue };
                        out.push(flag.clone());
                        out.push(t);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// stdin по правилу схемы. `json` — объект из перечисленных полей: ровно та
/// JSON-строка, которую тул ждал от модели, только собранная ядром.
fn stdin_of_schema(
    schema: &registry::ToolSchema,
    fields: &serde_json::Map<String, Value>,
) -> Result<Option<String>, String> {
    Ok(match &schema.stdin {
        registry::StdinRule::None => None,
        registry::StdinRule::Field(f) => fields.get(f).and_then(scalar_text),
        registry::StdinRule::Json(names) => {
            let mut o = serde_json::Map::new();
            for n in names {
                if let Some(v) = fields.get(n) {
                    if !v.is_null() {
                        o.insert(n.clone(), v.clone());
                    }
                }
            }
            if o.is_empty() {
                None
            } else {
                Some(Value::Object(o).to_string())
            }
        }
    })
}

/// Образец вызова для сообщения об ошибке: обязательные поля с заглушками.
fn example_call(schema: &registry::ToolSchema) -> String {
    let mut o = serde_json::Map::new();
    let types: std::collections::BTreeMap<String, String> = schema.fields().into_iter().collect();
    for r in schema.required() {
        let ty = types.get(&r).cloned().unwrap_or_default();
        let v = if ty.starts_with("array") {
            json!([])
        } else if ty.starts_with("integer") {
            json!(1)
        } else if ty.starts_with("boolean") {
            json!(false)
        } else {
            json!("...")
        };
        o.insert(r, v);
    }
    Value::Object(o).to_string()
}

/// stdin вызова. Модель регулярно кладёт сюда **объект** вместо JSON-строки
/// (`"stdin": {"old_string": …, "new_string": …}`) — схема допускает строку, но
/// фактически приходит структура. Молча терять её нельзя: тул получал пустой stdin
/// и отвечал «пустой stdin» либо «не JSON», а модель шла переписывать файл целиком
/// (тикет `edit-stdin-truncated`). Объект/массив сериализуем обратно в JSON —
/// это ровно тот текст, который тул и ждал.
fn stdin_of(a: &Value) -> Option<String> {
    match a.get("stdin") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(v @ (Value::Object(_) | Value::Array(_))) => Some(v.to_string()),
        Some(Value::Null) | None => None,
        // Число/бул в stdin — почти наверняка ошибка вызывающего; отдаём как есть,
        // тул скажет своё «не JSON» с колонкой.
        Some(v) => Some(v.to_string()),
    }
}

/// exit < 0 = процесс убит сигналом (tokio отдаёт -1 без подробностей). Частая
/// причина — `pkill -f`: shell-обёртка `run` содержит текст шаблона в собственном
/// cmdline и `pkill -f` убивает сам себя, а остаток `;`-цепочки не выполняется.
/// Один симптом, две трактовки — говорим об этом в ответе, а не оставляем -1 без слов.
fn explain_signal_death(exit: i32, bytes: usize, text: &str, command: &str) -> String {
    if exit >= 0 {
        return text.to_string();
    }
    let mut note = String::from(
        "[процесс убит сигналом (exit -1): команда не завершилась сама — убийца снаружи или\n   обёртка совпала с собственным шаблоном]",
    );
    if command.contains("pkill -f") || command.contains("pgrep -f") {
        note.push_str(
            "\n[похоже на `pkill -f`: шаблон встречается в тексте самой команды, обёртка\n   убивает себя и хвост `;`-цепочки не выполняется. Убивай по pid: `pgrep -f <шаблон>\n   | grep -v $$` → kill, либо сузь шаблон до `[p]attern`]",
        );
    }
    if bytes == 0 {
        note
    } else {
        format!("{text}\n{note}")
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

    /// Схема как у read: поле + флаги с значениями.
    fn read_schema() -> registry::ToolSchema {
        let v = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"}
            },
            "required": ["path"],
            "x-snens": {
                "argv": [
                    {"field": "path"},
                    {"flag": "--offset", "field": "offset"},
                    {"flag": "--limit", "field": "limit"}
                ],
                "stdin": {"mode": "none"}
            }
        });
        let dir = std::env::temp_dir().join(format!("snens-schema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("read.json");
        std::fs::write(&f, v.to_string()).unwrap();
        registry::read_schema(&f).unwrap().unwrap()
    }

    /// Схема как у edit: один позиционный аргумент + JSON на stdin.
    fn edit_schema() -> registry::ToolSchema {
        let v = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"},
                "edits": {"type": "array", "items": {"type": "object"}}
            },
            "required": ["path"],
            "x-snens": {
                "argv": [{"field": "path"}],
                "stdin": {"mode": "json", "fields": ["old_string", "new_string", "replace_all", "edits"]}
            }
        });
        let dir = std::env::temp_dir().join(format!("snens-schema2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("edit.json");
        std::fs::write(&f, v.to_string()).unwrap();
        registry::read_schema(&f).unwrap().unwrap()
    }

    #[test]
    fn typed_argv_builds_all_three_rule_kinds() {
        let s = read_schema();
        let tc = typed_call(
            "read",
            &s,
            &json!({"path": "/tmp/x", "offset": 3, "limit": 5}),
            Path::new("/base"),
        )
        .unwrap();
        assert_eq!(tc.argv, vec!["/tmp/x", "--offset", "3", "--limit", "5"]);
        assert_eq!(tc.stdin, None, "mode=none должен закрывать stdin");
        // Незаданное поле не эмитится вовсе (а не пустой строкой).
        let tc = typed_call("read", &s, &json!({"path": "/tmp/x"}), Path::new("/base")).unwrap();
        assert_eq!(tc.argv, vec!["/tmp/x"]);
        // const-литерал и bool-флаг собираются по спеке.
        let v = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "append": {"type": "boolean"}},
            "required": ["path"],
            "x-snens": {
                "argv": [
                    {"const": "run"},
                    {"field": "path"},
                    {"flag": "--append", "when_field": "append"}
                ],
                "stdin": {"mode": "none"}
            }
        });
        let dir = std::env::temp_dir().join(format!("snens-schema3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("w.json");
        std::fs::write(&f, v.to_string()).unwrap();
        let w = registry::read_schema(&f).unwrap().unwrap();
        let tc = typed_call(
            "write",
            &w,
            &json!({"path": "/tmp/y", "append": true}),
            Path::new("/base"),
        )
        .unwrap();
        assert_eq!(tc.argv, vec!["run", "/tmp/y", "--append"]);
        // append=false — флага нет.
        let tc = typed_call(
            "write",
            &w,
            &json!({"path": "/tmp/y", "append": false}),
            Path::new("/base"),
        )
        .unwrap();
        assert_eq!(tc.argv, vec!["run", "/tmp/y"]);
    }

    #[test]
    fn typed_stdin_json_is_built_by_core() {
        let s = edit_schema();
        let tc = typed_call(
            "edit",
            &s,
            &json!({"path": "/f", "old_string": "a", "new_string": "b"}),
            Path::new("/base"),
        )
        .unwrap();
        assert_eq!(tc.argv, vec!["/f"]);
        let sent: Value = serde_json::from_str(tc.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(sent["old_string"], "a");
        assert_eq!(sent["new_string"], "b");
        assert!(sent.get("replace_all").is_none(), "незаданное поле лишнее");
        // Незаданных полей нет вовсе — объекта тоже (тул скажет про пустой stdin).
        let tc = typed_call("edit", &s, &json!({"path": "/f"}), Path::new("/base")).unwrap();
        assert_eq!(tc.stdin, None);
    }

    #[test]
    fn typed_stdin_field_is_raw_value() {
        let v = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
                "append": {"type": "boolean"}
            },
            "required": ["path", "content"],
            "x-snens": {
                "argv": [{"field": "path"}, {"flag": "--append", "when_field": "append"}],
                "stdin": {"mode": "field", "field": "content"}
            }
        });
        let dir = std::env::temp_dir().join(format!("snens-schema4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("write.json");
        std::fs::write(&f, v.to_string()).unwrap();
        let s = registry::read_schema(&f).unwrap().unwrap();
        let body = "строка 1\nстрока 2 \"в кавычках\"\n";
        let tc = typed_call(
            "write",
            &s,
            &json!({"path": "/f", "content": body}),
            Path::new("/base"),
        )
        .unwrap();
        assert_eq!(tc.stdin.as_deref(), Some(body), "содержимое без JSON-обёртки");
    }

    #[test]
    fn typed_missing_required_names_field() {
        let s = read_schema();
        let e = typed_call("read", &s, &json!({"limit": 5}), Path::new("/base")).unwrap_err();
        assert!(e.contains("path"), "имя поля не названо: {e}");
        assert!(e.contains("обязательных"), "{e}");
        // Процесс не запускался — ошибка вернулась текстом.
    }

    #[test]
    fn typed_unknown_field_lists_allowed() {
        let s = read_schema();
        let e = typed_call(
            "read",
            &s,
            &json!({"path": "/f", "file": "/f"}),
            Path::new("/base"),
        )
        .unwrap_err();
        assert!(e.contains("file"), "имя неизвестного поля: {e}");
        assert!(e.contains("path") && e.contains("offset"), "нет списка допустимых: {e}");
    }

    #[test]
    fn typed_tolerates_double_encoded_and_single_object() {
        let s = edit_schema();
        // Массив edits приехал JSON-строкой: разбираем.
        let payload = json!([{"old_string": "a", "new_string": "b"}]);
        let tc = typed_call(
            "edit",
            &s,
            &json!({"path": "/f", "edits": payload.to_string()}),
            Path::new("/base"),
        )
        .unwrap();
        let sent: Value = serde_json::from_str(tc.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(sent["edits"][0]["old_string"], "a", "{sent}");
        // Одиночный объект вместо массива — массив из одного.
        let tc = typed_call(
            "edit",
            &s,
            &json!({"path": "/f", "edits": {"old_string": "a", "new_string": "b"}}),
            Path::new("/base"),
        )
        .unwrap();
        let sent: Value = serde_json::from_str(tc.stdin.as_deref().unwrap()).unwrap();
        assert!(sent["edits"].is_array(), "{sent}");
        assert_eq!(sent["edits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn typed_accepts_legacy_args_stdin() {
        // Модель зовёт типизированный тул по-старому: stdin строкой с JSON.
        let s = edit_schema();
        let a = json!({
            "args": ["/f"],
            "stdin": "{\"old_string\":\"a\",\"new_string\":\"b\"}"
        });
        let tc = typed_call("edit", &s, &a, Path::new("/base")).unwrap();
        assert_eq!(tc.argv, vec!["/f"]);
        let sent: Value = serde_json::from_str(tc.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(sent["new_string"], "b");
    }

    #[test]
    fn typed_cwd_resolves_against_session_cwd() {
        let s = read_schema();
        let tc = typed_call("read", &s, &json!({"path": "x"}), Path::new("/sess")).unwrap();
        assert_eq!(tc.cwd, PathBuf::from("/sess"));
        let tc = typed_call(
            "read",
            &s,
            &json!({"path": "x", "cwd": "sub"}),
            Path::new("/sess"),
        )
        .unwrap();
        assert_eq!(tc.cwd, PathBuf::from("/sess/sub"));
    }

    #[test]
    fn stdin_accepts_object_payload() {
        // Наблюдённый промах: модель кладёт JSON-объект в `stdin`, а не строку.
        let a = json!({"args": ["/tmp/x"], "stdin": {"old_string": "a", "new_string": "b"}});
        let s = stdin_of(&a).expect("объект в stdin не должен теряться");
        assert!(s.contains("old_string"), "{s}");
        assert_eq!(
            serde_json::from_str::<Value>(&s).unwrap()["new_string"],
            "b"
        );
        assert_eq!(stdin_of(&json!({"stdin": "plain"})).unwrap(), "plain");
        assert!(stdin_of(&json!({})).is_none());
        assert!(stdin_of(&json!({"stdin": null})).is_none());
    }

    #[test]
    fn signal_death_is_explained() {
        let txt = explain_signal_death(-1, 0, "", "pkill -f 'app_live.py'; ls");
        assert!(txt.contains("убит сигналом"), "{txt}");
        assert!(txt.contains("pkill -f"), "нет подсказки про самоубийство: {txt}");
        // Обычный exit не трогаем и текст не дублируем.
        assert_eq!(explain_signal_death(0, 5, "ok", "echo hi"), "ok");
        assert_eq!(
            explain_signal_death(-1, 5, "partial", "kill -9 1"),
            "partial\n[процесс убит сигналом (exit -1): команда не завершилась сама — убийца снаружи или\n   обёртка совпала с собственным шаблоном]"
        );
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