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
use crate::net::http::{ApiError, Response, parse_response};

/// Owns the TLS buffers and exposes typed HTTP methods for talking to k3s.
pub struct ApiClient<'a> {
    stack: Stack<'static>,
    api_ip: Ipv4Address,
    api_port: u16,
    rng: Rng,
    tls_read: &'a mut [u8],
    tls_write: &'a mut [u8],
    resp_buf: &'a mut [u8],
}

impl<'a> ApiClient<'a> {
    pub fn new(
        stack: Stack<'static>,
        api_ip: Ipv4Address,
        api_port: u16,
        rng: Rng,
        tls_read: &'a mut [u8],
        tls_write: &'a mut [u8],
        resp_buf: &'a mut [u8],
    ) -> Self {
        Self {
            stack,
            api_ip,
            api_port,
            rng,
            tls_read,
            tls_write,
            resp_buf,
        }
    }

    pub async fn get(&mut self, path: &str) -> Result<Response<'_>, ApiError> {
        self.request("GET", "application/json", path, None).await
    }

    pub async fn post(&mut self, path: &str, body: &[u8]) -> Result<Response<'_>, ApiError> {
        self.request("POST", "application/json", path, Some(body))
            .await
    }

    pub async fn patch_merge(&mut self, path: &str, body: &[u8]) -> Result<Response<'_>, ApiError> {
        self.request("PATCH", "application/merge-patch+json", path, Some(body))
            .await
    }

    pub async fn patch_strategic(
        &mut self,
        path: &str,
        body: &[u8],
    ) -> Result<Response<'_>, ApiError> {
        self.request(
            "PATCH",
            "application/strategic-merge-patch+json",
            path,
            Some(body),
        )
        .await
    }

    async fn request(
        &mut self,
        http_method: &str,
        content_type: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Response<'_>, ApiError> {
        // TCP.
        let mut tcp_rx = [0u8; 4096];
        let mut tcp_tx = [0u8; 4096];
        let mut socket = TcpSocket::new(self.stack, &mut tcp_rx, &mut tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        socket
            .connect((IpAddress::Ipv4(self.api_ip), self.api_port))
            .await?;

        // TLS handshake.
        let cfg = TlsConfig::new().with_server_name(K3S_API_HOST);
        let mut tls = TlsConnection::<_, Aes128GcmSha256>::new(
            socket,
            &mut self.tls_read[..],
            &mut self.tls_write[..],
        );

        let mut chacha_seed = [0u8; 32];
        for chunk in chacha_seed.chunks_exact_mut(4) {
            chunk.copy_from_slice(&self.rng.random().to_le_bytes());
        }
        let chacha = ChaCha8Rng::from_seed(chacha_seed);
        tls.open(TlsContext::new(
            &cfg,
            UnsecureProvider::new::<Aes128GcmSha256>(chacha),
        ))
        .await?;

        // Build the request line + headers. Sized for a ~1KB SA-token JWT plus
        // the long-ish lease PATCH path; bumping further is cheap.
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
            if total == self.resp_buf.len() {
                return Err(ApiError::Truncated(total));
            }
            match tls.read(&mut self.resp_buf[total..]).await {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(embedded_tls::TlsError::ConnectionClosed) => break,
                Err(e) => return Err(ApiError::Tls(e)),
            }
        }
        let _ = tls.close().await;

        parse_response(&self.resp_buf[..total])
    }
}
