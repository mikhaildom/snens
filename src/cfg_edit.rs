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
//
// Путь правки — цепочка таблиц (`["providers", "nous"]`), пустая — верхний
// уровень: профили провайдеров живут в `[providers.<имя>]`, и без вложенных путей
// профиль нельзя ни завести, ни удалить целиком.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::R;

/// Одна правка документа.
pub enum Edit {
    Set {
        path: Vec<String>,
        key: String,
        value: Value,
    },
    Remove {
        path: Vec<String>,
        key: String,
    },
    /// Убрать таблицу вместе с содержимым: удаление профиля не должно оставлять
    /// пустой `[providers.<имя>]` — иначе имя остаётся занятым.
    RemoveTable {
        path: Vec<String>,
    },
}

/// Путь из строки: `"providers.nous"` → `["providers", "nous"]`. Точки в имени
/// профиля запрещены валидацией (`providers::validate_name`), поэтому разделитель
/// однозначен.
fn split(path: &str) -> Vec<String> {
    path.split('.')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

impl Edit {
    pub fn top(key: impl Into<String>, value: Value) -> Self {
        Self::Set {
            path: Vec::new(),
            key: key.into(),
            value,
        }
    }

    pub fn section(section: impl Into<String>, key: impl Into<String>, value: Value) -> Self {
        Self::Set {
            path: split(&section.into()),
            key: key.into(),
            value,
        }
    }

    /// Убрать ключ: «пусто» и «ключа нет» — разные вещи, и у настроек бывает
    /// нужно именно второе (пустая модель подагента означала бы модель с пустым
    /// именем).
    pub fn remove_top(key: impl Into<String>) -> Self {
        Self::Remove {
            path: Vec::new(),
            key: key.into(),
        }
    }

    pub fn remove_section_key(section: impl Into<String>, key: impl Into<String>) -> Self {
        Self::Remove {
            path: split(&section.into()),
            key: key.into(),
        }
    }

    pub fn remove_table(section: impl Into<String>) -> Self {
        Self::RemoveTable {
            path: split(&section.into()),
        }
    }
}

/// Вписать значение, сохранив оформление прежнего: комментарий над строкой и
/// отступ — часть конфига человека, а вставка нового `Item` их сносила.
fn put(table: &mut Table, key: &str, value: Value) {
    if let Some((_k, Item::Value(old))) = table.get_key_value_mut(key) {
        let mut fresh = value;
        // Комментарий перед ключом живёт в decor ключа, хвостовой — в decor
        // значения; второе переносим, первое не трогаем вовсе.
        *fresh.decor_mut() = old.decor().clone();
        *old = fresh;
        return;
    }
    table.insert(key, Item::Value(value));
}

/// Таблица по пути, если она есть. Для удаления — без создания: `remove` не
/// должен оставлять после себя свежесозданную пустую секцию.
fn table_if<'a>(doc: &'a mut DocumentMut, path: &[String]) -> Option<&'a mut Table> {
    let mut table = doc.as_table_mut();
    for part in path {
        table = table.get_mut(part.as_str())?.as_table_mut()?;
    }
    Some(table)
}

/// Таблица по пути, создаваемая на ходу: `[providers.<имя>]` может ещё не быть.
/// Промежуточные уровни создаются `Table::new()` (в тексте это `[providers.nous]`,
/// как и написал бы человек).
fn table_at<'a>(doc: &'a mut DocumentMut, path: &[String]) -> R<&'a mut Table> {
    if path.is_empty() {
        return Ok(doc.as_table_mut());
    }
    let mut table = doc.as_table_mut();
    for (i, part) in path.iter().enumerate() {
        if !table.contains_key(part) {
            let mut fresh = Table::new();
            // Промежуточный уровень (`[providers]`) — невидимый заголовок: человеку
            // нужен `[providers.nous]`, а пустая `[providers]` над ним осталась бы
            // мусором после удаления последнего профиля.
            if i + 1 < path.len() {
                fresh.set_implicit(true);
            }
            table.insert(part, Item::Table(fresh));
        }
        table = table
            .get_mut(part)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| format!("[{part}] — не таблица, правка отменена"))?;
    }
    Ok(table)
}

/// Удалить таблицу по пути. Нет её — не ошибка: результат тот, который просили.
fn remove_table(doc: &mut DocumentMut, path: &[String]) -> R<()> {
    let Some((last, parents)) = path.split_last() else {
        return Err("удаление всего документа — не правка".into());
    };
    if let Some(parent) = table_if(doc, parents) {
        parent.remove(last);
    }
    Ok(())
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
        match edit {
            // Удаление: ключа нет — уже то, чего хотели, ошибкой не считаем.
            Edit::Remove { path, key } => {
                if let Some(table) = table_if(&mut doc, path) {
                    table.remove(key.as_str());
                }
            }
            Edit::RemoveTable { path } => remove_table(&mut doc, path)?,
            Edit::Set { path, key, value } => {
                put(table_at(&mut doc, path)?, key, value.clone());
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
        std::env::temp_dir().join(format!("snens-edit-{}-{name}.toml", std::process::id()))
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
        write(&path, &[Edit::top("max_turns", int(9).unwrap())]).unwrap();
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
        write(&path, &[Edit::top("subagent_model", string("a\nb\"c"))]).unwrap();
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

    /// Вложенная таблица: профиль провайдера — это `[providers.<имя>]`, и он
    /// должен появляться и исчезать целиком, не трогая соседей.
    #[test]
    fn nested_table_is_created_and_removed_whole() {
        let path = tmp("nested");
        std::fs::write(
            &path,
            "# шапка\nmodel = \"a\"\n\n[providers.old]\nbase_url = \"http://old\"\n",
        )
        .unwrap();
        let brief = Edit::section("providers.new", "base_url", string("http://new"));
        let silent = Edit::section("providers.new", "model", string("m"));
        // Промежуточной таблицы `[providers]` ещё нет — создаётся сама.
        write(&path, &[brief, silent]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# шапка"), "{text}");
        assert!(
            text.contains("[providers.new]") && text.contains("[providers.old]"),
            "{text}"
        );
        // Промежуточная `[providers]` не выводится заголовком: она — только
        // контейнер, и пустой строкой в файле была бы мусором.
        assert!(!text.contains("\n[providers]\n"), "пустой заголовок: {text}");
        // Удаление — таблицей, а не по ключам: пустой профиль занимал бы имя.
        write(&path, &[Edit::remove_table("providers.new")]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("providers.new"), "{text}");
        assert!(text.contains("[providers.old]"), "{text}");
        let cfg = crate::cfg::Config::load(Some(&path.display().to_string())).unwrap();
        assert!(cfg.providers.contains_key("old"));
        // Второй раз — не ошибка: результат тот, который просили.
        write(&path, &[Edit::remove_table("providers.new")]).unwrap();
        std::fs::remove_file(&path).ok();
    }
}
