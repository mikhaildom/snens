//! Сетевые вызовы: эмбеддинг запроса, реранкер, дистилляция хода.
//!
//! Чтение (recall) не должно зависеть от сети дважды: эмбеддинг запроса — один
//! вызов, реранк — второй, и оба падают мягко. Ход человека не ждёт памяти.

use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::time::Duration;

use crate::cfg::Cfg;

/// Клиент один на процесс: свой ему таймаут нужен разный (20/25/120 с), поэтому
/// кэшируем по секундам в маленькой таблице. Новый клиент на каждый вызов
/// выбрасывал пул соединений — каждый факт `flush` платил TCP+TLS рукопожатием.
fn client(timeout_s: u64) -> Result<Client, String> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<Vec<(u64, Client)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = cache.lock().map_err(|e| e.to_string())?;
    if let Some((_, c)) = guard.iter().find(|(t, _)| *t == timeout_s) {
        return Ok(c.clone());
    }
    let c = Client::builder()
        .timeout(Duration::from_secs(timeout_s))
        // Провайдер отвергает запросы без User-Agent (403), а reqwest по
        // умолчанию его не ставит.
        .user_agent("snens-memory/0.1")
        .build()
        .map_err(|e| e.to_string())?;
    guard.push((timeout_s, c.clone()));
    Ok(c)
}

/// Эмбеддинг одной строки. `None` — сеть/провайдер отказал: recall продолжается
/// на одном BM25, это хуже, но не ноль.
pub fn embed(cfg: &Cfg, text: &str) -> Option<Vec<f32>> {
    let body = json!({"model": cfg.embed_model, "input": [text]});
    let resp = client(20)
        .ok()?
        .post(format!("{}/embeddings", cfg.api_base))
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().ok()?;
    let arr = v["data"][0]["embedding"].as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        out.push(x.as_f64()? as f32);
    }
    if out.len() != cfg.embed_dim {
        return None;
    }
    Some(out)
}

/// Эмбеддинги пачкой: запись идёт фоном, лишний круг к провайдеру не жалко,
/// но пачка дешевле и быстрее.
pub fn embed_batch(cfg: &Cfg, texts: &[String]) -> Option<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Some(Vec::new());
    }
    let body = json!({"model": cfg.embed_model, "input": texts});
    let resp = client(120)
        .ok()?
        .post(format!("{}/embeddings", cfg.api_base))
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .map_err(|e| eprintln!("memory: embeddings: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        eprintln!("memory: embeddings: статус {}", resp.status());
        return None;
    }
    let v: Value = resp.json().ok()?;
    let arr = v["data"].as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let vec = item["embedding"].as_array()?;
        // Размерность и числа проверяем так же строго, как в `embed`: вызывающие
        // пишут вектор в базу с `cfg.embed_dim`, и молчаливое дополнение нулями
        // дало бы порченые строки.
        if vec.len() != cfg.embed_dim {
            eprintln!(
                "memory: embeddings: размерность {} вместо {}",
                vec.len(),
                cfg.embed_dim
            );
            return None;
        }
        let mut row = Vec::with_capacity(vec.len());
        for x in vec {
            let f = x.as_f64()? as f32;
            if !f.is_finite() {
                return None;
            }
            row.push(f);
        }
        out.push(row);
    }
    (out.len() == texts.len()).then_some(out)
}

/// Инт8-квантование: 1 байт на измерение вместо 4. Для ранжирования кандидатов
/// этого достаточно, точный скор считает реранкер.
pub fn quantize(v: &[f32]) -> Vec<u8> {
    // Масштаб — максимум модуля: у нормализованных эмбеддингов он мал, и
    // фиксированный 127 растянул бы шум.
    // NaN/inf в компонентах — испорченный вектор: max стал бы NaN или inf,
    // и весь вектор ушёл бы в нули. Такой факт лучше не векторизовать.
    let max = v
        .iter()
        .filter(|x| x.is_finite())
        .fold(0f32, |a, x| a.max(x.abs()));
    let k = if max > 0.0 { 127.0 / max } else { 0.0 };
    v.iter()
        .map(|x| ((x * k).round().clamp(-127.0, 127.0) as i8) as u8)
        .collect()
}

/// Скоры реранкера по кандидатам, в порядке документов. `None` — реранк не
/// настроен или отказал: тогда решает RRF.
pub fn rerank(cfg: &Cfg, query: &str, docs: &[&str]) -> Option<Vec<f32>> {
    let model = cfg.rerank_model.as_ref()?;
    let key = cfg.rerank_key()?;
    if docs.is_empty() {
        return Some(Vec::new());
    }
    let body = json!({"model": model, "query": query, "documents": docs});
    let resp = client(25)
        .ok()?
        .post("https://openrouter.ai/api/v1/rerank")
        .bearer_auth(key)
        .json(&body)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().ok()?;
    let mut out = vec![f32::MIN; docs.len()];
    let mut got = 0usize;
    for item in v["results"].as_array()? {
        // Битый элемент не должен обнулять весь реранк: пропускаем его, остальные
        // оценки остаются. Индекс вне диапазона — тоже просто пропуск.
        let (Some(i), Some(score)) = (
            item["index"].as_u64().map(|i| i as usize),
            item["relevance_score"].as_f64(),
        ) else {
            eprintln!("memory: rerank: элемент без index/relevance_score пропущен");
            continue;
        };
        if let Some(slot) = out.get_mut(i) {
            *slot = score as f32;
            got += 1;
        }
    }
    if got == 0 {
        return None;
    }
    // Документы, которых провайдер не вернул, — в самый конец, а не «наравне с
    // нулём»: иначе настоящий нулевой скор неотличим от отсутствия ответа.
    Some(out)
}

/// Дистилляция хода в отдельные записи. Отдельная модель по той же причине,
/// что и у эмбеддинга: у записи свой лимит времени, чтение от неё не зависит.
/// Пустой список — законный ответ: в ходе нечего запоминать.
pub fn distill(
    cfg: &Cfg,
    user: &str,
    assistant: &str,
    known: &[String],
) -> Result<Vec<NewFact>, String> {
    let prompt = format!(
        "Ты извлекаешь долговременные факты из хода ассистента. Верни строго JSON.\n\
         Схема: {{\"facts\":[{{\"text\":\"...\",\"kind\":\"fact|preference|episode|procedure\",\"subject\":\"user|world|project\"}}]}}\n\
         Правила:\n\
         - text — одно самодостаточное утверждение по-русски, до 200 символов, без местоимений без антецедента.\n\
         - fact: проверяемый факт о мире/человеке/железе. preference: как человек хочет, чтобы делали. procedure: как делать повторяющуюся операцию. episode: короткое событие с датой.\n\
         - Только долговременное: то, что понадобится в будущих сессиях. Не писать: пересказ хода, временные состояния, ход мыслей, содержимое файлов.\n\
         - Не дублируй то, что уже есть в списке известного.\n\
         - Мало и точно: 0–5 фактов. Пустой список нормален.\n\n\
         Известное (может быть устаревшим):\n{}\n\n\
         Ход:\nUser: {}\nAssistant: {}",
        if known.is_empty() {
            "- (пусто)".to_string()
        } else {
            known
                .iter()
                .map(|k| format!("- {}", cut(k, 200)))
                .collect::<Vec<_>>()
                .join("\n")
        },
        cut(user, 2000),
        cut(assistant, 4000)
    );
    let body = json!({
        "model": cfg.distill_model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 700,
        "temperature": 0.1,
    });
    let resp = client(60)
        .map_err(|e| e.to_string())?
        .post(format!("{}/chat/completions", cfg.api_base))
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("distill: {}", resp.status()));
    }
    let v: Value = resp.json().map_err(|e| e.to_string())?;
    // Отсутствие content — не «нечего запоминать»: это сломанный ответ
    // провайдера, и ход должен остаться в очереди, а не пропасть.
    let text = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "distill: в ответе нет choices[0].message.content".to_string())?;
    parse_facts(text)
}

#[derive(Debug, Clone)]
pub struct NewFact {
    pub text: String,
    pub kind: String,
    pub subject: String,
}

/// Разбор ответа модели: JSON где-то в тексте (модели любят ```json).
fn parse_facts(text: &str) -> Result<Vec<NewFact>, String> {
    // Перебираем кандидатов от каждой `{`: в преамбуле модели бывает `{схема}`,
    // и первый сбалансированный объект — не обязательно полезный.
    let mut parsed: Option<Value> = None;
    for (pos, _) in text.match_indices('{') {
        let Some(slice) = balanced_object_at(text, pos) else {
            continue;
        };
        match serde_json::from_str::<Value>(&slice) {
            Ok(v) if v.get("facts").is_some() => {
                parsed = Some(v);
                break;
            }
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    let Some(v) = parsed else {
        return Err(format!(
            "distill: в ответе нет объекта с \"facts\": {}",
            cut(text.trim(), 200)
        ));
    };
    let mut out = Vec::new();
    for item in v["facts"].as_array().cloned().unwrap_or_default() {
        let text = item["text"].as_str().unwrap_or_default().trim().to_string();
        if text.len() < 8 {
            continue;
        }
        let kind = match item["kind"].as_str().unwrap_or("fact") {
            "preference" => "preference",
            "episode" => "episode",
            "procedure" => "procedure",
            _ => "fact",
        };
        let subject = match item["subject"].as_str().unwrap_or("world") {
            "user" => "user",
            "project" => "project",
            _ => "world",
        };
        out.push(NewFact {
            text: cut(&text, 400).to_string(),
            kind: kind.into(),
            subject: subject.into(),
        });
    }
    out.truncate(5);
    Ok(out)
}

/// Сбалансированный JSON-объект, начинающийся в `start`: скобки внутри строк не
/// считаем, экранирование учитываем. `find('{')..rfind('}')` ловил преамбулу с
/// фигурной скобкой и вторым объектом после полезного — и факты молча терялись.
fn balanced_object_at(text: &str, start: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Обрезка по символам, а не байтам: русский текст иначе рвётся пополам.
pub fn cut(s: &str, chars: usize) -> &str {
    match s.char_indices().nth(chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_keeps_direction() {
        let v = vec![0.5f32, -0.5, 0.0, 0.25];
        let q = quantize(&v);
        assert_eq!(q[0], 127u8);
        assert_eq!(q[1], (-127i8) as u8);
        assert_eq!(q[2], 0);
    }

    #[test]
    fn facts_parse_from_fenced_json() {
        let text = "вот:\n```json\n{\"facts\":[{\"text\":\"пользователь живёт в Нидерландах\",\"kind\":\"fact\",\"subject\":\"user\"},{\"text\":\"ок\"}]}\n```";
        let facts = parse_facts(text).expect("ответ разбирается");
        assert_eq!(facts.len(), 1, "короткий мусор не должен проходить");
        assert_eq!(facts[0].kind, "fact");
        assert_eq!(facts[0].subject, "user");
    }

    #[test]
    fn parse_takes_the_first_balanced_object() {
        // Перед JSON — фигурная скобка в преамбуле, после — второй объект:
        // прежний срез по первому и последнему `{`/`}` не разбирался вовсе.
        let text =
            "схема {a} → вот ответ: {\"facts\":[{\"text\":\"факт про питон и задачи\"}]} конец";
        let facts = parse_facts(text).expect("первый объект разобран");
        assert_eq!(facts.len(), 1);
        // Фигурные скобки внутри строк не должны ломать баланс.
        let text = "{\"facts\":[{\"text\":\"в тексте есть { и } и это нормально\"}]}";
        assert_eq!(parse_facts(text).unwrap().len(), 1);
    }

    #[test]
    fn missing_object_is_an_error_not_an_empty_list() {
        // «Нечего запоминать» приходит как {"facts":[]}; текст без JSON — сбой.
        assert!(parse_facts("я ничего не нашёл").is_err());
        assert!(parse_facts("{\"facts\":[]}").unwrap().is_empty());
    }

    #[test]
    fn cut_is_char_safe() {
        assert_eq!(cut("привет", 3), "при");
        assert_eq!(cut("при", 10), "при");
    }
}
