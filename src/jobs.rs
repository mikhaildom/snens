// Фоновые задания: длинная команда уезжает отдельным процессом, а её результат
// возвращается в сессию сообщением, а не через ожидание в туле.
//
// Зачем: `run` держит ход, пока команда не закончится, и человек со своим словом
// стоит в очереди до таймаута. Здесь — ровно то же исполнение (exec, тот же
// таймаут и кап вывода), но ждёт его отдельный процесс `snens job-run`, а ход
// заканчивается сразу. Готовность забирает будильник сессии (surface_wake).
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{cfg::Config, R};

/// Сколько байт хвоста вывода уезжает в пробуждение. Полный вывод — в файле,
/// модель дочитает его тем же read/grep, если хвоста мало.
const TAIL: usize = 8_000;

#[derive(Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub timeout_s: u64,
    pub log: String,
    pub pid: u32,
    pub started: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Done {
    pub exit: i32,
    pub ms: u64,
    pub timed_out: bool,
    pub bytes: usize,
}

/// Каталог заданий сессии: рядом с дампами вывода (`<id>.out/`).
pub fn dir(sess_path: &Path) -> PathBuf {
    sess_path.with_extension("out").join("jobs")
}

fn job_path(sess_path: &Path, id: &str) -> PathBuf {
    dir(sess_path).join(format!("{id}.json"))
}

fn done_path(sess_path: &Path, id: &str) -> PathBuf {
    dir(sess_path).join(format!("{id}.done"))
}

/// Поставить команду в фон. Имя задания — метка времени и pid: человеку оно
/// нужно, чтобы сопоставить уведомление с командой.
pub fn start(
    cfg: &Config,
    sess_path: &Path,
    command: &str,
    cwd: &str,
    timeout_s: u64,
) -> R<String> {
    let id = format!("{}-{}", crate::log::now(), std::process::id() % 100000);
    let dir = dir(sess_path);
    std::fs::create_dir_all(&dir)?;
    let log = dir.join(format!("{id}.log")).display().to_string();
    let exe = crate::self_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("job-run")
        .arg("--job")
        .arg(job_path(sess_path, &id))
        .arg("--config")
        .arg(crate::cfg::config_path())
        .env("SNENS_CONFIG", crate::cfg::config_path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Отсоединяемся: задание переживает ход и не зависит от терминала. Заодно
    // в окружение едет то, чем живёт тул внутри задания (сессия, ящик, PATH).
    detach(&mut cmd);
    let child = cmd.spawn()?;
    let job = Job {
        id: id.clone(),
        command: command.to_string(),
        cwd: cwd.to_string(),
        timeout_s,
        log,
        pid: child.id(),
        started: crate::log::now(),
    };
    // Запись атомарно: `job-run` откроет её сразу после спавна.
    let bytes = serde_json::to_vec(&job)?;
    let tmp = job_path(sess_path, &id).with_extension("tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, job_path(sess_path, &id))?;
    let _ = cfg;
    Ok(id)
}

#[cfg(unix)]
fn detach(cmd: &mut std::process::Command) {
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach(_cmd: &mut std::process::Command) {}

pub fn done(sess_path: &Path, id: &str) -> Option<Done> {
    serde_json::from_slice(&std::fs::read(done_path(sess_path, id)).ok()?).ok()
}

/// Исполнение задания: это и есть отдельный процесс `snens job-run`.
pub async fn run_job(job: &Job, shell: &[String], cap: usize) -> R<Done> {
    let mut sp = crate::exec::Spawn::sh(shell, &job.command, job.timeout_s, cap);
    sp.cwd = Some(&job.cwd);
    sp.dump_dir = None;
    let out = crate::exec::spawn(sp).await?;
    let done = Done {
        exit: out.exit,
        ms: out.ms,
        timed_out: out.timed_out,
        bytes: out.bytes,
    };
    // Полный вывод — в лог; хвост модель получит в уведомлении.
    let mut text = out.text;
    if text.len() > TAIL {
        let cut = floor_boundary(&text, text.len() - TAIL);
        text = format!("[…начало в {}]\n{}", job.log, &text[cut..]);
    }
    std::fs::write(&job.log, &text)?;
    Ok(done)
}

/// Байтовое смещение, не разрывающее UTF-8: срез в середине символа паникует.
fn floor_boundary(s: &str, mut at: usize) -> usize {
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Тело пробуждения по завершённому заданию: что запускали, чем кончилось, хвост.
fn report(job: &Job, done: &Done) -> String {
    let status = if done.timed_out {
        format!("истёк таймаут {}s", job.timeout_s)
    } else {
        format!("exit {}", done.exit)
    };
    let tail = std::fs::read_to_string(&job.log).unwrap_or_default();
    let tail = tail.trim();
    let mut text = format!(
        "[задание {} завершилось: {status}, {:.1} с]\n$ {}",
        job.id,
        done.ms as f64 / 1000.0,
        job.command
    );
    if !tail.is_empty() {
        text.push_str("\n\n");
        text.push_str(tail);
    }
    text
}

/// Забрать готовые задания: каждое отдаётся один раз, запись переносится в
/// `collected/`, чтобы повторное пробуждение не докладывало то же дважды.
pub fn collect_finished(sess_path: &Path) -> Vec<String> {
    let dir = dir(sess_path);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|x| x.to_str()).map(str::to_string) else {
            continue;
        };
        let Ok(job) = serde_json::from_slice::<Job>(&std::fs::read(&path).unwrap_or_default()) else {
            continue;
        };
        match done(sess_path, &id) {
            Some(done) => {
                out.push(report(&job, &done));
            }
            // Процесса нет, а `done` нет — задание сняли наглухо (OOM, SIGKILL).
            // Молчать об этом нельзя: ход ждёт результата, которого не будет.
            // Старое задание в расчёт не идёт: его pid давно переиспользован, и
            // докладывать о нём как о свежем — врать.
            None if stale(&job) => {
                let collected = dir.join("collected");
                let _ = std::fs::create_dir_all(&collected);
                let _ = std::fs::rename(&path, collected.join(path.file_name().unwrap_or_default()));
                continue;
            }
            None if !crate::subagent::alive(job.pid) => {
                let tail = std::fs::read_to_string(&job.log).unwrap_or_default();
                let tail = tail.trim();
                let mut text = format!(
                    "[задание {} снято: процесса {} больше нет, а результат не записан]\n$ {}",
                    job.id, job.pid, job.command
                );
                if !tail.is_empty() {
                    text.push_str("\n\n");
                    text.push_str(tail);
                }
                out.push(text);
            }
            None => continue,
        }
        // Отданное уезжает из каталога ожидания; id остаётся виден в `collected/`.
        let collected = dir.join("collected");
        let _ = std::fs::create_dir_all(&collected);
        let _ = std::fs::rename(&path, collected.join(path.file_name().unwrap_or_default()));
        let _ = std::fs::rename(
            done_path(sess_path, &id),
            collected.join(format!("{id}.done")),
        );
    }
    out.sort();
    out
}

/// Сколько задание считается новым после запуска. Всё, что старше, — мусор от
/// прежних сессий: pid переиспользован, а `done` мог не записаться. Без порога
/// будильник поднимал сессии месячной давности (они же и устроили шторм).
const STALE_AFTER: u64 = 24 * 3600;

fn stale(job: &Job) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(job.started) > STALE_AFTER
}

/// Есть ли что забирать — дешёвая проверка без изъятия. Кроме готового задания
/// это ещё и снятое (pid мёртв, а `done` нет): иначе о таком сессия узнала бы
/// только когда её разбудят по другому поводу.
pub fn has_finished(sess_path: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir(sess_path)) else {
        return false;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|x| x.to_str()) else {
            continue;
        };
        if done(sess_path, id).is_some() {
            return true;
        }
        let job = serde_json::from_slice::<Job>(&std::fs::read(&path).unwrap_or_default());
        if job.is_ok_and(|job| !stale(&job) && !crate::subagent::alive(job.pid)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_never_splits_a_character() {
        let s = "привет";
        for at in 0..=s.len() {
            let cut = floor_boundary(s, at);
            assert!(s.is_char_boundary(cut));
        }
        assert_eq!(floor_boundary("привет", 1), 2);
    }

    /// Завершённое задание забирается ровно один раз: иначе пробуждение
    /// докладывало бы один и тот же результат на каждом тике.
    #[test]
    fn finished_job_is_collected_once() {
        let root = std::env::temp_dir().join(format!("snens-jobs-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let sess_path = root.join("1-2.jsonl");
        let jobs = dir(&sess_path);
        std::fs::create_dir_all(&jobs).unwrap();
        let log = jobs.join("7-1.log");
        std::fs::write(&log, "готово\n").unwrap();
        // Пид заведомо мёртвого процесса: без `done` задание считается снятым,
        // и будильник обязан это увидеть (иначе о нём узнают по другому поводу).
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = dead.id();
        let _ = dead.wait();
        let job = Job {
            id: "7-1".into(),
            command: "make all".into(),
            cwd: root.display().to_string(),
            timeout_s: 60,
            log: log.display().to_string(),
            pid: dead_pid,
            // Текущее время, а не 0: задание с `started: 0` считается старьём,
            // и о снятом задании тогда молчат — проверяется не это.
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        std::fs::write(
            jobs.join("7-1.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        assert!(has_finished(&sess_path));
        std::fs::write(
            jobs.join("7-1.done"),
            serde_json::to_vec(&Done {
                exit: 0,
                ms: 42,
                timed_out: false,
                bytes: 8,
            })
            .unwrap(),
        )
        .unwrap();
        let first = collect_finished(&sess_path);
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].contains("exit 0"), "{}", first[0]);
        assert!(first[0].contains("make all"), "{}", first[0]);
        assert!(first[0].contains("готово"), "{}", first[0]);
        assert!(!has_finished(&sess_path));
        assert!(collect_finished(&sess_path).is_empty(), "выдано дважды");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn report_carries_status_command_and_tail() {
        let dir = std::env::temp_dir().join(format!("snens-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("1.log");
        std::fs::write(&log, "готово: 3 файла\n").unwrap();
        let job = Job {
            id: "1".into(),
            command: "cargo test".into(),
            cwd: "/tmp".into(),
            timeout_s: 600,
            log: log.display().to_string(),
            pid: 0,
            started: 0,
        };
        let text = report(
            &job,
            &Done {
                exit: 0,
                ms: 1500,
                timed_out: false,
                bytes: 16,
            },
        );
        assert!(text.contains("exit 0"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        assert!(text.contains("готово: 3 файла"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
