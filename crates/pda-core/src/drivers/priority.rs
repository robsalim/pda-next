//! Приоритет процесса и потоков опроса.
//!
//! Драйверы PLC/Modbus работают в выделенном tokio-рантайме, потоки которого
//! поднимаются до HIGHEST, а процесс — до ABOVE_NORMAL. Так циклы опроса не
//! конкурируют с HTTP/WS/журналом БД за рабочие потоки и не плывут при нагрузке.
//! На не-Windows все функции — no-op.
#![allow(non_snake_case, dead_code)]

/// Поднять приоритет процесса выше обычного (ABOVE_NORMAL). Вызывать один раз
/// при старте сервера/приложения.
pub fn set_process_priority() {
    #[cfg(windows)]
    unsafe {
        if win::SetPriorityClass(win::GetCurrentProcess(), win::ABOVE_NORMAL_PRIORITY_CLASS) == 0 {
            tracing::warn!("[Priority] не удалось поднять приоритет процесса");
        }
    }
}

/// Поднять приоритет ТЕКУЩЕГО потока до HIGHEST. Вызывается из on_thread_start
/// рантайма опроса, то есть внутри каждого его worker-потока.
pub fn set_thread_priority() {
    #[cfg(windows)]
    unsafe {
        if win::SetThreadPriority(win::GetCurrentThread(), win::THREAD_PRIORITY_HIGHEST) == 0 {
            tracing::warn!("[Priority] не удалось поднять приоритет потока опроса");
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
    pub const THREAD_PRIORITY_HIGHEST: i32 = 2;

    #[link(name = "kernel32")]
    extern "system" {
        /// Псевдо-дескриптор текущего процесса ((HANDLE)-1).
        pub fn GetCurrentProcess() -> *mut c_void;
        /// Псевдо-дескриптор текущего потока ((HANDLE)-2).
        pub fn GetCurrentThread() -> *mut c_void;
        /// Возвращает BOOL ( nonzero = success ).
        pub fn SetPriorityClass(h_process: *mut c_void, priority_class: u32) -> i32;
        /// Возвращает BOOL ( nonzero = success ).
        pub fn SetThreadPriority(h_thread: *mut c_void, priority: i32) -> i32;
    }
}
