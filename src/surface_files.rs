// Файлы сессии: загрузка вложений и отдача того, что лежит в её каталогах.
//
// Обе стороны держат границу «только своё»: вложение пишется в
// `<id>.attachments`, а на чтение отдаётся лишь путь внутри `<id>.out` или
// `<id>.attachments` — иначе `/file` был бы дыркой на весь диск.

use crate::surface_runtime::{self as rt, Runtime};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use super::http::err;

#[derive(Deserialize)]
pub(super) struct UploadQuery {
    filename: String,
    command_id: String,
}
pub(super) fn attachment_response(
    id: &str,
    name: &str,
    path: &str,
    mime: &str,
    bytes: usize,
) -> serde_json::Value {
    serde_json::json!({"attachment_id":id,"name":name,"path":path,"mime":mime,"bytes":bytes})
}
pub(super) async fn upload_attachment(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(q): Query<UploadQuery>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    if !rt::valid_id(&id)
        || !rt::valid_id(&q.command_id)
        || q.filename.is_empty()
        || q.filename.len() > 255
        || q.filename.contains('/')
        || q.filename.contains('\\')
    {
        return err(
            "invalid",
            "invalid attachment id or filename",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let max = s.cfg.max_attachment_bytes;
    let body = match axum::body::to_bytes(request.into_body(), max.saturating_add(1)).await {
        Ok(b) if b.len() <= max => b,
        _ => {
            return err(
                "too_large",
                "attachment exceeds configured size limit",
                StatusCode::PAYLOAD_TOO_LARGE,
            )
            .into_response();
        }
    };
    let digest = format!("{:x}", Sha256::digest(&body));
    let _guard = s.session_lock(&id).await.lock_owned().await;
    let mut d = s.data.lock().await;
    if !d.sessions.contains_key(&id) {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    if let Some(existing) = d
        .attachments
        .get(&id)
        .and_then(|m| m.values().find(|a| a.command_id == q.command_id))
    {
        if existing.filename == q.filename && existing.sha256 == digest {
            let p = &existing.part;
            return (
                StatusCode::OK,
                Json(attachment_response(
                    &existing.attachment_id,
                    &existing.name,
                    &p.path,
                    &p.mime,
                    p.bytes,
                )),
            )
                .into_response();
        }
        return err(
            "idempotency_conflict",
            "command_id reused with different upload",
            StatusCode::CONFLICT,
        )
        .into_response();
    }
    let original = std::path::Path::new(&q.filename);
    let candidate = crate::attach::attachment_kind(original);
    let (kind, mime) = candidate.unwrap_or_else(|| {
        let mime = match original
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "md" | "csv" | "log" => "text/plain",
            "json" => "application/json",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
        ("file", mime)
    });
    let aid = uuid::Uuid::new_v4().to_string();
    let extension = original
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 16 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let dir = s.cfg.sessions_path().join(format!("{id}.attachments"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(format!("{aid}{extension}"));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let write_result = async {
        let mut f = options.open(&path).await?;
        f.write_all(&body).await?;
        f.sync_all().await
    }
    .await;
    if let Err(e) = write_result {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    let (width, height) = if kind == "image" {
        crate::attach::image_dimensions(&body, mime)
    } else {
        (0, 0)
    };
    if kind == "image" && (width == 0 || height == 0) {
        let _ = tokio::fs::remove_file(&path).await;
        return err(
            "invalid_image",
            "image extension/header mismatch",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let name = q
        .filename
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let part = crate::log::Part {
        kind: kind.into(),
        path: path.to_string_lossy().into_owned(),
        mime: mime.into(),
        bytes: body.len(),
        width,
        height,
    };
    let attachment = rt::Attachment {
        attachment_id: aid.clone(),
        command_id: q.command_id,
        filename: q.filename,
        name: name.clone(),
        part: part.clone(),
        sha256: digest,
    };
    d.attachments
        .entry(id.clone())
        .or_default()
        .insert(aid.clone(), attachment.clone());
    if let Err(e) = s.save_attachment(&id, &attachment) {
        d.attachments.get_mut(&id).and_then(|m| m.remove(&aid));
        let _ = tokio::fs::remove_file(&path).await;
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    (
        StatusCode::CREATED,
        Json(attachment_response(
            &aid, &name, &part.path, &part.mime, part.bytes,
        )),
    )
        .into_response()
}

#[derive(Deserialize)]
pub(super) struct FileQuery {
    /// Абсолютный путь к файлу из `parts` записи лога: картинка, которую
    /// посмотрел `view`, или вложение инструмента. Отдаётся только то, что
    /// лежит в каталогах этой сессии, — иначе это была бы дырка на весь диск.
    path: String,
}

pub(super) async fn session_file(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) || q.path.is_empty() {
        return err("invalid", "invalid session id or path", StatusCode::BAD_REQUEST).into_response();
    }
    let root = s.cfg.sessions_path();
    let Ok(canonical) = tokio::fs::canonicalize(&q.path).await else {
        return err("not_found", "file not found", StatusCode::NOT_FOUND).into_response();
    };
    let Ok(root) = tokio::fs::canonicalize(&root).await else {
        return err("storage", "sessions directory is missing", StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    };
    // Файл обязан лежать в каталоге, принадлежащем именно этой сессии:
    // `<id>.out/…` (канал вложений инструментов) или `<id>.attachments/…`.
    let in_session_out = canonical.starts_with(root.join(format!("{id}.out")));
    let in_session_attach = canonical.starts_with(root.join(format!("{id}.attachments")));
    if !canonical.starts_with(&root) || !(in_session_out || in_session_attach) {
        return err(
            "forbidden",
            "file does not belong to this session",
            StatusCode::FORBIDDEN,
        )
        .into_response();
    }
    let Ok(bytes) = tokio::fs::read(&canonical).await else {
        return err("not_found", "file not found", StatusCode::NOT_FOUND).into_response();
    };
    let mime = crate::attach::attachment_kind(&canonical)
        .map(|(_, mime)| mime.to_string())
        .unwrap_or_else(|| {
            match canonical
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str()
            {
                "txt" | "md" | "log" | "csv" => "text/plain; charset=utf-8".to_string(),
                "json" => "application/json".to_string(),
                _ => "application/octet-stream".to_string(),
            }
        });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime)],
        [(header::CACHE_CONTROL, "private, max-age=3600".to_string())],
        bytes,
    )
        .into_response()
}
pub(super) fn resolve_attachments(
    d: &rt::Store,
    sid: &str,
    ids: &[String],
) -> Result<Vec<crate::log::Part>, Box<axum::response::Response>> {
    let mut result = Vec::new();
    for id in ids {
        let Some(a) = d.attachments.get(sid).and_then(|m| m.get(id)) else {
            return Err(Box::new(
                err(
                    "not_found",
                    "attachment not found in session",
                    StatusCode::BAD_REQUEST,
                )
                .into_response(),
            ));
        };
        result.push(a.part.clone());
    }
    Ok(result)
}
