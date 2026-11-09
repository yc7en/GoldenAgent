use serde_json::Value;

/// 编码 LSP JSON-RPC 消息帧（Content-Length: <len>\r\n\r\n<json>）。
pub fn encode_message(payload: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut msg = Vec::with_capacity(header.len() + body.len());
    msg.extend_from_slice(header.as_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// 从流式字节缓冲区中解析出完整的 LSP JSON-RPC 消息。
///
/// 成功解析的消息从 `buffer` 中消费（drain）。若缓冲区数据不完整则保留等待后续字节。
pub fn decode_messages(buffer: &mut Vec<u8>) -> Result<Vec<Value>, String> {
    let mut messages = Vec::new();

    loop {
        // 查找 HTTP 风格的头部与消息体分隔符 \r\n\r\n
        let sep = b"\r\n\r\n";
        let Some(sep_idx) = buffer.windows(4).position(|w| w == sep) else {
            break;
        };

        let header_bytes = &buffer[..sep_idx];
        let header_str = match std::str::from_utf8(header_bytes) {
            Ok(s) => s,
            Err(e) => return Err(format!("LSP 头部包含非 UTF-8 字符: {e}")),
        };

        // 提取 Content-Length
        let mut content_length: Option<usize> = None;
        for line in header_str.lines() {
            let line = line.trim();
            if line.to_ascii_lowercase().starts_with("content-length:") {
                let val_part = line["content-length:".len()..].trim();
                if let Ok(len) = val_part.parse::<usize>() {
                    content_length = Some(len);
                    break;
                }
            }
        }

        let Some(content_len) = content_length else {
            return Err("LSP 头部缺少合法的 Content-Length 字段".to_string());
        };

        let body_start = sep_idx + 4;
        let body_end = body_start + content_len;

        if buffer.len() < body_end {
            // 数据体尚未接收完全，等待更多数据
            break;
        }

        let body_bytes = &buffer[body_start..body_end];
        let value: Value = serde_json::from_slice(body_bytes)
            .map_err(|e| format!("LSP 消息体 JSON 反序列化失败: {e}"))?;
        messages.push(value);

        // 消费已解析的数据段
        buffer.drain(..body_end);
    }

    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_encode_and_decode_roundtrip() {
        let msg1 = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"processId": 1234}
        });
        let msg2 = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///test.rs", "diagnostics": []}
        });

        let mut buf = Vec::new();
        buf.extend_from_slice(&encode_message(&msg1));
        buf.extend_from_slice(&encode_message(&msg2));

        let decoded = decode_messages(&mut buf).expect("解析应该成功");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], msg1);
        assert_eq!(decoded[1], msg2);
        assert!(buf.is_empty(), "解析完成后缓冲区应该为空");
    }

    #[test]
    fn test_partial_buffer_waits_for_complete_payload() {
        let msg = json!({"jsonrpc": "2.0", "result": "ok"});
        let full = encode_message(&msg);

        // 只喂前一半数据
        let mut buf = full[..full.len() - 5].to_vec();
        let decoded = decode_messages(&mut buf).expect("不应报错");
        assert!(decoded.is_empty());
        assert!(!buf.is_empty());

        // 补全后一半数据
        buf.extend_from_slice(&full[full.len() - 5..]);
        let decoded = decode_messages(&mut buf).expect("不应报错");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], msg);
        assert!(buf.is_empty());
    }
}
