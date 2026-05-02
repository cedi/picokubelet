#[derive(Debug)]
#[allow(dead_code)]
pub enum ApiError {
    Tcp(embassy_net::tcp::ConnectError),
    Tls(embedded_tls::TlsError),
    Fmt,
    Truncated(usize),
    BadResponse,

    /// HTTP 4xx the caller can recover from (e.g. 404 NotFound, 409
    /// AlreadyExists). Body is not preserved here — callers that want it
    /// should inspect the Response before turning it into an error.
    ClientError(u16),

    /// HTTP 5xx, treat as transient and retry.
    ServerError(u16),

    /// 401 Unauthorized or 403 Forbidden. Distinct because no amount of
    /// retrying will help.
    AuthError(u16),
}

impl From<embassy_net::tcp::ConnectError> for ApiError {
    fn from(e: embassy_net::tcp::ConnectError) -> Self {
        Self::Tcp(e)
    }
}
impl From<embedded_tls::TlsError> for ApiError {
    fn from(e: embedded_tls::TlsError) -> Self {
        Self::Tls(e)
    }
}

pub struct Response<'a> {
    pub status: u16,
    headers: &'a str,
    pub body: &'a [u8],
}

impl<'a> Response<'a> {
    pub fn header(&self, name: &str) -> Option<&'a str> {
        for line in self.headers.lines() {
            if let Some((k, v)) = line.split_once(':')
                && k.eq_ignore_ascii_case(name)
            {
                return Some(v.trim());
            }
        }
        None
    }
}

pub(crate) fn parse_response(buf: &[u8]) -> Result<Response<'_>, ApiError> {
    // Find header/body split.
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(ApiError::BadResponse)?;
    let head = core::str::from_utf8(&buf[..split]).map_err(|_| ApiError::BadResponse)?;
    let body = &buf[split + 4..];

    // Status line: "HTTP/1.1 200 OK"
    let status_line = head.lines().next().ok_or(ApiError::BadResponse)?;
    let mut parts = status_line.split_ascii_whitespace();
    let _http = parts.next();
    let code = parts.next().ok_or(ApiError::BadResponse)?;
    let status: u16 = code.parse().map_err(|_| ApiError::BadResponse)?;

    // Headers = everything after the status line.
    let headers = head.split_once("\r\n").map(|(_, h)| h).unwrap_or("");

    Ok(Response {
        status,
        headers,
        body,
    })
}
