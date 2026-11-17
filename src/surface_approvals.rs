// Одобрение привилегированных действий: сокет для sudo-шима и решение владельца.
//
// Headless-сессия не может спросить пароль в терминале, поэтому шим
// подключается к сокету, а согласие даёт человек кнопкой в чате. Всё, что
// касается заявок, собрано здесь: сокет, карточка, ответ.

use crate::surface_runtime::{self as rt, Runtime};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::time::Duration;

use super::http::err;

/// Поднять сокет одобрений. Шим `sudo` в headless-сессии подключается к нему,
/// шлёт заявку (session/cwd/argv) и ждёт решение. Ядро эмитит `approval.requested`
/// в сессию владельца, а решение приходит POST'ом и закрывает канал ответа.
/// Поднять сокет заявок. Возвращается ручка: без неё задача переживает shutdown,
/// `accept()` держит сокет и соединения, а ошибка `bind` видна только в stderr.
pub(super) fn spawn_approval_server(s: Arc<Runtime>) -> Option<tokio::task::JoinHandle<()>> {
    let timeout = s.cfg.approval_timeout_seconds;
    if timeout == 0 {
        return None;
    }
    #[cfg(not(unix))]
    {
        let _ = (s, timeout);
        None
    }
    #[cfg(unix)]
    Some(tokio::spawn(async move {
        let path = crate::approval::sock_path(&s.cfg.sessions_path());
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = tokio::net::UnixListener::bind(&path) else {
            eprintln!("[snens] не удалось поднять сокет одобрений {}", path.display());
            return;
        };
        let _ = std::fs::set_permissions(
            &path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let s = s.clone();
            tokio::spawn(async move {
                let _ = handle_approval_conn(s, stream, timeout).await;
            });
        }
    }))
}

#[cfg(unix)]
pub(super) async fn handle_approval_conn(
    s: Arc<Runtime>,
    mut stream: tokio::net::UnixStream,
    timeout: u64,
) -> crate::R<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).await?;
    let req: crate::approval::Request = serde_json::from_str(line.trim_end())?;
    // Заявка привязана к сессии: решение ищется по её событиям. Неизвестная
    // сессия — сразу отказ, без карточки (нельзя показать владельцу — нет чата).
    let known = s.data.lock().await.sessions.contains_key(&req.session);
    let answer = if !known {
        false
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        s.approvals.lock().await.insert(id.clone(), tx);
        // Точное действие в карточке: argv как есть, плюс cwd и сессия.
        let detail = req
            .argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = s
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
    if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
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
    let Some(tx) = s.approvals.lock().await.remove(&aid) else {
        // Уже решено/истекло: повтор идемпотентен как «нет» — карточку давно сняли.
        return err("gone", "approval is no longer pending", StatusCode::GONE).into_response();
    };
    let _ = s
        .emit(
            &id,
            None,
            "approval.resolved",
            serde_json::json!({"approval_id":aid,"allow":b.allow}),
        )
        .await;
    let _ = tx.send(b.allow);
    (StatusCode::OK, Json(serde_json::json!({"resolved":true}))).into_response()
}
