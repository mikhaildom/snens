// Выполнение процесса: единственный механизм и для `run`, и для внешних тулов.
// Таймаут, кап вывода, закрытый stdin, exit code. Сокращение текста и разбор
// вложений — отдельные модули (reduce, attach): здесь только запуск и сборка.
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::{
    attach::{self, MAX_ATTACHMENT_BYTES},
    log::Part,
    reduce, R,
};

pub struct Out {
    pub exit: i32,
    pub text: String,
    pub bytes: usize,
    pub ms: u64,
    pub timed_out: bool,
    /// Вложения, принесённые процессом через `SNENS_ATTACH_DIR`.
    pub parts: Vec<Part>,
}

pub struct Spawn<'a> {
    /// Путь к исполняемому файлу или имя программы: и тул, и оболочка — процесс.
    pub program: String,
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
    /// Каталог вложений вызова: если задан, процесс получает его в
    /// `SNENS_ATTACH_DIR` и может оставить там файлы. Ядро подберёт их после
    /// выхода и вернёт как `Out::parts` — одинаково для тулов и для `run`.
    pub attach_dir: Option<&'a Path>,
    /// Сколько байт вложения допускаем; больше — файл отбрасывается с пометкой.
    pub max_attachment_bytes: usize,
    /// Доп. переменные окружения ребёнка (сессия, ящик, родитель): по ним тул
    /// понимает, в какой сессии он работает и с кем может говорить.
    pub envs: Vec<(String, String)>,
}

impl<'a> Spawn<'a> {
    /// Команда строкой через оболочку из конфига: на unix `sh -c`, на Windows
    /// `cmd /C`. Платформа в коде не зашита, и `sh` ищется в PATH — на Android
    /// он в /system/bin, а не в /bin.
    pub fn sh(shell: &[String], cmd: &str, timeout_s: u64, cap: usize) -> Self {
        let mut args: Vec<String> = shell.iter().skip(1).cloned().collect();
        args.push(cmd.to_string());
        Spawn {
            program: shell.first().cloned().unwrap_or_else(|| "sh".into()),
            args,
            stdin: None,
            cwd: None,
            timeout_s,
            cap,
            path: None,
            reduce: false,
            dump_dir: None,
            attach_dir: None,
            max_attachment_bytes: MAX_ATTACHMENT_BYTES,
            envs: Vec::new(),
        }
    }

    pub fn argv(program: &str, args: Vec<String>) -> Self {
        Spawn {
            program: program.to_string(),
            args,
            stdin: None,
            cwd: None,
            timeout_s: 120,
            cap: 40_000,
            path: None,
            reduce: false,
            dump_dir: None,
            attach_dir: None,
            max_attachment_bytes: MAX_ATTACHMENT_BYTES,
            envs: Vec::new(),
        }
    }
}

pub async fn spawn(sp: Spawn<'_>) -> R<Out> {
    let started = Instant::now();
    let mut c = Command::new(&sp.program);
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
    // Каталог вложений не создаём заранее: большинство вызовов ничего туда не
    // кладут, а пустой каталог на каждый вызов копил бы мусор в <id>.out/attach/.
    // Путь сообщаем переменной; кому файл нужен — сам делает mkdir (view так и
    // делает), а collect_attachments толерантен к отсутствующему каталогу.
    if let Some(d) = sp.attach_dir {
        c.env("SNENS_ATTACH_DIR", d);
    }
    for (k, v) in &sp.envs {
        c.env(k, v);
    }

    // Each tool owns a process group: cancelling only the shell leaves sleep,
    // compilers and other descendants alive. Detached agents use setsid themselves.
    #[cfg(unix)]
    c.process_group(0);
    let mut child = c.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    #[cfg(unix)]
    let _group = ProcessGroup(child.id().ok_or("spawned process has no pid")? as i32);
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

    // Потоки читаем отдельными задачами, а ждём ВЫХОД процесса, не EOF на пайпах.
    // `wait_with_output` ждёт закрытия пайпа, а его держит любой потомок,
    // унаследовавший stdout: `cmd &`, `setsid`, tmux. Вызов висел до таймаута,
    // хотя работа давно ушла в фон (`run-background-holds-stdout`). Побочно это
    // даёт частичный вывод: при обрыве по таймауту уже прочитанное не теряется.
    let out_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let err_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let pumps: Vec<tokio::task::JoinHandle<()>> = [
        pump(child.stdout.take(), Arc::clone(&out_buf)),
        pump(child.stderr.take(), Arc::clone(&err_buf)),
    ]
    .into_iter()
    .flatten()
    .collect();

    let waited = tokio::time::timeout(Duration::from_secs(sp.timeout_s.max(1)), child.wait()).await;
    let (exit, timed_out) = match waited {
        Ok(Ok(status)) => (status.code().unwrap_or(-1), false),
        Ok(Err(e)) => return Err(format!("spawn failed: {e}").into()),
        // Таймаут: снимаем процесс сами (kill_on_drop сработал бы только на
        // брошенном Child, а он теперь живёт дольше future) и забираем группу.
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            (-1, true)
        }
    };
    // Дать читателям донести уже прочитанное, но не ждать EOF от потомков: его
    // может не быть никогда. Нормальный выход — короткая пауза на долив пайпа.
    for mut h in pumps {
        let grace = Duration::from_millis(if timed_out { 50 } else { 250 });
        let _ = tokio::time::timeout(grace, &mut h).await;
        h.abort();
    }
    let drain =
        |b: &Arc<Mutex<Vec<u8>>>| std::mem::take(&mut *b.lock().unwrap_or_else(|e| e.into_inner()));
    let stdout = drain(&out_buf);
    let stderr = drain(&err_buf);
    let ms = started.elapsed().as_millis() as u64;

    let (exit, stderr) = if timed_out {
        // Молчаливый 124 выглядит как падение программы: агент выводит причину из кода
        // возврата и дробит команду вслепую. Называем причину и что с ней делать.
        let mut note = format!(
            "[таймаут {}s: команда прервана, вывод неполный — не падение. Дроби команду, подними timeout_seconds для этого вызова или уведи в фон]\n",
            sp.timeout_s
        );
        note.push_str(&String::from_utf8_lossy(&stderr));
        (124, note.into_bytes())
    } else {
        (exit, stderr)
    };

    // Полный вывод (то, что уйдёт в дамп) — байты как есть, до замены бинарных
    // потоков дескриптором: иначе в дампе был бы текст-заглушка, а не оригинал.
    let mut raw_bytes = stdout.clone();
    if !stderr.is_empty() {
        raw_bytes.extend_from_slice(b"\n[stderr]\n");
        raw_bytes.extend_from_slice(&stderr);
    }
    let bytes = raw_bytes.len();
    let (assembled, binary) = attach::join_streams(&stdout, &stderr);
    let mut text = if binary {
        // Дескриптор уже короткий: reduce/truncate к нему неприменимы.
        assembled
    } else if sp.reduce {
        reduce::reduce(&assembled, sp.cap)
    } else {
        reduce::truncate(&assembled, sp.cap)
    };
    // Обрезанное не теряется: полный вывод ложится в файл, модель получает путь.
    // Бинарный вывод выносим в дамп всегда — в контексте ему места нет, а посмотреть
    // оригинал (hexdump, file) модели может понадобиться.
    if bytes > sp.cap || binary {
        if let Some(dir) = sp.dump_dir {
            if let Ok(p) = dump(dir, &raw_bytes) {
                text.push_str(&format!("\n[полный вывод: {}]", p.display()));
            }
        }
    }

    let parts = match sp.attach_dir {
        Some(d) => attach::collect_attachments(d, sp.max_attachment_bytes, &mut text),
        None => Vec::new(),
    };

    Ok(Out {
        exit,
        text,
        bytes,
        ms,
        timed_out,
        parts,
    })
}

/// Читатель одного потока ребёнка: копит байты в общий буфер, пока пайп не
/// закроется. Отдельная задача, потому что ждать EOF на пайпе нельзя — его
/// держит фоновый потомок (см. комментарий в `spawn`).
fn pump(
    src: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    buf: Arc<Mutex<Vec<u8>>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut src = src?;
    Some(tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match src.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    b.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }))
}

#[cfg(unix)]
struct ProcessGroup(i32);
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Includes timeout and dropped futures. Normal completion also cleans
        // up background children which inherited the tool's process group.
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

/// Полный вывод на диск; имя — от содержимого, поэтому одинаковые дампы не плодятся.
fn dump(dir: &Path, text: &[u8]) -> R<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let path = dir.join(format!("{:016x}.txt", h.finish()));
    if !path.exists() {
        std::fs::write(&path, text)?;
    }
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Обрыв по таймауту объясняет себя: код 124, причина в тексте, не «падение».
    #[tokio::test]
    async fn timeout_names_itself() {
        let mut sp = Spawn::sh(&["sh".into(), "-c".into()], "sleep 5", 1, 10_000);
        sp.reduce = true;
        let out = spawn(sp).await.expect("спавн упал");
        assert_eq!(out.exit, 124, "таймаут должен отдавать 124");
        assert!(out.timed_out, "флаг timed_out не поднят");
        assert!(
            out.text.contains("таймаут 1s") && out.text.contains("timeout_seconds"),
            "в ответе нет причины и подсказки: {}",
            out.text
        );
    }

    /// Фоновый потомок не держит вызов: пайп наследуется, EOF не приходит, а
    /// `run` должен вернуться по выходу шелла, а не по таймауту.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_child_does_not_hold_the_call() {
        let started = Instant::now();
        // Потомок живёт дольше таймаута и держит унаследованный stdout.
        let out = spawn(Spawn::sh(
            &["sh".into(), "-c".into()],
            "sleep 30 & echo started",
            5,
            10_000,
        ))
        .await
        .expect("спавн упал");
        assert!(
            !out.timed_out,
            "вызов дождался EOF от фонового потомка и упал по таймауту"
        );
        assert_eq!(out.exit, 0, "выход шелла подменён: {}", out.text);
        assert!(
            out.text.contains("started"),
            "вывод шелла потерян: {}",
            out.text
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "вызов держался дольше таймаута"
        );
    }

    /// Обрыв по таймауту отдаёт то, что успело прийти: пустой ответ не даёт
    /// понять, началась ли работа вообще.
    #[tokio::test]
    async fn timeout_keeps_partial_output() {
        let out = spawn(Spawn::sh(
            &["sh".into(), "-c".into()],
            "echo first; sleep 10",
            1,
            10_000,
        ))
        .await
        .expect("спавн упал");
        assert!(out.timed_out, "таймаут не отмечен");
        assert_eq!(out.exit, 124);
        assert!(
            out.text.contains("first"),
            "частичный вывод потерян: {}",
            out.text
        );
        assert!(out.text.contains("таймаут 1s"), "нет причины: {}", out.text);
    }

    /// Обрезанное не теряется: полный вывод лежит в файле, путь уходит модели.
    #[tokio::test]
    async fn big_output_is_dumped_to_file() {
        let dir = std::env::temp_dir().join(format!("snens-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut sp = Spawn::sh(&["sh".into(), "-c".into()], "seq 1 20000", 20, 500);
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

    /// Канал вложений: процесс получает SNENS_ATTACH_DIR и кладёт туда картинку,
    /// ядро подбирает её как Part. Незнакомые типы игнорируются, большие —
    /// отбрасываются с пометкой.
    #[tokio::test]
    async fn attach_dir_collects_images() {
        let dir = std::env::temp_dir().join(format!("snens-attach-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Пишем валидный PNG-заголовок и мусорный файл.
        let png = dir.join("bad-source");
        std::fs::write(
            &png,
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01",
        )
        .unwrap();
        let sh = format!(
            "cp '{}' \"$SNENS_ATTACH_DIR/pic.png\"; printf 'x' > \"$SNENS_ATTACH_DIR/note.txt\"",
            png.display()
        );
        let mut sp = Spawn::sh(&["sh".into(), "-c".into()], &sh, 10, 10_000);
        sp.attach_dir = Some(dir.as_path());
        let out = spawn(sp).await.expect("спавн упал");
        assert_eq!(
            out.parts.len(),
            1,
            "должна быть одна картинка: {:?}",
            out.parts
        );
        assert_eq!(out.parts[0].mime, "image/png");
        assert_eq!(out.parts[0].kind, "image");
        assert_eq!((out.parts[0].width, out.parts[0].height), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Каталог вложений ядро не создаёт: путь сообщается, а mkdir — дело тула.
    /// Иначе на каждый вызов (даже без вложений) оставался бы пустой каталог.
    #[tokio::test]
    async fn attach_dir_is_not_created_eagerly() {
        let base = std::env::temp_dir().join(format!("snens-attach-lazy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("attach/call_1");

        // Вызов без вложений: каталога после него быть не должно.
        let mut sp = Spawn::sh(&["sh".into(), "-c".into()], "true", 10, 10_000);
        sp.attach_dir = Some(dir.as_path());
        spawn(sp).await.expect("спавн упал");
        assert!(!dir.exists(), "пустой каталог вложений создан ядром");

        // Тул, которому вложение нужно, создаёт каталог сам — ядро его подберёт.
        let src = base.join("src.png");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            &src,
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01",
        )
        .unwrap();
        let sh = format!(
            "mkdir -p \"$SNENS_ATTACH_DIR\"; cp '{}' \"$SNENS_ATTACH_DIR/pic.png\"",
            src.display()
        );
        let mut sp = Spawn::sh(&["sh".into(), "-c".into()], &sh, 10, 10_000);
        sp.attach_dir = Some(dir.as_path());
        let out = spawn(sp).await.expect("спавн упал");
        assert_eq!(out.parts.len(), 1, "вложение не подобрано: {:?}", out.parts);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Потолок вложения: слишком большой файл не едет, но и не теряется молча.
    #[tokio::test]
    async fn oversized_attachment_is_reported() {
        let dir = std::env::temp_dir().join(format!("snens-attach-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sp = Spawn::sh(
            &["sh".into(), "-c".into()],
            "head -c 5000 /dev/zero > \"$SNENS_ATTACH_DIR/big.png\"",
            10,
            10_000,
        );
        sp.attach_dir = Some(dir.as_path());
        sp.max_attachment_bytes = 1000;
        let out = spawn(sp).await.expect("спавн упал");
        assert!(out.parts.is_empty(), "большой файл не должен уехать");
        assert!(
            out.text.contains("вложение отброшено"),
            "нет пометки: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_tool_kills_shell_descendants() {
        let dir =
            std::env::temp_dir().join(format!("snens-process-group-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let pidfile = dir.join("pid");
        let marker = dir.join("escaped");
        let script = format!(
            "(sleep 2; echo escaped > '{}') & echo $! > '{}'; wait",
            marker.display(),
            pidfile.display()
        );
        let task =
            tokio::spawn(async move { spawn(Spawn::argv("sh", vec!["-c".into(), script])).await });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !pidfile.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(pidfile.exists(), "shell must have started its child");
        task.abort();
        assert!(matches!(task.await, Err(e) if e.is_cancelled()));
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(
            !marker.exists(),
            "descendant survived cancellation and executed a side effect"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
