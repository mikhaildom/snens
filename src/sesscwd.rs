// Рабочий каталог сессии: `cd` внутри `run` не переносился на read/edit/write —
// это была основная причина отказов «файл не найден». Каталог живёт в файле
// `<sessions>/<id>.out/cwd`, чтобы переживать resume и быть видимым всем тулам.
use std::path::{Path, PathBuf};

use crate::log::Session;

/// Файл с каталогом сессии: рядом с LIVE_CONTEXT.json в `<id>.out/`.
pub fn cwd_file(sess: &Session) -> PathBuf {
    sess.path().with_extension("out").join("cwd")
}

/// Текущий рабочий каталог сессии. Файла нет (или он пуст/битый) — берём
/// каталог процесса: поведение для новых сессий не меняется.
pub fn read(sess: &Session) -> PathBuf {
    if let Ok(text) = std::fs::read_to_string(cwd_file(sess)) {
        let t = text.trim();
        if !t.is_empty() {
            let p = PathBuf::from(t);
            if p.is_dir() {
                return p;
            }
        }
    }
    process_cwd()
}

/// Каталог процесса инстанса — начальное значение для новой сессии.
pub fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Записать каталог сессии. Только существующий каталог, только абсолютный:
/// эвристика ошибается, а хранить несуществующий путь — значит сломать все
/// последующие вызовы. Best-effort: сама сессия из-за этого не падает.
pub fn write(sess: &Session, dir: &Path) {
    if !dir.is_absolute() || !dir.is_dir() {
        return;
    }
    let f = cwd_file(sess);
    if let Some(parent) = f.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&f, format!("{}\n", dir.display()));
}

/// Записать, если каталог существует (относительный путь разрешается от `base`).
pub fn write_if_dir(sess: &Session, dir: &Path, base: &Path) {
    let abs = resolve(dir, base);
    write(sess, &abs);
}

/// Разрешить путь относительно базового каталога, если он относительный.
pub fn resolve(p: &Path, base: &Path) -> PathBuf {
    let p = PathBuf::from(expand_tilde(&p.display().to_string()));
    if p.is_absolute() {
        normalize(&p)
    } else {
        normalize(&base.join(p))
    }
}

/// `~` в начале пути → $HOME: тулы исполняются без оболочки, и `cd ~/x` в run
/// доезжал до эвристики буквальной тильдой, то есть никогда не становился
/// каталогом сессии: отказ был молчаливый (`write` пишет только существующий
/// абсолютный каталог), а следующие `read` искали файл в старом месте.
fn expand_tilde(p: &str) -> String {
    let rest = match p.strip_prefix('~') {
        Some(r) if r.is_empty() || r.starts_with('/') => r,
        _ => return p.to_string(),
    };
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(home) if !home.is_empty() => format!("{home}{rest}"),
        _ => p.to_string(),
    }
}

/// Убрать `.` и `..` текстуально: `canonicalize` требует существования и
/// разворачивает симлинки — здесь нужен предсказуемый путь, а не физический.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// `cd` из команды `run`: последний cd в начале команды или после `;`, `&&`,
/// `||`, перевода строки — вне кавычек и вне `$( )`. Возвращает аргумент
/// последнего такого cd. Консервативно: сомнительное (`cd` без аргумента,
/// `cd -`, переменные, подстановки) не отдаём вовсе.
pub fn last_cd(command: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for seg in command_segments(command) {
        if let Some(target) = cd_target(&seg) {
            found = Some(target);
        }
    }
    found
}

/// Разбить команду на «начала команд»: куски после `;`, `&&`, `||`, `|`, `\n`.
/// Кавычки и `$( )` уважаются — внутри них разделители не считаются.
fn command_segments(command: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        if quote.is_none() && c == '\\' {
            // Экранированный символ: переносим и его, разделителем он не станет.
            if let Some(n) = chars.next() {
                cur.push(c);
                cur.push(n);
            }
            continue;
        }
        if quote.is_none() && c == '$' && chars.peek() == Some(&'(') {
            depth += 1;
            cur.push(c);
            continue;
        }
        if quote.is_none() && depth > 0 {
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            }
            cur.push(c);
            continue;
        }
        if quote == Some(c) {
            quote = None;
            cur.push(c);
            continue;
        }
        if quote.is_none() && (c == '"' || c == '\'') {
            quote = Some(c);
            cur.push(c);
            continue;
        }
        if quote.is_none() && (c == ';' || c == '|' || c == '\n') {
            segs.push(std::mem::take(&mut cur));
            continue;
        }
        if quote.is_none() && c == '&' {
            // `&&` и одиночный `&` — граница команды.
            segs.push(std::mem::take(&mut cur));
            continue;
        }
        cur.push(c);
    }
    segs.push(cur);
    segs
}

/// Аргумент `cd` из начала куска команды. Только литеральный путь: `$VAR`,
/// `~`-с-логином, `cd -` и пустой `cd` — не наш случай, не угадываем.
fn cd_target(seg: &str) -> Option<String> {
    let mut parts = shlex_like(seg);
    while parts.first().is_some_and(|p: &String| is_env_assign(p)) {
        parts.remove(0);
    }
    if parts.first().map(String::as_str) != Some("cd") {
        return None;
    }
    let arg = parts.get(1)?;
    if arg.is_empty() || arg == "-" || arg.contains('$') || arg.contains('`') {
        return None;
    }
    Some(arg.clone())
}

/// `KEY=value` перед командой (`FOO=1 cd /tmp`) — не сама команда.
fn is_env_assign(s: &str) -> bool {
    match s.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && k.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !k.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Грубый разбор куска на слова с учётом кавычек: `cd "my dir"` → `cd`, `my dir`.
/// Полноценный shell-парсер здесь не нужен — только первая пара слов.
fn shlex_like(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        if quote == Some(c) {
            quote = None;
            has = true;
            continue;
        }
        if quote.is_none() && (c == '"' || c == '\'') {
            quote = Some(c);
            has = true;
            continue;
        }
        if quote.is_none() && c.is_whitespace() {
            if has {
                out.push(std::mem::take(&mut cur));
                has = false;
            }
            continue;
        }
        cur.push(c);
        has = true;
    }
    if has {
        out.push(cur);
    }
    out
}

/// Обновить каталог сессии после `run`: явный `cwd` вызова, иначе эвристика по
/// `cd` в команде. Молча ничего не делает, если каталог не существует.
pub fn after_run(sess: &Session, command: &str, explicit_cwd: Option<&str>) {
    if let Some(c) = explicit_cwd {
        write_if_dir(sess, Path::new(c), &process_cwd());
        return;
    }
    let Some(target) = last_cd(command) else {
        return;
    };
    let base = read(sess);
    write_if_dir(sess, Path::new(&target), &base);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Rec, Session};

    fn tmp(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "snens-cwd-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn session(tag: &str) -> (PathBuf, Session) {
        let dir = tmp(tag);
        let sess = Session::open(&dir, "t").unwrap();
        (dir, sess)
    }

    #[test]
    fn cd_simple_and_chained() {
        assert_eq!(last_cd("cd /tmp").as_deref(), Some("/tmp"));
        assert_eq!(last_cd("cd /a; cd /b").as_deref(), Some("/b"));
        assert_eq!(last_cd("ls; cd /tmp && make").as_deref(), Some("/tmp"));
        assert_eq!(last_cd("cd /a\ncd /b").as_deref(), Some("/b"));
        assert_eq!(last_cd("cd /a || cd /b").as_deref(), Some("/b"));
    }

    /// `cd ~/x` — путь, а не тильда: relative-resolve пишет каталог сессии
    /// только для существующего абсолютного, и с буквальной `~` он молчал, а
    /// следующие read искали файл в старом месте (`read-path-cwd-mismatch`).
    #[test]
    fn tilde_resolves_from_home() {
        let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
        let Ok(home) = home else {
            return; // платформа без HOME — проверять нечего
        };
        let got = resolve(Path::new("~/projects"), Path::new("/base"));
        assert_eq!(got, PathBuf::from(format!("{home}/projects")));
        // `~user` и тильда в середине пути не трогаются.
        assert_eq!(
            resolve(Path::new("~user/x"), Path::new("/base")),
            PathBuf::from("/base/~user/x")
        );
    }

    #[test]
    fn cd_not_at_command_start_is_ignored() {
        // `cd` в середине чужой команды (аргумент) — не смена каталога.
        assert_eq!(last_cd("echo cd /tmp"), None);
        assert_eq!(last_cd("sh -c 'cd /tmp'"), None);
        assert_eq!(last_cd("tar -C /x cd"), None);
    }

    #[test]
    fn cd_inside_quotes_and_subshell_ignored() {
        assert_eq!(last_cd("echo \"cd /tmp\""), None);
        assert_eq!(last_cd("echo 'cd /tmp'"), None);
        assert_eq!(last_cd("x=$(cd /tmp; pwd)"), None);
        assert_eq!(last_cd("x=$(ls) && echo cd /x"), None);
    }

    #[test]
    fn cd_suspicious_forms_ignored() {
        assert_eq!(last_cd("cd"), None);
        assert_eq!(last_cd("cd -"), None);
        assert_eq!(last_cd("cd $HOME"), None);
    }

    #[test]
    fn cd_with_quoted_path_and_env_prefix() {
        assert_eq!(last_cd("cd \"my dir\"").as_deref(), Some("my dir"));
        assert_eq!(last_cd("FOO=1 cd /tmp").as_deref(), Some("/tmp"));
    }

    #[test]
    fn cwd_file_roundtrip_and_fallback() {
        let (dir, sess) = session("file");
        // Файла нет — каталог процесса.
        assert_eq!(read(&sess), process_cwd());
        let target = dir.join("work");
        std::fs::create_dir_all(&target).unwrap();
        write(&sess, &target);
        assert_eq!(read(&sess), target);
        // Несуществующий каталог не записывается: файл остаётся прежним.
        write(&sess, &dir.join("нет-такого"));
        assert_eq!(read(&sess), target);
        // Относительный путь не пишем вообще.
        write(&sess, Path::new("relative"));
        assert_eq!(read(&sess), target);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn after_run_updates_only_existing_dirs() {
        let (dir, sess) = session("after");
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        write(&sess, &dir);
        // Относительный cd разрешается от текущего каталога сессии.
        after_run(&sess, "cd a && ls", None);
        assert_eq!(read(&sess), a);
        // Второй cd разрешается от нового каталога сессии (a), как в shell.
        after_run(&sess, "cd ../b", None);
        assert_eq!(read(&sess), b);
        // Несуществующий — каталог сессии не сдвигается.
        after_run(&sess, "cd нет-такого", None);
        assert_eq!(read(&sess), b);
        // Явный cwd вызова сильнее эвристики.
        after_run(&sess, "cd нет-такого", Some(a.to_str().unwrap()));
        assert_eq!(read(&sess), a);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn session_cwd_is_recorded_in_log_dir() {
        let (dir, sess) = session("out");
        let sess = {
            // `Rec` не нужен, но открытие сессии должно дать живой лог.
            sess.append(Rec::System {
                ts: 0,
                text: "s".into(),
            })
            .unwrap();
            sess
        };
        let target = dir.join("w");
        std::fs::create_dir_all(&target).unwrap();
        write(&sess, &target);
        assert!(cwd_file(&sess).is_file());
        assert!(cwd_file(&sess).starts_with(dir.join("t.out")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
