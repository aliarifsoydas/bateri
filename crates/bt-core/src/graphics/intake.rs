//! The read side of the protocol, as `TappedPty::read` runs it: the bytes
//! the PTY gave, with every graphics command taken out and a marker — which
//! carries the cursor's movement and the command's id, as numbers
//! ([`Motion::mark`]) — put in its place.
//!
//! **Plain output goes through as it was read.** A read is offered the whole
//! buffer, the bytes of it that hold no `ESC _` are the stream, untouched
//! (one `ESC` at the end of a read waits for the next, which says whether a
//! command begins), and the number of reads is the number of the PTY's own.
//!
//! **Nothing here touches a file.** A command that carries a name is queued
//! with the name, and the file is read where the image is decoded, outside the
//! terminal's lock.
//!
//! **A marker is longer than the command it stands for may be** (38 bytes
//! against five), so the stream a read comes to may not fit the buffer. What
//! does not fit stays in the read side and is the next thing `read` returns,
//! before the PTY is read again, and [`Hub::backlog`] tells the loop not to
//! stop until it is gone, so that nothing of a read outlives the turn of the
//! loop it was read in. Only a read of many small commands comes to more than
//! the buffer.
//!
//! **Known limits:**
//! - A graphics query is answered where it is read, ahead of everything the
//!   parser answers from the same read, so it can pass the answer to an
//!   `ESC [ c` that came before it in the stream and is still to be answered.
//! - The `ESC` or `ESC _` the splitter holds at the end of a read is in no
//!   journal until it is let go — lost with a crash, carried by a handover. A
//!   read that ends in an `ESC` does not take the path of plain output.
//! - A flood of small commands (a hostile `cat`) can come to more in one
//!   `pty_read` than the journal's `GATE_ROOM`, and the journal of that pane
//!   breaks: crash recovery is gone for it. This could not happen before
//!   graphics. The read of a handover's carried prefix is the same.
//! - While stream is left over, the terminal's lock and lease are kept past
//!   `MAX_LOCKED_READ`: the same flood freezes the frames for as long as it
//!   takes to parse.
//! - [`Intake::undelivered`] does not carry the stream left over. That none is
//!   left when a turn of the loop ends is the loop's to keep.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io;
use std::time::Instant;

use super::{Env, Hub, Outcome, PAYLOAD_LIMIT, Piece, Receiver, Splitter};
use crate::reader::READ_BUFFER_SIZE;

/// Where the ids of the markers come from: a counter, hashed by the standard
/// library's hasher, whose key is random (the system's entropy, once for each
/// one made). 28 bits of it, and never 0, which a marker with no id carries.
/// The ids are not secret from the stream, only from the program: nothing the
/// terminal writes back carries one.
struct Ids {
    key: RandomState,
    count: u64,
}

impl Ids {
    fn next(&mut self) -> u32 {
        self.count += 1;
        (self.key.hash_one(self.count) as u32 & 0x0fff_ffff).max(1)
    }
}

/// Separates graphics commands from a PTY's stream.
pub(crate) struct Intake {
    pub(crate) splitter: Splitter,
    pub(crate) receiver: Receiver,
    ids: Ids,
    /// The stream a read came to, when the buffer had no room for all of it:
    /// from `served` on is still to be read.
    stream: Vec<u8>,
    served: usize,
}

impl Intake {
    pub(crate) fn new() -> Self {
        Self::with_limit(PAYLOAD_LIMIT)
    }

    /// `payload_limit`: the most base64 bytes of one command that are kept.
    pub(crate) fn with_limit(payload_limit: usize) -> Self {
        Self {
            splitter: Splitter::new(payload_limit),
            receiver: Receiver::new(),
            ids: Ids {
                key: RandomState::new(),
                count: 0,
            },
            stream: Vec::new(),
            served: 0,
        }
    }

    /// The bytes read but not yet delivered: the graphics command the
    /// separation was in the middle of, or the beginning of one it was
    /// holding back. The handover carries them on; the adopting side's reader
    /// finishes the command from what the master has left.
    pub(crate) fn undelivered(&self) -> Vec<u8> {
        self.splitter.inflight()
    }

    /// One read of a PTY into `buf`, which every reader of one goes through:
    /// `source` fills the front of `buf` and the stream those bytes come to is
    /// what is left in `buf` and returned. What a previous read left that did
    /// not fit comes first, with no read of the PTY. `WouldBlock` for a read
    /// that delivers nothing though it took bytes in (a command still
    /// arriving, or the beginning of one): `Ok(0)` is what the loop reads as
    /// "nothing to read" on macOS and stops on.
    ///
    /// A query's answer goes to `reply` at once, ahead of everything the
    /// parser answers from this read: a program that asks what the terminal
    /// can draw and then for its attributes (`ESC [ c`) takes the second
    /// answer as "the first is not coming". The channel behind it is first in,
    /// first out.
    pub(crate) fn read(
        &mut self,
        buf: &mut [u8],
        now: Instant,
        source: impl FnOnce(&mut [u8]) -> io::Result<usize>,
        hub: &Hub,
        env: impl FnOnce() -> Env,
        reply: &mut dyn FnMut(Vec<u8>),
    ) -> io::Result<usize> {
        if self.served == self.stream.len() {
            let read = source(buf)?;
            hub.note_raw(read);
            if read == 0 || self.splitter.passes(&buf[..read]) {
                return Ok(read);
            }
            self.separate(&buf[..read], now, hub, env(), reply);
            if self.stream.is_empty() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
        }
        let n = (self.stream.len() - self.served).min(buf.len());
        buf[..n].copy_from_slice(&self.stream[self.served..self.served + n]);
        self.served += n;
        if self.served == self.stream.len() {
            self.stream.clear();
            self.stream.shrink_to(READ_BUFFER_SIZE);
            self.served = 0;
        }
        hub.set_backlog(self.served > 0);
        Ok(n)
    }

    /// Makes `self.stream` of the bytes a read gave: the commands out, a
    /// marker in the place of each.
    fn separate(
        &mut self,
        bytes: &[u8],
        now: Instant,
        hub: &Hub,
        env: Env,
        reply: &mut dyn FnMut(Vec<u8>),
    ) {
        let stored = |echo| hub.stored_size(echo);
        let remote = || hub.in_remote_session();
        let out = &mut self.stream;
        let (receiver, ids) = (&mut self.receiver, &mut self.ids);
        self.splitter.feed(bytes, |piece| match piece {
            Piece::Bytes(bytes) => out.extend_from_slice(bytes),
            Piece::Command(apc) => {
                receiver.receive(apc, now, env, &stored, &remote, |outcome| match outcome {
                    Outcome::Pending(pending) => {
                        let id = ids.next();
                        pending.motion().mark(env.lines, id, out);
                        hub.queue(id, pending);
                    }
                    Outcome::Query(answer) => {
                        if let Some(bytes) = answer.encode() {
                            reply(bytes);
                        }
                    }
                });
            }
        });
        hub.set_open(self.splitter.inside() || self.receiver.is_open());
    }
}
