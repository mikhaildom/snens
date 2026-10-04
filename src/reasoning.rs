// Рассуждение провайдера в двух формах: обычная строка `delta.reasoning`
// (так отдают DeepSeek-подобные и Anthropic-совместимые) и список
// `delta.reasoning_details` (так отдаёт GPT-6 через OpenRouter). Во второй
// форме полный текст закрыт блобом — читается только сводка.
//
// Спецфайл не «на будущее»: без разбора `reasoning_details` агент на GPT-6
// молчит там, где модель потратила токены рассуждения, и `↓` в расходе выглядит
// дешевле, чем ход стоил на самом деле.

use serde_json::Value;

/// Разбор `delta.reasoning_details` — того, чем провайдер с зашифрованным
/// reasoning заменяет строку `delta.reasoning`.
///
/// Формат (`format: openai-responses-v1`) — список объектов двух видов:
/// `{"type":"reasoning.summary","summary":"…","index":0}` приходит кусками по
/// ходу стрима (индексы частей различаются), а в конце одним куском
/// `{"type":"reasoning.encrypted","data":"gAAA…"}`. Текста у блоба нет: это
/// подпись апстрима, которую клиент по контракту обязан вернуть обратно
/// дословно.
///
/// Возвращает сводку, если она была; `encrypted` — признак, что полный текст
/// хода нам не читаем, и дальше по цепочке его некуда положить.
pub fn take_details(d: &Value, encrypted: &mut bool) -> Option<String> {
    let items = d.get("reasoning_details")?.as_array()?;
    let mut out = String::new();
    for it in items {
        match it.get("type").and_then(|t| t.as_str()) {
            Some("reasoning.summary") => {
                if let Some(s) = it.get("summary").and_then(|s| s.as_str()) {
                    out.push_str(s);
                }
            }
            // Незнакомый тип с данными считаем блобом: лучше пометить ход как
            // «рассуждение закрыто», чем показать пустоту вместо платного текста.
            _ => *encrypted = true,
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // На вход — сам `delta` кадра: парсер работает по полям дельты, без
    // обвязки `choices` (её разбирает провайдерский слой выше).
    fn delta(v: Value) -> Value {
        v
    }

    #[test]
    fn summary_chunks_join_into_one_text() {
        let mut enc = false;
        let mut got = String::new();
        for chunk in ["Обдумываю ", "числа", "."] {
            if let Some(s) = take_details(
                &delta(json!({"reasoning": null, "reasoning_details": [
                    {"type": "reasoning.summary", "summary": chunk,
                     "format": "openai-responses-v1", "index": 0}
                ]})),
                &mut enc,
            ) {
                got.push_str(&s);
            }
        }
        assert_eq!(got, "Обдумываю числа.");
        assert!(!enc, "сводка — не блоб");
    }

    #[test]
    fn encrypted_blob_is_flagged_and_not_turned_into_text() {
        let mut enc = false;
        let got = take_details(
            &delta(json!({"reasoning": null, "reasoning_details": [
                {"type": "reasoning.summary", "summary": "**План**", "index": 0},
                {"type": "reasoning.encrypted", "data": "gAAAABq…",
                 "format": "openai-responses-v1", "id": "rs_1", "index": 1}
            ]})),
            &mut enc,
        );
        assert_eq!(got.as_deref(), Some("**План**"), "в блоб текста не попадает");
        assert!(enc, "блоб обязан быть помечен");
    }

    #[test]
    fn blob_only_frame_yields_nothing_but_still_marks_encrypted() {
        let mut enc = false;
        let got = take_details(
            &delta(json!({"reasoning_details": [
                {"type": "reasoning.encrypted", "data": "gAAA…"}
            ]})),
            &mut enc,
        );
        assert!(got.is_none());
        assert!(enc);
    }

    #[test]
    fn provider_without_details_is_ignored() {
        let mut enc = false;
        assert!(take_details(&delta(json!({"content": "привет"})), &mut enc).is_none());
        assert!(take_details(&delta(json!({"reasoning_details": []})), &mut enc).is_none());
        assert!(!enc);
    }
}