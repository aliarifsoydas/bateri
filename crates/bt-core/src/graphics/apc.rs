//! Takes `ESC _ G … ESC \` out of a byte stream, wherever the reads cut it.
//!
//! The stream is not parsed here, only searched: every other byte, and every
//! APC that is not a graphics command, passes through as it came. The one
//! thing held back is a possible beginning — a trailing `ESC` or `ESC _` —
//! because the byte after it decides whether the bytes belong to us. Held
//! bytes come out with the next feed, or from [`Splitter::take_held`] when
//! the stream ends.
//!
//! The grammar follows what the parser behind it would do with the same
//! bytes. An `ESC` ends whatever string it is in and starts a new sequence
//! (so an `ESC _ G` inside an OSC title is still ours, as it is the
//! parser's), and `CAN` and `SUB` cancel a string. A command whose `ESC`
//! is followed by anything but `\` is dropped and the `ESC` starts afresh.

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// The longest control data kept: the keys of a command are a few dozen
/// bytes. Past it the command is dropped (no key can be trusted) and the
/// body is skipped to its end.
pub(super) const CONTROL_LIMIT: usize = 1024;

/// One graphics command as it was on the wire.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Apc {
    /// The text between `G` and `;` (or the end).
    pub(crate) control: Vec<u8>,
    /// The base64 text after `;`. Cut at the splitter's limit: see
    /// [`Apc::overflowed`].
    pub(crate) payload: Vec<u8>,
    /// The payload was longer than the limit; `payload` holds only what fit.
    pub(crate) overflowed: bool,
    /// The control data was longer than [`CONTROL_LIMIT`]: nothing in it can
    /// be read.
    pub(crate) garbled: bool,
}

/// A run of the stream, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    /// Bytes for the parser as they came.
    Bytes(&'a [u8]),
    /// A graphics command, taken out.
    Command(Apc),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// Holding `ESC`.
    Esc,
    /// Holding `ESC _`.
    Underscore,
    /// Inside `ESC _ G`, before the `;`.
    Control,
    /// After the `;`.
    Payload,
    /// Saw `ESC` in a body (`payload`: after the `;`); `\` completes the
    /// command.
    Closing {
        payload: bool,
    },
}

pub(crate) struct Splitter {
    state: State,
    current: Apc,
    payload_limit: usize,
}

impl Splitter {
    /// `payload_limit`: the most base64 bytes of one command that are kept.
    pub(crate) fn new(payload_limit: usize) -> Self {
        Self {
            state: State::Ground,
            current: Apc::default(),
            payload_limit,
        }
    }

    /// Whether the stream is inside a graphics command: bytes that are
    /// neither delivered nor dropped yet, which a cut would turn into text.
    pub(crate) fn inside(&self) -> bool {
        matches!(
            self.state,
            State::Control | State::Payload | State::Closing { .. }
        )
    }

    /// Whether `feed` would pass `bytes` through unchanged: nothing held,
    /// not inside a command, and no `ESC _` anywhere (nor an `ESC` at the end,
    /// which may begin one). The plain shell output's path.
    pub(crate) fn passes(&self, bytes: &[u8]) -> bool {
        if self.state != State::Ground {
            return false;
        }
        let mut rest = bytes;
        while let Some(at) = rest.iter().position(|&byte| byte == ESC) {
            match rest.get(at + 1) {
                None | Some(b'_') => return false,
                Some(_) => rest = &rest[at + 1..],
            }
        }
        true
    }

    /// The bytes held back, in order.
    pub(crate) fn held(&self) -> &'static [u8] {
        match self.state {
            State::Esc => &[ESC],
            State::Underscore => &[ESC, b'_'],
            _ => &[],
        }
    }

    /// The bytes of the unfinished command, as the stream carried them: the
    /// held beginning, or the beginning, the control data and the payload so
    /// far. A command cut short by a handover is given to the adopting side
    /// this way, and its reader finishes it from what the master has left.
    /// A command whose control or payload outgrew its limit comes back
    /// truncated; the adopting side then fails it with an answer, not text.
    pub(crate) fn inflight(&self) -> Vec<u8> {
        let with_payload = match self.state {
            State::Ground => return Vec::new(),
            State::Esc | State::Underscore => return self.held().to_vec(),
            State::Control | State::Closing { payload: false } => false,
            State::Payload | State::Closing { payload: true } => true,
        };
        let mut bytes = vec![ESC, b'_', b'G'];
        bytes.extend_from_slice(&self.current.control);
        if with_payload {
            bytes.push(b';');
            bytes.extend_from_slice(&self.current.payload);
        }
        if matches!(self.state, State::Closing { .. }) {
            bytes.push(ESC);
        }
        bytes
    }

    /// Releases the held bytes. Production never does: a read of nothing is
    /// not the end of the stream on macOS, and releasing the `ESC` of a
    /// command still arriving would print it. The handover carries them
    /// instead ([`Splitter::held`]).
    #[cfg(test)]
    pub(crate) fn take_held(&mut self) -> &'static [u8] {
        let held = self.held();
        if !held.is_empty() {
            self.state = State::Ground;
        }
        held
    }

    /// Splits `input`, handing the pieces to `out` in stream order.
    pub(crate) fn feed<'a>(&mut self, input: &'a [u8], mut out: impl FnMut(Piece<'a>)) {
        // Where the run of bytes not yet handed on begins; only meaningful
        // in the ground state.
        let mut run = 0;
        let mut at = 0;
        while at < input.len() {
            match self.state {
                State::Ground => {
                    match input[at..].iter().position(|&byte| byte == ESC) {
                        Some(offset) => {
                            at += offset;
                            if at > run {
                                out(Piece::Bytes(&input[run..at]));
                            }
                            self.state = State::Esc;
                            at += 1;
                        }
                        None => {
                            out(Piece::Bytes(&input[run..]));
                            return;
                        }
                    }
                    run = at;
                }
                State::Esc => match input[at] {
                    b'_' => {
                        self.state = State::Underscore;
                        at += 1;
                    }
                    ESC => {
                        out(Piece::Bytes(&[ESC]));
                        at += 1;
                    }
                    _ => {
                        out(Piece::Bytes(&[ESC]));
                        self.state = State::Ground;
                        run = at;
                    }
                },
                State::Underscore => {
                    if input[at] == b'G' {
                        self.current = Apc::default();
                        self.state = State::Control;
                        at += 1;
                        run = at;
                    } else {
                        out(Piece::Bytes(&[ESC, b'_']));
                        self.state = State::Ground;
                        run = at;
                    }
                }
                State::Control => {
                    let rest = &input[at..];
                    let stop = rest
                        .iter()
                        .position(|&byte| matches!(byte, b';' | ESC | CAN | SUB))
                        .unwrap_or(rest.len());
                    self.keep_control(&rest[..stop]);
                    at += stop;
                    if let Some(&byte) = input.get(at) {
                        at += 1;
                        match byte {
                            b';' => self.state = State::Payload,
                            ESC => self.state = State::Closing { payload: false },
                            _ => self.state = State::Ground,
                        }
                        run = at;
                    }
                }
                State::Payload => {
                    let rest = &input[at..];
                    let stop = rest
                        .iter()
                        .position(|&byte| matches!(byte, ESC | CAN | SUB))
                        .unwrap_or(rest.len());
                    self.keep_payload(&rest[..stop]);
                    at += stop;
                    if let Some(&byte) = input.get(at) {
                        at += 1;
                        self.state = if byte == ESC {
                            State::Closing { payload: true }
                        } else {
                            State::Ground
                        };
                        run = at;
                    }
                }
                State::Closing { .. } => {
                    if input[at] == b'\\' {
                        at += 1;
                        self.state = State::Ground;
                        out(Piece::Command(std::mem::take(&mut self.current)));
                    } else {
                        // Not a terminator: the command is abandoned and
                        // this `ESC` begins a sequence of its own.
                        self.current = Apc::default();
                        self.state = State::Esc;
                    }
                    run = at;
                }
            }
        }
        if self.state == State::Ground && run < input.len() {
            out(Piece::Bytes(&input[run..]));
        }
    }

    fn keep_control(&mut self, bytes: &[u8]) {
        if self.current.garbled {
            return;
        }
        if self.current.control.len() + bytes.len() > CONTROL_LIMIT {
            self.current.control.clear();
            self.current.garbled = true;
        } else {
            self.current.control.extend_from_slice(bytes);
        }
    }

    fn keep_payload(&mut self, bytes: &[u8]) {
        let room = self
            .payload_limit
            .saturating_sub(self.current.payload.len());
        if bytes.len() > room {
            self.current.overflowed = true;
        }
        let kept = bytes.len().min(room);
        self.current.payload.extend_from_slice(&bytes[..kept]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the splitter made of `chunks`, fed one after the other: the
    /// bytes that passed and the commands taken out.
    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Apc>) {
        let mut splitter = Splitter::new(1 << 20);
        let mut bytes = Vec::new();
        let mut commands = Vec::new();
        for chunk in chunks {
            splitter.feed(chunk, |piece| match piece {
                Piece::Bytes(passed) => bytes.extend_from_slice(passed),
                Piece::Command(apc) => commands.push(apc),
            });
        }
        bytes.extend_from_slice(splitter.take_held());
        (bytes, commands)
    }

    fn command(control: &str, payload: &str) -> Apc {
        Apc {
            control: control.as_bytes().to_vec(),
            payload: payload.as_bytes().to_vec(),
            ..Apc::default()
        }
    }

    #[test]
    fn a_stream_without_a_graphics_command_passes_byte_for_byte() {
        for stream in [
            &b"plain text\r\n"[..],
            b"\x1b[31mred\x1b[0m",
            b"\x1b]0;title\x07",
            b"\x1b_Xnot ours\x1b\\after",
            b"\x1b\x1b\x1b[1m",
            b"\x1b_",
            b"\x1b",
            b"",
        ] {
            let (bytes, commands) = run(&[stream]);
            assert_eq!(bytes, stream);
            assert!(commands.is_empty());
        }
    }

    #[test]
    fn a_command_is_taken_out_and_the_text_around_it_stays() {
        let (bytes, commands) = run(&[b"before\x1b_Ga=t,i=1;AAAA\x1b\\after"]);
        assert_eq!(bytes, b"beforeafter");
        assert_eq!(commands, [command("a=t,i=1", "AAAA")]);
    }

    #[test]
    fn a_command_without_a_payload_or_without_controls() {
        let (bytes, commands) = run(&[b"\x1b_Ga=d,d=A\x1b\\\x1b_G;QUJD\x1b\\"]);
        assert!(bytes.is_empty());
        assert_eq!(commands, [command("a=d,d=A", ""), command("", "QUJD")]);
    }

    #[test]
    fn cutting_the_stream_anywhere_gives_the_same_result() {
        let stream = b"ab\x1b[1m\x1b_Ga=T,f=100,i=9;iVBORw0KGgo=\x1b\\cd\x1b_Xskip\x1b\\\
            \x1b_Gm=0;AAAA\x1b\\\x1b";
        let whole = run(&[stream]);
        for first in 0..=stream.len() {
            assert_eq!(run(&[&stream[..first], &stream[first..]]), whole, "{first}");
            for second in first..=stream.len() {
                assert_eq!(
                    run(&[&stream[..first], &stream[first..second], &stream[second..]]),
                    whole,
                    "{first} {second}"
                );
            }
        }
        // And one byte at a time.
        let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(run(&bytewise), whole);
        assert_eq!(whole.1.len(), 2);
    }

    #[test]
    fn a_held_beginning_comes_out_when_the_stream_says_it_was_not_one() {
        let mut splitter = Splitter::new(16);
        let mut bytes = Vec::new();
        splitter.feed(b"x\x1b", |piece| {
            if let Piece::Bytes(passed) = piece {
                bytes.extend_from_slice(passed);
            }
        });
        assert_eq!(bytes, b"x");
        assert_eq!(splitter.held(), b"\x1b");
        splitter.feed(b"[m", |piece| {
            if let Piece::Bytes(passed) = piece {
                bytes.extend_from_slice(passed);
            }
        });
        assert_eq!(bytes, b"x\x1b[m");
        assert!(splitter.held().is_empty());
    }

    #[test]
    fn an_esc_inside_a_body_that_is_not_a_terminator_abandons_the_command() {
        let (bytes, commands) = run(&[b"\x1b_Ga=t;AA\x1b[1mtext"]);
        assert_eq!(bytes, b"\x1b[1mtext");
        assert!(commands.is_empty());
    }

    #[test]
    fn a_new_command_inside_an_unfinished_one_starts_afresh() {
        let (bytes, commands) = run(&[b"\x1b_Ga=t;AA\x1b_Gi=2;BB\x1b\\"]);
        assert!(bytes.is_empty());
        assert_eq!(commands, [command("i=2", "BB")]);
    }

    #[test]
    fn cancel_and_substitute_abandon_a_body() {
        for cancel in [0x18u8, 0x1a] {
            let stream = [&b"\x1b_Ga=t;AA"[..], &[cancel], b"after"].concat();
            let (bytes, commands) = run(&[&stream]);
            assert_eq!(bytes, b"after");
            assert!(commands.is_empty());
        }
    }

    #[test]
    fn a_payload_past_the_limit_is_cut_and_flagged() {
        let mut splitter = Splitter::new(4);
        let mut seen = Vec::new();
        splitter.feed(b"\x1b_Gi=1;AAAABBBB\x1b\\", |piece| {
            if let Piece::Command(apc) = piece {
                seen.push(apc);
            }
        });
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].payload, b"AAAA");
        assert!(seen[0].overflowed && !seen[0].garbled);
    }

    #[test]
    fn controls_past_the_limit_are_garbled_not_kept() {
        let mut stream = b"\x1b_G".to_vec();
        stream.extend(std::iter::repeat_n(b'k', CONTROL_LIMIT + 1));
        stream.extend_from_slice(b";AAAA\x1b\\");
        let (_, commands) = run(&[&stream]);
        assert_eq!(commands.len(), 1);
        assert!(commands[0].garbled && commands[0].control.is_empty());
    }

    #[test]
    fn the_fast_path_is_only_for_streams_that_cannot_hold_anything() {
        let splitter = Splitter::new(16);
        assert!(splitter.passes(b"ls -la\r\n\x1b[0m"));
        assert!(!splitter.passes(b"x\x1b"));
        assert!(!splitter.passes(b"x\x1b_Gy"));
        let mut busy = Splitter::new(16);
        busy.feed(b"\x1b_Ga=t", |_| {});
        assert!(busy.inside());
        assert!(!busy.passes(b"plain"));
    }

    #[test]
    fn the_unfinished_command_can_be_given_back_as_the_stream_carried_it() {
        let stream: &[u8] = b"x\x1b_Ga=t,i=1;AAAA\x1b\\y\x1b_Gm=1;BB\x1b\\\x1b_Gq=2;CCCC\x1b\\";
        for cut in 0..=stream.len() {
            let mut splitter = Splitter::new(1 << 20);
            let mut delivered = Vec::new();
            let mut commands = 0;
            splitter.feed(&stream[..cut], |piece| match piece {
                Piece::Bytes(passed) => delivered.extend_from_slice(passed),
                Piece::Command(_) => commands += 1,
            });
            // What the stream said so far is what was passed on, what was
            // taken out as commands and what is given back.
            let back = splitter.inflight();
            let mut again = Splitter::new(1 << 20);
            let (mut bytes, mut seen) = (Vec::new(), 0);
            again.feed(&[&back[..], &stream[cut..]].concat(), |piece| match piece {
                Piece::Bytes(passed) => bytes.extend_from_slice(passed),
                Piece::Command(_) => seen += 1,
            });
            bytes.extend_from_slice(again.take_held());
            delivered.extend_from_slice(&bytes);
            assert_eq!(delivered, b"xy", "{cut}");
            assert_eq!(commands + seen, 3, "{cut}");
        }
    }

    #[test]
    fn inside_means_a_body_is_open() {
        let mut splitter = Splitter::new(16);
        assert!(!splitter.inside());
        splitter.feed(b"\x1b_G", |_| {});
        assert!(splitter.inside());
        splitter.feed(b"i=1;AA\x1b", |_| {});
        assert!(splitter.inside());
        splitter.feed(b"\\", |_| {});
        assert!(!splitter.inside());
        splitter.feed(b"\x1b_", |_| {});
        assert!(!splitter.inside());
    }
}
