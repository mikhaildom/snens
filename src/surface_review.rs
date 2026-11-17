// Авторевью сессии после хода: порог ходов, простой, поколение работы.
//
// Вынесено из запуска хода: это не часть хода, а отдельный таймер, который
// обязан пережить сам ход и не выстрелить посреди следующего.

use crate::cfg::Config;
use crate::surface_runtime::{Runtime, Session};
use std::sync::Arc;
use tokio::time::Duration;

/// Пора ли запускать авторевью: набрался порог ходов и с прошлого ревью добавлено
/// не меньше порога новых ходов. Простой (`review_idle_seconds`) проверяется
/// отдельно таймером; эта функция — только счётная часть.
pub(super) fn review_due(min_turns: usize, turns: usize, last_review_turns: u64) -> bool {
    min_turns > 0
        && turns >= min_turns
        && (turns as u64).saturating_sub(last_review_turns) >= min_turns as u64
}

/// Число ходов сессии (ответов ассистента) по её JSONL.
///
/// Ошибка чтения — это `None`, а не `0`: ноль означал бы «ходов нет», и
/// `review_due` никогда бы не сработал — авторевью выключалось бы навсегда без
/// единого признака, что журнал не читается.
pub(super) fn session_turns(cfg: &Config, id: &str) -> Option<usize> {
    let path = cfg.sessions_path().join(format!("{id}.jsonl"));
    let records = crate::log::read_file(&path).ok()?;
    Some(
        records
            .iter()
            .filter(|r| matches!(r, crate::log::Rec::Assistant { .. }))
            .count(),
    )
}

/// Планирует авторевью после конца хода. Ревью запускается только при двух
/// условиях сразу: набралось `review_min_turns` ходов и сессия простояла
/// `review_idle_seconds` без новой работы. Таймер запоминает поколение работы:
/// если за время простоя начался новый ход, ревью отменяется — оно дождётся
/// следующей паузы. `review_idle_seconds = 0` выключает авторевью.
///
/// Граница разбора — число ходов, а не смещение в логе: `review_due` требует
/// `review_min_turns` НОВЫХ ходов с прошлого запуска. Маркер (`Rec::Review`) в
/// журнал пишется для человека и для следующего прогона, но порог считается по
/// `Assistant`, поэтому сама постановка маркера счёт не сдвигает.
pub(super) fn schedule_review(s: Arc<Runtime>, id: String) {
    // Пороги — из файла, а не из снимка старта: они есть в меню настроек, и
    // обещание «действует со следующего хода» должно быть правдой. Планировщик
    // стартует на конце хода, так что правка успевает вступить в силу.
    let live = s.cfg.live();
    let idle = live.review_idle_seconds;
    if idle == 0 {
        return;
    }
    let min = live.review_min_turns;
    let sched = s.clone();
    let sid = id.clone();
    // Поколение снимается ЗДЕСЬ, до `tokio::spawn`: задача не исполняется в
    // момент `spawn`, и если следующий ход успевал поднять поколение до её
    // первого опроса, таймер брал уже новое значение и считал его своим —
    // отмена по изменению поколения переставала работать.
    tokio::spawn(async move {
        let gen = {
            let mut gens = sched.review_gen.lock().await;
            *gens.entry(sid.clone()).or_insert(0)
        };
        drop(s);
        let s = sched;
        // Планировщик стартует на конце хода, а не на его старте: поколение только
        // что установлено. Ждём простой.
        tokio::time::sleep(Duration::from_secs(idle)).await;
        // Простой нарушен — новая работа перезапустит планировщик сама.
        if s.review_gen.lock().await.get(&sid).copied().unwrap_or(0) != gen {
            return;
        }
        // Сессия всё ещё занята? Планировщик хода снял бы поколение, но перестрахуемся.
        if s.data
            .lock()
            .await
            .sessions
            .get(&sid)
            .map(|x| x.state != "idle")
            .unwrap_or(false)
        {
            return;
        }
        // Полный разбор мегабайтного журнала — не на async-воркере.
        let cfg = s.cfg.clone();
        let read_id = sid.clone();
        let turns = match tokio::task::spawn_blocking(move || session_turns(&cfg, &read_id)).await {
            Ok(Some(turns)) => turns,
            Ok(None) => {
                s.emit(
                    &sid,
                    None,
                    "notice",
                    serde_json::json!({"text":"журнал не прочитан: авторевью отложено"}),
                )
                .await
                .ok();
                return;
            }
            Err(_) => return,
        };
        // Проверка порога и резервирование — в ОДНОМ замке: раньше `review_due`
        // считался по одной копии `reviews`, а запись шла вторым захватом, и два
        // таймера одной сессии могли оба пройти проверку и запустить два ревью.
        let reserved = {
            let mut d = s.data.lock().await;
            let last = d.reviews.get(&sid).copied().unwrap_or(0);
            if !review_due(min, turns, last) {
                None
            } else {
                // Записываем порог до запуска: даже если процесс ревью не стартует,
                // повтор немедленно не случится — только после роста числа ходов.
                d.reviews.insert(sid.clone(), turns as u64);
                Some(turns as u64)
            }
        };
        let Some(turns) = reserved else {
            return;
        };
        // Запись — без замка store.
        if let Err(error) = s.save_review(&sid, turns) {
            // Не глотаем: без строки в stderr «порог откатился после рестарта»
            // выглядит как загадочно повторное ревью той же ревизии.
            eprintln!("[snens] порог ревью сессии {sid} не записан: {error}");
        }
        // Маркер в журнал сессии — до запуска процесса: `review::run` при своём
        // старте видит уже готовую границу, а если процесс не поднимется, в
        // транскрипте останется честная строка «здесь планировалось ревью».
        // Ошибку записи не глотаем молча, но и ревью из-за неё не отменяем: сам
        // разбор полезнее строки-отметки.
        let mark_path = s.cfg.sessions_path().join(format!("{sid}.jsonl"));
        let mark = match s.writers.lock().await.get(&sid).cloned() {
            Some(writer) => writer.append(crate::review::mark(&sid, turns)),
            None if mark_path.is_file() => crate::log::Session::open(&s.cfg.sessions_path(), &sid)
                .and_then(|sess| sess.append(crate::review::mark(&sid, turns))),
            None => Ok(0),
        };
        if let Err(error) = mark {
            s.emit(
                &sid,
                None,
                "notice",
                serde_json::json!({"text":format!("маркер ревью не записан: {error}")}),
            )
            .await
            .ok();
        }
        if let Err(error) = crate::spawn_review(&s.cfg, &sid) {
            s.emit(
                &sid,
                None,
                "notice",
                serde_json::json!({"text":format!("не удалось запустить авторевью: {error}")}),
            )
            .await
            .ok();
            return;
        }
        s.emit(
            &sid,
            None,
            "notice",
            serde_json::json!({"text":"запущено саморевью сессии (фоном)"}),
        )
        .await
        .ok();
    });
}

/// Порог уборки контекста для запуска: per-session значение сессии, если оно
/// задано, иначе общее из конфига. Вынесено из `launch` ради теста: развилка
/// «сессия диктует свой бюджет или живёт на общем» должна быть проверяемой.
pub(super) fn session_cleanup_limits(cfg: &Config, session: Option<&Session>) -> (u64, u64) {
    let budget = session
        .and_then(|s| s.context_budget)
        .filter(|b| *b > 0)
        .unwrap_or(cfg.context_budget);
    let target = session
        .and_then(|s| s.context_target)
        .filter(|t| *t > 0)
        .unwrap_or(cfg.context_target);
    (budget, target)
}
