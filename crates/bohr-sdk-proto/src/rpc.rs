//! JSON-RPC over HTTP/1.1, as the guests that question a cluster speak it: the probe and the load.
//!
//! One client for both, so a reply is read one way: whole or not yet, by its id in a batch, its
//! fields by one path language.

use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    time::Duration,
};

use crate::json::{self, Json};

/// Largest reply read; a larger one is refused as too large.
pub const MOST_BODY: usize = 1 << 20;

/// The address the world gives node `index`.
#[must_use]
pub fn address(index: u32, port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::from(0x0A42_0000 | (index + 1)), port)
}

/// One HTTP request carrying `body` to `to`.
#[must_use]
pub fn request_bytes(to: SocketAddrV4, body: &str) -> Vec<u8> {
    let head = format!(
        "POST / HTTP/1.1\r\nHost: {to}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// One call as a JSON-RPC 2.0 request with `id`.
#[must_use]
pub fn call_body(id: i64, method: &str, params: &Json) -> String {
    let mut call = BTreeMap::new();
    call.insert("jsonrpc".to_owned(), Json::Str("2.0".to_owned()));
    call.insert("id".to_owned(), Json::Int(id));
    call.insert("method".to_owned(), Json::Str(method.to_owned()));
    call.insert("params".to_owned(), params.clone());
    Json::Object(call).to_json()
}

/// One HTTP reply from what has arrived, with whether the connection may be used again; `None`
/// while some of it has still to come.
///
/// # Errors
///
/// A status other than 200, a body neither sized nor chunked, or a body that is not text.
pub fn parse_reply(buf: &[u8]) -> Result<Option<(String, bool)>, String> {
    let Some(header_end) = find(buf, b"\r\n\r\n").map(|at| at + 4) else { return Ok(None) };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let status = head.split_whitespace().nth(1).unwrap_or("").to_owned();
    let keep = !head.contains("\r\nconnection: close");
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|n| n.trim().parse::<usize>().ok());
    let chunked =
        head.lines().any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
    let raw = &buf[header_end..];
    let body = if chunked {
        match dechunk(raw)? {
            Some(body) => body,
            None => return Ok(None),
        }
    } else {
        let Some(length) = length else {
            return Err("reply has neither a length nor chunks".to_owned());
        };
        if raw.len() < length {
            return Ok(None);
        }
        raw[..length].to_vec()
    };
    if status != "200" {
        return Err(format!("http {status}"));
    }
    let text = String::from_utf8(body).map_err(|_| "reply is not text".to_owned())?;
    Ok(Some((text, keep)))
}

/// A chunked body, where every chunk to the last has arrived.
fn dechunk(mut raw: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut body = Vec::new();
    loop {
        let Some(line_end) = find(raw, b"\r\n") else { return Ok(None) };
        let size_text = String::from_utf8_lossy(&raw[..line_end]).to_string();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "a chunk size does not read".to_owned())?;
        if raw.len() < line_end + 2 + size + 2 {
            return Ok(None);
        }
        body.extend_from_slice(&raw[line_end + 2..line_end + 2 + size]);
        raw = &raw[line_end + 2 + size + 2..];
        if size == 0 {
            return Ok(Some(body));
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// One call's result in a reply, found by its id where the reply is a batch.
///
/// # Errors
///
/// The call is missing from a batch, or the server answered it with an error.
pub fn answer_for(reply: &Json, id: i64) -> Result<Json, String> {
    let item = match reply {
        Json::Array(items) => items
            .iter()
            .find(|item| item.get("id").and_then(Json::as_int) == Some(id))
            .ok_or_else(|| "missing from the batch".to_owned())?,
        single => single,
    };
    if let Some(error) = item.get("error") {
        let message = error.get("message").and_then(Json::as_str).unwrap_or("error");
        return Err(format!("error: {message}"));
    }
    Ok(item.get("result").cloned().unwrap_or(Json::Null))
}

/// The value at a dotted path, where `name[n]` takes the nth element of a list.
#[must_use]
pub fn at<'a>(item: &'a Json, path: &str) -> Option<&'a Json> {
    if path.is_empty() || path == "." {
        return Some(item);
    }
    path.split('.').try_fold(item, |here, part| match part.split_once('[') {
        Some((name, index)) => {
            let list = if name.is_empty() { here } else { here.get(name)? };
            let n: usize = index.strip_suffix(']')?.parse().ok()?;
            match list {
                Json::Array(items) => items.get(n),
                _ => None,
            }
        }
        None => here.get(part),
    })
}

/// Makes one call and waits for its answer, at most `wait`.
///
/// For a guest that asks one thing at a time; the connection is opened for the call and closed
/// after it, so a late reply never reads as the next call's.
///
/// # Errors
///
/// The connection, the send or the reply failed, or the server answered with an error.
pub fn call(to: SocketAddrV4, method: &str, params: &Json, wait: Duration) -> Result<Json, String> {
    let mut stream = TcpStream::connect(to).map_err(|e| format!("connect: {}", e.kind()))?;
    stream.set_read_timeout(Some(wait)).map_err(|e| format!("timeout: {}", e.kind()))?;
    stream
        .write_all(&request_bytes(to, &call_body(1, method, params)))
        .map_err(|e| format!("send: {}", e.kind()))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Some((text, _)) = parse_reply(&buf)? {
            let reply = json::parse(&text).map_err(|e| format!("unreadable: {e}"))?;
            return answer_for(&reply, 1);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err("closed".to_owned()),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MOST_BODY {
                    return Err("reply too large".to_owned());
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("timeout".to_owned());
            }
            Err(e) => return Err(format!("receive: {}", e.kind())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_taken_only_once_whole() {
        let whole = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nnull";
        assert_eq!(parse_reply(&whole[..20]), Ok(None));
        assert_eq!(parse_reply(&whole[..whole.len() - 1]), Ok(None));
        assert_eq!(parse_reply(whole), Ok(Some(("null".to_owned(), true))));
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nnu\r\n2\r\nll\r\n0\r\n\r\n";
        assert_eq!(parse_reply(&chunked[..chunked.len() - 3]), Ok(None));
        assert_eq!(parse_reply(chunked), Ok(Some(("null".to_owned(), true))));
    }

    #[test]
    fn a_batch_reply_is_read_by_id_in_any_order() {
        let reply =
            json::parse(r#"[{"id":1,"result":[]},{"id":0,"error":{"code":-1,"message":"no"}}]"#)
                .expect("json");
        assert_eq!(answer_for(&reply, 1), Ok(Json::Array(Vec::new())));
        assert_eq!(answer_for(&reply, 0), Err("error: no".to_owned()));
        assert!(answer_for(&reply, 7).is_err());
    }

    #[test]
    fn a_path_reads_fields_and_list_elements() {
        let value = json::parse(
            r#"{"result":{"block":{"data":{"txs":["a","b"]},"header":{"height":"7"}}}}"#,
        )
        .expect("json");
        assert_eq!(at(&value, "result.block.header.height"), Some(&Json::Str("7".to_owned())));
        assert_eq!(at(&value, "result.block.data.txs[1]"), Some(&Json::Str("b".to_owned())));
        assert_eq!(at(&value, "result.block.data.txs[2]"), None);
        assert_eq!(at(&value, "."), Some(&value));
        assert_eq!(address(1, 26657).to_string(), "10.66.0.2:26657");
    }
}
