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
        {
            let mut out = io::stdout();
            let _ = write!(out, "\r\x1b[K{} ", prompt.trim_end());
            let _ = out.flush();
        }
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
        render(&i);
    }

    pub fn backspace(&self) {
        let mut i = self.lock();
        if i.cursor > 0 {
            let at = byte_of(&i.buf, i.cursor - 1);
            i.buf.remove(at);
            i.cursor -= 1;
            render(&i);
        }
    }

    /// Delete под курсором (Ctrl+D при непустой строке).
    pub fn delete(&self) {
        let mut i = self.lock();
        if i.cursor < i.buf.chars().count() {
            let at = byte_of(&i.buf, i.cursor);
            i.buf.remove(at);
            render(&i);
        }
    }

    pub fn left(&self) {
        let mut i = self.lock();
        if i.cursor > 0 {
            i.cursor -= 1;
            render(&i);
        }
    }

    pub fn right(&self) {
        let mut i = self.lock();
        if i.cursor < i.buf.chars().count() {
            i.cursor += 1;
            render(&i);
        }
    }

    pub fn home(&self) {
        let mut i = self.lock();
        i.cursor = 0;
        render(&i);
    }

    pub fn end(&self) {
        let mut i = self.lock();
        i.cursor = i.buf.chars().count();
        render(&i);
    }

    /// Ctrl+K: стереть от курсора до конца строки.
    pub fn kill_to_end(&self) {
        let mut i = self.lock();
        let at = byte_of(&i.buf, i.cursor);
        i.buf.truncate(at);
        render(&i);
    }

    /// Ctrl+U: стереть строку целиком.
    pub fn clear_line(&self) {
        let mut i = self.lock();
        i.buf.clear();
        i.cursor = 0;
        render(&i);
    }

    /// Подстановка строки истории: курсор в конец.
    pub fn set_line(&self, s: &str) {
        let mut i = self.lock();
        i.buf = s.to_string();
        i.cursor = i.buf.chars().count();
        render(&i);
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
        let _ = writeln!(out);
        render(&i);
        s
    }

    /// Перерисовать строку ввода (после запроса пароля).
    pub fn redraw(&self) {
        render(&self.lock());
    }

    /// Печать строки НАД приглашением: стереть строку ввода, напечатать сообщение,
    /// перерисовать приглашение с буфером. Тем самым вывод агента не рвёт ввод.
    pub fn print_above(&self, s: &str) {
        let i = self.lock();
        let mut out = io::stdout();
        let _ = write!(out, "\r\x1b[K{}\n", s);
        let _ = write!(out, "\r\x1b[K{}", i.prompt);
        let _ = write!(out, "{}", i.buf);
        let back = i.buf.chars().count().saturating_sub(i.cursor);
        if back > 0 {
            let _ = write!(out, "\x1b[{back}D");
        }
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

/// Отрисовка строки ввода: стереть текущую строку, вывести приглашение и буфер,
/// вернуть курсор на нужную позицию (сдвиг влево на число символов после курсора).
fn render(i: &Inner) {
    let mut out = io::stdout();
    let _ = write!(out, "\r\x1b[K{}", i.prompt);
    let _ = write!(out, "{}", i.buf);
    let back = i.buf.chars().count().saturating_sub(i.cursor);
    if back > 0 {
        let _ = write!(out, "\x1b[{back}D");
    }
    let _ = out.flush();
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
            // Escape-последовательности стрелок и Home/End.
            0x1b => {
                if !poll_byte(30) {
                    continue;
                }
                match read_byte() {
                    Some(b'[') | Some(b'O') => match read_byte() {
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
                        Some(b'C') => term.right(),
                        Some(b'D') => term.left(),
                        Some(b'H') => term.home(),
                        Some(b'F') => term.end(),
                        _ => {}
                    },
                    _ => {}
                }
            }
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
