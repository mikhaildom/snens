use crate::cfg::Config;
use crate::log::{Body, Rec};
use serde::Serialize;
use std::{
    io::{BufRead, BufReader},
    path::Path,
};

#[derive(Clone, Debug, Serialize)]
pub struct HistoryMessage {
    pub seq: u64,
    pub ts: u64,
    pub role: &'static str,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// Сколько занял вызов инструмента, мс (пишется в `ToolResult`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// argv вызова — уже подставленный, а не сырой JSON аргументов. Нужен,
    /// чтобы показать вызов и результат одним блоком без разбора лога на клиенте.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// Вложения, которые инструмент положил в канал (`SNENS_ATTACH_DIR`): по ним
    /// поверхность показывает картинку, которую смотрел агент, а не только текст.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<crate::log::Part>>,
    /// Автор записи: `steer` | `ask` | `report` для вставок в ход (сообщение
    /// человека, вопрос подагента, его отчёт). `None` — обычная реплика.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Кто вставил: человек/подагент. Для `via: "steer"` пусто.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Расход хода, к которому относится эта запись `assistant`: считается из
    /// следующей за ней записи `usage` того же хода.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    /// Весь вход хода (включая кешированную часть).
    pub in_tok: u64,
    pub cached_tok: u64,
    pub out_tok: u64,
    pub reason_tok: u64,
    /// Стоимость хода, микро-доллары (провайдерская цена).
    pub cost_micro: u64,
}

/// Транскрипт без служебной справки памяти — обычное чтение журнала.
pub fn read(path: &Path, cfg: &Config) -> crate::R<Vec<HistoryMessage>> {
    read_with(path, cfg, false)
}

/// Project transcript records directly from the append-only journal. `seq` is
/// the 1-based physical JSONL line number, independent of delivery event seq.
///
/// `with_recall` включает служебную справку долгой памяти в выдачу — она нужна
/// только поверхности, которая решила её показать (веб-клиент, галочка в
/// настройках). По умолчанию выключено: справка — это контекст промпта, а не
/// транскрипт, и в общей ленте/поиске ей не место.
pub fn read_with(
    path: &Path,
    _cfg: &Config,
    with_recall: bool,
) -> crate::R<Vec<HistoryMessage>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path)?;
    let mut rows: Vec<HistoryMessage> = Vec::new();
    let mut lines = BufReader::new(bytes.as_slice()).lines().peekable();
    let mut seq = 0u64;
    while let Some(line) = lines.next() {
        seq += 1;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rec = match serde_json::from_str::<Rec>(&line) {
            Ok(rec) => rec,
            Err(error) if lines.peek().is_none() && !bytes.ends_with(b"\n") => {
                let _ = error;
                continue;
            }
            Err(error) => {
                return Err(format!("invalid JSONL record at physical line {seq}: {error}").into());
            }
        };
        let row = match rec {
            Rec::User {
                ts,
                text,
                source,
                parts,
            } => Some(HistoryMessage {
                seq,
                ts,
                role: "user",
                // `content` — только текст сообщения. Раньше при непустых parts
                // сюда уезжала сериализация сырых сообщений провайдера: в чате
                // это была простыня JSON с системным промптом вместо реплики.
                // Вложения едут отдельным полем `parts`.
                content: Some(text),
                source: Some(source),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
                exit: None,
                ms: None,
                argv: None,
                parts: if parts.is_empty() { None } else { Some(parts) },
                via: None,
                from: None,
                usage: None,
            }),
            Rec::Steer {
                ts,
                text,
                from,
                via,
            } => {
                // Префиксы вида «[сообщение от человека]», «[итог от 1792…]» —
                // дело поверхности, а не данных: роль и автор едут полями.
                // В тексте остаётся ровно то, что было сказано.
                Some(HistoryMessage {
                    seq,
                    ts,
                    role: "user",
                    content: Some(text),
                    source: Some(from.clone()),
                    via: Some(via),
                    from: Some(from),
                    reasoning: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    exit: None,
                    ms: None,
                    argv: None,
                    parts: None,
                    usage: None,
                })
            }
            // These records are internal prompt context, not transcript.
            // Справка памяти — исключение по запросу поверхности: человек вправе
            // видеть, что агент вспомнил перед ходом (шаг `kind: "recall"`).
            Rec::Recall { ts, text } if with_recall => Some(HistoryMessage {
                seq,
                ts,
                // Роль `recall` — не `user`/`assistant`, а отдельный вид шага:
                // клиент рисует её серой строкой и не путает с репликами.
                role: "recall",
                content: Some(text),
                source: None,
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
                exit: None,
                ms: None,
                argv: None,
                parts: None,
                via: None,
                from: None,
                usage: None,
            }),
            Rec::Recall { .. } | Rec::ContextNotice { .. } => None,
            // Отметка саморевью — служебная строка журнала, но в транскрипте
            // ей место: человек должен видеть, где проходила граница разбора.
            // Отдельная роль `review` (как `recall` у справки памяти): клиент
            // рисует её тонкой серой строкой и не путает с репликой.
            Rec::Review { ts, turns, text, .. } => Some(HistoryMessage {
                seq,
                ts,
                role: "review",
                content: Some(if text.is_empty() {
                    crate::log::review_line(turns)
                } else {
                    text
                }),
                source: None,
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
                exit: None,
                ms: None,
                argv: None,
                parts: None,
                via: None,
                from: None,
                usage: None,
            }),
            Rec::Assistant {
                ts,
                text,
                reasoning,
                tool_calls,
            } => {
                let calls = tool_calls
                    .into_iter()
                    .map(|call| {
                        serde_json::json!({
                            "id": call.id, "type": "function",
                            "function": {"name": call.name, "arguments": call.args}
                        })
                    })
                    .collect::<Vec<_>>();
                Some(HistoryMessage {
                    seq,
                    ts,
                    role: "assistant",
                    content: text,
                    source: None,
                    reasoning,
                    tool_calls: if calls.is_empty() { None } else { Some(calls) },
                    tool_call_id: None,
                    name: None,
                    exit: None,
                    ms: None,
                    argv: None,
                    parts: None,
                    via: None,
                    from: None,
                    usage: None,
                })
            }
            Rec::ToolResult {
                ts,
                call_id,
                tool,
                exit,
                ms,
                argv,
                body,
                parts,
                ..
            } => {
                let content = match body {
                    Body::Inline { text } => Some(text),
                };
                Some(HistoryMessage {
                    seq,
                    ts,
                    role: "tool",
                    content,
                    source: None,
                    reasoning: None,
                    tool_calls: None,
                    tool_call_id: Some(call_id),
                    name: Some(tool),
                    exit: Some(exit),
                    ms: Some(ms),
                    argv: Some(argv),
                    parts: if parts.is_empty() { None } else { Some(parts) },
                    via: None,
                    from: None,
                    usage: None,
                })
            }
            // Расход хода лежит отдельной записью сразу после `assistant`.
            // Приклеиваем его к этой записи: модели интересна цена ответа, а не
            // отдельная строка «usage» в транскрипте.
            Rec::Usage {
                in_tok,
                cached_tok,
                out_tok,
                cost_micro,
                reason_tok,
                ..
            } => {
                if let Some(last) = rows.iter_mut().rev().find(|r| r.role == "assistant") {
                    last.usage = Some(Usage {
                        in_tok,
                        cached_tok,
                        out_tok,
                        reason_tok,
                        cost_micro,
                    });
                }
                None
            }
            _ => None,
        };
        if let Some(row) = row {
            rows.push(row);
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("surface-history-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn internal_prompt_markers_are_not_serialized_in_history() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"kind\":\"recall\",\"ts\":1,\"text\":\"PRIVATE_RECALL_MARKER\"}\n",
                "{\"kind\":\"context_notice\",\"ts\":2,\"text\":\"PRIVATE_CONTEXT_MARKER\"}\n"
            ),
        )
        .unwrap();

        let rows = read(&path, &Config::default()).unwrap();
        let json = serde_json::to_string(&rows).unwrap();
        assert!(rows.is_empty());
        assert!(!json.contains("PRIVATE_RECALL_MARKER"));
        assert!(!json.contains("PRIVATE_CONTEXT_MARKER"));

        // А по запросу поверхности справка памяти отдаётся как шаг `recall`:
        // человек включает её показ галочкой, и тогда он видит, что агент вспомнил.
        let shown = read_with(&path, &Config::default(), true).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].role, "recall");
        assert!(shown[0].content.as_deref().unwrap().contains("PRIVATE_RECALL_MARKER"));
        // `context_notice` остаётся служебным даже с флагом: это не память.
        assert!(!serde_json::to_string(&shown).unwrap().contains("PRIVATE_CONTEXT_MARKER"));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Маркер саморевью — отдельная роль `review`, а не реплика: клиент рисует её
    /// тонкой серой строкой, как справку памяти. Сама строка — короткая.
    #[test]
    fn review_marker_is_a_separate_thin_row() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"kind\":\"user\",\"ts\":1,\"text\":\"сделай\",\"source\":\"web\"}\n",
                "{\"kind\":\"review\",\"ts\":2,\"review_id\":\"s-review\",\"turns\":12,\"text\":\"саморевью · 12 ходов\"}\n",
                "{\"kind\":\"assistant\",\"ts\":3,\"text\":\"готово\",\"tool_calls\":[]}\n",
                // Запись без текста (ручная/старая) тоже читается.
                "{\"kind\":\"review\",\"ts\":4,\"review_id\":\"s-review\",\"turns\":13}\n"
            ),
        )
        .unwrap();

        let rows = read(&path, &Config::default()).unwrap();
        let roles: Vec<&str> = rows.iter().map(|r| r.role).collect();
        assert_eq!(roles, ["user", "review", "assistant", "review"], "маркер не отличим от реплики");
        assert_eq!(rows[1].content.as_deref(), Some("саморевью · 12 ходов"));
        assert_eq!(rows[1].seq, 2, "seq — физическая строка журнала");
        // Без текста строка собирается из числа ходов и остаётся короткой.
        let bare = rows[3].content.as_deref().unwrap();
        assert!(bare.contains("13") && bare.chars().count() <= 80, "{bare}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_message_with_attachments_keeps_plain_text() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        // Ровно тот случай, что ломал первое сообщение сессии: голосовое/картинка
        // в телеграме даёт непустые parts, и раньше content подменялся JSON-ом
        // сырых сообщений провайдера (системный промпт вместо реплики).
        fs::write(
            &path,
            concat!(
                "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n",
                "{\"kind\":\"user\",\"ts\":2,\"text\":\"послушай голосовое\",\"source\":\"telegram\",",
                "\"parts\":[{\"kind\":\"audio\",\"path\":\"/tmp/v.ogg\",\"mime\":\"audio/ogg\",\"bytes\":10}]}\n"
            ),
        )
        .unwrap();

        let rows = read(&path, &Config::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content.as_deref(), Some("послушай голосовое"));
        let parts = rows[0].parts.as_ref().expect("вложения сохраняются");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].kind, "audio");
        assert_eq!(parts[0].mime, "audio/ogg");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn projects_transcript_with_physical_line_sequences_and_no_mutation() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let raw = concat!(
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\"}\n",
            "{\"kind\":\"user\",\"ts\":2,\"text\":\"hi\",\"source\":\"surface\"}\n",
            "{\"kind\":\"assistant\",\"ts\":3,\"text\":null,\"reasoning\":\"think\",\"tool_calls\":[{\"id\":\"c1\",\"name\":\"run\",\"args\":\"{}\"}]}\n",
            "{\"kind\":\"tool_result\",\"ts\":4,\"call_id\":\"c1\",\"tool\":\"run\",\"argv\":[],\"exit\":0,\"bytes\":0,\"ms\":1,\"body\":{\"t\":\"inline\",\"text\":\"ok\"},\"parts\":[]}\n"
        );
        fs::write(&path, raw).unwrap();
        let before = fs::read(&path).unwrap();
        let rows = read(&path, &Config::default()).unwrap();
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [2, 3, 4]);
        assert_eq!(rows[0].role, "user");
        assert_eq!(rows[0].content.as_deref(), Some("hi"));
        assert_eq!(rows[1].tool_calls.as_ref().unwrap()[0]["id"], "c1");
        assert_eq!(rows[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(before, fs::read(&path).unwrap());
        fs::write(
            &path,
            "{\"kind\":\"user\",\"ts\":1,\"text\":\"ok\",\"source\":\"tui\"}\n{broken}\n",
        )
        .unwrap();
        assert!(
            read(&path, &Config::default())
                .unwrap_err()
                .to_string()
                .contains("physical line 2")
        );
        fs::write(
            &path,
            "{\"kind\":\"user\",\"ts\":1,\"text\":\"ok\",\"source\":\"tui\"}\n{incomplete",
        )
        .unwrap();
        assert_eq!(read(&path, &Config::default()).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
