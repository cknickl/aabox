//! Encrypted application-data I/O over an established AAP TLS tunnel.
//!
//! After [`tls_tunnel::client_handshake`](crate::tls_tunnel::client_handshake)
//! completes, every subsequent AAP frame for any channel that uses encryption
//! has the `ENCRYPTED` flag set and a payload that's the TLS-encrypted form
//! of the plaintext frame body. This module wraps rustls's record I/O so
//! callers can think in terms of plaintext payloads.
//!
//! Note: AAP's frame *header* (channel ID + flags + length) stays plaintext;
//! only the payload bytes pass through rustls.

use anyhow::{anyhow, Context, Result};
use std::io::{Cursor, Read, Write};

/// Encrypt one plaintext payload, returning the bytes that should be wrapped
/// in an AAP frame with the `ENCRYPTED` flag set.
///
/// Generic over the rustls connection side (client or server) via the
/// crate-private [`Side`] trait, which deliberately uses *different* method
/// names than rustls's inherent methods to avoid name-resolution recursion
/// against the `Deref<Target = ConnectionCommon<...>>` impls.
pub fn encrypt_payload<C: Side>(conn: &mut C, plaintext: &[u8]) -> Result<Vec<u8>> {
    conn.tls_writer_write_all(plaintext)
        .context("rustls writer.write_all")?;
    let mut out = Vec::new();
    // Drain everything rustls wants to send. write_tls returns 0 when done.
    loop {
        let n = conn.tls_write_records(&mut out).context("rustls write_tls")?;
        if n == 0 {
            break;
        }
    }
    Ok(out)
}

/// Decrypt one AAP frame's encrypted payload, returning the original
/// plaintext bytes.
pub fn decrypt_payload<C: Side>(conn: &mut C, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(ciphertext);
    while cursor.position() < ciphertext.len() as u64 {
        conn.tls_read_records(&mut cursor)
            .context("rustls read_tls")?;
    }
    conn.tls_process_new_packets()
        .map_err(|e| anyhow!("rustls process_new_packets: {e}"))?;

    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match conn.tls_reader_read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(anyhow!("rustls reader.read: {e}")),
        }
    }
    Ok(out)
}

/// Bridge trait so `encrypt_payload` / `decrypt_payload` accept either
/// [`rustls::ClientConnection`] or [`rustls::ServerConnection`]. Method names
/// are intentionally distinct from rustls's inherent methods so the impl
/// blocks below can't recursively resolve to themselves.
pub trait Side {
    fn tls_writer_write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
    fn tls_reader_read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn tls_write_records(&mut self, w: &mut dyn Write) -> std::io::Result<usize>;
    fn tls_read_records(&mut self, r: &mut dyn Read) -> std::io::Result<usize>;
    fn tls_process_new_packets(&mut self) -> Result<(), rustls::Error>;
}

impl Side for rustls::ClientConnection {
    fn tls_writer_write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.writer().write_all(buf)
    }
    fn tls_reader_read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader().read(buf)
    }
    fn tls_write_records(&mut self, w: &mut dyn Write) -> std::io::Result<usize> {
        self.write_tls(w)
    }
    fn tls_read_records(&mut self, r: &mut dyn Read) -> std::io::Result<usize> {
        self.read_tls(r)
    }
    fn tls_process_new_packets(&mut self) -> Result<(), rustls::Error> {
        self.process_new_packets().map(|_| ())
    }
}

impl Side for rustls::ServerConnection {
    fn tls_writer_write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.writer().write_all(buf)
    }
    fn tls_reader_read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader().read(buf)
    }
    fn tls_write_records(&mut self, w: &mut dyn Write) -> std::io::Result<usize> {
        self.write_tls(w)
    }
    fn tls_read_records(&mut self, r: &mut dyn Read) -> std::io::Result<usize> {
        self.read_tls(r)
    }
    fn tls_process_new_packets(&mut self) -> Result<(), rustls::Error> {
        self.process_new_packets().map(|_| ())
    }
}
