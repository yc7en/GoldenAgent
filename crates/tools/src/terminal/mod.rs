use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// 过滤终端中的 ANSI 转义序列与控制字符，保留干净的可读文本。
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(&next) = chars.peek() {
                if next == ']' {
                    // OSC 序列: \x1b] ... (\x07 | \x1b\)
                    chars.next();
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                    continue;
                } else if next == '[' {
                    // CSI 序列: \x1b[ ... [a-zA-Z~]
                    chars.next();
                    while let Some(ch) = chars.next() {
                        if ch.is_ascii_alphabetic() || ch == '~' {
                            break;
                        }
                    }
                    continue;
                } else {
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// 单个有状态终端会话。
pub struct TerminalSession {
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    _master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    buffer: Arc<Mutex<Vec<u8>>>,
    last_output_time: Arc<Mutex<Instant>>,
    is_alive: Arc<AtomicBool>,
}

impl TerminalSession {
    pub fn spawn(id: String, name: String, cwd: PathBuf, shell: Option<&str>) -> Result<Self, String> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("打开 PTY 失败: {e}"))?;

        let shell_cmd = shell.unwrap_or({
            #[cfg(windows)]
            {
                "cmd.exe"
            }
            #[cfg(not(windows))]
            {
                "sh"
            }
        });

        let mut cmd = CommandBuilder::new(shell_cmd);
        cmd.cwd(&cwd);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("启动终端进程失败 ({shell_cmd}): {e}"))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("克隆 PTY 读取流失败: {e}"))?;
        let writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|e| format!("获取 PTY 写入流失败: {e}"))?,
        ));
        let master = Arc::new(Mutex::new(pair.master));

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let last_output_time = Arc::new(Mutex::new(Instant::now()));
        let is_alive = Arc::new(AtomicBool::new(true));

        let buf_clone = Arc::clone(&buffer);
        let time_clone = Arc::clone(&last_output_time);
        let alive_clone = Arc::clone(&is_alive);
        let writer_for_reader = Arc::clone(&writer);

        // 后台持续读取 PTY 输出，自动响应光标查询，更新活跃时间
        std::thread::spawn(move || {
            let mut read_buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut read_buf) {
                if n == 0 {
                    break;
                }
                // 自动回复 Windows ConPTY 的 DSR 光标位置探测 (\x1b[6n)
                if read_buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    if let Ok(mut w) = writer_for_reader.lock() {
                        let _ = w.write_all(b"\x1b[1;1R");
                        let _ = w.flush();
                    }
                }
                if let Ok(mut b) = buf_clone.lock() {
                    b.extend_from_slice(&read_buf[..n]);
                }
                if let Ok(mut t) = time_clone.lock() {
                    *t = Instant::now();
                }
            }
            alive_clone.store(false, Ordering::SeqCst);
        });

        Ok(Self {
            id,
            name,
            cwd,
            _master: master,
            writer,
            child: Arc::new(Mutex::new(child)),
            buffer,
            last_output_time,
            is_alive,
        })
    }

    /// 向终端写入输入（字符、换行、控制字符）
    pub fn write_input(&self, input: &str) -> Result<usize, String> {
        if !self.is_alive.load(Ordering::SeqCst) {
            return Err("终端已退出，无法写入".into());
        }
        let mut w = self
            .writer
            .lock()
            .map_err(|_| "获取终端写入锁失败".to_string())?;
        
        let bytes = input.as_bytes();
        w.write_all(bytes)
            .map_err(|e| format!("写入终端失败: {e}"))?;
        w.flush().map_err(|e| format!("刷新终端写入失败: {e}"))?;
        Ok(bytes.len())
    }

    /// 带静默检测（Inactivity Detection）读取屏幕增量。
    ///
    /// 当持续超过 `inactivity_threshold`（默认 400ms）没有新输出且缓冲区非空时，判定当前指令完成或等待输入，返回增量。
    pub async fn read_output(
        &self,
        timeout_ms: u64,
        inactivity_ms: u64,
    ) -> (String, bool, bool) {
        let start = Instant::now();
        let max_timeout = Duration::from_millis(timeout_ms.max(500));
        let inactivity = Duration::from_millis(inactivity_ms.max(200));

        loop {
            let (has_bytes, last_active) = {
                let b = self.buffer.lock().unwrap();
                let t = *self.last_output_time.lock().unwrap();
                (!b.is_empty(), t)
            };

            let alive = self.is_alive.load(Ordering::SeqCst);

            // 如果已经拿到输出，且静默时间已超过阈值（或者进程已死），立即返回增量
            if has_bytes && (last_active.elapsed() >= inactivity || !alive) {
                let raw_bytes = {
                    let mut b = self.buffer.lock().unwrap();
                    std::mem::take(&mut *b)
                };
                let raw_str = String::from_utf8_lossy(&raw_bytes);
                let clean = strip_ansi(&raw_str);
                return (clean, alive, true);
            }

            // 如果进程已经死了且没有缓存了
            if !alive && !has_bytes {
                return (String::new(), false, false);
            }

            // 超时保护
            if start.elapsed() >= max_timeout {
                let raw_bytes = {
                    let mut b = self.buffer.lock().unwrap();
                    std::mem::take(&mut *b)
                };
                let raw_str = String::from_utf8_lossy(&raw_bytes);
                let clean = strip_ansi(&raw_str);
                return (clean, alive, false);
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 终止终端进程
    pub fn kill(&self) -> Result<(), String> {
        self.is_alive.store(false, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
        Ok(())
    }

    pub fn is_alive(&self) -> bool {
        self.is_alive.load(Ordering::SeqCst)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// 常驻终端管理器。
pub struct TerminalManager {
    sessions: RwLock<HashMap<String, Arc<TerminalSession>>>,
    counter: AtomicU64,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            counter: AtomicU64::new(1),
        }
    }

    pub fn create_session(
        &self,
        name: Option<String>,
        cwd: PathBuf,
        shell: Option<&str>,
    ) -> Result<Arc<TerminalSession>, String> {
        let seq = self.counter.fetch_add(1, Ordering::SeqCst);
        let id = format!("term-{seq}");
        let session_name = name.unwrap_or_else(|| format!("Terminal {seq}"));

        let session = Arc::new(TerminalSession::spawn(
            id.clone(),
            session_name,
            cwd,
            shell,
        )?);

        let mut lock = self
            .sessions
            .write()
            .map_err(|_| "获取会话表写锁失败".to_string())?;
        lock.insert(id, Arc::clone(&session));
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<TerminalSession>> {
        let lock = self.sessions.read().ok()?;
        lock.get(id).cloned()
    }

    pub fn list(&self) -> Vec<(String, String, PathBuf, bool)> {
        let Ok(lock) = self.sessions.read() else {
            return Vec::new();
        };
        lock.values()
            .map(|s| (s.id.clone(), s.name.clone(), s.cwd.clone(), s.is_alive()))
            .collect()
    }

    pub fn kill(&self, id: &str) -> Result<bool, String> {
        let mut lock = self
            .sessions
            .write()
            .map_err(|_| "获取会话表写锁失败".to_string())?;
        if let Some(session) = lock.remove(id) {
            session.kill()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn clear(&self) {
        if let Ok(mut lock) = self.sessions.write() {
            for session in lock.values() {
                let _ = session.kill();
            }
            lock.clear();
        }
    }
}

impl std::fmt::Debug for TerminalManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalManager")
            .field("sessions", &self.list().len())
            .finish()
    }
}

pub mod tools;
