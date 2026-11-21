//! WebSocket (RFC 6455), as much of it as the relay needs (decision D94): an HTTP/1.1 request
//! head, the upgrade on both sides, and frames. The relay sits behind Cloudflare or a reverse
//! proxy, which speak HTTP and WebSocket and nothing else, so a relayed link is a WebSocket of
//! binary frames; what the frames carry is the same pinned TLS as a direct link, which neither
//! the proxy nor the relay can read.
//!
//! `Entry` is where a machine finds the relay: `host:port` and `https://…` are dialled as
//! `wss://` and checked against the usual web roots (Cloudflare's certificate, or the reverse
//! proxy's); `http://host:port` as `ws://`, for a relay on the same machine or a test.

use std::io;
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::pairing::Io;

/// The longest request or response head taken.
pub const MAX_HEAD: usize = 16 * 1024;
/// The longest message taken in one piece. A link's frames are much smaller; a larger one is
/// not NyPi's.
pub const MAX_MESSAGE: usize = 1 << 20;
/// How long a dial may take, connection and upgrade together.
const DIAL_WAIT: Duration = Duration::from_secs(10);
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

/// An HTTP/1.1 request or response head.
#[derive(Clone, Debug, Default)]
pub struct Head {
    /// `GET`, `POST`, …; empty for a response.
    pub method: String,
    /// The path with its query; empty for a response.
    pub path: String,
    /// A response's status; 0 for a request.
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// The first value of header `name`, whatever its case.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.trim())
    }

    /// A request to switch to WebSocket.
    pub fn is_upgrade(&self) -> bool {
        self.get("upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            && self.get("sec-websocket-key").is_some()
    }

    /// The request again, as bytes, with `drop` headers left out and `add` put in.
    pub fn request_bytes(&self, drop: &[&str], add: &[(&str, &str)]) -> Vec<u8> {
        let mut out = format!("{} {} HTTP/1.1\r\n", self.method, self.path);
        for (key, value) in &self.headers {
            if !drop.iter().any(|name| key.eq_ignore_ascii_case(name)) {
                out.push_str(&format!("{key}: {value}\r\n"));
            }
        }
        for (key, value) in add {
            out.push_str(&format!("{key}: {value}\r\n"));
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// Read a head: a request when `request`, else a response. None when the stream ended first.
pub async fn read_head<R: AsyncBufRead + Unpin>(reader: &mut R, request: bool) -> io::Result<Option<Head>> {
    let mut bytes = Vec::new();
    loop {
        let read = (&mut *reader).take((MAX_HEAD - bytes.len()) as u64).read_until(b'\n', &mut bytes).await?;
        if read == 0 {
            return if bytes.is_empty() { Ok(None) } else { Err(invalid("the head ended early")) };
        }
        if bytes.ends_with(b"\r\n\r\n") || bytes == b"\r\n" {
            break;
        }
        if bytes.len() >= MAX_HEAD {
            return Err(invalid("a head longer than the relay takes"));
        }
    }
    let mut fields = [httparse::EMPTY_HEADER; 64];
    let mut head = Head::default();
    let fields = if request {
        let mut parsed = httparse::Request::new(&mut fields);
        parsed.parse(&bytes).map_err(|error| invalid(error.to_string()))?;
        head.method = parsed.method.unwrap_or_default().to_string();
        head.path = parsed.path.unwrap_or_default().to_string();
        parsed.headers.to_vec()
    } else {
        let mut parsed = httparse::Response::new(&mut fields);
        parsed.parse(&bytes).map_err(|error| invalid(error.to_string()))?;
        head.status = parsed.code.unwrap_or_default();
        parsed.headers.to_vec()
    };
    head.headers = fields
        .iter()
        .map(|field| (field.name.to_string(), String::from_utf8_lossy(field.value).into_owned()))
        .collect();
    Ok(Some(head))
}

/// What the server answers a key with.
pub fn accept_key(key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, format!("{key}{GUID}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest.as_ref())
}

/// The server's answer to an upgrade request.
pub fn upgrade_response(head: &Head) -> Vec<u8> {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(head.get("sec-websocket-key").unwrap_or_default())
    )
    .into_bytes()
}

/// A plain answer that ends the connection.
pub fn plain_response(status: u16, reason: &str, text: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
    .into_bytes()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

/// The reading side of a WebSocket.
pub struct Reader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(inner: R) -> Self {
        Reader { inner }
    }

    /// The next whole message; None when the connection ended without a close.
    pub async fn next(&mut self) -> io::Result<Option<Message>> {
        let mut gathered: Option<(u8, Vec<u8>)> = None;
        loop {
            let mut start = [0u8; 2];
            match self.inner.read_exact(&mut start).await {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && gathered.is_none() => return Ok(None),
                Err(error) => return Err(error),
            }
            let (fin, opcode, masked) = (start[0] & 0x80 != 0, start[0] & 0x0f, start[1] & 0x80 != 0);
            let length = match start[1] & 0x7f {
                126 => self.inner.read_u16().await? as u64,
                127 => self.inner.read_u64().await?,
                short => short as u64,
            };
            let so_far = gathered.as_ref().map_or(0, |(_, bytes)| bytes.len() as u64);
            if length + so_far > MAX_MESSAGE as u64 {
                return Err(invalid("a message larger than the link takes"));
            }
            let mut key = [0u8; 4];
            if masked {
                self.inner.read_exact(&mut key).await?;
            }
            let mut payload = vec![0u8; length as usize];
            self.inner.read_exact(&mut payload).await?;
            if masked {
                payload.iter_mut().enumerate().for_each(|(i, byte)| *byte ^= key[i % 4]);
            }
            match opcode {
                0x8 => return Ok(Some(Message::Close)),
                0x9 => return Ok(Some(Message::Ping(payload))),
                0xA => return Ok(Some(Message::Pong(payload))),
                0x0 => match gathered.as_mut() {
                    Some((_, bytes)) => bytes.extend_from_slice(&payload),
                    None => return Err(invalid("a continuation with nothing to continue")),
                },
                0x1 | 0x2 if gathered.is_none() => gathered = Some((opcode, payload)),
                _ => return Err(invalid("a frame this side does not take")),
            }
            if fin {
                let (opcode, bytes) = gathered.take().unwrap_or_default();
                return Ok(Some(match opcode {
                    0x1 => Message::Text(String::from_utf8(bytes).map_err(|_| invalid("text that is not UTF-8"))?),
                    _ => Message::Binary(bytes),
                }));
            }
        }
    }
}

/// The writing side. A client masks what it sends, as RFC 6455 asks; a server does not.
pub struct Writer<W> {
    inner: W,
    mask: bool,
}

impl<W: AsyncWrite + Unpin> Writer<W> {
    pub fn new(inner: W, mask: bool) -> Self {
        Writer { inner, mask }
    }

    pub async fn send(&mut self, message: &Message) -> io::Result<()> {
        let (opcode, payload): (u8, &[u8]) = match message {
            Message::Text(text) => (0x1, text.as_bytes()),
            Message::Binary(bytes) => (0x2, bytes),
            Message::Ping(bytes) => (0x9, bytes),
            Message::Pong(bytes) => (0xA, bytes),
            Message::Close => (0x8, &[]),
        };
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let mask_bit = if self.mask { 0x80 } else { 0 };
        match payload.len() {
            n if n < 126 => frame.push(mask_bit | n as u8),
            n if n <= u16::MAX as usize => {
                frame.push(mask_bit | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(mask_bit | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        if self.mask {
            let mut key = [0u8; 4];
            ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key)
                .map_err(|_| io::Error::other("no random bytes"))?;
            frame.extend_from_slice(&key);
            frame.extend(payload.iter().enumerate().map(|(i, byte)| byte ^ key[i % 4]));
        } else {
            frame.extend_from_slice(payload);
        }
        self.inner.write_all(&frame).await?;
        self.inner.flush().await
    }

    pub async fn shutdown(&mut self) {
        let _ = self.inner.shutdown().await;
    }
}

pub type SocketReader = Reader<BufReader<tokio::io::ReadHalf<Box<dyn Io>>>>;
pub type SocketWriter = Writer<tokio::io::WriteHalf<Box<dyn Io>>>;

/// How large a piece of a byte stream goes in one frame.
const PIECE: usize = 16 * 1024;

/// A WebSocket as a byte stream (decision D94): binary frames in, bytes out, and the other way.
/// What runs over it (a TLS session) sees an ordinary connection. Pings are answered; a close,
/// or the end of either side, ends the stream.
pub fn into_stream<R, W>(mut reader: Reader<R>, mut writer: Writer<W>) -> tokio::io::DuplexStream
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (ours, theirs) = tokio::io::duplex(4 * PIECE);
    let (mut from_us, mut to_us) = tokio::io::split(ours);
    let (pong, mut pongs) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Ok(Some(message)) = reader.next().await {
            match message {
                Message::Binary(bytes) => {
                    if to_us.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Message::Ping(bytes) => {
                    let _ = pong.send(bytes);
                }
                Message::Close => break,
                Message::Text(_) | Message::Pong(_) => {}
            }
        }
        let _ = to_us.shutdown().await;
    });
    tokio::spawn(async move {
        let mut buffer = vec![0u8; PIECE];
        loop {
            tokio::select! {
                read = from_us.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => if writer.send(&Message::Binary(buffer[..n].to_vec())).await.is_err() { return },
                },
                Some(bytes) = pongs.recv() => if writer.send(&Message::Pong(bytes)).await.is_err() { return },
            }
        }
        let _ = writer.send(&Message::Close).await;
        writer.shutdown().await;
    });
    theirs
}

/// Where the relay is reached (decision D94).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// `wss://`: TLS to Cloudflare or the reverse proxy, checked against the web's roots.
    pub secure: bool,
    pub host: String,
    pub port: u16,
}

impl Entry {
    /// `host:port`, `host`, `https://host[:port]`, or `http://host:port`. A path is ignored.
    pub fn parse(text: &str) -> Option<Entry> {
        let text = text.trim();
        let (secure, rest) = match text.split_once("://") {
            Some(("http", rest)) => (false, rest),
            Some(("https", rest)) => (true, rest),
            Some(_) => return None,
            None => (true, text),
        };
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty() {
            return None;
        }
        let default = if secure { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, _)) if !host.ends_with(']') && host.contains(':') => (authority, default),
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, default),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        Some(Entry { secure, host, port })
    }

    /// As the user reads it.
    pub fn url(&self) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{}", self.authority())
    }

    fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default = if self.secure { 443 } else { 80 };
        if self.port == default { host } else { format!("{host}:{}", self.port) }
    }
}

fn web_roots() -> io::Result<tokio_rustls::TlsConnector> {
    let roots = tokio_rustls::rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsConnector::from(std::sync::Arc::new(config)))
}

/// Open a WebSocket to `path` at the entry, with `headers` added to the request.
pub async fn dial(entry: &Entry, path: &str, headers: &[(&str, &str)]) -> io::Result<(SocketReader, SocketWriter)> {
    tokio::time::timeout(DIAL_WAIT, dial_now(entry, path, headers))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{} did not answer", entry.url())))?
}

async fn dial_now(entry: &Entry, path: &str, headers: &[(&str, &str)]) -> io::Result<(SocketReader, SocketWriter)> {
    let tcp = tokio::net::TcpStream::connect((entry.host.as_str(), entry.port)).await?;
    tcp.set_nodelay(true).ok();
    let stream: Box<dyn Io> = if entry.secure {
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(entry.host.clone())
            .map_err(|_| invalid(format!("{} is not a name a certificate can carry", entry.host)))?;
        Box::new(web_roots()?.connect(name, tcp).await?)
    } else {
        Box::new(tcp)
    };
    let (read, mut write) = tokio::io::split(stream);
    let mut key = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key)
        .map_err(|_| io::Error::other("no random bytes"))?;
    let key = base64::engine::general_purpose::STANDARD.encode(key);
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: nypi/{}\r\n",
        entry.authority(),
        env!("CARGO_PKG_VERSION")
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    write.write_all(request.as_bytes()).await?;
    let mut read = BufReader::new(read);
    let head = read_head(&mut read, false).await?.ok_or_else(|| invalid("the entry closed without an answer"))?;
    if head.status != 101 {
        let mut body = vec![0u8; 512];
        let n = tokio::time::timeout(Duration::from_secs(2), read.read(&mut body)).await.ok().and_then(Result::ok);
        let said = String::from_utf8_lossy(&body[..n.unwrap_or(0)]).trim().to_string();
        let said = if said.is_empty() { String::new() } else { format!(": {said}") };
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("the entry answered {}{said}", head.status),
        ));
    }
    if head.get("sec-websocket-accept") != Some(accept_key(&key).as_str()) {
        return Err(invalid("the entry's answer was not a WebSocket's"));
    }
    Ok((Reader::new(read), Writer::new(write, true)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_read_as_the_user_writes_them() {
        let entry = |text: &str| Entry::parse(text).map(|e| (e.secure, e.host, e.port));
        assert_eq!(entry("nypi.example.com:443"), Some((true, "nypi.example.com".into(), 443)));
        assert_eq!(entry("nypi.example.com"), Some((true, "nypi.example.com".into(), 443)));
        assert_eq!(entry("https://hub.example.org/x"), Some((true, "hub.example.org".into(), 443)));
        assert_eq!(entry("http://127.0.0.1:7682"), Some((false, "127.0.0.1".into(), 7682)));
        assert_eq!(entry("http://[::1]:9"), Some((false, "::1".into(), 9)));
        assert_eq!(entry("ftp://x"), None);
        assert_eq!(Entry::parse("nypi.example.com:443").unwrap().url(), "wss://nypi.example.com");
        assert_eq!(Entry::parse("http://127.0.0.1:7682").unwrap().url(), "ws://127.0.0.1:7682");
    }

    /// The accept value of RFC 6455's own example.
    #[test]
    fn the_accept_key_is_the_rfcs() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    /// Frames both ways, masked from the client, large and small, and a byte stream over them.
    #[tokio::test]
    async fn messages_and_streams_cross_a_websocket() {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let mut to_server = Writer::new(client_write, true);
        let mut at_server = Reader::new(server_read);
        let big = vec![7u8; 70_000];
        to_server.send(&Message::Text("hello".into())).await.unwrap();
        to_server.send(&Message::Binary(big.clone())).await.unwrap();
        to_server.send(&Message::Ping(b"p".to_vec())).await.unwrap();
        assert_eq!(at_server.next().await.unwrap(), Some(Message::Text("hello".into())));
        assert_eq!(at_server.next().await.unwrap(), Some(Message::Binary(big)));
        assert_eq!(at_server.next().await.unwrap(), Some(Message::Ping(b"p".to_vec())));

        let mut client_stream = into_stream(Reader::new(client_read), to_server);
        let mut server_stream = into_stream(at_server, Writer::new(server_write, false));
        client_stream.write_all(b"over the frames").await.unwrap();
        let mut got = [0u8; 15];
        server_stream.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"over the frames");
        drop(client_stream);
        let mut rest = Vec::new();
        server_stream.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty(), "the close ends the other side");
    }
}
