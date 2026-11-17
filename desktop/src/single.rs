//! Один экземпляр приложения на пользователя.
//!
//! Зачем. У приложения одно окно и одно ядро с одним каталогом данных: второй
//! экземпляр либо не сможет взять лок ядра, либо начнёт вторую жизнь рядом.
//! Раньше это кончалось тем, что запуск по шорткату просто ничего не делал
//! (второй процесс падал в setup, а паника в GUI-сборке не видна) — снаружи
//! «приложение не перезапускается».
//!
//! Как связаны экземпляры. Второй не показывает окно сам: он просит об этом
//! ПЕРВЫЙ через локальный сокет, а первый зовёт `tauri::Window::show()`.
//!
//! Почему не Win32 `ShowWindow` из второго процесса (так было сначала и это
//! оказалось багом). Tauri помнит состояние своего окна. Показав окно в обход
//! него, мы получали рассинхрон: для Tauri окно оставалось спрятанным, и
//! следующий клик по крестику честно звал `hide()` — который становился
//! no-op, потому что «уже спрятано». Снаружи это выглядело так: окно закрыли
//! крестиком, а оно не закрывается; выйти можно только через трей. Ровно это
//! и поймал владелец на живой Windows.
//!
//! Файл-замок остаётся (он и определяет «первый экземпляр»): на Windows это
//! именованный мьютекс, на unix — файл с `flock`.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

use tauri::Manager;

/// Захватить право быть единственным экземпляром.
///
/// `true` — уже работает другой: просьба «покажи окно» отправлена, этому
/// процессу делать нечего.
pub fn already_running() -> bool {
    if !claim_lock() {
        // Просьба не дошла (устаревший файл порта, первый экземпляр уже умирает):
        // тогда мы не «уже работаем», а единственные — и обязаны открыть окно.
        // Раньше признак игнорировался, и запуск по шорткату ничего не делал.
        return request_show();
    }
    false
}

// --- Windows: именованный мьютекс ---

#[cfg(windows)]
fn claim_lock() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    // Имя включает сессию пользователя: два человека за одной машиной — это
    // два разных приложения, и мешать друг другу они не должны.
    let name: Vec<u16> = "Local\\snens-desktop-single"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // SAFETY: имя — `Vec<u16>` с завершающим NUL, живёт до конца вызова
        // (передаём указатель на его буфер). `null` вместо SECURITY_ATTRIBUTES
        // — это дефолтный DACL, что для Local\-мьютекса и нужно. `GetLastError`
        // читается сразу после вызова, без промежуточных вызовов Win32, иначе
        // код ошибки уже был бы перезаписан.
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            // Не смогли создать мьютекс — считаем себя первыми: лучше открыть
            // окно, чем не открыть ничего.
            return true;
        }
        // Ручка живёт до выхода процесса — намеренно не закрываем.
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

// --- unix: файл с flock ---

#[cfg(not(windows))]
fn claim_lock() -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    // Замок — в каталоге конфига пользователя, а не в общем /tmp: на машине с
    // несколькими людьми замок в /tmp отказывал бы второму пользователю
    // («уже работает»), хотя никакого его экземпляра нет. Права 0600 — чтобы
    // чужой не мог пересоздать файл и сбить flock.
    let Some(path) = port_file_path().map(|p| p.with_file_name("instance.lock")) else {
        return true;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
    else {
        return true;
    };
    extern "C" {
        fn flock(fd: i32, op: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        return false;
    }
    // Держим ручку до выхода процесса.
    std::mem::forget(file);
    true
}

// --- просьба «покажи окно» ---

/// Второй экземпляр: попросить первый показать окно. Не блокируется и не
/// падает, если первый уже умирает — тогда человек просто запустит ещё раз.
fn request_show() -> bool {
    let Some(path) = port_file_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(port) = text.trim().parse::<u16>() else {
        return false;
    };
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    s.write_all(b"show\n").is_ok() && s.flush().is_ok()
}

/// Идентификатор приложения — один на все места, где путь зависит от него.
const IDENT: &str = "ch.svuvi.snens";

/// Путь к файлу порта.
///
/// Функция одна на обоих концах: и писатель (первый экземпляр), и читатель
/// (второй) берут путь отсюда. Раньше писатель брал `app_config_dir()` Tauri, а
/// читатель строил путь сам по XDG/HOME — на macOS это разные каталоги
/// (`~/Library/Application Support`), и второй экземпляр вообще не находил
/// порт (ровно тот симптом, ради которого модуль и написан).
fn port_file_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)?
    } else if cfg!(target_os = "macos") {
        // Ровно то, что Tauri зовёт `app_config_dir` на macOS.
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))?
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
    };
    Some(base.join(IDENT).join("instance.port"))
}

/// Первый экземпляр: слушать просьбы и показывать окно. Вызывается из setup
/// после создания окна — до этого показывать нечего.
pub fn serve_show_requests(app: tauri::AppHandle) {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(_) => return,
    };
    let Ok(addr) = listener.local_addr() else {
        return;
    };
    // Порт — в тот же файл, откуда его читает второй экземпляр (тот же
    // `port_file_path`), иначе показ окна молча перестанет работать.
    let Some(path) = port_file_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&path, addr.port().to_string()).is_err() {
        return;
    }

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // Таймаут обязателен: локальный процесс может открыть соединение и
            // не прислать перевода строки. Без него этот поток встал бы на
            // чтении навсегда, и все следующие «покажи окно» игнорировались бы.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            if line.trim() != "show" {
                continue;
            }
            // Показ — ТОЛЬКО через Tauri: он ведёт состояние окна сам, и
            // обход его (Win32 ShowWindow) ломал следующий hide по крестику.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }
    });
}
