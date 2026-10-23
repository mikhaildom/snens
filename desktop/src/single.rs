//! Один экземпляр приложения на пользователя.
//!
//! Зачем. У приложения одно окно и одно ядро с одним каталогом данных: второй
//! экземпляр либо не сможет взять лок ядра, либо начнёт вторую жизнь рядом.
//! Раньше это кончалось тем, что запуск по шорткату просто ничего не делал
//! (второй процесс падал в setup, а паника в GUI-сборке не видна) — снаружи
//! «приложение не перезапускается».
//!
//! Механизм без внешних зависимостей: именованный мьютекс Windows (на unix —
//! файл с `flock`). Имя включает пользователя, потому что два человека на
//! одной машине — это два разных приложения.
//!
//! Второй экземпляр не молчит: он показывает окно первого (сообщение
//! зарегистрированного класса окна) и выходит.

/// Что делать второму экземпляру: показать уже открытое окно и уйти.
#[cfg(windows)]
pub fn already_running() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    let name: Vec<u16> = "Local\\snens-desktop-single"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if handle.is_null() {
            // Не смогли создать мьютекс — считаем, что мы первые: лучше открыть
            // окно, чем не открыть ничего.
            return false;
        }
        if GetLastError() != ERROR_ALREADY_EXISTS {
            // Ручка живёт до выхода процесса — намеренно не закрываем.
            return false;
        }
        // Уже работает: поднимаем окно первого экземпляра и уходим.
        //
        // Ищем по ЗАГОЛОВКУ, а не по классу: класс окна задаёт сам Tauri, и
        // подставлять туда своё имя значило бы зависеть от его версии. Заголовок
        // наш (`open_window` ставит «snens»), и он же — то, что человек видит в
        // панели задач.
        let title: Vec<u16> = "snens".encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
        true
    }
}

#[cfg(not(windows))]
pub fn already_running() -> bool {
    use std::io::Write;
    let path = std::env::temp_dir().join("snens-desktop-single.lock");
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
    {
        Ok(mut f) => {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc_flock(f.as_raw_fd()) };
            if rc == 0 {
                // Наш лок: подержим ручку до выхода, а pid запишем для другого.
                let _ = write!(f, "{}", std::process::id());
                std::mem::forget(f);
                false
            } else {
                true
            }
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32) -> i32;
}

extern "C" {}
