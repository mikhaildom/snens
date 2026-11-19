mod agent;
mod approval;
mod askpass;
mod attach;
mod cfg;
mod cfg_edit;
mod clients;
mod context;
mod context_document;
#[cfg(test)]
mod context_loop_tests;
mod dispatch;
mod env_file;
mod exec;
mod improve;
mod input;
mod llm;
mod log;
mod mail;
mod memory;
mod pairing;
mod production;
mod project;
mod prompt;
mod providers;
mod reasoning;
mod remote;
mod reduce;
mod registry;
mod render;
mod review;
mod selftest;
mod sesscwd;
mod settings;
mod subagent;
mod jobs;
mod wake;
mod disk;
mod sweep;
mod surface_artifact;
mod surface_client;
mod surface_ingest;
mod surface_facts;
mod surface_runtime;
mod surface_server;
mod surface_tui;
mod tickets;
mod tui;

use std::{collections::BTreeMap, error::Error};

pub type R<T> = Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "\
snens — агент с инструментами

  snens run [--new] [--session ID] [--config PATH] \"запрос\" — ответить на запрос в терминале
  snens serve [--config PATH] [--listen 127.0.0.1:7420] [--token-file PATH] — поднять ядро для клиентов
  snens tui [--endpoint URL] [--token-file PATH] [--session ID] — диалог в терминале
  snens show [--session ID] — чего стоили токены этой сессии
  snens log [--session ID] — журнал сессии, строками JSON
  snens sessions — список сессий
  snens sweep [--older-than DAYS] [--attachments] [--dry-run] — удалить старые сессии и вложения
  snens disk [--older-than DAYS] [--dry-run] — снести брошенные сборочные каталоги
  snens tickets [--all] [--target T] [--kind K] — заведённые проблемы
  snens tickets show <имя> | grep <текст> | stats — посмотреть, найти, сводка
  snens tools [--json] — список тулов; --json — определения для модели
  snens model [slug] — какая модель сейчас; со slug — сменить
  snens config [ключ=значение …] — показать значения; с аргументами — поправить на ходу
       ключи: model, max_turns (0 — без лимита), memory_recall, …
  snens providers [list|activate <имя>|key <имя> <ключ>] — профили провайдеров
  snens memory [on|off] [recall always|session|off] [retain on|off] — долгая память
  snens build [<tool>…] — собрать тулы; без аргументов — все
  snens path [--repo DIR] — где binарники сборки: debug, release, установленный в PATH
  snens review [--session ID] — разобрать сессию вторым проходом
  snens improve — поработать над самим ядром
  snens selftest — интеграционная проверка цикла
  snens verify — check, test и selftest одним вердиктом

Подагенты:
  snens run --detach --session ID [--parent PID] <задача> — запустить подагента фоном
  snens steer <ID> <текст> — сказать подагенту; текст можно отдать со stdin
  snens agents [--session ID] — какие подагенты запущены
  snens result <ID> — что подагент ответил

Совместимость:
  snens tui-local [--session ID] [--config PATH] — прежний локальный TUI, без ядра
";

async fn legacy_main() -> R<()> {
    // Роль-шим: если нас позвали под именем `sudo` или `snens-askpass` (симлинки
    // в каталоге шимов, только в PATH инструментов агента), это не CLI, а
    // трансляция в системный sudo / выдача пароля серверу TUI.
    match askpass::invoked_as().as_str() {
        "sudo" => return approval::privileged_shim(std::env::args().skip(1).collect()),
        "snens-askpass" => {
            // Роль клиента askpass: ошибка (нет сервера) — просто код ≠ 0, без
            // debug-шума: sudo сам поймёт, что пароля нет.
            let prompt = std::env::args().nth(1).unwrap_or_default();
            if askpass::client(prompt).await.is_err() {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, flags, positional) = parse_args(&argv);
    match cmd.as_deref() {
        Some("run") => cmd_run(&flags, positional).await,
        // Внутренние подкоманды, которые сам себя спавнит харнесс (run --detach
        // и agent start). Человеку звать их не нужно — только через `run`/`agent`.
        Some("run-child") => cmd_run_child(&flags).await,
        // Исполнитель фонового задания: отдельный процесс, чей результат вернётся
        // в сессию уведомлением (`run --background`). Человеку звать не нужно.
        Some("job-run") => cmd_job_run(&flags).await,
        Some("steer") => cmd_steer(&flags, positional),
        Some("agents") => cmd_agents(&flags),
        Some("result") => cmd_result(&flags, positional),
        Some("tui") => tui::cmd(&flags).await,
        Some("show") => cmd_show(&flags),
        Some("log") => cmd_log(&flags),
        Some("sweep") => sweep::cmd(&flags, positional),
        Some("disk") => disk::cmd(&flags, positional),
        Some("sessions") => {
            let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
            for id in log::list(&cfg.sessions_path())? {
                println!("{id}");
            }
            Ok(())
        }
        Some("tickets") => tickets::cmd(&flags, positional),
        Some("tools") => cmd_tools(&flags).await,
        Some("model") => cmd_model(&flags, positional),
        Some("config") => settings::cmd(&flags, positional),
        Some("providers") => providers::cmd(&flags, positional),
        Some("memory") => cmd_memory(&flags, positional),
        Some("pair") => cmd_pair(&flags).await,
        Some("clients") => cmd_clients(&flags, positional).await,
        Some("build") => cmd_build(&flags, positional),
        Some("path") => cmd_path(&flags),
        Some("review") => review::cmd(&flags).await,
        Some("improve") => improve::cmd(&flags).await,
        Some("selftest") => selftest::selftest().await,
        Some("verify") => cmd_verify(&flags),
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

/// Разбор аргументов подкоманд. Тот же парсер, что у `serve`/`tui`
/// (`production::parse_flags`), но мягкий: подкоманды принимают свои
/// переключатели (`--new`, `--detach`, `--json`), а повтор флага перебивает
/// прежнее значение.
fn parse_args(argv: &[String]) -> (Option<String>, BTreeMap<String, String>, Vec<String>) {
    let cmd = argv.first().cloned();
    let rest = argv.get(1..).unwrap_or_default();
    // Спека подкоманд — в `production`, рядом с остальными: держать её копию
    // здесь значило бы снова завести второй список «что берёт значение».
    match crate::production::parse_flags(rest, &crate::production::CLI_SPEC) {
        Ok((flags, positional)) => (cmd, flags, positional),
        // Мягкий режим ошибается только на отсутствующем значении; раньше такой
        // флаг просто выбрасывался. Пустая карта — то же поведение для вызывающих.
        Err(_) => (cmd, BTreeMap::new(), rest.to_vec()),
    }
}

fn session_id(cfg: &cfg::Config, flags: &BTreeMap<String, String>) -> R<String> {
    let dir = cfg.sessions_path();
    if let Some(s) = flags.get("session") {
        return Ok(s.clone());
    }
    if flags.contains_key("new") {
        return Ok(log::new_id());
    }
    Ok(log::latest(&dir).unwrap_or_else(|_| log::new_id()))
}

async fn cmd_run(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let prompt = positional.join(" ");
    if prompt.trim().is_empty() {
        return Err("нужен текст запроса".into());
    }
    // --detach: не гоняем цикл здесь, а спавним себя отдельным процессом и
    // возвращаем id. Так подагент автономен и переживает родителя.
    if flags.contains_key("detach") {
        let id = session_id(&cfg, flags)?;
        let pid = spawn_child(&cfg, &id, &prompt, flags.get("parent").map(|s| s.as_str()))?;
        // Регистрируем ребёнка у родителя: по pid отличаем «убили наглухо» от
        // «ещё работает», если `Done` так и не появится.
        if let Some(parent) = flags.get("parent").filter(|p| !p.is_empty()) {
            subagent::register_child(&cfg.mail_path(), parent, &id, pid)?;
        }
        println!("{id}");
        eprintln!("[snens] подагент {id}: pid {pid}");
        return Ok(());
    }
    let id = session_id(&cfg, flags)?;
    let parent = flags.get("parent").map(|s| s.as_str()).unwrap_or("");
    let sess = log::Session::open_child(&cfg.sessions_path(), &id, parent)?;
    sess.append(log::Rec::User {
        ts: log::now(),
        text: prompt,
        source: "cli".into(),
        parts: Vec::new(),
        ask: None,
    })?;

    let key = cfg.api_key()?;
    let llm = llm::OpenAi::new(key)?;
    eprintln!("[snens] {id} → {} ({})", cfg.base_url, cfg.model);
    agent::run_agent(&sess, &cfg, &llm, &render::Ui::default()).await?;
    autopilot(&cfg, &id).await
}

/// Дитя-подагент: то, что запускает `run --detach`. Задача приходит в переменной
/// окружения (стабильно при detached-спавне), родитель — в `SNENS_PARENT` или
/// флаге. В конце кладём родителю итог в ящик — это харнесс, а не модель.
async fn cmd_run_child(flags: &BTreeMap<String, String>) -> R<()> {
    let mut cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    if let Some(model) = &cfg.subagent_model {
        cfg.model = model.clone();
        // Модель подагента задана кодом: перечитка файла в цикле её затрёт.
        cfg.model_pinned = true;
    }
    let prompt = std::env::var("SNENS_PROMPT").unwrap_or_default();
    if prompt.trim().is_empty() {
        return Err("run-child: пустая задача (SNENS_PROMPT)".into());
    }
    let id = flags
        .get("session")
        .cloned()
        .ok_or("run-child: нужен --session")?;
    let parent = flags
        .get("parent")
        .cloned()
        .or_else(|| std::env::var("SNENS_PARENT").ok())
        .unwrap_or_default();
    let sess = log::Session::open_child(&cfg.sessions_path(), &id, &parent)?;
    // Первая запись ребёнка — о том, что он жив: pid и время старта. Журнал
    // становится единственным источником правды о состоянии подагента, и
    // поверхность читает `running`/`done` из него, а не угадывает по
    // `kill(pid, 0)` третьей стороной. Файл в ящике (`register_child`)
    // остаётся индексом детей и подсказкой на случай, когда процесс снесли
    // наглухо и `Done` не появился.
    sess.append(log::Rec::Started {
        ts: log::now(),
        pid: std::process::id(),
    })?;
    sess.append(log::Rec::User {
        ts: log::now(),
        text: prompt,
        source: "agent".into(),
        parts: Vec::new(),
        ask: None,
    })?;
    let key = cfg.api_key()?;
    let llm = llm::OpenAi::new(key)?;
    let ui = render::Ui::plain();
    // Своей SSE-шины у подагента нет (её держит поверхность), поэтому его
    // сигналы уезжают в неё по `POST /v1/sessions/{id}/ingest` — иначе работа
    // ребёнка не стримилась и клиент видел только готовый журнал.
    let reporter = surface_ingest::Reporter::new(&id, &parent);
    let observe = |signal| reporter.send(signal);
    let options = agent::SurfaceRunOptions {
        observe: &observe,
        ..Default::default()
    };
    // Терминатор (`Done`) и доклад родителю пишет харнесс — по факту выхода цикла,
    // а не по вызову модели: `agent report` мог бы не состояться.
    let res = agent::run_agent_surface(&sess, &cfg, &llm, &ui, options).await;
    if !parent.is_empty() {
        let status = if res.is_ok() { "ok" } else { "error" };
        let text = subagent::final_text(&cfg, &id).unwrap_or_default();
        let body = if text.trim().is_empty() {
            format!("подагент {id} завершился ({status}), финального текста нет")
        } else {
            format!("подагент {id} завершился ({status}):\n{text}")
        };
        let _ = mail::send(&cfg.mail_path(), &parent, &id, "report", &body);
    }
    // Пустая пачка перед выходом: поверхность пересчитает карточку подагента и
    // увидит `Done`, который лёг выше. Без этого подтверждения процесс мог выйти
    // раньше запроса, и карточка висела «работает» до перезагрузки окна.
    reporter.finish().await;
    res
}

/// Исполнитель фонового задания (`run --background`). Задание приходит файлом:
/// команда, рабочий каталог, таймаут. Результат — файлы `done` рядом с логом,
/// заберёт будильник сессии (`surface_wake`). Родитель этот процесс не ждёт.
async fn cmd_job_run(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let path = flags
        .get("job")
        .ok_or("job-run: нужен --job <файл>")?;
    let job: jobs::Job = serde_json::from_slice(&std::fs::read(path)?)?;
    let done = jobs::run_job(&job, &cfg.shell, cfg.max_process_chars).await?;
    // `done` кладётся рядом с записью задания: будильник ищет его там (`jobs::done`).
    let done_path = std::path::Path::new(path).with_extension("done");
    std::fs::write(&done_path, serde_json::to_vec(&done)?)?;
    Ok(())
}

/// Текст со stdin, если не задан позиционно.
fn text_or_stdin(positional: &[String]) -> String {
    let joined = positional.join(" ");
    if !joined.trim().is_empty() {
        return joined;
    }
    use std::io::Read;
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s.trim().to_string()
}

/// Steer: сообщение сессии на границе ближайшего хода (человек — тоже сессия).
fn cmd_steer(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    // Идентификатор берём либо флагом `--session`, либо первым позиционным
    // аргументом: в справке он всегда стоял позиционным, и человек вводил именно
    // так. Отказ в обоих случаях — с подсказкой.
    let (id, rest) = match flags.get("session").cloned() {
        Some(id) => (id, positional.as_slice()),
        None => match positional.split_first() {
            Some((id, rest)) => (id.clone(), rest),
            None => return Err("нужен id сессии: snens steer <ID> <текст>".into()),
        },
    };
    let text = text_or_stdin(rest);
    if text.trim().is_empty() {
        return Err("пустое сообщение — нечего передавать".into());
    }
    mail::send(&cfg.mail_path(), &id, "human", "steer", &text)?;
    eprintln!("[snens] {id}: уйдёт агенту после шага");
    Ok(())
}

fn cmd_agents(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = session_id(&cfg, flags)?;
    let kids = subagent::children(&cfg, &id);
    println!("сессия: {id}");
    if kids.is_empty() {
        println!("подагентов нет");
        return Ok(());
    }
    for c in &kids {
        let extra = match &c.state {
            subagent::State::Failed(s) => format!(" ({s})"),
            _ => String::new(),
        };
        let tail = subagent::final_text(&cfg, &c.id)
            .map(|t| {
                let one = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let s: String = one.chars().take(100).collect();
                format!(" — {s}")
            })
            .unwrap_or_default();
        println!("{}: {}{extra}{tail}", c.id, subagent::describe(&c.state));
    }
    Ok(())
}

fn cmd_result(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = match positional.first() {
        Some(s) => s.clone(),
        None => session_id(&cfg, flags)?,
    };
    match subagent::final_text(&cfg, &id) {
        Some(t) => println!("{t}"),
        None => eprintln!("[snens] сессия {id} не ответила текстом — смотри snens log --session {id}"),
    }
    Ok(())
}

/// Спавн подагента отдельным процессом: своя сессия процесса (setsid), как у
/// фонового ревью, — чтобы Ctrl+C и закрытие окна до ребёнка не дострелили.
///
/// Задачу передаём переменной окружения, а не argv: она может быть длинной и
/// многострочной. Стандартные потоки — в /dev/null: подагент работает молча, его
/// жизнь видна через лог и ящик, а не через наш терминал.
fn spawn_child(_cfg: &cfg::Config, id: &str, prompt: &str, parent: Option<&str>) -> R<u32> {
    let _hup = HupBlock::new();
    let exe = self_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("run-child")
        .arg("--session")
        .arg(id)
        .arg("--config")
        .arg(cfg::config_path())
        .env("SNENS_CONFIG", cfg::config_path())
        .env("SNENS_PROMPT", prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(p) = parent.filter(|p| !p.is_empty()) {
        cmd.arg("--parent").arg(p);
    }
    // Куда стримить свою работу (`SNENS_SURFACE_URL`/`SNENS_SURFACE_TOKEN`):
    // окружение наследуется от процесса поверхности через вызов тула `agent`.
    // Если подагента запускают из TUI или `snens run`, переменных нет — он
    // просто никуда не стримит.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            libc::sigprocmask(libc::SIG_UNBLOCK, &hup_set(), std::ptr::null_mut());
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok(child.id())
}

/// Автономный цикл: длинная сессия → ревью. Порог по длине, не по цене: цена
/// слишком разбросана, чтобы быть сигналом. Общий у `run` и `tui` — оба
/// заканчиваются одинаково.
///
/// Улучшение здесь намеренно не запускается: это слишком расточительно после
/// каждого ревью. `snens improve` пока зовётся руками — так видно, когда он нужен.
pub async fn autopilot(cfg: &cfg::Config, id: &str) -> R<()> {
    let path = cfg.sessions_path().join(format!("{id}.jsonl"));
    let turns = log::read_file(&path)?
        .iter()
        .filter(|r| matches!(r, log::Rec::Assistant { .. }))
        .count();
    if turns < cfg.review_min_turns {
        return Ok(());
    }
    eprintln!(
        "[snens] {turns} ходов ≥ {} — ревью в фоне",
        cfg.review_min_turns
    );
    spawn_review(cfg, id)
}

/// Ревью уезжает отдельным процессом и не держит поверхность: TUI можно закрыть,
/// а `snens review --session <id>` доработает и допишет тикеты. Жизнь агента не
/// должна быть привязана к жизни окна — это первый шаг к демону (M5).
///
/// Ждём только факт запуска, не завершения. Вывод — в файл `<id>-review.log`
/// рядом с сессией: там видно, чем кончилось фоновое ревью.
pub(crate) fn spawn_review(cfg: &cfg::Config, id: &str) -> R<()> {
    // Блокируем SIGHUP на весь критический участок: закрытие окна терминала шлёт
    // SIGHUP всей foreground-группе, и без этого родитель умрёт, не успев даже
    // создать лог, а форкнутый ребёнок — до того, как pre_exec дозвонит setsid.
    let _hup = HupBlock::new();
    let exe = self_exe()?;
    let log_path = cfg.sessions_path().join(format!("{id}-review.log"));
    let out = std::fs::File::create(&log_path)?;
    let err = out.try_clone()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("review")
        .arg("--session")
        .arg(id)
        .env("SNENS_CONFIG", cfg::config_path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err));
    // Своя сессия процесса: Ctrl+C в терминале не должен дострелить до фонового
    // ревью, а SIGHUP при закрытии окна — убить его.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            // После setsid групповая рассылка при закрытии окна до нас уже не
            // достаёт — снимаем блокировку SIGHUP, унаследованную от родителя.
            libc::sigprocmask(libc::SIG_UNBLOCK, &hup_set(), std::ptr::null_mut());
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    eprintln!(
        "[snens] ревью {id}: pid {}, лог {}",
        child.id(),
        log_path.display()
    );
    // Дочерний процесс не ждём: TUI и run выходят сразу. Дети переподвешиваются
    // к init и работают сами — именно этого и хочет пользователь.
    Ok(())
}

/// Путь к собственному бинарнику, устойчивый к пересборке во время работы.
///
/// cargo (и `snens build`) заменяют бинарник через rename: старый инод отвязывается,
/// и если наш процесс уже был запущен из этого файла, `current_exe()` вернёт путь
/// с суффиксом " (deleted)". `Command::new` по такому пути падает с ENOENT — именно
/// поэтому фоновое ревью, запущенное из только что пересобранного ядра, умирало,
/// оставив пустой `-review.log`. Снимаем суффикс: путь снова указывает на свежий
/// бинарник, а его и надо запускать.
fn self_exe() -> R<std::path::PathBuf> {
    Ok(strip_deleted(std::env::current_exe()?))
}

/// Убирает пометку " (deleted)", которую ядро дописывает к `/proc/self/exe`,
/// когда файл уже отвязан (пересборка ядра под собой). Путь снова оживает.
fn strip_deleted(p: std::path::PathBuf) -> std::path::PathBuf {
    match p.to_string_lossy().strip_suffix(" (deleted)") {
        Some(stripped) => std::path::PathBuf::from(stripped),
        None => p,
    }
}

/// Набор сигналов из одного SIGHUP: для sigprocmask-блокировки на время spawn.
#[cfg(unix)]
fn hup_set() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGHUP);
        set
    }
}

/// Блокирует SIGHUP на время жизни: закрытие окна в этот момент лишь отложит
/// сигнал (он станет pending), а не убьёт процесс посреди spawn. При отпускании
/// маска восстанавливается — отложенный SIGHUP дострелит уже после того, как
/// ребёнок отцепился в свою сессию.
struct HupBlock {
    #[cfg(unix)]
    old: libc::sigset_t,
}

impl HupBlock {
    fn new() -> Self {
        #[cfg(unix)]
        unsafe {
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigprocmask(libc::SIG_BLOCK, &hup_set(), &mut old);
            Self { old }
        }
        #[cfg(not(unix))]
        Self {}
    }
}

#[cfg(unix)]
impl Drop for HupBlock {
    fn drop(&mut self) {
        unsafe {
            libc::sigprocmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut());
        }
    }
}

fn cmd_show(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = session_id(&cfg, flags)?;
    let sess = log::Session::open(&cfg.sessions_path(), &id)?;
    let recs = sess.read()?;

    let (mut in_tok, mut cached, mut out_tok, mut cost) = (0u64, 0u64, 0u64, 0u64);
    let mut reason_tok = 0u64;
    let mut encrypted_turns = 0u64;
    let mut biggest: Option<(usize, &str)> = None;
    // Для диагностики кеша: сколько ходов пришло без кеша и менялся ли апстрим
    // провайдера. Смена бэкенда объясняет промах, который иначе искать в проекции.
    let (mut uncached_turns, mut usages) = (0u64, 0u64);
    let mut upstreams: Vec<&str> = Vec::new();
    let mut fingerprints: Vec<&str> = Vec::new();
    for r in &recs {
        match r {
            log::Rec::ToolResult { tool, bytes, .. } => {
                if biggest.is_none_or(|(b, _)| *bytes > b) {
                    biggest = Some((*bytes, tool.as_str()));
                }
            }
            log::Rec::Usage {
                in_tok: i,
                cached_tok: c,
                out_tok: o,
                cost_micro: m,
                upstream_id,
                fingerprint,
                reason_tok: r,
                encrypted_reasoning,
                ..
            } => {
                in_tok += i;
                cached += c;
                out_tok += o;
                cost += m;
                reason_tok += r;
                if *encrypted_reasoning {
                    encrypted_turns += 1;
                }
                usages += 1;
                if *c == 0 {
                    uncached_turns += 1;
                }
                if !upstream_id.is_empty() {
                    upstreams.push(upstream_id.as_str());
                }
                if !fingerprint.is_empty() {
                    fingerprints.push(fingerprint.as_str());
                }
            }
            _ => {}
        }
    }
    println!("сессия:  {id}");
    println!("файл:    {}", sess.path().display());
    if let Some((b, name)) = biggest {
        println!("макс. результат: {b} Б ({name})");
    }
    println!("токены:  вход {in_tok} (из них кеш {cached}), выход {out_tok}");
    if reason_tok > 0 {
        // Рассуждение у зашифрованных моделей не читается, но оплачено —
        // сумма показывает, сколько стоили ходы, которых мы не видели.
        let closed = if encrypted_turns > 0 {
            format!(", ходов с закрытым рассуждением: {encrypted_turns}")
        } else {
            String::new()
        };
        println!("рассуж.: {reason_tok} токенов{closed}");
    }
    if usages > 0 {
        // Ходы без кеша — норма для первого хода и после смены апстрима. Если их
        // много подряд на одном апстриме — тогда промах наш, искать в проекции.
        println!("кеш:     {uncached_turns} из {usages} ходов без попадания");
        // Разные серверы опознаём по отпечатку бэкенда, когда он есть: id ответа
        // (`gen-…`) у каждого хода свой и о смене сервера ничего не говорит.
        let keys = if fingerprints.len() == usages as usize {
            &fingerprints
        } else {
            &upstreams
        };
        let distinct: std::collections::BTreeSet<&str> = keys.iter().copied().collect();
        if distinct.len() > 1 {
            println!(
                "серверы:  {} — запросы уходили на разные, кеш на них не общий",
                distinct.len()
            );
        } else if let Some(u) = keys.first() {
            println!("сервер:  {u}");
        }
    }
    if cost > 0 {
        println!("цена:    ${:.4}", cost as f64 / 1e6);
    }
    Ok(())
}

fn cmd_log(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = session_id(&cfg, flags)?;
    let dir = cfg.sessions_path();
    let path = dir.join(format!("{id}.jsonl"));
    // Чтение без лока и без правки хвоста: `log` — просмотрщик, в том числе
    // битого лога. Префикс ревью раскрывается (`Rec::Replay`), чтобы grep по
    // выводу видел тот же транскрипт, что раньше лежал копией в файле.
    for rec in log::expand(&log::read_file(&path)?, &dir) {
        println!("{}", serde_json::to_string(&rec)?);
    }
    Ok(())
}

/// Что видит модель: встроенные тулы ядра + процессы реестра, теми же
/// summary/usage, что уезжают в определения. Встроенные печатаются первыми и
/// отдельной секцией: без них ответ на вопрос «есть ли у меня ctx» был ложью
/// (модель спрашивала именно этой командой и делала вывод, что тула нет).
async fn cmd_tools(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let tools = registry::discover(&cfg).await;
    let bin = registry::sync_bin_dir(&cfg, &tools)?;
    // `--json` — ровно то, что уезжает модели: дамп определений для отладки
    // контракта (generic vs schema) без догадок по тексту.
    if flags.contains_key("json") {
        let mut defs = dispatch::builtin_defs(&cfg.shell_line());
        defs.extend(registry::definitions(&tools));
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"tools": defs}))?
        );
        return Ok(());
    }
    println!("каталог тулов: {}", cfg.tools_path().display());
    println!("PATH для тулов и человека: {}\n", bin.display());
    println!("встроенные:");
    for def in dispatch::builtin_defs(&cfg.shell_line()) {
        let name = def
            .pointer("/function/name")
            .and_then(|x| x.as_str())
            .unwrap_or("?");
        let summary = def
            .pointer("/function/description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!("  {name}\n    {summary}");
    }
    println!("\nреестр (процессы в каталоге тулов):");
    if tools.is_empty() {
        println!("  (пусто)");
    }
    for t in &tools {
        println!("  {}\n    {}", t.name, t.summary);
        match &t.schema {
            // Схема есть — показываем поля (имя: тип), а не generic usage:
            // именно их видит модель, и `args` больше не нужен.
            Some(s) => {
                let req = s.required();
                let fields = s
                    .fields()
                    .into_iter()
                    .map(|(n, ty)| {
                        if req.contains(&n) {
                            format!("{n}: {ty} (обязательно)")
                        } else {
                            format!("{n}: {ty}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("    [схема] поля: {fields}");
                println!("    {}", t.usage);
            }
            None => println!("    аргументы: {}", t.usage),
        }
    }
    Ok(())
}

/// `snens memory` — состояние канала долгой памяти и правка его ключей.
/// Без аргументов печатает `enabled`/`recall`/`retain`/`cmd`; с аргументами
/// правит указанные ключи в `[memory]` конфига. Действует со следующего хода:
/// живой процесс перечитывает секцию на старте цикла (`Config::memory_now`).
/// `snens pair` — открыть приглашение и напечатать код (docs/pairing.md).
///
/// Идёт через HTTP к живому ядру, а не пишет файл сам: приглашение живёт в
/// памяти процесса, и второй процесс, положивший его «рядом», ядро бы не увидело.
async fn cmd_pair(flags: &BTreeMap<String, String>) -> R<()> {
    if flags.contains_key("help") || flags.contains_key("h") {
        println!(
            "snens pair [--config PATH] [--endpoint URL] [--token-file PATH]\n\
             \x20 открыть приглашение: код на 5 минут и адрес для приложения"
        );
        return Ok(());
    }
    let (client, _) = surface_client_for(flags)?;
    let reply = client
        .pair_open()
        .await
        .map_err(|e| format!("ядро не ответило: {e}"))?;
    let code = reply["code"].as_str().unwrap_or("?");
    let left = reply["seconds_left"].as_u64().unwrap_or(0);
    println!("Код пейринга: {code}");
    println!("код действует {}:{:02}", left / 60, left % 60);
    for e in reply["endpoints"].as_array().cloned().unwrap_or_default() {
        if let Some(e) = e.as_str() {
            println!("Адрес: {e}");
        }
    }
    println!("введи адрес и код в приложении — токен оно получит само");
    Ok(())
}

/// `snens clients [revoke <id>]` — кто подключён и отзыв доступа.
async fn cmd_clients(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    if flags.contains_key("help") || flags.contains_key("h") {
        println!(
            "snens clients            — подключённые устройства\n\
             \x20 snens clients revoke <id>  — отозвать (токен перестаёт работать сразу)"
        );
        return Ok(());
    }
    let (client, _) = surface_client_for(flags)?;
    let mut args = positional.into_iter();
    match args.next().as_deref() {
        Some("revoke") => {
            let id = args.next().ok_or("clients revoke: нужен id устройства")?;
            client
                .revoke_client(&id)
                .await
                .map_err(|e| format!("отзыв не удался: {e}"))?;
            println!("отозвано: {id}");
        }
        _ => {
            let reply = client.clients().await.map_err(|e| format!("ядро не ответило: {e}"))?;
            let items = reply["clients"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("подключённых устройств нет");
            }
            for c in items {
                let seen = c["last_seen"].as_u64().unwrap_or(0);
                println!(
                    "{} · {} · создан {}",
                    c["name"].as_str().unwrap_or("?"),
                    c["id"].as_str().unwrap_or("?"),
                    if seen > 0 { "использовался" } else { "ещё не входил" }
                );
            }
        }
    }
    Ok(())
}

/// Клиент поверхности по флагам: адрес, токен из файла, конфиг — как у `tui`.
fn surface_client_for(
    flags: &BTreeMap<String, String>,
) -> R<(crate::surface_client::Client, cfg::Config)> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let token_path = flags
        .get("token-file")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| cfg.data_path().join("api.token"));
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("нет токена {}: {e} (ядро запущено?)", token_path.display()))?
        .trim()
        .to_string();
    let endpoint = flags
        .get("endpoint")
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:7420".into());
    let client = crate::surface_client::Client::new(endpoint, token)
        .map_err(|e| format!("клиент поверхности: {e}"))?;
    Ok((client, cfg))
}

fn cmd_memory(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    if flags.contains_key("help") || flags.contains_key("h") {
        println!(
            "snens memory [on|off] [recall always|session|off] [retain on|off]\n\
             \x20 без аргументов — показать состояние; аргументы правят конфиг"
        );
        return Ok(());
    }
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let mut changes: Vec<(&str, String)> = Vec::new();
    let mut it = positional.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "on" => changes.push(("enabled", "true".into())),
            "off" => changes.push(("enabled", "false".into())),
            "recall" => {
                let v = it
                    .next()
                    .ok_or("memory recall: нужен режим — always, session или off")?;
                match v.as_str() {
                    "always" | "session" | "off" => {
                        changes.push(("recall", format!("\"{v}\"")))
                    }
                    other => return Err(format!("memory recall: режим {other} не знаю — always, session или off").into()),
                }
            }
            "retain" => {
                let v = it.next().ok_or("memory retain: нужен on или off")?;
                match v.as_str() {
                    "on" => changes.push(("retain", "true".into())),
                    "off" => changes.push(("retain", "false".into())),
                    other => return Err(format!("memory retain: {other} не знаю — on или off").into()),
                }
            }
            other => return Err(format!("memory: непонятный аргумент {other}").into()),
        }
    }
    if changes.is_empty() {
        let mem = cfg.memory_now();
        println!(
            "enabled={}\nrecall={}\nretain={}\ncmd={}",
            mem.enabled,
            match mem.recall {
                cfg::RecallMode::Always => "always",
                cfg::RecallMode::Session => "session",
                cfg::RecallMode::Off => "off",
            },
            mem.retain,
            if mem.cmd.is_empty() { "(нет)" } else { &mem.cmd },
        );
        return Ok(());
    }
    let mut path = None;
    for (key, value) in changes {
        path = Some(cfg.set_memory_key(key, &value)?);
    }
    println!("{}", path.map(|p| p.display().to_string()).unwrap_or_default());
    Ok(())
}

/// `snens model [slug]` — какая модель сейчас и смена основной модели в конфиге.
/// Без аргумента печатает текущую; со slug правит `model = "..."` в config.toml.
fn cmd_model(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    if flags.contains_key("help") || flags.contains_key("h") {
        println!("snens model [slug] — без аргумента показать текущую модель, со slug — сменить в конфиге");
        return Ok(());
    }
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    match positional.first() {
        None => println!("{}", cfg.model),
        Some(slug) => {
            let path = cfg.set_model(slug, flags.get("config").map(|s| s.as_str()))?;
            println!("{slug}\n({})", path.display());
        }
    }
    Ok(())
}

/// Собрать вшитые тулы и положить бинарники туда, откуда их берёт реестр:
/// `tools/<имя>/<имя>[.exe]`. Шаг входит в дефолтную установку, поэтому он в
/// харнессе, а не в sh-скрипте: на Windows sh нет.
/// `snens build [<tool>…]` — без аргументов весь workspace, с аргументами только
/// названные крейты (правка одного тула не должна линковать все, ~40 с → ~2 с).
/// Где бинарники: спрашиваем cargo, а не угадываем путь.
///
/// `./target/debug/snens` есть не у каждой установки: target может быть
/// вынесен на другой диск (`.cargo/config.toml`), а `snens` в PATH — симлинк на
/// release-сборку или на каталог релиза. Сессия из четырёх hits искала
/// «естественный» путь, получала exit=127 и добывала настоящий через
/// `cargo metadata` python-ом.
pub fn cmd_path(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let repo = match flags.get("repo") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => cfg
            .tools_path()
            .parent()
            .ok_or("у tools_dir нет родителя — негде искать cargo-проект")?
            .to_path_buf(),
    };
    // Установленный в PATH — то, что реально исполняется; это не всегда link на
    // debug-сборку, и именно на этом расхождении сессии и спотыкались.
    match on_path("snens") {
        Some(p) => {
            let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            println!("в PATH: {}", p.display());
            if real != p {
                println!("  → {}", real.display());
            }
        }
        None => println!("в PATH: нет"),
    }
    // target спрашиваем у cargo: перенос каталога — законный конфиг, а не ошибка.
    match cargo_target_dir(&repo) {
        Ok(dir) => {
            for profile in ["debug", "release"] {
                let bin = dir.join(profile).join(format!("snens{}", std::env::consts::EXE_SUFFIX));
                let mark = if bin.is_file() { "" } else { " (не собран)" };
                println!("{profile}: {}{mark}", bin.display());
            }
        }
        Err(e) => println!("target: не спросить ({e})"),
    }
    Ok(())
}

fn cmd_build(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    if flags.contains_key("help") || flags.contains_key("h") {
        println!("snens build [<tool>…] — без аргументов весь workspace, с аргументами — только названные тулы");
        return Ok(());
    }
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let tools_dir = cfg.tools_path();
    let root = tools_dir
        .parent()
        .ok_or("у tools_dir нет родителя — негде запускать cargo")?;

    // Что собирать: названные крейты или весь workspace. Имена сверяем с
    // каталогами тулов, чтобы `snens build nosuch` падал, а не собирал всё молча.
    let all = positional.is_empty();
    let mut targets = Vec::new();
    for name in &positional {
        let dir = tools_dir.join(name);
        if !dir.join("Cargo.toml").is_file() {
            return Err(format!("нет тула {name} в {}", tools_dir.display()).into());
        }
        if !dir.join("src/main.rs").is_file() {
            return Err(format!(
                "{name} — библиотека, а не тул: собирается вместе с теми, кто её использует"
            )
            .into());
        }
        targets.push(name.clone());
    }

    let mut cargo = std::process::Command::new("cargo");
    cargo.args(["build", "--release"]);
    if all {
        cargo.arg("--workspace");
    } else {
        for name in &targets {
            cargo.args(["-p", name]);
        }
    }
    let ok = cargo.current_dir(root).status()?;
    if !ok.success() {
        return Err("cargo build завершился с ошибкой".into());
    }

    let release = cargo_target_dir(root)?.join("release");
    let mut built = 0;
    for e in std::fs::read_dir(&tools_dir)?.flatten() {
        let dir = e.path();
        if !dir.join("Cargo.toml").is_file() {
            continue;
        }
        // Крейт без `src/main.rs` — библиотека (например `cdp`, транспорт для
        // `shot` и `genui`), а не тул: бинарника у неё нет, и копировать нечего.
        if !dir.join("src/main.rs").is_file() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !all && !targets.contains(&name) {
            continue;
        }
        let from = release.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if !from.is_file() {
            eprintln!("[snens] {name}: нет {} — пропускаю", from.display());
            continue;
        }
        let to = dir.join(from.file_name().unwrap());
        // Через симлинк (наследие сборки скриптом) копирование записало бы
        // бинарник сам в себя и обнулило его.
        let _ = std::fs::remove_file(&to);
        std::fs::copy(&from, &to)?;
        println!("{name} → {}", to.display());
        built += 1;
    }
    println!("собрано тулов: {built}");
    Ok(())
}

/// Пересобрать ядро и положить бинарник на место — туда, откуда его берёт PATH
/// (симлинк `~/.local/bin/snens` → debug-сборка). Зовётся в конце `snens improve`:
/// улучшение правит собственный код, и новая сборка должна вступить в силу без
/// ручного `cargo build`. Если `snens` в PATH — это сама debug-сборка (симлинк на
/// тот же файл), копировать нечего, cargo пишет прямо туда.
///
/// `fs::copy` поверх симлинка обнулил бы источник (назначение — симлинк на
/// копируемый файл): поэтому назначение сначала резолвим и, если это тот же файл,
/// копию не делаем.
pub fn rebuild_and_install(repo: &std::path::Path) -> R<()> {
    let ok = std::process::Command::new("cargo")
        .arg("build")
        .current_dir(repo)
        .status()?;
    if !ok.success() {
        return Err("cargo build завершился с ошибкой".into());
    }
    let built =
        cargo_target_dir(repo)?.join(format!("debug/snens{}", std::env::consts::EXE_SUFFIX));
    let Some(dest) = on_path("snens") else {
        eprintln!("[snens] snens не найден в PATH — бинарник только пересобран");
        return Ok(());
    };
    // Симлинк на саму сборку (обычная установка из исходников): cargo уже
    // записал новый файл, копировать в него же нельзя.
    if std::fs::canonicalize(&dest).ok().as_deref() == Some(built.as_path()) {
        return Ok(());
    }
    let _ = std::fs::remove_file(&dest);
    std::fs::copy(&built, &dest)?;
    eprintln!("[snens] бинарник обновлён: {}", dest.display());
    Ok(())
}

/// Одна команда «проверь всё»: cargo check → cargo test --workspace → selftest.sh,
/// стоп на первой красной. Смысл не в скорости, а в вердикте: без него проверка —
/// рутина из трёх команд, а фильтрованный вывод на успехе печатает пустоту, и
/// «тихо» неотличимо от «неясно».
fn cmd_verify(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let repo = cfg.repo_path()?;

    let steps: Vec<(&str, Vec<String>)> = vec![
        // check, а не build: на каждой правке линковка не нужна.
        ("cargo check", vec!["check".into(), "--workspace".into()]),
        (
            "cargo test --workspace",
            vec!["test".into(), "--workspace".into()],
        ),
        ("selftest тулов", vec![]),
    ];
    for (label, args) in steps {
        println!("── {label}");
        let status = if label == "selftest тулов" {
            let sh = repo.join("tools/selftest.sh");
            if !sh.is_file() {
                eprintln!("[snens] нет {} — шаг пропущен", sh.display());
                continue;
            }
            std::process::Command::new("sh")
                .arg(&sh)
                .current_dir(&repo)
                .status()?
        } else {
            std::process::Command::new("cargo")
                .args(&args)
                .current_dir(&repo)
                .status()?
        };
        if !status.success() {
            return Err(format!("{label}: красный (код {:?})", status.code()).into());
        }
    }
    println!("проверка прошла");
    Ok(())
}

/// Найти исполняемый файл по имени в PATH (аналог `which`, без сабпроцесса).
fn on_path(name: &str) -> Option<std::path::PathBuf> {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(&exe))
            .find(|c| c.is_file())
    })
}

/// target может быть перенесён (`.cargo/config.toml`) — спрашиваем cargo, а не угадываем.
fn cargo_target_dir(root: &std::path::Path) -> R<std::path::PathBuf> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Err("cargo metadata упал".into());
    }
    let j: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let dir = j
        .get("target_directory")
        .and_then(|v| v.as_str())
        .ok_or("cargo metadata без target_directory")?;
    Ok(std::path::PathBuf::from(dir))
}
#[cfg(test)]
mod tests {
    use super::strip_deleted;
    use std::path::PathBuf;

    #[test]
    fn strip_deleted_recovers_path_after_rebuild() {
        // Ровно то, что отдаёт /proc/self/exe после пересборки бинарника под ногами.
        let p = PathBuf::from("/mnt/target/debug/snens (deleted)");
        assert_eq!(strip_deleted(p), PathBuf::from("/mnt/target/debug/snens"));
    }

    #[test]
    fn strip_deleted_leaves_clean_path() {
        let p = PathBuf::from("/mnt/target/debug/snens");
        assert_eq!(strip_deleted(p.clone()), p);
    }
}
