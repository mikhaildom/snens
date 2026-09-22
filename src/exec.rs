// Выполнение процесса: единственный механизм и для `run`, и для внешних тулов.
// Таймаут, кап вывода, закрытый stdin, exit code.
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::R;

/// Строку такой длины модель всё равно не читает, а контекст она съедает.
const MAX_LINE: usize = 400;

pub struct Out {
    pub exit: i32,
    pub text: String,
    pub bytes: usize,
    pub ms: u64,
    pub timed_out: bool,
}

pub struct Spawn<'a> {
    /// true — program это строка для `sh -c`; false — путь к исполняемому файлу.
    pub shell: bool,
    pub program: &'a str,
    pub args: Vec<String>,
    /// Что подать на stdin. None — stdin закрывается сразу (иначе тул, читающий
    /// stdin, будет ждать до таймаута).
    pub stdin: Option<String>,
    pub cwd: Option<&'a str>,
    pub timeout_s: u64,
    pub cap: usize,
    /// PATH дочернего процесса: тулы из реестра видны и внутри `run`.
    pub path: Option<&'a str>,
    /// Превентивное сокращение вывода (ANSI, длинные строки, повторы).
    /// Для shell-команд — да, для тулов с точным контрактом (read/edit) — нет:
    /// там схлопывание повторов исказило бы содержимое файла.
    pub reduce: bool,
    /// Куда положить полный вывод, если он не влез в cap. Модель получает путь
    /// и может дочитать или grep'нуть вместо повторного запуска команды.
    pub dump_dir: Option<&'a Path>,
}

impl<'a> Spawn<'a> {
    pub fn sh(cmd: &'a str, timeout_s: u64, cap: usize) -> Self {
        Spawn {
            shell: true,
            program: cmd,
            args: Vec::new(),
            stdin: None,
            cwd: None,
            timeout_s,
            cap,
            path: None,
            reduce: false,
            dump_dir: None,
        }
    }

    pub fn argv(program: &'a str, args: Vec<String>) -> Self {
        Spawn {
            shell: false,
            program,
            args,
            stdin: None,
            cwd: None,
            timeout_s: 120,
            cap: 40_000,
            path: None,
            reduce: false,
            dump_dir: None,
        }
    }
}

pub async fn spawn(sp: Spawn<'_>) -> R<Out> {
    let started = Instant::now();
    let mut c = if sp.shell {
        let mut c = Command::new("sh");
        c.arg("-c").arg(sp.program);
        c
    } else {
        Command::new(sp.program)
    };
    c.args(&sp.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Процесс убивается вместе с future: таймаут и Ctrl-C без доп. механики.
        .kill_on_drop(true);
    if let Some(d) = sp.cwd {
        c.current_dir(d);
    }
    if let Some(p) = sp.path {
        c.env("PATH", p);
    }

    let mut child = c.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    match sp.stdin {
        // Пишем в отдельной задаче: иначе дедлок, если ребёнок ждёт конца stdin,
        // а мы ждём его вывода.
        Some(input) => {
            if let Some(mut si) = child.stdin.take() {
                tokio::spawn(async move {
                    let _ = si.write_all(input.as_bytes()).await;
                    let _ = si.shutdown().await;
                });
            }
        }
        None => drop(child.stdin.take()),
    }

    let res = tokio::time::timeout(
        Duration::from_secs(sp.timeout_s.max(1)),
        child.wait_with_output(),
    )
    .await;
    let ms = started.elapsed().as_millis() as u64;

    let (exit, raw, timed_out) = match res {
        Ok(Ok(out)) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                s.push_str("\n[stderr]\n");
                s.push_str(&err);
            }
            (out.status.code().unwrap_or(-1), s, false)
        }
        Ok(Err(e)) => return Err(format!("spawn failed: {e}").into()),
        Err(_) => (124, format!("[timeout after {}s]", sp.timeout_s), true),
    };

    let bytes = raw.len();
    let mut text = if sp.reduce {
        reduce(&raw, sp.cap)
    } else {
        truncate(&raw, sp.cap)
    };
    // Обрезанное не теряется: полный вывод ложится в файл, модель получает путь.
    if bytes > sp.cap {
        if let Some(dir) = sp.dump_dir {
            if let Ok(p) = dump(dir, &raw) {
                text.push_str(&format!("\n[полный вывод: {}]", p.display()));
            }
        }
    }

    Ok(Out {
        exit,
        text,
        bytes,
        ms,
        timed_out,
    })
}

/// Полный вывод на диск; имя — от содержимого, поэтому одинаковые дампы не плодятся.
fn dump(dir: &Path, text: &str) -> R<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let path = dir.join(format!("{:016x}.txt", h.finish()));
    if !path.exists() {
        std::fs::write(&path, text)?;
    }
    Ok(path)
}

/// Превентивное сокращение вывода shell-команд до попадания в контекст:
/// ANSI-мусор, нечеловечески длинные строки, повторы, общий размер.
pub fn reduce(s: &str, cap: usize) -> String {
    let stripped = strip_ansi(s);
    let mut lines: Vec<String> = Vec::new();
    let mut prev: Option<String> = None;
    let mut run = 0usize;
    for raw_line in stripped.lines() {
        let line = cap_line(raw_line.trim_end());
        if Some(&line) == prev.as_ref() {
            run += 1;
            continue;
        }
        if run > 0 {
            lines.push(format!("  [×{}]", run + 1));
            run = 0;
        }
        prev = Some(line.clone());
        lines.push(line);
    }
    if run > 0 {
        lines.push(format!("  [×{}]", run + 1));
    }
    truncate(&lines.join("\n"), cap)
}

fn cap_line(l: &str) -> String {
    let n = l.chars().count();
    if n <= MAX_LINE {
        return l.to_string();
    }
    let head: String = l.chars().take(MAX_LINE).collect();
    format!("{head}… (+{} симв.)", n - MAX_LINE)
}

/// Управляющие последовательности: цвет, прогресс-бары, курсор — токены без смысла.
fn strip_ansi(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.peek() {
            // CSI: ESC [ ... финальный байт в диапазоне @..~
            Some('[') => {
                it.next();
                while let Some(&n) = it.peek() {
                    it.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL или ESC \
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Усечение с явной пометкой: модель должна знать, что видит часть.
pub fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let head: String = s.chars().take(cap * 3 / 4).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(cap / 4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!(
        "{head}\n[... обрезано {} символов из {} ...]\n{tail}",
        s.len() - cap,
        s.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_and_repeats_are_squeezed() {
        let s = "\u{1b}[32mok\u{1b}[0m\nшум\nшум\nшум\nконец\n";
        let r = reduce(s, 10_000);
        assert!(r.contains("ok"), "текст потерян: {r}");
        assert!(!r.contains('\u{1b}'), "ANSI остался: {r:?}");
        assert!(r.contains("[×3]"), "повторы не схлопнуты: {r}");
        assert!(r.contains("конец"));
        let long = "x".repeat(1000);
        let r2 = reduce(&long, 10_000);
        assert!(r2.contains("симв."), "длинная строка без пометки: {}", r2.len());
    }

    /// Обрезанное не теряется: полный вывод лежит в файле, путь уходит модели.
    #[tokio::test]
    async fn big_output_is_dumped_to_file() {
        let dir = std::env::temp_dir().join(format!("snens-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut sp = Spawn::sh("seq 1 20000", 20, 500);
        sp.reduce = true;
        sp.dump_dir = Some(dir.as_path());
        let out = spawn(sp).await.expect("спавн упал");

        assert!(out.bytes > 500, "вывод должен быть больше капа");
        assert!(
            out.text.contains("полный вывод: "),
            "нет ссылки на дамп: {}",
            &out.text[..200.min(out.text.len())]
        );
        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("каталог дампов не создан")
            .flatten()
            .collect();
        assert_eq!(files.len(), 1, "ожидался один дамп, их {}", files.len());
        let full = std::fs::read_to_string(files[0].path()).expect("дамп не читается");
        assert_eq!(full.lines().count(), 20000, "дамп обрезан");
        std::fs::remove_dir_all(&dir).ok();
    }
}
