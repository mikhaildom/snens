// Сборка системного промпта: вкомпилированный текст + файлы инстанса
// (soul/user/memory) + оглавление скиллов. Чистая функция от конфига и диска,
// потому и отдельно от разбора конфига.
use crate::cfg::Config;

pub fn system_prompt(cfg: &Config) -> String {
    let mut s = match &cfg.system_prompt {
        Some(p) => std::fs::read_to_string(p).unwrap_or_else(|_| default_system()),
        None => default_system(),
    };
    // Инстанс: кто я и что помню. Файлы правятся по ходу (агент — memory.md,
    // человек — user.md), поэтому читаются каждый ход, а не вкомпилированы.
    for f in ["soul.md", "user.md", "memory.md"] {
        if let Ok(text) = std::fs::read_to_string(cfg.data_path().join(f)) {
            if !text.trim().is_empty() {
                s.push_str(&format!("\n\n{text}"));
            }
        }
    }
    s.push_str(&skills_index(cfg));
    // Без этого модель угадывает, где она: относительные пути работают случайно.
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    // Дешёвый канал самоулучшения: неудобство фиксируется сразу, пока факт жив.
    let tickets = cfg.tickets_path().display().to_string();
    let memory = cfg.data_path().join("memory.md").display().to_string();
    s.push_str(&format!(
        "\nРабочий каталог: {cwd} — инструменты и команды выполняются в нём.\n\
         Заметил неудобство по ходу работы — тул путает контракт, вызов пришлось \
         повторять, вывод пришлось дочитывать — сразу допиши тикет в {tickets}: \
         markdown-файл с front-matter (status: open, kind, target, hits) и секциями \
         Симптом / Гипотеза / История. Не откладывай на потом.\n\
         Что куда: разовое неудобство — тикет; то же самое второй раз (тикет с hits ≥ 2) — \
         процедура или грабли, строка в подходящий скилл — это работа improve, а не твоя \
         по ходу дела; долгий факт, нужный в любой сессии, — строка в {memory} \
         (дописывая, сжимай, а не удлиняй)."
    ));
    // Пароль sudo: в TUI его спрашивают у человека через askpass (sudo в PATH
    // инструментов транслируется в `sudo -A`), поэтому агенту не надо бояться
    // sudo-команд — но и ждать в headless нечего: там ответа нет и sudo падает сразу.
    s.push_str(
        "\nКоманды с sudo выполняй обычным `run` — в интерактивной сессии пароль \
         спросят у пользователя (он вводится в поле ввода), ждать и обходить sudo не надо. \
         Если сессия неинтерактивная (фоновый прогон, подагент), пароля нет и sudo \
         откажет сразу — тогда попроси человека выполнить команду.",
    );
    // Подагенты: параллелизуемое стоит поручать, а не делать в одиночку. Правило
    // в промпте — единственное место, где модель узнаёт о возможности.
    s.push_str(
        "\nУ тебя есть подагенты (тул `agent`): отдельные сессии snens, которые работают \
         параллельно и переживают твой выход. Поручай им то, что распараллеливается и где \
         важен результат, а не процесс: исследование нескольких вариантов сразу, \
         независимые проверки, разбор незнакомого кода. Порядок: `agent start \"задача\"` \
         возвращает id → `agent wait <id>` ждёт итог → `agent result <id>` берёт финальный \
         текст. Не хватает данных — `agent ask \"вопрос\"` (только из подагента): вопрос \
         уйдёт родителю, ответ вернётся в ящик. Дай подагенту самодостаточную задачу с \
         критерием готовности: он не видит твой контекст, только формулировку.",
    );
    // Вложение — не текст: его не видно через read и не приносит run. Единственный
    // канал к модели — тул view, поэтому о нём надо сказать там, где модель ищет.
    if cfg.attachments {
        s.push_str(
            "\nИзображения (PNG/JPEG/GIF/WebP) смотри через тул view — он отправляет файл \
             модели картинкой; read и run картинок не передают. Скриншот или кадр сперва \
             сохрани в файл командой через run.",
        );
    }
    s
}

/// Оглавление скиллов: по строке на скилл, тело — только по требованию. Скилл,
/// о котором модель не знает, не существует, поэтому оглавление уезжает в каждый
/// промпт — как `--help` тулов в их описании.
fn skills_index(cfg: &Config) -> String {
    let dir = cfg.skills_path();
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(e.path().join("SKILL.md")) else {
                continue;
            };
            let (name, desc) = frontmatter(&text);
            let name = if name.is_empty() {
                e.file_name().to_string_lossy().into_owned()
            } else {
                name
            };
            rows.push((name, desc));
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    rows.sort();
    let list: String = rows.iter().map(|(n, d)| format!("- {n}: {d}\n")).collect();
    format!(
        "\nСкиллы ({}) — как работать в незнакомой части проекта: прочитай подходящий \
         прежде чем действовать, рядом с ним могут быть references/ с деталями и \
         scripts/ для запуска.\n{list}",
        dir.display()
    )
}

/// `name` и `description` из front-matter. Формат общий с Hermes: один и тот же
/// файл читают оба.
fn frontmatter(text: &str) -> (String, String) {
    let (mut name, mut desc) = (String::new(), String::new());
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (name, desc);
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            desc = v.trim().trim_matches('"').to_string();
        }
    }
    (name, desc)
}

fn default_system() -> String {
    "Ты агент. Работаешь через инструменты. `run` исполняет shell-команду. \
     Остальные инструменты — программы в PATH: у каждой есть `--help` с описанием и примерами.\n\
     Отвечай кратко. Перед изменением файла сначала прочитай его. \
     Не выдумывай результат команды — сначала выполни."
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Промпт — сборка из файлов инстанса, поэтому проверка одна: файлы попали,
    /// тело скилла не попало (только оглавление), и сборка детерминирована —
    /// иначе промахнётся префикс-кеш.
    #[test]
    fn prompt_takes_instance_files_and_skills_index() {
        let dir = std::env::temp_dir().join(format!("snens-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data/sessions")).unwrap();
        std::fs::create_dir_all(dir.join("data/skills/harness/references")).unwrap();
        std::fs::write(dir.join("data/soul.md"), "душа инстанса").unwrap();
        std::fs::write(dir.join("data/memory.md"), "память инстанса").unwrap();
        std::fs::write(
            dir.join("data/skills/harness/SKILL.md"),
            "---\nname: harness\ndescription: как работать над snens\n---\n\nтело скилла\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("data/skills/harness/references/portability.md"),
            "детали",
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.sessions_dir = dir.join("data/sessions").to_string_lossy().into_owned();
        cfg.skills_dir = dir.join("data/skills").to_string_lossy().into_owned();
        let p = system_prompt(&cfg);

        assert!(p.contains("душа инстанса"), "soul.md не попал в промпт");
        assert!(p.contains("память инстанса"), "memory.md не попал в промпт");
        assert!(
            p.contains("- harness: как работать над snens"),
            "нет оглавления скиллов: {p}"
        );
        assert!(
            !p.contains("тело скилла"),
            "тело скилла уехало в промпт вместо оглавления"
        );
        assert!(p.contains(&cfg.tickets_path().display().to_string()));
        assert_eq!(p, system_prompt(&cfg), "промпт недетерминирован");

        // Пустой каталог скиллов ничего не добавляет: чистая установка не платит за фичу.
        std::fs::remove_dir_all(dir.join("data/skills")).unwrap();
        assert!(!system_prompt(&cfg).contains("Скиллы"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
