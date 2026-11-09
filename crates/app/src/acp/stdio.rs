use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use goldenagent_core::{AcpNotification, AcpRequest};
use super::server::{AcpServer, NotificationSender};

pub async fn run_stdio<R, W>(
    server: Arc<AcpServer>,
    reader: R,
    writer: W,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    let notify_writer = Arc::clone(&writer);
    let notifier: NotificationSender = Arc::new(move |notif: AcpNotification| {
        let w = Arc::clone(&notify_writer);
        tokio::spawn(async move {
            if let Ok(mut line) = serde_json::to_string(&notif) {
                line.push('\n');
                let mut lock = w.lock().await;
                let _ = lock.write_all(line.as_bytes()).await;
                let _ = lock.flush().await;
            }
        });
    });

    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: AcpRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": { "code": -32700, "message": format!("JSON 解析错误: {e}") }
                });
                let mut out = serde_json::to_string(&err_resp)?;
                out.push('\n');
                let mut lock = writer.lock().await;
                lock.write_all(out.as_bytes()).await?;
                lock.flush().await?;
                continue;
            }
        };

        if let Some(resp) = server.handle_request(req, Some(Arc::clone(&notifier))).await {
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            let mut lock = writer.lock().await;
            lock.write_all(out.as_bytes()).await?;
            lock.flush().await?;
        }
    }

    Ok(())
}
