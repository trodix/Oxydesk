//! Protocol between the unprivileged RDP server (`rdsktp`) and the session agent running as
//! the logged-in user (`rdsktp-agent`), over a Unix socket.
//!
//! Frame: `[tag: u8][length: u32 big-endian][payload]`. Hand-written binary format
//! (no serialization dependency); every field is bounded so that a faulty peer cannot
//! trigger a huge allocation.

use std::io::{Read, Write};

use thiserror::Error;

/// Maximum payload size (a 1080p H.264 frame is a few hundred KiB).
pub const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("E/S : {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid message: {0}")]
    Invalid(&'static str),
}

/// Messages from the server to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToAgent {
    Key {
        scancode: u16,
        pressed: bool,
    },
    PointerMove {
        x: i32,
        y: i32,
    },
    /// 0 left, 1 right, 2 middle, 3 back, 4 forward.
    PointerButton {
        button: u8,
        pressed: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    RequestKeyframe {
        output: u32,
    },
    ClipboardSet(String),
    AudioOpen {
        sample_rate: u32,
        channels: u16,
    },
    AudioClose,
    /// New desktop size (single output); answered by `FromAgent::Resized`.
    Resize {
        width: u32,
        height: u32,
    },
}

/// Messages from the agent to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromAgent {
    Hello {
        outputs: u32,
        width: u32,
        height: u32,
    },
    Video {
        output: u32,
        keyframe: bool,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    ClipboardText(String),
    Audio(Vec<u8>),
    Fatal(String),
    /// Answer to `ToAgent::Resize`.
    Resized {
        ok: bool,
    },
}

struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
}

struct In<'a>(&'a [u8]);

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.0.len() < n {
            return Err(ProtoError::Invalid("payload too short"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool, ProtoError> {
        Ok(self.u8()? != 0)
    }
    fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn i32(&mut self) -> Result<i32, ProtoError> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0).to_vec()
    }
    fn string(&mut self) -> Result<String, ProtoError> {
        String::from_utf8(self.rest()).map_err(|_| ProtoError::Invalid("text is not UTF-8"))
    }
    fn end(&self) -> Result<(), ProtoError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ProtoError::Invalid("extra bytes"))
        }
    }
}

/// Message serializable into a frame.
pub trait Message: Sized {
    fn encode(&self) -> (u8, Vec<u8>);
    fn decode(tag: u8, payload: &[u8]) -> Result<Self, ProtoError>;
}

impl Message for ToAgent {
    fn encode(&self) -> (u8, Vec<u8>) {
        let mut o = Out(Vec::new());
        let tag = match self {
            Self::Key { scancode, pressed } => {
                o.u16(*scancode);
                o.u8(u8::from(*pressed));
                1
            }
            Self::PointerMove { x, y } => {
                o.i32(*x);
                o.i32(*y);
                2
            }
            Self::PointerButton { button, pressed } => {
                o.u8(*button);
                o.u8(u8::from(*pressed));
                3
            }
            Self::Wheel { dx, dy } => {
                o.i32(*dx);
                o.i32(*dy);
                4
            }
            Self::RequestKeyframe { output } => {
                o.u32(*output);
                5
            }
            Self::ClipboardSet(t) => {
                o.bytes(t.as_bytes());
                6
            }
            Self::AudioOpen {
                sample_rate,
                channels,
            } => {
                o.u32(*sample_rate);
                o.u16(*channels);
                7
            }
            Self::AudioClose => 8,
            Self::Resize { width, height } => {
                o.u32(*width);
                o.u32(*height);
                9
            }
        };
        (tag, o.0)
    }

    fn decode(tag: u8, payload: &[u8]) -> Result<Self, ProtoError> {
        let mut i = In(payload);
        let msg = match tag {
            1 => Self::Key {
                scancode: i.u16()?,
                pressed: i.bool()?,
            },
            2 => Self::PointerMove {
                x: i.i32()?,
                y: i.i32()?,
            },
            3 => Self::PointerButton {
                button: i.u8()?,
                pressed: i.bool()?,
            },
            4 => Self::Wheel {
                dx: i.i32()?,
                dy: i.i32()?,
            },
            5 => Self::RequestKeyframe { output: i.u32()? },
            6 => Self::ClipboardSet(i.string()?),
            7 => Self::AudioOpen {
                sample_rate: i.u32()?,
                channels: i.u16()?,
            },
            8 => Self::AudioClose,
            9 => Self::Resize {
                width: i.u32()?,
                height: i.u32()?,
            },
            _ => return Err(ProtoError::Invalid("unknown tag")),
        };
        i.end()?;
        Ok(msg)
    }
}

impl Message for FromAgent {
    fn encode(&self) -> (u8, Vec<u8>) {
        let mut o = Out(Vec::new());
        let tag = match self {
            Self::Hello {
                outputs,
                width,
                height,
            } => {
                o.u32(*outputs);
                o.u32(*width);
                o.u32(*height);
                1
            }
            Self::Video {
                output,
                keyframe,
                width,
                height,
                data,
            } => {
                o.u32(*output);
                o.u8(u8::from(*keyframe));
                o.u32(*width);
                o.u32(*height);
                o.bytes(data);
                2
            }
            Self::ClipboardText(t) => {
                o.bytes(t.as_bytes());
                3
            }
            Self::Audio(d) => {
                o.bytes(d);
                4
            }
            Self::Fatal(t) => {
                o.bytes(t.as_bytes());
                5
            }
            Self::Resized { ok } => {
                o.u8(u8::from(*ok));
                6
            }
        };
        (tag, o.0)
    }

    fn decode(tag: u8, payload: &[u8]) -> Result<Self, ProtoError> {
        let mut i = In(payload);
        let msg = match tag {
            1 => {
                let m = Self::Hello {
                    outputs: i.u32()?,
                    width: i.u32()?,
                    height: i.u32()?,
                };
                i.end()?;
                m
            }
            2 => Self::Video {
                output: i.u32()?,
                keyframe: i.bool()?,
                width: i.u32()?,
                height: i.u32()?,
                data: i.rest(),
            },
            3 => Self::ClipboardText(i.string()?),
            4 => Self::Audio(i.rest()),
            5 => Self::Fatal(i.string()?),
            6 => Self::Resized { ok: i.bool()? },
            _ => return Err(ProtoError::Invalid("unknown tag")),
        };
        Ok(msg)
    }
}

/// Writes a frame (a single `write_all`, the stream is not interleaved if the caller holds the lock).
pub fn write_msg<W: Write, M: Message>(w: &mut W, msg: &M) -> Result<(), ProtoError> {
    let (tag, payload) = msg.encode();
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtoError::Invalid("payload too large"));
    }
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(tag);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Reads a frame; `Ok(None)` if the peer closed cleanly between two frames.
pub fn read_msg<R: Read, M: Message>(r: &mut R) -> Result<Option<M>, ProtoError> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(ProtoError::Invalid("payload too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    M::decode(head[0], &payload).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<M: Message + PartialEq + std::fmt::Debug>(m: M) {
        let mut buf = Vec::new();
        write_msg(&mut buf, &m).unwrap();
        let back: M = read_msg(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn all_messages_roundtrip() {
        roundtrip(ToAgent::Key {
            scancode: 0xE01D,
            pressed: true,
        });
        roundtrip(ToAgent::PointerMove { x: -5, y: 1079 });
        roundtrip(ToAgent::PointerButton {
            button: 4,
            pressed: false,
        });
        roundtrip(ToAgent::Wheel { dx: 0, dy: -120 });
        roundtrip(ToAgent::RequestKeyframe { output: 1 });
        roundtrip(ToAgent::ClipboardSet("Café 😀 日本".into()));
        roundtrip(ToAgent::AudioOpen {
            sample_rate: 48_000,
            channels: 2,
        });
        roundtrip(ToAgent::AudioClose);
        roundtrip(ToAgent::Resize {
            width: 1024,
            height: 576,
        });
        roundtrip(FromAgent::Resized { ok: true });
        roundtrip(FromAgent::Hello {
            outputs: 2,
            width: 1920,
            height: 1080,
        });
        roundtrip(FromAgent::Video {
            output: 1,
            keyframe: true,
            width: 1920,
            height: 1080,
            data: vec![0, 0, 0, 1, 9],
        });
        roundtrip(FromAgent::ClipboardText("x".into()));
        roundtrip(FromAgent::Audio(vec![1, 2, 3, 4]));
        roundtrip(FromAgent::Fatal("boom".into()));
    }

    #[test]
    fn clean_eof_between_frames_is_none() {
        let r: Option<ToAgent> = read_msg(&mut [].as_slice()).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn rejects_garbage() {
        // unknown tag
        let mut b = vec![99, 0, 0, 0, 0];
        assert!(read_msg::<_, ToAgent>(&mut b.as_slice()).is_err());
        // oversized length: rejected without allocation
        b = vec![1, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(read_msg::<_, ToAgent>(&mut b.as_slice()).is_err());
        // payload too short and extra bytes
        assert!(ToAgent::decode(1, &[0]).is_err());
        assert!(ToAgent::decode(8, &[0]).is_err());
        // text is not UTF-8
        assert!(ToAgent::decode(6, &[0xFF, 0xFE]).is_err());
    }
}
