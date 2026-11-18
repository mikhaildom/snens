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
    /// Ответ человека на вопрос из артефакта (`snens.ask`): какой артефакт, какой
    /// вопрос и что уехало модели. Есть — клиент рисует серую строку с
    /// раскрытием, а не реплику человека.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask: Option<crate::log::AskRef>,
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

/// Project transcript records directly from the append-only journal. `seq` is
/// the 1-based physical JSONL line number, independent of delivery event seq.
///
/// `with_recall` включает служебную справку долгой памяти в выдачу — она нужна
/// только поверхности, которая решила её показать (веб-клиент, галочка в
/// настройках). По умолчанию выключено: справка — это контекст промпта, а не
/// транскрипт, и в общей ленте/поиске ей не место.
pub fn read_with(path: &Path, with_recall: bool) -> crate::R<Vec<HistoryMessage>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    // Читаем потоком, а не через `std::fs::read` + `BufReader` по срезу: файл
    // всё равно целиком уезжает в память в виде записей, и вторая копия байтов
    // на мегабайтном журнале удваивала пик.
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut rows: Vec<HistoryMessage> = Vec::new();
    let mut seq = 0u64;
    let mut raw = String::new();
    let mut last_ended_with_newline;
    loop {
        raw.clear();
        let read = reader.read_line(&mut raw)?;
        if read == 0 {
            break;
        }
        seq += 1;
        last_ended_with_newline = raw.ends_with('\n');
        let line = raw.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        let rec = match serde_json::from_str::<Rec>(line) {
            Ok(rec) => rec,
            // Недописанный хвост (обрыв записи, процесс убили) — норма журнала,
            // иначе один оборванный ход ломал бы весь транскрипт. Граница
            // честная: терпим только `is_eof` или строку без завершающего `\n`
            // — это и есть признаки обрыва. Битый, но ЗАВЕРШЁННЫЙ переводом
            // строки последний байт остаётся ошибкой: это порча файла, и её
            // видно, а не прячем.
            Err(error) if error.is_eof() || !last_ended_with_newline => {
                // Не молча: тихая потеря последней записи неотличима от «её не
                // было», и разбираться с этим потом нечем.
                eprintln!(
                    "[snens] журнал {}: хвост не разобран (строка {seq}), запись пропущена: {error}",
                    path.display()
                );
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
                ask,
            } => Some(HistoryMessage {
                seq,
                ts,
                role: "user",
                // `content` — только текст сообщения. Раньше при непустых parts
                // сюда уезжала сериализация сырых сообщений провайдера: в чате
                // это была простыня JSON с системным промптом вместо реплики.
                // Вложения едут отдельным полем `parts`.
                content: Some(text),
                // `wake` — вернувшаяся работа (итог фонового задания, отчёт
                // подагента), а не слово человека. Едет в том же поле `via`, что
                // у вставок в ход: лента по нему понимает, что это не реплика.
                via: (source == "wake").then(|| source.clone()),
                source: Some(source),
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
                exit: None,
                ms: None,
                argv: None,
                parts: if parts.is_empty() { None } else { Some(parts) },
                from: None,
                usage: None,
                ask,
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
                    ask: None,
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
                ask: None,
            }),
            Rec::Recall { .. } | Rec::ContextNotice { .. } => None,
            // Отметка саморевью — служебная строка журнала, но в транскрипте
            // ей место: человек должен видеть, где проходила граница разбора.
            // Отдельная роль `review` (как `recall` у справки памяти): клиент
            // рисует её тонкой серой строкой и не путает с репликой.
            Rec::Review {
                ts, turns, text, ..
            } => Some(HistoryMessage {
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
                ask: None,
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
                    ask: None,
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
                    ask: None,
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
                // Приклеиваем расход к записи, стоящей НЕПОСРЕДСТВЕННО перед
                // `usage`, и только если у неё ещё нет своего.
                // Обратный поиск «последний assistant где угодно» перезаписывал
                // уже приклеенный расход предыдущего хода: для пустого ответа
                // провайдера агент пишет `Usage` без `Assistant` (agent.rs:407)
                // и повторяет запрос — и ход N показывал цену выброшенного ответа.
                match rows.last_mut() {
                    Some(last) if last.role == "assistant" && last.usage.is_none() => {
                        last.usage = Some(Usage {
                            in_tok,
                            cached_tok,
                            out_tok,
                            reason_tok,
                            cost_micro,
                        });
                    }
                    _ => {}
                }
                None
            }
            // Служебные записи журнала, которым в транскрипте места нет.
            // Перечислены явно, без `_`: новый вариант `Rec` в этом же крейте
            // тогда даст ошибку компиляции, а не молча исчезнет с поверхности.
            Rec::Meta { .. }
            | Rec::System { .. }
            | Rec::Replay { .. }
            | Rec::Ctx { .. }
            | Rec::ContextRevision { .. }
            | Rec::Started { .. }
            | Rec::Done { .. } => None,
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

        let rows = read_with(&path, false).unwrap();
        let json = serde_json::to_string(&rows).unwrap();
        assert!(rows.is_empty());
        assert!(!json.contains("PRIVATE_RECALL_MARKER"));
        assert!(!json.contains("PRIVATE_CONTEXT_MARKER"));

        // А по запросу поверхности справка памяти отдаётся как шаг `recall`:
        // человек включает её показ галочкой, и тогда он видит, что агент вспомнил.
        let shown = read_with(&path, true).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].role, "recall");
        assert!(shown[0]
            .content
            .as_deref()
            .unwrap()
            .contains("PRIVATE_RECALL_MARKER"));
        // `context_notice` остаётся служебным даже с флагом: это не память.
        assert!(!serde_json::to_string(&shown)
            .unwrap()
            .contains("PRIVATE_CONTEXT_MARKER"));
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

        let rows = read_with(&path, false).unwrap();
        let roles: Vec<&str> = rows.iter().map(|r| r.role).collect();
        assert_eq!(
            roles,
            ["user", "review", "assistant", "review"],
            "маркер не отличим от реплики"
        );
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

        let rows = read_with(&path, false).unwrap();
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
        let rows = read_with(&path, false).unwrap();
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
        assert!(read_with(&path, false)
            .unwrap_err()
            .to_string()
            .contains("physical line 2"));
        fs::write(
            &path,
            "{\"kind\":\"user\",\"ts\":1,\"text\":\"ok\",\"source\":\"tui\"}\n{incomplete",
        )
        .unwrap();
        assert_eq!(read_with(&path, false).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
