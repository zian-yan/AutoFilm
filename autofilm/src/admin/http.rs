use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

use serde_json::Value;

/// 请求头最大长度，超过视为非法请求。
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// 请求体最大长度，配置 JSON 远小于该值。
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// 整个请求（头部 + 主体）的读取总时限。
///
/// 单次 `read` 的超时挡不住慢速攻击：对端每 9 秒发 1 字节，就能把 16 KB 头部
/// 拖上几十小时并长期占住连接名额。这里给整次读取一个硬预算，到点直接断开。
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);

/// 一个已解析完成的 HTTP 请求。
///
/// 只保留管理面板需要的字段：方法、路径、头、定长请求体与来源地址。
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub remote_addr: String,
}

impl Request {
    /// 读取指定请求头（大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// 把请求体解析为 JSON。
    pub fn json_body(&self) -> Result<Value, String> {
        if self.body.is_empty() {
            return Err("请求体为空".to_string());
        }
        serde_json::from_slice(&self.body).map_err(|err| format!("请求体不是合法 JSON: {err}"))
    }
}

/// 待写出的 HTTP 响应。
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn new(status: u16, content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body: body.into(),
            headers: Vec::new(),
        }
    }

    pub fn json(status: u16, value: &Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self::new(status, "application/json; charset=utf-8", body)
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status, "text/plain; charset=utf-8", body.into())
    }

    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    /// 302 跳转；未登录访问页面时用来转到登录页。
    pub fn redirect(location: impl Into<String>) -> Self {
        Self::new(302, "text/plain; charset=utf-8", Vec::new()).with_header("Location", location)
    }

    /// 追加一个响应头。
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 写出完整响应；管理面板统一使用短连接。
    pub fn write_to(&self, stream: &mut TcpStream) -> io::Result<()> {
        let mut head = String::with_capacity(256);
        head.push_str("HTTP/1.1 ");
        head.push_str(&self.status.to_string());
        head.push(' ');
        head.push_str(status_text(self.status));
        head.push_str("\r\n");

        head.push_str("Content-Type: ");
        head.push_str(&self.content_type);
        head.push_str("\r\nContent-Length: ");
        head.push_str(&self.body.len().to_string());
        head.push_str("\r\nConnection: close\r\nCache-Control: no-store\r\n");
        head.push_str("X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n");

        for (name, value) in &self.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");

        stream.write_all(head.as_bytes())?;
        stream.write_all(&self.body)?;
        stream.flush()
    }
}

/// 读取并解析一个 HTTP 请求。
///
/// 返回 `Ok(None)` 表示对端直接关闭或请求明显不合法，调用方静默断开即可。
pub fn read_request(stream: &mut TcpStream, remote_addr: &str) -> io::Result<Option<Request>> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    let header_end = loop {
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }

        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Ok(None);
    }

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    // 管理面板不使用查询参数，这里直接裁掉，避免路由匹配时误判。
    let path = target
        .split(|character| character == '?' || character == '#')
        .next()
        .unwrap_or("/")
        .to_string();

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "请求体超过上限"));
    }

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Ok(Some(Request {
        method,
        path,
        headers,
        body,
        remote_addr: remote_addr.to_string(),
    }))
}

/// 在缓冲区中查找头部结束位置（`\r\n\r\n` 的起始下标）。
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        423 => "Locked",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}
