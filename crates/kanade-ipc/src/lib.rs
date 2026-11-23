// SPDX-License-Identifier: MIT
//! Versioned, bounded local IPC. No conversion dictionaries or async runtime.
use prost::Message;
use std::io::{self, Read, Write};

mod generated {
    include!("generated/kanade.rs");
}
pub use generated::*;
pub mod diagnostics;
pub mod frontend;
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

/// Version 3 adds `RestoreRequest`.
pub const CURRENT_VERSION: u32 = 3;
pub const RESTORE_VERSION: u32 = 3;
pub const OLDEST_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1 << 20;
pub const MAX_CONTEXT: usize = 2048;
pub const MAX_PREEDIT: usize = 4096;
pub const PAGE_SIZE: usize = 9;
// These limits detect a peer that stopped answering. They are not latency
// targets: a reply inside its limit is delivered, however slow. Each outer
// layer waits longer than the layers inside it, with a margin, so a failure
// normally arrives as an explicit reply. Scheduling can still exceed any finite
// limit; recovery must stay correct when an outer limit expires first.
/// All worker calls made for one key, selection, restore or acknowledgement.
pub const WORKER_KEY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);
/// One worker call outside input: history sync, settings, session open and
/// close, administration. These touch the disk and run while input is idle.
pub const WORKER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);
/// A request the engine has not started by now is refused unprocessed. It may
/// have queued behind one call bounded by `WORKER_TIMEOUT`.
pub const QUEUE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1100);
/// How long a connection waits for the engine to answer its request.
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1750);
/// A frontend's whole exchange, including fragmented reads and blocked writes.
pub const KEY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);
/// A frame that has begun must arrive completely within this time. A
/// connection with no frame in progress has no limit: being idle is healthy.
pub const FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2000);
const _: () = assert!(
    WORKER_TIMEOUT.as_millis() < QUEUE_TIMEOUT.as_millis()
        && QUEUE_TIMEOUT.as_millis() + WORKER_KEY_TIMEOUT.as_millis() < REQUEST_TIMEOUT.as_millis()
        && REQUEST_TIMEOUT.as_millis() < KEY_TIMEOUT.as_millis()
);
pub const MOD_SHIFT: u32 = 1;
pub const MOD_CONTROL: u32 = 2;
pub const MOD_ALT: u32 = 4;
pub const MOD_META: u32 = 8;

impl CandidatePage {
    /// Visible feedback for this list. The footer field on the wire holds the reading.
    pub fn status_line(&self) -> String {
        let status = if self.refinement_pending {
            "AI 評価中…"
        } else {
            match self.refinement_outcome() {
                RefinementOutcome::Evaluated => "AI 評価済み",
                RefinementOutcome::Unavailable => "AI 評価できず・通常候補",
                RefinementOutcome::TimedOut => "AI 時間切れ・通常候補",
                RefinementOutcome::SetupRequired => "AI は設定が必要・通常候補",
                RefinementOutcome::Unspecified => "通常候補",
            }
        };
        format!(
            "{} / {}　{}",
            self.selected.saturating_add(1),
            self.total,
            status
        )
    }

    pub fn mark_refinement_timeout(&mut self) {
        self.refinement_pending = false;
        self.refinement_outcome = RefinementOutcome::TimedOut as i32;
    }
}

/// Known applications learn by default, unless explicitly excluded. An absent
/// or malformed identity never upgrades an unknown input context to normal.
pub fn learning_app_allowed(app: &str, excluded: &[String]) -> bool {
    !app.is_empty()
        && app.len() <= 256
        && app.trim() == app
        && !app.chars().any(char::is_control)
        && !excluded.iter().any(|value| {
            if cfg!(windows) {
                value.eq_ignore_ascii_case(app)
            } else {
                value == app
            }
        })
}

pub fn negotiate(hello: &Hello) -> Option<u32> {
    let low = hello.min_version.max(OLDEST_VERSION);
    let high = hello.max_version.min(CURRENT_VERSION);
    (low <= high).then_some(high)
}
pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok(body)
}
pub fn write_frame<W: Write>(writer: &mut W, body: &[u8]) -> io::Result<()> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    writer.write_all(&(body.len() as u32).to_le_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}
pub fn receive<M: Message + Default, R: Read>(reader: &mut R) -> io::Result<M> {
    decode(&read_frame(reader)?)
}
pub fn decode<M: Message + Default>(frame: &[u8]) -> io::Result<M> {
    M::decode(frame).map_err(|_| io::ErrorKind::InvalidData.into())
}
/// Reads one frame from a long-lived connection. `Ok(None)` is an orderly
/// close between frames. `started` runs once the first byte arrived and must
/// bound the rest of the frame; before that the caller's reader may wait
/// without limit, because a quiet peer is not a failed one.
pub fn read_idle_frame<R: Read>(
    reader: &mut R,
    started: impl FnOnce(&mut R) -> io::Result<()>,
) -> io::Result<Option<Vec<u8>>> {
    let mut first = [0; 1];
    loop {
        match reader.read(&mut first) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    started(reader)?;
    read_frame(&mut first.as_slice().chain(reader)).map(Some)
}
pub fn send<M: Message, W: Write>(writer: &mut W, message: &M) -> io::Result<()> {
    write_frame(writer, &message.encode_to_vec())
}
pub fn utf16_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}
pub fn scalar_to_utf16(s: &str, scalar: usize) -> u32 {
    s.chars().take(scalar).map(char::len_utf16).sum::<usize>() as u32
}
pub fn reading(preedit: &Preedit) -> String {
    preedit
        .segments
        .iter()
        .map(|s| s.reading.as_str())
        .collect()
}
pub fn surface(preedit: &Preedit) -> String {
    preedit.segments.iter().map(|s| s.value.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn halfwidth_kana_do_not_start_composition() {
        for kana in 'ｦ'..='ﾟ' {
            assert!(!frontend::starts_composition(&Key {
                text: kana.to_string(),
                ..Default::default()
            }));
        }
    }
    #[test]
    fn fragmented_truncated_and_oversized_frames() {
        let mut data = Vec::new();
        write_frame(&mut data, b"abc").unwrap();
        assert_eq!(read_frame(&mut data.as_slice()).unwrap(), b"abc");
        assert!(read_frame(&mut &data[..6]).is_err());
        assert!(read_frame(&mut &(MAX_FRAME as u32 + 1).to_le_bytes()[..]).is_err());
        assert!(read_frame(&mut &[0, 0, 0, 0][..]).is_err());
    }
    #[test]
    fn utf16_positions_are_not_utf8_or_scalar_offsets() {
        assert_eq!(scalar_to_utf16("あ😀い", 2), 3);
        assert_eq!(utf16_len("あ😀い"), 4);
    }
    #[test]
    fn accepts_previous_protocol_rejects_disjoint_ranges() {
        let offer = |min_version, max_version| {
            negotiate(&Hello {
                min_version,
                max_version,
            })
        };
        assert_eq!(offer(1, 1), Some(1));
        // A peer from before `RestoreRequest` keeps its own version.
        assert_eq!(offer(1, 2), Some(2));
        assert_eq!(offer(1, 3), Some(RESTORE_VERSION));
        assert_eq!(offer(2, 9), Some(CURRENT_VERSION));
        assert_eq!(offer(CURRENT_VERSION + 1, CURRENT_VERSION + 2), None);
        assert_eq!(offer(2, 1), None);
    }
    #[test]
    fn restore_request_has_a_stable_wire_form() {
        let request = Request {
            body: Some(request::Body::Restore(RestoreRequest {
                revision: Some(Revision {
                    connection: 5,
                    context: 1,
                    composition: 0,
                }),
                event_seq: 1,
                reading: "か".into(),
                context: Some(Context {
                    privacy: Privacy::Private as i32,
                    ..Default::default()
                }),
            })),
        };
        let bytes = request.encode_to_vec();
        // Field 9 of the request, then revision, sequence, reading, context.
        assert_eq!(
            bytes,
            [
                0x4a, 0x11, 0x0a, 0x04, 0x08, 0x05, 0x10, 0x01, 0x10, 0x01, 0x1a, 0x03, 0xe3, 0x81,
                0x8b, 0x22, 0x02, 0x08, 0x02
            ]
        );
        assert_eq!(Request::decode(bytes.as_slice()).unwrap(), request);
        // Earlier requests keep their field numbers.
        let key = Request {
            body: Some(request::Body::Key(KeyRequest::default())),
        };
        assert_eq!(key.encode_to_vec(), [0x12, 0x00]);
        let poll = Request {
            body: Some(request::Body::Poll(CandidatePoll::default())),
        };
        assert_eq!(poll.encode_to_vec(), [0x42, 0x00]);
    }
    #[test]
    fn a_peer_without_restore_reads_it_as_an_empty_request() {
        // Version 2 decoders skip the unknown field; the engine then answers
        // a request without a body as invalid instead of misreading it.
        #[derive(Clone, PartialEq, prost::Message)]
        struct VersionTwo {
            #[prost(message, optional, tag = "2")]
            key: Option<KeyRequest>,
            #[prost(message, optional, tag = "8")]
            poll: Option<CandidatePoll>,
        }
        let request = Request {
            body: Some(request::Body::Restore(RestoreRequest {
                reading: "か".into(),
                ..Default::default()
            })),
        };
        let old = VersionTwo::decode(request.encode_to_vec().as_slice()).unwrap();
        assert_eq!(old, VersionTwo::default());
    }
    #[test]
    fn idle_frames_report_an_orderly_close_and_bound_the_rest() {
        let mut data = Vec::new();
        write_frame(&mut data, b"abc").unwrap();
        let mut reader = data.as_slice();
        let mut started = 0;
        let frame = read_idle_frame(&mut reader, |_| {
            started += 1;
            Ok(())
        });
        assert_eq!(frame.unwrap().unwrap(), b"abc");
        assert!(
            read_idle_frame(&mut reader, |_| unreachable!())
                .unwrap()
                .is_none()
        );
        assert_eq!(started, 1);
        let refused = read_idle_frame(
            &mut data.as_slice(),
            |_| Err(io::ErrorKind::TimedOut.into()),
        );
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
#[cfg(target_os = "macos")]
pub mod settings_log;
