// Выполнение процесса: единственный механизм, общий для `run` и для будущих
// внешних тулов. Таймаут, кап вывода, exit code.
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::R;

pub struct Out {
    pub exit: i32,
    pub text: String,
    pub bytes: usize,
    pub ms: u64,
    pub timed_out: bool,
}

pub async fn run(cmd: &str, cwd: Option<&str>, timeout_s: u64, cap: usize) -> R<Out> {
    let started = Instant::now();
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    // Процесс убивается вместе с future: таймаут и Ctrl-C работают без доп. механики.
    c.kill_on_drop(true);

    let fut = c.output();
    let res = tokio::time::timeout(Duration::from_secs(timeout_s.max(1)), fut).await;
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
        Err(_) => (124, format!("[timeout after {timeout_s}s]"), true),
    };

    let bytes = raw.len();
    Ok(Out {
        exit,
        text: truncate(&raw, cap),
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