//! Привязка жизни ядра к жизни окна.
//!
//! Зачем отдельный механизм, если в конце мы всё равно зовём `kill`. Потому что
//! `kill` — это «мы дошли до этой строки»: паника в setup, аварийное закрытие,
//! выгрузка процесса диспетчером задач — и ядро остаётся жить. А живое ядро
//! держит `surface-api/server.lock`; следующий запуск окна не может поднять
//! своё и молча умирает: в GUI-сборке паника не видна, шорткат «не работает».
//! Ровно это и случилось на живой машине.
//!
//! Job object решает это на уровне ОС: процесс ядра кладётся в job с флагом
//! `KILL_ON_JOB_CLOSE`, и когда закрывается последняя ручка job (она живёт в
//! нашем процессе), Windows убивает всё содержимое. Не зависит от того, как
//! именно умерло окно.
//!
//! На unix эквивалент — `setsid` + `kill` по группе; там окно и ядро и так
//! связаны родительством, и отдельного механизма не заводим.

/// Держатель job object. Живёт столько же, сколько приложение: ручка
/// закрывается вместе с ним, и ядро умирает.
pub struct Job {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// Ручки — просто числа, их можно передавать между потоками. `Send` — чтобы
// ручка могла уехать в `Daemon` вместе с процессом; `Sync` — потому что через
// неё ничего не пишется: `close(job)` потребляет `Job`, а `&Job` никуда не
// отдаётся, так что одновременный доступ только читающий и без гонок.
#[cfg(windows)]
unsafe impl Send for Job {}
#[cfg(windows)]
unsafe impl Sync for Job {}

/// Закрыть ручку, когда `Job` уронили без `close`.
///
/// Без `Drop` любой путь, где `Job` просто уронили, терял бы ручку навсегда —
/// и до выхода процесса defeated бы весь смысл `KILL_ON_JOB_CLOSE`.
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
        // `close()` ниже уже закрыл ручку и обнулил её — повторно не закрываем.
        self.handle = std::ptr::null_mut();
    }
}

/// Привязать уже запущенный процесс к новому job с `KILL_ON_JOB_CLOSE`.
///
/// `None` — привязка не удалась (старая ОС, ограниченные права); вызывающий это
/// переживёт, потому что явный `kill` при выходе остаётся.
#[cfg(windows)]
pub fn attach(child: &std::process::Child) -> Option<Job> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            // Не оставляем открытую ручку: attach зовётся на каждый перезапуск,
            // и утечка копилась бы до выхода окна.
            report("SetInformationJobObject");
            windows_sys::Win32::Foundation::CloseHandle(job);
            return None;
        }
        let handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        if AssignProcessToJobObject(job, handle) == 0 {
            report("AssignProcessToJobObject");
            windows_sys::Win32::Foundation::CloseHandle(job);
            return None;
        }
        Some(Job { handle: job })
    }
}

/// На unix отдельного механизма нет — и это единственное верное место, где
/// сказать честно: связи по родительству в unix НЕТ (при смерти окна ядро
/// переезжает под init и продолжает держать `surface-api/server.lock`). Что
/// есть на самом деле: `Daemon::stop()` в конце `app.run` и, главное, сам ядро
/// получает SIGKILL по смерти родителя только если его запускать через
/// `setsid`/`prctl` — чего здесь нет. То есть на unix эту дыру закрывает
/// исключительно явный `kill`; падение окна без него оставит ядро жить.
#[cfg(not(windows))]
pub fn attach(_child: &std::process::Child) -> Option<Job> {
    Some(Job {})
}

/// Код ошибки Win32 — в stderr: без него «привязка не удалась» ничем не
/// диагностируется (job может не даваться политикой или вложенностью).
#[cfg(windows)]
fn report(what: &str) {
    eprintln!(
        "snens-desktop: {what} не удалось: {}",
        std::io::Error::last_os_error()
    );
}

/// Закрыть ручку job, не убивая содержимое (нормальный выход: ядро уже убито
/// явно). `Drop` делает то же самое, но явный вызов не вредит: он обнуляет
/// ручку, чтобы `Drop` не закрыл её второй раз.
#[cfg(windows)]
pub fn close(mut job: Job) {
    unsafe {
        if !job.handle.is_null() {
            windows_sys::Win32::Foundation::CloseHandle(job.handle);
        }
    }
    job.handle = std::ptr::null_mut();
}

#[cfg(not(windows))]
pub fn close(_job: Job) {}
