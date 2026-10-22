// Тесты профилей провайдеров: сериализация секции, запись в конфиг и `.env`,
// валидация правок и запреты (активный профиль, неполный профиль).
//
// Отдельным файлом — конвенция ядра: модуль кода до 400 строк, тесты рядом.

use super::*;
use serde_json::json;
use std::path::PathBuf;

/// Конфиг во временном каталоге: рядом `.env`, который создаёт правка.
fn cfg_at(text: &str, name: &str) -> (Config, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "snens-prov-{}-{name}-{}",
        std::process::id(),
        name.len()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    let cfg = Config::load(Some(&path.display().to_string())).unwrap();
    (cfg, path)
}

fn names(list: &Value) -> Vec<String> {
    list["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn section_is_read_and_active_profile_wins() {
    let (cfg, path) = cfg_at(
        "base_url = \"http://top\"\nmodel = \"top/model\"\nkey_env = \"TOP_KEY\"\n\
         active_provider = \"nous\"\n\n\
         [providers.nous]\nbase_url = \"https://nous/v1/\"\nmodel = \"n/m\"\nkey_env = \"NOUS_KEY\"\n\n\
         [providers.orphan]\nbase_url = \"http://x\"\n",
        "read",
    );
    assert_eq!(cfg.base_url, "https://nous/v1/", "активный перебивает top-level");
    assert_eq!(cfg.model, "n/m");
    assert_eq!(cfg.key_env, "NOUS_KEY");
    // Список видит оба профиля, активен один.
    let list = list(&cfg);
    assert_eq!(names(&list), vec!["nous", "orphan"]);
    assert_eq!(list["active"], "nous");
    assert_eq!(list["providers"][0]["active"], true);
    // Неполный профиль не может стать активным: иначе ядро встанет.
    assert!(activate(&cfg, "orphan").is_err());
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn no_section_falls_back_to_top_level() {
    let (cfg, path) = cfg_at("base_url = \"http://top\"\nmodel = \"m\"\n", "fallback");
    let list = list(&cfg);
    assert_eq!(names(&list), vec![TOP_LEVEL.to_string()]);
    assert_eq!(list["providers"][0]["base_url"], "http://top");
    assert_eq!(list["providers"][0]["active"], true);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn incomplete_active_profile_does_not_break_the_core() {
    // Человек стёр модель у активного профиля в редакторе: ядро обязано
    // остаться работоспособным на top-level, а не падать на пустом slug.
    let cfg: Config = toml::from_str(
        "base_url = \"http://top\"\nmodel = \"m\"\nactive_provider = \"broken\"\n\n\
         [providers.broken]\nbase_url = \"http://x\"\n",
    )
    .unwrap();
    assert!(cfg.active_profile().is_none());
    assert_eq!(cfg.effective_model(), "m");
}

#[test]
fn create_writes_key_to_env_not_to_toml() {
    let (cfg, path) = cfg_at("# шапка\nmodel = \"top\"\n", "create");
    let draft = Draft {
        base_url: Some("https://openrouter.ai/api/v1/".into()),
        model: Some("m/one".into()),
        key: Some("sk-secret".into()),
        key_env: None,
    };
    write_profile(&cfg, "openrouter", &draft, None, true).unwrap();
    let toml = std::fs::read_to_string(&path).unwrap();
    assert!(toml.contains("# шапка"), "комментарий человека съеден: {toml}");
    assert!(toml.contains("[providers.openrouter]"), "{toml}");
    assert!(toml.contains("active_provider = \"openrouter\""), "{toml}");
    // Хвостовой слэш убран, ключа в TOML нет.
    assert!(toml.contains("base_url = \"https://openrouter.ai/api/v1\""), "{toml}");
    assert!(!toml.contains("sk-secret"), "ключ попал в конфиг: {toml}");
    let env = std::fs::read_to_string(path.with_file_name(".env")).unwrap();
    assert!(env.contains("SNENS_OPENROUTER_KEY=sk-secret"), "{env}");
    // И ядро после перечитки работает именно этим профилем.
    let live = Config::load(Some(&path.display().to_string())).unwrap();
    assert_eq!(live.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(live.model, "m/one");
    assert_eq!(live.api_key().unwrap(), "sk-secret");
    assert!(list(&live)["providers"][0]["key_set"].as_bool().unwrap());
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn update_without_key_keeps_the_old_one() {
    let (cfg, path) = cfg_at("model = \"top\"\n", "update");
    let create = Draft {
        base_url: Some("https://a/v1".into()),
        model: Some("m1".into()),
        key: Some("k1".into()),
        key_env: None,
    };
    write_profile(&cfg, "a", &create, None, false).unwrap();
    let live = Config::load(Some(&path.display().to_string())).unwrap();
    let existing = live.providers["a"].clone();
    // Правка модели без ключа: ключ не стирается, профиль не активируется.
    let edit = Draft {
        model: Some("m2".into()),
        ..Default::default()
    };
    write_profile(&live, "a", &edit, Some(&existing), false).unwrap();
    let live = Config::load(Some(&path.display().to_string())).unwrap();
    assert_eq!(live.providers["a"].model, "m2");
    assert_eq!(live.providers["a"].base_url, "https://a/v1");
    // Активным он не стал: создавали без activate, и top-level ключ не тронут.
    assert_eq!(live.active_provider, "");
    // Ключ жив: он в `.env` под именем своего профиля, а не стёрт правкой.
    let env = std::fs::read_to_string(path.with_file_name(".env")).unwrap();
    assert!(env.contains("k1"), "{env}");
    activate(&live, "a").unwrap();
    let live = Config::load(Some(&path.display().to_string())).unwrap();
    assert_eq!(live.api_key().unwrap(), "k1");
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn active_profile_cannot_be_removed_and_switch_unblocks_it() {
    let (cfg, path) = cfg_at("model = \"top\"\n", "remove");
    for (name, activate) in [("a", true), ("b", false)] {
        let draft = Draft {
            base_url: Some(format!("https://{name}/v1")),
            model: Some("m".into()),
            key: Some(format!("k-{name}")),
            key_env: None,
        };
        write_profile(&cfg, name, &draft, None, activate).unwrap();
    }
    assert!(remove_profile(&cfg, "a").is_err(), "активный удалился");
    activate(&cfg, "b").unwrap();
    remove_profile(&cfg, "a").unwrap();
    let live = Config::load(Some(&path.display().to_string())).unwrap();
    assert!(!live.providers.contains_key("a"));
    assert!(live.providers.contains_key("b"));
    // Секрет удалённого профиля не остался в .env без хозяина.
    let env = std::fs::read_to_string(path.with_file_name(".env")).unwrap();
    assert!(!env.contains("k-a"), "{env}");
    assert!(env.contains("k-b"), "ключ живого профиля пропал: {env}");
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn shared_variable_survives_removal_of_one_profile() {
    let (cfg, path) = cfg_at("model = \"top\"\n", "shared");
    let base = Draft {
        base_url: Some("https://a/v1".into()),
        model: Some("m".into()),
        key: Some("k".into()),
        key_env: Some("SHARED_KEY".into()),
    };
    write_profile(&cfg, "a", &base, None, false).unwrap();
    write_profile(&cfg, "b", &base, None, true).unwrap();
    remove_profile(&cfg, "a").unwrap();
    let env = std::fs::read_to_string(path.with_file_name(".env")).unwrap();
    assert!(env.contains("SHARED_KEY=k"), "ключ нужен живому профилю: {env}");
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn validation_rejects_junk_before_writing() {
    assert!(validate_name("").is_err());
    assert!(validate_name("a.b").is_err(), "точка ломает путь таблицы");
    assert!(validate_name("a/b").is_err());
    assert!(validate_name("наш провайдер").is_ok());
    assert!(validate_base_url("").is_err());
    assert!(validate_base_url("ftp://x").is_err());
    assert_eq!(validate_base_url("https://x/v1///").unwrap(), "https://x/v1");
    assert!(validate_model("  ").is_err());
    assert!(validate_key_env("1BAD").is_err());
    assert!(validate_key_env("SNENS_KEY").is_ok());
    // Неизвестное поле в теле — отказ, а не тихая потеря правки.
    assert!(Draft::parse(&json!({"base_url": "https://x" }), "a").is_ok());
    assert!(Draft::parse(&json!({"nosuch": 1}), "a").is_err());
    assert!(Draft::parse(&json!({"base_url": 1}), "a").is_err());
}

#[test]
fn preset_fills_base_url_and_model() {
    let draft = Draft::parse(&json!({"preset": "nous"}), "p").unwrap();
    assert_eq!(
        draft.base_url.as_deref(),
        Some("https://inference-api.nousresearch.com/v1")
    );
    assert!(draft.model.unwrap().contains("deepseek"));
    assert!(Draft::parse(&json!({"preset": "нет-такого"}), "p").is_err());
    // Явный base_url сильнее пресета: человек мог поправить адрес.
    let draft = Draft::parse(
        &json!({"preset": "nous", "base_url": "https://mine/v1"}),
        "p",
    )
    .unwrap();
    assert_eq!(draft.base_url.as_deref(), Some("https://mine/v1"));
    assert_eq!(preset("Open-Router").map(|p| p.0), Some("https://openrouter.ai/api/v1"));
}
