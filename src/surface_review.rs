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
    min_turns > 0 && turns >= min_turns && (turns as u64).saturating_sub(last_review_turns) >= min_turns as u64
}

/// Число ходов сессии (ответов ассистента) по её JSONL.
pub(super) fn session_turns(cfg: &Config, id: &str) -> usize {
    let path = cfg.sessions_path().join(format!("{id}.jsonl"));
    crate::log::read_file(&path)
        .map(|rs| {
            rs.iter()
                .filter(|r| matches!(r, crate::log::Rec::Assistant { .. }))
                .count()
        })
        .unwrap_or(0)
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
    tokio::spawn(async move {
        let gen = s.review_gen.lock().await.get(&id).copied().unwrap_or(0);
        // Планировщик стартует на конце хода, а не на его старте: поколение только
        // что установлено. Ждём простой.
        tokio::time::sleep(Duration::from_secs(idle)).await;
        // Простой нарушен — новая работа перезапустит планировщик сама.
        if s.review_gen.lock().await.get(&id).copied().unwrap_or(0) != gen {
            return;
        }
        // Сессия всё ещё занята? Планировщик хода снял бы поколение, но перестрахуемся.
        if s.data
            .lock()
            .await
            .sessions
            .get(&id)
            .map(|x| x.state != "idle")
            .unwrap_or(false)
        {
            return;
        }
        let turns = session_turns(&s.cfg, &id);
        let last = s.data.lock().await.reviews.get(&id).copied().unwrap_or(0);
        if !review_due(min, turns, last) {
            return;
        }
        // Записываем порог до запуска: даже если процесс ревью не стартует, повтор
        // немедленно не случится — только после нового роста числа ходов.
        {
            let mut d = s.data.lock().await;
            d.reviews.insert(id.clone(), turns as u64);
            let _ = s.save_review(&id, turns as u64);
        }
        // Маркер в журнал сессии — до запуска процесса: `review::run` при своём
        // старте видит уже готовую границу, а если процесс не поднимется, в
        // транскрипте останется честная строка «здесь планировалось ревью».
        // Ошибку записи не глотаем молча, но и ревью из-за неё не отменяем: сам
        // разбор полезнее строки-отметки.
        let mark_path = s.cfg.sessions_path().join(format!("{id}.jsonl"));
        let mark = match s.writers.lock().await.get(&id).cloned() {
            Some(writer) => writer.append(crate::review::mark(&id, turns as u64)),
            None if mark_path.is_file() => crate::log::Session::open(&s.cfg.sessions_path(), &id)
                .and_then(|sess| sess.append(crate::review::mark(&id, turns as u64))),
            None => Ok(0),
        };
        if let Err(error) = mark {
            s.emit(
                &id,
                None,
                "notice",
                serde_json::json!({"text":format!("маркер ревью не записан: {error}")}),
            )
            .await
            .ok();
        }
        if let Err(error) = crate::spawn_review(&s.cfg, &id) {
            s.emit(
                &id,
                None,
                "notice",
                serde_json::json!({"text":format!("не удалось запустить авторевью: {error}")}),
            )
            .await
            .ok();
            return;
        }
        s.emit(
            &id,
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
