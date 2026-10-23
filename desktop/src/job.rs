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

// Ручка ядра Windows — просто число, её можно передавать между потоками.
#[cfg(windows)]
unsafe impl Send for Job {}
#[cfg(windows)]
unsafe impl Sync for Job {}

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
            return None;
        }
        let handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        if AssignProcessToJobObject(job, handle) == 0 {
            return None;
        }
        Some(Job { handle: job })
    }
}

/// На unix отдельного механизма нет: ядро и так умирает вместе с окном, а
/// явный `kill` при выходе есть в `Daemon::stop`.
#[cfg(not(windows))]
pub fn attach(_child: &std::process::Child) -> Option<Job> {
    Some(Job {})
}

/// Закрыть ручку job, не убивая содержимое (нормальный выход: ядро уже убито
/// явно). Отдельный метод, а не `Drop`, чтобы порядок был явным: сначала
/// `kill` + `wait`, потом закрытие.
#[cfg(windows)]
pub fn close(job: Job) {
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(job.handle);
    }
}

#[cfg(not(windows))]
pub fn close(_job: Job) {}
