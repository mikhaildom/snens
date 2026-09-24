// Свой читатель строки для TUI — замена rustyline.
//
// Две причины не использовать rustyline. Первая: вывод агента всё равно печатается
// МИМО редактора (через `Sink`), поэтому нативная отрисовка rustyline не нужна, а
// своя — не лишняя. Вторая, главная: редактор обязан уметь читать строку без эха и
// владеть терминалом — это понадобится, чтобы не отдавать sudo сырой `/dev/tty`, а
// спрашивать пароль у человека самим (askpass спрашивает через этот читатель).
//
// Шаг 1 (здесь): паритет с прежним поведением — приглашение, история, стрелки,
// backspace, Ctrl+C/Ctrl+D, и вывод агента НАД приглашением, не поверх него.
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use tokio::sync::mpsc::UnboundedSender;

use crate::render::Sink;

/// Что приходит от потока ввода в основной цикл.
#[derive(Debug)]
pub enum In {
    Line(String),
    /// Ctrl+C: на приглашении — «передумал», в ходе — отмена хода.
    Interrupt,
    Eof,
}

/// Состояние строки ввода, общее для потока ввода и приёмника вывода: вывод агента
/// приходит из задачи агента и обязан печататься НАД приглашением, для этого он
/// должен видеть буфер и курсор — отсюда общий `Inner` за мьютексом.
struct Inner {
    prompt: String,
    buf: String,
    /// Курсор — индекс символа в `buf` (не байт: ввод кириллицей многобайтный).
    cursor: usize,
    /// Сколько экранных строк занимает область ввода после последней отрисовки
    /// и в какой из них (0-based) стоит курсор. Нужны, чтобы при переносе строки
    /// подняться ровно к её началу, а не к началу текущей визуальной строки
    /// (иначе вывод дублирует уже нарисованное).
    rows: usize,
    cur_row: usize,
}

/// Запрос на секретный ввод (пароль): читатель уходит в режим без эха, ответ
/// отдаётся по каналу тому, кто ждёт (askpass-сервер).
pub struct SecretReq {
    pub tx: std::sync::mpsc::Sender<String>,
}

pub struct Term {
    inner: Mutex<Inner>,
    /// Ожидающий запрос пароля; забирает его поток чтения (он один владеет stdin).
    secret: Mutex<Option<SecretReq>>,
}

impl Term {
    pub fn new(prompt: &str) -> Term {
        Term {
            inner: Mutex::new(Inner {
                prompt: prompt.to_string(),
                buf: String::new(),
                cursor: 0,
                rows: 1,
                cur_row: 0,
            }),
            secret: Mutex::new(None),
        }
    }

    /// Попросить пароль: показать его приглашение и вернуть канал с ответом.
    /// Ответ придёт, когда поток чтения прочитает строку без эха.
    pub fn request_secret(&self, prompt: String) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        // Запрос регистрируем ДО показа приглашения: иначе символ, набранный
        // сразу после появления промпта, мог бы попасть в обычный буфер.
        *self.secret.lock().unwrap_or_else(|e| e.into_inner()) = Some(SecretReq { tx });
        let mut i = self.lock();
        let mut out = io::stdout();
        // Схлопываем многострочную область ввода к её началу и стираем её целиком:
        // приглашение пароля печатается на верхней строке области ввода.
        if i.cur_row > 0 {
            let _ = write!(out, "\x1b[{}A", i.cur_row);
        }
        let _ = write!(out, "\r\x1b[J{} ", prompt.trim_end());
        let _ = out.flush();
        i.rows = 1;
        i.cur_row = 0;
        rx
    }

    fn secret_take(&self) -> Option<SecretReq> {
        self.secret.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn insert(&self, ch: char) {
        let mut i = self.lock();
        let at = byte_of(&i.buf, i.cursor);
        i.buf.insert(at, ch);
        i.cursor += 1;
        render(&mut i);
    }

    pub fn backspace(&self) {
        let mut i = self.lock();
        if i.cursor > 0 {
            let at = byte_of(&i.buf, i.cursor - 1);
            i.buf.remove(at);
            i.cursor -= 1;
            render(&mut i);
        }
    }

    /// Delete под курсором (Ctrl+D при непустой строке).
    pub fn delete(&self) {
        let mut i = self.lock();
        if i.cursor < i.buf.chars().count() {
            let at = byte_of(&i.buf, i.cursor);
            i.buf.remove(at);
            render(&mut i);
        }
    }

    pub fn left(&self) {
        let mut i = self.lock();
        if i.cursor > 0 {
            i.cursor -= 1;
            render(&mut i);
        }
    }

    pub fn right(&self) {
        let mut i = self.lock();
        if i.cursor < i.buf.chars().count() {
            i.cursor += 1;
            render(&mut i);
        }
    }

    pub fn home(&self) {
        let mut i = self.lock();
        i.cursor = 0;
        render(&mut i);
    }

    pub fn end(&self) {
        let mut i = self.lock();
        i.cursor = i.buf.chars().count();
        render(&mut i);
    }

    /// Ctrl+← / Alt+←: перейти к началу предыдущего слова.
    pub fn word_left(&self) {
        let mut i = self.lock();
        i.cursor = prev_word(&i.buf, i.cursor);
        render(&mut i);
    }

    /// Ctrl+→ / Alt+→: перейти к началу следующего слова.
    pub fn word_right(&self) {
        let mut i = self.lock();
        i.cursor = next_word(&i.buf, i.cursor);
        render(&mut i);
    }

    /// Ctrl/Alt+Backspace: стереть слово слева от курсора.
    pub fn delete_word(&self) {
        let mut i = self.lock();
        let from = prev_word(&i.buf, i.cursor);
        let a = byte_of(&i.buf, from);
        let b = byte_of(&i.buf, i.cursor);
        i.buf.replace_range(a..b, "");
        i.cursor = from;
        render(&mut i);
    }

    /// Ctrl+K: стереть от курсора до конца строки.
    pub fn kill_to_end(&self) {
        let mut i = self.lock();
        let at = byte_of(&i.buf, i.cursor);
        i.buf.truncate(at);
        render(&mut i);
    }

    /// Ctrl+U: стереть строку целиком.
    pub fn clear_line(&self) {
        let mut i = self.lock();
        i.buf.clear();
        i.cursor = 0;
        render(&mut i);
    }

    /// Подстановка строки истории: курсор в конец.
    pub fn set_line(&self, s: &str) {
        let mut i = self.lock();
        i.buf = s.to_string();
        i.cursor = i.buf.chars().count();
        render(&mut i);
    }

    pub fn buf_is_empty(&self) -> bool {
        self.lock().buf.is_empty()
    }

    /// Забрать введённую строку: строка остаётся в скроллбеке (перевод строки),
    /// буфер очищается, приглашение перерисовывается.
    pub fn take_line(&self) -> String {
        let mut i = self.lock();
        let s = std::mem::take(&mut i.buf);
        i.cursor = 0;
        let mut out = io::stdout();
        collapse(&mut i, &mut out);
        let _ = write!(out, "\r\n");
        draw(&mut i, &mut out);
        let _ = out.flush();
        s
    }

    /// Перерисовать строку ввода (после запроса пароля).
    pub fn redraw(&self) {
        let mut i = self.lock();
        let mut out = io::stdout();
        draw(&mut i, &mut out);
        let _ = out.flush();
    }

    /// Печать строки НАД приглашением: свернуть область ввода к её началу, напечатать
    /// сообщение, перерисовать приглашение с буфером. Тем самым вывод агента не рвёт ввод.
    pub fn print_above(&self, s: &str) {
        let mut i = self.lock();
        let mut out = io::stdout();
        collapse(&mut i, &mut out);
        let _ = writeln!(out, "{}", s);
        draw(&mut i, &mut out);
        let _ = out.flush();
    }
}

/// Приёмник вывода для `Ui`: печатает над приглашением своего `Term`.
pub struct TermSink(pub Arc<Term>);

impl Sink for TermSink {
    fn line(&mut self, s: &str) {
        self.0.print_above(s);
    }
}

/// Отрисовка строки ввода с учётом переноса: стереть область ввода, вывести
/// приглашение и буфер, поставить курсор в его экранную позицию.
fn render(i: &mut Inner) {
    let mut out = io::stdout();
    draw(i, &mut out);
    let _ = out.flush();
}

/// Убрать с экрана ранее нарисованную область ввода: вернуться к её верхней строке,
/// стереть всё до конца экрана и оставить курсор в начале верхней строки.
fn collapse(i: &mut Inner, out: &mut impl Write) {
    if i.cur_row > 0 {
        let _ = write!(out, "\x1b[{}A", i.cur_row);
    }
    let _ = write!(out, "\r\x1b[J");
    i.rows = 1;
    i.cur_row = 0;
}

/// Нарисовать область ввода с текущего места: приглашение, буфер, переносы задаёт
/// сам терминал; курсор возвращается к экранной позиции, отвечающей `cursor`.
fn draw(i: &mut Inner, out: &mut impl Write) {
    // Стираем прежнюю область к её началу, чтобы не оставлять хвосты длинной строки.
    if i.cur_row > 0 {
        let _ = write!(out, "\x1b[{}A", i.cur_row);
    }
    let _ = write!(out, "\r\x1b[J{}{}", i.prompt, i.buf);

    let cols = terminal_cols().max(1);
    let pw = display_width(&i.prompt);
    // Число экранных строк и строка/колонка, где стоит курсор (0-based).
    let (rows, cur_row, cur_col) = cursor_pos(pw, &i.buf, i.cursor, cols);
    i.rows = rows;
    let end_row = rows - 1;
    i.cur_row = cur_row;
    // Курсор сейчас в конце нарисованного: поднимаемся с последней строки к строке
    // курсора, затем ставим колонку.
    let up = end_row - cur_row;
    if up > 0 {
        let _ = write!(out, "\x1b[{up}A");
    }
    let _ = write!(out, "\r");
    if cur_col > 0 {
        let _ = write!(out, "\x1b[{cur_col}C");
    }
}

/// Ширина терминала в колонках (80, если узнать не удалось).
#[cfg(unix)]
fn terminal_cols() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80
}

#[cfg(not(unix))]
fn terminal_cols() -> usize {
    80
}

/// Число экранных строк области ввода и позиция курсора (строка, колонка).
/// Перенос — по колонкам: приглашение и буфер идут сплошным потоком, `cols`
/// символов на строку. Строки 0-based, колонка — позиция внутри строки.
fn cursor_pos(prompt_w: usize, buf: &str, cursor: usize, cols: usize) -> (usize, usize, usize) {
    // Число колонок, напечатанных до курсора (приглашение + буфер до него).
    let n = prompt_w + buf.chars().take(cursor).count();
    let total = prompt_w + buf.chars().count();
    // Строк использовано: последняя строка занята, даже если ровно в границу.
    let rows = total.div_ceil(cols).max(1);
    // Терминал оставляет курсор на последней колонке (отложенный перенос), пока
    // не придёт следующий символ: позиция ровно на границе — это конец строки.
    let (row, col) = if n > 0 && n.is_multiple_of(cols) {
        (n / cols - 1, cols - 1)
    } else {
        (n / cols, n % cols)
    };
    (rows, row, col)
}

/// Экранная ширина строки (символы; углублённый учёт широких мы не ведём).
fn display_width(s: &str) -> usize {
    s.chars().count()
}

/// Начало слова слева от курсора (пропустить пробелы, затем непробелы).
fn prev_word(buf: &str, cursor: usize) -> usize {
    let chars: Vec<char> = buf.chars().collect();
    let mut i = cursor.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// Конец слова справа от курсора (пропустить пробелы, затем непробелы).
fn next_word(buf: &str, cursor: usize) -> usize {
    let chars: Vec<char> = buf.chars().collect();
    let len = chars.len();
    let mut i = cursor.min(len);
    while i < len && chars[i].is_whitespace() {
        i += 1;
    }
    while i < len && !chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// Разобрать тело CSI на финальный байт и его параметры: `1;5D` → ([1, 5], b'D').
/// Промежуточные байты (0x20..=0x2f) отбрасываются, пустой параметр даёт None.
fn split_csi(seq: &[u8]) -> (Vec<u32>, Option<u8>) {
    let Some((&last, head)) = seq.split_last() else {
        return (Vec::new(), None);
    };
    let final_byte = if (0x40..=0x7e).contains(&last) {
        Some(last)
    } else {
        return (Vec::new(), None);
    };
    let params = head
        .split(|b| *b == b';')
        .filter(|p| !p.is_empty() && p[0] != b'?')
        .filter_map(|p| std::str::from_utf8(p).ok()?.parse().ok())
        .collect();
    (params, final_byte)
}

/// Байтовое смещение символа с индексом `char_idx` (или конец строки).
fn byte_of(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Запустить поток ввода: читает нажатия, ведёт буфер, шлёт строки в основной цикл.
/// История (`hist`) — файл рядом с сессиями: она про человека, а не про сессию.
pub fn spawn(term: Arc<Term>, tx: UnboundedSender<In>, hist: PathBuf) {
    thread::spawn(move || {
        #[cfg(unix)]
        reader_loop(term, tx, hist);
        #[cfg(not(unix))]
        {
            let _ = (term, tx, hist);
        }
    });
}

#[cfg(unix)]
fn reader_loop(term: Arc<Term>, tx: UnboundedSender<In>, hist_path: PathBuf) {
    let mut hist = load_history(&hist_path);
    let mut hidx = hist.len();
    loop {
        // Ждём нажатие. Если в это время поступил запрос пароля (askpass),
        // первый же байт — начало пароля, и он не должен попасть в обычный буфер.
        let first = read_byte();
        if let Some(req) = term.secret_take() {
            let pw = read_secret_line(first);
            let _ = req.tx.send(pw);
            // Строку ввода после запроса перерисовываем как была.
            term.redraw();
            continue;
        }
        let Some(b) = first else {
            let _ = tx.send(In::Eof);
            break;
        };
        match b {
            0x0d | 0x0a => {
                let line = term.take_line();
                let t = line.trim().to_string();
                if t.is_empty() {
                    continue;
                }
                hist.push(t.clone());
                save_history(&hist_path, &hist);
                hidx = hist.len();
                if tx.send(In::Line(t)).is_err() {
                    break;
                }
            }
            // Ctrl+C: «передумал» — строка сбрасывается, ход отменяется основным циклом.
            0x03 => {
                term.take_line();
                if tx.send(In::Interrupt).is_err() {
                    break;
                }
            }
            // Ctrl+D: на пустой строке — выход, иначе удаление символа под курсором.
            0x04 => {
                if term.buf_is_empty() {
                    let _ = tx.send(In::Eof);
                    break;
                }
                term.delete();
            }
            0x7f | 0x08 => term.backspace(),
            0x01 => term.home(),
            0x05 => term.end(),
            0x0b => term.kill_to_end(),
            0x15 => term.clear_line(),
            // Escape-последовательности: стрелки, Home/End, Delete, модификаторы.
            0x1b => {
                if !poll_byte(30) {
                    continue;
                }
                match read_byte() {
                    Some(b'[') | Some(b'O') => {
                        // Собираем CSI: конечный байт — в диапазоне 0x40..=0x7e.
                        let mut seq: Vec<u8> = Vec::new();
                        while let Some(c) = read_byte() {
                            let final_byte = (0x40..=0x7e).contains(&c);
                            if final_byte {
                                seq.push(c);
                                break;
                            }
                            seq.push(c);
                            if seq.len() > 8 {
                                break;
                            }
                        }
                        let (params, final_byte) = split_csi(&seq);
                        // Модификатор: «;5» = Ctrl, «;3» = Alt (иначе — без него).
                        let ctrl = params.contains(&5);
                        let alt = params.contains(&3);
                        let word = ctrl || alt;
                        match final_byte {
                            Some(b'A') => {
                                if hidx > 0 {
                                    hidx -= 1;
                                    term.set_line(&hist[hidx]);
                                }
                            }
                            Some(b'B') => {
                                if hidx + 1 < hist.len() {
                                    hidx += 1;
                                    term.set_line(&hist[hidx]);
                                } else {
                                    hidx = hist.len();
                                    term.set_line("");
                                }
                            }
                            Some(b'C') => {
                                if word {
                                    term.word_right();
                                } else {
                                    term.right();
                                }
                            }
                            Some(b'D') => {
                                if word {
                                    term.word_left();
                                } else {
                                    term.left();
                                }
                            }
                            Some(b'H') => term.home(),
                            Some(b'F') => term.end(),
                            // «~»-последовательности: 3 = Delete, 1/7 = Home, 4/8 = End.
                            Some(b'~') => match params.first() {
                                Some(3) => term.delete(),
                                Some(1) | Some(7) => term.home(),
                                Some(4) | Some(8) => term.end(),
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                    // Alt+буква (meta без CSI): Alt+b / Alt+f как word-motion.
                    Some(b'b') => term.word_left(),
                    Some(b'f') => term.word_right(),
                    Some(0x7f | 0x08) => term.delete_word(),
                    _ => {}
                }
            }
            // Ctrl+Backspace (0x1f на многих терминалах) и Ctrl+W — стереть слово.
            0x17 | 0x1f => term.delete_word(),
            _ => {
                if b < 0x80 {
                    term.insert(b as char);
                } else if let Some(ch) = read_utf8(b) {
                    term.insert(ch);
                }
            }
        }
    }
}

/// Прочитать строку без эха (пароль): Ctrl+U/Ctrl+W и backspace правят буфер,
/// Enter завершает. На экран ничего не выводим — пароль не должен остаться
/// ни на терминале, ни в истории, ни в логе.
#[cfg(unix)]
fn read_secret_line(first: Option<u8>) -> String {
    let mut buf: Vec<char> = Vec::new();
    // Первый байт уже прочитан вызывающим (гонка запрос/нажатие): не теряем его.
    let mut next = first;
    loop {
        let b = match next.take() {
            Some(b) => b,
            None => match read_byte() {
                Some(b) => b,
                None => break,
            },
        };
        match b {
            0x0d | 0x0a => break,
            0x7f | 0x08 => {
                buf.pop();
            }
            0x15 => buf.clear(),
            0x17 => {
                while buf.pop().map(|c| c.is_whitespace()).unwrap_or(false) {}
                while buf.last().map(|c| !c.is_whitespace()).unwrap_or(false) {
                    buf.pop();
                }
            }
            0x03 => break,
            _ => {
                if b < 0x80 {
                    buf.push(b as char);
                } else if let Some(ch) = read_utf8(b) {
                    buf.push(ch);
                }
            }
        }
    }
    buf.into_iter().collect()
}

/// Собрать многобайтный символ UTF-8: ведущий байт уже прочитан.
#[cfg(unix)]
fn read_utf8(lead: u8) -> Option<char> {
    let need = if lead >> 5 == 0b110 {
        1
    } else if lead >> 4 == 0b1110 {
        2
    } else if lead >> 3 == 0b11110 {
        3
    } else {
        0
    };
    let mut bytes = vec![lead];
    for _ in 0..need {
        bytes.push(read_byte()?);
    }
    String::from_utf8(bytes).ok()?.chars().next()
}

fn load_history(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn save_history(path: &PathBuf, hist: &[String]) {
    let mut s = hist.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    let _ = std::fs::write(path, s);
}

// --- Низкоуровневый ввод: сырой режим и чтение нажатий (unix) ---

/// Сырой режим терминала на stdin: без канонического ввода и без эха — нажатия
/// приходят по одному байту, Ctrl+C не генерирует SIGINT, а читается как байт.
/// Оригинальные атрибуты восстанавливаются при `Drop` (выход из TUI — нормально).
#[cfg(unix)]
pub struct RawGuard {
    fd: i32,
    orig: libc::termios,
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}

#[cfg(unix)]
pub fn raw_mode() -> io::Result<RawGuard> {
    let fd = 0;
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        let orig = t;
        libc::cfmakeraw(&mut t);
        // Вывод оставляем с постобработкой: иначе `\n` не давал бы возврата каретки.
        t.c_oflag |= libc::OPOST | libc::ONLCR;
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(RawGuard { fd, orig })
    }
}

#[cfg(unix)]
fn read_byte() -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        return if n == 0 { None } else { Some(b[0]) };
    }
}

/// Есть ли байт на stdin в течение `ms` миллисекунд (ждём продолжение escape-кода,
/// но одиночный Escape не должен висеть).
#[cfg(unix)]
fn poll_byte(ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut p, 1, ms) > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Курсор считается в символах, а вставка/удаление — в байтах: многобайтный
    /// ввод (кириллица) не должен рвать строку пополам.
    #[test]
    fn byte_offset_counts_chars_not_bytes() {
        let s = "aбв"; // 1 + 2 + 2 байта
        assert_eq!(byte_of(s, 0), 0);
        assert_eq!(byte_of(s, 1), 1);
        assert_eq!(byte_of(s, 2), 3);
        assert_eq!(byte_of(s, 3), 5); // конец
        assert_eq!(byte_of(s, 99), s.len());
    }

    /// Разбор CSI: модификаторы стрелок и «~»-последовательности.
    #[test]
    fn csi_params() {
        assert_eq!(split_csi(b"D"), (vec![], Some(b'D')));
        assert_eq!(split_csi(b"1;5D"), (vec![1, 5], Some(b'D')));
        assert_eq!(split_csi(b"3~"), (vec![3], Some(b'~')));
        assert_eq!(split_csi(b""), (Vec::new(), None));
    }

    /// Геометрия переноса: позиция курсора и число строк.
    #[test]
    fn cursor_position_accounts_for_wrap() {
        // Ширина 10, приглашение «> » = 2 колонки.
        assert_eq!(cursor_pos(2, "", 0, 10), (1, 0, 2));
        // 8 символов доводят курсор до колонки 9 (последней).
        assert_eq!(cursor_pos(2, "abcdefgh", 8, 10), (1, 0, 9));
        // 9-й символ переносит на вторую строку; курсор после него — колонка 1.
        assert_eq!(cursor_pos(2, "abcdefghi", 9, 10), (2, 1, 1));
        // Ровно на границе (n % cols == 0) курсор стоит в конце строки (перенос
        // терминал ещё не сделал) — это одна экранная строка, колонка 9.
        assert_eq!(cursor_pos(0, "abcdefghij", 10, 10), (1, 0, 9));
    }

    /// Word-motion: переходы по словам в обе стороны.
    #[test]
    fn word_navigation() {
        // «one two  three»: индексы начала слов 0, 4, 9.
        let s = "one two  three";
        assert_eq!(prev_word(s, 0), 0);
        assert_eq!(prev_word(s, 5), 4);
        assert_eq!(prev_word(s, 13), 9);
        assert_eq!(next_word(s, 0), 3);
        assert_eq!(next_word(s, 3), 7);
        assert_eq!(next_word(s, 7), 14);
        assert_eq!(next_word(s, 14), 14);
    }

    /// Ctrl/Alt+Backspace стирает слово слева от курсора.
    #[test]
    fn delete_word_left() {
        let t = Term::new("> ");
        t.set_line("one two three");
        t.delete_word();
        assert_eq!(t.take_line(), "one two ");
        t.set_line("one two");
        t.home();
        t.delete_word();
        assert_eq!(t.take_line(), "one two");
    }

    /// Вставка и удаление по символьному курсору не портят UTF-8.
    #[test]
    fn insert_and_backspace_on_multibyte() {
        let t = Term::new("> ");
        t.insert('д');
        t.insert('а');
        t.insert('!');
        assert_eq!(t.take_line(), "да!");
        t.insert('я');
        t.backspace();
        assert_eq!(t.take_line(), "");
    }
}

#[cfg(not(unix))]
pub struct RawGuard;

#[cfg(not(unix))]
pub fn raw_mode() -> io::Result<RawGuard> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "сырой режим терминала не поддержан на этой платформе",
    ))
}
