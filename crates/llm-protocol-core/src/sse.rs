//! Incremental Server-Sent Events framing.
//!
//! This module frames SSE bytes without assigning provider semantics. Vendor
//! codecs own JSON decoding and translation from [`SseFrame`] to typed stream
//! events. The framer is partition-invariant: the same byte stream produces
//! the same frames regardless of byte chunk boundaries.

use std::mem;

use thiserror::Error;

/// Conservative limit for one framed SSE event. The limit prevents an
/// unterminated event from retaining unbounded stream input in memory.
pub const DEFAULT_MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

/// A fully framed SSE event. Empty dispatches and comment-only blocks are not
/// emitted because they do not dispatch an SSE event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
    pub fields: Vec<SseField>,
}

/// A parsed, ordered SSE field retained for provider codecs that need an
/// extension or comment source location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseField {
    pub kind: SseFieldKind,
    pub value: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseFieldKind {
    Comment,
    Event,
    Data,
    Id,
    Retry,
    Unknown,
}

/// Stateful incremental SSE framing.
#[derive(Clone, Debug)]
pub struct SseFramer {
    line_buffer: Vec<u8>,
    frame_bytes: usize,
    max_frame_bytes: usize,
    current: PendingFrame,
}

#[derive(Clone, Debug, Default)]
struct PendingFrame {
    event: Option<String>,
    data_lines: Vec<String>,
    id: Option<String>,
    retry: Option<u64>,
    fields: Vec<SseField>,
    has_dispatch_field: bool,
}

impl SseFramer {
    pub fn new() -> Self {
        Self::with_max_frame_bytes(DEFAULT_MAX_SSE_FRAME_BYTES)
    }

    pub fn with_max_frame_bytes(max_frame_bytes: usize) -> Self {
        assert!(max_frame_bytes > 0, "max_frame_bytes must be positive");
        Self {
            line_buffer: Vec::new(),
            frame_bytes: 0,
            max_frame_bytes,
            current: PendingFrame::default(),
        }
    }

    /// Push arbitrary raw bytes and return every complete event framed by
    /// blank-line dispatches.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, SseFramingError> {
        let mut frames = Vec::new();
        for &byte in bytes {
            self.frame_bytes = self
                .frame_bytes
                .checked_add(1)
                .expect("usize cannot overflow in a single process buffer");
            if self.frame_bytes > self.max_frame_bytes {
                self.reset();
                return Err(SseFramingError::FrameTooLarge {
                    max_frame_bytes: self.max_frame_bytes,
                });
            }

            if byte == b'\n' {
                if let Some(frame) = self.finish_line()? {
                    frames.push(frame);
                }
            } else {
                self.line_buffer.push(byte);
            }
        }
        Ok(frames)
    }

    /// Finish the byte stream. A final line without a trailing newline is
    /// parsed, but it does not dispatch until a blank line is present.
    pub fn finish(mut self) -> Result<Vec<SseFrame>, SseFramingError> {
        if self.line_buffer.is_empty() {
            return Ok(Vec::new());
        }

        self.parse_current_line()?;
        Ok(Vec::new())
    }

    pub fn is_idle(&self) -> bool {
        self.line_buffer.is_empty() && !self.current.has_dispatch_field
    }

    fn finish_line(&mut self) -> Result<Option<SseFrame>, SseFramingError> {
        let line = self.take_line()?;
        if line.is_empty() {
            let frame = self.current.take_frame();
            self.frame_bytes = 0;
            return Ok(frame);
        }

        self.parse_line(&line);
        Ok(None)
    }

    fn parse_current_line(&mut self) -> Result<(), SseFramingError> {
        let line = self.take_line()?;
        if !line.is_empty() {
            self.parse_line(&line);
        }
        Ok(())
    }

    fn take_line(&mut self) -> Result<String, SseFramingError> {
        let bytes = mem::take(&mut self.line_buffer);
        let line = if bytes.last() == Some(&b'\r') {
            &bytes[..bytes.len() - 1]
        } else {
            &bytes
        };
        String::from_utf8(line.to_vec()).map_err(SseFramingError::InvalidUtf8)
    }

    fn parse_line(&mut self, line: &str) {
        if let Some(comment) = line.strip_prefix(':') {
            self.current.fields.push(SseField {
                kind: SseFieldKind::Comment,
                value: comment.strip_prefix(' ').unwrap_or(comment).to_owned(),
            });
            return;
        }

        let (name, value) = match line.split_once(':') {
            Some((name, value)) => (name, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match name {
            "event" => {
                self.current.event = Some(value.to_owned());
                self.current.has_dispatch_field = true;
                self.current.fields.push(SseField {
                    kind: SseFieldKind::Event,
                    value: value.to_owned(),
                });
            }
            "data" => {
                self.current.data_lines.push(value.to_owned());
                self.current.has_dispatch_field = true;
                self.current.fields.push(SseField {
                    kind: SseFieldKind::Data,
                    value: value.to_owned(),
                });
            }
            "id" => {
                if !value.contains('\0') {
                    self.current.id = Some(value.to_owned());
                    self.current.fields.push(SseField {
                        kind: SseFieldKind::Id,
                        value: value.to_owned(),
                    });
                }
            }
            "retry" => {
                if let Ok(retry) = value.parse::<u64>() {
                    self.current.retry = Some(retry);
                    self.current.fields.push(SseField {
                        kind: SseFieldKind::Retry,
                        value: value.to_owned(),
                    });
                }
            }
            _ => {
                self.current.fields.push(SseField {
                    kind: SseFieldKind::Unknown,
                    value: line.to_owned(),
                });
            }
        }
    }

    fn reset(&mut self) {
        self.line_buffer.clear();
        self.frame_bytes = 0;
        self.current = PendingFrame::default();
    }
}

impl Default for SseFramer {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingFrame {
    fn take_frame(&mut self) -> Option<SseFrame> {
        if !self.has_dispatch_field {
            self.fields.clear();
            return None;
        }

        let pending = mem::take(self);
        Some(SseFrame {
            event: pending.event,
            data: pending.data_lines.join("\n"),
            id: pending.id,
            retry: pending.retry,
            fields: pending.fields,
        })
    }
}

#[derive(Debug, Error)]
pub enum SseFramingError {
    #[error("SSE frame exceeded the configured maximum of {max_frame_bytes} bytes")]
    FrameTooLarge { max_frame_bytes: usize },
    #[error("SSE stream contains invalid UTF-8: {0}")]
    InvalidUtf8(#[source] std::string::FromUtf8Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_all_at_once(input: &[u8]) -> Vec<SseFrame> {
        let mut framer = SseFramer::new();
        let frames = framer.push(input).unwrap();
        assert!(framer.finish().unwrap().is_empty());
        frames
    }

    #[test]
    fn frames_multi_line_events_and_retains_comments() {
        let frames = frame_all_at_once(
            b": synthetic\nid: item-1\nevent: response.delta\ndata: first\ndata: second\nretry: 250\n\n",
        );

        assert_eq!(
            frames,
            vec![SseFrame {
                event: Some("response.delta".to_owned()),
                data: "first\nsecond".to_owned(),
                id: Some("item-1".to_owned()),
                retry: Some(250),
                fields: vec![
                    SseField {
                        kind: SseFieldKind::Comment,
                        value: "synthetic".to_owned()
                    },
                    SseField {
                        kind: SseFieldKind::Id,
                        value: "item-1".to_owned()
                    },
                    SseField {
                        kind: SseFieldKind::Event,
                        value: "response.delta".to_owned()
                    },
                    SseField {
                        kind: SseFieldKind::Data,
                        value: "first".to_owned()
                    },
                    SseField {
                        kind: SseFieldKind::Data,
                        value: "second".to_owned()
                    },
                    SseField {
                        kind: SseFieldKind::Retry,
                        value: "250".to_owned()
                    },
                ],
            }]
        );
    }

    #[test]
    fn frames_are_invariant_across_every_byte_partition() {
        let input = b": note\r\nevent: item\r\ndata: {\"synthetic\":true}\r\n\r\ndata: [DONE]\n\n";
        let expected = frame_all_at_once(input);

        for split in 0..=input.len() {
            let mut framer = SseFramer::new();
            let mut actual = framer.push(&input[..split]).unwrap();
            actual.extend(framer.push(&input[split..]).unwrap());
            assert!(framer.finish().unwrap().is_empty());
            assert_eq!(actual, expected, "split at byte {split}");
        }

        for chunk_size in 1..=input.len() {
            let mut framer = SseFramer::new();
            let mut actual = Vec::new();
            for chunk in input.chunks(chunk_size) {
                actual.extend(framer.push(chunk).unwrap());
            }
            assert!(framer.finish().unwrap().is_empty());
            assert_eq!(actual, expected, "chunk size {chunk_size}");
        }
    }

    #[test]
    fn incomplete_final_event_is_not_dispatched() {
        let mut framer = SseFramer::new();
        assert!(framer.push(b"data: partial").unwrap().is_empty());
        assert!(framer.finish().unwrap().is_empty());
    }

    #[test]
    fn oversized_frame_resets_the_framer() {
        let mut framer = SseFramer::with_max_frame_bytes(4);
        assert!(matches!(
            framer.push(b"data: too long"),
            Err(SseFramingError::FrameTooLarge { max_frame_bytes: 4 })
        ));
        assert!(framer.is_idle());
    }
}
