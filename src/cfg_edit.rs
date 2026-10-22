// Правка конфига как документа, а не как строки.
//
// Раньше значение вписывалось текстом: искалась строка `ключ = …` и заменялась
// целиком. Это ломалось на всём, что не помещается в одну строку, и один раз
// уже стоило сервиса — незакрытая кавычка в записанном значении оставляла конфиг
// неразбираемым, и `serve` не поднимался.
//
// `toml_edit` держит документ целиком: комментарии человека, порядок полей и
// многострочные строки остаются как были, а меняется ровно то значение, которое
// просили (`system_prompt = """… max_turns = 4 …"""` больше не перехватывается
// правкой `max_turns`). Запись атомарная: сначала временный файл рядом, проверка
// разбора, потом `rename` — при любой ошибке на диске остаётся прежний конфиг.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::R;

/// Одна правка: значение по ключу верхнего уровня (`section: None`), в секции —
/// или удаление ключа (`value: None`).
pub struct Edit {
    pub section: Option<String>,
    pub key: String,
    pub value: Option<Value>,
}

impl Edit {
    pub fn top(key: impl Into<String>, value: Value) -> Self {
        Self {
            section: None,
            key: key.into(),
            value: Some(value),
        }
    }

    pub fn section(section: impl Into<String>, key: impl Into<String>, value: Value) -> Self {
        Self {
            section: Some(section.into()),
            key: key.into(),
            value: Some(value),
        }
    }

    /// Убрать ключ: «пусто» и «ключа нет» — разные вещи, и у настроек бывает
    /// нужно именно второе (пустая модель подагента означала бы модель с пустым
    /// именем).
    pub fn remove_top(key: impl Into<String>) -> Self {
        Self {
            section: None,
            key: key.into(),
            value: None,
        }
    }
}

/// Вписать значение, сохранив оформление прежнего: комментарий над строкой и
/// отступ — часть конфига человека, а вставка нового `Item` их сносила.
fn put(table: &mut Table, key: &str, value: Value) {
    if let Some((_k, item)) = table.get_key_value_mut(key) {
        if let Item::Value(old) = item {
            let mut fresh = value;
            // Комментарий перед ключом живёт в decor ключа, хвостовой — в decor
            // значения; второе переносим, первое не трогаем вовсе.
            *fresh.decor_mut() = old.decor().clone();
            *old = fresh;
            return;
        }
    }
    table.insert(key, Item::Value(value));
}

/// Применить пачку правок одним заходом: либо все, либо ни одна.
///
/// Проверка «файл остался разбираемым» идёт на содержании временного файла до
/// `rename`, поэтому сломанного конфига на диске не бывает даже на миг.
pub fn write(path: &Path, edits: &[Edit]) -> R<PathBuf> {
    if edits.is_empty() {
        return Err("нечего применять".into());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display()).into()),
    };
    // Файл уже сломан — это не наша правка, но и писать в него вслепую нельзя:
    // сохранить человека можно только поверх разобранного документа.
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("{}: конфиг не разбирается, правка отменена: {e}", path.display()))?;
    for edit in edits {
        let key = edit.key.as_str();
        match (&edit.section, &edit.value) {
            // Удаление: ключа нет — уже то, чего хотели, ошибкой не считаем.
            (None, None) => {
                doc.remove(key);
            }
            (Some(section), None) => {
                if let Some(table) = doc.get_mut(section).and_then(Item::as_table_mut) {
                    table.remove(key);
                }
            }
            (None, Some(value)) => put(&mut doc.as_table_mut(), key, value.clone()),
            (Some(section), Some(value)) => {
                if !doc.contains_key(section) {
                    doc.insert(section, Item::Table(Table::new()));
                }
                let table = doc
                    .get_mut(section)
                    .and_then(Item::as_table_mut)
                    .ok_or_else(|| format!("[{section}] — не таблица, правка отменена"))?;
                put(table, key, value.clone());
            }
        }
    }
    let out = doc.to_string();
    // Проверка до записи: смысл временного файла в том, чтобы прежний остался
    // цел, если новое не читается.
    toml::from_str::<crate::cfg::Config>(&out)
        .map_err(|e| format!("правка дала нечитаемый конфиг, отменена: {e}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("toml.tmp{}", std::process::id()));
    std::fs::write(&tmp, &out)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Временный файл не должен переживать неудачу: иначе следующий запуск
        // найдёт рядом непонятный огрызок.
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()).into());
    }
    Ok(path.to_path_buf())
}

/// Строка для TOML: экранирование делает `toml_edit`, поэтому переводы строк,
/// кавычки и управляющие символы больше не могут раскавычить значение.
pub fn string(s: &str) -> Value {
    Value::from(s)
}

pub fn int(n: u64) -> R<Value> {
    i64::try_from(n)
        .map(Value::from)
        .map_err(|_| format!("{n}: не влезает в целое TOML").into())
}

pub fn boolean(b: bool) -> Value {
    Value::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("snens-edit-{}-{name}.toml", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn multi_line_string_does_not_swallow_the_edit() {
        let path = tmp("multiline");
        // Промпт человека содержит строку, похожую на ключ. Правка реального
        // `max_turns` не должна попасть внутрь строки.
        std::fs::write(
            &path,
            "model = \"real\"\nmax_turns = 200\nsystem_prompt = \"\"\"\nmax_turns = 4\nmodel = \"fake\"\n\"\"\"\n",
        )
        .unwrap();
        write(
            &path,
            &[Edit::top("max_turns", int(7).unwrap()), Edit::top("model", string("new"))],
        )
        .unwrap();
        let cfg = crate::cfg::Config::load(Some(&path.display().to_string())).unwrap();
        assert_eq!(cfg.max_turns, 7);
        assert_eq!(cfg.model, "new");
        let text = std::fs::read_to_string(&path).unwrap();
        // Содержимое промпта цело, комментарии и порядок не тронуты.
        assert!(text.contains("max_turns = 4\nmodel = \"fake\""), "{text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn comment_and_order_survive() {
        let path = tmp("comment");
        std::fs::write(
            &path,
            "# шапка\nmodel = \"a\"\n# про лимит\nmax_turns = 5\n\n[memory]\ncmd = \"memory\"\n",
        )
        .unwrap();
        write(
            &path,
            &[Edit::top("max_turns", int(9).unwrap())],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# шапка"), "{text}");
        assert!(text.contains("# про лимит"), "{text}");
        assert!(text.contains("max_turns = 9"), "{text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn newline_in_value_cannot_break_the_file() {
        let path = tmp("newline");
        std::fs::write(&path, "model = \"a\"\n").unwrap();
        // Значение с переводом строки и кавычкой: раньше это оставляло на диске
        // конфиг, который не разбирался, и сервис не стартовал.
        write(
            &path,
            &[Edit::top("subagent_model", string("a\nb\"c"))],
        )
        .unwrap();
        let cfg = crate::cfg::Config::load(Some(&path.display().to_string())).unwrap();
        assert_eq!(cfg.subagent_model.as_deref(), Some("a\nb\"c"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn section_keys_are_created_and_kept_apart() {
        let path = tmp("section");
        std::fs::write(&path, "model = \"a\"\n\n[memory]\ncmd = \"memory\"\n").unwrap();
        write(
            &path,
            &[
                Edit::section("memory", "enabled", boolean(false)),
                Edit::section("memory", "retain", boolean(true)),
                Edit::top("model", string("b")),
            ],
        )
        .unwrap();
        let cfg = crate::cfg::Config::load(Some(&path.display().to_string())).unwrap();
        assert!(!cfg.memory.enabled);
        assert!(cfg.memory.retain);
        assert_eq!(cfg.model, "b");
        assert_eq!(cfg.memory.cmd, "memory", "чужие ключи секции целы");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn broken_file_is_not_overwritten() {
        let path = tmp("broken");
        std::fs::write(&path, "model = [не toml\n").unwrap();
        assert!(write(&path, &[Edit::top("max_turns", int(1).unwrap())]).is_err());
        // Прежнее содержимое на месте: правка не имеет права добивать файл.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model = [не toml\n");
        std::fs::remove_file(&path).ok();
    }
}
