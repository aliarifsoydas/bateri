//! From a command taken out of the stream to a record the parser can apply.
//!
//! Two stages, on the reader thread, outside the parser. [`Receiver`] is the
//! cheap one and runs inside `TappedPty::read`, under the read lease: it
//! assembles the chunks of a transmission as base64 text and does nothing to
//! it. [`decode_payload`] is the expensive one (base64, zlib, PNG) and runs
//! where neither the lease nor the terminal lock is held
//! ([`Hub::decode_pending`](super::Hub::decode_pending), called by the read
//! loop between a read and the parse of it): a frame must not wait for an
//! image to inflate. Nothing here panics on a bad payload: every way it can
//! be wrong is an [`Error`] that becomes the program's answer.

use std::io::Cursor;
use std::time::Instant;

use miniz_oxide::inflate::{TINFLStatus, decompress_to_vec_zlib_with_limit};

use super::apc::Apc;
use super::command::{self, Action, Command, Format, Medium};
use super::place::{Motion, Spec};
use super::{
    Echo, Error, MAX_DIMENSION, OPEN_TIMEOUT, Pending, QUERY_LIMIT, QUOTA_BYTES, Reply,
    TRANSMISSION_LIMIT,
};
use crate::shell::decode_base64;

/// How many images' sizes the receiver remembers. A placement of an image
/// still waiting in the queue is sized from them; a program that keeps more
/// than this many images at once and places one it sent long ago gets an
/// image that does not move the cursor.
const KNOWN_LIMIT: usize = 256;

/// How much of a transmission's base64 text is read to find a PNG's size.
/// The size is 24 bytes into the file; a compressed file needs the head of
/// its compressed stream, which is longer than the text it holds.
const HEAD_PLAIN: usize = 64;
const HEAD_COMPRESSED: usize = 4096;

/// What the reader knows of the terminal when it takes a command out of the
/// stream: how big the cells are and how far the cursor can go.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Env {
    /// A cell's size in pixels.
    pub(crate) cell: (u16, u16),
    /// The screen's size in cells.
    pub(crate) columns: u16,
    pub(crate) lines: u16,
}

/// A decoded image: straight (non-premultiplied) RGBA, 8 bits a channel,
/// whatever the payload's format was.
pub(crate) struct Decoded {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

impl Decoded {
    pub(crate) fn len(&self) -> usize {
        self.pixels.len()
    }
}

/// What a command comes to.
pub(crate) enum Outcome {
    /// Apply this when the parser reaches the marker.
    Pending(Pending),
    /// A query's answer. It is not stored and has no place in the stream, so
    /// it is the reader's to send, now.
    Query(Reply),
}

/// The size of an image the receiver has seen go by.
struct Known {
    id: u32,
    number: u32,
    size: (u32, u32),
}

/// A chunked transmission in progress.
struct Open {
    /// The keys of the first chunk; a chunk after it names none of them.
    command: Command,
    /// The base64 text of the payload so far, chunks joined as they came.
    text: Vec<u8>,
    since: Instant,
}

/// Assembles transmissions command by command.
#[derive(Default)]
pub(crate) struct Receiver {
    open: Option<Open>,
    /// The sizes of the images sent lately, newest last: placing an image
    /// moves the cursor by its size, which is known here before the image is
    /// decoded (and while it is still waiting for its marker).
    known: Vec<Known>,
}

impl Receiver {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether a transmission is waiting for its next chunk.
    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Takes one command; `out` is called for what it comes to, in order.
    /// `stored` tells the size of an image the store already holds.
    pub(crate) fn receive(
        &mut self,
        apc: Apc,
        now: Instant,
        env: Env,
        stored: &dyn Fn(Echo) -> Option<(u32, u32)>,
        mut out: impl FnMut(Outcome),
    ) {
        if let Some(open) = &self.open
            && now.saturating_duration_since(open.since) > OPEN_TIMEOUT
        {
            let (query, echo) = (open.command.action == Action::Query, open.command.echo);
            self.open = None;
            out(failure(
                query,
                echo,
                Error::invalid("the next chunk did not come in time"),
            ));
        }
        if apc.garbled {
            self.abandon(&mut out, Error::invalid("the control data is too long"));
            return;
        }
        let command = match Command::parse(&apc.control) {
            Ok(command) => command,
            Err((echo, error)) => {
                self.open = None;
                out(failure(command::is_query(&apc.control), echo, error));
                return;
            }
        };
        if self.open.is_some() && !command.explicit_action {
            self.chunk(command, apc, env, &mut out);
            return;
        }
        // A command that names its action ends a transmission in progress.
        self.open = None;
        match command.action {
            Action::Delete => out(Outcome::Pending(Pending::Delete(command))),
            Action::Put => {
                let size = self.size_of(command.echo).or_else(|| stored(command.echo));
                let motion = Self::motion(&command, size, env);
                out(Outcome::Pending(Pending::Put { command, motion }));
            }
            Action::Transmit | Action::TransmitAndPut | Action::Query => {
                if command.medium == Medium::Indirect {
                    let error = Error::invalid("files and shared memory are not supported");
                    out(failure(
                        command.action == Action::Query,
                        command.echo,
                        error,
                    ));
                    return;
                }
                let (query, echo, last) =
                    (command.action == Action::Query, command.echo, !command.more);
                let mut open = Open {
                    command,
                    text: Vec::new(),
                    since: now,
                };
                if let Err(error) = open.append(&apc) {
                    out(failure(query, echo, error));
                    return;
                }
                if last {
                    self.finish(open, env, &mut out);
                } else {
                    self.open = Some(open);
                }
            }
        }
    }

    /// The size of an image sent lately, by id or else by number.
    fn size_of(&self, echo: Echo) -> Option<(u32, u32)> {
        self.known
            .iter()
            .rev()
            .find(|known| {
                if echo.id != 0 {
                    known.id == echo.id
                } else {
                    echo.number != 0 && known.number == echo.number
                }
            })
            .map(|known| known.size)
    }

    fn remember(&mut self, echo: Echo, size: (u32, u32)) {
        if echo.id == 0 && echo.number == 0 {
            return;
        }
        if echo.id != 0 {
            self.known.retain(|known| known.id != echo.id);
        }
        if self.known.len() >= KNOWN_LIMIT {
            self.known.remove(0);
        }
        self.known.push(Known {
            id: echo.id,
            number: echo.number,
            size,
        });
    }

    /// How placing `command` moves the cursor; the cursor stays when the
    /// command asks it to or when the image's size is not known.
    fn motion(command: &Command, size: Option<(u32, u32)>, env: Env) -> Motion {
        match size {
            Some(size) if !command.still => {
                Motion::of(&Spec::new(command, size), env.cell, env.columns)
            }
            _ => Motion::NONE,
        }
    }

    fn chunk(&mut self, command: Command, apc: Apc, env: Env, out: &mut impl FnMut(Outcome)) {
        let Some(mut open) = self.open.take() else {
            return;
        };
        // A late `q=` still counts: the last word on how much to hear.
        if command.echo.quiet != 0 {
            open.command.echo.quiet = command.echo.quiet;
        }
        let last = !command.more;
        if let Err(error) = open.append(&apc) {
            let query = open.command.action == Action::Query;
            out(failure(query, open.command.echo, error));
            return;
        }
        if last {
            self.finish(open, env, out);
        } else {
            self.open = Some(open);
        }
    }

    /// The transmission is whole. A store waits, undecoded, for the read loop
    /// to decode it between a read and its parse; a query is answered here
    /// and now, so it is decoded now — which is why it may only be small.
    fn finish(&mut self, open: Open, env: Env, out: &mut impl FnMut(Outcome)) {
        let Open { command, text, .. } = open;
        if command.action != Action::Query {
            // The size is read from the head of the text, nothing is decoded.
            let size = sniff(&command, &text);
            if let Some(size) = size {
                self.remember(command.echo, size);
            }
            let motion = if command.action == Action::TransmitAndPut {
                Self::motion(&command, size, env)
            } else {
                Motion::NONE
            };
            out(Outcome::Pending(Pending::Raw {
                command,
                text,
                motion,
            }));
            return;
        }
        let echo = command.echo;
        let decoded = if text.len() > QUERY_LIMIT {
            Err(Error::too_big("a query carries a few bytes of image"))
        } else {
            decode_payload(&command, text)
        };
        out(Outcome::Query(match decoded {
            Ok(_) => Reply::ok(echo),
            Err(error) => Reply::failed(echo, error),
        }));
    }

    fn abandon(&mut self, out: &mut impl FnMut(Outcome), error: Error) {
        if let Some(open) = self.open.take() {
            let query = open.command.action == Action::Query;
            out(failure(query, open.command.echo, error));
        }
    }
}

/// A failed command's answer: a query's is the reader's, anything else's
/// waits for its marker like the rest of the stream.
fn failure(query: bool, echo: Echo, error: Error) -> Outcome {
    let reply = Reply::failed(echo, error);
    if query {
        Outcome::Query(reply)
    } else {
        Outcome::Pending(Pending::Reply(reply))
    }
}

impl Open {
    /// Adds one command's payload text.
    fn append(&mut self, apc: &Apc) -> Result<(), Error> {
        if apc.overflowed {
            return Err(Error::too_big("the payload of one command is too large"));
        }
        if self.text.len() + apc.payload.len() > TRANSMISSION_LIMIT {
            return Err(Error::too_big("the image data is too large"));
        }
        self.text.extend_from_slice(&apc.payload);
        Ok(())
    }
}

/// The pixel size a transmission's text holds, read without decoding the
/// image: a raw payload's is in its keys, a PNG's is in its first bytes
/// (`IHDR`), after inflating the head of the stream when it is compressed.
/// `None` when it cannot be told, and the cursor then stays.
fn sniff(command: &Command, text: &[u8]) -> Option<(u32, u32)> {
    if command.format != Format::Png {
        let (width, height) = (command.width, command.height);
        return (width > 0 && height > 0).then_some((width, height));
    }
    let take = if command.zlib {
        HEAD_COMPRESSED
    } else {
        HEAD_PLAIN
    };
    let head = text.get(..take.min(text.len()))?;
    let mut bytes = Vec::with_capacity(head.len() / 4 * 3 + 3);
    decode_base64(head, &mut bytes)?;
    if command.zlib {
        bytes = match decompress_to_vec_zlib_with_limit(&bytes, PNG_HEAD) {
            Ok(inflated) => inflated,
            // Stopped at the limit, or short of input: what came out is the
            // head of the file either way.
            Err(error) => error.output,
        };
    }
    png_size(&bytes)
}

/// The signature, the first chunk's length and name, and its width and height.
const PNG_HEAD: usize = 24;

fn png_size(head: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if head.len() < PNG_HEAD || !head.starts_with(SIGNATURE) || head.get(12..16)? != b"IHDR" {
        return None;
    }
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(head.get(at..at + 4)?.try_into().ok()?))
    };
    let (width, height) = (word(16)?, word(20)?);
    (width > 0 && height > 0).then_some((width, height))
}

/// The image a transmission's base64 text holds, as RGBA.
pub(crate) fn decode_payload(command: &Command, text: Vec<u8>) -> Result<Decoded, Error> {
    // The dimensions first: an image that will be refused is refused before
    // any of its text is decoded.
    if let Some(per_pixel) = command.format.bytes_per_pixel() {
        raw_size(command, per_pixel)?;
    }
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3 + 3);
    if decode_base64(&text, &mut bytes).is_none() {
        return Err(Error::invalid("the payload is not valid base64"));
    }
    drop(text);
    decode(command, bytes)
}

/// The pixels a payload holds, as RGBA.
fn decode(command: &Command, bytes: Vec<u8>) -> Result<Decoded, Error> {
    let raw = command.format.bytes_per_pixel();
    let expected = match raw {
        Some(per_pixel) => Some(raw_size(command, per_pixel)?),
        None => None,
    };
    let bytes = if command.zlib {
        match decompress_to_vec_zlib_with_limit(&bytes, expected.unwrap_or(QUOTA_BYTES)) {
            Ok(inflated) => inflated,
            Err(error) if error.status == TINFLStatus::HasMoreOutput => {
                return Err(Error::too_big("the image data is larger than it should be"));
            }
            Err(_) => return Err(Error::invalid("the compressed data is corrupt")),
        }
    } else {
        bytes
    };
    match (command.format, expected) {
        (Format::Png, _) => decode_png(&bytes),
        (_, Some(expected)) => decode_raw(command, &bytes, expected),
        // `expected` exists exactly for the raw formats.
        _ => Err(Error::invalid("unknown pixel format")),
    }
}

/// The payload size the dimensions of a raw image call for.
fn raw_size(command: &Command, per_pixel: usize) -> Result<usize, Error> {
    let (width, height) = (command.width, command.height);
    if width == 0 || height == 0 {
        return Err(Error::invalid("the image has no dimensions (s= and v=)"));
    }
    check_dimensions(width, height)?;
    // Both sides are bounded by the dimension limit, so this cannot overflow
    // a 64-bit `usize`; a 32-bit one answers "too large" instead.
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(per_pixel))
        .ok_or_else(|| Error::too_big("the image is too large"))
}

/// An image past the side limit or past what the store keeps is refused
/// before any pixel is allocated.
fn check_dimensions(width: u32, height: u32) -> Result<(), Error> {
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::too_big("the image is too large"));
    }
    let stored = (u64::from(width) * u64::from(height)).saturating_mul(4);
    if stored > QUOTA_BYTES as u64 {
        return Err(Error::no_space("the image does not fit the storage quota"));
    }
    Ok(())
}

fn decode_raw(command: &Command, bytes: &[u8], expected: usize) -> Result<Decoded, Error> {
    if bytes.len() < expected {
        return Err(Error::short(format!(
            "insufficient image data: {} < {expected}",
            bytes.len()
        )));
    }
    if bytes.len() > expected {
        return Err(Error::too_big(format!(
            "too much image data: {} > {expected}",
            bytes.len()
        )));
    }
    let pixels = match command.format {
        Format::Rgb => {
            let mut pixels = Vec::with_capacity(bytes.len() / 3 * 4);
            for rgb in bytes.chunks_exact(3) {
                pixels.extend_from_slice(rgb);
                pixels.push(u8::MAX);
            }
            pixels
        }
        _ => bytes.to_vec(),
    };
    Ok(Decoded {
        width: command.width,
        height: command.height,
        pixels,
    })
}

fn decode_png(bytes: &[u8]) -> Result<Decoded, Error> {
    let corrupt = |_| Error::invalid("the PNG data cannot be decoded");
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits { bytes: QUOTA_BYTES });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(corrupt)?;
    let (width, height) = reader.info().size();
    check_dimensions(width, height)?;
    let size = reader
        .output_buffer_size()
        .filter(|&size| size <= QUOTA_BYTES)
        .ok_or_else(|| Error::too_big("the image is too large"))?;
    let mut buffer = vec![0; size];
    let frame = reader.next_frame(&mut buffer).map_err(corrupt)?;
    if frame.bit_depth != png::BitDepth::Eight {
        return Err(Error::invalid("the PNG has an unsupported bit depth"));
    }
    let samples = &buffer[..frame.buffer_size()];
    let pixels = match frame.color_type {
        png::ColorType::Rgba => samples.to_vec(),
        png::ColorType::Rgb => {
            let mut pixels = Vec::with_capacity(samples.len() / 3 * 4);
            for rgb in samples.chunks_exact(3) {
                pixels.extend_from_slice(rgb);
                pixels.push(u8::MAX);
            }
            pixels
        }
        png::ColorType::GrayscaleAlpha => samples
            .chunks_exact(2)
            .flat_map(|pair| [pair[0], pair[0], pair[0], pair[1]])
            .collect(),
        png::ColorType::Grayscale => samples
            .iter()
            .flat_map(|&gray| [gray, gray, gray, u8::MAX])
            .collect(),
        png::ColorType::Indexed => {
            return Err(Error::invalid("the PNG palette could not be expanded"));
        }
    };
    Ok(Decoded {
        width: frame.width,
        height: frame.height,
        pixels,
    })
}
