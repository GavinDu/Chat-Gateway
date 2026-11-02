use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Enforce the bound while reading, not after allocating an unbounded line.
pub async fn read_worker_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    bytes: &mut zeroize::Zeroizing<Vec<u8>>,
) -> anyhow::Result<Option<zeroize::Zeroizing<String>>> {
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            anyhow::ensure!(bytes.is_empty(), "truncated browser IPC");
            return Ok(None);
        }
        let end = chunk.iter().position(|c| *c == b'\n');
        let size = end.unwrap_or(chunk.len());
        anyhow::ensure!(bytes.len() + size <= 4 * 1024 * 1024, "browser IPC limit");
        bytes.extend_from_slice(&chunk[..size]);
        reader.consume(size + usize::from(end.is_some()));
        if end.is_some() {
            let line = zeroize::Zeroizing::new(String::from_utf8(bytes.to_vec())?);
            bytes.clear();
            return Ok(Some(line));
        }
    }
}
fn address(value: &Value) -> String {
    let Some(uri) = value
        .as_str()
        .filter(|s| s.len() <= 4096)
        .and_then(|s| s.split('#').next())
        .and_then(|s| s.parse::<axum::http::Uri>().ok())
    else {
        return "about:blank".into();
    };
    let Some(authority) = uri.authority() else {
        return "about:blank".into();
    };
    if uri.scheme_str() != Some("https") || authority.as_str().contains('@') {
        return "about:blank".into();
    }
    format!("https://{}{}", authority, uri.path())
}
/// IPC is not a public wire contract: never forward arbitrary helper fields.
pub fn public_event(v: &Value) -> Option<Value> {
    Some(match v["type"].as_str()? {
        "ready" => json!({"type":"ready"}),
        "status" => json!({"type":"status","url":address(&v["url"]),
            "canBack":v["canBack"].as_bool().unwrap_or(false),
            "canForward":v["canForward"].as_bool().unwrap_or(false)}),
        "frame" => {
            let data = v["data"].as_str()?;
            let w = v["width"].as_u64()?;
            let h = v["height"].as_u64()?;
            if data.len() > 4 * 1024 * 1024
                || !(640..=1920).contains(&w)
                || !(480..=1080).contains(&h)
            {
                return None;
            }
            json!({"type":"frame","data":data,"width":w,"height":h,
                "pageId":v["pageId"].as_str().unwrap_or("")})
        }
        "pages" => {
            let pages: Vec<_> = v["pages"]
                .as_array()?
                .iter()
                .take(4)
                .map(|p| {
                    json!({"id":p["id"].as_str().unwrap_or(""),
                    "url":address(&p["url"]),"active":p["active"].as_bool().unwrap_or(false)})
                })
                .collect();
            json!({"type":"pages","pages":pages,"activePage":v["activePage"].as_str().unwrap_or("")})
        }
        "error" => {
            let code = v["code"].as_str().unwrap_or("");
            let safe = match code {
                "navigation_blocked"
                | "invalid_input"
                | "invalid_resize"
                | "no_provider_session"
                | "browser_not_ready"
                | "page_limit"
                | "input_queue_full"
                | "popup_blocked" => code,
                _ => "browser_runtime_unavailable",
            };
            let phase = v["phase"].as_str().filter(|s| {
                matches!(
                    *s,
                    "target_attach"
                        | "page_enable"
                        | "document_guard"
                        | "child_attach"
                        | "target_resume"
                )
            });
            json!({"type":"error","code":safe,"phase":phase})
        }
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ipc_projection_never_forwards_snapshots_extra_fields_or_oauth_query() {
        assert!(public_event(&json!({"type":"snapshot","session":"synthetic"})).is_none());
        let event = public_event(&json!({"type":"status","url":"https://chatgpt.com/callback?code=synthetic#state","cookie":"synthetic"})).unwrap();
        assert_eq!(event["url"], "https://chatgpt.com/callback");
        assert!(event.get("cookie").is_none());
        let event =
            public_event(&json!({"type":"error","code":"unreviewed-error-detail"})).unwrap();
        assert_eq!(event["code"], "browser_runtime_unavailable");
        assert!(
            public_event(&json!({"type":"frame","data":"","width":9000,"height":700})).is_none()
        );
    }
    #[tokio::test]
    async fn ipc_reader_bounds_unterminated_lines_and_detects_truncation() {
        let mut pending = zeroize::Zeroizing::new(Vec::new());
        let bytes = vec![b'x'; 4 * 1024 * 1024 + 1];
        assert!(
            read_worker_line(&mut BufReader::new(bytes.as_slice()), &mut pending)
                .await
                .is_err()
        );
        pending.clear();
        assert!(
            read_worker_line(&mut BufReader::new(&b"truncated"[..]), &mut pending)
                .await
                .is_err()
        );
        let mut reader = BufReader::new(&b"{}\n"[..]);
        pending.clear();
        assert_eq!(
            &**read_worker_line(&mut reader, &mut pending)
                .await
                .unwrap()
                .unwrap(),
            "{}"
        );
        assert!(read_worker_line(&mut reader, &mut pending)
            .await
            .unwrap()
            .is_none());
    }
    #[tokio::test]
    async fn partial_ipc_line_survives_select_cancellation() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, pipe) = tokio::io::duplex(128);
        let mut reader = BufReader::new(pipe);
        let mut pending = zeroize::Zeroizing::new(Vec::new());
        writer.write_all(b"{\"fixture\":").await.unwrap();
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            read_worker_line(&mut reader, &mut pending)
        )
        .await
        .is_err());
        writer.write_all(b"true}\n").await.unwrap();
        let line = read_worker_line(&mut reader, &mut pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&**line, "{\"fixture\":true}");
    }
}
