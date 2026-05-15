//! AsyncRead wrapper that decodes HTTP/1.1 chunked transfer encoding.
//!
//! Source-protocol clients that respect HTTP body framing (typically
//! libraries like `reqwest` driving a streaming body) send their payload as
//! chunked encoding:
//!
//! ```text
//! <hex-size>[;ext]\r\n
//! <size bytes of payload>\r\n
//! <hex-size>[;ext]\r\n
//! <size bytes of payload>\r\n
//! 0\r\n
//! \r\n             (no trailers — we ignore optional trailer headers)
//! ```
//!
//! `handle_source_connection` reads directly from the TCP socket without
//! going through hyper, so we have to decode the framing ourselves. The
//! wrapper presents the decoded payload as a continuous byte stream and
//! signals EOF (`Ok(())` with no bytes filled) once the terminating
//! zero-size chunk is consumed.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

const MAX_SIZE_LINE_BYTES: usize = 64;
const MAX_TRAILER_BYTES: usize = 4096;

pub(crate) struct ChunkedDecoder<R> {
    inner: R,
    state: State,
    /// Scratch buffer used to accumulate chunk-size lines and final trailers.
    /// Never grows beyond `MAX_TRAILER_BYTES`.
    scratch: Vec<u8>,
}

enum State {
    /// Reading the next `<hex>[;ext]\r\n` chunk-size line.
    NextSize,
    /// Forwarding `remaining` bytes of chunk payload.
    Payload { remaining: u64 },
    /// Consuming the `\r\n` that follows a non-zero payload. `seen` is 0 or 1.
    AfterPayload { seen: u8 },
    /// Zero-size chunk seen; consuming optional trailer headers until the
    /// final blank `\r\n` line.
    Trailers,
    /// Stream complete — further reads return EOF.
    Done,
    /// A previous poll returned an error; subsequent reads also return EOF.
    Errored,
}

impl<R> ChunkedDecoder<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self { inner, state: State::NextSize, scratch: Vec::with_capacity(64) }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ChunkedDecoder<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        dst: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            match self.state {
                State::Done | State::Errored => return Poll::Ready(Ok(())), // EOF

                State::Payload { remaining: 0 } => {
                    self.state = State::AfterPayload { seen: 0 };
                }

                State::Payload { remaining } => {
                    if dst.remaining() == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    // Read straight from inner into the caller's buffer, but
                    // cap by the chunk's remaining bytes so we never bleed
                    // past the payload boundary.
                    let cap = std::cmp::min(remaining, dst.remaining() as u64) as usize;
                    let before = dst.filled().len();
                    let mut limited = dst.take(cap);
                    match Pin::new(&mut self.inner).poll_read(cx, &mut limited) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => {
                            self.state = State::Errored;
                            return Poll::Ready(Err(e));
                        }
                        Poll::Ready(Ok(())) => {
                            let n = limited.filled().len();
                            if n == 0 {
                                // Inner EOF mid-payload — malformed chunked stream.
                                self.state = State::Errored;
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "chunked stream ended mid-payload",
                                )));
                            }
                            // SAFETY of `assume_init` not needed: poll_read
                            // already marked the bytes as filled in the inner
                            // ReadBuf via `limited`. We have to mirror that
                            // into the parent ReadBuf manually.
                            unsafe {
                                dst.assume_init(before + n);
                            }
                            dst.set_filled(before + n);
                            self.state = State::Payload { remaining: remaining - n as u64 };
                            return Poll::Ready(Ok(()));
                        }
                    }
                }

                State::NextSize => match self.poll_fill_line(cx, MAX_SIZE_LINE_BYTES)? {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(line) => {
                        let n = parse_chunk_size(&line).map_err(|e| {
                            self.state = State::Errored;
                            e
                        })?;
                        self.state = if n == 0 {
                            State::Trailers
                        } else {
                            State::Payload { remaining: n }
                        };
                    }
                },

                State::AfterPayload { mut seen } => {
                    let mut byte = [0u8; 1];
                    let mut buf = ReadBuf::new(&mut byte);
                    match Pin::new(&mut self.inner).poll_read(cx, &mut buf) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => {
                            self.state = State::Errored;
                            return Poll::Ready(Err(e));
                        }
                        Poll::Ready(Ok(())) if buf.filled().is_empty() => {
                            self.state = State::Errored;
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "chunked stream ended in payload trailer",
                            )));
                        }
                        Poll::Ready(Ok(())) => {
                            let want = if seen == 0 { b'\r' } else { b'\n' };
                            if byte[0] != want {
                                self.state = State::Errored;
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "chunked payload not followed by CRLF",
                                )));
                            }
                            seen += 1;
                            if seen == 2 {
                                self.state = State::NextSize;
                            } else {
                                self.state = State::AfterPayload { seen };
                            }
                        }
                    }
                }

                State::Trailers => match self.poll_fill_line(cx, MAX_TRAILER_BYTES)? {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(line) => {
                        if line.is_empty() {
                            // Blank line → end of trailers.
                            self.state = State::Done;
                        }
                        // Otherwise the line was a trailer header we don't
                        // need; keep reading more lines.
                    }
                },
            }
        }
    }
}

impl<R: AsyncRead + Unpin> ChunkedDecoder<R> {
    /// Read one CRLF-terminated line from `inner` into `self.scratch`, then
    /// strip the trailing `\r\n` and return the line contents. The CRLF is
    /// guaranteed to be present.
    fn poll_fill_line(
        &mut self,
        cx: &mut Context<'_>,
        max_bytes: usize,
    ) -> Poll<io::Result<Vec<u8>>> {
        loop {
            // Have we already accumulated a complete CRLF-terminated line?
            if let Some(pos) = find_crlf(&self.scratch) {
                let mut line: Vec<u8> = self.scratch.drain(..pos + 2).collect();
                line.truncate(line.len() - 2); // strip trailing \r\n
                return Poll::Ready(Ok(line));
            }
            if self.scratch.len() >= max_bytes {
                self.state = State::Errored;
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunked header line too long",
                )));
            }
            // Read one more byte at a time. Chunk-size lines are short, so
            // the simplicity matters more than throughput here.
            let mut byte = [0u8; 1];
            let mut buf = ReadBuf::new(&mut byte);
            match Pin::new(&mut self.inner).poll_read(cx, &mut buf) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) if buf.filled().is_empty() => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "chunked stream ended mid-header",
                    )));
                }
                Poll::Ready(Ok(())) => {
                    self.scratch.push(byte[0]);
                }
            }
        }
    }
}

fn find_crlf(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|w| w == b"\r\n")
}

/// Parse a chunk-size line: hex digits, optionally followed by `;ext...`.
fn parse_chunk_size(line: &[u8]) -> io::Result<u64> {
    let end = line.iter().position(|&b| b == b';').unwrap_or(line.len());
    let hex = std::str::from_utf8(&line[..end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk size not utf-8"))?
        .trim();
    u64::from_str_radix(hex, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk size not hex"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn decode(input: &[u8]) -> io::Result<Vec<u8>> {
        let cursor = std::io::Cursor::new(input.to_vec());
        let mut decoder = ChunkedDecoder::new(cursor);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).await?;
        Ok(out)
    }

    #[tokio::test]
    async fn decodes_simple_two_chunk_stream() {
        let input = b"5\r\nHello\r\n6\r\n World\r\n0\r\n\r\n";
        let out = decode(input).await.unwrap();
        assert_eq!(out, b"Hello World");
    }

    #[tokio::test]
    async fn decodes_zero_chunk_stream() {
        let input = b"0\r\n\r\n";
        let out = decode(input).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn ignores_chunk_extensions() {
        let input = b"3;ext=bar\r\nfoo\r\n0\r\n\r\n";
        let out = decode(input).await.unwrap();
        assert_eq!(out, b"foo");
    }

    #[tokio::test]
    async fn errors_on_malformed_hex() {
        let input = b"zz\r\nhi\r\n0\r\n\r\n";
        let err = decode(input).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn errors_on_unexpected_eof_mid_payload() {
        let input = b"10\r\nshort";
        let err = decode(input).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn decodes_chunk_larger_than_caller_buffer() {
        // Caller reads only 4 bytes at a time; chunk is 11 bytes.
        let input = b"b\r\nHello World\r\n0\r\n\r\n";
        let cursor = std::io::Cursor::new(input.to_vec());
        let mut decoder = ChunkedDecoder::new(cursor);
        let mut out = Vec::new();
        let mut tmp = [0u8; 4];
        loop {
            let n = decoder.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&tmp[..n]);
        }
        assert_eq!(out, b"Hello World");
    }

    #[tokio::test]
    async fn skips_trailers_and_returns_eof() {
        let input = b"3\r\nfoo\r\n0\r\nX-Trailer: yes\r\n\r\n";
        let out = decode(input).await.unwrap();
        assert_eq!(out, b"foo");
    }
}
