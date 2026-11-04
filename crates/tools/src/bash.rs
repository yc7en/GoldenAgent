use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use goldenagent_core::{Error, Result, ToolKind, ToolSpec};
use serde_json::{json, Value};

use crate::{Tool, ToolContext};

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 上限压到 10 分钟：再长的命令该由用户自己开终端跑，
/// 而不是让一次工具调用把整个会话挂在那儿。
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// 返回给模型的输出上限。超了截断并注明——
/// 这条只影响模型看到的量，不影响命令本身执行。
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash",
            "在工作区内执行一条 shell 命令并返回退出码与输出（stdout 与 stderr 合并展示）。\
             默认 120 秒超时，最长 600 秒；不会等待标准输入。\
             注意：这条命令能读写工作区之外的任何东西，所以它属于执行类工具，需要单独授权。",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令，Windows 走 cmd /C，其他平台走 sh -c" },
                    "cwd": { "type": "string", "description": "工作目录（相对工作区），默认工作区根" },
                    "timeout_secs": { "type": "integer", "description": "超时秒数，默认 120，最大 600" }
                },
                "required": ["command"]
            }),
            ToolKind::Exec,
        )
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> Result<String> {
        let command = crate::required_str(&input, "command")?;
        let cwd = ctx.resolve(input.get("cwd").and_then(Value::as_str).unwrap_or("."))?;
        if !cwd.is_dir() {
            return Err(Error::Tool(format!(
                "工作目录 {} 不存在或不是目录",
                cwd.display()
            )));
        }

        let timeout_secs = input
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        let mut builder = shell_command(command);
        builder
            .current_dir(&cwd)
            // 不接 stdin：命令若是 `read` 之类，会一直等到超时，那是最难解释的一种"卡住"。
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let started = Instant::now();
        let mut child = builder
            .spawn()
            .map_err(|e| Error::Tool(format!("启动命令失败: {e}")))?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Tool("无法接管命令的标准输出".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Tool("无法接管命令的标准错误".into()))?;

        // 自己拼 wait + 读输出，而不是 `wait_with_output`：
        // 那个方法会把 `Child` 消费掉，超时就没法再显式 kill 一次。
        let run = async {
            let (out, err, status) = tokio::join!(
                slurp(&mut stdout),
                slurp(&mut stderr),
                child.wait()
            );
            (out, err, status)
        };

        let (raw_stdout, raw_stderr, status) =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), run).await {
                Ok((out, err, status)) => (out, err, status.ok()),
                Err(_) => {
                    terminate_tree(&mut child).await;
                    return Err(Error::Tool(format!(
                        "命令超过 {timeout_secs} 秒仍未结束，已终止"
                    )));
                }
            };

        let elapsed = started.elapsed().as_millis();
        let status = match status.and_then(|s| s.code()) {
            Some(code) => format!("退出码 {code}"),
            None => "进程被信号终止".to_string(),
        };

        let mut body = String::new();
        let stdout = String::from_utf8_lossy(&raw_stdout);
        if !stdout.trim().is_empty() {
            body.push_str(&stdout);
        }
        let stderr = String::from_utf8_lossy(&raw_stderr);
        if !stderr.trim().is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str("--- stderr ---\n");
            body.push_str(&stderr);
        }

        if body.trim().is_empty() {
            return Ok(format!("{status} · {elapsed}ms（无输出）"));
        }

        let (body, clipped) = clip(&body, MAX_OUTPUT_BYTES);
        let mut out = format!("{status} · {elapsed}ms\n{body}");
        if clipped {
            out.push_str(&format!(
                "\n…（输出超过 {MAX_OUTPUT_BYTES} 字节已截断，可加管道只取需要的部分）"
            ));
        }
        Ok(out)
    }
}

/// 杀掉整棵进程树。
///
/// **只杀 shell 是不够的**：`cmd /C ping ...` 里 shell 只是个壳，
/// 真正的子进程会活下来继续占着输出管道，于是"已经超时返回"的调用
/// 在后台又跑了几十秒。Unix 同理（`sh -c 'long &'`）。
///
/// 所以 spawn 时先让子进程自成进程组，超时时按组杀。
async fn terminate_tree(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else {
        // 已经退出了，没什么可杀的。
        let _ = child.wait().await;
        return;
    };

    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    #[cfg(unix)]
    {
        // 前缀的负号表示"整个进程组"，这正是 spawn 时那个 process_group(0) 的用途。
        let _ = tokio::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    // 上面的命令不保证在所有平台上都成功，再兜一次直接 kill。
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Windows 用 `cmd /C`，别处用 `sh -c`。
///
/// 不解析命令、不做参数拆分：模型给的就是一整条 shell 命令，
/// 自己拆反而会把引号和管道弄坏。
///
/// 先建 `std::process::Command` 是为了在 Windows 上设 `CREATE_NO_WINDOW`——
/// 从 GUI 进程起控制台程序会闪一个黑窗，每次调用闪一下很难看。
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    let std_cmd = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    };

    #[cfg(not(windows))]
    let std_cmd = {
        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        // 自成进程组，超时时才能按组一起杀（见 terminate_tree）。
        cmd.process_group(0);
        cmd
    };

    tokio::process::Command::from(std_cmd)
}

/// 读干一条管道。读出错就当读完了——命令已经把退出码告诉我们了，
/// 这里再抛一个"读管道失败"只会把真正的原因盖掉。
async fn slurp<R>(reader: &mut R) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    buf
}

/// 按字符边界截断，别把多字节字符切成半个。
///
/// 判据是"这个字符**结束**时还在限内"，不是"起点在限内"——
/// 后者会多带一个字符，让结果超过上限。
fn clip(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let cut = text
        .char_indices()
        .take_while(|(index, ch)| index + ch.len_utf8() <= limit)
        .last()
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);
    (text[..cut].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_keeps_utf8_boundaries() {
        let text = "中".repeat(100);
        let (out, clipped) = clip(&text, 10);
        assert!(clipped);
        assert_eq!(out.len(), 9, "一个汉字 3 字节，10 字节只能放 3 个");
        assert!(out.chars().all(|c| c == '中'));
    }

    #[test]
    fn clip_leaves_short_text_alone() {
        assert_eq!(clip("abc", 10), ("abc".to_string(), false));
        assert_eq!(clip("abc", 3), ("abc".to_string(), false));
    }
}
