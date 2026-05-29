//! Bulk endpoint stream — once accessory mode is live, `/dev/usb_accessory` is
//! a regular char device backed by the kernel f_accessory driver. Reads pull
//! bytes from the host (car), writes push to it. AAP framing (channel ID + len
//! + protobuf body) is layered on top by the codec module (Phase 3).

#![cfg(any(target_os = "linux", target_os = "android"))]

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const ACCESSORY_DEV: &str = "/dev/usb_accessory";

/// Open the accessory device read+write. O_CLOEXEC is set so child processes
/// (if any) won't inherit it. The kernel returns EAGAIN before accessory mode
/// is live, so callers should retry on EAGAIN/ENODEV with backoff.
pub fn open() -> Result<OwnedFd> {
    open_at(Path::new(ACCESSORY_DEV))
}

pub fn open_at(path: &Path) -> Result<OwnedFd> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(OwnedFd::from(f))
}

// Pull libc through nix's transitive dependency. Adding it directly to
// aabox-aapd's deps wasn't necessary — nix re-exports the constant we need.
mod libc_consts {
    pub use ::nix::libc::O_CLOEXEC;
}
use libc_consts as libc;

// ============================================================================
// UsbAccessoryStream: spawn_blocking-based async I/O for /dev/usb_accessory
// ============================================================================
//
// The accessory chardev (kernel `f_accessory`) is NOT seekable AND does NOT
// implement `.poll` in the Rockchip vendor kernel. Two things this rules out:
//   - `tokio::fs::File`: calls `lseek` inside `poll_write`/`poll_flush` to
//     discard read-buffer overshoot → ESPIPE (errno 29) on every video send
//     after the TLS handshake's large reads. Killed every 2026-05-24 KIA
//     test mid-video-stream.
//   - `tokio::io::unix::AsyncFd`: registers the FD with epoll. The kernel's
//     `epoll_ctl(EPOLL_CTL_ADD)` returns EPERM (errno 1) because f_accessory
//     has no `.poll` file_operation. Verified by deploy 2026-05-24
//     (binary 633403e4): "AsyncFd::with_interest failed: Operation not
//     permitted (os error 1)".
//
// Solution: pure `spawn_blocking` per read/write call. Each operation runs
// on tokio's blocking thread pool, makes a single raw `read(2)`/`write(2)`
// syscall on the chardev, returns the result. No `lseek`, no `epoll`, no
// internal buffering, no flush. The AsyncRead/AsyncWrite trait surface is
// preserved so callers see no change in shape.
//
// State machine: at any moment we are Idle, in-flight Reading, or in-flight
// Writing. `poll_read` while Idle spawns a blocking read; subsequent polls
// of `poll_read` poll the JoinHandle. Same shape for `poll_write`. This is
// the same pattern `tokio::fs::File` uses internally, minus the seek logic.

use ::nix::libc as nix_libc;
use std::io;
use std::os::fd::{IntoRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
// NOTE: deliberately NOT importing `Context` here — this file's top-level
// `use anyhow::Context;` brings the anyhow trait into scope under that name,
// and we don't want to shadow it. We use the full path `std::task::Context`
// where we need the poll context type.
use std::task::Poll;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::JoinHandle;

/// Owns a raw FD and closes it on drop. We can't just store `OwnedFd` because
/// we need to send the FD into `spawn_blocking` closures (`'static` bound),
/// which requires `Arc<Self>`. `Arc<OwnedFd>` doesn't work because closures
/// can't take a non-`Copy` reference into them. So we wrap the raw FD and
/// manage its lifetime manually under `Arc`.
struct FdGuard {
    fd: RawFd,
}

impl FdGuard {
    fn new(fd: RawFd) -> Self {
        Self { fd }
    }
    fn as_raw(&self) -> RawFd {
        self.fd
    }
}

impl Drop for FdGuard {
    fn drop(&mut self) {
        // SAFETY: We're the unique owner of this FD (Arc<FdGuard> ensures
        // single-drop semantics). close(2) is safe on any valid FD.
        unsafe {
            nix_libc::close(self.fd);
        }
    }
}

/// In-flight blocking I/O slots. We keep read and write SEPARATE — sharing one
/// IoState caused a poisoned-state deadlock: when `tokio::select!` cancelled a
/// pending `read_frame` future, the stream was left in `IoState::Reading(handle)`,
/// and a subsequent `poll_write` returned Poll::Pending without registering a
/// waker (the old code had `IoState::Reading(_) => return Poll::Pending` in
/// poll_write) — so writes parked forever. With separate read/write slots, a
/// dropped read JoinHandle becomes garbage we collect on the next poll_read,
/// and writes always have an independent slot to use. Reads and writes on
/// /dev/usb_accessory use separate bulk endpoints (IN vs OUT) so concurrency
/// at the kernel level is safe.
pub struct UsbAccessoryStream {
    read_fd: Arc<FdGuard>,
    write_fd: Arc<FdGuard>,
    read_state: Option<JoinHandle<io::Result<(Vec<u8>, usize)>>>,
    write_state: Option<JoinHandle<io::Result<usize>>>,
}

impl UsbAccessoryStream {
    /// Wrap a single read+write FD (e.g. `/dev/usb_accessory`) for async I/O.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let raw = fd.into_raw_fd();
        tracing::info!(raw_fd = raw, "UsbAccessoryStream::new (spawn_blocking variant)");
        let guard = Arc::new(FdGuard::new(raw));
        Ok(Self {
            read_fd: Arc::clone(&guard),
            write_fd: guard,
            read_state: None,
            write_state: None,
        })
    }

    /// Wrap separate bulk-OUT (read) and bulk-IN (write) FDs — used by the
    /// FunctionFS path where ep1 and ep2 are distinct file descriptors.
    pub fn new_split(read_fd: OwnedFd, write_fd: OwnedFd) -> io::Result<Self> {
        let r = read_fd.into_raw_fd();
        let w = write_fd.into_raw_fd();
        tracing::info!(read_fd = r, write_fd = w, "UsbAccessoryStream::new_split (ffs)");
        Ok(Self {
            read_fd: Arc::new(FdGuard::new(r)),
            write_fd: Arc::new(FdGuard::new(w)),
            read_state: None,
            write_state: None,
        })
    }
}

impl AsyncRead for UsbAccessoryStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Acquire (or take ownership of) the in-flight read JoinHandle.
        let mut handle = match this.read_state.take() {
            Some(h) => h,
            None => {
                // No outstanding read — start a fresh one.
                let cap = buf.remaining();
                if cap == 0 {
                    return Poll::Ready(Ok(()));
                }
                let fd = Arc::clone(&this.read_fd);
                tokio::task::spawn_blocking(move || {
                    let mut v = vec![0u8; cap];
                    // SAFETY: read(2) with our owned FD, a mutable buffer
                    // pointer we just allocated, and length. Returns -1
                    // on error or 0..=len on success.
                    let n = unsafe {
                        nix_libc::read(fd.as_raw(), v.as_mut_ptr() as *mut _, v.len())
                    };
                    if n < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok((v, n as usize))
                })
            }
        };
        match Pin::new(&mut handle).poll(cx) {
            Poll::Pending => {
                // Put the handle back so the NEXT poll_read polls it again.
                this.read_state = Some(handle);
                Poll::Pending
            }
            Poll::Ready(Err(join_err)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                format!("read blocking task: {join_err}"),
            ))),
            Poll::Ready(Ok(Err(e))) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(Ok((data, n)))) => {
                buf.put_slice(&data[..n]);
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncWrite for UsbAccessoryStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        src: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n_req = src.len();
        // Acquire (or take ownership of) the in-flight write JoinHandle.
        let mut handle = match this.write_state.take() {
            Some(h) => h,
            None => {
                tracing::info!(n_req, "poll_write: no in-flight write → spawning blocking write");
                let fd = Arc::clone(&this.write_fd);
                // Copy caller's bytes — we have to move them into the
                // blocking worker (closure must be 'static).
                let buf: Vec<u8> = src.to_vec();
                let buf_len = buf.len();
                tokio::task::spawn_blocking(move || {
                    tracing::info!(buf_len, "blocking write: entering write(2)");
                    // SAFETY: write(2) with our owned FD, a buffer
                    // pointer, and length. Returns -1 on error or
                    // bytes-written on success. Partial writes are
                    // legal under POSIX; the caller handles that via
                    // `write_all`.
                    let n = unsafe {
                        nix_libc::write(fd.as_raw(), buf.as_ptr() as *const _, buf.len())
                    };
                    tracing::info!(buf_len, n, "blocking write: write(2) returned");
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                })
            }
        };
        match Pin::new(&mut handle).poll(cx) {
            Poll::Pending => {
                tracing::trace!(n_req, "poll_write: still Pending");
                this.write_state = Some(handle);
                Poll::Pending
            }
            Poll::Ready(Err(join_err)) => {
                tracing::warn!(n_req, %join_err, "poll_write: blocking task join error");
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("write blocking task: {join_err}"),
                )))
            }
            Poll::Ready(Ok(Err(e))) => {
                tracing::warn!(n_req, %e, "poll_write: write(2) returned error");
                Poll::Ready(Err(e))
            }
            Poll::Ready(Ok(Ok(n))) => {
                tracing::info!(n_req, n, "poll_write: write(2) ok");
                Poll::Ready(Ok(n))
            }
        }
    }

    /// Chardev writes hit the kernel's bulk endpoint synchronously by the
    /// time `write(2)` returns. There's no application-layer buffer to
    /// drain, so `poll_flush` is a no-op. Crucially we do NOT call any
    /// seek or sync syscall — that's what tripped ESPIPE on tokio's File.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        // Dropping the Arc<FdGuard> closes the underlying FD on its last
        // reference. Nothing extra at the AAP layer — the host (KIA) sees
        // the bulk endpoint go away once the gadget unbinds.
        Poll::Ready(Ok(()))
    }
}

use std::future::Future;
