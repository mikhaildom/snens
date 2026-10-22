// Секреты рядом с конфигом: `~/.config/snens/.env`, формат `KEY=VALUE`, 0600.
//
// Почему отдельный файл, а не поле в TOML: config.toml — документ, который
// человек показывает, копирует на другую машину и иногда держит в git. Ключ
// провайдера, попавший туда, утекает вместе с конфигом. Поэтому в конфиге
// лежит только ИМЯ переменной (`key_env`), а значение — здесь.
//
// Правка идёт как у `cfg_edit`: читаем, меняем нужную строку, пишем временный
// файл рядом, затем `rename` под 0600. Чужие строки (и комментарии) человека
// остаются как были — этот файл он правит и руками.

use std::path::{Path, PathBuf};

use crate::R;

/// Файл секретов рядом с конфигом. Рядом с *фактическим* конфигом, а не с
/// глобальным каталогом: при `--config /tmp/x/config.toml` чужие ключи брать
/// нельзя (иначе инстансы обмениваются секретами).
pub fn path_near(config: &Path) -> PathBuf {
    config.with_file_name(".env")
}

/// Значение переменной из файла. Пусто/нет файла — `None`: «не задано» и
/// «задано пустым» для ключа одно и то же.
pub fn read(var: &str, path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text, var)
}

/// Разбор `KEY=VALUE` по строкам. Кавычки снимаются: файл правят и руками, и
/// `export KEY="…"` в него копируют.
pub fn parse(text: &str, var: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `export KEY=…` — привычная форма для тех, кто держит .env как shell-файл.
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != var {
            continue;
        }
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// Записать значение, сохранив остальные строки и комментарии.
///
/// Пустая строка вместо значения — это удаление: ключ, который «задан пустым»,
/// всё равно не работает, а `key_env` в конфиге должен остаться, чтобы профиль
/// не потерял имя переменной.
pub fn write(path: &Path, var: &str, value: &str) -> R<PathBuf> {
    let value = value.trim();
    if value.contains(['\n', '\r']) {
        return Err("ключ: перевод строки в значении недопустим".into());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display()).into()),
    };
    let mut out: Vec<String> = Vec::new();
    let mut written = false;
    for line in text.lines() {
        let bare = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let matches = bare
            .split_once('=')
            .is_some_and(|(k, _)| k.trim() == var);
        if !matches {
            out.push(line.to_string());
            continue;
        }
        // Дубль имени (файл правили руками) схлопывается в одну строку: иначе
        // значение зависело бы от порядка чтения.
        if written || value.is_empty() {
            continue;
        }
        out.push(format!("{var}={value}"));
        written = true;
    }
    if !written && !value.is_empty() {
        out.push(format!("{var}={value}"));
    }
    let mut body = out.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("env.tmp{}", std::process::id()));
    std::fs::write(&tmp, &body)?;
    // Права до `rename`: файл с секретом не должен ни мгновения лежать читаемым
    // для всех — а `umask` у людей разный.
    restrict(&tmp)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", path.display()).into());
    }
    Ok(path.to_path_buf())
}

/// 0600 на unix. На Windows прав таких нет — файл живёт в профиле пользователя.
#[cfg(unix)]
fn restrict(path: &Path) -> R<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> R<()> {
    Ok(())
}

/// Свободное имя переменной для нового профиля: `SNENS_<ИМЯ>_KEY`, где имя
/// приведено к верхнему регистру с `_` вместо всего не-буквенного. Так человеку
/// ключ легко найти в `.env` глазами, а ядру — собрать без словаря.
pub fn var_for(profile: &str) -> String {
    let mut out = String::from("SNENS_");
    for c in profile.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_uppercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.push_str("_KEY");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("snens-env-{}-{name}.env", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn existing_lines_and_comments_survive_edit() {
        let path = tmp("keep");
        std::fs::write(&path, "# мой ключ\nSNENS_API_KEY=old\nOTHER=keep\n").unwrap();
        write(&path, "SNENS_API_KEY", "new").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# мой ключ"), "{text}");
        assert!(text.contains("OTHER=keep"), "{text}");
        assert!(text.contains("SNENS_API_KEY=new"), "{text}");
        assert!(!text.contains("old"), "{text}");
        assert_eq!(read("SNENS_API_KEY", &path).as_deref(), Some("new"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_value_removes_the_line_but_not_others() {
        let path = tmp("remove");
        std::fs::write(&path, "A=1\nB=2\n").unwrap();
        write(&path, "A", "").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("A=1"), "{text}");
        assert!(text.contains("B=2"), "{text}");
        assert_eq!(read("A", &path), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn quotes_and_export_prefix_are_understood() {
        let text = "export SNENS_OPENROUTER_KEY=\"sk-or-1\"\n";
        assert_eq!(parse(text, "SNENS_OPENROUTER_KEY").as_deref(), Some("sk-or-1"));
        // Комментарий перед ключом — не значение.
        assert_eq!(parse("# SNENS_X=1\n", "SNENS_X"), None);
    }

    #[test]
    fn new_profile_var_is_readable() {
        assert_eq!(var_for("nous"), "SNENS_NOUS_KEY");
        assert_eq!(var_for("open-router"), "SNENS_OPEN_ROUTER_KEY");
        assert_eq!(var_for("мой"), "SNENS_МОЙ_KEY");
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("mode");
        write(&path, "K", "v").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "секрет читается всеми: {mode:o}");
        std::fs::remove_file(&path).ok();
    }

    /// Перевод строки в значении раскавычил бы файл и подсунул лишнюю пару.
    #[test]
    fn newline_is_refused() {
        let path = tmp("nl");
        assert!(write(&path, "K", "a\nB=1").is_err());
    }
}
