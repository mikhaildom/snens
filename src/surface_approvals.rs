// Одобрение привилегированных действий: сокет для sudo-шима и решение владельца.
//
// Headless-сессия не может спросить пароль в терминале, поэтому шим
// подключается к сокету, а согласие даёт человек кнопкой в чате. Всё, что
// касается заявок, собрано здесь: сокет, карточка, ответ.

use crate::surface_runtime::{self as rt, Runtime};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::time::Duration;

use super::http::err;

/// Поднять сокет одобрений. Шим `sudo` в headless-сессии подключается к нему,
/// шлёт заявку (session/cwd/argv) и ждёт решение. Ядро эмитит `approval.requested`
/// в сессию владельца, а решение приходит POST'ом и закрывает канал ответа.
pub(super) fn spawn_approval_server(s: Arc<Runtime>) {
    let timeout = s.cfg.approval_timeout_seconds;
    if timeout == 0 {
        return;
    }
    #[cfg(not(unix))]
    {
        let _ = (s, timeout);
    }
    #[cfg(unix)]
    tokio::spawn(async move {
        let path = crate::approval::sock_path(&s.cfg.sessions_path());
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = bind_private(&path) else {
            eprintln!(
                "[snens] не удалось поднять сокет одобрений {} (или он не стал 0600)",
                path.display()
            );
            return;
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let s = s.clone();
            tokio::spawn(async move {
                let _ = handle_approval_conn(s, stream, timeout).await;
            });
        }
    });
}

#[cfg(unix)]
fn bind_private(path: &std::path::Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    // Сокет создаётся с 0600 сразу: между `bind` и `chmod` он бы жил с
    // `0777 & ~umask` (обычно 0755), и любой локальный пользователь мог бы
    // подсунуть заявку на `sudo` — владелец увидел бы чужой argv.
    let old = unsafe { libc::umask(0o177) };
    let bound = tokio::net::UnixListener::bind(path);
    unsafe { libc::umask(old) };
    let listener = bound?;
    // Даже с umask проверяем: на файловой системе без прав режим мог не
    // примениться — тогда сокет убираем и сервер не поднимаем (молчаливый
    // 0755-сокет хуже, чем отсутствие одобрений).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode != 0o600 {
        let _ = std::fs::remove_file(path);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("режим сокета {mode:o}, ожидался 600"),
        ));
    }
    Ok(listener)
}

#[cfg(unix)]
pub(super) async fn handle_approval_conn(
    s: Arc<Runtime>,
    mut stream: tokio::net::UnixStream,
    timeout: u64,
) -> crate::R<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    // Потолок на строку и таймаут на её получение: без них клиент, который
    // подключился и молчит (или льёт строку без `\n`), держал бы задачу и её
    // буфер вечно, а таймаут одобрения начинался бы только после разбора.
    const MAX_LINE: u64 = 64 * 1024;
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream).take(MAX_LINE);
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .map_err(|_| "заявка на одобрение не пришла за 10 с")??;
    }
    let req: crate::approval::Request = serde_json::from_str(line.trim_end())?;
    // Заявка привязана к сессии: решение ищется по её событиям. Неизвестная
    // сессия — сразу отказ, без карточки (нельзя показать владельцу — нет чата).
    let known = s.data.lock().await.sessions.contains_key(&req.session);
    let answer = if !known {
        false
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        s.approvals
            .lock()
            .await
            .insert(id.clone(), (req.session.clone(), tx));
        // Точное действие в карточке: argv как есть, плюс cwd и сессия.
        let detail = req
            .argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        // Ошибку `emit` не глотаем: если карточка не ушла (диск, сериализация),
        // шим будет ждать весь `approval_timeout_seconds` и получить
        // необъяснимый отказ. Честнее ответить «нет» сразу.
        let card = s
            .emit(
                &req.session,
                None,
                "approval.requested",
                serde_json::json!({
                    "approval_id":id,
                    "command":format!("sudo {detail}"),
                    "cwd":req.cwd,
                }),
            )
            .await;
        if card.is_err() {
            s.approvals.lock().await.remove(&id);
            let _ = stream.write_all(b"deny\n").await;
            let _ = stream.flush().await;
            return Ok(());
        }
        let decided = tokio::time::timeout(Duration::from_secs(timeout), rx).await;
        s.approvals.lock().await.remove(&id);
        match decided {
            Ok(Ok(allow)) => allow,
            // Таймаут или обрыв: считаем отказом, шим получит «не одобрено».
            _ => false,
        }
    };
    let _ = stream
        .write_all(if answer { b"allow\n" } else { b"deny\n" })
        .await;
    let _ = stream.flush().await;
    Ok(())
}

/// Показать argv в карточке так, как его увидел бы командный интерпретатор:
/// элементы с пробелами оборачиваем в кавычки, чтобы владелец видел точную
/// границу аргумента, а не «одну длинную команду».
pub(super) fn shell_quote(arg: &str) -> String {
    if arg.is_empty()
        || arg
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        format!("{:?}", arg)
    } else {
        arg.to_string()
    }
}

#[derive(Deserialize)]
pub(super) struct ApprovalDecision {
    allow: bool,
}

/// Решение владельца по заявке: `{allow: true|false}`.
pub(super) async fn approve(
    State(s): State<Arc<Runtime>>,
    Path((id, aid)): Path<(String, String)>,
    Json(b): Json<ApprovalDecision>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) || aid.is_empty() {
        return err("invalid", "invalid id", StatusCode::BAD_REQUEST).into_response();
    }
    let Some((owner, tx)) = s.approvals.lock().await.remove(&aid) else {
        // Уже решено/истекло: повтор идемпотентен как «нет» — карточку давно сняли.
        return err("gone", "approval is no longer pending", StatusCode::GONE).into_response();
    };
    // Решение принадлежит той сессии, где висит карточка: иначе резолв летел бы
    // в чужой поток, а в настоящей сессии карточка оставалась бы висеть.
    if owner != id {
        // Заявка не снимается: её законный владелец ещё может решить.
        s.approvals.lock().await.insert(aid.clone(), (owner, tx));
        return err(
            "not_found",
            "approval belongs to another session",
            StatusCode::NOT_FOUND,
        )
        .into_response();
    }
    let _ = s
        .emit(
            &owner,
            None,
            "approval.resolved",
            serde_json::json!({"approval_id":aid,"allow":b.allow}),
        )
        .await;
    let _ = tx.send(b.allow);
    (StatusCode::OK, Json(serde_json::json!({"resolved":true}))).into_response()
}
