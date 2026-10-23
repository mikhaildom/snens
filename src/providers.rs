// Профили провайдеров LLM: список, выбор активного и запись в конфиг.
//
// Разделение, которое тут важно: в TOML лежит всё, кроме ключа (`[providers.<имя>]`
// + `active_provider`), а ключ — строкой в `.env` рядом с конфигом. Поэтому
// «записать профиль» — это всегда две записи, и они обязаны быть согласованы:
// сначала ключ в `.env` (он не мешает, если конфиг не запишется), потом секция
// конфига атомарной правкой документа (`cfg_edit` сохраняет комментарии).
//
// Обратная совместимость: секции `[providers]` может не быть вовсе — тогда всё
// работает на top-level `base_url`/`model`/`key_env` (см. `Config::resolve_providers`),
// и `GET` отдаёт этот случай единственным профилем «(конфиг)»: клиенту нужен
// видимый активный провайдер, а не пустой список при рабочем ядре.

use serde_json::{json, Value};

use crate::{
    cfg::{Config, Provider},
    R,
};

/// Пресеты для кнопок «добавить»: адрес и модель, без ключа. Нужны потому, что
/// точный base_url — то, что человек не помнит, а именно он чаще всего и
/// оказывается причиной «401 на пустом месте».
pub const PRESETS: &[(&str, &str, &str)] = &[
    (
        "openrouter",
        "https://openrouter.ai/api/v1",
        "deepseek/deepseek-v4.1-flash-1017",
    ),
    (
        "nous",
        "https://inference-api.nousresearch.com/v1",
        "deepseek/deepseek-v4.1-flash-1017",
    ),
];

/// Пресет по имени: `(base_url, model)`. Имя нормализуется (регистр, `-`/`_`).
pub fn preset(name: &str) -> Option<(&'static str, &'static str)> {
    // `open-router`, `Open Router` и `openrouter` — один и тот же пресет.
    let key: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect();
    PRESETS
        .iter()
        .find(|(n, _, _)| *n == key)
        .map(|(_, base, model)| (*base, *model))
}

/// Что человек прислал для профиля. Все поля необязательные, потому что это же
/// тело служит правкой: `PUT` без `key` не должен стирать ключ.
#[derive(Default)]
pub struct Draft {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub key: Option<String>,
    /// `key_env` руками — для профиля, чья переменная уже есть в окружении.
    pub key_env: Option<String>,
}

impl Draft {
    /// Разбор тела запроса. Неизвестные поля игнорируются: клиент новее ядра —
    /// не повод для 400, а вот неверный тип поля — повод.
    pub fn parse(body: &Value, name: &str) -> R<Self> {
        let obj = body
            .as_object()
            .ok_or("ожидается объект {base_url, model, key}")?;
        let mut draft = Draft::default();
        for (key, value) in obj {
            let slot = match key.as_str() {
                "base_url" => &mut draft.base_url,
                "model" => &mut draft.model,
                "key" | "api_key" => &mut draft.key,
                "key_env" => &mut draft.key_env,
                "preset" => {
                    let p = value
                        .as_str()
                        .ok_or("preset: ожидается строка")?;
                    let (base, model) = preset(p)
                        .ok_or_else(|| format!("preset: неизвестный пресет {p}"))?;
                    draft.base_url.get_or_insert_with(|| base.to_string());
                    draft.model.get_or_insert_with(|| model.to_string());
                    continue;
                }
                "name" => {
                    // Имя приходит и в теле (POST): расхождение с путём — ошибка
                    // клиента, а не повод угадывать, кого он имел в виду.
                    let given = value.as_str().ok_or("name: ожидается строка")?;
                    if !name.is_empty() && given.trim() != name && !given.trim().is_empty() {
                        return Err(format!("name: в теле «{given}», а в пути «{name}»").into());
                    }
                    continue;
                }
                // Вычисляемые поля ответа (`active`, `env`) и `activate` клиент
                // может прислать обратно: первое — не правка, второе читает
                // маршрут создания сам. Отвергать их значит ломать клиент на
                // ровном месте.
                "active" | "env" | "key_env_value" | "activate" => continue,
                other => return Err(format!("неизвестное поле профиля: {other}").into()),
            };
            *slot = Some(
                value
                    .as_str()
                    .ok_or_else(|| format!("{key}: ожидается строка"))?
                    .to_string(),
            );
        }
        Ok(draft)
    }
}

/// Имя профиля — это ещё и ключ таблицы TOML (`[providers.<имя>]`), и имя файла
/// переменной. Точки запрещены: путь правки разбирается по ним, и профиль
/// `a.b` оказался бы вложенной таблицей. Всё непечатное и `/` — тоже нельзя.
pub fn validate_name(name: &str) -> R<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err("имя профиля пустое".into());
    }
    if name.chars().count() > 40 {
        return Err("имя профиля длиннее 40 символов".into());
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        || name.contains('.')
    {
        return Err(format!(
            "имя профиля «{name}»: допустимы буквы, цифры, `-`, `_` и пробел"
        )
        .into());
    }
    Ok(())
}

/// Привести имя к виду, в котором оно будет жить в конфиге и в `.env`:
/// пробелы схлопываются, регистр сохраняется (человек назвал — человек читает).
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// base_url: непустой, http(s) и без хвостового слэша (иначе `…/v1//chat/completions`
/// у части провайдеров даёт 404, и это ищут полдня).
pub fn validate_base_url(url: &str) -> R<String> {
    let url = url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return Err("base_url пустой: укажи адрес API провайдера".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("base_url «{url}»: нужен адрес вида https://…").into());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("base_url: пробелы и управляющие символы недопустимы".into());
    }
    Ok(url)
}

pub fn validate_model(model: &str) -> R<String> {
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err("model пустая: укажи slug модели провайдера".into());
    }
    if model.chars().any(|c| c.is_control()) {
        return Err("model: управляющие символы недопустимы".into());
    }
    Ok(model)
}

/// Имя переменной с ключом: либо явно переданное, либо выведенное из имени
/// профиля (`SNENS_<ИМЯ>_KEY`). Пустое у существующего профиля — наследование
/// общего `key_env`, это допустимо.
pub fn validate_key_env(var: &str) -> R<String> {
    let var = var.trim().to_string();
    if var.is_empty() {
        return Ok(var);
    }
    if !var
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        || var.starts_with(|c: char| c.is_ascii_digit())
    {
        return Err(format!(
            "key_env «{var}»: допустимы латиница, цифры и `_`, первой — не цифра"
        )
        .into());
    }
    Ok(var)
}

/// Свободное имя переменной для нового профиля: выведенное из имени, но не
/// занятое другим профилем. Кириллические имена дают одно и то же `SNENS_KEY`, а
/// два профиля на одной переменной — это не «общий аккаунт», а потеря ключа при
/// удалении одного из них.
fn unique_var(cfg: &Config, name: &str) -> String {
    let base = crate::env_file::var_for(name);
    let taken = |var: &str| {
        cfg.providers
            .iter()
            .any(|(_, p)| p.key_env.trim() == var)
    };
    if !taken(&base) {
        return base;
    }
    for n in 2..1000 {
        let candidate = format!("{base}_{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    base
}

/// Профиль в ответе клиенту. Ключ не отдаётся никогда — только признак, что он
/// задан, и имя переменной (по нему человек найдёт строку в `.env`).
pub fn view(cfg: &Config, name: &str, profile: &Provider) -> Value {
    let var = if profile.key_env.trim().is_empty() {
        cfg.key_env.clone()
    } else {
        profile.key_env.clone()
    };
    json!({
        "name": name,
        "base_url": profile.base_url,
        "model": profile.model,
        "key_env": var,
        "key_set": cfg.key_is_set(&var),
        "active": cfg.active_provider.trim() == name,
        // Профиль, заведённый руками без модели, ядро не применит — клиенту
        // полезно знать это, а не выяснять по 400 от провайдера.
        "complete": profile.is_complete(),
    })
}

/// Список профилей. Секции нет — единственный виртуальный профиль из top-level
/// ключей с именем `(конфиг)`: активного провайдера надо видеть всегда.
pub fn list(cfg: &Config) -> Value {
    let live = cfg.fresh().unwrap_or_else(|| cfg.clone());
    let mut items: Vec<Value> = live
        .providers
        .iter()
        .map(|(name, profile)| view(&live, name, profile))
        .collect();
    // Порядок — как в файле у человека: `BTreeMap` уже отсортирован по имени,
    // так список и остаётся стабильным между запросами.
    if items.is_empty() {
        items.push(json!({
            "name": TOP_LEVEL,
            "base_url": live.base_url,
            "model": live.model,
            "key_env": live.key_env,
            "key_set": live.key_is_set(&live.key_env),
            "active": true,
            "complete": true,
            "implicit": true,
        }));
    }
    json!({
        "providers": items,
        "active": if live.active_provider.trim().is_empty() { Value::Null } else { json!(live.active_provider) },
        // Какой адрес и модель ядро взяло бы прямо сейчас: у профиля — его, без
        // секции — top-level. Клиенту это нужно, чтобы показать «работает отсюда»,
        // не повторяя правило приоритета у себя.
        "effective": {
            "base_url": live.base_url,
            "model": live.model,
            "key_env": live.key_env,
            "key_set": live.key_is_set(&live.key_env),
        },
        // Куда писать — клиенту не нужно, но человеку в подсказке панели полезно.
        "config": live.source_path.as_ref().map(|p| p.display().to_string()),
        "presets": PRESETS.iter().map(|(name, base, model)| json!({
            "name": name, "base_url": base, "model": model,
        })).collect::<Vec<_>>(),
    })
}

/// Имя виртуального профиля для конфига без секции `[providers]`. В скобках —
/// чтобы отличать его от настоящего имени профиля (скобки в имени запрещены).
pub const TOP_LEVEL: &str = "(конфиг)";

/// Куда писать: файл конфига этого процесса. `None` — конфига нет вовсе, и
/// правка создаст его рядом с дефолтным путём.
fn config_path(cfg: &Config) -> std::path::PathBuf {
    cfg.source_path
        .clone()
        .unwrap_or_else(crate::cfg::config_path)
}

/// Записать профиль целиком: ключ в `.env`, потом секцию в конфиг.
///
/// Порядок именно такой: `.env` без секции безвреден (лишняя строка), а секция
/// без ключа — профиль, который «активен, но не работает». Частичный сбой
/// оставляет конфиг рабочим.
pub fn write_profile(
    cfg: &Config,
    name: &str,
    draft: &Draft,
    existing: Option<&Provider>,
    activate: bool,
) -> R<()> {
    validate_name(name)?;
    let path = config_path(cfg);
    let base_url = match &draft.base_url {
        Some(url) => validate_base_url(url)?,
        None => existing
            .map(|p| p.base_url.clone())
            .ok_or("base_url пустой: укажи адрес API провайдера")?,
    };
    let model = match &draft.model {
        Some(model) => validate_model(model)?,
        None => existing
            .map(|p| p.model.clone())
            .ok_or("model пустая: укажи slug модели провайдера")?,
    };
    let key_env = match (&draft.key_env, existing) {
        (Some(var), _) => validate_key_env(var)?,
        (None, Some(p)) if !p.key_env.trim().is_empty() => p.key_env.clone(),
        // Имя переменной не задали — выводим из имени профиля, кроме случая
        // «правим профиль, чья переменная была пустой» (наследование сохраняем).
        (None, Some(_)) => String::new(),
        (None, None) => validate_key_env(&unique_var(cfg, name))?,
    };
    // Ключ сохраняем в `.env` ДО записи конфига: если конфиг не запишется,
    // останется только лишняя строка в `.env`, а не профиль без ключа.
    let var = if key_env.is_empty() {
        cfg.key_env.clone()
    } else {
        key_env.clone()
    };
    if let Some(key) = &draft.key {
        let key = key.trim();
        if !key.is_empty() {
            crate::env_file::write(&cfg.env_path(), &var, key)?;
        }
    }
    let mut edits = vec![
        crate::cfg_edit::Edit::section(
            format!("providers.{name}"),
            "base_url",
            crate::cfg_edit::string(&base_url),
        ),
        crate::cfg_edit::Edit::section(
            format!("providers.{name}"),
            "model",
            crate::cfg_edit::string(&model),
        ),
    ];
    // Пустой `key_env` в конфиг не пишем: он значит «как общий», а пустая строка
    // в файле выглядела бы как осознанно заданное пустое значение.
    edits.push(if key_env.is_empty() {
        crate::cfg_edit::Edit::remove_section_key(format!("providers.{name}"), "key_env")
    } else {
        crate::cfg_edit::Edit::section(
            format!("providers.{name}"),
            "key_env",
            crate::cfg_edit::string(&key_env),
        )
    });
    if activate {
        edits.push(crate::cfg_edit::Edit::top(
            "active_provider",
            crate::cfg_edit::string(name),
        ));
    }
    crate::cfg_edit::write(&path, &edits)?;
    Ok(())
}

/// Удалить профиль: секция целиком (а не по ключам — иначе осталась бы пустая
/// таблица и занятое имя), из `.env` — строка его переменной, если её больше не
/// использует никто. Ключ не должен переживать профиль: иначе секрет висит в
/// файле без хозяина.
pub fn remove_profile(cfg: &Config, name: &str) -> R<()> {
    let live = cfg.fresh().unwrap_or_else(|| cfg.clone());
    let Some(profile) = live.providers.get(name) else {
        return Err(format!("профиль «{name}» не найден").into());
    };
    if live.active_provider.trim() == name {
        return Err(
            "нельзя удалить активный провайдер: сначала переключись на другой".into(),
        );
    }
    let var = if profile.key_env.trim().is_empty() {
        live.key_env.clone()
    } else {
        profile.key_env.clone()
    };
    let path = config_path(cfg);
    crate::cfg_edit::write(
        &path,
        &[crate::cfg_edit::Edit::remove_table(format!("providers.{name}"))],
    )?;
    // Переменную убираем, только если на неё больше никто не ссылается: у двух
    // профилей бывает один ключ (один аккаунт, два base_url).
    let still_used = var == live.key_env
        || live
            .providers
            .iter()
            .any(|(other, p)| other != name && p.key_env.trim() == var);
    if !still_used && crate::env_file::read(&var, &live.env_path()).is_some() {
        crate::env_file::write(&live.env_path(), &var, "")?;
    }
    Ok(())
}

/// Сделать профиль активным. Требования к полноте — здесь: переключение на
/// профиль без модели/адреса оставило бы ядро нерабочим, а человек бы решил, что
/// «сломался снапс».
pub fn activate(cfg: &Config, name: &str) -> R<()> {
    let live = cfg.fresh().unwrap_or_else(|| cfg.clone());
    let Some(profile) = live.providers.get(name) else {
        return Err(format!("профиль «{name}» не найден").into());
    };
    if !profile.is_complete() {
        return Err(format!(
            "профиль «{name}» неполный: нужны base_url и model — правь его, а не выбирай"
        )
        .into());
    }
    // Ключа нет — профиль включится, но первый же ход упадёт «ключ не найден».
    // Это не отказ: ключ бывает в окружении, которое ядро не видит глазами, и
    // решать за человека нельзя. Клиент покажет `key_set: false`.
    crate::cfg_edit::write(
        &config_path(cfg),
        &[crate::cfg_edit::Edit::top(
            "active_provider",
            crate::cfg_edit::string(name),
        )],
    )?;
    Ok(())
}

/// `snens providers [list|activate <имя>|key <имя> <ключ>]` — та же правка, что у
/// меню настроек: файл один, значит и правила одни (`write_profile`/`activate`).
pub fn cmd(
    flags: &std::collections::BTreeMap<String, String>,
    positional: Vec<String>,
) -> R<()> {
    if flags.contains_key("help") || flags.contains_key("h") {
        println!(
            "snens providers [list]\n\
             \x20 snens providers activate <имя>   — сделать активным\n\
             \x20 snens providers key <имя> <ключ>  — положить ключ в .env (не в конфиг)"
        );
        return Ok(());
    }
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let mut args = positional.into_iter();
    match args.next().as_deref() {
        None | Some("list") => {
            let state = list(&cfg);
            for p in state["providers"].as_array().cloned().unwrap_or_default() {
                let name = p["name"].as_str().unwrap_or("?");
                let mark = if p["active"].as_bool().unwrap_or(false) { "●" } else { "○" };
                let key = if p["key_set"].as_bool().unwrap_or(false) {
                    "ключ задан".to_string()
                } else {
                    format!("ключа нет ({})", p["key_env"].as_str().unwrap_or(""))
                };
                println!(
                    "{mark} {name}\n  {} · {}\n  {key}",
                    p["base_url"].as_str().unwrap_or(""),
                    p["model"].as_str().unwrap_or("")
                );
            }
            if let Some(eff) = state.get("effective") {
                println!(
                    "сейчас: {} · {}",
                    eff["base_url"].as_str().unwrap_or(""),
                    eff["model"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        Some("activate") => {
            let name = args.next().ok_or("providers activate: нужно имя профиля")?;
            activate(&cfg, name.trim())?;
            println!("активен: {}", name.trim());
            Ok(())
        }
        Some("key") => {
            let name = args.next().ok_or("providers key: нужно имя профиля")?;
            let key = args
                .next()
                .ok_or("providers key: нужен ключ (или - со stdin)")?;
            // Ключ в argv виден в `ps` любому в системе. Флаг `-` читает stdin —
            // так делают `docker login` и `gh auth login`, и не зря.
            let key = if key == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s.trim().to_string()
            } else {
                key
            };
            let live = cfg.fresh().unwrap_or_else(|| cfg.clone());
            let profile = live
                .providers
                .get(name.trim())
                .ok_or_else(|| format!("профиль «{}» не найден", name.trim()))?;
            let var = if profile.key_env.trim().is_empty() {
                live.key_env.clone()
            } else {
                profile.key_env.clone()
            };
            let path = crate::env_file::write(&live.env_path(), &var, &key)?;
            println!("{var} → {}", path.display());
            Ok(())
        }
        Some(other) => Err(format!(
            "providers: непонятная подкоманда {other} (list|activate|key)"
        )
        .into()),
    }
}

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
