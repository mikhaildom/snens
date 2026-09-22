// Выполнение процесса: единственный механизм и для `run`, и для внешних тулов.
// Таймаут, кап вывода, закрытый stdin, exit code.
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::R;

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
    Ok(Out {
        exit,
        text: truncate(&raw, sp.cap),
        bytes,
        ms,
        timed_out,
    })
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
