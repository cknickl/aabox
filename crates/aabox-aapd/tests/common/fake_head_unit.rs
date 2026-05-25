//! A Rust-native replacement for Google's Desktop Head Unit (DHU 2.0).
//!
//! ## Why this exists
//!
//! Our daemon's TLS server presents the JVC Kenwood "headunit" cert that
//! aasdk has embedded since 2017. DHU 2.0 enforces a strict allowlist on the
//! cert's `O=` (organization) field during the BoringSSL handshake and
//! rejects our cert outright; we tried several alternates (`Google`,
//! `Android-Auto-Internal`, …) and all got rejected. Patching DHU is
//! fragile across OS versions, and re-issuing the embedded cert under a
//! Google-blessed `O=` opens its own legal can of worms.
//!
//! So instead, we built our own head unit. `FakeHeadUnit` speaks enough of
//! the AA protocol's client side to exercise every channel we advertise:
//!
//!   1. AAP version handshake (initiator role).
//!   2. TLS-over-AAP handshake (TLS client, using `build_client_config()`'s
//!      embedded JVC Kenwood cert — same cert the real DHU presents).
//!   3. Reads the daemon's plaintext `AuthComplete{OK}`.
//!   4. Sends `ServiceDiscoveryRequest`, parses the response, then for every
//!      non-control channel in the response: `ChannelOpenRequest -> OK`.
//!   5. Drives per-channel synthetic traffic and checks the daemon's reply:
//!       - Control: Ping, NavigationFocusRequest, AudioFocusRequest.
//!       - Sensor: SENSOR_START_REQUEST -> response; SENSOR_EVENT (GPS) → log.
//!       - Input: INPUT_EVENT_INDICATION (touch).
//!       - AV channels: SETUP_REQUEST -> SETUP_RESPONSE.
//!       - AV input channels: SETUP_REQUEST then AV_INPUT_OPEN_REQUEST.
//!       - Video: SETUP_REQUEST then START_INDICATION (tolerates empty data).
//!   6. Clean shutdown via `ShutdownRequest` / `ShutdownResponse`.
//!
//! Each step is a separate small method so individual tests can pick and
//! choose what they want to exercise. The `run_full_session` convenience
//! drives every step, including the asserts.
//!
//! Reuses the daemon's own framing + TLS-tunnel code so we test the same
//! wire path the real device would.

use aabox_aapd::control::{read_frame, write_frame};
use aabox_aapd::encrypted::{decrypt_payload, encrypt_payload};
use aabox_aapd::framing::{Frame, FrameType};
use aabox_aapd::{control as aap_control, tls, tls_tunnel};
use aabox_common::{ChannelId, ControlMessageId};
use aabox_proto::data::{GpsLocation, TouchEvent, TouchLocation};
use aabox_proto::enums::{av_channel_setup_status, status, touch_action};
use aabox_proto::messages::{
    AudioFocusRequest, AudioFocusResponse, AvChannelSetupRequest, AvChannelSetupResponse,
    AvChannelStartIndication, AvInputOpenRequest, AvInputOpenResponse, ChannelOpenRequest,
    ChannelOpenResponse, InputEventIndication, NavigationFocusRequest, NavigationFocusResponse,
    PingRequest, PingResponse, SensorEventIndication, SensorStartRequestMessage,
    SensorStartResponseMessage, ServiceDiscoveryRequest, ServiceDiscoveryResponse,
    ShutdownRequest, ShutdownResponse,
};
use anyhow::{anyhow, Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use rustls::pki_types::ServerName;
use rustls::ClientConnection;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

/// AV channel msg ids — duplicated from `aabox_aapd::channels::audio` because
/// those constants live in a private-ish location; pulling them out keeps the
/// test self-contained and immune to refactors that re-shape the module tree.
pub const MSG_AV_SETUP_REQUEST: u16 = 0x8000;
pub const MSG_AV_START_INDICATION: u16 = 0x8001;
pub const MSG_AV_STOP_INDICATION: u16 = 0x8002;
pub const MSG_AV_SETUP_RESPONSE: u16 = 0x8003;
pub const MSG_AV_INPUT_OPEN_REQUEST: u16 = 0x8005;
pub const MSG_AV_INPUT_OPEN_RESPONSE: u16 = 0x8006;

pub const MSG_SENSOR_START_REQUEST: u16 = 0x8001;
pub const MSG_SENSOR_START_RESPONSE: u16 = 0x8002;
pub const MSG_SENSOR_EVENT_INDICATION: u16 = 0x8003;

pub const MSG_INPUT_EVENT_INDICATION: u16 = 0x8001;
pub const MSG_BINDING_REQUEST: u16 = 0x8002;
pub const MSG_BINDING_RESPONSE: u16 = 0x8003;

/// Reasonable bound on any single AAP request/response exchange. If the
/// daemon doesn't answer in this long, something is wrong — fail rather
/// than hang the test runner.
pub const OP_TIMEOUT: Duration = Duration::from_secs(10);

/// Connected, post-TLS fake head unit. Drives the AA client side of a
/// session against a daemon that's already listening on TCP.
pub struct FakeHeadUnit {
    sock: TcpStream,
    tls: ClientConnection,
}

impl FakeHeadUnit {
    /// Connect to an aabox-aapd-style listener at `addr` (typically
    /// `127.0.0.1:<port>` from a `TcpListener::bind("127.0.0.1:0")`), run
    /// the AAP version handshake as initiator, then perform the TLS-over-AAP
    /// handshake as TLS client. Returns an instance ready for application
    /// messages.
    pub async fn connect(addr: std::net::SocketAddr) -> Result<Self> {
        let mut sock = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connect to fake head unit at {addr}"))?;

        // (1) AAP version handshake — we are the initiator (head unit).
        let (major, minor, _status) =
            timeout(OP_TIMEOUT, aap_control::version_handshake_initiator(&mut sock))
                .await
                .map_err(|_| anyhow!("version handshake timed out"))??;
        // The daemon advertises (1,7) per `PROTOCOL_MAJOR` / `PROTOCOL_MINOR`.
        if major != aap_control::PROTOCOL_MAJOR || minor != aap_control::PROTOCOL_MINOR {
            return Err(anyhow!(
                "daemon advertised unexpected protocol version: {major}.{minor} (expected {}.{})",
                aap_control::PROTOCOL_MAJOR,
                aap_control::PROTOCOL_MINOR,
            ));
        }

        // (2) The daemon next sends an empty `SslHandshake` kickoff frame
        // (see `aabox_aapd::main::dhu_listen`). Consume it before driving
        // the TLS handshake — otherwise rustls will see it as a malformed
        // ClientHello.
        let kickoff =
            timeout(OP_TIMEOUT, read_frame(&mut sock))
                .await
                .map_err(|_| anyhow!("SSL kickoff timed out"))??;
        let body = aap_control::ssl_handshake_body(&kickoff)
            .context("daemon kickoff was not an SslHandshake frame")?;
        if !body.is_empty() {
            return Err(anyhow!(
                "expected empty SslHandshake kickoff body, got {} bytes",
                body.len()
            ));
        }

        // (3) TLS handshake. The daemon is the TLS server; we're the TLS
        // client. We reuse the daemon's own `build_client_config()` which
        // presents the same JVC Kenwood cert real DHU would.
        let client_cfg = tls::build_client_config().context("build_client_config")?;
        let name: ServerName<'static> =
            ServerName::try_from("android.car").unwrap().to_owned();
        let mut tls_conn = ClientConnection::new(Arc::clone(&client_cfg), name)
            .context("ClientConnection::new")?;
        timeout(
            OP_TIMEOUT,
            tls_tunnel::client_handshake(&mut sock, &mut tls_conn),
        )
        .await
        .map_err(|_| anyhow!("TLS handshake timed out"))?
        .context("client_handshake")?;
        if tls_conn.is_handshaking() {
            return Err(anyhow!("TLS still handshaking after client_handshake"));
        }

        Ok(Self {
            sock,
            tls: tls_conn,
        })
    }

    /// Wait for the daemon's plaintext `AuthComplete` indication on the
    /// control channel. Returns once received and verified.
    pub async fn expect_auth_complete(&mut self) -> Result<()> {
        let f = timeout(OP_TIMEOUT, read_frame(&mut self.sock))
            .await
            .map_err(|_| anyhow!("AuthComplete read timed out"))??;
        if f.channel_id != ChannelId::Control as u8 {
            return Err(anyhow!(
                "AuthComplete on wrong channel: got {}, want control",
                f.channel_id
            ));
        }
        if f.encrypted {
            return Err(anyhow!("AuthComplete should be plaintext, was encrypted"));
        }
        if f.payload.len() < 2 {
            return Err(anyhow!("AuthComplete payload too short"));
        }
        let msg_id = u16::from_be_bytes([f.payload[0], f.payload[1]]);
        if msg_id != ControlMessageId::AuthComplete as u16 {
            return Err(anyhow!(
                "expected AuthComplete (0x0004), got 0x{:04x}",
                msg_id
            ));
        }
        Ok(())
    }

    /// Send a request body on `channel` with `msg_id`, encrypted.
    pub async fn send_encrypted(
        &mut self,
        channel: ChannelId,
        msg_id: u16,
        body: &[u8],
    ) -> Result<()> {
        let mut plain = BytesMut::with_capacity(2 + body.len());
        plain.put_u16(msg_id);
        plain.put_slice(body);
        let ct = encrypt_payload(&mut self.tls, &plain).context("encrypt_payload")?;
        let frame = Frame {
            channel_id: channel as u8,
            frame_type: FrameType::Bulk,
            control: false,
            encrypted: true,
            total_length: None,
            payload: Bytes::from(ct),
        };
        write_frame(&mut self.sock, &frame).await
    }

    /// Encode `msg` with prost and send as an encrypted frame on `channel`.
    pub async fn send_proto<M: Message>(
        &mut self,
        channel: ChannelId,
        msg_id: u16,
        msg: &M,
    ) -> Result<()> {
        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf).map_err(|e| anyhow!("encode: {e}"))?;
        self.send_encrypted(channel, msg_id, &buf).await
    }

    /// Read the next decrypted (channel, msg_id, body) tuple. Skips empty
    /// frames and bare plaintext (e.g. duplicate AuthCompletes).
    pub async fn recv_decrypted(&mut self) -> Result<(u8, u16, Vec<u8>)> {
        loop {
            let f = timeout(OP_TIMEOUT, read_frame(&mut self.sock))
                .await
                .map_err(|_| anyhow!("frame read timed out"))??;
            let plain = if f.encrypted {
                decrypt_payload(&mut self.tls, &f.payload).context("decrypt_payload")?
            } else {
                f.payload.to_vec()
            };
            if plain.len() < 2 {
                continue;
            }
            let id = u16::from_be_bytes([plain[0], plain[1]]);
            return Ok((f.channel_id, id, plain[2..].to_vec()));
        }
    }

    /// Read frames until we get one matching `(want_chan, want_msg)`. Frames
    /// that don't match are silently dropped — useful for ignoring the
    /// daemon's periodic nav demo emissions that arrive in the middle of
    /// our request/response exchanges.
    pub async fn recv_expect(
        &mut self,
        want_chan: ChannelId,
        want_msg: u16,
    ) -> Result<Vec<u8>> {
        let want_chan_u8 = want_chan as u8;
        let deadline = tokio::time::Instant::now() + OP_TIMEOUT;
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(anyhow!(
                    "timed out waiting for ({:?}, 0x{:04x})",
                    want_chan,
                    want_msg
                ));
            }
            let (chan, mid, body) = self.recv_decrypted().await?;
            if chan == want_chan_u8 && mid == want_msg {
                return Ok(body);
            }
            // Drop unrelated traffic (e.g., the nav demo task). Log via
            // eprintln so failing tests still tell us what got dropped.
            eprintln!(
                "fake-head-unit: ignoring frame chan={} msg=0x{:04x} ({} bytes) while waiting for ({:?}, 0x{:04x})",
                chan, mid, body.len(), want_chan, want_msg
            );
        }
    }

    // ---- Step helpers -----------------------------------------------------

    /// Send a ServiceDiscoveryRequest and return the parsed response.
    pub async fn service_discovery(&mut self) -> Result<ServiceDiscoveryResponse> {
        let req = ServiceDiscoveryRequest {
            device_name: "FakeHeadUnit".into(),
            device_brand: "AABox-Test".into(),
        };
        self.send_proto(
            ChannelId::Control,
            ControlMessageId::ServiceDiscoveryRequest as u16,
            &req,
        )
        .await?;
        let body = self
            .recv_expect(
                ChannelId::Control,
                ControlMessageId::ServiceDiscoveryResponse as u16,
            )
            .await?;
        ServiceDiscoveryResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode ServiceDiscoveryResponse: {e}"))
    }

    /// Open a single channel by id. Returns OK iff the daemon replied with
    /// `ChannelOpenResponse{status=OK}`.
    pub async fn channel_open(&mut self, channel_id: i32) -> Result<()> {
        let req = ChannelOpenRequest {
            priority: 0,
            channel_id,
        };
        self.send_proto(
            ChannelId::Control,
            ControlMessageId::ChannelOpenRequest as u16,
            &req,
        )
        .await?;
        let body = self
            .recv_expect(
                ChannelId::Control,
                ControlMessageId::ChannelOpenResponse as u16,
            )
            .await?;
        let resp = ChannelOpenResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode ChannelOpenResponse: {e}"))?;
        if resp.status != status::Enum::Ok as i32 {
            return Err(anyhow!(
                "channel {channel_id} open returned status {}",
                resp.status
            ));
        }
        Ok(())
    }

    /// Send PingRequest and expect a PingResponse that echoes the timestamp.
    pub async fn ping(&mut self, timestamp: i64) -> Result<()> {
        let req = PingRequest { timestamp };
        self.send_proto(
            ChannelId::Control,
            ControlMessageId::PingRequest as u16,
            &req,
        )
        .await?;
        let body = self
            .recv_expect(ChannelId::Control, ControlMessageId::PingResponse as u16)
            .await?;
        let resp = PingResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode PingResponse: {e}"))?;
        if resp.timestamp != timestamp {
            return Err(anyhow!(
                "ping echo mismatch: sent {}, got {}",
                timestamp,
                resp.timestamp
            ));
        }
        Ok(())
    }

    /// Send NavigationFocusRequest, expect NATIVE (1) response.
    pub async fn navigation_focus(&mut self) -> Result<u32> {
        let req = NavigationFocusRequest { r#type: 1 };
        self.send_proto(
            ChannelId::Control,
            ControlMessageId::NavigationFocusRequest as u16,
            &req,
        )
        .await?;
        let body = self
            .recv_expect(
                ChannelId::Control,
                ControlMessageId::NavigationFocusResponse as u16,
            )
            .await?;
        let resp = NavigationFocusResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode NavigationFocusResponse: {e}"))?;
        Ok(resp.r#type)
    }

    /// Send AudioFocusRequest, expect GAIN response.
    pub async fn audio_focus(&mut self) -> Result<i32> {
        let req = AudioFocusRequest {
            audio_focus_type: 1, // GAIN
        };
        self.send_proto(
            ChannelId::Control,
            ControlMessageId::AudioFocusRequest as u16,
            &req,
        )
        .await?;
        let body = self
            .recv_expect(
                ChannelId::Control,
                ControlMessageId::AudioFocusResponse as u16,
            )
            .await?;
        let resp = AudioFocusResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode AudioFocusResponse: {e}"))?;
        Ok(resp.audio_focus_state)
    }

    /// Send a SENSOR_START_REQUEST and wait for a SENSOR_START_RESPONSE.
    pub async fn sensor_start(&mut self, sensor_type: i32) -> Result<i32> {
        let req = SensorStartRequestMessage {
            sensor_type,
            refresh_interval: 1000,
        };
        self.send_proto(ChannelId::Sensor, MSG_SENSOR_START_REQUEST, &req)
            .await?;
        let body = self
            .recv_expect(ChannelId::Sensor, MSG_SENSOR_START_RESPONSE)
            .await?;
        let resp = SensorStartResponseMessage::decode(&body[..])
            .map_err(|e| anyhow!("decode SensorStartResponseMessage: {e}"))?;
        Ok(resp.status)
    }

    /// Push a single synthetic GPS fix to the daemon. No reply expected.
    pub async fn send_gps_event(&mut self, loc: GpsLocation) -> Result<()> {
        let ev = SensorEventIndication {
            gps_location: vec![loc],
            ..Default::default()
        };
        self.send_proto(ChannelId::Sensor, MSG_SENSOR_EVENT_INDICATION, &ev)
            .await
    }

    /// Push a single touch event on the input channel. No reply expected.
    pub async fn send_touch_event(&mut self, x: u32, y: u32) -> Result<()> {
        let ev = InputEventIndication {
            timestamp: 0,
            disp_channel: 0,
            touch_event: Some(TouchEvent {
                touch_location: vec![TouchLocation {
                    x,
                    y,
                    pointer_id: 0,
                }],
                action_index: 0,
                touch_action: touch_action::Enum::Press as i32,
            }),
            button_event: None,
            absolute_input_event: None,
            relative_input_event: None,
        };
        self.send_proto(ChannelId::Input, MSG_INPUT_EVENT_INDICATION, &ev)
            .await
    }

    /// Send AV SETUP_REQUEST on `channel`, expect SETUP_RESPONSE with OK.
    pub async fn av_setup(&mut self, channel: ChannelId) -> Result<()> {
        let req = AvChannelSetupRequest { config_index: 0 };
        self.send_proto(channel, MSG_AV_SETUP_REQUEST, &req).await?;
        let body = self.recv_expect(channel, MSG_AV_SETUP_RESPONSE).await?;
        let resp = AvChannelSetupResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode AvChannelSetupResponse: {e}"))?;
        if resp.media_status != av_channel_setup_status::Enum::Ok as i32 {
            return Err(anyhow!(
                "AV SETUP on chan {:?} returned status {}",
                channel,
                resp.media_status
            ));
        }
        Ok(())
    }

    /// Send AV input open. Expect AV_INPUT_OPEN_RESPONSE.
    pub async fn av_input_open(&mut self, channel: ChannelId) -> Result<()> {
        let req = AvInputOpenRequest {
            open: true,
            anc: false,
            ec: false,
            max_unacked: 4,
        };
        self.send_proto(channel, MSG_AV_INPUT_OPEN_REQUEST, &req)
            .await?;
        let body = self
            .recv_expect(channel, MSG_AV_INPUT_OPEN_RESPONSE)
            .await?;
        let _resp = AvInputOpenResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode AvInputOpenResponse: {e}"))?;
        Ok(())
    }

    /// Send AV START_INDICATION on `channel`. No reply expected (the daemon
    /// would start emitting data plane frames, but for video we tolerate
    /// silence — there's no encoder hooked up yet).
    pub async fn av_start(&mut self, channel: ChannelId) -> Result<()> {
        let ind = AvChannelStartIndication {
            session: 0,
            config: 0,
        };
        self.send_proto(channel, MSG_AV_START_INDICATION, &ind)
            .await
    }

    /// Send a ShutdownRequest. The daemon replies with ShutdownResponse,
    /// then closes the encrypted stream.
    pub async fn shutdown(&mut self) -> Result<()> {
        let req = ShutdownRequest { reason: 0 };
        self.send_proto(
            ChannelId::Control,
            ControlMessageId::ShutdownRequest as u16,
            &req,
        )
        .await?;
        let body = self
            .recv_expect(
                ChannelId::Control,
                ControlMessageId::ShutdownResponse as u16,
            )
            .await?;
        let _resp = ShutdownResponse::decode(&body[..])
            .map_err(|e| anyhow!("decode ShutdownResponse: {e}"))?;
        Ok(())
    }

    /// Close the connection. After this the daemon's control loop sees a
    /// stream error and exits cleanly.
    pub async fn close(self) -> Result<()> {
        drop(self.sock);
        Ok(())
    }
}

// Re-exports so callers don't have to re-import proto types just to build
// the small fixtures we use in tests.
pub use aabox_proto::data::GpsLocation as Gps;
