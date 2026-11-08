use std::os::windows::io::RawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

static GLOBAL_JOB: OnceLock<WindowsJobObject> = OnceLock::new();

/// 获取进程级全局 Job Object 单例。
pub fn get_global_job() -> Result<&'static WindowsJobObject, String> {
    if let Some(job) = GLOBAL_JOB.get() {
        return Ok(job);
    }
    let job = WindowsJobObject::default_sandbox()?;
    let _ = GLOBAL_JOB.set(job);
    GLOBAL_JOB.get().ok_or_else(|| "获取全局 Job 失败".into())
}

/// 将指定的子进程句柄加入全局安全 Job Object。
pub fn assign_process_handle(process_handle: RawHandle) -> Result<(), String> {
    let job = get_global_job()?;
    job.assign_process(process_handle)
}

/// 将指定的子进程 PID 加入全局安全 Job Object。
pub fn assign_process_pid(pid: u32) -> Result<(), String> {
    unsafe {
        let handle = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(format!("打开进程 PID {pid} 句柄失败"));
        }
        let res = assign_process_handle(handle as RawHandle);
        CloseHandle(handle);
        res
    }
}

/// Windows 内核级作业对象（Job Object）封装。
/// 限制进程树最大并发进程数、内存配额，并在父进程退出时级联终止整个进程树。
pub struct WindowsJobObject {
    handle: HANDLE,
    is_valid: AtomicBool,
}

unsafe impl Send for WindowsJobObject {}
unsafe impl Sync for WindowsJobObject {}

impl WindowsJobObject {
    /// 创建并配置安全受限的 Job Object。
    pub fn create(max_active_processes: u32, memory_limit_bytes: usize) -> Result<Self, String> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err("创建 Windows Job Object 失败".into());
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.BasicLimitInformation.ActiveProcessLimit = max_active_processes.max(16);
            info.JobMemoryLimit = memory_limit_bytes.max(512 * 1024 * 1024);

            let res = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );

            if res == 0 {
                CloseHandle(handle);
                return Err("配置 Windows Job Object 限制失败".into());
            }

            Ok(Self {
                handle,
                is_valid: AtomicBool::new(true),
            })
        }
    }

    /// 创建默认配置的沙箱 Job Object（32 进程上限，2 GiB 内存配额，KillOnClose）。
    pub fn default_sandbox() -> Result<Self, String> {
        Self::create(32, 2 * 1024 * 1024 * 1024)
    }

    /// 将子进程句柄关联到该 Job Object。
    pub fn assign_process(&self, process_handle: RawHandle) -> Result<(), String> {
        if !self.is_valid.load(Ordering::SeqCst) {
            return Err("Job Object 句柄无效".into());
        }
        unsafe {
            let res = AssignProcessToJobObject(self.handle, process_handle as HANDLE);
            if res == 0 {
                return Err("将子进程加入 Job Object 失败".into());
            }
            Ok(())
        }
    }
}

impl Drop for WindowsJobObject {
    fn drop(&mut self) {
        if self.is_valid.swap(false, Ordering::SeqCst) && !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}
