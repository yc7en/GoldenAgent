use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use goldenagent_app::acp::{run_stdio, AcpServer};
use goldenagent_app::{AgentConfig, AgentService};
use goldenagent_core::{AcpNotification, AcpRequest};
use goldenagent_provider::{ProviderConfig, Provider};

fn make_service(name: &str) -> (Arc<AgentService>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("ga_acp_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let provider: Arc<dyn Provider> = ProviderConfig::mock().build().unwrap();
    let config = AgentConfig::new(&dir)
        .with_model("mock")
        .with_session_dir(dir.join(".sessions"))
        .with_state_dir(&dir);
    let service = Arc::new(AgentService::new(config, provider));
    (service, dir)
}

#[tokio::test]
async fn test_acp_ping_and_session_lifecycle() {
    let (service, dir) = make_service("lifecycle");
    let server = AcpServer::new(service);

    // 1. ping
    let ping_req = AcpRequest::new(1, "ping", json!({}));
    let ping_resp = server.handle_request(ping_req, None).await.expect("应当有响应");
    assert_eq!(ping_resp.id, json!(1));
    assert_eq!(ping_resp.result.unwrap(), json!({ "pong": true }));

    // 2. session/create
    let create_req = AcpRequest::new(2, "session/create", json!({}));
    let create_resp = server.handle_request(create_req, None).await.expect("应当有响应");
    let sess_id = create_resp.result.unwrap().get("session_id").unwrap().as_str().unwrap().to_string();
    assert!(!sess_id.is_empty());

    // 3. session/list (空会话尚未落盘，列表为空)
    let list_req = AcpRequest::new(3, "session/list", json!({}));
    let list_resp = server.handle_request(list_req, None).await.expect("应当有响应");
    let list_val = list_resp.result.unwrap();
    let sessions = list_val.get("sessions").unwrap().as_array().unwrap();
    assert!(sessions.is_empty());

    // 4. 对话一次后自动落盘
    let chat_req = AcpRequest::new(4, "agent/chat", json!({ "prompt": "初始化第一条消息" }));
    let chat_resp = server.handle_request(chat_req, None).await.expect("应当有响应");
    assert!(chat_resp.result.is_some());

    // 5. 再次查询，此时会话已成功落地
    let list_req2 = AcpRequest::new(5, "session/list", json!({}));
    let list_resp2 = server.handle_request(list_req2, None).await.expect("应当有响应");
    let list_val2 = list_resp2.result.unwrap();
    let sessions2 = list_val2.get("sessions").unwrap().as_array().unwrap();
    assert_eq!(sessions2.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_acp_chat_and_stream_notifications() {
    let (service, dir) = make_service("chat");
    let server = AcpServer::new(service);

    let notifications = Arc::new(StdMutex::new(Vec::new()));
    let notif_clone = Arc::clone(&notifications);

    let chat_req = AcpRequest::new(42, "agent/chat", json!({
        "prompt": "你好，测试 ACP 会话"
    }));

    let resp = server.handle_request(chat_req, Some(Arc::new(move |n: AcpNotification| {
        let mut lock = notif_clone.lock().unwrap();
        lock.push(n);
    }))).await.expect("应当有响应");

    assert_eq!(resp.id, json!(42));
    assert!(resp.result.is_some());

    // 验证收到了流式 event/stream 通知
    let notifs = notifications.lock().unwrap();
    assert!(!notifs.is_empty());
    assert!(notifs.iter().any(|n| n.method == "event/stream"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_acp_card_interaction_flow() {
    let (service, dir) = make_service("card");
    let server = AcpServer::new(service);

    let notifications = Arc::new(StdMutex::new(Vec::new()));
    let notif_clone = Arc::clone(&notifications);

    let interact_req = AcpRequest::new(99, "card/interact", json!({
        "card_id": "card_confirm_delete",
        "action": "confirm",
        "data": { "confirmed": true, "reason": "用户已人工核对" }
    }));

    let resp = server.handle_request(interact_req, Some(Arc::new(move |n: AcpNotification| {
        let mut lock = notif_clone.lock().unwrap();
        lock.push(n);
    }))).await.expect("应当有响应");

    assert_eq!(resp.id, json!(99));
    let res = resp.result.unwrap();
    assert_eq!(res.get("status").unwrap(), "interaction_processed");

    // 验证广播了 card/interacted 通知
    let notifs = notifications.lock().unwrap();
    assert!(notifs.iter().any(|n| n.method == "card/interacted"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_acp_stdio_runner() {
    let (service, dir) = make_service("stdio");
    let server = Arc::new(AcpServer::new(service));

    let (client_read, server_write) = tokio::io::duplex(4096);
    let (server_read, mut client_write) = tokio::io::duplex(4096);

    let runner = tokio::spawn(async move {
        let _ = run_stdio(server, server_read, server_write).await;
    });

    // 发送 ping 请求
    let ping_line = format!("{}\n", serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": "req-1",
        "method": "ping",
        "params": {}
    })).unwrap());
    client_write.write_all(ping_line.as_bytes()).await.unwrap();

    let mut client_lines = tokio::io::BufReader::new(client_read);
    let mut line = String::new();
    use tokio::io::AsyncBufReadExt;
    client_lines.read_line(&mut line).await.unwrap();

    let resp_val: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp_val.get("id").unwrap(), "req-1");
    assert_eq!(resp_val.get("result").unwrap(), &json!({ "pong": true }));

    drop(client_write);
    let _ = runner.await;
    let _ = std::fs::remove_dir_all(&dir);
}
