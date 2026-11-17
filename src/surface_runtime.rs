use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::sync::{broadcast, mpsc, watch, Mutex};

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Session {
    pub id: String,
    pub surface: String,
    /// Человекочитаемый заголовок. Сгенерированный (моделью или ядром) лежит в
    /// `Meta.title`; пока его нет, discovery подставляет первые слова первого
    /// сообщения пользователя. Пусто — ни того, ни другого.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Время последней активности (unix, сек): `mtime` лога сессии. Список
    /// сортируется по нему — поверхность и порядок id тут не при чём.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    /// Сессия-родитель, если это подагент. Пусто у пользовательских сессий.
    /// Позволяет поверхности отличить работу подагента от разговора с человеком.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Модель, которой реально шёл ход: из `Meta.model`, иначе из последней
    /// записи `usage`/`assistant` журнала. У старых логов `Meta.model` пуст.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub state: String,
    pub run_id: Option<String>,
    /// Экономический бюджет уборки контекста для этой сессии; пусто — общий
    /// из конфига. Нужен поверхностям с иной ценой контекста: групповой чат
    /// живёт долго и просит 100k, тогда как TUI-сессии держат свои 500k.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<u64>,
    /// Цель после уборки для этой сессии; пусто — общая из конфига.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_target: Option<u64>,
    /// Сколько ходов модели уже прошло (ответов ассистента в журнале). Для
    /// подагентов это главная цифра прогресса: по ней видно, что ребёнок жив
    /// и работает, не открывая его сессию.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u64>,
    /// Последняя запись `Done` в журнале: `ok` | `max_turns` | ... Пусто —
    /// сессия ещё не закрывалась. Нужна, чтобы отличить «закончил» от «упал».
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<String>,
    /// Когда подагент начал работу: unix-время первой записи журнала. Вместе с
    /// `done_at` даёт длительность — клиент рисует «работает 4 мин» или
    /// «заняло 12 мин», не опрашивая ядро повторно.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// Когда подагент закрылся (запись `Done`). Пусто — ещё работает.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Event {
    pub seq: u64,
    pub session_id: String,
    pub run_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub fields: serde_json::Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub command_id: String,
    pub text: String,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    pub state: String,
    pub run_id: Option<String>,
    /// Ответ на вопрос артефакта (`snens.ask`): ссылка на артефакт и вопрос.
    /// Пишется в журнал при старте хода и едет в `Rec::User.ask`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<crate::log::AskRef>,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Attachment {
    pub attachment_id: String,
    pub command_id: String,
    pub filename: String,
    pub name: String,
    pub part: crate::log::Part,
    pub sha256: String,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    pub sessions: BTreeMap<String, Session>,
    pub events: BTreeMap<String, Vec<Event>>,
    pub messages: BTreeMap<String, Vec<Message>>,
    #[serde(default)]
    pub attachments: BTreeMap<String, BTreeMap<String, Attachment>>,
    pub commands: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub surface_tools: BTreeMap<String, Vec<serde_json::Value>>,
    #[serde(default)]
    pub tool_results: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub tool_requests: BTreeMap<String, serde_json::Value>,
    /// Сколько ходов было в сессии на момент последнего запуска авторевью.
    /// Нужно, чтобы ревью не повторялось, пока не наберётся ещё `review_min_turns`.
    #[serde(default)]
    pub reviews: BTreeMap<String, u64>,
}
impl Session {
    /// Подагент ли это: у него есть родитель. Признак берётся из `parent`
    /// (`Meta` журнала) — store его не хранит.
    pub fn is_child(&self) -> bool {
        self.parent.as_deref().is_some_and(|p| !p.trim().is_empty())
    }

    /// Новая сессия поверхности без per-session бюджета: пороги берутся из конфига.
    pub fn new(id: impl Into<String>, surface: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            surface: surface.into(),
            title: None,
            updated_at: None,
            parent: None,
            model: None,
            state: "idle".into(),
            run_id: None,
            context_budget: None,
            context_target: None,
            turns: None,
            done: None,
            started_at: None,
            done_at: None,
        }
    }
}

pub struct Control {
    pub cancel: watch::Sender<bool>,
    pub queued: mpsc::UnboundedSender<crate::agent::QueuedInput>,
}
pub struct Runtime {
    pub data: Mutex<Store>,
    pub db: StdMutex<Connection>,
    pub tx: broadcast::Sender<Event>,
    /// Снимок конфига. Модель здесь — только стартовая: `agent::run_loop`
    /// перечитывает её из файла на каждом ходу, поэтому правка конфига во время
    /// жизни сервера видна новым ходам без перезапуска.
    pub cfg: crate::cfg::Config,
    pub llm: Arc<crate::llm::OpenAi>,
    pub locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    pub controls: Mutex<BTreeMap<String, Control>>,
    pub pending_tools:
        Mutex<BTreeMap<String, tokio::sync::oneshot::Sender<crate::agent::SurfaceToolResult>>>,
    /// Exclusive JSONL writer handles. The log lock is held for each Session lifetime.
    pub writers: Mutex<BTreeMap<String, Arc<crate::log::Session>>>,
    pub event_seq: Mutex<BTreeMap<String, u64>>,
    /// Поколение работы сессии: растёт на каждом старте хода. Отложенный таймер
    /// авторевью запоминает поколение и отменяется, если оно изменилось — тогда
    /// за время простоя началась новая работа и ревью ждёт следующей паузы.
    pub review_gen: Mutex<BTreeMap<String, u64>>,
    /// Ожидающие решения заявки на привилегированное действие: id → канал ответа.
    /// Живут только в памяти: заявка одноразовая и короткоживущая, рестарт её
    /// отменяет (шим получит отказ).
    pub approvals: Mutex<BTreeMap<String, tokio::sync::oneshot::Sender<bool>>>,
    /// Действующее приглашение пейринга (docs/pairing.md). Отдельная ячейка, а
    /// не поле store: код живёт минуты, в SQLite ему делать нечего, а рестарт
    /// ядра его законно отменяет.
    pub pending: crate::pairing::Pending,
    /// Подключённые устройства: проверка токена по реестру, а не по одному файлу.
    pub registry: crate::clients::Registry,
    /// Поток изменений списка сессий (`GET /v1/events`): клиент делает upsert
    /// вместо опроса `GET /v1/sessions`. Несёт уже собранный JSON события:
    /// разный вид у разных подписчиков не нужен.
    pub tx_updates: tokio::sync::broadcast::Sender<serde_json::Value>,
}
impl Runtime {
    /// Соединение с SQLite под замком.
    ///
    /// Отравленный замок (`PoisonError`) не паникует: он значит, что паника уже
    /// случилась в другом потоке, а нам нужна база такой, какая есть. Падать
    /// здесь — это 500 без тела на каждой записи сессии после первой аварии.
    /// Вызывать только из синхронных `save_*`/`persist_*`: под замком ни одного
    /// `await`, иначе async-воркер встал бы вместе с базой.
    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // Полная перезапись store (`persist_store`) нужна только там, где меняется
    // всё сразу: старт (миграция legacy `store.json`), shutdown, пересборка
    // после `interrupt_running`. В горячем пути — никогда: она переписывает все
    // таблицы, и цена растёт вместе с объёмом истории.

    /// Записать одну сессию (upsert).
    pub fn save_session(&self, s: &Session) -> crate::R<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO sessions(id,surface,state,run_id,context_budget,context_target,parent)\n             VALUES(?1,?2,?3,?4,?5,?6,?7)\n             ON CONFLICT(id) DO UPDATE SET surface=excluded.surface,state=excluded.state,\n               run_id=excluded.run_id,context_budget=excluded.context_budget,\n               context_target=excluded.context_target,parent=excluded.parent",
            params![
                s.id,
                s.surface,
                s.state,
                s.run_id,
                s.context_budget,
                s.context_target,
                s.parent
            ],
        )?;
        Ok(())
    }

    /// Записать одно событие и сдвинуть курсор сессии — одной транзакцией.
    /// Записать в историю одно событие и обновить курсор сессии.
    pub fn save_event(&self, e: &Event) -> crate::R<()> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO events(session_id,seq,run_id,kind,fields) VALUES(?1,?2,?3,?4,?5)",
            params![
                e.session_id,
                e.seq as i64,
                e.run_id,
                e.kind,
                serde_json::to_string(&e.fields)?
            ],
        )?;
        tx.execute(
            "INSERT INTO event_sequences(session_id,seq) VALUES(?1,?2)\n             ON CONFLICT(session_id) DO UPDATE SET seq=excluded.seq",
            params![e.session_id, e.seq as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Сохранить только курсор, без самого события.
    ///
    /// Нужен транзитным событиям (`assistant.delta`): они не пишутся в историю
    /// (дёшево и незачем), но номер из общего счётчика всё равно занимают. Если
    /// этот номер не сохранить, после падения ядра счётчик откатится к последнему
    /// уцелевшему и номера начнут переиспользоваться — клиент с курсором
    /// 12 перестанет видеть события 13, 14… пока счётчик не перегонит его.
    /// Одна строчка апдейта без транзакции: транзитных событий много, полная
    /// транзакция на каждую дельту стоит дороже самой дельты.
    pub fn save_cursor(&self, session: &str, seq: u64) -> crate::R<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO event_sequences(session_id,seq) VALUES(?1,?2)\n             ON CONFLICT(session_id) DO UPDATE SET seq=excluded.seq",
            params![session, seq as i64],
        )?;
        Ok(())
    }

    /// Удалить все строки сессии.
    pub fn drop_session(&self, id: &str) -> crate::R<()> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        for table in [
            "message_attachments",
            "attachments",
            "messages",
            "events",
            "surface_tools",
            "reviews",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE session_id=?1"),
                params![id],
            )?;
        }
        tx.execute(
            "DELETE FROM event_sequences WHERE session_id=?1",
            params![id],
        )?;
        tx.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Записать одну сессию и её сообщения — без полной перезаписи store.
    ///
    /// Вызывать его приходится после того, как async-замок `data` уже отпущен
    /// (блокирующий SQLite под ним держал бы всех, кому нужен store). Поэтому
    /// снимок нужной сессии клонируется под замком, а пишется — вне его.
    /// Берёт `&Store` целиком больше не нужен: полная перезапись на каждый ход —
    /// это O(объём истории), а нужна запись одной сессии.
    pub fn save_session_and_messages(
        &self,
        id: &str,
        session: Option<&Session>,
        messages: &[Message],
    ) -> crate::R<()> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        if let Some(s) = session {
            tx.execute(
                "INSERT INTO sessions(id,surface,state,run_id,context_budget,context_target,parent)\n                 VALUES(?1,?2,?3,?4,?5,?6,?7)\n                 ON CONFLICT(id) DO UPDATE SET surface=excluded.surface,state=excluded.state,\n                   run_id=excluded.run_id,context_budget=excluded.context_budget,\n                   context_target=excluded.context_target,parent=excluded.parent",
                params![
                    s.id,
                    s.surface,
                    s.state,
                    s.run_id,
                    s.context_budget,
                    s.context_target,
                    s.parent
                ],
            )?;
        }
        tx.execute(
            "DELETE FROM message_attachments WHERE session_id=?1",
            params![id],
        )?;
        tx.execute("DELETE FROM messages WHERE session_id=?1", params![id])?;
        for (seq, m) in messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages(session_id,id,command_id,seq,text,state,run_id,ask)\n                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    id,
                    m.id,
                    m.command_id,
                    seq as i64,
                    m.text,
                    m.state,
                    m.run_id,
                    m.ask.as_ref().map(serde_json::to_string).transpose()?
                ],
            )?;
            for (aseq, aid) in m.attachment_ids.iter().enumerate() {
                // Вложение могло не попасть в базу (legacy-импорт, упавшая
                // запись). FK-нарушение не гасится `OR IGNORE` — проверяем
                // строку сами, иначе одно битое вложение роняло бы всю
                // транзакцию сообщений (500 на ходе).
                let exists: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM attachments WHERE session_id=?1 AND id=?2",
                    params![id, aid],
                    |r| r.get(0),
                )?;
                if exists == 0 {
                    continue;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO message_attachments VALUES(?1,?2,?3,?4)",
                    params![id, m.id, aid, aseq as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Мелкие справочники: одна строка — одна запись.
    pub fn save_command(&self, command_id: &str, payload: &serde_json::Value) -> crate::R<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO commands VALUES(?1,?2)",
            params![command_id, serde_json::to_string(payload)?],
        )?;
        Ok(())
    }

    pub fn save_surface_tools(&self, id: &str, defs: &[serde_json::Value]) -> crate::R<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO surface_tools VALUES(?1,?2)",
            params![id, serde_json::to_string(defs)?],
        )?;
        Ok(())
    }

    pub fn save_tool_result(&self, key: &str, payload: &serde_json::Value) -> crate::R<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO tool_results VALUES(?1,?2)",
            params![key, serde_json::to_string(payload)?],
        )?;
        Ok(())
    }

    pub fn save_tool_request(&self, key: &str, payload: &serde_json::Value) -> crate::R<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO tool_requests VALUES(?1,?2)",
            params![key, serde_json::to_string(payload)?],
        )?;
        Ok(())
    }

    pub fn save_review(&self, id: &str, turns: u64) -> crate::R<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO reviews VALUES(?1,?2)",
            params![id, turns as i64],
        )?;
        Ok(())
    }

    pub fn save_attachment(&self, id: &str, a: &Attachment) -> crate::R<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO attachments VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                id,
                a.attachment_id,
                a.command_id,
                a.filename,
                a.name,
                serde_json::to_string(&a.part)?,
                a.sha256
            ],
        )?;
        Ok(())
    }

    /// Собрать событие и отдать его без записи в базу — запись делает вызывающий.
    ///
    /// `emit` пишет SQLite прямо под замком `data`, а это fsync на блокирующем
    /// замке: каждый emit держал и воркер, и store (все хендлеры стоят
    /// в очереди). Поэтому порядок такой: под замком только последовательность
    /// и память, потом запись — уже без `data`. Курсор двигается в памяти там же,
    /// где берётся номер, так что пара «событие + курсор» остаётся согласованной.
    pub async fn build_event(
        &self,
        id: &str,
        run: Option<String>,
        kind: &str,
        fields: serde_json::Value,
    ) -> Event {
        let mut d = self.data.lock().await;
        let seq = {
            let mut sequences = self.event_seq.lock().await;
            let next = sequences.get(id).copied().unwrap_or_else(|| {
                d.events
                    .get(id)
                    .and_then(|events| events.iter().map(|event| event.seq).max())
                    .unwrap_or(0)
            }) + 1;
            sequences.insert(id.to_string(), next);
            next
        };
        let e = Event {
            seq,
            session_id: id.into(),
            run_id: run,
            kind: kind.into(),
            fields,
        };
        // Дельта в историю не идёт, но занятый ею номер — идёт: иначе после
        // падения ядра нумерация откатится и повторит уже доставленные номера,
        // а клиент с курсором молча перестанет получать события.
        if e.kind != "assistant.delta" {
            d.events.entry(id.into()).or_default().push(e.clone());
        }
        e
    }

    /// Дописать событие в базу и разослать его. Вызывать без замка `data`.
    pub async fn publish_event(&self, e: Event) {
        let saved = if e.kind != "assistant.delta" {
            self.save_event(&e)
        } else {
            self.save_cursor(&e.session_id, e.seq)
        };
        if let Err(error) = saved {
            eprintln!(
                "[snens] событие {} сессии {} не записано: {error}",
                e.kind, e.session_id
            );
        }
        let _ = self.tx.send(e.clone());
        // Карточка подагента — отдельным шагом и своим сигналом: у ребёнка
        // событий много, а меняется из них немногое. Само решение «это
        // подагент» принимается по слоту store, без чтения журнала на каждую
        // дельту (см. `refresh_child`).
        self.refresh_child(&e.session_id).await;
    }

    ///
    /// Инвариант: `emit` берёт мьютекс store сам, поэтому вызывать его ПОД
    /// `self.data.lock()` нельзя — это взаимная блокировка (tokio-мьютекс не
    /// реентрантен). Порядок такой: собрать данные под замком, отпустить,
    /// потом `emit`. Отдельно: `emit` → `refresh_child` → снова `data`, и это
    /// безопасно ровно потому, что к моменту вызова замок уже отпущен.
    pub async fn emit(
        &self,
        id: &str,
        run: Option<String>,
        kind: &str,
        fields: serde_json::Value,
    ) -> crate::R<Event> {
        let e = self.build_event(id, run, kind, fields).await;
        self.publish_event(e.clone()).await;
        Ok(e)
    }

    /// Пересчитать запись подагента по журналу и pid и, если карточке есть что
    /// перерисовать, опубликовать её в поток списка.
    ///
    /// Вынесено из `emit` отдельным шагом: событий у ребёнка много (дельты идут
    /// пачками по 32), а меняется при этом только счётчик ходов и состояние —
    /// кадр на каждую дельту был бы теми же данными тридцать два раза.
    pub async fn refresh_child(&self, id: &str) {
        // Дешёвая проверка до любого чтения журнала: признак «подагент» уже
        // лежит в слоте store. Раньше `session_by_id` звался на КАЖДОЕ событие
        // (включая дельты) и до этой проверки — обычная сессия платила чтением
        // головы журнала за каждую дельту хода.
        let parent_known = {
            let d = self.data.lock().await;
            match d.sessions.get(id) {
                Some(slot) => Some(slot.is_child()),
                // Слота нет — решение возможно только по журналу.
                None => None,
            }
        };
        if parent_known == Some(false) {
            return;
        }
        // Чтение журнала — блокирующее: не на async-воркере.
        let cfg = self.cfg.clone();
        let read_id = id.to_string();
        let fresh = tokio::task::spawn_blocking(move || {
            crate::surface_server::discovery::session_by_id(&cfg, &read_id)
        })
        .await
        .ok()
        .flatten();
        let Some(mut fresh) = fresh else {
            return;
        };
        // Подагент ли это — по журналу (`Meta.parent`), а не по store: в store
        // запись могла лечь до того, как метка появилась, и признак там пустой.
        if !fresh.is_child() {
            return;
        }
        let changed = {
            let mut d = self.data.lock().await;
            let before = match d.sessions.get(id) {
                Some(x) => (x.turns, x.done.clone(), x.state.clone()),
                None => (None, None, String::new()),
            };
            if let Some(slot) = d.sessions.get_mut(id) {
                // `state` берём из журнала и pid (это и есть правда про
                // подагента: работает/готов/прерван), из слота — только то,
                // чего в журнале нет. Раньше здесь перетиралось состояние, и SSE
                // говорил «running» там, где `GET /v1/sessions` уже отдавал «idle».
                fresh.run_id = slot.run_id.clone();
                fresh.title = slot.title.clone().or(fresh.title);
                // `build_session` строит запись с нуля (`context_budget`/
                // `context_target` там всегда `None`). Без переноса из слота
                // пересборка карточки стирала бюджет сессии и в памяти, и в
                // SQLite — уборка контекста у подагента теряла свой порог.
                fresh.context_budget = slot.context_budget;
                fresh.context_target = slot.context_target;
            }
            let after = (fresh.turns, fresh.done.clone(), fresh.state.clone());
            let snapshot = fresh.clone();
            d.sessions.insert(id.to_string(), fresh);
            (before, after, snapshot)
        };
        let (before, after, snapshot) = changed;
        // Запись — без async-замка: блокирующий SQLite под ним держал всё.
        let _ = self.save_session(&snapshot);
        if before != after {
            self.publish_session(snapshot);
        }
    }

    /// Разослать подписчикам свежую запись списка сессий. Отдельный канал, а не
    /// шина событий транскрипта: у тех `session_id` один, а тут меняется сам
    /// список.
    /// Опубликовать карточку сессии в `GET /v1/events`.
    ///
    /// Не `async`: внутри только `broadcast::send`, а он синхронный. Раньше был
    /// `async` без единого `await` — и вызывался под мьютексом store
    /// (`message`), где любой лишний `await` означал бы `Send`-обязательства
    /// под замком. Функции здесь нечего ждать.
    pub fn publish_session(&self, session: Session) {
        let _ = self.tx_updates.send(serde_json::json!({
            "type": "session.updated",
            "session": session,
        }));
    }

    pub async fn session_lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut m = self.locks.lock().await;
        m.entry(id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

fn schema(c: &Connection) -> crate::R<()> {
    // WAL: запись события не блокирует читателей (SSE, список сессий), а
    // `DELETE`-журнал на каждый коммит переписывал страницы целиком.
    let _: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    c.execute_batch("PRAGMA foreign_keys=ON;
      CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,surface TEXT NOT NULL,state TEXT NOT NULL,run_id TEXT,context_budget INTEGER,context_target INTEGER);
      CREATE TABLE IF NOT EXISTS messages(session_id TEXT NOT NULL,id TEXT NOT NULL,command_id TEXT NOT NULL UNIQUE,seq INTEGER NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL,run_id TEXT,ask TEXT,PRIMARY KEY(session_id,id),FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE TABLE IF NOT EXISTS attachments(session_id TEXT NOT NULL,id TEXT NOT NULL,command_id TEXT NOT NULL,filename TEXT NOT NULL,name TEXT NOT NULL,part TEXT NOT NULL,sha256 TEXT NOT NULL,PRIMARY KEY(session_id,id),FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE UNIQUE INDEX IF NOT EXISTS attachments_command ON attachments(session_id,command_id);
      CREATE TABLE IF NOT EXISTS message_attachments(session_id TEXT NOT NULL,message_id TEXT NOT NULL,attachment_id TEXT NOT NULL,seq INTEGER NOT NULL,PRIMARY KEY(session_id,message_id,attachment_id),FOREIGN KEY(session_id,message_id) REFERENCES messages(session_id,id),FOREIGN KEY(session_id,attachment_id) REFERENCES attachments(session_id,id));
      CREATE TABLE IF NOT EXISTS commands(command_id TEXT PRIMARY KEY,payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS surface_tools(session_id TEXT PRIMARY KEY,definitions TEXT NOT NULL,FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE TABLE IF NOT EXISTS tool_results(action_id TEXT PRIMARY KEY,payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS tool_requests(action_id TEXT PRIMARY KEY,payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS reviews(session_id TEXT PRIMARY KEY,turns INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS events(session_id TEXT NOT NULL,seq INTEGER NOT NULL,run_id TEXT,kind TEXT NOT NULL,fields TEXT NOT NULL,PRIMARY KEY(session_id,seq),FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE TABLE IF NOT EXISTS event_sequences(session_id TEXT PRIMARY KEY,seq INTEGER NOT NULL);")?;
    // Старые store.sqlite3 созданы до per-session бюджета: колонки добавляем
    // отдельно, чтобы обновление бинарника не требовало пересоздания store.
    // `parent` — признак подагента. Без него store не отличал ребёнка от
    // пользовательской сессии, и обновление карточки молча не срабатывало.
    for column in [
        "context_budget INTEGER",
        "context_target INTEGER",
        "parent TEXT",
    ] {
        if let Err(error) = c.execute(&format!("ALTER TABLE sessions ADD COLUMN {column}"), []) {
            if !error
                .to_string()
                .to_lowercase()
                .contains("duplicate column")
            {
                return Err(error.into());
            }
        }
    }
    // `ask` — ответ на вопрос артефакта (`snens.ask`): в базах, созданных до
    // этой колонки, её нет, и без миграции повтор после рестарта считался бы
    // другим payload'ом (409 вместо идемпотентного ответа).
    if let Err(error) = c.execute("ALTER TABLE messages ADD COLUMN ask TEXT", []) {
        if !error
            .to_string()
            .to_lowercase()
            .contains("duplicate column")
        {
            return Err(error.into());
        }
    }
    Ok(())
}
pub fn persist_store(c: &Connection, d: &Store) -> crate::R<()> {
    persist(c, d)
}
fn persist(c: &Connection, d: &Store) -> crate::R<()> {
    let tx = c.unchecked_transaction()?;
    tx.execute_batch(
        "DELETE FROM events; DELETE FROM tool_results; DELETE FROM tool_requests; DELETE FROM surface_tools; DELETE FROM reviews; DELETE FROM message_attachments; DELETE FROM attachments; DELETE FROM messages; DELETE FROM commands; DELETE FROM sessions;",
    )?;
    for s in d.sessions.values() {
        tx.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                s.id,
                s.surface,
                s.state,
                s.run_id,
                s.context_budget,
                s.context_target,
                s.parent
            ],
        )?;
    }
    for (sid, ms) in &d.messages {
        for (seq, m) in ms.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    sid,
                    m.id,
                    m.command_id,
                    seq as i64,
                    m.text,
                    m.state,
                    m.run_id,
                    m.ask.as_ref().map(serde_json::to_string).transpose()?
                ],
            )?;
        }
    }
    for (sid, by_id) in &d.attachments {
        for a in by_id.values() {
            tx.execute(
                "INSERT INTO attachments VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    sid,
                    a.attachment_id,
                    a.command_id,
                    a.filename,
                    a.name,
                    serde_json::to_string(&a.part)?,
                    a.sha256
                ],
            )?;
        }
    }
    for (sid, ms) in &d.messages {
        for m in ms {
            for (seq, attachment_id) in m.attachment_ids.iter().enumerate() {
                tx.execute(
                    "INSERT INTO message_attachments VALUES(?1,?2,?3,?4)",
                    params![sid, m.id, attachment_id, seq as i64],
                )?;
            }
        }
    }
    for (sid, es) in &d.events {
        for e in es {
            tx.execute(
                "INSERT INTO events VALUES(?1,?2,?3,?4,?5)",
                params![
                    sid,
                    e.seq as i64,
                    e.run_id,
                    e.kind,
                    serde_json::to_string(&e.fields)?
                ],
            )?;
        }
    }
    for (id, payload) in &d.commands {
        tx.execute(
            "INSERT INTO commands VALUES(?1,?2)",
            params![id, serde_json::to_string(payload)?],
        )?;
    }
    for (sid, defs) in &d.surface_tools {
        tx.execute(
            "INSERT INTO surface_tools VALUES(?1,?2)",
            params![sid, serde_json::to_string(defs)?],
        )?;
    }
    for (id, payload) in &d.tool_results {
        tx.execute(
            "INSERT INTO tool_results VALUES(?1,?2)",
            params![id, serde_json::to_string(payload)?],
        )?;
    }
    for (id, payload) in &d.tool_requests {
        tx.execute(
            "INSERT INTO tool_requests VALUES(?1,?2)",
            params![id, serde_json::to_string(payload)?],
        )?;
    }
    for (id, turns) in &d.reviews {
        tx.execute(
            "INSERT INTO reviews VALUES(?1,?2)",
            params![id, *turns as i64],
        )?;
    }
    tx.commit()?;
    Ok(())
}
pub fn event_sequences(c: &Connection) -> crate::R<BTreeMap<String, u64>> {
    let mut sequences = BTreeMap::new();
    let mut query = c.prepare("SELECT session_id,seq FROM event_sequences")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
    })? {
        let (id, seq) = row?;
        sequences.insert(id, seq);
    }
    Ok(sequences)
}
fn read_store(c: &Connection) -> crate::R<Store> {
    let mut d = Store::default();
    {
        let mut q = c
            .prepare("SELECT id,surface,state,run_id,context_budget,context_target,parent FROM sessions ORDER BY id")?;
        for row in q.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                surface: r.get(1)?,
                // Заголовок не хранится в store: он выводится из лога при
                // discovery, чтобы правка Meta.title не требовала миграции.
                title: None,
                updated_at: None,
                parent: r.get(6)?,
                model: None,
                state: r.get(2)?,
                run_id: r.get(3)?,
                context_budget: r.get(4)?,
                context_target: r.get(5)?,
                // В store не хранятся: выводятся из журнала при discovery.
                turns: None,
                done: None,
                started_at: None,
                done_at: None,
            })
        })? {
            let s = row?;
            d.sessions.insert(s.id.clone(), s);
        }
    }
    {
        let mut q=c.prepare("SELECT session_id,id,command_id,text,state,run_id,ask FROM messages ORDER BY session_id,seq")?;
        for row in q.query_map([], |r| {
            let sid: String = r.get(0)?;
            let mid: String = r.get(1)?;
            // `ask` — часть payload'а идемпотентности (`same`-сравнение в
            // `surface_commands::message`). Пока колонки не было, после рестарта
            // повтор ответа на артефакт сравнивал `Some(..)` с `None` и получал
            // 409 `idempotency_conflict` вместо идемпотентного ответа.
            let ask: Option<String> = r.get(6)?;
            Ok((
                sid,
                Message {
                    ask: ask.and_then(|raw| serde_json::from_str(&raw).ok()),
                    id: mid,
                    command_id: r.get(2)?,
                    text: r.get(3)?,
                    state: r.get(4)?,
                    run_id: r.get(5)?,
                    attachment_ids: Vec::new(),
                },
            ))
        })? {
            let (sid, m) = row?;
            d.messages.entry(sid).or_default().push(m);
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,message_id,attachment_id FROM message_attachments ORDER BY session_id,message_id,seq")?;
        for row in q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (sid, mid, aid) = row?;
            if let Some(m) = d
                .messages
                .get_mut(&sid)
                .and_then(|ms| ms.iter_mut().find(|m| m.id == mid))
            {
                m.attachment_ids.push(aid);
            }
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,id,command_id,filename,name,part,sha256 FROM attachments ORDER BY session_id,id")?;
        for row in q.query_map([], |r| {
            let part: String = r.get(5)?;
            Ok((
                r.get::<_, String>(0)?,
                Attachment {
                    attachment_id: r.get(1)?,
                    command_id: r.get(2)?,
                    filename: r.get(3)?,
                    name: r.get(4)?,
                    part: serde_json::from_str(&part).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                    sha256: r.get(6)?,
                },
            ))
        })? {
            let (sid, attachment) = row?;
            d.attachments
                .entry(sid)
                .or_default()
                .insert(attachment.attachment_id.clone(), attachment);
        }
    }
    {
        let mut q = c.prepare(
            "SELECT session_id,seq,run_id,kind,fields FROM events ORDER BY session_id,seq",
        )?;
        for row in q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Event {
                    session_id: r.get(0)?,
                    seq: r.get::<_, i64>(1)? as u64,
                    run_id: r.get(2)?,
                    kind: r.get(3)?,
                    fields: serde_json::from_str(&r.get::<_, String>(4)?)
                        .unwrap_or(serde_json::Value::Null),
                },
            ))
        })? {
            let (sid, e) = row?;
            d.events.entry(sid).or_default().push(e);
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,definitions FROM surface_tools")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (sid, p) = row?;
            d.surface_tools.insert(sid, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT action_id,payload FROM tool_requests")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, p) = row?;
            d.tool_requests.insert(id, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT action_id,payload FROM tool_results")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, p) = row?;
            d.tool_results.insert(id, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT command_id,payload FROM commands")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, p) = row?;
            d.commands.insert(id, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,turns FROM reviews")?;
        for row in q.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })? {
            let (id, turns) = row?;
            d.reviews.insert(id, turns);
        }
    }
    Ok(d)
}
pub fn open_store(dir: &Path) -> crate::R<(Store, Connection)> {
    fs::create_dir_all(dir)?;
    let db = Connection::open(dir.join("store.sqlite3"))?;
    schema(&db)?;
    let count: i64 = db.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    if count == 0 {
        let legacy = dir.join("store.json");
        if legacy.exists() {
            let old: Store = serde_json::from_slice(&fs::read(&legacy)?)?;
            persist(&db, &old)?;
            fs::rename(&legacy, dir.join("store.json.imported"))?;
        }
    }
    Ok((read_store(&db)?, db))
}
pub fn interrupt_running(store: &mut Store, sequences: &mut BTreeMap<String, u64>) {
    let running: Vec<String> = store
        .sessions
        .values()
        .filter(|s| s.state == "running" || s.state == "stopping")
        .map(|s| s.id.clone())
        .collect();
    for id in running {
        let run_id = store.sessions.get(&id).and_then(|s| s.run_id.clone());
        for (action_id, action) in store.tool_requests.iter_mut() {
            if action_id.starts_with(&format!(
                "{}/{}/",
                id,
                run_id.as_deref().unwrap_or_default()
            )) && action.get("status").and_then(|v| v.as_str()) == Some("pending")
            {
                *action = serde_json::json!({"status":"cancelled"});
            }
        }
        if let Some(session) = store.sessions.get_mut(&id) {
            session.state = "interrupted".into();
            session.run_id = None;
        }
        let events = store.events.entry(id.clone()).or_default();
        let seq = sequences
            .get(&id)
            .copied()
            .unwrap_or_else(|| events.iter().map(|event| event.seq).max().unwrap_or(0))
            + 1;
        sequences.insert(id.clone(), seq);
        events.push(Event {
            seq,
            session_id: id.clone(),
            run_id,
            kind: "run.finished".into(),
            fields: serde_json::json!({"status":"interrupted"}),
        });
        if let Some(messages) = store.messages.get_mut(&id) {
            for message in messages.iter_mut().filter(|m| m.state == "accepted") {
                message.state = "cancelled".into();
            }
        }
    }
}

pub fn valid_id(s: &str) -> bool {
    if uuid::Uuid::parse_str(s).is_ok() {
        return true;
    }
    let Some((stamp, pid)) = s.split_once('-') else {
        return false;
    };
    !stamp.is_empty()
        && !pid.is_empty()
        && !pid.contains('-')
        && stamp.bytes().all(|b| b.is_ascii_digit())
        && pid.bytes().all(|b| b.is_ascii_digit())
}
#[cfg(test)]
mod legacy_id_tests {
    use super::valid_id;
    #[test]
    fn accepts_production_legacy_ids_but_rejects_paths_and_suffixes() {
        assert!(valid_id("1791498455-70558"));
        assert!(valid_id("1791499422-79425"));
        assert!(valid_id("550e8400-e29b-41d4-a716-446655440000"));
        for invalid in [
            "../1791498455-70558",
            "1791498455-70558.jsonl",
            "1791498455-70558-review",
            "-70558",
            "1791498455-",
        ] {
            assert!(!valid_id(invalid), "{invalid}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("surface-{}", uuid::Uuid::new_v4()))
    }
    #[test]
    fn per_session_cleanup_limits_survive_persist_roundtrip() {
        let dir = temp();
        let (mut d, db) = open_store(&dir).unwrap();
        let mut s = Session::new("s", "telegram");
        s.context_budget = Some(100_000);
        s.context_target = Some(70_000);
        d.sessions.insert("s".into(), s);
        persist(&db, &d).unwrap();
        let got = read_store(&db).unwrap();
        assert_eq!(got.sessions["s"].context_budget, Some(100_000));
        assert_eq!(got.sessions["s"].context_target, Some(70_000));
        // Сессия без своего порога читается как None, а не как ноль.
        let mut plain = Session::new("p", "tui");
        plain.context_budget = None;
        d.sessions.insert("p".into(), plain);
        persist(&db, &d).unwrap();
        let got = read_store(&db).unwrap();
        assert_eq!(got.sessions["p"].context_budget, None);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_store_without_cleanup_columns_is_migrated() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        // Store старой сборки: колонок бюджета в sessions ещё нет.
        {
            let db = Connection::open(dir.join("store.sqlite3")).unwrap();
            db.execute_batch(
                "CREATE TABLE sessions(id TEXT PRIMARY KEY,surface TEXT NOT NULL,state TEXT NOT NULL,run_id TEXT);\
                 INSERT INTO sessions VALUES('s','telegram','idle',NULL);",
            )
            .unwrap();
        }
        let (store, db) = open_store(&dir).unwrap();
        assert_eq!(store.sessions["s"].context_budget, None);
        assert!(store.sessions["s"].context_target.is_none());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn emit_load_roundtrip_and_dedup_persist() {
        let dir = temp();
        let (mut d, db) = open_store(&dir).unwrap();
        let id = "s".to_string();
        d.sessions
            .insert(id.clone(), Session::new(id.clone(), "test"));
        d.surface_tools.insert(id.clone(),vec![serde_json::json!({"type":"function","function":{"name":"send_file","parameters":{"type":"object"}}})]);
        d.tool_requests
            .insert("s/r/c".into(), serde_json::json!({"status":"pending"}));
        d.commands
            .insert("cmd".into(), serde_json::json!({"ok":true}));
        d.messages.insert(
            id.clone(),
            vec![Message {
                id: "m".into(),
                command_id: "cmd".into(),
                text: "hello".into(),
                attachment_ids: Vec::new(),
                state: "applied".into(),
                run_id: None,
                ask: None,
            }],
        );
        d.events.insert(
            id.clone(),
            vec![Event {
                seq: 1,
                session_id: id,
                run_id: None,
                kind: "test".into(),
                fields: serde_json::json!({"x":1}),
            }],
        );
        persist(&db, &d).unwrap();
        let got = read_store(&db).unwrap();
        assert_eq!(got.events["s"][0].kind, "test");
        db.execute(
            "INSERT INTO event_sequences(session_id,seq) VALUES('s',9)",
            [],
        )
        .unwrap();
        assert_eq!(event_sequences(&db).unwrap()["s"], 9);
        assert_eq!(got.surface_tools["s"][0]["function"]["name"], "send_file");
        assert_eq!(got.tool_requests["s/r/c"]["status"], "pending");
        assert_eq!(got.messages["s"][0].command_id, "cmd");
        assert_eq!(got.commands["cmd"]["ok"], true);
        assert!(db
            .execute(
                "INSERT INTO messages VALUES('s','m2','cmd',1,'dup','accepted',NULL,NULL)",
                []
            )
            .is_err());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn restart_marks_running_interrupted_and_queued_cancelled() {
        let mut d = Store::default();
        d.sessions.insert(
            "s".into(),
            Session {
                run_id: Some("r".into()),
                state: "running".into(),
                ..Session::new("s", "test")
            },
        );
        d.tool_requests
            .insert("s/r/call".into(), serde_json::json!({"status":"pending"}));
        d.messages.insert(
            "s".into(),
            vec![Message {
                id: "m".into(),
                command_id: "c".into(),
                text: "queued".into(),
                attachment_ids: Vec::new(),
                state: "accepted".into(),
                run_id: Some("r".into()),
                ask: None,
            }],
        );
        let mut sequences = BTreeMap::from([("s".to_string(), 9)]);
        d.events.insert(
            "s".into(),
            vec![Event {
                seq: 4,
                session_id: "s".into(),
                run_id: None,
                kind: "assistant.delta".into(),
                fields: serde_json::Value::Null,
            }],
        );
        interrupt_running(&mut d, &mut sequences);
        assert_eq!(sequences["s"], 10);
        assert_eq!(d.sessions["s"].state, "interrupted");
        assert_eq!(d.tool_requests["s/r/call"]["status"], "cancelled");
        assert_eq!(d.messages["s"][0].state, "cancelled");
        assert_eq!(d.events["s"].last().unwrap().seq, 10);
        assert_eq!(d.events["s"][1].fields["status"], "interrupted");
    }
    #[test]
    fn attachment_metadata_and_message_references_roundtrip_in_order() {
        let dir = temp();
        let (mut store, db) = open_store(&dir).unwrap();
        store.sessions.insert("s".into(), Session::new("s", "test"));
        let part = |path: &str, bytes| crate::log::Part {
            kind: "image".into(),
            path: path.into(),
            mime: "image/png".into(),
            bytes,
            width: 12,
            height: 34,
        };
        let first = Attachment {
            attachment_id: "a1".into(),
            command_id: "cmd".into(),
            filename: "first.png".into(),
            name: "upload-1".into(),
            part: part("/session/first.png", 123),
            sha256: "hash-1".into(),
        };
        let second = Attachment {
            attachment_id: "a2".into(),
            command_id: "cmd-upload-2".into(),
            filename: "second.png".into(),
            name: "upload-2".into(),
            part: part("/session/second.png", 456),
            sha256: "hash-2".into(),
        };
        store.attachments.insert(
            "s".into(),
            [("a1".into(), first.clone()), ("a2".into(), second.clone())]
                .into_iter()
                .collect(),
        );
        store.messages.insert(
            "s".into(),
            vec![Message {
                id: "m".into(),
                command_id: "cmd".into(),
                text: "with files".into(),
                attachment_ids: vec!["a2".into(), "a1".into()],
                state: "applied".into(),
                run_id: None,
                // `ask` обязан пережить рестарт: он участвует в сравнении
                // payload'а при повторе команды.
                ask: Some(crate::log::AskRef {
                    seq: 3,
                    artifact: "quiz".into(),
                    id: "q".into(),
                    question: "какой вариант?".into(),
                }),
            }],
        );

        persist(&db, &store).unwrap();
        let loaded = read_store(&db).unwrap();
        assert_eq!(loaded.attachments["s"]["a1"], first);
        assert_eq!(loaded.attachments["s"]["a2"], second);
        assert_eq!(loaded.messages["s"][0].attachment_ids, ["a2", "a1"]);
        assert_eq!(
            loaded.messages["s"][0]
                .ask
                .as_ref()
                .map(|a| a.artifact.as_str()),
            Some("quiz"),
            "ask не пережил перезапись store"
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_record_parts_default_for_legacy_and_roundtrip() {
        use crate::log::Rec;

        let legacy = r#"{"kind":"user","ts":1,"text":"old","source":"surface"}"#;
        match serde_json::from_str::<Rec>(legacy).unwrap() {
            Rec::User { parts, .. } => assert!(parts.is_empty()),
            other => panic!("expected user record, got {other:?}"),
        }

        let record = Rec::User {
            ts: 2,
            text: "new".into(),
            source: "surface".into(),
            parts: vec![crate::log::Part {
                kind: "image".into(),
                path: "/session/image.png".into(),
                mime: "image/png".into(),
                bytes: 42,
                width: 640,
                height: 480,
            }],
            ask: None,
        };
        let decoded: Rec = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        match decoded {
            Rec::User { parts, .. } => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].path, "/session/image.png");
                assert_eq!(parts[0].bytes, 42);
                assert_eq!((parts[0].width, parts[0].height), (640, 480));
            }
            other => panic!("expected user record, got {other:?}"),
        }
    }

    #[test]
    fn migration_imports_json_and_renames_it() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("store.json"),
            serde_json::to_vec(&Store::default()).unwrap(),
        )
        .unwrap();
        let _ = open_store(&dir).unwrap();
        assert!(dir.join("store.json.imported").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
