// Конфиг: один TOML, ничего больше. Провайдерские квирки — через body_patch/headers,
// чтобы новый провайдер не требовал кода.
use std::{collections::BTreeMap, path::Path, path::PathBuf};

use serde::Deserialize;

use crate::{env_file, R};

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "d_base_url")]
    pub base_url: String,
    #[serde(default = "d_model")]
    pub model: String,
    /// Откуда конфиг прочитан. Нужен, чтобы перечитать `model` в рантайме:
    /// `snens model <slug>` правит файл, а долгоживущие процессы (сервер API)
    /// держат снимок и без этого не видят смены.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    /// Модель задана кодом, а не файлом (подагент на `subagent_model`):
    /// перечитывать её из конфига нельзя — файл про основную модель.
    #[serde(skip)]
    pub model_pinned: bool,
    /// Optional override for detached child agents, never the primary session.
    #[serde(default)]
    pub subagent_model: Option<String>,
    #[serde(default = "d_key_env")]
    pub key_env: String,
    /// Именованные профили провайдеров: `[providers.<имя>]`. Секции нет — работают
    /// top-level `base_url`/`model`/`key_env` (обратная совместимость). Есть
    /// активный профиль — он и перебивает top-level (см. `resolve_providers`).
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// Имя активного профиля. Пусто — профилей нет или ни один не выбран, и живём
    /// на top-level ключах.
    #[serde(default)]
    pub active_provider: String,
    /// Произвольные поля в тело запроса (quirks конкретного провайдера).
    #[serde(default)]
    pub body_patch: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "d_true")]
    pub stream_usage: bool,
    /// Размер окна модели в токенах (не хардкодим «1M у всех»).
    #[serde(default = "d_ctx")]
    pub ctx_window: u64,
    /// Экономический порог уборки в токенах; 0 — авто по размеру окна.
    #[serde(default)]
    pub context_budget: u64,
    /// Профили по модели: префикс slug → свои пороги уборки. Нужны потому, что
    /// цена кеша у моделей разная: дешёвый кеш терпит большой контекст,
    /// дорогой требует уборки раньше. Побеждает самый длинный префикс.
    #[serde(default)]
    pub models: BTreeMap<String, ModelProfile>,
    /// Сколько последних tool-результатов автоэлизия не трогает: модель работает
    /// именно с ними. Остальное — переваренный материал.
    #[serde(default = "d_keep_results")]
    pub keep_results: usize,
    /// Запас до физического предела окна; для маленьких окон не больше четверти.
    #[serde(default = "d_context_reserve")]
    pub context_reserve: u64,
    /// Цель после уборки в токенах; 0 — 70% эффективного бюджета.
    #[serde(default)]
    pub context_target: u64,
    /// Ступени давления в процентах бюджета: 50 и 75 — уборка просится дважды,
    /// раньше и мягче, чем по единственному порогу. Пусто — одна ступень по
    /// бюджету (прежнее поведение).
    #[serde(default = "d_context_pressure_tiers")]
    pub context_pressure_tiers: Vec<u64>,
    /// Минимальное сжатие правки зеркала в токенах. Не задано — 1% бюджета, но
    /// не меньше 2000: правка, которая ничего не сняла, — потерянный ход.
    /// `0` отключает проверку совсем (принимается любая валидная правка).
    #[serde(default)]
    pub context_min_savings: Option<u64>,
    /// Сколько символов одного tool-результата доезжает до модели.
    #[serde(default = "d_max_chars")]
    pub max_result_chars: usize,
    /// Таймаут одного `run` по умолчанию, секунды.
    #[serde(default = "d_timeout")]
    pub timeout_seconds: u64,
    /// Верхняя граница вывода процесса, символы.
    #[serde(default = "d_max_proc")]
    pub max_process_chars: usize,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default = "d_sessions")]
    pub sessions_dir: String,
    /// Каталог тулов: подкаталог = тул. Рядом с ним создаётся bin/ с симлинками.
    #[serde(default = "d_tools")]
    pub tools_dir: String,
    /// Сокращать вывод shell-команд (ANSI, длинные строки, повторы) до отправки модели.
    #[serde(default = "d_true")]
    pub reduce_output: bool,
    /// Мультимодальные вложения (канал `SNENS_ATTACH_DIR`). Выключено — ядро не
    /// даёт тулам канал и не отправляет части провайдеру: модель на тексте не
    /// должна получать 400 из-за картинки, которую заведомо не понимает.
    #[serde(default = "d_true")]
    pub attachments: bool,
    /// Потолок одного вложения, байты.
    #[serde(default = "d_max_attach")]
    pub max_attachment_bytes: usize,
    /// Сколько байт картинок уезжает в одном запросе.
    ///
    /// Провайдер ограничивает тело запроса (Nous: 30 МБ) и на превышении отвечает
    /// 413 — отказом не на картинку, а на весь ход. Сессию после этого не
    /// продолжить: проекция собирается заново с теми же файлами и снова не
    /// влезает. Поэтому бюджет — свойство проекции: свежие вложения едут, старые
    /// остаются пометкой с путём. `0` — картинки не отправляются вовсе.
    #[serde(default = "d_media_budget")]
    pub media_budget_bytes: usize,
    /// Оболочка для `run`: программа и её флаги. `sh -c` в коде не зашит —
    /// на Windows это `cmd /C`.
    #[serde(default = "d_shell")]
    pub shell: Vec<String>,
    #[serde(default = "d_docs_url")]
    pub docs_url: String,
    /// Каталог тикетов инстанса: публикуется код, а не история улучшений.
    #[serde(default = "d_tickets")]
    pub tickets_dir: String,
    /// Каталог скиллов: подкаталог = скилл (`SKILL.md` + `references/` + `scripts/`).
    /// У инстанса из исходников указывает в репозиторий: скилл описывает код, значит
    /// версионируется с ним и правится тем же коммитом.
    #[serde(default = "d_skills")]
    pub skills_dir: String,
    /// Сессия длиннее этого числа ходов — повод для ревью (порог по длине, не по цене).
    #[serde(default = "d_review_min")]
    pub review_min_turns: usize,
    /// Простой перед авторевью на поверхности: секунд тишины после конца хода.
    /// Ревью запускается, только если и порог ходов взят, и сессия простояла
    /// столько без новой работы. Ноль — таймер выключен (ревью не запускается).
    #[serde(default = "d_review_idle")]
    pub review_idle_seconds: u64,
    /// Сколько секунд headless-сессия ждёт решения по привилегированному действию.
    /// Шим `sudo` в этом окне ждёт ответа на сокете одобрений; истёк — отказ.
    /// Ноль — очередь одобрений выключена: шим сразу отдаёт sudo без пароля и тот
    /// честно падает (поведение до этой правки).
    #[serde(default = "d_approval_timeout")]
    pub approval_timeout_seconds: u64,
    /// Потолок ходов на один запуск цикла. Не бюджет по токенам, а защита от
    /// разгона; при достижении цикл не падает, а просит у модели промежуточный итог.
    #[serde(default = "d_max_turns")]
    pub max_turns: u32,
    /// Сколько секунд стрим провайдера может молчать между кадрами, прежде чем
    /// ход считается оборванным. Без этого залипший стрим вешает run навсегда
    /// (`Done` не пишется, состояние остаётся `running`, авторевью не срабатывает).
    #[serde(default = "d_stream_idle")]
    pub stream_idle_seconds: u64,
    /// Свой файл промпта ревью вместо вкомпилированного (для опытов).
    #[serde(default)]
    pub review_prompt: Option<String>,
    /// Каталог инстанса под git: промпт просит коммитить тикеты сразу — строку
    /// с командой берёт из memory.md инстанса, а не из текста продукта.
    #[serde(default)]
    pub instance_git: bool,
    /// Ящик сессий: канал между подагентом и родителем. Дефолт — рядом с сессиями,
    /// чтобы всё инстанса жило в одном месте.
    #[serde(default = "d_mail")]
    pub mail_dir: String,
    /// Долгая память: программа-провайдер, вызываемая как `<cmd> recall <текст>`
    /// и `<cmd> retain` (JSON хода на stdin). По умолчанию — штатный плагин
    /// `memory` из поставки; нет бинаря или `enabled = false` — канала нет, и
    /// ядро об этом знает только по argv-контракту.
    #[serde(default)]
    pub memory: Memory,
}

/// Профиль провайдера: куда стучаться и чем подписываться.
///
/// Ключа здесь нет — только имя переменной: config.toml уезжает в бэкап и git, а
/// значение лежит в `.env` рядом с конфигом (см. `env_file`). Модель — часть
/// профиля, потому что у провайдеров разный нейминг одних и тех же моделей:
/// переключение провайдера без модели отправило бы чужой slug.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Provider {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Имя переменной окружения с ключом. Пусто — наследуется общий `key_env`:
    /// профиль, заведённый руками, не должен требовать своей переменной.
    #[serde(default)]
    pub key_env: String,
}

impl Provider {
    /// Профиль пригоден для работы: без base_url и модели запрос всё равно не
    /// соберётся, и «активный» он был бы только на бумаге.
    pub fn is_complete(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// Пороги уборки для конкретной модели. Пустое поле = взять общий дефолт.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct ModelProfile {
    #[serde(default)]
    pub context_budget: u64,
    #[serde(default)]
    pub context_target: u64,
}

/// Как часто справка памяти впрыскивается в контекст. Разница не косметическая:
/// справка уезжает в префикс навсегда, поэтому `always` на простом чате стоит
/// ~3–4к токенов на каждое сообщение.
#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecallMode {
    /// Каждый ход (дефолт): свежая справка под текущий запрос. Плагин памяти сам
    /// держит справку компактной (`max_facts`/`max_tokens`), поэтому мусор от
    /// чужого банка больше не аргумент за экономию.
    #[default]
    Always,
    /// Одна справка на сессию: свежая только на первом ходу. Дешевле, но пустой
    /// реранк на первом (болтливом) ходу оставляет сессию без памяти вообще.
    Session,
    /// Не впрыскивать: поиск остаётся доступен модели вызовом тула `memory recall`.
    Off,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Memory {
    /// Имя программы в каталоге тулов (или путь, если со слэшем).
    #[serde(default)]
    pub cmd: String,
    /// Мастер-выключатель канала: `false` — ни recall, ни retain не зовутся,
    /// как будто `cmd` пуст. Правки подрежимов при этом сохраняются.
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Когда впрыскивать справку. Действует только при `enabled`.
    #[serde(default)]
    pub recall: RecallMode,
    /// Писать ли ход в банк. Не влияет на контекст, но это сетевой вызов на
    /// каждый ответ и запись в чужой стек — для простого чата его выключают.
    #[serde(default = "d_true")]
    pub retain: bool,
    /// Таймаут одного вызова, секунды: канал вспомогательный, ход его не ждёт.
    #[serde(default = "d_mem_timeout")]
    pub timeout_seconds: u64,
    /// Теги записи — свой канал в общем банке (у Гермеса свой).
    #[serde(default)]
    pub tag: Vec<String>,
}

/// Дефолт секции: память включена и указывает на штатный плагин `memory`.
/// Плагин — часть поставки (`tools/memory`), поэтому «чистая установка» его уже
/// имеет; выключают осознанно (`enabled = false`), а не по умолчанию.
/// Отсутствие бинаря — не ошибка: канала просто нет (`memory_bin` вернёт `None`).
impl Default for Memory {
    fn default() -> Self {
        Self {
            cmd: d_memory_cmd(),
            enabled: true,
            recall: RecallMode::Always,
            retain: true,
            timeout_seconds: d_mem_timeout(),
            tag: Vec::new(),
        }
    }
}

/// Имя штатного плагина памяти в каталоге тулов.
fn d_memory_cmd() -> String {
    "memory".into()
}

/// Провайдер не выбирается за человека: ключ у всех свой, а подставить
/// чужой base_url значит молча отправить ключ не туда. Пусто — ошибка с
/// подсказкой (см. `api_key`/`llm`), а не тихая попытка «куда-нибудь».
fn d_base_url() -> String {
    String::new()
}
fn d_model() -> String {
    String::new()
}
fn d_key_env() -> String {
    "SNENS_API_KEY".into()
}
fn d_true() -> bool {
    true
}
fn d_ctx() -> u64 {
    1_000_000
}
fn d_context_reserve() -> u64 {
    8_192
}
fn d_keep_results() -> usize {
    5
}
/// Ступени давления по умолчанию: уборка просится на 50% и на 75% бюджета.
fn d_context_pressure_tiers() -> Vec<u64> {
    vec![50, 75]
}
fn d_max_chars() -> usize {
    24_000
}
fn d_timeout() -> u64 {
    120
}
fn d_max_proc() -> usize {
    40_000
}
fn d_sessions() -> String {
    format!("{}/sessions", data_dir())
}
/// Тулы: если рядом с бинарником ядра лежит `tools/` (так их кладёт установщик
/// десктоп-приложения), берём оттуда — иначе из каталога данных (сборка из
/// исходников кладёт их в репозиторий, а `SNENS_DATA_DIR` указывает на него).
fn d_tools() -> String {
    if let Some(dir) = beside_exe("tools") {
        return dir;
    }
    format!("{}/tools", data_dir())
}

/// Каталог `name` рядом с исполняемым файлом ядра, если он там есть.
///
/// Нужно ровно для поставки: установленное приложение — read-only, писать тулы
/// в `%LOCALAPPDATA%` значило бы копировать бандл при каждом обновлении и
/// ловить рассинхрон «ядро новое, тулы старые».
pub fn beside_exe(name: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join(name);
    dir.is_dir().then(|| dir.to_string_lossy().into_owned())
}
fn d_max_attach() -> usize {
    32 * 1024 * 1024
}
/// Двадцать мегабайт на запрос — по **телу**: base64 в нём на треть больше файла.
/// Потолок Nous — 30 МБ содержимого картинок, и замер сломанной сессии дал 54.6 МБ
/// тела при 40 МБ файлов на диске. Запас оставлен, чтобы серия свежих скриншотов
/// не упиралась в отказ на ровном месте.
fn d_media_budget() -> usize {
    20 * 1024 * 1024
}
fn d_mail() -> String {
    format!("{}/mail", data_dir())
}
fn d_tickets() -> String {
    format!("{}/tickets", data_dir())
}
fn d_skills() -> String {
    format!("{}/skills", data_dir())
}
fn d_review_min() -> usize {
    20
}
/// Простой перед авторевью: 10 минут. Порог нужен потому, что 4 минуты дёргали
/// ревью слишком рано: сессия ещё жила, а следующее отодвигалось.
fn d_review_idle() -> u64 {
    600
}
/// Ожидание решения по sudo: 5 минут. Ход останавливается на этом действии,
/// поэтому окно не должно быть коротким, но вечно висеть тоже нельзя.
fn d_approval_timeout() -> u64 {
    300
}
fn d_max_turns() -> u32 {
    200
}
/// Молчание стрима дольше двух минут — уже не «модель думает»: соединение
/// живое, а данных нет (залипший апстрим). Пустая строка ответа? Нет — обрыв.
fn d_stream_idle() -> u64 {
    120
}
fn d_mem_timeout() -> u64 {
    60
}
fn d_docs_url() -> String {
    // Документация для агентов: плейнтекст, читается fetch'ем. Адрес в конфиге,
    // а не в коде: у чужой установки может быть свой (или никакого).
    "https://snens.svuvi.ch/llm/index.txt".into()
}
fn d_shell() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into()]
    } else {
        vec!["sh".into(), "-c".into()]
    }
}

/// Windows: USERPROFILE. unix (Linux, macOS, Termux): HOME.
pub fn home() -> String {
    for k in ["HOME", "USERPROFILE"] {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    ".".into()
}

/// Каталог данных: XDG на unix (`XDG_DATA_HOME`, иначе `~/.local/share`),
/// APPDATA на Windows. macOS идёт по unix-ветке: своя раскладка на каждую ОС
/// дороже, чем ноль, а `SNENS_DATA_DIR`/`SNENS_CONFIG_DIR` закрывают остальное
/// (портативная установка, несколько инстансов, тесты без домашнего каталога).
pub fn data_dir() -> String {
    if let Some(p) = env_dir("SNENS_DATA_DIR") {
        return p;
    }
    if cfg!(windows) {
        if let Ok(a) = std::env::var("APPDATA") {
            return format!("{a}/snens");
        }
    }
    let base = env_dir("XDG_DATA_HOME").unwrap_or_else(|| format!("{}/.local/share", home()));
    format!("{base}/snens")
}

fn config_dir() -> String {
    if let Some(p) = env_dir("SNENS_CONFIG_DIR") {
        return p;
    }
    if cfg!(windows) {
        if let Ok(a) = std::env::var("APPDATA") {
            return format!("{a}/snens");
        }
    }
    let base = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| format!("{}/.config", home()));
    format!("{base}/snens")
}

/// Переменная как каталог: пустое значение — «не задано», а не корень ФС.
fn env_dir(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SNENS_CONFIG") {
        return PathBuf::from(p);
    }
    PathBuf::from(format!("{}/config.toml", config_dir()))
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str("").expect("defaults must deserialize")
    }
}

/// Профиль модели по slug: побеждает самый длинный совпавший префикс, чтобы
/// `deepseek/deepseek-v4.1-flash` не перехватывался профилем `deepseek`.
pub fn model_profile<'a>(
    models: &'a BTreeMap<String, ModelProfile>,
    model: &str,
) -> Option<&'a ModelProfile> {
    models
        .iter()
        .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, profile)| profile)
}

impl Config {
    /// Экономический бюджет, ограниченный окном с запасом для следующего хода.
    /// Профиль модели (если задан) перебивает авто-оценку по окну.
    pub fn context_budget(&self) -> u64 {
        // Модель спрашиваем в рантайме: `snens model <slug>` меняет профиль
        // уборки на следующем ходу, без перезапуска сервиса.
        let profile = model_profile(&self.models, &self.model_now());
        let budget = if self.context_budget > 0 {
            self.context_budget
        } else if let Some(p) = profile.filter(|p| p.context_budget > 0) {
            p.context_budget
        } else if self.ctx_window <= 300_000 {
            100_000
        } else {
            200_000
        };
        // Дефолтный reserve не должен обнулять синтетические маленькие окна.
        let reserve = self.context_reserve.min(self.ctx_window / 4);
        let physical = self.ctx_window.saturating_sub(reserve);
        let ceiling = (u128::from(physical) * 85 / 100) as u64;
        budget.min(ceiling)
    }

    /// Цель уборки не превышает эффективный экономический бюджет.
    pub fn context_target(&self) -> u64 {
        let budget = self.context_budget();
        let profile = model_profile(&self.models, &self.model_now());
        let target = if self.context_target > 0 {
            self.context_target
        } else if let Some(p) = profile.filter(|p| p.context_target > 0) {
            p.context_target
        } else {
            // u128 сохраняет точные 70% без переполнения даже при u64::MAX.
            (u128::from(budget) * 7 / 10) as u64
        };
        target.min(budget)
    }

    /// Точки взвода давления в токенах по возрастанию. Проценты больше 100 или
    /// нулевые отбрасываются: ступень выше бюджета не сработает никогда, а
    /// нулевая требовала бы уборку на каждом ходу. Пустой список — одна ступень
    /// по бюджету, как было до ступеней.
    pub fn context_pressure_tiers(&self) -> Vec<u64> {
        let budget = self.context_budget();
        let mut tiers: Vec<u64> = self
            .context_pressure_tiers
            .iter()
            .filter(|percent| **percent > 0 && **percent <= 100)
            .map(|percent| (u128::from(budget) * u128::from(*percent) / 100) as u64)
            .filter(|tier| *tier > 0)
            .collect();
        tiers.sort_unstable();
        tiers.dedup();
        if tiers.is_empty() {
            tiers.push(budget);
        }
        tiers
    }

    /// Минимальное сжатие правки зеркала. Ключа нет — 1% бюджета, но не меньше
    /// 2000 токенов (на синтетических мелких бюджетах процент дал бы единицы
    /// токенов и проверка выродилась бы в шум). `0` — проверку выключить:
    /// принимается любая валидная правка.
    pub fn context_min_savings(&self) -> u64 {
        match self.context_min_savings {
            Some(0) => 0,
            Some(tokens) => tokens,
            None => (u128::from(self.context_budget()) / 100).max(2000) as u64,
        }
    }

    /// Активный профиль, если он есть и пригоден к работе. Непригодный профиль
    /// (человек стёр модель в редакторе) не подменяет top-level: иначе ядро
    /// падало бы на пустом slug вместо того, чтобы работать как раньше.
    pub fn active_profile(&self) -> Option<&Provider> {
        let name = self.active_provider.trim();
        if name.is_empty() {
            return None;
        }
        self.providers.get(name).filter(|p| p.is_complete())
    }

    /// Модель, с которой реально пойдёт запрос: у активного профиля своя.
    pub fn effective_model(&self) -> String {
        match self.active_profile() {
            Some(p) => p.model.trim().to_string(),
            None => self.model.clone(),
        }
    }

    /// Подставить поля активного профиля поверх top-level. Делается при каждом
    /// перечитывании файла (`fresh`), а не один раз в `load`: меню настроек
    /// переключает провайдера на живом сервисе, и со следующего хода должен
    /// действовать новый base_url, а не снимок старта.
    ///
    /// Модель, закреплённая кодом (`model_pinned` — подагент на
    /// `subagent_model`), профилем не перебивается: модель этого процесса, а не
    /// настройка инстанса.
    fn resolve_providers(&mut self) {
        let Some(profile) = self.active_profile().cloned() else {
            return;
        };
        self.base_url = profile.base_url.trim().to_string();
        if !self.model_pinned {
            self.model = profile.model.trim().to_string();
        }
        if !profile.key_env.trim().is_empty() {
            self.key_env = profile.key_env.trim().to_string();
        }
    }

    pub fn load(override_path: Option<&str>) -> R<Config> {
        let path = match override_path {
            Some(p) => PathBuf::from(p),
            None => config_path(),
        };
        let cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            // Нет файла — работаем на дефолтах, это не ошибка.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(format!("{}: {e}", path.display()).into()),
        };
        // ponytail: путь к сессиям из конфига может содержать ~, разворачиваем один раз здесь
        let mut cfg = cfg;
        cfg.source_path = Some(path);
        cfg.sessions_dir = cfg.sessions_dir.replace('~', &home());
        cfg.tools_dir = cfg.tools_dir.replace('~', &home());
        cfg.tickets_dir = cfg.tickets_dir.replace('~', &home());
        cfg.skills_dir = cfg.skills_dir.replace('~', &home());
        cfg.mail_dir = cfg.mail_dir.replace('~', &home());
        cfg.resolve_providers();
        Ok(cfg)
    }

    pub fn sessions_path(&self) -> PathBuf {
        PathBuf::from(&self.sessions_dir)
    }

    /// Каталог данных инстанса: рядом с сессиями лежат `soul.md`, `user.md`, `memory.md`.
    pub fn data_path(&self) -> PathBuf {
        self.sessions_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn skills_path(&self) -> PathBuf {
        PathBuf::from(&self.skills_dir)
    }

    /// Репозиторий исходников харнесса: improve правит код здесь. Инсталляция из
    /// исходников — это каталог над tools_dir; иначе конфигурация неполная.
    pub fn repo_path(&self) -> R<PathBuf> {
        let repo = PathBuf::from(&self.tools_dir)
            .parent()
            .map(Path::to_path_buf)
            .ok_or("не могу определить репозиторий из tools_dir")?;
        if !repo.join(".git").exists() {
            return Err(format!(
                "в {} нет .git — улучшению нужен репозиторий исходников",
                repo.display()
            )
            .into());
        }
        Ok(repo)
    }

    /// Оболочка одной строкой для текста, который видит модель (`bash -c`).
    pub fn shell_line(&self) -> String {
        self.shell.join(" ")
    }

    /// Адрес документации о самом харнессе. Пусто — в промпте нет строки
    /// вовсе: мёртвый чужой домен в префиксе хуже молчания.
    pub fn docs_url(&self) -> Option<&str> {
        let u = self.docs_url.trim();
        (!u.is_empty()).then_some(u)
    }

    pub fn tickets_path(&self) -> PathBuf {
        PathBuf::from(&self.tickets_dir)
    }

    pub fn mail_path(&self) -> PathBuf {
        PathBuf::from(&self.mail_dir)
    }

    pub fn tools_path(&self) -> PathBuf {
        PathBuf::from(&self.tools_dir)
    }

    /// Каталог симлинков на тулы — сосед каталога тулов, чтобы всё жило в одном месте.
    ///
    /// Исключение — поставка: там тулы лежат рядом с exe (`%LOCALAPPDATA%\snens\
    /// tools`), а писать в каталог установки нельзя: обновление перезаписывает
    /// бандл, и каталог может быть только для чтения. Поэтому в поставке `bin/`
    /// живёт в каталоге данных.
    pub fn bin_path(&self) -> PathBuf {
        let tools = self.tools_path();
        let shipped = tools.parent().is_some_and(|p| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(Path::to_path_buf))
                .is_some_and(|exe| exe == p)
        });
        let base = if shipped {
            self.data_path()
        } else {
            tools
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        base.join("bin")
    }

    /// Программа долгой памяти: имя — из каталога тулов (там же, где `bin/`),
    /// путь со слэшем — как задан. Пусто или канал выключен — канала нет.
    pub fn memory_bin(&self) -> Option<PathBuf> {
        self.memory_bin_for(&self.memory)
    }

    /// То же, но для секции, перечитанной на ходу (`memory_now`): тумблер должен
    /// действовать на следующем ходу, а не после перезапуска сервиса.
    pub fn memory_bin_for(&self, mem: &Memory) -> Option<PathBuf> {
        if !mem.enabled {
            return None;
        }
        let cmd = mem.cmd.trim();
        if cmd.is_empty() {
            return None;
        }
        let named = Path::new(cmd);
        // Имя без слэша — программа из `bin/` рядом с тулами; её отсутствие
        // значит «канала нет», а не «попробуем несуществующий путь»: иначе ход
        // падал бы на spawn вместо тихой работы без памяти.
        if named.components().count() == 1 {
            let file = format!("{cmd}{}", std::env::consts::EXE_SUFFIX);
            // Сначала рядом с ядром, потом в `bin/` (его раскладывает
            // `sync_bin_dir`). В поставке второго места достаточно: sidecar'ом
            // плагин больше не ездит (он был в бандле дважды одним файлом), а
            // поиск рядом с exe оставлен ради совместимости с уже
            // установленными копиями.
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    let candidate = dir.join(&file);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            let exe = self.bin_path().join(&file);
            return exe.is_file().then_some(exe);
        }
        let path = PathBuf::from(cmd);
        path.is_file().then_some(path)
    }

    /// Сменить основную модель: правит `model` в конфиге как документ (см.
    /// `cfg_edit`), так что комментарии и порядок полей человека остаются.
    /// Возвращает путь конфига (что именно поправили).
    pub fn set_model(&self, slug: &str, override_path: Option<&str>) -> R<PathBuf> {
        let slug = slug.trim();
        if slug.is_empty() {
            return Err("пустой slug модели".into());
        }
        let path = match override_path {
            Some(p) => PathBuf::from(p),
            None => self.source_path.clone().unwrap_or_else(config_path),
        };
        // Активный профиль перекрывает top-level `model`. Отказываем ДО записи:
        // иначе на диске осталась бы правка, которая не действует, и человек
        // искал бы причину в модели, а не в профиле.
        let before = Config::load(Some(&path.display().to_string()))?;
        if before.active_profile().is_some() {
            return Err(format!(
                "модель задаёт активный профиль «{}»: меняй её в профиле (snens providers, \
                 раздел «провайдеры» в вебе) или переключись на другой",
                before.active_provider.trim()
            )
            .replace("  ", " ")
            .into());
        }
        crate::cfg_edit::write(
            &path,
            &[crate::cfg_edit::Edit::top(
                "model",
                crate::cfg_edit::string(slug),
            )],
        )?;
        // Проверяем, что модель действительно та: правка могла не совпасть с
        // тем, что прочитает ядро (например, ключ перекрыт секцией).
        let check = Config::load(Some(&path.display().to_string()))?;
        if check.model != slug {
            // Активный профиль перекрывает top-level `model`: молчаливое «не
            // применилось» отправляло бы человека искать причину в файле, где
            // всё написано верно.
            let hint = if check.active_provider.trim().is_empty() {
                String::new()
            } else {
                format!(
                    " (её задаёт активный профиль «{}» — меняй модель в профиле)",
                    check.active_provider.trim()
                )
            };
            return Err(format!("модель не применилась: в конфиге {}{hint}", check.model).into());
        }
        Ok(path)
    }

    /// Правка одного ключа секции `[memory]` (совместимость с `snens memory`).
    pub fn set_memory_key(&self, key: &str, value: &str) -> R<PathBuf> {
        self.set_section_key("memory", key, value)
    }

    /// Правка одного ключа секции: `value` — готовая TOML-строка значения,
    /// разбираемая в документ (например `"always"` или `true`).
    pub fn set_section_key(&self, section: &str, key: &str, value: &str) -> R<PathBuf> {
        self.set_keys(&[(
            Some(section.to_string()),
            key.to_string(),
            value.to_string(),
        )])
    }

    /// Пачка правок: одна запись файла, поэтому пачка либо применяется целиком,
    /// либо не применяется вовсе. `(секция, ключ, TOML-значение)`.
    pub fn set_keys(&self, changes: &[(Option<String>, String, String)]) -> R<PathBuf> {
        let edits = self.parse_edits(changes)?;
        let path = self.source_path.clone().unwrap_or_else(config_path);
        crate::cfg_edit::write(&path, &edits)
    }

    /// Текст значения → правка документа. Строки разбираются как TOML, так что
    /// экранирование (перевод строки, кавычки) делает сам парсер, а не мы.
    fn parse_edits(
        &self,
        changes: &[(Option<String>, String, String)],
    ) -> R<Vec<crate::cfg_edit::Edit>> {
        let mut out = Vec::new();
        for (section, key, value) in changes {
            let parsed: toml::Value = format!("v = {value}")
                .parse()
                .map_err(|e| format!("{key}: значение не разбирается как TOML: {e}"))?;
            let value = match parsed.get("v") {
                Some(toml::Value::String(s)) => crate::cfg_edit::string(s),
                Some(toml::Value::Integer(n)) => crate::cfg_edit::int(*n as u64)?,
                Some(toml::Value::Boolean(b)) => crate::cfg_edit::boolean(*b),
                _ => return Err(format!("{key}: неподдерживаемый тип значения {value}").into()),
            };
            out.push(match section.as_deref() {
                None => crate::cfg_edit::Edit::top(key.clone(), value),
                Some(section) => crate::cfg_edit::Edit::section(section, key.clone(), value),
            });
        }
        Ok(out)
    }

    /// Секция памяти для нового хода: перечитать из файла. Поверхности живут
    /// долго, а `snens memory …` правит только файл — без перечитки тумблер не
    /// действовал бы до перезапуска сервиса. Файла нет или он сломан — снимок
    /// старта: правка конфига под руками не должна ронять ход.
    pub fn memory_now(&self) -> Memory {
        self.fresh()
            .map(|f| f.memory)
            .unwrap_or_else(|| self.memory.clone())
    }

    /// Конфиг на следующий ход: перечитанный из файла, но с моделями снимка
    /// старта, если они закреплены кодом (`subagent_model` подагента) — они не
    /// в файле и перечиткой затёрлись бы.
    ///
    /// Единое место для всего, что живо меняется без рестарта сервиса: модель,
    /// память, лимит ходов, пороги уборки, оболочка, вложения. Поверхность
    /// живёт днями, а правит файл кто угодно — UI настроек, `snens config`,
    /// человек в редакторе. Файла нет или он сломан — снимок старта: правка
    /// конфига под руками не должна ронять ход.
    pub fn live(&self) -> Config {
        let mut fresh = self.fresh().unwrap_or_else(|| self.clone());
        if self.model_pinned {
            fresh.model = self.model.clone();
            fresh.model_pinned = true;
        }
        // Поля, заданные кодом на этот запуск, а не файлом: их перечитка
        // затёрла бы. `context_budget`/`context_target` выставляет поверхность
        // per-session (групповой чат живёт с своим порогом уборки) — это не
        // настройка инстанса, а параметр сессии.
        fresh.context_budget = self.context_budget;
        fresh.context_target = self.context_target;
        fresh.source_path = self.source_path.clone();
        fresh
    }

    /// Конфиг, каким он сейчас лежит в файле. `None` — файла нет или он не
    /// разбирается.
    pub fn fresh(&self) -> Option<Config> {
        let path = self.source_path.as_ref()?;
        let text = std::fs::read_to_string(path).ok()?;
        let mut fresh: Config = toml::from_str(&text).ok()?;
        // Пути раскрываются один раз в `load`; перечитанный конфиг их не теряет,
        // только если получит тот же `~`-разворот.
        fresh.sessions_dir = self.sessions_dir.clone();
        fresh.tools_dir = self.tools_dir.clone();
        fresh.tickets_dir = self.tickets_dir.clone();
        fresh.skills_dir = self.skills_dir.clone();
        fresh.mail_dir = self.mail_dir.clone();
        fresh.source_path = self.source_path.clone();
        if self.model_pinned {
            fresh.model_pinned = true;
        }
        fresh.resolve_providers();
        Some(fresh)
    }

    /// Модель для нового хода: перечитать из файла.
    /// Долгоживущий процесс (сервер API) держит снимок конфига, а `snens model
    /// <slug>` правит только файл — без перечитки новый ход уходит старой моделью.
    /// Файла нет или он сломан — остаёмся на снимке: ход не должен падать из-за
    /// правки конфига под руками.
    pub fn model_now(&self) -> String {
        if self.model_pinned {
            return self.model.clone();
        }
        self.fresh()
            .map(|f| f.model)
            .unwrap_or_else(|| self.model.clone())
    }

    /// Сменить модель в файле и сразу применить её в этом процессе: без второго
    /// шага сервер продолжал бы ходить старой моделью до перезапуска.
    pub fn set_model_now(&mut self, slug: &str) -> R<PathBuf> {
        let path = match self.source_path.clone() {
            Some(path) => self.set_model(slug, Some(&path.display().to_string()))?,
            None => self.set_model(slug, None)?,
        };
        self.model = slug.trim().to_string();
        self.model_pinned = false;
        Ok(path)
    }

    /// Ключ: сначала переменная окружения, потом `.env` рядом с конфигом —
    /// чтобы snens мог стоять сам, без оболочки с экспортами.
    pub fn api_key(&self) -> R<String> {
        if let Some(key) = self.key_value(&self.key_env) {
            return Ok(key);
        }
        Err(format!(
            "ключ не найден: ни ${}, ни {} в {}",
            self.key_env,
            self.key_env,
            self.env_path().display()
        )
        .into())
    }

    /// Значение ключа по имени переменной: окружение, потом `.env` рядом с
    /// конфигом. `None` — не задан нигде.
    ///
    /// Отдельно от `api_key`, потому что имя переменной у профиля может быть
    /// пустым (наследуется общий `key_env`) — и тогда вопрос «задан ли ключ» надо
    /// задавать с подставленным именем, а не с пустой строкой.
    pub fn key_value(&self, var: &str) -> Option<String> {
        let var = var.trim();
        if var.is_empty() {
            return None;
        }
        if let Ok(k) = std::env::var(var) {
            if !k.trim().is_empty() {
                return Some(k);
            }
        }
        crate::env_file::read(var, &self.env_path())
    }

    /// Ключ задан (в окружении или `.env`) — для списка профилей: значение
    /// клиенту не отдаётся никогда, только признак.
    pub fn key_is_set(&self, var: &str) -> bool {
        self.key_value(var).is_some()
    }

    /// Файл секретов рядом с *фактическим* конфигом: при `--config /tmp/x/config.toml`
    /// секрет берётся из /tmp/x/.env, а не из глобального каталога
    /// пользователя (иначе инстансы обмениваются чужими ключами).
    pub fn env_path(&self) -> PathBuf {
        env_file::path_near(&self.source_path.clone().unwrap_or_else(config_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_section_defaults_and_switches() {
        // Секции нет — память включена на штатном плагине: он часть поставки.
        let cfg = Config::default();
        assert!(cfg.memory.enabled);
        assert_eq!(cfg.memory.cmd, "memory");
        assert_eq!(cfg.memory.recall, RecallMode::Always);
        assert!(cfg.memory.retain);
        // Но бинаря в тестовом окружении нет — канала нет, и это не ошибка.
        assert!(cfg.memory_bin().is_none());

        // Дефолты полей секции: enabled=true, recall=always, retain=true.
        let cfg: Config = toml::from_str("[memory]\ncmd = \"memory\"\n").unwrap();
        assert!(cfg.memory.enabled);
        assert_eq!(cfg.memory.recall, RecallMode::Always);
        assert!(cfg.memory.retain);

        // Мастер-выключатель гасит канал даже с заданным cmd.
        let cfg: Config =
            toml::from_str("[memory]\ncmd = \"memory\"\nenabled = false\nrecall = \"always\"\n")
                .unwrap();
        assert_eq!(cfg.memory.recall, RecallMode::Always);
        assert!(cfg.memory_bin_for(&cfg.memory).is_none());
    }

    #[test]
    fn set_memory_key_edits_section_in_place() {
        let dir = std::env::temp_dir().join(format!("snens-cfg-mem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "model = \"m\"\n\n[memory]\n# комментарий человека\ncmd = \"memory\"\nretain = true\n\n[models.\"x\"]\ncontext_budget = 1\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&path.display().to_string())).unwrap();
        cfg.set_memory_key("recall", "\"always\"").unwrap();
        cfg.set_memory_key("retain", "false").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // Комментарий человека и вторая секция целы.
        assert!(text.contains("# комментарий человека"), "{text}");
        assert!(text.contains("[models.\"x\"]"), "{text}");
        let reloaded = Config::load(Some(&path.display().to_string())).unwrap();
        assert_eq!(reloaded.memory.recall, RecallMode::Always);
        assert!(!reloaded.memory.retain);
        assert_eq!(reloaded.models["x"].context_budget, 1);

        // Ключа нет — он добавляется в существующую секцию, не создаётся новая.
        cfg.set_memory_key("enabled", "true").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[memory]").count(), 1, "{text}");
        assert!(
            Config::load(Some(&path.display().to_string()))
                .unwrap()
                .memory
                .enabled,
            "enabled не применился"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_memory_key_creates_section_when_missing() {
        let dir = std::env::temp_dir().join(format!("snens-cfg-mem2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "model = \"m\"\n").unwrap();
        let cfg = Config::load(Some(&path.display().to_string())).unwrap();
        cfg.set_memory_key("enabled", "false").unwrap();
        let reloaded = Config::load(Some(&path.display().to_string())).unwrap();
        assert!(!reloaded.memory.enabled);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn context_defaults_and_auto_windows() {
        let cfg = Config::default();
        assert_eq!(cfg.context_budget, 0);
        assert_eq!(cfg.context_reserve, 8_192);
        assert_eq!(cfg.context_target, 0);
        for (window, budget, target) in [
            (250_000, 100_000, 70_000),
            (300_000, 100_000, 70_000),
            (300_001, 200_000, 140_000),
            (1_000_000, 200_000, 140_000),
        ] {
            let cfg: Config = toml::from_str(&format!("ctx_window = {window}")).unwrap();
            assert_eq!(cfg.context_budget(), budget);
            assert_eq!(cfg.context_target(), target);
        }
    }

    #[test]
    fn model_profiles_override_auto_by_longest_prefix() {
        let cfg: Config = toml::from_str(
            "ctx_window = 1000000\nmodel = \"deepseek/deepseek-v4.1-flash\"\n\
             [models.\"deepseek\"]\ncontext_budget = 300000\n\
             [models.\"deepseek/deepseek-v4.1-flash\"]\ncontext_budget = 500000\ncontext_target = 350000",
        )
        .unwrap();
        // Побеждает длинный префикс, а не общий `deepseek`.
        assert_eq!(cfg.context_budget(), 500_000);
        assert_eq!(cfg.context_target(), 350_000);

        // Модель без профиля — авто по окну.
        let mut cfg = cfg;
        cfg.model = "openai/gpt-6-luna".into();
        cfg.model_pinned = true;
        assert_eq!(cfg.context_budget(), 200_000);

        // Явный context_budget сильнее профиля.
        let cfg: Config = toml::from_str(
            "ctx_window = 1000000\nmodel = \"deepseek/x\"\ncontext_budget = 123456\n\
             [models.\"deepseek\"]\ncontext_budget = 500000",
        )
        .unwrap();
        assert_eq!(cfg.context_budget(), 123_456);
    }

    #[test]
    fn context_overrides_and_reserve() {
        let mut cfg: Config = toml::from_str(
            "ctx_window = 250000\ncontext_budget = 150000\ncontext_target = 90000\ncontext_reserve = 20000",
        )
        .unwrap();
        assert_eq!(cfg.context_budget(), 150_000);
        assert_eq!(cfg.context_target(), 90_000);
        cfg.context_budget = 250_000;
        cfg.context_target = 240_000;
        assert_eq!(cfg.context_budget(), 195_500);
        assert_eq!(cfg.context_target(), 195_500);
        cfg.context_reserve = 0;
        assert_eq!(cfg.context_budget(), 212_500);
        cfg.context_target = 0;
        assert_eq!(cfg.context_target(), 148_750);
    }

    #[test]
    fn context_tiny_windows_and_extreme_values_are_safe() {
        for (window, budget) in [(0, 0), (1, 0), (3, 2), (4, 2), (400, 255)] {
            let cfg = Config {
                ctx_window: window,
                ..Config::default()
            };
            assert_eq!(cfg.context_budget(), budget);
            assert_eq!(cfg.context_target(), budget * 7 / 10);
        }
        let mut cfg = Config {
            ctx_window: 400,
            context_reserve: u64::MAX,
            ..Config::default()
        };
        assert_eq!(cfg.context_budget(), 255);
        cfg.ctx_window = u64::MAX;
        cfg.context_budget = u64::MAX;
        cfg.context_reserve = 0;
        assert_eq!(
            cfg.context_budget(),
            (u128::from(u64::MAX) * 85 / 100) as u64
        );
        assert_eq!(
            cfg.context_target(),
            (u128::from(cfg.context_budget()) * 7 / 10) as u64
        );
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("snens-cfg-{}-{name}.toml", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn set_model_заменяет_строку_и_хранит_комментарии() {
        let p = tmp("replace");
        std::fs::write(
            &p,
            "# шапка\nmodel = \"старая\"\nbase_url = \"http://x\"\n\n[memory]\ncmd = \"memory\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.set_model("новая/модель", Some(&p.display().to_string()))
            .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# шапка"), "комментарий потерян: {text}");
        assert!(text.contains("model = \"новая/модель\""));
        assert!(!text.contains("старая"));
        // секция осталась секцией, ключ не уехал в неё
        let reload = Config::load(Some(&p.display().to_string())).unwrap();
        assert_eq!(reload.model, "новая/модель");
        assert_eq!(reload.memory.cmd, "memory");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_model_вставляет_ключ_перед_секцией() {
        let p = tmp("insert");
        std::fs::write(
            &p,
            "base_url = \"http://x\"\n\n[memory]\ncmd = \"memory\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.set_model("m1", Some(&p.display().to_string())).unwrap();
        let reload = Config::load(Some(&p.display().to_string())).unwrap();
        assert_eq!(reload.model, "m1");
        assert_eq!(reload.memory.cmd, "memory");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_model_пустой_слаг_ошибка() {
        let p = tmp("empty");
        std::fs::write(&p, "model = \"x\"\n").unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        assert!(cfg.set_model("  ", Some(&p.display().to_string())).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn model_now_читает_файл_а_не_снимок() {
        let p = tmp("fresh");
        std::fs::write(&p, "model = \"старая\"\n").unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        assert_eq!(cfg.model_now(), "старая");
        // Правка файла мимо процесса должна быть видна: именно так `snens model`
        // меняет модель у уже запущенного сервера.
        std::fs::write(&p, "model = \"новая\"\n").unwrap();
        assert_eq!(cfg.model_now(), "новая");
        // Сломанный файл не должен ронять ход: остаёмся на снимке.
        std::fs::write(&p, "model = [не toml").unwrap();
        assert_eq!(cfg.model_now(), "старая");
        let _ = std::fs::remove_file(&p);
    }

    /// `snens model` при активном профиле: отказ до записи, файл не тронут.
    /// Иначе правка легла бы на диск, не подействовала, и причину искали бы в
    /// модели, а не в профиле.
    #[test]
    fn set_model_refuses_while_profile_is_active() {
        let p = tmp("active-profile");
        std::fs::write(
            &p,
            "model = \"top\"\nactive_provider = \"nous\"\n\n[providers.nous]\nbase_url = \"https://x/v1\"\nmodel = \"n/m\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&p.display().to_string())).unwrap();
        let err = cfg.set_model("other/model", None).unwrap_err().to_string();
        assert!(err.contains("nous"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&p)
                .unwrap()
                .matches("model = \"top\"")
                .count(),
            1,
            "файл всё-таки поправлен"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Закреплённая кодом модель (подагент) профилем не перебивается: это модель
    /// процесса, а не настройка инстанса.
    #[test]
    fn pinned_model_survives_active_profile() {
        let p = tmp("pinned-profile");
        std::fs::write(
            &p,
            "active_provider = \"nous\"\n\n[providers.nous]\nbase_url = \"https://x/v1\"\nmodel = \"n/m\"\n",
        )
        .unwrap();
        let mut cfg = Config::load(Some(&p.display().to_string())).unwrap();
        assert_eq!(cfg.model_now(), "n/m");
        cfg.model = "подагентская".into();
        cfg.model_pinned = true;
        // Адрес всё равно профильный: подагенту нужен тот же провайдер.
        assert_eq!(cfg.model_now(), "подагентская");
        assert_eq!(cfg.live().base_url, "https://x/v1");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn model_pinned_игнорирует_файл() {
        let p = tmp("pinned");
        std::fs::write(&p, "model = \"основная\"\n").unwrap();
        let mut cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.model = "подагентская".into();
        cfg.model_pinned = true;
        std::fs::write(&p, "model = \"другая\"\n").unwrap();
        assert_eq!(cfg.model_now(), "подагентская");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_model_now_меняет_и_файл_и_снимок() {
        let p = tmp("apply");
        std::fs::write(&p, "model = \"старая\"\n").unwrap();
        let mut cfg = Config::load(Some(&p.display().to_string())).unwrap();
        cfg.set_model_now("новая/модель").unwrap();
        assert_eq!(cfg.model, "новая/модель");
        assert_eq!(cfg.model_now(), "новая/модель");
        let _ = std::fs::remove_file(&p);
    }
}
