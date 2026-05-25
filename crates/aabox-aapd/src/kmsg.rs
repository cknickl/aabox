//! Mirror INFO+ tracing events to `/dev/kmsg` so they appear in `dmesg` and on
//! the UART console. Useful for in-car debugging where there's no display and
//! no laptop tail; pull `dmesg` after the drive, or watch UART live.
//!
//! Only INFO/WARN/ERROR are mirrored (DEBUG/TRACE would spam the kernel ring
//! buffer). Open failures are silent — on host builds or without permission
//! the layer is a no-op.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

pub struct KmsgLayer {
    file: Mutex<std::fs::File>,
}

impl KmsgLayer {
    pub fn try_new() -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/kmsg")
            .ok()?;
        Some(Self {
            file: Mutex::new(file),
        })
    }
}

impl<S> Layer<S> for KmsgLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // INFO/WARN/ERROR only; DEBUG/TRACE skipped.
        if *meta.level() > Level::INFO {
            return;
        }

        // <N> is the syslog facility|priority byte that /dev/kmsg parses.
        // 6 = KERN_INFO, 4 = KERN_WARNING, 3 = KERN_ERR.
        let prio = match *meta.level() {
            Level::ERROR => 3,
            Level::WARN => 4,
            _ => 6,
        };

        let mut buf = String::with_capacity(128);
        let _ = write!(buf, "<{}>aabox-aapd[{}]: ", prio, meta.target());

        let mut v = FieldFormatter {
            buf: &mut buf,
            wrote_message: false,
        };
        event.record(&mut v);

        // /dev/kmsg writes are one-line-per-syscall; trailing newline closes it.
        buf.push('\n');

        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(buf.as_bytes());
        }
    }
}

struct FieldFormatter<'a> {
    buf: &'a mut String,
    wrote_message: bool,
}

impl<'a> Visit for FieldFormatter<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.buf, "{:?}", value);
            self.wrote_message = true;
        } else {
            let sep = if self.wrote_message { " " } else { "" };
            let _ = write!(self.buf, "{}{}={:?}", sep, field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            let _ = write!(self.buf, "{}", value);
            self.wrote_message = true;
        } else {
            let sep = if self.wrote_message { " " } else { "" };
            let _ = write!(self.buf, "{}{}=\"{}\"", sep, field.name(), value);
        }
    }
}
