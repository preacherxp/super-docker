//! Blocking HTTP/1.1 client for the Docker daemon socket.
//!
//! Replaces bollard/hyper: the daemon speaks plain HTTP/1.1 over a unix
//! socket (or tcp with `DOCKER_HOST=tcp://…`), one request per connection.
//! Handles Content-Length, chunked transfer-encoding and read-to-EOF bodies;
//! long-lived streams (events / stats / logs) are aborted from another
//! thread by shutting down a clone of the socket.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Transport {
    Unix(PathBuf),
    /// host:port authority.
    Tcp(String),
}

pub enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Unix(s) => Stream::Unix(s.try_clone()?),
            Stream::Tcp(s) => Stream::Tcp(s.try_clone()?),
        })
    }

    fn shutdown(&self) {
        let _ = match self {
            Stream::Unix(s) => s.shutdown(Shutdown::Both),
            Stream::Tcp(s) => s.shutdown(Shutdown::Both),
        };
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.read(buf),
            Stream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.write(buf),
            Stream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.flush(),
            Stream::Tcp(s) => s.flush(),
        }
    }
}

/// Cross-thread cancellation for a streaming response: shutting the socket
/// down unblocks any pending read, ending the reader's loop.
pub struct Aborter {
    stream: Stream,
}

impl Aborter {
    pub fn abort(&self) {
        self.stream.shutdown();
    }
}

enum BodyKind {
    /// Remaining bytes of a Content-Length body.
    Length(u64),
    /// Chunked transfer-encoding; `remaining` bytes left in current chunk.
    Chunked { remaining: u64, done: bool },
    /// No framing header — read until the daemon closes the connection.
    Eof,
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    reader: BufReader<Stream>,
    body: BodyKind,
    /// Bytes already de-framed from the HTTP body but not yet returned by
    /// `read_line`. Keeping them here lets NDJSON streams read in blocks
    /// instead of dispatching through `Read` once per byte.
    line_buf: Vec<u8>,
    line_start: usize,
}

impl Response {
    /// Clone of the underlying socket, for aborting a stream mid-read.
    pub fn aborter(&self) -> io::Result<Aborter> {
        Ok(Aborter {
            stream: self.reader.get_ref().try_clone()?,
        })
    }

    pub fn read_all(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Next non-empty line of the (dechunked) body — for NDJSON streams.
    /// `Ok(None)` means the stream ended.
    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(relative_end) = self.line_buf[self.line_start..]
                .iter()
                .position(|&b| b == b'\n')
            {
                let end = self.line_start + relative_end;
                let text = String::from_utf8_lossy(&self.line_buf[self.line_start..end]);
                let line = text.trim().to_owned();
                self.line_start = end + 1;
                if self.line_start == self.line_buf.len() {
                    self.line_buf.clear();
                    self.line_start = 0;
                }
                if !line.is_empty() {
                    return Ok(Some(line));
                }
                continue;
            }

            // Reclaim bytes belonging to lines already returned. This only
            // moves data when a partial line spans multiple body reads.
            if self.line_start > 0 {
                self.line_buf.drain(..self.line_start);
                self.line_start = 0;
            }

            let mut chunk = [0u8; 8192];
            let n = self.read(&mut chunk)?;
            if n > 0 {
                self.line_buf.extend_from_slice(&chunk[..n]);
                continue;
            }

            let text = String::from_utf8_lossy(&self.line_buf[self.line_start..]);
            let line = text.trim().to_owned();
            self.line_buf.clear();
            self.line_start = 0;
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
    }

    fn next_chunk(&mut self) -> io::Result<()> {
        // previous chunk's trailing CRLF, then "<hex-size>\r\n"
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            line.clear();
            self.reader.read_line(&mut line)?;
        }
        let size_str = line.trim().split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_str, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // consume trailer up to final blank line
            loop {
                line.clear();
                if self.reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    break;
                }
            }
            self.body = BodyKind::Chunked {
                remaining: 0,
                done: true,
            };
        } else {
            self.body = BodyKind::Chunked {
                remaining: size,
                done: false,
            };
        }
        Ok(())
    }
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.body {
            BodyKind::Eof => self.reader.read(buf),
            BodyKind::Length(0) => Ok(0),
            BodyKind::Length(remaining) => {
                let want = buf.len().min(remaining as usize);
                let n = self.reader.read(&mut buf[..want])?;
                self.body = BodyKind::Length(remaining - n as u64);
                Ok(n)
            }
            BodyKind::Chunked { done: true, .. } => Ok(0),
            BodyKind::Chunked { remaining: 0, .. } => {
                self.next_chunk()?;
                self.read(buf)
            }
            BodyKind::Chunked { remaining, .. } => {
                let want = buf.len().min(remaining as usize);
                let n = self.reader.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "chunk cut short",
                    ));
                }
                self.body = BodyKind::Chunked {
                    remaining: remaining - n as u64,
                    done: false,
                };
                Ok(n)
            }
        }
    }
}

fn connect(t: &Transport) -> io::Result<Stream> {
    match t {
        Transport::Unix(p) => Ok(Stream::Unix(UnixStream::connect(p)?)),
        Transport::Tcp(authority) => Ok(Stream::Tcp(TcpStream::connect(authority.as_str())?)),
    }
}

/// One HTTP request on a fresh connection. `path` includes the query string.
/// Bodyless requests only — every Docker call this app makes sends no body.
pub fn request(t: &Transport, method: &str, path: &str) -> io::Result<Response> {
    request_stream(connect(t)?, method, path)
}

fn request_stream(mut stream: Stream, method: &str, path: &str) -> io::Result<Response> {
    // Host is required by HTTP/1.1; the daemon ignores its value on unix
    // sockets. Connection: close keeps EOF-framed bodies finite; streaming
    // (chunked) endpoints are unaffected — they end when we drop the socket.
    let content_length = if method == "GET" {
        ""
    } else {
        "Content-Length: 0\r\n"
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: docker\r\nAccept: application/json\r\nConnection: close\r\n{content_length}\r\n"
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad status line: {line:?}"),
            )
        })?;

    let mut content_type = String::new();
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "headers cut short",
            ));
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim();
            match k.to_ascii_lowercase().as_str() {
                "content-type" => content_type = v.to_string(),
                "content-length" => content_length = v.parse().ok(),
                "transfer-encoding" if v.eq_ignore_ascii_case("chunked") => chunked = true,
                _ => {}
            }
        }
    }

    let body = if status == 204 || status == 304 {
        BodyKind::Length(0)
    } else if chunked {
        BodyKind::Chunked {
            remaining: 0,
            done: false,
        }
    } else if let Some(n) = content_length {
        BodyKind::Length(n)
    } else {
        BodyKind::Eof
    };

    Ok(Response {
        status,
        content_type,
        reader,
        body,
        // Most responses are read wholesale and never need line buffering;
        // allocate lazily on the first streaming `read_line` call.
        line_buf: Vec::new(),
        line_start: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answer one request through a connected socket pair. This exercises the
    /// complete wire protocol without binding a port or filesystem socket,
    /// which also works in network-isolated CI sandboxes.
    fn exchange(reply: &'static str, method: &str) -> Response {
        try_exchange(reply, method).unwrap()
    }

    fn try_exchange(reply: &'static str, method: &str) -> io::Result<Response> {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let _ = server.read(&mut buf);
            let _ = server.write_all(reply.as_bytes());
        });
        request_stream(Stream::Unix(client), method, "/x")
    }

    #[test]
    fn content_length_body() {
        let mut r = exchange(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello",
            "GET",
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        assert_eq!(r.read_all().unwrap(), b"hello");
    }

    #[test]
    fn chunked_body() {
        let mut r = exchange(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            "GET",
        );
        assert_eq!(r.read_all().unwrap(), b"hello world");
    }

    #[test]
    fn chunked_ndjson_lines() {
        let mut r = exchange(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n8\r\n{\"a\":1}\n\r\n8\r\n{\"b\":2}\n\r\n0\r\n\r\n",
            "GET",
        );
        assert_eq!(r.read_line().unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(r.read_line().unwrap().as_deref(), Some("{\"b\":2}"));
        assert_eq!(r.read_line().unwrap(), None);
    }

    #[test]
    fn no_content_status_has_empty_body() {
        let mut r = exchange("HTTP/1.1 204 No Content\r\n\r\n", "POST");
        assert_eq!(r.status, 204);
        assert_eq!(r.read_all().unwrap(), b"");
    }

    #[test]
    fn error_status_body_readable() {
        let mut r = exchange(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 25\r\n\r\n{\"message\":\"no such ctr\"}",
            "GET",
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.read_all().unwrap(), br#"{"message":"no such ctr"}"#);
    }

    #[test]
    fn eof_framed_body_and_partial_final_line() {
        let mut r = exchange(
            "HTTP/1.1 200 OK\r\nX-Ignored: yes\r\n\r\n\n first \nlast",
            "GET",
        );
        assert_eq!(r.read_line().unwrap().as_deref(), Some("first"));
        assert_eq!(r.read_line().unwrap().as_deref(), Some("last"));
        assert_eq!(r.read_line().unwrap(), None);
    }

    #[test]
    fn chunk_extensions_and_trailers_are_consumed() {
        let mut r = exchange(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: ChUnKeD\r\n\r\n5;ext=x\r\nhello\r\n0\r\nChecksum: ok\r\n\r\n",
            "GET",
        );
        assert_eq!(r.read_all().unwrap(), b"hello");
    }

    #[test]
    fn malformed_and_truncated_responses_fail_cleanly() {
        let bad_status = try_exchange("not-http\r\n\r\n", "GET").err().unwrap();
        assert_eq!(bad_status.kind(), io::ErrorKind::InvalidData);
        let cut_headers = try_exchange("HTTP/1.1 200 OK\r\nHeader: value", "GET")
            .err()
            .unwrap();
        assert_eq!(cut_headers.kind(), io::ErrorKind::UnexpectedEof);

        let mut bad_size = exchange(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nnope\r\n",
            "GET",
        );
        assert_eq!(
            bad_size.read_all().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut cut = exchange(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhi",
            "GET",
        );
        assert_eq!(
            cut.read_all().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn not_modified_ignores_declared_body() {
        let mut r = exchange(
            "HTTP/1.1 304 Not Modified\r\nContent-Length: 5\r\n\r\nhello",
            "GET",
        );
        assert_eq!(r.read_all().unwrap(), b"");
    }
}
