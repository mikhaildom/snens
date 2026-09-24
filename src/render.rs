// Рендер терминала: один слой, через который проходит весь вывод агента, чтобы
// читаемый текст оставался как есть, а служебные блоки (рассуждение, вызовы)
// отличались. Панелей и своего скролла нет намеренно: скроллбек терминала —
// история диалога, выделение и копипаст работают.
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rustyline::ExternalPrinter;

/// Куда идёт вывод агента. Слой один: и живой стрим, и служебные строки проходят
/// через него. В TUI подмена на `ExternalPrinter` нужна, потому что поле ввода
/// живо всё время работы агента — вывод обязан печататься НАД приглашением, а не
/// поверх него.
pub trait Sink: Send {
    /// Печатать строку с переводом строки.
    fn line(&mut self, s: &str);
}

/// Обычный stdout: без активного приглашения (пайп, фоновый прогон, CLI).
struct Stdout;

impl Sink for Stdout {
    fn line(&mut self, s: &str) {
        println!("{s}");
    }
}

/// Печать над приглашением readline: он стирает строку ввода, печатает сообщение
/// и перерисовывает приглашение — так поток агента не рвёт ввод.
struct Printer(Box<dyn ExternalPrinter + Send>);

impl Sink for Printer {
    fn line(&mut self, s: &str) {
        let _ = self.0.print(format!("{s}\n"));
    }
}

/// Состояние вывода: цвет включаем только на терминале (в файл/пайп — plain),
/// а видимость рассуждения — переключатель, общий с поверхностью.
/// `sink` — разделяемый с задачей агента, поэтому за мьютексом.
pub struct Ui {
    pub color: bool,
    show_reasoning: AtomicBool,
    sink: Arc<Mutex<Box<dyn Sink>>>,
}

impl Default for Ui {
    fn default() -> Self {
        Ui {
            color: std::io::stdout().is_terminal(),
            show_reasoning: AtomicBool::new(true),
            sink: Arc::new(Mutex::new(Box::new(Stdout) as Box<dyn Sink>)),
        }
    }
}

impl Ui {
    /// Явно задать: фоновым прогонам (ревью/улучшение) вывод идёт в лог-файл,
    /// там цвет и рассуждение не нужны.
    pub fn plain() -> Self {
        Ui {
            color: false,
            show_reasoning: AtomicBool::new(false),
            sink: Arc::new(Mutex::new(Box::new(Stdout) as Box<dyn Sink>)),
        }
    }

    /// TUI: вывод печатается над приглашением, цвет включён (мы на терминале).
    pub fn with_printer(p: Box<dyn ExternalPrinter + Send>) -> Self {
        Ui {
            color: true,
            show_reasoning: AtomicBool::new(true),
            sink: Arc::new(Mutex::new(Box::new(Printer(p)) as Box<dyn Sink>)),
        }
    }

    /// Ссылка на приёмник вывода — её берёт `Stream` для стрима ответа.
    pub fn sink(&self) -> Arc<Mutex<Box<dyn Sink>>> {
        Arc::clone(&self.sink)
    }

    /// Строка вывода в общий приёмник (минуя разметку стилей).
    pub fn line(&self, s: &str) {
        lock(&self.sink).line(s);
    }

    fn emit(&self, s: String) {
        lock(&self.sink).line(&s);
    }

    pub fn show_reasoning(&self) -> bool {
        self.show_reasoning.load(Ordering::Relaxed)
    }

    pub fn toggle_reasoning(&self) -> bool {
        let v = !self.show_reasoning.load(Ordering::Relaxed);
        self.show_reasoning.store(v, Ordering::Relaxed);
        v
    }

    /// Строка вызова инструмента: третий стиль. Красным — если вызов не удался
    /// (ненулевой код или ошибка формата, приходящая как exit=-1).
    pub fn tool_call(&self, name: &str, hint: &str, exit: i32, bytes: usize, ms: u64, tail: &str) {
        let prefix = if self.color {
            if exit == 0 {
                "\x1b[36m" // cyan
            } else {
                "\x1b[31m" // red
            }
        } else {
            ""
        };
        let reset = if self.color { "\x1b[0m" } else { "" };
        self.emit(format!(
            "{prefix}[{name}] {hint} → exit={exit} {bytes} Б {ms} мс{tail}{reset}"
        ));
    }

    /// Голова результата инструмента человеку: приглушённо, чтобы не спорить с
    /// основным текстом. Модель видит результат целиком — это не для неё.
    pub fn tool_result(&self, preview: &str) {
        let (on, off) = if self.color {
            ("\x1b[2m", "\x1b[0m")
        } else {
            ("", "")
        };
        self.emit(format!("{on}{preview}{off}"));
    }

    pub fn notice(&self, text: &str) {
        self.emit(format!("[snens] {text}"));
    }
}

/// Мьютекс приёмника: паника в одном потоке не должна ронять печать в другом
/// (ядро не должно падать из-за косметики вывода). Блокировка внутри — через
/// `Box<dyn Sink>`: голый трейт-объект в мьютексе утаскивает 'static-границу.
fn lock(s: &Arc<Mutex<Box<dyn Sink>>>) -> std::sync::MutexGuard<'_, Box<dyn Sink>> {
    match s.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

/// Стриминг ответа ассистента со склейкой markdown-таблиц. Обычный текст уходит
/// по строкам сразу (живой поток), а блок таблицы придерживается до его конца и
/// рисуется рамкой: терминал получает читаемую таблицу, а не палку из `|`.
pub struct Stream {
    color: bool,
    in_reasoning: bool,
    line: String,
    /// Накопленные строки текущего табличного блока (все начинались с `|`).
    table: Vec<String>,
    /// Тот же приёмник, что у `Ui`: в TUI стрим печатается над приглашением.
    sink: Arc<Mutex<Box<dyn Sink>>>,
}

impl Stream {
    pub fn new(color: bool, sink: Arc<Mutex<Box<dyn Sink>>>) -> Self {
        Stream {
            color,
            in_reasoning: false,
            line: String::new(),
            table: Vec::new(),
            sink,
        }
    }

    fn emit(&mut self, s: &str) {
        lock(&self.sink).line(s);
    }

    /// Ответ ассистента: рассуждение заканчивается, текст печатается как есть.
    pub fn content(&mut self, s: &str) {
        self.end_reasoning();
        self.pump(s);
    }

    /// Чанк рассуждения: тот же поток символов, но без закрытия серого режима
    /// (его закрывает первый же `content`/`finish`).
    pub fn reasoning_chunk(&mut self, s: &str) {
        self.pump(s);
    }

    fn pump(&mut self, s: &str) {
        for ch in s.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line);
                self.emit_line(&line);
            } else {
                self.line.push(ch);
            }
        }
    }

    /// Начало рассуждения: серым и курсивом, чтобы визуально не путать с ответом.
    /// Стиль применяется к каждой строке отдельно (открыт+закрыт в `emit_line`):
    /// в TUI строка уходит в приглашение одним сообщением, а не потоком — нести
    /// состояние цвета между сообщениями нельзя.
    pub fn reasoning(&mut self, show: bool) {
        if show {
            self.in_reasoning = true;
        }
    }

    fn end_reasoning(&mut self) {
        if !self.in_reasoning {
            return;
        }
        // Хвост рассуждения без завершающего перевода строки не должен потеряться.
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.emit_line(&line);
        }
        self.in_reasoning = false;
    }

    /// Оформить строку по текущему режиму (рассуждение — серый курсив).
    fn style(&self, line: &str) -> String {
        if self.color && self.in_reasoning {
            format!("\x1b[90;3m{line}\x1b[0m")
        } else {
            line.to_string()
        }
    }

    fn emit_line(&mut self, line: &str) {
        if is_table_row(line) {
            self.table.push(line.to_string());
            return;
        }
        // Таблица кончилась: сначала рисуем её, потом текущую строку.
        self.flush_table();
        let styled = self.style(line);
        self.emit(&styled);
    }

    fn flush_table(&mut self) {
        if self.table.is_empty() {
            return;
        }
        let block = std::mem::take(&mut self.table);
        match table_align(&block) {
            Some(table) => {
                // Таблица рисуется рамкой — серый курсив к ней не применяем.
                self.emit(&table);
            }
            // Не таблица (нет разделителя) — печатаем строки как были.
            None => {
                for l in block {
                    let styled = self.style(&l);
                    self.emit(&styled);
                }
            }
        }
    }

    /// Конец стрима: добить недописанную строку и таблицу.
    pub fn finish(&mut self) {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.emit_line(&line);
        }
        self.flush_table();
        self.end_reasoning();
    }
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// Разделитель GFM: `|---|---|` (с опциональными `:` и пробелами).
fn is_separator(line: &str) -> bool {
    let t = line.trim().trim_matches('|').trim();
    if t.is_empty() {
        return false;
    }
    t.split('|').all(|c| {
        let c = c.trim();
        !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
    })
}

/// Рисует GFM-таблицу рамкой, выравнивая колонки по ширине содержимого.
/// Возвращает None, если разделитель не найден (тогда это не таблица).
fn table_align(lines: &[String]) -> Option<String> {
    if lines.len() < 2 || !is_separator(&lines[1]) {
        return None;
    }
    let cells = |l: &str| -> Vec<String> {
        let l = l.trim().trim_matches('|');
        l.split('|').map(|c| c.trim().to_string()).collect()
    };
    let rows: Vec<Vec<String>> = lines.iter().map(|l| cells(l)).collect();
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut width = vec![0usize; cols];
    for (i, r) in rows.iter().enumerate() {
        if i == 1 {
            continue; // разделитель в ширину не входит
        }
        for (c, cell) in r.iter().enumerate() {
            width[c] = width[c].max(cell.chars().count());
        }
    }
    // Выравнивание по `:` из разделителя.
    let seps = cells(&lines[1]);
    let align_right = |c: usize| -> bool {
        seps.get(c)
            .map(|s| s.ends_with(':') && !s.starts_with(':'))
            .unwrap_or(false)
    };
    let border = |l: &str, m: &str, r: &str| -> String {
        let mut s = String::from(l);
        for (c, w) in width.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if c + 1 == cols { r } else { m });
        }
        s
    };
    let mut out = String::new();
    let body: Vec<usize> = (0..rows.len()).filter(|i| *i != 1).collect();
    for (n, &i) in body.iter().enumerate() {
        if n == 0 {
            out.push_str(&border("┌", "┬", "┐"));
            out.push('\n');
        } else if n == 1 {
            out.push_str(&border("├", "┼", "┤"));
            out.push('\n');
        }
        let mut line = String::from("│");
        for (c, w) in width.iter().enumerate().take(cols) {
            let cell = rows[i].get(c).map(String::as_str).unwrap_or("");
            let pad = w - cell.chars().count();
            if align_right(c) {
                line.push(' ');
                line.push_str(&" ".repeat(pad));
                line.push_str(cell);
                line.push(' ');
            } else {
                line.push(' ');
                line.push_str(cell);
                line.push_str(&" ".repeat(pad));
                line.push(' ');
            }
            line.push('│');
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&border("└", "┴", "┘"));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gfm_table_is_aligned_with_borders() {
        let t = vec![
            "| тул | что |".to_string(),
            "|---|---|".to_string(),
            "| run | команда |".to_string(),
            "| edit | правка |".to_string(),
        ];
        let s = table_align(&t).expect("таблица не распознана");
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 6, "рамка из 6 строк: {s}");
        assert!(lines[0].starts_with('┌'), "{s}");
        assert!(lines[5].starts_with('└'), "{s}");
        // Ширина у всех рамок одинаковая.
        let w: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
        assert!(w.iter().all(|x| *x == w[0]), "колонки не выровнены: {w:?}");
    }

    #[test]
    fn right_alignment_from_separator() {
        let t = vec![
            "| a | b |".to_string(),
            "|---:|:--|".to_string(),
            "| 1 | 2 |".to_string(),
        ];
        let s = table_align(&t).unwrap();
        assert!(s.contains("│   1 │ 2 │") || s.contains("│ 1 │"), "{s}");
    }

    #[test]
    fn non_table_pipe_lines_pass_through() {
        let t = vec!["| не таблица".to_string(), "| просто труба".to_string()];
        assert!(table_align(&t).is_none());
    }
}
