//! Puts one graphics command together from the pieces the parser hands over.
//!
//! The parser finds `ESC _ … ESC \` and gives its body in slices, cut wherever
//! the reads cut it ([`Collector::start`], [`Collector::put`],
//! [`Collector::end`]). This is the one place that reads the body: the first
//! byte must be `G` (any other application program command — tmux sends
//! them — is not ours and is let go), the control data runs to the first `;`
//! and the payload after it. Nothing is interpreted here.
//!
//! Both parts are bounded. Control data past [`CONTROL_LIMIT`] is dropped
//! whole (no key in it can be trusted) and the command is flagged; a payload
//! past the limit the collector was given is cut and flagged, so that the
//! command is refused with an answer and not kept in memory.

/// The longest control data kept: the keys of a command are a few dozen
/// bytes. Past it the command is dropped (no key can be trusted) and the
/// body is skipped to its end.
pub(super) const CONTROL_LIMIT: usize = 1024;

/// One graphics command as it was on the wire.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Apc {
    /// The text between `G` and `;` (or the end).
    pub(crate) control: Vec<u8>,
    /// The base64 text after `;`. Cut at the collector's limit: see
    /// [`Apc::overflowed`].
    pub(crate) payload: Vec<u8>,
    /// The payload was longer than the limit; `payload` holds only what fit.
    pub(crate) overflowed: bool,
    /// The control data was longer than [`CONTROL_LIMIT`]: nothing in it can
    /// be read.
    pub(crate) garbled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Outside any application program command.
    Idle,
    /// Inside one, the first byte not seen yet.
    Head,
    /// Inside one that is not ours.
    Foreign,
    /// Inside `ESC _ G`, before the `;`.
    Control,
    /// After the `;`.
    Payload,
}

/// What is known of the command being received, between the parser's calls.
pub(crate) struct Collector {
    phase: Phase,
    current: Apc,
    payload_limit: usize,
}

impl Collector {
    /// `payload_limit`: the most base64 bytes of one command that are kept.
    pub(crate) fn new(payload_limit: usize) -> Self {
        Self {
            phase: Phase::Idle,
            current: Apc::default(),
            payload_limit,
        }
    }

    /// `ESC _`: a command begins. One already open was cancelled by the
    /// parser (it says so by [`Collector::end`] first).
    pub(crate) fn start(&mut self) {
        self.phase = Phase::Head;
        self.current = Apc::default();
    }

    /// A piece of the body, as the parser has it.
    pub(crate) fn put(&mut self, mut data: &[u8]) {
        if self.phase == Phase::Head {
            let Some((&first, rest)) = data.split_first() else {
                return;
            };
            self.phase = if first == b'G' {
                Phase::Control
            } else {
                Phase::Foreign
            };
            data = rest;
        }
        match self.phase {
            Phase::Control => match data.iter().position(|&byte| byte == b';') {
                Some(at) => {
                    self.keep_control(&data[..at]);
                    self.phase = Phase::Payload;
                    self.keep_payload(&data[at + 1..]);
                }
                None => self.keep_control(data),
            },
            Phase::Payload => self.keep_payload(data),
            Phase::Idle | Phase::Head | Phase::Foreign => {}
        }
    }

    /// The command ended: `cancelled` unless `ESC \` completed it. What the
    /// body came to if it is a whole graphics command.
    pub(crate) fn end(&mut self, cancelled: bool) -> Option<Apc> {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let apc = std::mem::take(&mut self.current);
        (!cancelled && matches!(phase, Phase::Control | Phase::Payload)).then_some(apc)
    }

    /// Whether a graphics command is open: bytes the parser took that no
    /// command comes of yet.
    pub(crate) fn inside(&self) -> bool {
        matches!(self.phase, Phase::Control | Phase::Payload)
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

    /// The commands a body made, fed in the pieces `cuts` marks, as the
    /// parser would call: `start`, the pieces, `end(false)`.
    fn run(bodies: &[&[u8]], cuts: &[usize], limit: usize) -> Vec<Apc> {
        let mut collector = Collector::new(limit);
        let mut found = Vec::new();
        for body in bodies {
            collector.start();
            let mut from = 0;
            for &cut in cuts.iter().chain([&body.len()]) {
                let cut = cut.min(body.len());
                if cut > from {
                    collector.put(&body[from..cut]);
                    from = cut;
                }
            }
            found.extend(collector.end(false));
        }
        found
    }

    fn command(control: &str, payload: &str) -> Apc {
        Apc {
            control: control.as_bytes().to_vec(),
            payload: payload.as_bytes().to_vec(),
            ..Apc::default()
        }
    }

    #[test]
    fn a_body_is_split_at_its_first_semicolon() {
        let found = run(&[b"Ga=t,i=1;AAAA;BB", b"G;QUJD", b"Ga=d,d=A"], &[], 1 << 20);
        assert_eq!(
            found,
            [
                command("a=t,i=1", "AAAA;BB"),
                command("", "QUJD"),
                command("a=d,d=A", "")
            ]
        );
    }

    #[test]
    fn a_body_that_is_not_ours_is_let_go() {
        assert!(run(&[b"Xnot ours", b"", b"tmux;passthrough"], &[], 16).is_empty());
    }

    #[test]
    fn cutting_the_body_anywhere_gives_the_same_command() {
        let body: &[u8] = b"Ga=T,f=100,i=9;iVBORw0KGgo=";
        let whole = run(&[body], &[], 1 << 20);
        assert_eq!(whole.len(), 1);
        for first in 0..=body.len() {
            assert_eq!(run(&[body], &[first], 1 << 20), whole, "{first}");
            for second in first..=body.len() {
                assert_eq!(run(&[body], &[first, second], 1 << 20), whole);
            }
        }
    }

    #[test]
    fn a_cancelled_body_is_nothing() {
        let mut collector = Collector::new(16);
        collector.start();
        collector.put(b"Ga=t;AA");
        assert!(collector.inside());
        assert_eq!(collector.end(true), None);
        assert!(!collector.inside());
        collector.start();
        collector.put(b"Gi=2;BB");
        assert_eq!(collector.end(false), Some(command("i=2", "BB")));
    }

    #[test]
    fn a_payload_past_the_limit_is_cut_and_flagged() {
        let found = run(&[b"Gi=1;AAAABBBB"], &[], 4);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].payload, b"AAAA");
        assert!(found[0].overflowed && !found[0].garbled);
        // Cut in pieces it is the same.
        assert_eq!(run(&[b"Gi=1;AAAABBBB"], &[5, 9], 4), found);
    }

    #[test]
    fn controls_past_the_limit_are_garbled_not_kept() {
        let mut body = b"G".to_vec();
        body.extend(std::iter::repeat_n(b'k', CONTROL_LIMIT + 1));
        body.extend_from_slice(b";AAAA");
        let found = run(&[&body], &[100], 1 << 20);
        assert_eq!(found.len(), 1);
        assert!(found[0].garbled && found[0].control.is_empty());
        assert_eq!(found[0].payload, b"AAAA");
    }

    #[test]
    fn inside_means_a_graphics_body_is_open() {
        let mut collector = Collector::new(16);
        assert!(!collector.inside());
        collector.start();
        assert!(!collector.inside(), "nothing yet says whose it is");
        collector.put(b"G");
        assert!(collector.inside());
        collector.put(b"i=1;AA");
        assert!(collector.inside());
        assert!(collector.end(false).is_some());
        assert!(!collector.inside());
        collector.start();
        collector.put(b"Y");
        assert!(!collector.inside());
    }
}
