//! The kitty graphics protocol: `ESC _ G <keys> ; <payload> ESC \`.
//!
//! Programs send pixels this way (`kitten icat`, `chafa --format=kitty`,
//! terminal-browser). This module is the protocol and the storage; it knows
//! nothing about the grid. Where the command takes effect is decided by the
//! stream, which is why it is split into parts that meet in a queue:
//!
//! - **The reader's side** ([`Splitter`], [`Receiver`]) runs in
//!   `TappedPty::read`, before the parser, and only separates: it takes
//!   `ESC _ G … ESC \` out of the bytes, joins the chunks of a transmission
//!   as base64 text and queues that, undecoded, as a [`Pending::Raw`] record.
//!   In the stream it leaves [`MARK`] in the command's place. (A query whose
//!   pixels are in the sequence is the exception: its answer is due at once, so
//!   it is decoded there, and it may only be small — [`QUERY_LIMIT`]. One that
//!   names a file or shared memory object is not: it is read with the rest and
//!   answered at its marker.)
//! - **The decoding** ([`Hub::decode_pending`]) is the expensive part —
//!   base64, zlib, PNG. The read loop does it between a read and the parse of
//!   it, with the terminal's lease and lock both let go, because a frame
//!   waits for either and an image can take long to inflate.
//! - **The parser's side** ([`Hub::apply_next`]) runs when the parser
//!   dispatches [`MARK`] (`handler::ClusterHandler`): the oldest record is
//!   applied to the store of the screen that is showing **at that point of
//!   the stream** — so an `?1049h` in the same read has already happened, and
//!   a synchronized update (`?2026`) buffers the marker with the rest of the
//!   frame. A marker with no record (a program wrote the bytes itself, or
//!   the stream is being replayed) does nothing.
//!
//! **Placing** is the parser's side too. A placement is anchored to the
//! cursor's grid line at its marker; when the command moves the cursor
//! (`C=0`, the default) the reader writes that movement into the stream
//! beside the marker as ordinary escape sequences ([`Motion`]), so the
//! journal's copy of the screen moves its cursor the same way without any of
//! this. How placements keep their lines while the content scrolls is
//! [`follow`]'s, and what the frame draws is [`Pictures`].
//!
//! **Where the pixels come from** is the command's `t=`. In the escape
//! sequence itself (`d`) they travel as the payload. A file (`f`), a
//! temporary file (`t`) or a shared memory object (`s`) is named by the payload
//! instead, and [`source`] reads it — where the decoding is, never in the
//! read — under rules that treat the name as what it is: text any program, and
//! anything a program prints, can write.
//!
//! Nothing here draws. The store holds decoded pixels and placements per
//! screen ([`Images`]); the frame hands them over as [`Picture`]s.

mod apc;
mod command;
mod follow;
mod place;
mod receive;
mod source;
mod store;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Term;

pub(crate) use apc::{Piece, Splitter};
pub(crate) use place::Motion;
pub(crate) use place::Screen;
pub use place::{Picture, PictureLayer, Pictures};
pub(crate) use receive::{Env, Outcome, Receiver};
pub(crate) use store::{Window, screen_of};

use command::{Action, Command};
use receive::Decoded;
use store::Images;

/// The private mode whose `set` is the marker. No terminal uses the number,
/// and the parser reports an unknown mode to the handler instead of dropping
/// it, which is what lets the marker be dispatched in stream order.
pub(crate) const MARK_MODE: u16 = 8166;

/// What stands in the stream where a graphics command was taken out.
pub(crate) const MARK: &[u8] = b"\x1b[?8166h";

/// The most stored pixel bytes one screen keeps: kitty's own quota, so a
/// program that fits kitty fits here. Past it the images with no placement
/// go first, then the oldest.
pub(crate) const QUOTA_BYTES: usize = 320 * 1024 * 1024;

/// The longest side of an image, kitty's limit.
pub(crate) const MAX_DIMENSION: u32 = 10_000;

/// The most base64 text one command may carry. Chunked transmission (4 KiB a
/// command, as the protocol asks) never gets near it; a single giant command
/// is held whole in memory, so it is bounded well under the quota.
pub(crate) const PAYLOAD_LIMIT: usize = 64 * 1024 * 1024;

/// The most base64 text a transmission may add up to over its chunks. The
/// text is kept whole until the image is decoded, so this is what a
/// transmission costs in memory before the decoded image does; past it a
/// program compresses (`o=z`) or sends a PNG.
pub(crate) const TRANSMISSION_LIMIT: usize = 192 * 1024 * 1024;

/// The most bytes read from a file or shared memory object, where the image
/// is not raw pixels of a known size: the decoded size of
/// [`TRANSMISSION_LIMIT`] worth of base64, so the three media cost the same
/// memory.
pub(crate) const FILE_LIMIT: usize = TRANSMISSION_LIMIT / 4 * 3;

/// The longest file or shared memory name, kitty's limit.
pub(crate) const NAME_LIMIT: usize = 2048;

/// The most base64 text a query (`a=q`) may carry. A query is answered the
/// moment it is read, so it is decoded there and then, in the read path; real
/// ones are a pixel. A query that names a file reads at most the same.
pub(crate) const QUERY_LIMIT: usize = 64 * 1024;

/// How long a chunked transmission may wait for its next chunk before it is
/// dropped. Checked when the next command arrives, not by a timer.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// The keys that address a command's answer: which image, which placement,
/// and how much of the answer the program wants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Echo {
    /// `i=`: the image id; 0 when absent.
    pub(crate) id: u32,
    /// `I=`: the image number; 0 when absent.
    pub(crate) number: u32,
    /// `p=`: the placement id; 0 when absent.
    pub(crate) placement: u32,
    /// `q=`: 1 hides the success answer, 2 hides every answer.
    pub(crate) quiet: u8,
}

impl Echo {
    /// Reads the addressing keys of control data that may be otherwise
    /// unreadable: a key that does not parse is left at 0, so the answer to
    /// a broken command still reaches the program that sent it.
    pub(crate) fn lenient(control: &[u8]) -> Self {
        let mut echo = Self::default();
        for (key, value) in command::pairs(control) {
            let read = command::number(value).unwrap_or(0);
            match key {
                b'i' => echo.id = read,
                b'I' => echo.number = read,
                b'p' => echo.placement = read,
                b'q' => echo.quiet = u8::try_from(read).unwrap_or(0),
                _ => {}
            }
        }
        echo
    }
}

/// Why a command failed, in the protocol's words: `EINVAL:message`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Error {
    code: &'static str,
    message: String,
}

impl Error {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A command or payload that cannot be understood.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new("EINVAL", message)
    }

    /// A payload shorter than the dimensions say.
    pub(crate) fn short(message: impl Into<String>) -> Self {
        Self::new("ENODATA", message)
    }

    /// A payload or image over a limit.
    pub(crate) fn too_big(message: impl Into<String>) -> Self {
        Self::new("EFBIG", message)
    }

    /// A command that names an image there is none of.
    pub(crate) fn missing(message: impl Into<String>) -> Self {
        Self::new("ENOENT", message)
    }

    /// An image that cannot be kept.
    pub(crate) fn no_space(message: impl Into<String>) -> Self {
        Self::new("ENOSPC", message)
    }

    /// A file or shared memory object that could not be read, for whatever
    /// reason: kitty's own words. One answer for all of them, and none of it
    /// the program's: a refusal that says why would tell a program that
    /// only prints text which files this machine has and how big they are,
    /// and an answer that repeats a name would write the program's text
    /// into the PTY's input.
    pub(crate) fn unreadable() -> Self {
        Self::new("EBADF", "Failed to read image file")
    }

    #[cfg(test)]
    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    #[cfg(test)]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// The answer to one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reply {
    echo: Echo,
    result: Result<(), Error>,
}

impl Reply {
    fn ok(echo: Echo) -> Self {
        Self {
            echo,
            result: Ok(()),
        }
    }

    fn failed(echo: Echo, error: Error) -> Self {
        Self {
            echo,
            result: Err(error),
        }
    }

    /// The bytes to write to the program, or `None` when the program asked
    /// not to hear (`q=`) or gave no way to address an answer (no `i=` and
    /// no `I=`).
    pub(crate) fn encode(&self) -> Option<Vec<u8>> {
        let echo = self.echo;
        if echo.id == 0 && echo.number == 0 {
            return None;
        }
        let hidden = match &self.result {
            Ok(()) => echo.quiet >= 1,
            Err(_) => echo.quiet >= 2,
        };
        if hidden {
            return None;
        }
        let mut keys = Vec::new();
        if echo.id != 0 {
            keys.push(format!("i={}", echo.id));
        }
        if echo.number != 0 {
            keys.push(format!("I={}", echo.number));
        }
        if echo.placement != 0 {
            keys.push(format!("p={}", echo.placement));
        }
        let body = match &self.result {
            Ok(()) => "OK".to_owned(),
            Err(error) => format!("{}:{}", error.code, error.message),
        };
        Some(format!("\x1b_G{};{body}\x1b\\", keys.join(",")).into_bytes())
    }
}

/// A command waiting for the parser to reach its marker.
pub(crate) enum Pending {
    /// A transmission that has not been decoded yet: [`Hub::decode_pending`]
    /// turns it into a `Store` or a `Reply`. `text` is the payload as it
    /// came — base64 pixels — or, when the command names a file or shared
    /// memory object (`t=f/t/s`), the name, which is read then. The `motion`
    /// is what the reader wrote around the marker if the command shows the
    /// image (`a=T`).
    Raw {
        command: Command,
        text: Vec<u8>,
        motion: Motion,
    },
    /// Keep a decoded image (and show it, for `a=T`).
    Store {
        command: Command,
        decoded: Decoded,
        motion: Motion,
    },
    /// Show a stored image (`a=p`).
    Put { command: Command, motion: Motion },
    /// Remove placements and, for the upper-case forms, image data.
    Delete(Command),
    /// An answer that has to wait its turn in the stream: the command
    /// failed before it could be applied.
    Reply(Reply),
}

/// The queue of [`Pending`] records and what they are applied to.
///
/// One per session, shared by the reader's two halves. The queue is a leaf
/// lock; the store comes after the terminal's own lock in the lock order
/// (`term` → `images`), because the parser applies commands while it holds
/// `Term`.
pub(crate) struct Hub {
    inbox: Mutex<Inbox>,
    images: Mutex<Images>,
    /// What [`Hub::queue`] lets wait: see there.
    waiting: usize,
    /// How many queued records are still [`Pending::Raw`]. Read without the
    /// queue's lock, by the read loop after every read.
    undecoded: AtomicUsize,
    /// Called before each image is decoded (a test sees what is locked then).
    #[cfg(test)]
    decoding: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Whether a command is half-received: bytes taken from the stream that
    /// no record carries yet. A handover cut here would turn the rest of the
    /// command into text on the other side.
    open: AtomicBool,
}

#[derive(Default)]
struct Inbox {
    queue: VecDeque<Pending>,
    /// Bytes the queued records hold: pixels, or text not yet decoded.
    bytes: usize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Pending {
    fn weight(&self) -> usize {
        match self {
            Self::Raw { text, .. } => text.len(),
            Self::Store { decoded, .. } => decoded.len(),
            Self::Put { .. } | Self::Delete(_) | Self::Reply(_) => 0,
        }
    }

    /// How the reader moved the cursor around this record's marker.
    pub(crate) fn motion(&self) -> Motion {
        match self {
            Self::Raw { motion, .. } | Self::Store { motion, .. } | Self::Put { motion, .. } => {
                *motion
            }
            Self::Delete(_) | Self::Reply(_) => Motion::NONE,
        }
    }
}

impl Hub {
    pub(crate) fn new() -> Self {
        Self::with_limits(QUOTA_BYTES, QUOTA_BYTES)
    }

    /// `quota`: the pixel bytes one screen keeps; `waiting`: the bytes
    /// (pixels, or text not yet decoded) that records may hold while they
    /// wait for their marker.
    fn with_limits(quota: usize, waiting: usize) -> Self {
        Self {
            inbox: Mutex::new(Inbox::default()),
            images: Mutex::new(Images::with_quota(quota)),
            waiting,
            undecoded: AtomicUsize::new(0),
            #[cfg(test)]
            decoding: Mutex::new(None),
            open: AtomicBool::new(false),
        }
    }

    /// Queues a record for the marker that is about to follow it in the
    /// stream. What waits for its marker is bounded too: a synchronized
    /// update can hold markers back while the reader goes on reading, and a
    /// record that would not fit becomes its own failure instead.
    pub(crate) fn queue(&self, pending: Pending) {
        let mut inbox = lock(&self.inbox);
        let pending = match pending {
            Pending::Raw { command, text, .. }
                if inbox.bytes.saturating_add(text.len()) > self.waiting =>
            {
                Pending::Reply(Reply::failed(
                    command.echo,
                    Error::no_space("too many images are waiting to be shown"),
                ))
            }
            other => other,
        };
        if matches!(pending, Pending::Raw { .. }) {
            self.undecoded.fetch_add(1, Ordering::AcqRel);
        }
        inbox.bytes += pending.weight();
        inbox.queue.push_back(pending);
    }

    /// Whether a queued transmission still has to be decoded.
    pub(crate) fn has_undecoded(&self) -> bool {
        self.undecoded.load(Ordering::Acquire) > 0
    }

    /// Decodes every queued transmission still in base64 — the expensive
    /// half of receiving an image. The read loop calls it between a read and
    /// the parse of it, with neither the terminal's lock nor its lease held,
    /// so a frame does not wait for an image to inflate. Only the reader
    /// takes the queue's lock, so holding it meanwhile blocks no one.
    pub(crate) fn decode_pending(&self) {
        let mut inbox = lock(&self.inbox);
        let Inbox { queue, bytes } = &mut *inbox;
        for slot in queue.iter_mut() {
            if !matches!(slot, Pending::Raw { .. }) {
                continue;
            }
            #[cfg(test)]
            if let Some(hook) = lock(&self.decoding).as_ref() {
                hook();
            }
            let Pending::Raw {
                command,
                text,
                motion,
            } = std::mem::replace(slot, Pending::Reply(Reply::ok(Echo::default())))
            else {
                continue;
            };
            *bytes -= text.len();
            self.undecoded.fetch_sub(1, Ordering::AcqRel);
            *slot = self.decoded(command, text, motion, *bytes);
            *bytes += slot.weight();
        }
    }

    /// The record a transmission comes to once its text is decoded; `held`
    /// is what the queue holds besides it.
    fn decoded(&self, command: Command, text: Vec<u8>, motion: Motion, held: usize) -> Pending {
        let echo = command.echo;
        match receive::decode_payload(&command, text) {
            // A query that names a file is a transmission that is not kept:
            // it only waits here, for its marker, to be answered in order.
            Ok(_) if command.action == Action::Query => Pending::Reply(Reply::ok(echo)),
            Ok(decoded) if held.saturating_add(decoded.len()) > self.waiting => {
                Pending::Reply(Reply::failed(
                    echo,
                    Error::no_space("too many images are waiting to be shown"),
                ))
            }
            Ok(decoded) => Pending::Store {
                command,
                decoded,
                motion,
            },
            Err(error) => Pending::Reply(Reply::failed(echo, error)),
        }
    }

    /// Applies the oldest record to the screen that is showing now and
    /// returns the bytes to answer with, if any. `None` and nothing applied
    /// when no record is waiting.
    pub(crate) fn apply_next(&self, screen: &Screen) -> Option<Vec<u8>> {
        let pending = {
            let mut inbox = lock(&self.inbox);
            let pending = inbox.queue.pop_front()?;
            inbox.bytes -= pending.weight();
            pending
        };
        let pending = match pending {
            // The read loop decodes before it parses, so this is a net under
            // it, not a path: the command is not lost if something else drove
            // the parser.
            Pending::Raw {
                command,
                text,
                motion,
            } => {
                self.undecoded.fetch_sub(1, Ordering::AcqRel);
                self.decoded(command, text, motion, 0)
            }
            other => other,
        };
        match pending {
            Pending::Store {
                command,
                decoded,
                motion,
            } => lock(&self.images)
                .store(screen, &command, decoded, motion)
                .encode(),
            Pending::Put { command, motion } => {
                lock(&self.images).put(screen, &command, motion).encode()
            }
            Pending::Delete(command) => {
                lock(&self.images).delete(screen, &command);
                None
            }
            Pending::Reply(reply) => reply.encode(),
            Pending::Raw { .. } => None,
        }
    }

    /// Brings the placements of the screen showing to its grid as it is now.
    /// Called before anything reads or changes them, with the `Term` lock
    /// held.
    pub(crate) fn look<T>(&self, term: &Term<T>, cell: (u16, u16)) {
        lock(&self.images).look(term, cell);
    }

    /// Takes the look of the screen showing as it is now, after something
    /// that knew where the placements went changed them.
    pub(crate) fn settle<T>(&self, term: &Term<T>) {
        lock(&self.images).settle(term);
    }

    /// `CSI 2 J` (`everything`: `CSI 3 J`): the placements on the screen
    /// showing go.
    pub(crate) fn clear_screen(&self, screen: &Screen, everything: bool) {
        lock(&self.images).clear_screen(screen, everything);
    }

    /// The terminal was reset (`RIS`): nothing is kept on either screen.
    pub(crate) fn reset(&self) {
        lock(&self.images).reset();
    }

    /// The main screen's top is grid line `top` now and the history above it
    /// is gone (⌘K and ⌥⌘K). `term` is the main screen's: those cannot run on
    /// the alternate one.
    pub(crate) fn wiped<T>(&self, term: &Term<T>, top: i32, cell: (u16, u16)) {
        let mut images = lock(&self.images);
        images.wiped(top, cell);
        images.settle_main(term);
    }

    /// The grid was resized. `moved`: how many lines up the content went
    /// with the cursor (down when negative); `reflowed`: the width changed or
    /// the main screen is not the one showing, and no line says where a
    /// placement went.
    pub(crate) fn resized<T>(&self, term: &Term<T>, moved: i32, reflowed: bool, cell: (u16, u16)) {
        let mut images = lock(&self.images);
        images.resized(moved, reflowed, cell);
        images.settle_main(term);
    }

    /// The size of a stored image on either screen.
    pub(crate) fn stored_size(&self, echo: Echo) -> Option<(u32, u32)> {
        lock(&self.images).size_of(echo)
    }

    /// Fills `out` with the pictures of the placements the frame draws: those
    /// that reach into the grid's `grid` lines and the fill band's `band`.
    pub(crate) fn pictures<T>(
        &self,
        term: &Term<T>,
        cell: (u16, u16),
        grid: Window,
        band: Option<Window>,
        out: &mut Pictures,
    ) {
        let mut images = lock(&self.images);
        let alt = term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
        if !images.shows(alt) {
            return;
        }
        images.look(term, cell);
        let columns = u16::try_from(term.columns()).unwrap_or(u16::MAX);
        images.collect(alt, cell, columns, grid, band, out);
    }

    /// The alternate screen was entered or left: what it showed goes.
    pub(crate) fn drop_alt(&self) {
        lock(&self.images).drop_alt();
    }

    /// Records whether a command is half-received ([`Hub::open`]).
    pub(crate) fn set_open(&self, open: bool) {
        self.open.store(open, Ordering::Release);
    }

    /// Waits, at most `grace`, for no command to be half-received. The
    /// reader goes on reading meanwhile, so a command in flight finishes.
    /// A client that stopped mid-command costs the whole grace.
    pub(crate) fn wait_closed(&self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while self.open.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Queues a transmission as a read of the PTY would: undecoded.
    #[cfg(test)]
    pub(crate) fn queue_raw(&self, control: &str, text: &[u8]) {
        let command = Command::parse(control.as_bytes()).expect("a command");
        self.queue(Pending::Raw {
            command,
            text: text.to_vec(),
            motion: Motion::NONE,
        });
    }

    /// `hook` runs before each queued image is decoded.
    #[cfg(test)]
    pub(crate) fn on_decode(&self, hook: Box<dyn Fn() + Send>) {
        *lock(&self.decoding) = Some(hook);
    }

    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// Records waiting for their marker.
    #[cfg(test)]
    pub(crate) fn waiting_records(&self) -> usize {
        lock(&self.inbox).queue.len()
    }

    #[cfg(test)]
    pub(crate) fn with_images<R>(&self, read: impl FnOnce(&Images) -> R) -> R {
        read(&lock(&self.images))
    }
}

/// Separated stream that did not fit the buffer the reader gave: the
/// marker is longer than the shortest command it replaces, so a buffer full
/// of commands is longer after the separation than before it.
#[derive(Default)]
pub(crate) struct Spill {
    bytes: Vec<u8>,
    at: usize,
}

impl Spill {
    /// Nothing is waiting to be served.
    pub(crate) fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// Starts a new separation: the (empty) buffer to put its output in.
    pub(crate) fn begin(&mut self) -> &mut Vec<u8> {
        self.bytes.clear();
        self.at = 0;
        &mut self.bytes
    }

    /// Moves what fits of the waiting bytes into `buf`; how many.
    pub(crate) fn serve(&mut self, buf: &mut [u8]) -> usize {
        let rest = &self.bytes[self.at..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.at += n;
        n
    }

    /// The bytes not yet served.
    pub(crate) fn rest(&self) -> &[u8] {
        &self.bytes[self.at..]
    }
}

/// What the parser's side needs from the session it runs in.
pub(crate) trait Sink: Send + Sync {
    fn hub(&self) -> &Hub;

    /// A cell's size in pixels.
    fn cell(&self) -> (u16, u16);

    /// Writes an answer to the program, the way the terminal's other
    /// answers go.
    fn reply(&self, bytes: Vec<u8>);

    /// The session's DECSET 1016 flag ([`crate::input::PIXEL_MOUSE_MODE`]),
    /// which the wrapper sets and clears as the stream says. It lives with
    /// the session rather than in `Term` because alacritty does not know the
    /// mode; the pointer reports read it.
    fn pixel_mouse(&self) -> &AtomicBool;
}

#[cfg(test)]
mod tests;
