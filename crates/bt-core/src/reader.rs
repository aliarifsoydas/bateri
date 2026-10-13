//! The PTY reader loop: I/O, parsing and the write queue.
//!
//! **This file is a copy.** Source: `alacritty_terminal` 0.26.0,
//! `src/event_loop.rs` — Copyright Christian Duerr, Joe Wilm and the
//! alacritty contributors, under the Apache License 2.0 (text in the bundle
//! as `THIRD-PARTY-LICENSES.txt`, attribution in `Credits.html`). Per
//! Apache-2.0 §4(b): the file was **modified**, the changes
//! being —
//!
//! - the parser sees `Term` not directly but through [`ClusterHandler`], both
//!   in `advance` and in `stop_sync` on the DEC 2026 timeout;
//! - the `ref_test` recording and `Notifier` were removed (nothing calls
//!   either);
//! - `log::error!` lines became `eprintln!` (`log` is not a dependency of
//!   this crate; logging is still a debt);
//! - a dead channel is an empty read, not a panic (no unjustified panic in
//!   `bt-core`; the branch is unreachable); the re-registration panic was
//!   kept with its justification;
//! - the PTY tokens are `pub(crate)` in alacritty, their values were copied
//!   here (and are `pub(crate)` here too: an adopted PTY registers on them);
//! - comments were translated (first to Turkish, later to English);
//! - an image a read carried (a graphics command) is decoded — and read, when
//!   the command names a file or shared memory object — between that
//!   read and its parse, with the terminal's lease and lock **let go**
//!   ([`crate::graphics::Hub::decode_pending`]) — a frame waits for the
//!   lease as well as the lock, so decoding under either would hold it back;
//! - the update handover: an opt-in read before the first poll
//!   ([`EventLoop::read_first`], so a carried prefix reaches the parser on
//!   a quiet PTY) and, on the loop handed back after `join`, the DEC 2026
//!   buffer applied through the wrapper ([`EventLoop::stop_sync`]), the
//!   PTY borrowed ([`EventLoop::pty_mut`]) and the input not yet written taken
//!   ([`EventLoop::unsent`]); `pty_read` returns the bytes it processed and,
//!   during that first read, waits for the terminal lock instead of reading
//!   on (every read is parsed before the next, so a carried prefix and the
//!   master's bytes never share one `advance` — the adopted session drops
//!   the prefix's replies, `Session::adopt`).
//!
//! Why a copy: grapheme clustering has to step **in between** the parser's
//! `Handler` calls, and alacritty's loop hands `Term` over as a fixed type.
//! The version is pinned with `=0.26.0` for that reason (root `Cargo.toml`).
//!
//! Preserved contracts — `session.rs`'s lock order and shutdown lean on
//! them: the terminal lease is held for the whole of `pty_read` but for that
//! one decode (the `term` → `shell` order in the module header, which the
//! decode does not touch), the locked read is bounded by
//! [`MAX_LOCKED_READ`], `Wakeup` is sent only if unsynchronized bytes were
//! processed, and [`EventLoop::spawn`] returns the `(EventLoop, State)` pair
//! — the `Pty` drops, hence `SIGHUP`, when that pair drops.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi;
use alacritty_terminal::{thread, tty};
use polling::{Event as PollingEvent, Events, PollMode, Poller};

use crate::graphics::{MARKER_LEN, Sink};
use crate::handler::ClusterHandler;
use crate::journal::{Journal, Side};

/// The most bytes read from the PTY before a forced synchronization.
pub(crate) const READ_BUFFER_SIZE: usize = 0x10_0000;

/// The most bytes read from the PTY while the terminal is locked.
pub(crate) const MAX_LOCKED_READ: usize = u16::MAX as usize;

/// Poller token of the read/write fd (alacritty 0.26.0, `tty/unix.rs`,
/// `pub(crate)`). `Pty::register` does the registration, so the number must
/// equal the one given there — the version pin is its guard.
pub(crate) const PTY_READ_WRITE_TOKEN: usize = 0;

/// Poller token of the child-event pipe (same source, same reason).
pub(crate) const PTY_CHILD_EVENT_TOKEN: usize = 1;

/// Messages sent to the loop.
#[derive(Debug)]
pub(crate) enum Msg {
    /// Bytes to write to the PTY.
    Input(Cow<'static, [u8]>),

    /// The loop must shut down.
    Shutdown,

    /// The PTY must be resized.
    Resize(WindowSize),

    /// The journal's compaction made room (or the journal broke): the
    /// read gate is asked again. A channel message, not a bare poller
    /// wake: an empty wake with an empty channel is the DEC 2026 timeout's
    /// arm, which would apply a synchronized update early and ask for a
    /// frame. Sent only to a loop that has a journal.
    Room,
}

/// The most bytes of the PTY a turn of the loop takes in while they deliver
/// nothing ([`EventLoop::read_rounds`]).
const PARTIAL_TURN: usize = 16 * READ_BUFFER_SIZE;

/// What one `pty_read` came to.
struct Round {
    /// Bytes the parser got.
    processed: usize,
    /// Bytes of the PTY taken in (more or fewer than were parsed: commands
    /// become markers).
    taken: usize,
}

impl Round {
    /// Whether the PTY had anything to give.
    fn progressed(&self) -> bool {
        self.processed > 0 || self.taken > 0
    }
}

/// Sending end of the loop's channel: queues the message and wakes the
/// poller.
#[derive(Clone)]
pub(crate) struct EventLoopSender {
    sender: Sender<Msg>,
    poller: Arc<Poller>,
}

impl EventLoopSender {
    /// `Err` if the receiver dropped or the poller could not be woken; the
    /// caller's only question is "did it go through", no detail is carried.
    pub(crate) fn send(&self, msg: Msg) -> Result<(), ()> {
        self.sender.send(msg).map_err(|_| ())?;
        self.poller.notify().map_err(|_| ())
    }
}

/// Tracks how much of a buffer has been written.
struct Writing {
    source: Cow<'static, [u8]>,
    written: usize,
}

impl Writing {
    #[inline]
    fn new(c: Cow<'static, [u8]>) -> Writing {
        Writing {
            source: c,
            written: 0,
        }
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.written += n;
    }

    #[inline]
    fn remaining_bytes(&self) -> &[u8] {
        &self.source[self.written..]
    }

    #[inline]
    fn finished(&self) -> bool {
        self.written >= self.source.len()
    }
}

/// All of the loop's mutable state: the write queue, the buffer being
/// written and the parser.
#[derive(Default)]
pub(crate) struct State {
    write_list: VecDeque<Cow<'static, [u8]>>,
    writing: Option<Writing>,
    parser: ansi::Processor,
    /// Whether the last `Handler` call was `input` — [`ClusterHandler`]'s
    /// only state. It lives here because the wrapper is reborn on every
    /// `advance`: a cluster split across two `read` chunks (`👍` · `🏽`) is
    /// still unclosed.
    last_input: bool,
}

impl State {
    #[inline]
    fn ensure_next(&mut self) {
        if self.writing.is_none() {
            self.goto_next();
        }
    }

    #[inline]
    fn goto_next(&mut self) {
        self.writing = self.write_list.pop_front().map(Writing::new);
    }

    #[inline]
    fn take_current(&mut self) -> Option<Writing> {
        self.writing.take()
    }

    #[inline]
    fn needs_write(&self) -> bool {
        self.writing.is_some() || !self.write_list.is_empty()
    }

    #[inline]
    fn set_current(&mut self, new: Option<Writing>) {
        self.writing = new;
    }
}

/// A receiver that can peek at the next message: the timeout arm asks "is
/// there a message in the channel" without consuming it.
struct PeekableReceiver<T> {
    rx: Receiver<T>,
    peeked: Option<T>,
}

impl<T> PeekableReceiver<T> {
    fn new(rx: Receiver<T>) -> Self {
        Self { rx, peeked: None }
    }

    fn peek(&mut self) -> Option<&T> {
        if self.peeked.is_none() {
            self.peeked = self.rx.try_recv().ok();
        }

        self.peeked.as_ref()
    }

    fn recv(&mut self) -> Option<T> {
        // alacritty panics when the channel dies (`Disconnected`). That
        // branch is unreachable here — one of the sender ends
        // (`EventLoop::tx`) is the loop's own field, so the channel does not
        // die while the loop lives — and the no-panic rule reduces it to an
        // empty read.
        self.peeked.take().or_else(|| self.rx.try_recv().ok())
    }
}

/// The reader loop: the PTY I/O and the parser that updates `Term`.
pub(crate) struct EventLoop<T: tty::EventedPty, U: EventListener> {
    poll: Arc<Poller>,
    pty: T,
    rx: PeekableReceiver<Msg>,
    tx: Sender<Msg>,
    terminal: Arc<FairMutex<Term<U>>>,
    event_proxy: U,
    drain_on_exit: bool,
    /// Whether clustering is on (`SessionOptions::cluster`); passed to the
    /// wrapper on every call.
    cluster: bool,
    /// How many carried bytes the thread reads before its first
    /// `poll.wait` ([`EventLoop::read_first`]); `None`: none.
    read_first: Option<usize>,
    /// The session's journal ([`EventLoop::journal`]); `None`: none, and
    /// the loop runs as alacritty's does.
    journal: Option<Arc<Journal>>,
    /// The session's graphics ([`EventLoop::graphics`]); `None`: none, and
    /// the marker the stream may carry does nothing.
    graphics: Option<Arc<dyn Sink>>,
}

/// The read gate's state between two loop rounds: while closed, since when
/// the compaction's progress last moved and what it read then.
#[derive(Default)]
struct Gate {
    stalled: Option<(Instant, u64)>,
}

/// Whether an empty wake with an empty channel is the DEC 2026 timeout.
/// Without a journal it always is — alacritty's arm, kept as it was (a wake
/// whose message an earlier round already drained lands here too, and
/// applies a pending update early). With one, only once the update's own
/// deadline has passed: the poll also wakes for the read gate's stall
/// deadline, and every `stop_sync` is a side record — even one with nothing
/// pending closes the open cluster — so a spurious one would apply what
/// the application is still writing, ask for a frame and fill the side
/// stream.
fn sync_due(sync: Option<Instant>, journaled: bool, now: Instant) -> bool {
    !journaled || sync.is_some_and(|deadline| deadline <= now)
}

impl<T, U> EventLoop<T, U>
where
    T: tty::EventedPty + OnResize + Send + 'static,
    U: EventListener + Send + 'static,
{
    pub(crate) fn new(
        terminal: Arc<FairMutex<Term<U>>>,
        event_proxy: U,
        pty: T,
        drain_on_exit: bool,
        cluster: bool,
    ) -> io::Result<EventLoop<T, U>> {
        let (tx, rx) = mpsc::channel();
        let poll = Poller::new()?.into();
        Ok(EventLoop {
            poll,
            pty,
            tx,
            rx: PeekableReceiver::new(rx),
            terminal,
            event_proxy,
            drain_on_exit,
            cluster,
            read_first: None,
            journal: None,
            graphics: None,
        })
    }

    /// Applies the graphics commands the stream carries ([`crate::graphics`]):
    /// the parser's wrapper dispatches their markers into `graphics`.
    pub(crate) fn graphics(&mut self, graphics: Arc<dyn Sink>) {
        self.graphics = Some(graphics);
    }

    /// Records the PTY into `journal`: the bytes `advance` applies are
    /// counted, the DEC 2026 timeout is recorded, and reading stops while
    /// the journal has no room for one more read ([`Journal::gate_open`]).
    pub(crate) fn journal(&mut self, journal: Arc<Journal>) {
        self.journal = Some(journal);
    }

    /// Whether the journal has room for the next read — always without a
    /// journal.
    fn gate_open(&self) -> bool {
        self.journal
            .as_ref()
            .is_none_or(|journal| journal.gate_open())
    }

    /// The read gate, before the next poll: closes the PTY's read interest
    /// while the journal has no room for a whole `pty_read` (the poll is
    /// level-triggered — a readable fd left registered would spin the
    /// loop) and opens it again once it has. While closed it asks the
    /// compaction to run and watches its progress: none for the journal's
    /// stall time breaks the journal and opens the gate, so a dead or stuck
    /// compaction cannot hold the pane. Returns the stall deadline while
    /// closed. Without a journal nothing happens and `None` comes back.
    fn gate(
        &mut self,
        gate: &mut Gate,
        interest: &mut PollingEvent,
        poll_opts: PollMode,
    ) -> Option<Instant> {
        let journal = Arc::clone(self.journal.as_ref()?);
        let mut open = journal.gate_open();
        if open {
            gate.stalled = None;
        } else {
            let now = Instant::now();
            let progress = journal.progress();
            match gate.stalled {
                Some((since, seen)) if seen == progress => {
                    if now >= since + journal.stall() {
                        eprintln!(
                            "bateri: the journal's compaction made no progress, the journal breaks"
                        );
                        journal.break_journal();
                        gate.stalled = None;
                        open = true;
                    }
                }
                _ => {
                    gate.stalled = Some((now, progress));
                    Journal::poke(&journal);
                }
            }
        }
        if open != interest.readable {
            interest.readable = open;
            // The same deliberate panic as the write interest's below.
            if let Err(err) = self.pty.reregister(&self.poll, *interest, poll_opts) {
                panic!("reader loop re-registration failed: {err}"); // audit: alacritty parity, the crash reaches the shutdown report through join's Err
            }
        }
        gate.stalled.map(|(since, _)| since + journal.stall())
    }

    /// How many bytes of the PTY the graphics side has taken in so far, if
    /// there is one: what a carried prefix is measured by.
    fn raw_taken(&self) -> Option<u64> {
        self.graphics
            .as_deref()
            .map(|graphics| graphics.hub().raw_taken())
    }

    /// Whether the read side holds stream the buffer had no room for, which
    /// the turn must read before it ends. It is asked of the loop's graphics
    /// sink, which a session always attaches; a loop without one ends its turn
    /// with whatever the read side holds.
    fn backlog(&self) -> bool {
        self.graphics
            .as_deref()
            .is_some_and(|graphics| graphics.hub().backlog())
    }

    /// The carried prefix's serial reads ([`EventLoop::read_first`]): `left`
    /// counts down as bytes go through and ends at zero when a round reads
    /// nothing or fails. A closed read gate stops the rounds with `left`
    /// kept; the loop calls this again once the gate opens — the prefix does
    /// not make the fd readable, so a quiet PTY would otherwise keep the
    /// rest until its next output.
    fn read_prefix(&mut self, state: &mut State, buf: &mut [u8], left: &mut usize) {
        while *left > 0 {
            if !self.gate_open() {
                return;
            }
            match self.read_round(state, buf, true) {
                // The prefix is counted in the PTY's bytes, which are not the
                // bytes the parser got: a command in it grows, and a part of
                // one delivers nothing at all.
                Ok(round) if round.progressed() => *left = left.saturating_sub(round.taken),
                Ok(_) => *left = 0,
                Err(err) => {
                    eprintln!("bateri: the first PTY read failed: {err}");
                    *left = 0;
                }
            }
        }
    }

    /// One [`EventLoop::pty_read`] and what it came to.
    fn read_round(&mut self, state: &mut State, buf: &mut [u8], serial: bool) -> io::Result<Round> {
        let before = self.raw_taken();
        let processed = self.pty_read(state, buf, serial)?;
        let taken = match (before, self.raw_taken()) {
            (Some(before), Some(now)) => {
                usize::try_from(now.saturating_sub(before)).unwrap_or(usize::MAX)
            }
            _ => processed,
        };
        Ok(Round { processed, taken })
    }

    /// Reads the PTY: rounds of `pty_read`, for as long as one delivers
    /// nothing and yet took bytes in. That is a read of part of a command still
    /// arriving, which is not a PTY with nothing to say — and a poller that
    /// does not wake for what a round left (a quiet PTY after its last output,
    /// a carried prefix) would leave it unread. Bounded in bytes taken
    /// ([`PARTIAL_TURN`]), so that a command that never ends cannot keep the
    /// loop from its messages; the poller wakes for the rest of a PTY that has
    /// more. Returns the bytes parsed.
    fn read_rounds(
        &mut self,
        state: &mut State,
        buf: &mut [u8],
        serial: bool,
    ) -> io::Result<usize> {
        let (mut processed, mut taken) = (0, 0);
        loop {
            let round = self.read_round(state, buf, serial)?;
            processed += round.processed;
            taken += round.taken;
            if round.processed > 0 || round.taken == 0 || taken >= PARTIAL_TURN {
                return Ok(processed);
            }
        }
    }

    /// What is left of the PTY after the child exited: every round until one
    /// makes no progress, or [`PARTIAL_TURN`] bytes have been taken (a writer
    /// that outlives the child, holding the other end, never runs dry). (The
    /// bytes of a big image the child wrote last, in parts, are not one read.)
    fn drain_remaining(&mut self, state: &mut State, buf: &mut [u8]) {
        let mut budget = PARTIAL_TURN;
        while budget > 0 {
            match self.read_round(state, buf, false) {
                Ok(round) if round.progressed() => {
                    budget = budget.saturating_sub(round.taken.max(1));
                }
                _ => return,
            }
        }
    }

    /// Reads once before the first `poll.wait`: an adopted PTY's
    /// carried prefix (`TappedPty`) must reach **this** loop's parser — it
    /// may end inside a sequence — and a quiet PTY gives no readable event
    /// to carry it. The reads stop once `prefix` bytes went through (a
    /// round may take some of the fd's too) — **not** when a round comes
    /// back short: a program writing without pause (`yes`, a build held at
    /// the holder's limit) fills every round, and the loop would never
    /// reach the channel (keys, resizes, the shutdown).
    ///
    /// These reads wait for the terminal lock rather than reading on while
    /// it is busy: each read is parsed before the next one, so no `advance`
    /// mixes the prefix's last bytes with the master's first — what lets the
    /// reader tell the prefix's replies from the live ones (`TappedPty`).
    pub(crate) fn read_first(&mut self, prefix: usize) {
        self.read_first = Some(prefix);
    }

    /// The PTY the loop owned — after `join`, when nothing reads it.
    pub(crate) fn pty_mut(&mut self) -> &mut T {
        &mut self.pty
    }

    /// Applies a pending DEC 2026 buffer to `Term` through the wrapper —
    /// the timeout arm's body, without the `Wakeup`. Meant for the loop
    /// handed back after `join` (the handover's freeze): the snapshot
    /// must see what the application already sent.
    pub(crate) fn stop_sync(&mut self, state: &mut State) {
        state.parser.stop_sync(
            &mut ClusterHandler::new(
                &mut *self.terminal.lock(),
                self.cluster,
                &mut state.last_input,
            )
            .graphics(self.graphics.as_deref()),
        );
    }

    /// The input bytes the loop never wrote, in order: the buffer being
    /// written, the queue behind it and the `Input` messages still in the
    /// channel (the `Shutdown` arm stops draining at itself). Meant for the
    /// loop handed back after `join`; a resize left in the channel is
    /// dropped — the adopting side sizes the PTY itself.
    pub(crate) fn unsent(&mut self, state: &mut State) -> Vec<u8> {
        let mut bytes = Vec::new();
        if let Some(writing) = state.writing.take() {
            bytes.extend_from_slice(writing.remaining_bytes());
        }
        for queued in state.write_list.drain(..) {
            bytes.extend_from_slice(&queued);
        }
        while let Some(msg) = self.rx.recv() {
            if let Msg::Input(input) = msg {
                bytes.extend_from_slice(&input);
            }
        }
        bytes
    }

    pub(crate) fn channel(&self) -> EventLoopSender {
        EventLoopSender {
            sender: self.tx.clone(),
            poller: self.poll.clone(),
        }
    }

    /// Drains the channel; `false` if `Shutdown` arrived.
    fn drain_recv_channel(&mut self, state: &mut State) -> bool {
        while let Some(msg) = self.rx.recv() {
            match msg {
                Msg::Input(input) => state.write_list.push_back(input),
                Msg::Resize(window_size) => self.pty.on_resize(window_size),
                // Only a wake: the gate is asked at the top of the round.
                Msg::Room => {}
                Msg::Shutdown => return false,
            }
        }

        true
    }

    /// `serial`: wait for the terminal lock after a read instead of reading
    /// on into the same buffer ([`EventLoop::read_first`]'s rounds).
    #[inline]
    fn pty_read(&mut self, state: &mut State, buf: &mut [u8], serial: bool) -> io::Result<usize> {
        let mut unprocessed = 0;
        let mut processed = 0;

        // Reserve the next terminal lock for the PTY read. The lease is held
        // for the whole read and `TappedPty::read` runs under it —
        // `session.rs`'s `term` → `shell` lock order depends on this. The one
        // time it is let go is to decode an image (below).
        let mut lease = Some(self.terminal.lease());
        let mut terminal = None;

        loop {
            // Read from the PTY.
            match self.pty.reader().read(&mut buf[unprocessed..]) {
                // The answer on macOS when nothing is left to read on the PTY.
                Ok(0) if unprocessed == 0 => break,
                Ok(got) => unprocessed += got,
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                        // If parsing has caught up and the PTY would block, go
                        // back to the poller.
                        if unprocessed == 0 {
                            break;
                        }
                    }
                    _ => return Err(err),
                },
            }

            // An image the read carried is decoded here, **between the read
            // and its parse**, and with the terminal neither locked nor
            // leased: a frame waits for the lease as well as the lock, so
            // inflating or decoding a large image under either would hold
            // every frame back for as long as it takes. The bytes of this
            // round wait in `buf`, as they do when the lock is busy; the
            // markers in them are not dispatched until the records behind
            // them are decoded, which is also why the order is kept.
            if let Some(graphics) = self.graphics.as_deref()
                && graphics.hub().has_undecoded()
            {
                terminal = None;
                drop(lease.take());
                graphics.hub().decode_pending();
                lease = Some(self.terminal.lease());
            }

            // Try to lock the terminal.
            let terminal = match &mut terminal {
                Some(terminal) => terminal,
                None => terminal.insert(match self.terminal.try_lock_unfair() {
                    // With less room than a marker takes, take the lock by
                    // waiting — a read of that size is a piece of a command,
                    // which delivers nothing, so the buffer never fills — and
                    // in the serial rounds always, so a read is parsed alone.
                    // (With a little more room than that and the lock busy,
                    // a piece of an image is still read in pieces of the
                    // room's size: about 100 reads for 4 KB in a room of 40
                    // bytes, where it was about 2000.)
                    None if serial || buf.len().saturating_sub(unprocessed) < MARKER_LEN => {
                        self.terminal.lock_unfair()
                    }
                    None => continue,
                    Some(terminal) => terminal,
                }),
            };

            // Parse the incoming bytes — into `Term` through the wrapper.
            state.parser.advance(
                &mut ClusterHandler::new(&mut **terminal, self.cluster, &mut state.last_input)
                    .graphics(self.graphics.as_deref()),
                &buf[..unprocessed],
            );
            // The journal's applied count, under the terminal lock and by the
            // slice `advance` got — not the bytes read: the reads before the
            // lock accumulate into one `advance`.
            if let Some(journal) = &self.journal {
                journal.applied(unprocessed);
            }

            processed += unprocessed;
            unprocessed = 0;

            // Do not hold the terminal locked longer than needed — but not
            // with stream left that the buffer had no room for, which nothing
            // would read after this turn ([`Intake::read`]).
            if processed >= MAX_LOCKED_READ && !self.backlog() {
                break;
            }
        }

        // Ask for a redraw unless all processed bytes were synchronized.
        if state.parser.sync_bytes_count() < processed && processed > 0 {
            self.event_proxy.send_event(Event::Wakeup);
        }

        Ok(processed)
    }

    #[inline]
    fn pty_write(&mut self, state: &mut State) -> io::Result<()> {
        state.ensure_next();

        'write_many: while let Some(mut current) = state.take_current() {
            'write_one: loop {
                match self.pty.writer().write(current.remaining_bytes()) {
                    Ok(0) => {
                        state.set_current(Some(current));
                        break 'write_many;
                    }
                    Ok(n) => {
                        current.advance(n);
                        if current.finished() {
                            state.goto_next();
                            break 'write_one;
                        }
                    }
                    Err(err) => {
                        state.set_current(Some(current));
                        match err.kind() {
                            ErrorKind::Interrupted | ErrorKind::WouldBlock => break 'write_many,
                            _ => return Err(err),
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn spawn(mut self) -> JoinHandle<(Self, State)> {
        thread::spawn_named("PTY reader", move || {
            let mut state = State::default();
            let mut buf = [0u8; READ_BUFFER_SIZE];

            let poll_opts = PollMode::Level;
            let mut interest = PollingEvent::readable(0);

            // Register the TTY through the `EventedReadWrite` interface.
            //
            // SAFETY: the registration's condition is that the sources
            // outlive it; `self.pty` owns the fds and the registration is
            // removed by the `deregister` below, before `self` leaves this
            // thread (alacritty's same call).
            if let Err(err) = unsafe { self.pty.register(&self.poll, interest, poll_opts) } {
                eprintln!("bateri: reader loop registration failed: {err}");
                return (self, state);
            }

            // One `pty_read` stops at `MAX_LOCKED_READ`; a long prefix on a
            // quiet PTY would wait for the next output, so read rounds until
            // the prefix went through — bounded by its length, so a program
            // writing without pause cannot keep the loop from the channel.
            let mut prefix_left = self.read_first.map_or(0, |prefix| prefix.max(1));
            self.read_prefix(&mut state, &mut buf, &mut prefix_left);

            let mut events = Events::with_capacity(EVENTS_CAPACITY);
            let mut gate = Gate::default();

            'event_loop: loop {
                // The journal's read gate (nothing without a journal), and the
                // prefix a closed gate stopped.
                let stall = self.gate(&mut gate, &mut interest, poll_opts);
                if prefix_left > 0 && interest.readable {
                    self.read_prefix(&mut state, &mut buf, &mut prefix_left);
                }

                // Wake at the deadline of a synchronized update (DEC 2026) —
                // or at the read gate's stall deadline, if sooner.
                let handler = state.parser.sync_timeout();
                let sync = handler.sync_timeout();
                let deadline = match (sync, stall) {
                    (Some(sync), Some(stall)) => Some(sync.min(stall)),
                    (sync, stall) => sync.or(stall),
                };
                let timeout = deadline.map(|st| st.saturating_duration_since(Instant::now()));

                events.clear();
                if let Err(err) = self.poll.wait(&mut events, timeout) {
                    match err.kind() {
                        ErrorKind::Interrupted => continue,
                        _ => {
                            eprintln!("bateri: reader loop poll failed: {err}");
                            break 'event_loop;
                        }
                    }
                }

                // Timeout of a synchronized update: the buffered bytes go to
                // the **wrapper**, by the same path `advance` sees — handed
                // to `Term` directly, clustering would be skipped in
                // this arm.
                if events.is_empty() && self.rx.peek().is_none() {
                    if sync_due(sync, self.journal.is_some(), Instant::now()) {
                        let mut terminal = self.terminal.lock();
                        state.parser.stop_sync(
                            &mut ClusterHandler::new(
                                &mut terminal,
                                self.cluster,
                                &mut state.last_input,
                            )
                            .graphics(self.graphics.as_deref()),
                        );
                        // In the same lock round: the replay applies the
                        // buffer after exactly the bytes applied so far.
                        if let Some(journal) = &self.journal {
                            journal.record(Side::StopSync);
                        }
                        drop(terminal);
                        self.event_proxy.send_event(Event::Wakeup);
                        // A compaction that found the update open gave up
                        // until a new side record — this one: ask again, or
                        // a closed gate would wait for the stall.
                        if let Some(journal) = &self.journal {
                            Journal::poke(journal);
                        }
                    }
                    continue;
                }

                // Process any messages in the channel.
                if !self.drain_recv_channel(&mut state) {
                    break;
                }

                for event in events.iter() {
                    match event.key {
                        PTY_CHILD_EVENT_TOKEN => {
                            if let Some(tty::ChildEvent::Exited(status)) =
                                self.pty.next_child_event()
                            {
                                if let Some(status) = status {
                                    self.event_proxy.send_event(Event::ChildExit(status));
                                }
                                if self.drain_on_exit {
                                    self.drain_remaining(&mut state, &mut buf);
                                }
                                self.terminal.lock().exit();
                                self.event_proxy.send_event(Event::Wakeup);
                                break 'event_loop;
                            }
                        }

                        PTY_READ_WRITE_TOKEN => {
                            if event.is_interrupt() {
                                // Do not attempt I/O on a dead PTY.
                                continue;
                            }

                            let read = if event.readable {
                                let read = self.read_rounds(&mut state, &mut buf, false);
                                // Outside the lease: the compaction may start a thread.
                                if let Some(journal) = &self.journal {
                                    Journal::poke(journal);
                                }
                                read
                            } else {
                                Ok(0)
                            };
                            if let Err(err) = read {
                                // On Linux, when the client end closes, the
                                // master's `read` may return `EIO`; go back to
                                // the loop for the inevitable `Exited` event.
                                // `libc` is not a dependency of this crate: 5
                                // is Linux's `EIO`.
                                #[cfg(target_os = "linux")]
                                if err.raw_os_error() == Some(5) {
                                    continue;
                                }

                                eprintln!("bateri: PTY read failed: {err}");
                                break 'event_loop;
                            }

                            if event.writable
                                && let Err(err) = self.pty_write(&mut state)
                            {
                                eprintln!("bateri: PTY write failed: {err}");
                                break 'event_loop;
                            }
                        }
                        _ => (),
                    }
                }

                // Register write interest if needed.
                let needs_write = state.needs_write();
                if needs_write != interest.writable {
                    interest.writable = needs_write;

                    // Re-register with the new interest. The panic is
                    // **deliberate** and the same as alacritty's:
                    // `Session::begin_shutdown` recognizes the reader's crash
                    // from `join`'s `Err` (`teardown=`); a silent `break`
                    // would leave the child alive and the window frozen, and
                    // report the shutdown as `clean`.
                    if let Err(err) = self.pty.reregister(&self.poll, interest, poll_opts) {
                        panic!("reader loop re-registration failed: {err}"); // audit: alacritty parity, the crash reaches the shutdown report through join's Err
                    }
                }
            }

            // The event sources are not dropped here, the registration is
            // removed explicitly.
            let _ = self.pty.deregister(&self.poll);

            (self, state)
        })
    }
}

/// The most events the poller returns in one round (alacritty's number).
const EVENTS_CAPACITY: NonZeroUsize = match NonZeroUsize::new(1024) {
    Some(capacity) => capacity,
    None => panic!("event capacity cannot be zero"), // audit: const evaluation, zero is a compile error
};

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;

    use super::*;
    use crate::graphics::{Env, Hub, Intake, Motion};

    /// The graphics side of a session, without a session.
    struct TestSink {
        hub: Hub,
        pixel_mouse: AtomicBool,
    }

    impl Sink for TestSink {
        fn hub(&self) -> &Hub {
            &self.hub
        }

        fn cell(&self) -> (u16, u16) {
            (10, 20)
        }

        fn reply(&self, _: Vec<u8>) {}

        fn pixel_mouse(&self) -> &AtomicBool {
            &self.pixel_mouse
        }
    }

    /// What the scripted PTY gives, a step at a time.
    enum Step {
        Text(&'static str),
        /// A graphics command taken out already: its record is queued,
        /// undecoded, and the marker is what the PTY gives.
        Image,
        /// Bytes as the PTY would give them, graphics commands in them
        /// included: they go through the read side's own separation
        /// ([`Intake`]), which is what `TappedPty` runs.
        Wire(Vec<u8>),
    }

    struct ScriptedPty {
        steps: VecDeque<Step>,
        /// The bytes of the step being given that the PTY still has.
        pending: Vec<u8>,
        /// The most bytes a read of it gives (a pipe gives what it has).
        chunk: usize,
        sink: Arc<TestSink>,
        void: io::Sink,
        intake: Intake,
        /// How much of the buffer each read of the script was offered.
        offered: Vec<usize>,
    }

    impl ScriptedPty {
        fn new(steps: impl IntoIterator<Item = Step>, sink: &Arc<TestSink>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                pending: Vec::new(),
                chunk: usize::MAX,
                sink: Arc::clone(sink),
                void: io::sink(),
                intake: Intake::new(),
                offered: Vec::new(),
            }
        }

        fn chunked(mut self, chunk: usize) -> Self {
            self.chunk = chunk;
            self
        }
    }

    impl Read for ScriptedPty {
        /// What `TappedPty::read_stream` does over a script, by the same
        /// [`Intake::read`]: the stream the bytes of the next step come to
        /// back in the buffer, `WouldBlock` for a read that only feeds a
        /// command still arriving.
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let (steps, pending, chunk, sink, offered) = (
                &mut self.steps,
                &mut self.pending,
                self.chunk,
                &self.sink,
                &mut self.offered,
            );
            self.intake.read(
                buf,
                Instant::now(),
                |room| {
                    offered.push(room.len());
                    if pending.is_empty() {
                        *pending = match steps.pop_front() {
                            Some(Step::Text(text)) => text.as_bytes().to_vec(),
                            Some(Step::Wire(bytes)) => bytes,
                            // A command taken out already: its record is
                            // queued, the marker is what the PTY gives.
                            Some(Step::Image) => {
                                sink.hub.queue_raw(1, "a=t,f=32,s=1,v=1,i=1", b"AAAAAA==");
                                let mut marker = Vec::new();
                                Motion::NONE.mark(4, 1, &mut marker);
                                marker
                            }
                            None => return Err(ErrorKind::WouldBlock.into()),
                        };
                    }
                    let take = pending.len().min(room.len()).min(chunk);
                    room[..take].copy_from_slice(&pending[..take]);
                    pending.drain(..take);
                    Ok(take)
                },
                &sink.hub,
                || Env {
                    cell: (10, 20),
                    columns: 20,
                    lines: 4,
                },
                &mut |_| {},
            )
        }
    }

    impl tty::EventedReadWrite for ScriptedPty {
        type Reader = Self;
        type Writer = io::Sink;

        unsafe fn register(
            &mut self,
            _: &Arc<Poller>,
            _: PollingEvent,
            _: PollMode,
        ) -> io::Result<()> {
            Ok(())
        }

        fn reregister(&mut self, _: &Arc<Poller>, _: PollingEvent, _: PollMode) -> io::Result<()> {
            Ok(())
        }

        fn deregister(&mut self, _: &Arc<Poller>) -> io::Result<()> {
            Ok(())
        }

        fn reader(&mut self) -> &mut Self::Reader {
            self
        }

        fn writer(&mut self) -> &mut Self::Writer {
            &mut self.void
        }
    }

    impl tty::EventedPty for ScriptedPty {
        fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
            None
        }
    }

    impl OnResize for ScriptedPty {
        fn on_resize(&mut self, _: WindowSize) {}
    }

    /// An image carried by a read that comes **after** the loop has taken the
    /// terminal's lock for the one before it: decoding it must not happen
    /// under that lock, nor under the lease a frame also waits for.
    #[test]
    fn an_image_is_decoded_with_neither_the_terminal_lease_nor_its_lock_held() {
        let sink = Arc::new(TestSink {
            hub: Hub::new(),
            pixel_mouse: AtomicBool::new(false),
        });
        let terminal = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TermSize::new(20, 4),
            VoidListener,
        )));
        let pty = ScriptedPty::new(
            [Step::Text("hello"), Step::Image, Step::Text("world")],
            &sink,
        );
        let mut event_loop =
            EventLoop::new(Arc::clone(&terminal), VoidListener, pty, false, false).expect("loop");
        event_loop.graphics(Arc::clone(&sink) as Arc<dyn Sink>);

        // What a frame does: `lock()`, from another thread, while the image
        // is being decoded.
        let (go, go_rx) = mpsc::channel::<()>();
        let (got, got_rx) = mpsc::channel::<()>();
        let frame = {
            let terminal = Arc::clone(&terminal);
            std::thread::spawn(move || {
                if go_rx.recv().is_ok() {
                    drop(terminal.lock());
                    let _ = got.send(());
                }
            })
        };
        let free = Arc::new(AtomicBool::new(false));
        {
            let free = Arc::clone(&free);
            sink.hub.on_decode(Box::new(move || {
                let _ = go.send(());
                free.store(
                    got_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
                    Ordering::Release,
                );
            }));
        }

        let mut state = State::default();
        let mut buf = vec![0; 4096];
        let processed = event_loop
            .pty_read(&mut state, &mut buf, true)
            .expect("read");
        frame.join().expect("the frame thread");

        assert!(
            free.load(Ordering::Acquire),
            "a frame could not take the terminal while an image was decoded"
        );
        let mut marker = Vec::new();
        Motion::NONE.mark(4, 1, &mut marker);
        assert_eq!(processed, 5 + marker.len() + 5);
        // The order is kept: the text on both sides of the marker reached the
        // grid, and the image its marker named is stored.
        let terminal = terminal.lock();
        let row: String = (0..terminal.columns())
            .map(|column| terminal.grid()[Line(0)][Column(column)].c)
            .collect();
        assert!(row.starts_with("helloworld"), "{row:?}");
        drop(terminal);
        assert!(!sink.hub.has_undecoded());
        assert_eq!(sink.hub.with_images(|images| images.main().ids()), [1]);
    }

    /// A file in the temporary directory for a command to name, removed when
    /// the guard goes.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(stem: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("{stem}-{}", std::process::id()));
            std::fs::write(&path, bytes).expect("a file to name");
            Self(path)
        }

        /// The file's name as a command carries it.
        fn name(&self) -> String {
            use std::os::unix::ffi::OsStrExt;
            crate::graphics::tests::base64(self.0.as_os_str().as_bytes())
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A one-cell PNG (10 × 20 pixels) in a file of its own.
    fn one_cell_png(stem: &str) -> TempFile {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 10, 20);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(&[7; 10 * 20 * 4]).expect("data");
        writer.finish().expect("finish");
        TempFile::new(stem, &bytes)
    }

    /// `kitten icat`'s command for a one-cell PNG as the PTY would carry it:
    /// the file, its kind and its size, nothing about the cursor.
    fn icat_command(file: &TempFile) -> Vec<u8> {
        format!("\x1b_Ga=T,t=f,f=100,s=10,v=20,i=1;{}\x1b\\", file.name()).into_bytes()
    }

    /// An image of `side` × `side` pixels sent as the protocol asks for a big
    /// one: in 4096-character commands, `m=1` on all but the last.
    fn chunked_image(side: u32, id: u32) -> Vec<u8> {
        use crate::graphics::tests::{base64, rgba};
        let text = base64(&rgba(side, side, 3));
        let chunks: Vec<&[u8]> = text.as_bytes().chunks(4096).collect();
        let mut wire = Vec::new();
        for (at, chunk) in chunks.iter().enumerate() {
            let more = u8::from(at + 1 < chunks.len());
            let control = if at == 0 {
                format!("a=t,f=32,s={side},v={side},i={id},m={more}")
            } else {
                format!("m={more}")
            };
            wire.extend_from_slice(b"\x1b_G");
            wire.extend_from_slice(control.as_bytes());
            wire.push(b';');
            wire.extend_from_slice(chunk);
            wire.extend_from_slice(b"\x1b\\");
        }
        wire
    }

    fn grid_row(terminal: &Term<VoidListener>, line: i32) -> String {
        (0..terminal.columns())
            .map(|column| terminal.grid()[Line(line)][Column(column)].c)
            .collect()
    }

    fn all_text(terminal: &Term<VoidListener>) -> String {
        (0..4).map(|line| grid_row(terminal, line)).collect()
    }

    fn scripted_loop(
        pty: ScriptedPty,
        sink: &Arc<TestSink>,
        drain_on_exit: bool,
    ) -> (
        EventLoop<ScriptedPty, VoidListener>,
        Arc<FairMutex<Term<VoidListener>>>,
    ) {
        let terminal = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &TermSize::new(20, 4),
            VoidListener,
        )));
        let mut event_loop = EventLoop::new(
            Arc::clone(&terminal),
            VoidListener,
            pty,
            drain_on_exit,
            false,
        )
        .expect("loop");
        event_loop.graphics(Arc::clone(sink) as Arc<dyn Sink>);
        (event_loop, terminal)
    }

    fn quiet_sink() -> Arc<TestSink> {
        Arc::new(TestSink {
            hub: Hub::new(),
            pixel_mouse: AtomicBool::new(false),
        })
    }

    /// The file a command names is read by the decode, with neither the
    /// terminal's lock nor its lease held — and the cursor still lands between
    /// the text on both sides of the command, by the size the command gave.
    /// The read of the command comes after the loop has taken the lock for the
    /// one before it, as a frame would be waiting on it.
    #[test]
    fn a_file_is_read_with_neither_the_lease_nor_the_lock_held() {
        let png = one_cell_png("bt-reader-png");
        let sink = quiet_sink();
        let pty = ScriptedPty::new(
            [
                Step::Text("hello"),
                Step::Wire(icat_command(&png)),
                Step::Text("world"),
            ],
            &sink,
        );
        let (mut event_loop, terminal) = scripted_loop(pty, &sink, false);

        let (go, go_rx) = mpsc::channel::<()>();
        let (got, got_rx) = mpsc::channel::<()>();
        let frame = {
            let terminal = Arc::clone(&terminal);
            std::thread::spawn(move || {
                if go_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                    drop(terminal.lock());
                    let _ = got.send(());
                }
            })
        };
        let free = Arc::new(AtomicBool::new(false));
        {
            let free = Arc::clone(&free);
            // Called just before the file is opened.
            sink.hub.on_decode(Box::new(move || {
                let _ = go.send(());
                free.store(
                    got_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
                    Ordering::Release,
                );
            }));
        }

        let mut state = State::default();
        let mut buf = vec![0; 4096];
        event_loop
            .read_rounds(&mut state, &mut buf, true)
            .expect("read");
        frame.join().expect("the frame thread");

        assert!(
            free.load(Ordering::Acquire),
            "a frame could not take the terminal while a file was read"
        );
        // The image is one cell: the cursor goes one column right of the
        // text before it, and the text after it follows.
        let terminal = terminal.lock();
        let row = grid_row(&terminal, 0);
        assert!(row.starts_with("hello world"), "{row:?}");
        drop(terminal);
        assert!(!sink.hub.has_undecoded());
        assert_eq!(sink.hub.with_images(|images| images.main().ids()), [1]);
    }

    /// 300 of the shortest commands that move the cursor, in a buffer a tenth
    /// of what they become: the PTY is read by what the room left can hold,
    /// so one turn takes them all in, in order, and nothing of what it read
    /// is left anywhere — not in the separation, not in the PTY's script.
    #[test]
    fn a_flood_of_short_commands_is_read_to_its_end_and_nothing_is_left() {
        use crate::graphics::tests::wire;
        let sink = quiet_sink();
        // The size the placements below are made from, from a command that
        // names a file (so the flood itself is nothing but commands).
        let image = TempFile::new("bt-reader-flood", &vec![3; 30 * 40 * 4]);
        let mut wire_bytes =
            format!("\x1b_Ga=t,t=f,f=32,s=30,v=40,i=1;{}\x1b\\", image.name()).into_bytes();
        for _ in 0..300 {
            wire_bytes.extend_from_slice(&wire("a=p,i=1", &[]));
        }
        wire_bytes.extend_from_slice(b"END");
        let pty = ScriptedPty::new([Step::Wire(wire_bytes)], &sink);
        let (mut event_loop, terminal) = scripted_loop(pty, &sink, false);

        let mut state = State::default();
        // A buffer far smaller than what the commands come to: each read of
        // the script fills it, and the rest of the stream is read before the
        // script is. The PTY still has bytes when a turn ends, as it does at
        // the limit of the lock; the poller brings the loop back for them.
        let mut buf = vec![0; 512];
        let mut processed = 0;
        for _ in 0..1000 {
            processed += event_loop
                .read_rounds(&mut state, &mut buf, true)
                .expect("read");
            let pty = event_loop.pty_mut();
            if pty.steps.is_empty() && pty.pending.is_empty() {
                break;
            }
        }

        assert!(
            all_text(&terminal.lock()).contains("END"),
            "the text after the commands did not arrive"
        );
        // 300 markers with their movement are more than the 12 bytes each was.
        assert!(processed > 300 * 12, "{processed} bytes parsed");
        let pty = event_loop.pty_mut();
        assert!(pty.pending.is_empty(), "the PTY has bytes left");
        assert!(pty.intake.undelivered().is_empty());
    }

    /// What the buffer has no room for is read in the turn that read the
    /// rest, also past the limit of the lock: no wake brings the loop back for
    /// stream that is held in the read side and not in the PTY. (The PTY may
    /// well have bytes when a turn ends; the poller brings the loop back.)
    #[test]
    fn a_turn_reads_the_stream_the_buffer_had_no_room_for_before_it_ends() {
        let sink = quiet_sink();
        // 3000 commands of eight bytes become 38 bytes each, more than the
        // lock's limit, from a buffer of 4096.
        let mut wire_bytes = b"\x1b_Ga=d\x1b\\".repeat(3000);
        wire_bytes.extend_from_slice(b"END");
        let pty = ScriptedPty::new([Step::Wire(wire_bytes)], &sink);
        let (mut event_loop, terminal) = scripted_loop(pty, &sink, false);
        let mut state = State::default();
        let mut buf = vec![0; 4096];
        let mut processed = 0;
        for turn in 0..100 {
            processed += event_loop
                .pty_read(&mut state, &mut buf, false)
                .expect("read");
            assert!(
                !sink.hub.backlog(),
                "stream left in the read side, turn {turn}"
            );
            if event_loop.pty_mut().pending.is_empty() {
                break;
            }
        }
        assert_eq!(processed, 3000 * 38 + 3);
        assert!(all_text(&terminal.lock()).contains("END"));
    }

    /// A writer that outlives the child, holding the other end of the PTY,
    /// never lets the drain run dry: it stops at its budget.
    #[test]
    fn the_drain_after_a_childs_exit_stops_at_its_budget() {
        let sink = quiet_sink();
        let steps = (0..40).map(|_| Step::Wire(vec![b'x'; 1 << 20]));
        let pty = ScriptedPty::new(steps, &sink);
        let (mut event_loop, _terminal) = scripted_loop(pty, &sink, true);
        let mut state = State::default();
        let mut buf = vec![0; READ_BUFFER_SIZE];
        event_loop.drain_remaining(&mut state, &mut buf);
        let left = event_loop.pty_mut().steps.len();
        assert!(left > 0, "all of it was read");
        assert!(40 - left >= PARTIAL_TURN / (1 << 20), "{left} steps left");
    }

    /// A buffer that has a few bytes left, a terminal that is locked, and an
    /// image coming in a piece: the bytes of its command are not parsed, so
    /// the buffer would never fill and the loop would read them a few at a
    /// time until the lock is free. It waits for the lock instead, and the
    /// whole stream is parsed once it has it.
    #[test]
    fn a_nearly_full_buffer_waits_for_a_busy_lock_instead_of_reading_by_the_byte() {
        use crate::graphics::tests::{rgba, wire};
        let sink = quiet_sink();
        let image = wire("a=t,f=32,s=32,v=32,i=1", &rgba(32, 32, 0));
        let steps = [
            Step::Wire(vec![b'p'; 61]),
            Step::Wire(image),
            Step::Wire(b"END".to_vec()),
        ];
        let (mut event_loop, terminal) =
            scripted_loop(ScriptedPty::new(steps, &sink), &sink, false);
        // The terminal is locked for a while, as a frame holds it.
        let (locked, hold) = std::sync::mpsc::channel();
        let holder = {
            let terminal = Arc::clone(&terminal);
            std::thread::spawn(move || {
                let guard = terminal.lock();
                locked.send(()).expect("the test is waiting");
                std::thread::sleep(Duration::from_millis(150));
                drop(guard);
            })
        };
        hold.recv().expect("the lock was taken");
        let mut state = State::default();
        let mut buf = vec![0; 64];
        for _ in 0..100 {
            event_loop
                .pty_read(&mut state, &mut buf, false)
                .expect("read");
            let pty = event_loop.pty_mut();
            if pty.steps.is_empty() && pty.pending.is_empty() {
                break;
            }
        }
        holder.join().expect("the holder");
        let reads = event_loop.pty_mut().offered.len();
        assert!(reads < 200, "{reads} reads of the PTY");
        let text = all_text(&terminal.lock());
        assert!(
            text.starts_with("ppppp") && text.contains("END"),
            "{text:?}"
        );
        assert_eq!(sink.hub.with_images(|images| images.main().ids()), [1]);
    }

    /// The promise made for a terminal that has seen no image command: the
    /// read path is what it was before graphics — the parser gets the bytes
    /// the PTY gave, each read is offered the whole buffer, and there is one
    /// read of the PTY for each piece it gave. Every read in this corpus ends
    /// where the next sequence may begin, and the programs' own markers and
    /// long mode lists are in it.
    #[test]
    fn a_stream_with_no_command_is_the_parsers_input_byte_for_byte() {
        let long_list = format!(
            "\x1b[?{}l",
            (1000..1032)
                .map(|mode| mode.to_string())
                .collect::<Vec<_>>()
                .join(";")
        );
        let pieces: Vec<&[u8]> = vec![
            b"plain text\r\nwith a second line\r\n",
            b"\x1b[31mred\x1b[0m \x1b[1;38;2;1;2;3mtrue\x1b[m",
            b"\x1b[?25l",
            b"\x1b[?25h\x1b[?2026h",
            b"\x1b[?2026l\x1b[?2004h\x1b[?1049h\x1b[?1049l",
            long_list.as_bytes(),
            b"\x1b[?8166h\x1b[?16385;16384;16384;16384;16385;8166h",
            b"\x1b[?8166\x07h\x1b[?8166\x7fh\x1b[\x07?8166h\x1b\x07[?8166h",
            b"\x1b",
            b"[?25h",
            b"\x1b[",
            b"?1049l",
            b"text\x1b",
            b"]0;a title\x07",
            b"\x1b",
            b"\x1b",
            b"_Xan APC that is not ours\x1b\\",
            b"\x1b_",
            b"Y\x1b\\ and plain again\r\n",
        ];
        let sink = quiet_sink();
        let steps = pieces.iter().map(|piece| Step::Wire(piece.to_vec()));
        let mut pty = ScriptedPty::new(steps, &sink);
        let mut buf = vec![0u8; 4096];
        let mut parsed = Vec::new();
        for _ in &pieces {
            match pty.read(&mut buf) {
                Ok(read) => parsed.extend_from_slice(&buf[..read]),
                // A piece that is only the beginning of what the next says.
                Err(error) => assert_eq!(error.kind(), ErrorKind::WouldBlock),
            }
        }
        assert_eq!(parsed, pieces.concat());
        assert_eq!(pty.offered, vec![buf.len(); pieces.len()]);
        assert!(pty.steps.is_empty() && pty.pending.is_empty());
    }

    /// An image in parts — 4 KB commands, as the protocol asks — delivers
    /// nothing until its last part, and a read of one part is a read that took
    /// bytes in, not a PTY with nothing to say. Live, the loop's turn goes on
    /// to the text behind it.
    #[test]
    fn a_big_image_in_parts_and_the_text_after_it_are_read_in_one_turn() {
        let sink = quiet_sink();
        let mut wire_bytes = chunked_image(100, 1);
        wire_bytes.extend_from_slice(b"END");
        let pty = ScriptedPty::new([Step::Wire(wire_bytes)], &sink);
        let (mut event_loop, terminal) = scripted_loop(pty, &sink, false);

        let mut state = State::default();
        let mut buf = vec![0; 4096];
        event_loop
            .read_rounds(&mut state, &mut buf, false)
            .expect("read");

        assert!(all_text(&terminal.lock()).contains("END"));
        assert_eq!(sink.hub.with_images(|images| images.main().ids()), [1]);
    }

    /// The same image in a handover's prefix: the rounds that read it stop
    /// when the prefix is through, not at the first round that parsed nothing.
    #[test]
    fn a_big_image_in_parts_in_a_carried_prefix_is_read_to_its_end() {
        let sink = quiet_sink();
        let mut prefix = chunked_image(100, 1);
        prefix.extend_from_slice(b"END");
        let carried = prefix.len();
        // A pipe that gives 4 KB a read: the first round takes part of the
        // image and delivers nothing.
        let pty = ScriptedPty::new([Step::Wire(prefix)], &sink).chunked(4096);
        let (mut event_loop, terminal) = scripted_loop(pty, &sink, false);
        let sender = event_loop.channel();
        event_loop.read_first(carried);
        let thread = event_loop.spawn();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            seen = all_text(&terminal.lock()).contains("END");
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(Msg::Shutdown).expect("the loop is there");
        thread.join().expect("the loop");

        assert!(seen, "the end of the prefix was left unread");
    }

    /// And the output a child wrote last before it exited: drained to its end,
    /// whatever the parts it came in.
    #[test]
    fn a_big_image_in_parts_a_child_wrote_last_is_drained_to_its_end() {
        let sink = quiet_sink();
        let mut wire_bytes = chunked_image(100, 1);
        wire_bytes.extend_from_slice(b"END");
        let pty = ScriptedPty::new([Step::Wire(wire_bytes)], &sink).chunked(4096);
        let (mut event_loop, terminal) = scripted_loop(pty, &sink, true);

        let mut state = State::default();
        let mut buf = vec![0; READ_BUFFER_SIZE];
        event_loop.drain_remaining(&mut state, &mut buf);

        assert!(all_text(&terminal.lock()).contains("END"));
    }

    /// A carried prefix is measured in bytes of the PTY, which are not the
    /// bytes the parser gets: commands in it grow. The loop's first rounds
    /// stop when the prefix is through — counted in what was read — and a
    /// quiet PTY gives nothing to wake the poller for the rest.
    #[test]
    fn a_carried_prefix_is_counted_in_the_bytes_of_the_pty() {
        use crate::graphics::tests::wire;
        let sink = quiet_sink();
        let image = TempFile::new("bt-reader-prefix", &vec![3; 30 * 40 * 4]);
        let mut prefix =
            format!("\x1b_Ga=t,t=f,f=32,s=30,v=40,i=1;{}\x1b\\", image.name()).into_bytes();
        for _ in 0..4000 {
            prefix.extend_from_slice(&wire("a=p,i=1", &[]));
        }
        prefix.extend_from_slice(b"END");
        let carried = prefix.len();
        // A pipe that gives a thousand bytes a read: the rounds stop at the
        // lock's time limit long before the prefix is through, and the stream
        // is by then well past the prefix's length.
        let pty = ScriptedPty::new([Step::Wire(prefix)], &sink).chunked(1000);
        let (mut event_loop, terminal) = scripted_loop(pty, &sink, false);
        let sender = event_loop.channel();
        event_loop.read_first(carried);
        let thread = event_loop.spawn();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            seen = all_text(&terminal.lock()).contains("END");
            std::thread::sleep(Duration::from_millis(5));
        }
        sender.send(Msg::Shutdown).expect("the loop is there");
        thread.join().expect("the loop");

        assert!(seen, "the end of the prefix was left unread");
    }
}
