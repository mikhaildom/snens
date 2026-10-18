//! `memory` — долгая память агента: одна SQLite-база, без демона.
//!
//! Контракт — тот же argv-контракт, что знает ядро:
//!   `memory recall <текст>`   → строки фактов на stdout (пусто = нечего)
//!   `memory retain` (JSON на stdin) → ход в очередь на дистилляцию
//!   `memory flush`            → дистилляция очереди, слияние дублей
//!   `memory stats`            → что в базе
//!   `memory reindex`          → досчитать недостающие вектора
//!   `memory forget <uid>`     → закрыть факт
//!
//! Чтение: BM25 ∪ вектор → RRF → реранк → порог → не больше N фактов и M
//! токенов. Модель на этом пути не участвует: она сидит на записи, где ошибка
//! дешевле (запись можно откатить, мусор в префиксе — нет).

mod api;
mod cfg;
mod store;

use serde_json::Value;
use std::io::Read;

use cfg::Cfg;
use store::{Hit, Store};

const HELP: &str = "memory — долгая память агента (SQLite + FTS5 + sqlite-vec)\n\
memory recall <запрос> | memory retain | memory flush | memory stats | memory reindex | memory forget <uid>\n\
\n\
recall  — гибридный поиск: BM25 ∪ вектор → RRF → реранк → порог; по строке на факт.\n\
retain  — ход (JSON {\"user\",\"assistant\"} на stdin) в очередь на дистилляцию.\n\
flush   — разобрать очередь: извлечь факты, погасить противоречащие, слить дубли.\n\
stats   — сколько живых фактов, устаревших, что в очереди.\n\
reindex — посчитать вектора для фактов, записанных без провайдера.\n\
forget  — закрыть факт по uid (остаётся в таймлайне, но не выдаётся).\n\
\n\
Настройки: ~/.config/snens/memory.toml (db, api_base, embed_model, rerank,\n\
rerank_model, max_facts, max_tokens), ключ — SNENS_API_KEY в ~/.config/snens/.env.";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().map(String::as_str) else {
        eprintln!("{HELP}");
        std::process::exit(2);
    };
    if matches!(cmd, "--help" | "-h" | "help") {
        println!("{HELP}");
        return;
    }
    let code = match run(cmd, &argv[1..]) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("memory: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cmd: &str, args: &[String]) -> Result<i32, String> {
    let cfg = Cfg::load()?;
    // `forget` не требует сети; остальным нужна база.
    let store = Store::open(&cfg.db)?;
    match cmd {
        "recall" => recall(&cfg, &store, args),
        "retain" => retain(&cfg, &store),
        "flush" => flush(&cfg, &store),
        "stats" => stats(&store),
        "reindex" => reindex(&cfg, &store),
        "import" => import(&cfg, &store),
        "forget" => {
            let Some(uid) = args.first() else {
                return Err("forget: нужен uid".into());
            };
            println!("{}", if store.forget(uid)? { "закрыт" } else { "не найден" });
            Ok(0)
        }
        other => Err(format!("неизвестная команда {other}")),
    }
}

/// Порог отсева после реранка.
///
/// Абсолютный порог не переносим: у каждого реранкера своя шкала. Замерено на
/// живом корпусе (32 факта; 6 информативных запросов и 5 пустых):
///   nemotron-vl-1b: релевантное 0.03…0.89, мусор 0.002…0.014
///   cohere-v3.5   : релевантное 0.13…0.14, мусор 0.02…0.04
///   cohere-4-fast : релевантное 0.49, мусор 0.07…0.39 — абстеншна нет
/// Решает **относительный** порог от лучшего скора: `FLOOR` нужен только чтобы
/// на «погода завтра» не выдать мусор (тогда и лучший скор низкий).
const RERANK_RELATIVE: f32 = 0.4;
const RERANK_FLOOR: f32 = 0.02;

fn recall(cfg: &Cfg, store: &Store, args: &[String]) -> Result<i32, String> {
    let query = args.join(" ");
    if query.trim().is_empty() {
        return Err("recall: нужен запрос".into());
    }
    let cand = 30;
    let mut fused: Vec<(i64, f64, Option<f64>, Option<f64>)> = Vec::new();
    let by_vec = api::embed(cfg, &query)
        .map(|e| {
            let q = api::quantize(&e);
            store.vector_search(&q, cfg.embed_dim, cand).unwrap_or_default()
        })
        .unwrap_or_default();
    let by_text = store.bm25_search(&query, cand).unwrap_or_default();

    let rank_of = |list: &[(i64, f64)], id: i64| list.iter().position(|(x, _)| *x == id);
    let mut ids: Vec<i64> = by_vec.iter().map(|(i, _)| *i).collect();
    for (id, _) in &by_text {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    for id in ids {
        let vr = rank_of(&by_vec, id);
        let tr = rank_of(&by_text, id);
        let mut ranks = Vec::new();
        if let Some(r) = vr {
            ranks.push(r);
        }
        if let Some(r) = tr {
            ranks.push(r);
        }
        if ranks.is_empty() {
            continue;
        }
        fused.push((
            id,
            store::rrf(&ranks, 20.0),
            vr.map(|r| by_vec[r].1),
            tr.map(|r| by_text[r].1),
        ));
    }
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused.truncate(cand);

    let mut hits: Vec<Hit> = store
        .load(&fused.iter().map(|(id, _, _, _)| *id).collect::<Vec<_>>())?;
    // `load` отдаёт в порядке ids; uid — единственный общий ключ, поэтому
    // скоры кладём по позиции в `fused`.
    let uids = store.uids(&fused.iter().map(|(id, _, _, _)| *id).collect::<Vec<_>>())?;
    for h in hits.iter_mut() {
        if let Some(pos) = uids.iter().position(|u| *u == h.uid) {
            let (_, rrf, vec_d, bm25) = fused[pos];
            h.rrf = rrf;
            h.vec = vec_d;
            h.bm25 = bm25;
        }
    }
    hits.sort_by(|a, b| b.rrf.partial_cmp(&a.rrf).unwrap_or(std::cmp::Ordering::Equal));

    // Реранк: только он умеет отсекать «похоже, но не отвечает». Нет реранкера —
    // решает RRF, и тогда бюджет урезается жёстче (без порога мы не уверены).
    let docs: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
    let reranked = api::rerank(cfg, &query, &docs);
    if std::env::var("MEMORY_DEBUG").is_ok() {
        for (h, s) in hits.iter().zip(reranked.clone().unwrap_or_else(|| vec![-1.0; hits.len()])) {
            eprintln!("rerank={s:.3} rrf={:.4} {}", h.rrf, api::cut(&h.text, 60));
        }
    }
    let mut ranked: Vec<Hit> = match reranked {
        Some(scores) => {
            let best = scores.iter().cloned().fold(0f32, f32::max);
            let cutoff = (best * RERANK_RELATIVE).max(RERANK_FLOOR);
            hits.into_iter()
                .zip(scores)
                .filter(|(_, s)| *s >= cutoff)
                .map(|(mut h, s)| {
                    h.rerank = Some(s);
                    h
                })
                .collect()
        }
        None => {
            hits.truncate(cfg.max_facts.min(3));
            hits
        }
    };
    ranked.sort_by(|a, b| {
        let key = |h: &Hit| h.rerank.unwrap_or(h.rrf as f32);
        key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Бюджет: не больше N фактов и M «токенов» (символы/4 — оценка без токенизатора).
    let mut out = Vec::new();
    let mut used = 0usize;
    for h in ranked {
        let cost = h.text.chars().count() / 4 + 1;
        if out.len() >= cfg.max_facts || used + cost > cfg.max_tokens {
            break;
        }
        used += cost;
        out.push(h.text);
    }
    if out.is_empty() {
        // Пустая выдача — законный исход: лучше ничего, чем дистрактор
        // (один нерелевантный документ роняет точность сильнее, чем помогает
        // случайный; arXiv:2401.14887).
        return Ok(0);
    }
    for line in out {
        println!("- {line}");
    }
    Ok(0)
}

/// Ход в очередь. Ядро зовёт это после каждого ответа, поэтому здесь только
/// локальная вставка: сеть и LLM — в `flush`, который живёт отдельно.
fn retain(_cfg: &Cfg, store: &Store) -> Result<i32, String> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("stdin не JSON: {e}"))?;
    let field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let (user, assistant, context) = (field("user"), field("assistant"), field("context"));
    if user.is_empty() && assistant.is_empty() {
        return Err("retain: пустой ход".into());
    }
    if assistant.len() < 80 {
        // Короткий ответ ничего не добавляет к знанию о мире: экономит вызов.
        return Ok(0);
    }
    let id = store.enqueue(&user, &assistant, &context)?;
    println!("turn {id}");
    Ok(0)
}

fn flush(cfg: &Cfg, store: &Store) -> Result<i32, String> {
    let pending = store.pending(20)?;
    if pending.is_empty() {
        return Ok(0);
    }
    let mut added = 0;
    let mut superseded = 0;
    for (id, user, assistant, _context) in pending {
        // Противоречие ищем вектором: в промпт к дистилляции кладём ближайшие
        // живые факты, чтобы модель могла сказать «это перекрывает вот это».
        let emb = api::embed(cfg, &format!("{user}\n{assistant}"));
        let q = emb.as_ref().map(|e| api::quantize(e));
        let known = store
            .similar_for_prompt(q.as_deref(), cfg.embed_dim, 5)
            .unwrap_or_default();
        match api::distill(cfg, &user, &assistant) {
            Ok(facts) => {
                for fact in facts {
                    let emb = api::embed(cfg, &fact.text);
                    let quant = emb.as_ref().map(|e| api::quantize(e));
                    // Дедуп до вставки: слишком близкий живой факт — не новый.
                    let dup = quant
                        .as_ref()
                        .map(|q| {
                            store
                                .vector_search(q, cfg.embed_dim, 1)
                                .unwrap_or_default()
                                .first()
                                .map(|(_, d)| *d < 0.15)
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if dup {
                        continue;
                    }
                    let uid = store.insert(
                        &fact.text,
                        &fact.kind,
                        &fact.subject,
                        "auto",
                        "",
                        quant.as_deref(),
                        cfg.embed_dim,
                    )?;
                    // Перекрытие старым: модель уже видела список known; если
                    // текст нового факта прямо отрицает старый — гасим старый.
                    for (old_uid, old_text) in &known {
                        if contradicts(old_text, &fact.text) {
                            if store.supersede(old_uid, &uid)? {
                                superseded += 1;
                            }
                            break;
                        }
                    }
                    added += 1;
                }
                store.finish(id, "done")?;
            }
            Err(e) => {
                eprintln!("memory: flush {id}: {e}");
                store.finish(id, "failed")?;
            }
        }
    }
    println!("новых фактов: {added}, устарело: {superseded}");
    Ok(0)
}

/// Дешёвая проверка перекрытия: общая тема (≥2 общих слова ≥4 букв) и разная
/// «развязка». Это не понимание смысла — грубый фильтр, чтобы не плодить пары
/// там, где модель и так бы справилась; тонкое решение — за дистилляцией.
fn contradicts(old: &str, new: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 4)
            .map(|w| w.to_lowercase())
            .collect()
    };
    let (a, b) = (words(old), words(new));
    let common = a.iter().filter(|w| b.contains(w)).count();
    if common < 2 {
        return false;
    }
    // Общая тема, но у каждой стороны есть своё слово — кандидат в
    // «переехал/сменил». Сравнение — встречное, у каждой стороны свой остаток.
    let tail_a: Vec<&String> = a.iter().filter(|w| !b.contains(w)).collect();
    let tail_b: Vec<&String> = b.iter().filter(|w| !a.contains(w)).collect();
    !tail_a.is_empty() && !tail_b.is_empty()
}

/// Разовый импорт из другой системы: на stdin — JSON `{"company","units":[{"text","kind","subject","created"}]}`.
/// Нужен для миграции: старый банк не должен потеряться.
fn import(cfg: &Cfg, store: &Store) -> Result<i32, String> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("stdin не JSON: {e}"))?;
    let units = v["units"].as_array().cloned().unwrap_or_default();
    let mut texts = Vec::new();
    for u in &units {
        texts.push(u["text"].as_str().unwrap_or_default().to_string());
    }
    let mut added = 0;
    for chunk in texts.chunks(32) {
        let Some(vectors) = api::embed_batch(cfg, chunk) else {
            return Err("эмбеддинги недоступны".into());
        };
        for (i, text) in chunk.iter().enumerate() {
            if text.trim().is_empty() {
                continue;
            }
            let idx = added + i;
            let kind = units
                .get(idx)
                .and_then(|u| u["kind"].as_str())
                .unwrap_or("fact");
            let subject = units
                .get(idx)
                .and_then(|u| u["subject"].as_str())
                .unwrap_or("world");
            store.insert(
                text,
                kind,
                subject,
                "import",
                "",
                Some(&api::quantize(&vectors[i])),
                cfg.embed_dim,
            )?;
        }
        added += chunk.len();
    }
    println!("импортировано: {added}");
    Ok(0)
}

fn stats(store: &Store) -> Result<i32, String> {
    let s = store.stats()?;
    println!(
        "фактов: {}, устаревших: {}, в очереди: {}",
        s.facts, s.stale, s.pending
    );
    let kinds: Vec<String> = s.kinds.iter().map(|(k, n)| format!("{k}:{n}")).collect();
    if !kinds.is_empty() {
        println!("по типам: {}", kinds.join(", "));
    }
    Ok(0)
}

fn reindex(cfg: &Cfg, store: &Store) -> Result<i32, String> {
    let missing = store.missing_vectors(200)?;
    if missing.is_empty() {
        println!("все факты с векторами");
        return Ok(0);
    }
    let texts: Vec<String> = missing.iter().map(|(_, t)| t.clone()).collect();
    let Some(vectors) = api::embed_batch(cfg, &texts) else {
        return Err("эмбеддинги недоступны".into());
    };
    let mut n = 0;
    for ((id, _), vec) in missing.iter().zip(vectors) {
        store.store_vector(*id, &api::quantize(&vec), cfg.embed_dim)?;
        n += 1;
    }
    println!("досчитано векторов: {n}");
    Ok(0)
}

/// Время в секундах: у ядра `ts` — unix-секунды, таймлайн памяти живёт в той же шкале.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Идентификатор факта: короткий, стабильный, без зависимостей.
pub fn uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    format!("{:x}{:04x}{:x}", now(), nanos as u16 ^ (pid as u16), n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_is_unique_within_process() {
        let a = uid();
        let b = uid();
        assert_ne!(a, b);
    }

    #[test]
    fn contradiction_heuristic_is_conservative() {
        assert!(contradicts(
            "пользователь живёт в Нидерландах",
            "пользователь живёт в Германии"
        ));
        assert!(!contradicts(
            "пользователь живёт в Нидерландах",
            "VPN стоит в Финляндии"
        ));
        // Один и тот же факт — не противоречие (его отсекает дедуп по вектору).
        assert!(!contradicts(
            "VPN стоит в Финляндии",
            "VPN стоит в Финляндии"
        ));
    }
}
