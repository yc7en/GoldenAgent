pub mod env;

#[cfg(windows)]
pub mod windows;

#[cfg(not(windows))]
pub mod unix;

pub use env::{apply_to_pty_command, apply_to_tokio_command, get_sanitized_env, is_sensitive};

#[cfg(windows)]
pub use windows::{assign_process_handle, assign_process_pid, WindowsJobObject};
