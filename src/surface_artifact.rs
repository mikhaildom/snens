//! Вопросы артефактов: разбор фенса `artifact` из журнала и учёт ответов.
//!
//! Ответ человека на интерфейс (`snens.ask`) уезжает в чат обычной репликой —
//! по одной команде её не отличить от набранной руками. Поэтому здесь живут две
//! вещи:
//!
//! 1. **разбор фенса** — иначе нечем проверить, на какой артефакт ссылается
//!    ответ. У клиента свой парсер (`snens-web/src/lib/artifact.ts`), у тула
//!    `genui` свой; третий, в обработчике команды, был бы третьей версией
//!    грамматики. Здесь нужен минимум: заголовок фенса;
//! 2. **учёт ответов** — `answered()` сканирует журнал. Скан дешёвый (журнал
//!    читается и так), и главное — он не держит состояния: после рестарта
//!    поверхности повторный ответ тоже отвергается.
//!
//! Грамматика целиком — `docs/artifact.md`.

use crate::log::Rec;

/// Ссылка на артефакт: номер записи журнала плюс имя из заголовка фенса.
///
/// Имя уникально внутри сообщения, а не сессии, поэтому пара нужна целиком:
/// «вопрос 1 артефакта quiz» и «вопрос 1 артефакта poll» — разные вопросы.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactRef {
    pub seq: u64,
    pub id: String,
}

/// Имя артефакта из открывающей строки фенса. `None` — это не фенс артефакта.
fn artifact_id(line: &str) -> Option<Option<String>> {
    let rest = line.trim_start().strip_prefix("```")?;
    let rest = rest.trim_start_matches('`');
    let mut parts = rest.split_whitespace();
    if parts.next()? != "artifact" {
        return None;
    }
    for token in parts {
        if let Some(id) = token.strip_prefix("id=") {
            let ok = !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if ok {
                return Some(Some(id.to_string()));
            }
        }
    }
    // Фенс без `id` — анонимный артефакт; клиент зовёт его `#1`.
    Some(None)
}

/// Артефакт, на который ссылается ответ.
///
/// `seq` — физическая строка журнала (как в истории), поэтому запись ищется по
/// номеру. Берётся первый артефакт записи: ответы на второй и дальше в одном
/// сообщении — редкость, а порядок совпадает с нумерацией клиента (`#1`, `#2`).
pub fn find(log: &[Rec], seq: u64) -> Option<ArtifactRef> {
    let rec = log.get(seq.checked_sub(1)? as usize)?;
    let Rec::Assistant { text, .. } = rec else {
        return None;
    };
    let text = text.as_deref()?;
    for line in text.lines() {
        if let Some(id) = artifact_id(line) {
            return Some(ArtifactRef {
                seq,
                id: id.unwrap_or_else(|| "#1".into()),
            });
        }
    }
    None
}

/// Сколько вопросов `snens.ask` объявлено в тексте ответа.
///
/// Поиск по подстроке, а не разбор JS: цифра нужна для сводки ревью
/// («артефактов N, без ответа M»), а ложный плюс здесь дешевле, чем целый
/// JS-парсер в ядре. Вопрос из цикла посчитается один раз — тоже приемлемо:
/// важно заметить системную проблему, а не сосчитать точно.
pub fn count_questions(text: &str) -> usize {
    if !text.contains("artifact") {
        return 0;
    }
    text.matches("snens.ask").count()
}

/// Отвеченные вопросы сессии: ключи `seq:artifact:id`.
///
/// Скан по журналу, а не по состоянию поверхности: ответ, полученный до
/// рестарта, обязан блокировать повтор и после него.
pub fn answered(log: &[Rec]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for rec in log {
        if let Rec::User {
            ask: Some(ask), ..
        } = rec
        {
            out.insert(format!("{}:{}:{}", ask.seq, ask.artifact, ask.id));
        }
    }
    out
}

/// Сводка по вопросам артефактов — для строки в разборе ревью.
pub struct AskStats {
    /// Сколько вопросов задано (по `snens.ask` в текстах модели).
    pub asked: usize,
    /// Сколько на них ответили (по записям с `ask`).
    pub answered: usize,
}

impl AskStats {
    pub fn of(log: &[Rec]) -> Self {
        let mut asked = 0usize;
        let mut answered = 0usize;
        for rec in log {
            match rec {
                Rec::Assistant { text, .. } => {
                    asked += count_questions(text.as_deref().unwrap_or(""));
                }
                Rec::User {
                    ask: Some(_), ..
                } => answered += 1,
                _ => {}
            }
        }
        Self { asked, answered }
    }

    /// Строка для разбора: `вопросов 3, без ответа 1`. Пустая — вопросов не было.
    pub fn line(&self) -> String {
        if self.asked == 0 && self.answered == 0 {
            return String::new();
        }
        let pending = self.asked.saturating_sub(self.answered);
        format!("вопросов артефактов {}, без ответа {pending}", self.asked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::AskRef;

    fn assistant(text: &str) -> Rec {
        Rec::Assistant {
            ts: 0,
            text: Some(text.into()),
            tool_calls: Vec::new(),
            reasoning: None,
        }
    }

    #[test]
    fn finds_artifact_by_fence_head() {
        let log = vec![
            Rec::User {
                ts: 0,
                text: "нарисуй квиз".into(),
                source: "surface".into(),
                parts: Vec::new(),
                ask: None,
            },
            assistant("вот:\n```artifact id=quiz\n<div></div>\n```\nготово"),
        ];
        assert_eq!(
            find(&log, 2),
            Some(ArtifactRef {
                seq: 2,
                id: "quiz".into()
            })
        );
    }

    #[test]
    fn anonymous_artifact_gets_positional_name() {
        let log = vec![assistant("```artifact\n<div></div>\n```")];
        assert_eq!(
            find(&log, 1),
            Some(ArtifactRef {
                seq: 1,
                id: "#1".into()
            })
        );
    }

    #[test]
    fn ordinary_code_fence_is_not_an_artifact() {
        let log = vec![assistant("```html\n<b>x</b>\n```")];
        assert_eq!(find(&log, 1), None);
    }

    #[test]
    fn record_that_is_not_assistant_has_no_artifact() {
        let log = vec![Rec::User {
            ts: 0,
            text: "```artifact id=q".into(),
            source: "surface".into(),
            parts: Vec::new(),
            ask: None,
        }];
        assert_eq!(find(&log, 1), None, "текст человека — не артефакт");
    }

    #[test]
    fn answered_keys_are_unique_per_artifact_and_question() {
        let ask = |seq: u64, artifact: &str, id: &str| Rec::User {
            ts: 0,
            text: "2".into(),
            source: "artifact".into(),
            parts: Vec::new(),
            ask: Some(AskRef {
                seq,
                artifact: artifact.into(),
                id: id.into(),
                question: "какой вариант?".into(),
            }),
        };
        let log = vec![ask(7, "quiz", "q1"), ask(7, "quiz", "q2")];
        let set = answered(&log);
        assert_eq!(set.len(), 2);
        assert!(set.contains("7:quiz:q1"));
        assert!(set.contains("7:quiz:q2"));
    }

    #[test]
    fn stats_count_questions_and_answers() {
        let log = vec![
            assistant("```artifact id=q\n<script>snens.ask({id:'a'})</script>\n```"),
            Rec::User {
                ts: 0,
                text: "1".into(),
                source: "artifact".into(),
                parts: Vec::new(),
                ask: Some(AskRef {
                    seq: 1,
                    artifact: "q".into(),
                    id: "a".into(),
                    question: String::new(),
                }),
            },
        ];
        let s = AskStats::of(&log);
        assert_eq!(s.asked, 1);
        assert_eq!(s.answered, 1);
        assert_eq!(s.line(), "вопросов артефактов 1, без ответа 0");
        assert_eq!(AskStats::of(&[]).line(), "");
    }
}
