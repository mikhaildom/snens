mod agent;
mod attach;
mod cfg;
mod dispatch;
mod exec;
mod improve;
mod llm;
mod log;
mod mail;
mod memory;
mod project;
mod prompt;
mod reduce;
mod registry;
mod render;
mod review;
mod selftest;
mod subagent;
mod tui;

use std::{collections::BTreeMap, error::Error};

pub type R<T> = Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "\
snens — узкое ядро агента

  snens run [--new] [--session ID] [--config PATH] \"запрос\"
  snens tui [--session ID] [--config PATH]
  snens show [--session ID]
  snens log  [--session ID]
  snens sessions
  snens tools
  snens build
  snens review [--session ID]
  snens improve
  snens selftest

Подагенты:
  snens run --detach --session ID [--parent PID] <задача>
  snens steer <ID> <текст>       (текст берётся со stdin, если не задан)
  snens agents [--session ID]    список и состояние детей
  snens result <ID>              финальный текст сессии
";

#[tokio::main]
async fn main() -> R<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, flags, positional) = parse_args(&argv);
    match cmd.as_deref() {
        Some("run") => cmd_run(&flags, positional).await,
        // Внутренние подкоманды, которые сам себя спавнит харнесс (run --detach
        // и agent start). Человеку звать их не нужно — только через `run`/`agent`.
        Some("run-child") => cmd_run_child(&flags).await,
        Some("steer") => cmd_steer(&flags, positional),
        Some("agents") => cmd_agents(&flags),
        Some("result") => cmd_result(&flags, positional),
        Some("tui") => tui::cmd(&flags).await,
        Some("show") => cmd_show(&flags),
        Some("log") => cmd_log(&flags),
        Some("sessions") => {
            let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
            for id in log::list(&cfg.sessions_path())? {
                println!("{id}");
            }
            Ok(())
        }
        Some("tools") => cmd_tools(&flags).await,
        Some("build") => cmd_build(&flags),
        Some("review") => review::cmd(&flags).await,
        Some("improve") => improve::cmd(&flags).await,
        Some("selftest") => selftest::selftest().await,
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

#[allow(clippy::type_complexity)]
fn parse_args(argv: &[String]) -> (Option<String>, BTreeMap<String, String>, Vec<String>) {
    let mut flags = BTreeMap::new();
    let mut positional = Vec::new();
    let mut it = argv.iter();
    let cmd = it.next().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" | "--config" | "--parent" => {
                if let Some(v) = it.next() {
                    flags.insert(a.trim_start_matches("--").to_string(), v.clone());
                }
            }
            other if other.starts_with("--") => {
                flags.insert(other.trim_start_matches("--").to_string(), "1".into());
            }
            other => positional.push(other.to_string()),
        }
    }
    (cmd, flags, positional)
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
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
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
    sess.append(log::Rec::User {
        ts: log::now(),
        text: prompt,
        source: "agent".into(),
    })?;
    let key = cfg.api_key()?;
    let llm = llm::OpenAi::new(key)?;
    let ui = render::Ui::plain();
    // Терминатор (`Done`) и доклад родителю пишет харнесс — по факту выхода цикла,
    // а не по вызову модели: `agent report` мог бы не состояться.
    let res = agent::run_agent(&sess, &cfg, &llm, &ui).await;
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
    res
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
    let id = flags
        .get("session")
        .cloned()
        .ok_or("steer: нужен --session <ID>")?;
    let text = text_or_stdin(&positional);
    if text.trim().is_empty() {
        return Err("steer: пустое сообщение".into());
    }
    mail::send(&cfg.mail_path(), &id, "human", "steer", &text)?;
    eprintln!("[snens] steer → {id}");
    Ok(())
}

fn cmd_agents(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = session_id(&cfg, flags)?;
    let kids = subagent::children(&cfg, &id);
    println!("сессия: {id}");
    if kids.is_empty() {
        println!("(детей нет)");
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
        None => eprintln!("[snens] у сессии {id} нет финального текста"),
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
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("run-child")
        .arg("--session")
        .arg(id)
        .arg("--config")
        .arg(cfg::config_path())
        .env("SNENS_PROMPT", prompt)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(p) = parent.filter(|p| !p.is_empty()) {
        cmd.arg("--parent").arg(p);
    }
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
fn spawn_review(cfg: &cfg::Config, id: &str) -> R<()> {
    // Блокируем SIGHUP на весь критический участок: закрытие окна терминала шлёт
    // SIGHUP всей foreground-группе, и без этого родитель умрёт, не успев даже
    // создать лог, а форкнутый ребёнок — до того, как pre_exec дозвонит setsid.
    let _hup = HupBlock::new();
    let exe = std::env::current_exe()?;
    let log_path = cfg.sessions_path().join(format!("{id}-review.log"));
    let out = std::fs::File::create(&log_path)?;
    let err = out.try_clone()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("review")
        .arg("--session")
        .arg(id)
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

    let (mut calls, mut in_tok, mut cached, mut out_tok, mut cost) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut biggest: Option<(usize, &str)> = None;
    // Для диагностики кеша: сколько ходов пришло без кеша и менялся ли апстрим
    // провайдера. Смена бэкенда объясняет промах, который иначе искать в проекции.
    let (mut uncached_turns, mut usages) = (0u64, 0u64);
    let mut upstreams: Vec<&str> = Vec::new();
    for r in &recs {
        match r {
            log::Rec::ToolResult { tool, bytes, .. } => {
                calls += 1;
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
                ..
            } => {
                in_tok += i;
                cached += c;
                out_tok += o;
                cost += m;
                usages += 1;
                if *c == 0 {
                    uncached_turns += 1;
                }
                if !upstream_id.is_empty() {
                    upstreams.push(upstream_id.as_str());
                }
            }
            _ => {}
        }
    }
    println!("сессия:  {id}");
    println!("файл:    {}", sess.path().display());
    println!("записей: {}", recs.len());
    println!("тулов:   {calls}");
    if let Some((b, name)) = biggest {
        println!("макс. результат: {b} Б ({name})");
    }
    println!("токены:  вход {in_tok} (из них кеш {cached}), выход {out_tok}");
    if usages > 0 {
        // Ходы без кеша — норма для первого хода и после смены апстрима. Если их
        // много подряд на одном апстриме — тогда промах наш, искать в проекции.
        println!("кеш:     ходов без кеша {uncached_turns} из {usages}");
        let distinct: std::collections::BTreeSet<&str> = upstreams.iter().copied().collect();
        if distinct.len() > 1 {
            println!("апстримы: {} (роутер мог увести запрос)", distinct.len());
        } else if let Some(u) = upstreams.first() {
            println!("апстрим: {u}");
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
    let path = cfg.sessions_path().join(format!("{id}.jsonl"));
    print!("{}", std::fs::read_to_string(&path)?);
    Ok(())
}

/// Что видит модель: те же summary/usage, что уезжают в определения тулов.
async fn cmd_tools(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let tools = registry::discover(&cfg).await;
    let bin = registry::sync_bin_dir(&cfg, &tools)?;
    println!("каталог тулов: {}", cfg.tools_path().display());
    println!("PATH (дети и человек): {}\n", bin.display());
    if tools.is_empty() {
        println!("(пусто)");
    }
    for t in &tools {
        println!("{}\n  {}\n  {}", t.name, t.summary, t.usage);
    }
    Ok(())
}

/// Собрать вшитые тулы и положить бинарники туда, откуда их берёт реестр:
/// `tools/<имя>/<имя>[.exe]`. Шаг входит в дефолтную установку, поэтому он в
/// харнессе, а не в sh-скрипте: на Windows sh нет.
fn cmd_build(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let tools_dir = cfg.tools_path();
    let root = tools_dir
        .parent()
        .ok_or("у tools_dir нет родителя — негде запускать cargo")?;

    let ok = std::process::Command::new("cargo")
        .args(["build", "--release", "--workspace"])
        .current_dir(root)
        .status()?;
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
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
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
