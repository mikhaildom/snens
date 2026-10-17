// Askpass: пароль для sudo — через свою программу, а не через raw /dev/tty.
//
// Почему так. sudo без `-A` открывает `/dev/tty` и читает пароль сам, вступая в
// драку с нашим читателем ввода за один терминал = пароль ввести нельзя. С `-A`
// sudo зовёт программу из `SUDO_ASKPASS` и читает пароль из её stdout — терминал
// ему не нужен вовсе.
//
// Две роли здесь, третья — в `approval.rs`:
//  - **сервер** (в процессе TUI): unix-сокет; на запрос спрашивает пароль у
//    человека своим читателем БЕЗ ЭХА и отдаёт ответ. Пароль живёт только в
//    памяти, ни в лог, ни на диск не попадает.
//  - **клиент** (`snens askpass <prompt>`): то, что зовёт sudo; берёт у сервера
//    пароль и печатает его на stdout.
//  - **шим** (`sudo` в PATH детей) переехал в `approval.rs`: в живой TUI он
//    транслирует в `sudo -A` (пароль у askpass), а в headless-сессии — в очередь
//    одобрений владельца кнопкой.
//
// Сервера нет (headless `snens run`, выключенный TUI) — клиент не находит сокет,
// молча выходит с кодом ≠ 0, и sudo мгновенно падает «a password is required»,
// а не висит до таймаута.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::input::Term;
use crate::R;

/// Сколько клиент ждёт пароль от человека. Меньше таймаута `run` (120–180 с),
/// чтобы отказ был от sudo, а не «команда не ответила».
const ASKPASS_WAIT_SECONDS: u64 = 45;

/// Файл сокета рядом с сессиями: один на инстанс.
pub fn sock_path(sessions: &Path) -> PathBuf {
    sessions
        .parent()
        .map(|p| p.join("askpass.sock"))
        .unwrap_or_else(|| PathBuf::from("snens-askpass.sock"))
}

/// Поднять сервер в текущем процессе (TUI). Сокет — только для этого пользователя:
/// пароль не должен быть никому доступен через файловую систему.
#[cfg(not(unix))]
pub fn spawn_server(_sock: PathBuf, _term: Arc<Term>) {}

#[cfg(unix)]
pub fn spawn_server(sock: PathBuf, term: Arc<Term>) {
    tokio::spawn(async move {
        let _ = std::fs::remove_file(&sock); // старый сокет от прошлого запуска
        let Ok(listener) = tokio::net::UnixListener::bind(&sock) else {
            return;
        };
        let _ =
            std::fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                break;
            };
            let term = Arc::clone(&term);
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let mut reader = BufReader::new(&mut conn);
                let mut prompt = String::new();
                if reader.read_line(&mut prompt).await.is_err() {
                    return;
                }
                // Пароль читает поток ввода (он один владеет stdin): просим и ждём.
                let rx = term.request_secret(prompt.trim_end().to_string());
                let pw = tokio::task::spawn_blocking(move || rx.recv().unwrap_or_default())
                    .await
                    .unwrap_or_default();
                let _ = conn.write_all(pw.as_bytes()).await;
                let _ = conn.write_all(b"\n").await;
                let _ = conn.flush().await;
            });
        }
    });
}

/// Клиент (то, что зовёт sudo): взять пароль у сервера и напечатать на stdout.
/// Нет сокета/ответа — молча выходим с ошибкой: sudo поймёт, что пароля нет.
#[cfg(not(unix))]
pub async fn client(_prompt: String) -> R<()> {
    Err("askpass: не поддержан на этой платформе".into())
}

#[cfg(unix)]
pub async fn client(prompt: String) -> R<()> {
    check_askpass_enabled(std::env::var_os("SNENS_ASKPASS_DISABLED").is_some())?;
    let path = client_sock_path()?;
    let Ok(mut conn) = tokio::net::UnixStream::connect(&path).await else {
        return Err("askpass: сервер пароля не запущен".into());
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let _ = conn.write_all(prompt.as_bytes()).await;
    let _ = conn.write_all(b"\n").await;
    let mut reader = BufReader::new(&mut conn);
    let mut pw = String::new();
    // Человек мог отойти: без своего таймаута `read_line` висит до таймаута `run`,
    // и агент видит просто «команда не ответила», а не «нужен пароль, никто не ввёл».
    // Отказываем сами — sudo скажет «a password is required» сразу.
    let wait = std::time::Duration::from_secs(ASKPASS_WAIT_SECONDS);
    match tokio::time::timeout(wait, reader.read_line(&mut pw)).await {
        Ok(r) => {
            r?;
        }
        Err(_) => {
            return Err(format!(
                "askpass: пароль не введён за {ASKPASS_WAIT_SECONDS} с — отказываю, sudo скажет «a password is required»"
            )
            .into())
        }
    }
    let pw = pw.trim_end_matches(['\n', '\r']);
    if pw.is_empty() {
        return Err("askpass: пустой пароль".into());
    }
    print!("{pw}");
    std::io::Write::flush(&mut std::io::stdout())?;
    Ok(())
}

fn check_askpass_enabled(disabled: bool) -> R<()> {
    if disabled {
        Err("askpass: disabled in headless surface server; run sudo from snens tui-local or an interactive terminal".into())
    } else {
        Ok(())
    }
}


/// Путь сокета для клиента: переменная окружения (её ставит харнесс детям) или
/// путь по умолчанию из конфига.
#[cfg(unix)]
fn client_sock_path() -> R<PathBuf> {
    if let Ok(p) = std::env::var("SNENS_ASKPASS_SOCK") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let cfg = crate::cfg::Config::load(None)?;
    Ok(sock_path(&cfg.sessions_path()))
}

/// Переменные окружения для процессов-инструментов агента: путь к сокету и
/// SUDO_ASKPASS, указывающий на шим `snens-askpass` (симлинк на сам `snens`:
/// при вызове sudo его `argv[0]` = это имя, и мы узнаём роль клиента).
/// Ставится только детям `run`/тулов.
pub fn child_env(sock: &Path, shim_dir: &Path) -> Vec<(String, String)> {
    let askpass = shim_dir.join(format!("snens-askpass{}", std::env::consts::EXE_SUFFIX));
    vec![
        ("SNENS_ASKPASS_SOCK".into(), sock.display().to_string()),
        ("SUDO_ASKPASS".into(), askpass.display().to_string()),
    ]
}

/// Создать каталог шимов: `sudo` (трансляция в `sudo -A`) и `snens-askpass`
/// (клиент, который зовёт sudo) — оба симлинки на сам `snens`. Каталог кладём
/// перед PATH детей, поэтому системные `sudo` и обычный интерактивный ввод
/// пользователя не трогаются.
#[cfg(not(unix))]
pub fn ensure_shims(_cfg: &crate::cfg::Config) -> R<PathBuf> {
    Err("shim: не поддержан на этой платформе".into())
}

#[cfg(unix)]
pub fn ensure_shims(cfg: &crate::cfg::Config) -> R<PathBuf> {
    let dir = cfg.data_path().join("shim");
    std::fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe()?;
    let suffix = std::env::consts::EXE_SUFFIX;
    for name in ["sudo", "snens-askpass"] {
        let link = dir.join(format!("{name}{suffix}"));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&exe, &link)?;
    }
    Ok(dir)
}

/// Имя, под которым нас позвали (basename argv[0]) — по нему узнаём роль шима.
pub fn invoked_as() -> String {
    std::env::args()
        .next()
        .map(|a| {
            Path::new(&a)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(a)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod headless_tests {
    use super::*;

    #[test]
    fn disabled_guard_fails_before_socket_access_with_actionable_notice() {
        // This is the same guard called before client_sock_path()/connect, so the
        // disabled case cannot contact any local askpass socket.
        assert!(check_askpass_enabled(true)
            .unwrap_err()
            .to_string()
            .contains("headless surface server"));
        assert!(check_askpass_enabled(false).is_ok());
    }
}
