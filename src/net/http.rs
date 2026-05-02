use core::fmt::Write as FmtWrite;

use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Ipv4Address, Stack};
use embassy_time::Duration;
use embedded_io_async::Write;
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, UnsecureProvider};
use esp_hal::rng::Rng;
use heapless::String as HString;
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

use crate::config::{K3S_API_HOST, K3S_API_PORT_STR, K3S_TOKEN};

#[derive(Debug)]
#[allow(dead_code)]
pub enum ApiError {
    Tcp(embassy_net::tcp::ConnectError),
    Tls(embedded_tls::TlsError),
    Fmt,
    Truncated(usize),
    BadResponse,
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

#[allow(clippy::too_many_arguments)]
pub async fn k8s_request<'a>(
    stack: Stack<'static>,
    api_ip: Ipv4Address,
    api_port: u16,
    rng: Rng,
    tls_read: &mut [u8],
    tls_write: &mut [u8],
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    resp_buf: &'a mut [u8],
) -> Result<Response<'a>, ApiError> {
    // TCP.
    let mut tcp_rx = [0u8; 4096];
    let mut tcp_tx = [0u8; 4096];
    let mut socket = TcpSocket::new(stack, &mut tcp_rx, &mut tcp_tx);
    socket.set_timeout(Some(Duration::from_secs(10)));
    socket.connect((IpAddress::Ipv4(api_ip), api_port)).await?;

    // TLS handshake.
    let cfg = TlsConfig::new().with_server_name(K3S_API_HOST);
    let mut tls = TlsConnection::<_, Aes128GcmSha256>::new(socket, tls_read, tls_write);

    let mut chacha_seed = [0u8; 32];
    for chunk in chacha_seed.chunks_exact_mut(4) {
        chunk.copy_from_slice(&rng.random().to_le_bytes());
    }
    let chacha = ChaCha8Rng::from_seed(chacha_seed);
    tls.open(TlsContext::new(
        &cfg,
        UnsecureProvider::new::<Aes128GcmSha256>(chacha),
    ))
    .await?;

    // Build the request line + headers. Sized for a ~1KB SA-token JWT plus
    // the long-ish lease PATCH path; bumping further is cheap.
    //
    // PATCH defaults to RFC 7396 merge-patch; "PATCH-STRATEGIC" picks
    // application/strategic-merge-patch+json, which is what the kubelet
    // status subresource wants so it knows to merge the conditions array
    // by `type` instead of replacing it wholesale.
    let (http_method, content_type) = match method {
        "PATCH" => ("PATCH", "application/merge-patch+json"),
        "PATCH-STRATEGIC" => ("PATCH", "application/strategic-merge-patch+json"),
        m => (m, "application/json"),
    };
    let mut head: HString<2048> = HString::new();
    let body_len = body.map(|b| b.len()).unwrap_or(0);
    write!(
        &mut head,
        "{method} {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Authorization: Bearer {tok}\r\n\
         User-Agent: picokubelet/0.1\r\n\
         Accept: application/json\r\n\
         Content-Type: {ctype}\r\n\
         Content-Length: {clen}\r\n\
         Connection: close\r\n\
         \r\n",
        method = http_method,
        path = path,
        host = K3S_API_HOST,
        port = K3S_API_PORT_STR,
        tok = K3S_TOKEN,
        ctype = content_type,
        clen = body_len,
    )
    .map_err(|_| ApiError::Fmt)?;

    tls.write_all(head.as_bytes()).await?;
    if let Some(b) = body {
        tls.write_all(b).await?;
    }
    tls.flush().await?;

    // Read until the server closes (we sent Connection: close).
    let mut total = 0;
    loop {
        if total == resp_buf.len() {
            return Err(ApiError::Truncated(total));
        }
        match tls.read(&mut resp_buf[total..]).await {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(embedded_tls::TlsError::ConnectionClosed) => break,
            Err(e) => return Err(ApiError::Tls(e)),
        }
    }
    let _ = tls.close().await;

    parse_response(&resp_buf[..total])
}

fn parse_response(buf: &[u8]) -> Result<Response<'_>, ApiError> {
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
