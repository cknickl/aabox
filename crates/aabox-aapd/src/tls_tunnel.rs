//! Drive the rustls handshake through AAP `SslHandshake` control frames.
//!
//! Unlike normal TLS where rustls owns the byte stream, AAP wraps each TLS
//! record (or chunk of records) inside a control-channel AAP frame with
//! `MessageId::SslHandshake`. We drive rustls's [`Connection`] state machine
//! manually:
//!
//! 1. While `is_handshaking()`:
//!    - If `wants_write()`: drain bytes via `write_tls()`, wrap in
//!      `SslHandshake` frame, send.
//!    - Else: read an AAP frame, extract the `SslHandshake` body, feed via
//!      `read_tls()`, then `process_new_packets()`.
//! 2. Return the connection once handshake is complete — subsequent
//!    application data is encrypted/decrypted via the same API
//!    (read_tls/process_new_packets/writer/reader).

use crate::control::{read_frame, ssl_handshake_body, ssl_handshake_frame, write_frame};
use anyhow::{anyhow, Context, Result};
use rustls::{ClientConnection, ServerConnection};
use std::io::Cursor;
use tokio::io::{AsyncRead, AsyncWrite};

/// Drive a `ClientConnection` through its handshake over an AAP-framed stream.
pub async fn client_handshake<S>(
    stream: &mut S,
    conn: &mut ClientConnection,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while conn.is_handshaking() {
        // Drain any outgoing handshake bytes first.
        while conn.wants_write() {
            let mut buf = Vec::new();
            conn.write_tls(&mut buf).context("write_tls")?;
            if !buf.is_empty() {
                write_frame(stream, &ssl_handshake_frame(&buf))
                    .await
                    .context("write SslHandshake frame")?;
            }
        }
        // If still handshaking, expect bytes from the peer.
        if conn.is_handshaking() {
            let frame = read_frame(stream)
                .await
                .context("read SslHandshake frame")?;
            let body = ssl_handshake_body(&frame).context("extract SslHandshake body")?;
            let mut cursor = Cursor::new(body);
            while cursor.position() < body.len() as u64 {
                conn.read_tls(&mut cursor).context("read_tls")?;
            }
            conn.process_new_packets()
                .map_err(|e| anyhow!("process_new_packets: {e}"))?;
        }
    }
    Ok(())
}

/// Mirror of [`client_handshake`] for the server side. Useful for in-process
/// test fixtures (and for any future "AABox as proxy" use case where we'd
/// act as the head unit).
pub async fn server_handshake<S>(
    stream: &mut S,
    conn: &mut ServerConnection,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while conn.is_handshaking() {
        // Server: usually we wait for ClientHello first.
        if conn.is_handshaking() && !conn.wants_write() {
            let frame = read_frame(stream)
                .await
                .context("read SslHandshake frame")?;
            let body = ssl_handshake_body(&frame).context("extract SslHandshake body")?;
            let mut cursor = Cursor::new(body);
            while cursor.position() < body.len() as u64 {
                conn.read_tls(&mut cursor).context("read_tls")?;
            }
            conn.process_new_packets()
                .map_err(|e| anyhow!("process_new_packets: {e}"))?;
        }
        while conn.wants_write() {
            let mut buf = Vec::new();
            conn.write_tls(&mut buf).context("write_tls")?;
            if !buf.is_empty() {
                write_frame(stream, &ssl_handshake_frame(&buf))
                    .await
                    .context("write SslHandshake frame")?;
            }
        }
    }
    Ok(())
}
