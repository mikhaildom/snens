// Настройки инстанса, правимые на ходу: то, что человек меняет без рестарта
// сервиса и без редактора в конфиге.
//
// Один список ключей на три поверхности — веб-клиент (`GET/PUT /v1/config`),
// `snens config` и файл конфига: иначе каждая новая галочка обрастала бы своим
// маршрутом и своей проверкой. Значения читаются из файла (`Config::fresh`), а не
// из снимка старта процесса: живой сервис обязан показывать то, что реально
// лежит на диске.

use serde_json::{json, Value};

use crate::{
    cfg::{Config, RecallMode},
    R,
};

/// Что именно правится через API. Список — контракт: ключ, которого здесь нет,
/// `PUT /v1/config` отвергает, а не молча пишет в никуда.
pub const KEYS: &[&str] = &[
    "model",
    "subagent_model",
    "max_turns",
    "review_min_turns",
    "review_idle_seconds",
    "timeout_seconds",
    "max_result_chars",
    "reduce_output",
    "attachments",
    "ctx_window",
    "context_budget",
    "context_target",
    "memory_enabled",
    "memory_recall",
    "memory_retain",
    "memory_cmd",
];

/// Текущие значения правимых ключей (из файла, не из снимка старта).
pub fn values(cfg: &Config) -> Value {
    let live = cfg.fresh().unwrap_or_else(|| cfg.clone());
    let mem = &live.memory;
    json!({
        "model": live.model,
        "subagent_model": live.subagent_model.clone().unwrap_or_default(),
        "max_turns": live.max_turns,
        "review_min_turns": live.review_min_turns,
        "review_idle_seconds": live.review_idle_seconds,
        "timeout_seconds": live.timeout_seconds,
        "max_result_chars": live.max_result_chars,
        "reduce_output": live.reduce_output,
        "attachments": live.attachments,
        "ctx_window": live.ctx_window,
        "context_budget": live.context_budget,
        "context_target": live.context_target,
        "memory_enabled": mem.enabled,
        "memory_recall": match mem.recall {
            RecallMode::Always => "always",
            RecallMode::Session => "session",
            RecallMode::Off => "off",
        },
        "memory_retain": mem.retain,
        "memory_cmd": mem.cmd,
    })
}

/// Ответ поверхности: путь конфига и значения после правки.
pub fn state(cfg: &Config, path: &std::path::Path) -> Value {
    json!({ "path": path.display().to_string(), "values": values(cfg) })
}

fn int(v: &Value, key: &str, min: u64, max: u64) -> R<toml_edit::Value> {
    let n = v
        .as_u64()
        .ok_or_else(|| format!("{key}: ожидается целое число"))?;
    if n < min || n > max {
        return Err(format!("{key}: вне диапазона {min}..{max}").into());
    }
    crate::cfg_edit::int(n)
}

fn boolean(v: &Value, key: &str) -> R<toml_edit::Value> {
    v.as_bool()
        .map(crate::cfg_edit::boolean)
        .ok_or_else(|| format!("{key}: ожидается true/false").into())
}

/// Строковое поле: только текст, без управляющих символов. Перевод строки в
/// модели или имени программы ядру не нужен, а в конфиге он превращался в
/// сломанный документ (нормальный парсер его уже экранирует — здесь запрет по
/// смыслу значения, а не по синтаксису).
fn text(v: &Value, key: &str) -> R<String> {
    let s = v
        .as_str()
        .ok_or_else(|| format!("{key}: ожидается строка"))?
        .to_string();
    if s.chars().any(|c| c.is_control()) {
        return Err(format!("{key}: управляющие символы в значении недопустимы").into());
    }
    Ok(s)
}

/// Проверить правку, ничего не записывая. Ошибка здесь — ошибка клиента (400).
///
/// Возвращает правки документа (`cfg_edit::Edit`): значения уже типизированы и
/// экранированы парсером, поэтому перевод строки в строковом поле не может
/// раскавычить конфиг.
pub fn prepare(cfg: &Config, patch: &Value) -> R<Vec<crate::cfg_edit::Edit>> {
    use crate::cfg_edit::Edit;
    let obj = patch
        .as_object()
        .ok_or("правка должна быть объектом {ключ: значение}")?;
    if obj.is_empty() {
        return Err("пустая правка".into());
    }
    let mut edits: Vec<Edit> = Vec::new();
    let mut nums: Vec<(&'static str, u64)> = Vec::new();
    for (key, value) in obj {
        let k: &'static str = KEYS
            .iter()
            .find(|x| **x == key.as_str())
            .copied()
            .ok_or_else(|| format!("неизвестный ключ настройки: {key}"))?;
        match k {
            "model" => {
                let s = text(value, k)?.trim().to_string();
                if s.is_empty() {
                    return Err("model: пустой slug".into());
                }
                edits.push(Edit::top("model", crate::cfg_edit::string(&s)));
            }
            // Пусто — «как основная модель»: писать `subagent_model = ""` нельзя,
            // это не пустая строка-заглушка, а модель с пустым именем, на которой
            // подагент падает. Поэтому ключ просто убираем.
            "subagent_model" => {
                let s = text(value, k)?.trim().to_string();
                edits.push(Edit::remove_top("subagent_model"));
                if !s.is_empty() {
                    edits.push(Edit::top(
                        "subagent_model",
                        crate::cfg_edit::string(&s),
                    ));
                }
            }
            "max_turns" => edits.push(Edit::top(k, int(value, k, 0, 100_000)?)),
            "review_min_turns" => edits.push(Edit::top(k, int(value, k, 0, 100_000)?)),
            "review_idle_seconds" => edits.push(Edit::top(k, int(value, k, 0, 86_400)?)),
            "timeout_seconds" => edits.push(Edit::top(k, int(value, k, 1, 86_400)?)),
            "max_result_chars" => edits.push(Edit::top(k, int(value, k, 200, 1_000_000)?)),
            "reduce_output" => edits.push(Edit::top(k, boolean(value, k)?)),
            "attachments" => edits.push(Edit::top(k, boolean(value, k)?)),
            "ctx_window" => edits.push(Edit::top(k, int(value, k, 1_000, 100_000_000)?)),
            "context_budget" | "context_target" => {
                let n = value
                    .as_u64()
                    .ok_or_else(|| format!("{k}: ожидается целое число"))?;
                if n > 100_000_000 {
                    return Err(format!("{k}: вне диапазона 0..100000000").into());
                }
                nums.push((k, n));
                edits.push(Edit::top(k, crate::cfg_edit::int(n)?));
            }
            "memory_enabled" => edits.push(Edit::section("memory", "enabled", boolean(value, k)?)),
            "memory_retain" => edits.push(Edit::section("memory", "retain", boolean(value, k)?)),
            "memory_cmd" => {
                let s = text(value, k)?;
                edits.push(Edit::section("memory", "cmd", crate::cfg_edit::string(&s)));
            }
            "memory_recall" => {
                let mode = text(value, k)?;
                if !matches!(mode.as_str(), "always" | "session" | "off") {
                    return Err("memory_recall: ожидается always|session|off".into());
                }
                edits.push(Edit::section(
                    "memory",
                    "recall",
                    crate::cfg_edit::string(&mode),
                ));
            }
            _ => unreachable!("ключи перечислены в KEYS и разобраны выше"),
        }
    }
    // Цель уборки не может быть больше бюджета: иначе она недостижима и правка
    // «тихо не работает». Бюджет 0 — это авто по окну модели, и там предел
    // известен только ядру: обещать человеку число, которое будет обрезано,
    // нельзя, поэтому цель без бюджета отвергаем.
    let live = cfg.fresh().unwrap_or_else(|| cfg.clone());
    let budget = pick(&nums, "context_budget").unwrap_or(live.context_budget);
    let target = pick(&nums, "context_target").unwrap_or(live.context_target);
    if target > 0 {
        if budget == 0 {
            return Err(
                "context_target задан, а context_budget = 0 (авто по окну): непонятно, с чем \
                 сравнивать цель — задай бюджет явно"
                    .into(),
            );
        }
        if target > budget {
            return Err(format!("context_target {target} больше context_budget {budget}").into());
        }
    }
    Ok(edits)
}

/// Записать проверенную правку в конфиг — одной атомарной записью.
pub fn commit(cfg: &Config, edits: &[crate::cfg_edit::Edit]) -> R<std::path::PathBuf> {
    let path = cfg.source_path.clone().unwrap_or_else(crate::cfg::config_path);
    crate::cfg_edit::write(&path, edits)
}

/// Проверить, записать и вернуть путь конфига: повторяющаяся пара шагов.
pub fn apply(cfg: &Config, patch: &Value) -> R<std::path::PathBuf> {
    commit(cfg, &prepare(cfg, patch)?)
}

/// Значение ключа из уже проверенной правки (число как u64).
fn pick(nums: &[(&str, u64)], key: &str) -> Option<u64> {
    nums.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `snens config [ключ=значение …]` — тот же список ключей, что у `PUT /v1/config`:
/// без аргументов печатает текущие значения из файла, с ними — правит.
/// Действует со следующего хода: живой процесс перечитывает файл (`Config::live`).
pub fn cmd(flags: &std::collections::BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    if positional.is_empty() {
        let now = values(&cfg);
        let path = cfg
            .source_path
            .clone()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(файла нет)".into());
        println!("# {path}");
        for key in KEYS {
            // Строкой без кавычек — так это копируется обратно в `config key=value`.
            let v = &now[*key];
            let shown = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            println!("{key} = {shown}");
        }
        return Ok(());
    }
    let mut patch = serde_json::Map::new();
    for arg in positional {
        let (key, raw) = arg
            .split_once('=')
            .ok_or_else(|| format!("ожидается ключ=значение, а не {arg}"))?;
        // Тип угадываем по виду значения: true/false — флаг, число — число,
        // остальное — строка. Валидацию типов делает сам `apply`.
        let value = if raw == "true" || raw == "false" {
            Value::Bool(raw == "true")
        } else if let Ok(n) = raw.parse::<u64>() {
            Value::from(n)
        } else {
            Value::from(raw)
        };
        patch.insert(key.to_string(), value);
    }
    let path = apply(&cfg, &Value::Object(patch))?;
    println!("{}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(text: &str, name: &str) -> (Config, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("snens-set-{}-{name}.toml", std::process::id()));
        std::fs::write(&path, text).unwrap();
        let cfg = Config::load(Some(&path.display().to_string())).unwrap();
        (cfg, path)
    }

    #[test]
    fn reads_values_from_file_not_from_snapshot() {
        let (cfg, path) = cfg_with("model = \"a\"\nmax_turns = 5\n", "read");
        // Правка файла мимо процесса — значения меняются: меню показывает диск.
        std::fs::write(&path, "model = \"b\"\nmax_turns = 7\n").unwrap();
        let v = values(&cfg);
        assert_eq!(v["model"], "b");
        assert_eq!(v["max_turns"], 7);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unlimited_turns_is_zero_and_written_top_level() {
        let (cfg, path) = cfg_with(
            "# шапка\nmodel = \"a\"\n\n[memory]\ntimeout_seconds = 60\n",
            "turns",
        );
        apply(&cfg, &json!({"max_turns": 0})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# шапка"), "{text}");
        assert!(text.contains("max_turns = 0"), "{text}");
        // Одноимённый ключ в секции не тронут: верхний уровень — только до `[`.
        assert!(text.contains("timeout_seconds = 60"), "{text}");
        let reloaded = Config::load(Some(&path.display().to_string())).unwrap();
        assert_eq!(reloaded.max_turns, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn memory_keys_go_into_their_section() {
        let (cfg, path) = cfg_with("model = \"a\"\n\n[memory]\ncmd = \"memory\"\n", "mem");
        apply(&cfg, &json!({"memory_recall": "off", "memory_retain": false})).unwrap();
        let reloaded = Config::load(Some(&path.display().to_string())).unwrap();
        assert_eq!(reloaded.memory.recall, RecallMode::Off);
        assert!(!reloaded.memory.retain);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn bad_patch_writes_nothing() {
        let (cfg, path) = cfg_with("model = \"a\"\n", "bad");
        // Первое поле валидно, второе — нет: файл обязан остаться как был.
        assert!(apply(
            &cfg,
            &json!({"max_turns": 5, "no_such_key": 1})
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model = \"a\"\n");
        assert!(apply(&cfg, &json!({"context_budget": 100, "context_target": 500})).is_err());
        std::fs::remove_file(&path).ok();
    }

    /// Значение с переводом строки: раньше это оставляло на диске конфиг, который
    /// не разбирался, и `serve` не поднимался. Теперь — отказ до записи.
    #[test]
    fn control_chars_are_rejected_before_writing() {
        let (cfg, path) = cfg_with("model = \"a\"\nmax_turns = 5\n", "ctrl");
        assert!(apply(&cfg, &json!({"subagent_model": "a\nb"})).is_err());
        // Пачка с одним плохим полем не применяется вовсе: ни `max_turns`, ни битой строки.
        assert!(apply(&cfg, &json!({"max_turns": 77, "subagent_model": "a\nb"})).is_err());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "model = \"a\"\nmax_turns = 5\n");
        // И файл по-прежнему читается.
        assert!(Config::load(Some(&path.display().to_string())).is_ok());
        std::fs::remove_file(&path).ok();
    }

    /// Пустая модель подагента — «как основная»: ключ убирается, а не пишется
    /// пустой строкой (с ней `run-child` уходил моделью с пустым именем).
    #[test]
    fn empty_subagent_model_removes_the_key() {
        let (cfg, path) = cfg_with(
            "model = \"a\"\nsubagent_model = \"x\"\n\n[memory]\ncmd = \"memory\"\n",
            "sub",
        );
        apply(&cfg, &json!({"subagent_model": ""})).unwrap();
        let reloaded = Config::load(Some(&path.display().to_string())).unwrap();
        assert_eq!(reloaded.subagent_model, None);
        // Соседняя секция цела: правка верхнего уровня не задела `[memory]`.
        assert_eq!(reloaded.memory.cmd, "memory");
        std::fs::remove_file(&path).ok();
    }

    /// Цель уборки без явного бюджета отвергается: `context_budget = 0` — авто по
    /// окну, и обещать число, которое ядро молча обрежет, нельзя.
    #[test]
    fn target_without_explicit_budget_is_refused() {
        let (cfg, path) = cfg_with("model = \"a\"\n", "target");
        assert!(apply(&cfg, &json!({"context_target": 900000})).is_err());
        assert!(apply(&cfg, &json!({"context_budget": 100, "context_target": 500})).is_err());
        // Согласованная пара проходит.
        apply(&cfg, &json!({"context_budget": 100000, "context_target": 70000})).unwrap();
        let reloaded = Config::load(Some(&path.display().to_string())).unwrap();
        assert_eq!(reloaded.context_budget, 100_000);
        assert_eq!(reloaded.context_target, 70_000);
        std::fs::remove_file(&path).ok();
    }
}
