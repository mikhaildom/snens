// Одобрение привилегированного действия — согласие владельца кнопкой, отдельно
// от аутентификации ОС.
//
// Почему отдельно. Пароль на поверхности (телеграм, мобилка) некуда ввести:
// askpass спрашивает человека только в живом TUI. Но и пускать агента под root
// без спроса нельзя. Решение: шим `sudo` в headless-сессии не зовёт askpass, а
// спрашивает разрешение у владельца через ядро — кнопкой «разрешить/отклонить».
// Кнопка НЕ подменяет пароль: одобренный вызов идёт как `sudo -n`, то есть
// сработает только под узким хостовым правилом NOPASSWD. Нет правила — sudo
// честно падает «password required», и это ожидаемый отказ, а не обход.
//
// Роли, как в askpass:
//  - **шим** (`sudo` в PATH детей): читает argv, шлёт заявку на сокет ядра,
//    ждёт решение и по «allow» транслоирует в `sudo -n`.
//  - **сервер** живёт в ядре (`surface_server`): сокет, заявка → SSE-событие
//    `approval.requested`, ожидание ответа владельца, ответ шиму.
//
// Заявка одноразовая и привязана к session/argv/cwd: повторное использование
// того же согласия невозможно — каждый вызов `sudo` спрашивает заново.
use std::path::{Path, PathBuf};

use crate::R;

/// Путь сокета одобрений рядом с сессиями инстанса.
pub fn sock_path(sessions: &Path) -> PathBuf {
    sessions
        .parent()
        .map(|p| p.join("approval.sock"))
        .unwrap_or_else(|| PathBuf::from("snens-approval.sock"))
}

/// Заявка шима серверу. `session` — что за сессия просит root; `cwd` и `argv` —
/// точное действие, которое показывают владельцу в карточке.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Request {
    pub session: String,
    pub cwd: String,
    pub argv: Vec<String>,
}

/// Шим привилегированного действия: если ядро дало канал одобрений — спрашиваем
/// согласие и исполняем `sudo -n`; иначе ведём себя как раньше (`-A` через askpass).
#[cfg(unix)]
pub fn privileged_shim(args: Vec<String>) -> R<()> {
    use std::os::unix::process::CommandExt;
    // Живой TUI: пароль есть у кого спросить — ведём как раньше, через askpass.
    // `SNENS_ASKPASS_DISABLED` ставит именно headless-сервер (surface), и только
    // там уместна кнопка одобрения вместо пароля.
    let headless = std::env::var_os("SNENS_ASKPASS_DISABLED").is_some();
    if !headless {
        let mut cmd = std::process::Command::new("/usr/bin/sudo");
        // `-n`/`-A` пользователя не затираем: `sudo -n` — рабочий способ «откажи,
        // если нет пароля» (тесты, скрипты, человек отошёл), а подмена на `-A`
        // превращала его в ожидание человека до таймаута run.
        if !has_askpass_flag(&args) {
            cmd.arg("-A");
        }
        let err = cmd.args(&args).exec();
        return Err(format!("sudo: не удалось запустить /usr/bin/sudo: {err}").into());
    }
    let approval_sock = std::env::var("SNENS_APPROVAL_SOCK").unwrap_or_default();
    if approval_sock.is_empty() {
        return Err("sudo: в headless-сессии нет канала одобрений (approval_timeout_seconds = 0)".into());
    }
    let allowed = request(&approval_sock, &args).unwrap_or(false);
    if !allowed {
        return Err("sudo: действие не одобрено владельцем (или истёк срок ожидания)".into());
    }
    // Одобрено. Исполняем через root-owned broker с его whitelist, если он есть:
    // тогда работает только разрешённый набор действий и валидация аргументов.
    // Broker — граница привилегии, кнопка — только согласие владельца.
    // `-n` — без запроса пароля: сработает под NOPASSWD-правилом на broker.
    let mut cmd = std::process::Command::new("/usr/bin/sudo");
    cmd.arg("-n");
    if let Some(broker) = privileged_broker() {
        cmd.arg(broker);
    }
    let err = cmd.args(&args).exec();
    Err(format!("sudo: не удалось запустить /usr/bin/sudo: {err}").into())
}

/// Есть ли у пользователя свой выбор способа аутентификации: `-n` (без спроса),
/// `-A` (askpass), `-S` (stdin) или длинные формы. Короткие флаги sudo допускают
/// склейку (`-nA`), поэтому проверяем каждый символ после дефиса.
fn has_askpass_flag(args: &[String]) -> bool {
    args.iter()
        // До `--` — флаги sudo, после — аргументы команды (`-n` там не наш).
        .take_while(|a| a.as_str() != "--")
        .any(|a| {
            if a.starts_with("--") {
            return matches!(
                a.as_str(),
                "--non-interactive" | "--askpass" | "--stdin" | "--reset-timestamp"
            );
        }
        a.starts_with('-')
            && a.chars()
                .skip(1)
                .any(|c| matches!(c, 'n' | 'A' | 'S' | 'k' | 'K'))
    })
}

/// Путь к root-owned broker привилегированных действий, если он установлен.
/// Значение читается из `SNENS_PRIV` (его ставит харнесс детям), дефолт —
/// стандартное место установки. Наличие файла проверяем, чтобы безусловно не
/// обещать канал, которого нет.
pub fn privileged_broker() -> Option<String> {
    let candidate = std::env::var("SNENS_PRIV").unwrap_or_else(|_| "/usr/local/bin/snens-priv".into());
    if candidate.is_empty() {
        return None;
    }
    let path = Path::new(&candidate);
    if path.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Отправить заявку и дождаться решения. Строка «allow» — согласие, всё прочее
/// (в т.ч. молчание/обрыв) — отказ.
#[cfg(unix)]
fn request(sock: &str, args: &[String]) -> R<bool> {
    use std::io::{BufRead, BufReader, Write};
    let req = Request {
        session: std::env::var("SNENS_SESSION").unwrap_or_default(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        argv: args.to_vec(),
    };
    let mut conn = std::os::unix::net::UnixStream::connect(sock)
        .map_err(|e| format!("approval: нет канала одобрений ({e})"))?;
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    conn.write_all(line.as_bytes())?;
    conn.flush()?;
    let mut answer = String::new();
    BufReader::new(conn).read_line(&mut answer)?;
    Ok(answer.trim() == "allow")
}

#[cfg(not(unix))]
pub fn privileged_shim(_args: Vec<String>) -> R<()> {
    Err("approval: не поддержан на этой платформе".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_keeps_user_auth_flags() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Свой выбор способа аутентификации шим не затирает: `sudo -n` должен
        // отказывать сразу, а не превращаться в ожидание человека.
        assert!(has_askpass_flag(&s(&["-n", "true"])));
        assert!(has_askpass_flag(&s(&["-nA", "true"])));
        assert!(has_askpass_flag(&s(&["--non-interactive", "true"])));
        assert!(has_askpass_flag(&s(&["-S", "true"])));
        // Обычный вызов — флага аутентификации нет, шим добавит свой `-A`.
        assert!(!has_askpass_flag(&s(&["-u", "www-data", "id"])));
        // После `--` идут аргументы команды, а не флаги sudo.
        assert!(!has_askpass_flag(&s(&["--", "-n"])));
    }

    #[test]
    fn sock_sits_next_to_sessions() {
        assert_eq!(
            sock_path(Path::new("/data/snens/sessions")),
            PathBuf::from("/data/snens/approval.sock")
        );
    }

    #[test]
    fn request_round_trips_as_one_line() {
        let req = Request {
            session: "s1".into(),
            cwd: "/tmp".into(),
            argv: vec!["mkdir".into(), "/mnt/x".into()],
        };
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        assert_eq!(line.matches('\n').count(), 1);
        let back: Request = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back.argv, vec!["mkdir", "/mnt/x"]);
    }
}
