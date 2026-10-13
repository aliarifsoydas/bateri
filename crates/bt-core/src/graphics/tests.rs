use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::apc::Apc;
use super::command::Command;
use super::store::Images;
use super::*;

/// What the reader knows of an 80 × 24 terminal with 10 × 20 pixel cells.
const ENV: Env = Env {
    cell: (10, 20),
    columns: 80,
    lines: 24,
};

const ESC_G: &[u8] = b"\x1b_G";
const ST: &[u8] = b"\x1b\\";

/// The marker of a command whose cursor moves `rows` rows down and `cols`
/// columns right at the cell size and width of [`ENV`].
fn marker(rows: u32, cols: u32) -> Vec<u8> {
    let mut out = Vec::new();
    Motion {
        rows,
        cols,
        wrap: false,
    }
    .mark(ENV.lines, 0, &mut out);
    out
}

/// The marker of an image as wide as the screen: down `rows`, then to the
/// start of the next line.
fn wrapped(rows: u32) -> Vec<u8> {
    let mut out = Vec::new();
    Motion {
        rows,
        cols: 0,
        wrap: true,
    }
    .mark(ENV.lines, 0, &mut out);
    out
}

/// The marker of a command that leaves the cursor, with no id (the real ones
/// are random; see [`anonymous`]).
fn mark() -> Vec<u8> {
    marker(0, 0)
}

/// The markers a stream carries: where each begins, and its five arguments
/// (the cursor's movement and the id, less [`ARG_BASE`]).
fn marks(stream: &[u8]) -> Vec<(usize, [u16; 5])> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = stream[at..].windows(3).position(|w| w == b"\x1b[?") {
        let start = at + offset;
        let end = stream[start..]
            .iter()
            .position(|&byte| byte == b'h')
            .map_or(stream.len(), |end| start + end);
        let modes: Vec<u16> = String::from_utf8_lossy(&stream[start + 3..end])
            .split(';')
            .filter_map(|mode| mode.parse().ok())
            .collect();
        if let [a, b, c, d, e, last] = modes[..]
            && last == MARK_MODE
        {
            let args = [a, b, c, d, e].map(|arg| arg - ARG_BASE);
            found.push((start, args));
        }
        at = end.max(start + 1);
    }
    found
}

/// How many markers a stream carries.
fn markers(stream: &[u8]) -> usize {
    marks(stream).len()
}

/// The ids of the markers a stream carries, in order.
fn ids(stream: &[u8]) -> Vec<u32> {
    marks(stream)
        .into_iter()
        .map(|(_, args)| Motion::from_marker(args).1)
        .collect()
}

/// The stream with the id of each marker set to 0: the ids are random, the
/// rest of a marker is what the command said.
fn anonymous(stream: &[u8]) -> Vec<u8> {
    let mut out = stream.to_vec();
    for (start, args) in marks(stream) {
        let mut marker = Vec::new();
        Motion::from_marker(args).0.mark(u16::MAX, 0, &mut marker);
        out[start..start + marker.len()].copy_from_slice(&marker);
    }
    out
}

pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::new();
    for group in bytes.chunks(3) {
        let word = group.iter().enumerate().fold(0u32, |word, (at, &byte)| {
            word | u32::from(byte) << (16 - 8 * at)
        });
        for at in 0..4 {
            if at <= group.len() {
                text.push(TABLE[(word >> (18 - 6 * at) & 63) as usize] as char);
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// One command as a program writes it.
pub(crate) fn wire(control: &str, payload: &[u8]) -> Vec<u8> {
    [
        ESC_G,
        control.as_bytes(),
        b";",
        base64(payload).as_bytes(),
        ST,
    ]
    .concat()
}

pub(crate) fn rgba(width: u32, height: u32, seed: u8) -> Vec<u8> {
    (0..width * height * 4)
        .map(|at| seed.wrapping_add(at as u8))
        .collect()
}

fn zlib(bytes: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(bytes, 6)
}

fn png_file(
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    palette: Option<&[u8]>,
    trns: Option<&[u8]>,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    if let Some(palette) = palette {
        encoder.set_palette(palette.to_vec());
    }
    if let Some(trns) = trns {
        encoder.set_trns(trns.to_vec());
    }
    let mut writer = encoder.write_header().expect("header");
    writer.write_image_data(data).expect("data");
    writer.finish().expect("finish");
    out
}

/// The reader's two halves and the store, joined the way a session joins
/// them: bytes in, the stream the parser would read out, and the answers
/// the parser's side wrote back.
/// The loop's turn over a PTY that has `chunk` to give: reads into `buf`,
/// which takes as much of it as fits each time, until the PTY has nothing
/// more and the read side holds nothing the buffer had no room for. Returns
/// the stream, which is nothing for reads that only feed a command still
/// arriving (`WouldBlock`).
fn turn(
    intake: &mut Intake,
    hub: &Hub,
    buf: &mut [u8],
    chunk: &[u8],
    now: Instant,
    env: Env,
    reply: &mut dyn FnMut(Vec<u8>),
) -> Vec<u8> {
    let mut stream = Vec::new();
    let mut left = chunk;
    loop {
        let got = intake.read(
            buf,
            now,
            |room| {
                let take = left.len().min(room.len());
                room[..take].copy_from_slice(&left[..take]);
                left = &left[take..];
                Ok(take)
            },
            hub,
            || env,
            reply,
        );
        stream.extend_from_slice(&buf[..got.unwrap_or(0)]);
        if left.is_empty() && !hub.backlog() {
            return stream;
        }
    }
}

struct Rig {
    intake: Intake,
    hub: Hub,
    /// Which screen the parser is on when a marker arrives.
    alt: bool,
    /// Where the cursor is when a marker arrives.
    cursor: (i32, u16),
    /// The answers to queries, which are the reader's own.
    queries: Vec<Vec<u8>>,
    /// Whether the pane is in a remote session: what the stream and the
    /// decode are told.
    remote: Arc<AtomicBool>,
}

impl Rig {
    fn new() -> Self {
        Self::with_hub(Hub::new())
    }

    fn with_hub(hub: Hub) -> Self {
        let remote = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&remote);
        hub.set_remote_gate(Box::new(move || flag.load(Ordering::Acquire)));
        Self {
            intake: Intake::new(),
            hub,
            alt: false,
            cursor: (0, 0),
            queries: Vec::new(),
            remote,
        }
    }

    /// What the reader knows of the terminal.
    fn env(&self) -> Env {
        ENV
    }

    fn screen(&self) -> Screen {
        Screen {
            alt: self.alt,
            cursor: self.cursor,
            ..Screen::blank(self.alt)
        }
    }

    /// Feeds `bytes` and applies every marker that comes out. Returns the
    /// stream and the answers.
    fn run(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut stream = Vec::new();
        let mut now = Instant::now();
        self.run_at(bytes, &mut now, &mut stream)
    }

    /// The PTY's reads of `bytes` and what each leads to, the way the read
    /// loop runs them: the images decoded between a read and its parse, the
    /// markers dispatched.
    fn run_at(
        &mut self,
        bytes: &[u8],
        now: &mut Instant,
        stream: &mut Vec<u8>,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut buf = vec![0u8; 1 << 20];
        let mut answers = Vec::new();
        let screen = self.screen();
        for chunk in bytes.chunks(buf.len()) {
            let delivered = self.read(&mut buf, chunk, *now);
            stream.extend_from_slice(&delivered);
            // The read loop decodes between a read and its parse.
            self.hub.decode_pending();
            for id in ids(&delivered) {
                answers.extend(self.hub.apply(id, &screen).unwrap_or_default());
            }
        }
        (anonymous(stream), answers)
    }

    /// The loop's turn over one read of the PTY that gave `chunk`: the read
    /// into `buf` and the reads of what did not fit, until none is left. The
    /// stream it comes to (none for a read that only feeds a command still
    /// arriving, which is `WouldBlock`).
    fn read(&mut self, buf: &mut [u8], chunk: &[u8], now: Instant) -> Vec<u8> {
        let env = self.env();
        let queries = &mut self.queries;
        turn(
            &mut self.intake,
            &self.hub,
            buf,
            chunk,
            now,
            env,
            &mut |reply| queries.push(reply),
        )
    }

    fn main_ids(&self) -> Vec<u32> {
        self.hub.with_images(|images| images.main().ids())
    }

    fn pixels(&self, id: u32) -> Option<Vec<u8>> {
        self.hub.with_images(|images| {
            images
                .main()
                .get(id)
                .map(|image| image.pixels.as_ref().clone())
        })
    }
}

fn ok(id: u32) -> Vec<u8> {
    format!("\x1b_Gi={id};OK\x1b\\").into_bytes()
}

fn error_of(answers: &[u8]) -> String {
    String::from_utf8_lossy(answers).into_owned()
}

// ─── transmission ───────────────────────────────────────────────────────

#[test]
fn a_raw_rgba_transmission_is_stored_and_acknowledged() {
    let mut rig = Rig::new();
    let pixels = rgba(2, 1, 1);
    let (stream, answers) = rig.run(&wire("a=t,f=32,s=2,v=1,i=7", &pixels));
    assert_eq!(stream, mark(), "the command is replaced by the marker");
    assert_eq!(answers, ok(7));
    assert_eq!(rig.pixels(7), Some(pixels));
    rig.hub.with_images(|images| {
        let image = images.main().get(7).expect("stored");
        assert_eq!((image.width, image.height), (2, 1));
    });
}

#[test]
fn rgb_gains_an_opaque_alpha_and_default_format_is_rgba() {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=24,s=2,v=1,i=1", &[1, 2, 3, 4, 5, 6]));
    assert_eq!(rig.pixels(1), Some(vec![1, 2, 3, 255, 4, 5, 6, 255]));
    rig.run(&wire("s=1,v=1,i=2", &[9, 8, 7, 6]));
    assert_eq!(rig.pixels(2), Some(vec![9, 8, 7, 6]));
}

#[test]
fn a_zlib_payload_is_inflated() {
    let mut rig = Rig::new();
    let pixels = rgba(8, 8, 3);
    let (_, answers) = rig.run(&wire("a=T,f=32,o=z,s=8,v=8,i=4", &zlib(&pixels)));
    assert_eq!(answers, ok(4));
    assert_eq!(rig.pixels(4), Some(pixels));
}

#[test]
fn png_payloads_of_every_color_type_come_out_as_rgba() {
    let mut rig = Rig::new();
    struct Case {
        color: png::ColorType,
        palette: Option<&'static [u8]>,
        trns: Option<&'static [u8]>,
        data: &'static [u8],
        expected: &'static [u8],
    }
    let cases = [
        Case {
            color: png::ColorType::Rgba,
            palette: None,
            trns: None,
            data: &[10, 20, 30, 40, 50, 60, 70, 80],
            expected: &[10, 20, 30, 40, 50, 60, 70, 80],
        },
        Case {
            color: png::ColorType::Rgb,
            palette: None,
            trns: None,
            data: &[10, 20, 30, 40, 50, 60],
            expected: &[10, 20, 30, 255, 40, 50, 60, 255],
        },
        Case {
            color: png::ColorType::Grayscale,
            palette: None,
            trns: None,
            data: &[7, 9],
            expected: &[7, 7, 7, 255, 9, 9, 9, 255],
        },
        Case {
            color: png::ColorType::GrayscaleAlpha,
            palette: None,
            trns: None,
            data: &[7, 100, 9, 200],
            expected: &[7, 7, 7, 100, 9, 9, 9, 200],
        },
        Case {
            color: png::ColorType::Indexed,
            palette: Some(&[1, 2, 3, 4, 5, 6]),
            trns: Some(&[128, 255]),
            data: &[0, 1],
            expected: &[1, 2, 3, 128, 4, 5, 6, 255],
        },
    ];
    for (id, case) in cases.into_iter().enumerate() {
        let id = id as u32 + 1;
        let color = case.color;
        let file = png_file(
            2,
            1,
            color,
            png::BitDepth::Eight,
            case.palette,
            case.trns,
            case.data,
        );
        let (_, answers) = rig.run(&wire(&format!("a=t,f=100,i={id}"), &file));
        assert_eq!(answers, ok(id), "{color:?}");
        assert_eq!(rig.pixels(id).as_deref(), Some(case.expected), "{color:?}");
    }
}

#[test]
fn a_sixteen_bit_png_is_brought_down_to_eight() {
    let mut rig = Rig::new();
    let file = png_file(
        1,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Sixteen,
        None,
        None,
        &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc],
    );
    rig.run(&wire("a=t,f=100,i=1", &file));
    assert_eq!(rig.pixels(1), Some(vec![0x12, 0x56, 0x9a, 255]));
}

#[test]
fn a_png_inside_zlib_is_inflated_first() {
    let mut rig = Rig::new();
    let file = png_file(
        1,
        1,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        None,
        None,
        &[1, 2, 3, 4],
    );
    rig.run(&wire("a=t,f=100,o=z,i=1", &zlib(&file)));
    assert_eq!(rig.pixels(1), Some(vec![1, 2, 3, 4]));
}

// ─── chunks ─────────────────────────────────────────────────────────────

#[test]
fn a_chunked_transmission_is_one_image_whose_chunks_name_only_m() {
    let mut rig = Rig::new();
    let pixels = rgba(4, 4, 9);
    let text = base64(&pixels);
    let pieces: Vec<&[u8]> = text.as_bytes().chunks(24).collect();
    let mut wire_bytes = Vec::new();
    for (at, piece) in pieces.iter().enumerate() {
        let control = match at {
            0 => "a=t,f=32,s=4,v=4,i=11,m=1".to_owned(),
            last if last + 1 == pieces.len() => "m=0".to_owned(),
            _ => "m=1".to_owned(),
        };
        wire_bytes.extend([ESC_G, control.as_bytes(), b";", piece, ST].concat());
    }
    let (stream, answers) = rig.run(&wire_bytes);
    assert_eq!(stream, mark(), "the chunks make one marker, at the last");
    assert_eq!(answers, ok(11), "one answer, after the last chunk");
    assert_eq!(rig.pixels(11), Some(pixels));
}

#[test]
fn a_chunk_may_stop_inside_a_group_of_four_characters() {
    let mut rig = Rig::new();
    let pixels = rgba(2, 2, 5);
    let text = base64(&pixels);
    let (a, b) = text.as_bytes().split_at(7);
    let first = [ESC_G, b"a=t,f=32,s=2,v=2,i=3,m=1;", a, ST].concat();
    let second = [ESC_G, b"m=0;", b, ST].concat();
    let (_, answers) = rig.run(&[first, second].concat());
    assert_eq!(answers, ok(3));
    assert_eq!(rig.pixels(3), Some(pixels));
}

#[test]
fn nothing_is_applied_until_the_last_chunk_and_the_flag_says_so() {
    let mut rig = Rig::new();
    let mut stream = Vec::new();
    let mut now = Instant::now();
    rig.run_at(
        &[ESC_G, b"a=t,f=32,s=1,v=1,i=1,m=1;AAAA", ST].concat(),
        &mut now,
        &mut stream,
    );
    assert!(
        stream.is_empty(),
        "an unfinished transmission leaves no marker"
    );
    assert!(rig.intake.receiver.is_open());
    assert!(rig.main_ids().is_empty());
    rig.run_at(&[ESC_G, b"m=0;AA==", ST].concat(), &mut now, &mut stream);
    assert!(!rig.intake.receiver.is_open());
    assert_eq!(rig.main_ids(), [1]);
}

#[test]
fn a_transmission_that_keeps_coming_is_not_one_that_waited_too_long() {
    // Every chunk is inside the wait of the one before it, though the whole
    // is far longer than the wait (a big image over a slow link).
    let mut rig = Rig::new();
    let mut stream = Vec::new();
    let mut now = Instant::now();
    let step = OPEN_TIMEOUT - Duration::from_secs(1);
    rig.run_at(
        &[ESC_G, b"a=t,f=32,s=1,v=1,i=6,m=1;AAAA", ST].concat(),
        &mut now,
        &mut stream,
    );
    now += step;
    rig.run_at(&[ESC_G, b"m=1;AAAA", ST].concat(), &mut now, &mut stream);
    now += step;
    // 4 + 4 + 0 characters is 6 bytes, and a pixel is 4: the last chunk
    // finishes it with the right size.
    let (_, answers) = rig.run_at(&[ESC_G, b"m=0;", ST].concat(), &mut now, &mut stream);
    assert!(
        !error_of(&answers).contains("in time"),
        "{}",
        error_of(&answers)
    );
}

#[test]
fn a_command_that_names_its_action_ends_a_transmission_in_progress() {
    let mut rig = Rig::new();
    rig.run(&[ESC_G, b"a=t,f=32,s=1,v=1,i=1,m=1;AAAA", ST].concat());
    assert!(rig.intake.receiver.is_open());
    rig.run(&wire("a=t,f=32,s=1,v=1,i=2", &[1, 2, 3, 4]));
    assert!(!rig.intake.receiver.is_open());
    assert_eq!(rig.main_ids(), [2]);
}

#[test]
fn a_transmission_that_waits_too_long_is_dropped_and_told_so() {
    let mut rig = Rig::new();
    let mut stream = Vec::new();
    let mut now = Instant::now();
    rig.run_at(
        &[ESC_G, b"a=t,f=32,s=1,v=1,i=6,m=1;AAAA", ST].concat(),
        &mut now,
        &mut stream,
    );
    now += OPEN_TIMEOUT + Duration::from_secs(1);
    let (_, answers) = rig.run_at(&[ESC_G, b"m=0;AA==", ST].concat(), &mut now, &mut stream);
    assert!(
        error_of(&answers).starts_with("\x1b_Gi=6;EINVAL:"),
        "{answers:?}"
    );
    assert!(rig.main_ids().is_empty());
}

// ─── what is refused ────────────────────────────────────────────────────

#[test]
fn every_way_a_payload_can_be_wrong_is_an_answer_and_nothing_is_stored() {
    let mut rig = Rig::new();
    let pixels = rgba(2, 2, 0);
    let big = (MAX_DIMENSION + 1).to_string();
    let cases: Vec<(String, Vec<u8>, &str)> = vec![
        (
            "a=t,f=32,s=2,v=2,i=1".into(),
            pixels[..15].to_vec(),
            "ENODATA",
        ),
        (
            "a=t,f=32,s=2,v=2,i=1".into(),
            [pixels.clone(), vec![0]].concat(),
            "EFBIG",
        ),
        ("a=t,f=32,i=1".into(), pixels.clone(), "EINVAL"),
        ("a=t,f=32,s=0,v=2,i=1".into(), pixels.clone(), "EINVAL"),
        (format!("a=t,f=32,s={big},v=1,i=1"), pixels.clone(), "EFBIG"),
        (
            "a=t,f=32,s=10000,v=10000,i=1".into(),
            pixels.clone(),
            "ENOSPC",
        ),
        (
            "a=t,f=32,o=z,s=2,v=2,i=1".into(),
            b"not zlib at all".to_vec(),
            "EINVAL",
        ),
        (
            "a=t,f=32,o=z,s=2,v=2,i=1".into(),
            zlib(&rgba(4, 4, 0)),
            "EFBIG",
        ),
        ("a=t,f=100,i=1".into(), b"not a png".to_vec(), "EINVAL"),
        ("a=t,f=100,i=1".into(), Vec::new(), "EINVAL"),
        ("a=t,t=f,i=1".into(), b"/tmp/x".to_vec(), "EINVAL"),
        ("a=t,f=7,i=1".into(), pixels.clone(), "EINVAL"),
        ("a=f,i=1".into(), pixels.clone(), "EINVAL"),
        ("a=z,i=1".into(), pixels.clone(), "EINVAL"),
    ];
    for (control, payload, code) in cases {
        let (_, answers) = rig.run(&wire(&control, &payload));
        let text = error_of(&answers);
        assert!(
            text.starts_with(&format!("\x1b_Gi=1;{code}:")) && text.ends_with("\x1b\\"),
            "{control}: {text:?}"
        );
        assert!(rig.main_ids().is_empty(), "{control}");
    }
}

#[test]
fn a_png_that_is_cut_short_or_corrupt_does_not_panic() {
    let mut rig = Rig::new();
    let file = png_file(
        8,
        8,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        None,
        None,
        &rgba(8, 8, 1),
    );
    for cut in [8, 20, file.len() / 2] {
        let (_, answers) = rig.run(&wire("a=t,f=100,i=1", &file[..cut]));
        assert!(error_of(&answers).contains("EINVAL"), "{cut}: {answers:?}");
    }
    let mut flipped = file.clone();
    for at in 0..flipped.len() {
        flipped[at] ^= 0xff;
        rig.run(&wire("a=t,f=100,i=1", &flipped));
        flipped[at] ^= 0xff;
    }
}

#[test]
fn invalid_base64_is_refused() {
    let mut rig = Rig::new();
    let bytes = [ESC_G, b"a=t,f=32,s=1,v=1,i=1;AA!A", ST].concat();
    let (_, answers) = rig.run(&bytes);
    assert!(error_of(&answers).starts_with("\x1b_Gi=1;EINVAL:"));
}

#[test]
fn a_payload_past_the_limit_is_refused_without_keeping_it() {
    let mut rig = Rig::new();
    rig.intake.splitter = Splitter::new(8);
    let (_, answers) = rig.run(&wire("a=t,f=32,s=2,v=2,i=1", &rgba(2, 2, 0)));
    assert!(error_of(&answers).starts_with("\x1b_Gi=1;EFBIG:"));
    assert!(rig.main_ids().is_empty());
}

#[test]
fn a_command_whose_controls_cannot_be_read_still_tells_who_asked() {
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("i=9,a=t,s=banana", &[0; 4]));
    assert!(error_of(&answers).starts_with("\x1b_Gi=9;EINVAL:"));
}

// ─── answers ────────────────────────────────────────────────────────────

#[test]
fn quiet_hides_the_ok_then_everything() {
    let mut rig = Rig::new();
    let px = [0u8; 4];
    let (_, answers) = rig.run(&wire("a=t,f=32,s=1,v=1,i=1,q=1", &px));
    assert!(answers.is_empty(), "q=1 hides OK");
    let (_, answers) = rig.run(&wire("a=t,f=32,s=1,v=1,i=1,q=1", &px[..2]));
    assert!(error_of(&answers).contains("ENODATA"), "q=1 shows errors");
    let (_, answers) = rig.run(&wire("a=t,f=32,s=1,v=1,i=1,q=2", &px[..2]));
    assert!(answers.is_empty(), "q=2 hides errors too");
}

#[test]
fn a_command_with_neither_id_nor_number_cannot_be_answered() {
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("a=t,f=32,s=1,v=1", &[0; 4]));
    assert!(answers.is_empty());
    assert_eq!(rig.hub.with_images(|images| images.main().len()), 1);
    let (_, answers) = rig.run(&wire("a=t,f=32,s=1,v=1", &[0; 2]));
    assert!(answers.is_empty(), "not even an error");
}

#[test]
fn the_answer_names_the_number_and_the_placement_it_was_given() {
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("a=t,f=32,s=1,v=1,I=5,p=2", &[0; 4]));
    let text = error_of(&answers);
    let id = rig.main_ids()[0];
    assert!(id >= 0x8000_0000, "the terminal chose the id: {id}");
    assert_eq!(text, format!("\x1b_Gi={id},I=5,p=2;OK\x1b\\"));
}

#[test]
fn a_query_answers_as_a_store_would_and_stores_nothing() {
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire("i=4207,a=q,t=d,f=24,s=1,v=1", &[0, 0, 0]));
    assert!(stream.is_empty(), "a query leaves nothing in the stream");
    assert!(answers.is_empty());
    assert_eq!(rig.queries, [ok(4207)]);
    assert!(rig.main_ids().is_empty());
    rig.queries.clear();
    rig.run(&wire("i=4207,a=q,f=24,s=1,v=1", &[0, 0]));
    assert!(error_of(&rig.queries.concat()).contains("ENODATA"));
}

#[test]
fn terminal_browsers_own_query_text_is_a_valid_query() {
    let mut rig = Rig::new();
    rig.run(b"\x1b_Gi=4207,a=q,t=d,f=24,s=1,v=1;AAAA\x1b\\");
    assert_eq!(rig.queries, [ok(4207)]);
}

#[test]
fn a_broken_query_is_still_a_query() {
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire("i=3,a=q,f=99", &[]));
    assert!(stream.is_empty() && answers.is_empty());
    assert!(error_of(&rig.queries.concat()).starts_with("\x1b_Gi=3;EINVAL:"));
}

// ─── the store ──────────────────────────────────────────────────────────

fn parsed(control: &str) -> Command {
    Command::parse(control.as_bytes()).expect("parses")
}

fn decoded(bytes: usize) -> Decoded {
    Decoded {
        width: 1,
        height: (bytes / 4) as u32,
        pixels: vec![0; bytes],
    }
}

#[test]
fn sending_an_id_again_replaces_the_image() {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=1,v=1,i=1", &[1, 1, 1, 1]));
    rig.run(&wire("a=t,f=32,s=1,v=1,i=2", &[2, 2, 2, 2]));
    rig.run(&wire("a=t,f=32,s=1,v=1,i=1", &[3, 3, 3, 3]));
    assert_eq!(rig.pixels(1), Some(vec![3; 4]));
    assert_eq!(rig.main_ids(), [2, 1], "the replaced id is the newest now");
    rig.hub.with_images(|images| {
        assert_eq!(images.main().bytes(), 8);
        assert_eq!(images.main().get(1).expect("kept").generation, 3);
    });
}

#[test]
fn past_the_quota_images_without_a_placement_go_first_then_the_oldest() {
    let mut images = Images::with_quota(100);
    for id in 1..=4 {
        images.store(
            &Screen::blank(false),
            &parsed(&format!("i={id}")),
            decoded(20),
            Motion::NONE,
        );
    }
    // 1 and 3 are shown; 2 and 4 are not.
    images.main_mut().pin(1);
    images.main_mut().pin(3);
    let reply = images.store(
        &Screen::blank(false),
        &parsed("i=5"),
        decoded(40),
        Motion::NONE,
    );
    assert_eq!(reply.encode(), Some(ok(5)));
    // 120 > 100: the oldest unshown (2) goes; 100 now fits.
    assert_eq!(images.main().ids(), [1, 3, 4, 5]);
    images.store(
        &Screen::blank(false),
        &parsed("i=6"),
        decoded(40),
        Motion::NONE,
    );
    // Unshown 4 goes, then 5 (the next unshown, 40 bytes): 100 fits.
    assert_eq!(images.main().ids(), [1, 3, 6]);
    images.main_mut().pin(6);
    images.store(
        &Screen::blank(false),
        &parsed("i=7"),
        decoded(80),
        Motion::NONE,
    );
    // Everything left is shown: the oldest go until 7 fits (1, 3, 6).
    assert_eq!(images.main().ids(), [7]);
    assert!(images.main().bytes() <= 100);
}

#[test]
fn an_image_larger_than_the_quota_is_refused_and_evicts_nothing() {
    let mut images = Images::with_quota(100);
    images.store(
        &Screen::blank(false),
        &parsed("i=1"),
        decoded(60),
        Motion::NONE,
    );
    let reply = images.store(
        &Screen::blank(false),
        &parsed("i=2"),
        decoded(104),
        Motion::NONE,
    );
    assert!(String::from_utf8_lossy(&reply.encode().expect("answered")).contains("ENOSPC"));
    assert_eq!(images.main().ids(), [1]);
}

#[test]
fn each_screen_has_its_own_store_and_the_alternate_one_goes_when_it_is_left() {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=1,v=1,i=1", &[1; 4]));
    rig.alt = true;
    rig.run(&wire("a=t,f=32,s=1,v=1,i=1", &[2; 4]));
    rig.run(&wire("a=t,f=32,s=1,v=1,i=2", &[3; 4]));
    rig.hub.with_images(|images| {
        assert_eq!(images.main().ids(), [1]);
        assert_eq!(images.alt().ids(), [1, 2]);
        assert_eq!(
            images.main().get(1).expect("kept").pixels.as_ref(),
            &vec![1; 4]
        );
    });
    rig.hub.drop_alt();
    rig.hub.with_images(|images| {
        assert_eq!(images.main().ids(), [1], "the main screen's wait for it");
        assert!(images.alt().ids().is_empty());
        assert_eq!(images.alt().bytes(), 0);
    });
}

#[test]
fn deleting_by_id_frees_the_data_only_in_upper_case() {
    let mut rig = Rig::new();
    for id in 1..=3 {
        rig.run(&wire(&format!("a=t,f=32,s=1,v=1,i={id}"), &[0; 4]));
    }
    rig.run(&wire("a=d,d=i,i=1", &[]));
    assert_eq!(
        rig.main_ids(),
        [1, 2, 3],
        "d=i takes the placements, not the data"
    );
    rig.run(&wire("a=d,d=I,i=1", &[]));
    assert_eq!(rig.main_ids(), [2, 3]);
    rig.run(&wire("a=d,d=I,i=99", &[]));
    assert_eq!(rig.main_ids(), [2, 3], "an unknown id is nothing");
    rig.run(&wire("a=d,d=A", &[]));
    assert!(rig.main_ids().is_empty(), "d=A frees what has no placement");
}

#[test]
fn a_shown_image_keeps_its_data_until_its_last_placement_goes() {
    let mut rig = Rig::new();
    rig.run(&wire("a=T,f=32,s=1,v=1,i=1,p=1", &[0; 4]));
    rig.run(&wire("a=p,i=1,p=2", &[]));
    rig.run(&wire("a=d,d=I,i=1,p=1", &[]));
    assert_eq!(rig.main_ids(), [1], "a placement is left");
    rig.run(&wire("a=d,d=I,i=1,p=2", &[]));
    assert!(rig.main_ids().is_empty(), "the last one took the data");
}

#[test]
fn deleting_all_visible_keeps_ided_data_and_drops_anonymous_images() {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=1,v=1,i=1", &[0; 4]));
    rig.run(&wire("a=t,f=32,s=1,v=1", &[0; 4]));
    assert_eq!(rig.hub.with_images(|images| images.main().len()), 2);
    rig.run(&wire("a=d,d=a", &[]));
    assert_eq!(
        rig.main_ids(),
        [1],
        "nothing can address the anonymous one again"
    );
}

#[test]
fn deleting_by_number_takes_the_newest_image_with_it() {
    let mut rig = Rig::new();
    for _ in 0..3 {
        rig.run(&wire("a=t,f=32,s=1,v=1,I=7", &[0; 4]));
    }
    let before = rig.main_ids();
    assert_eq!(before.len(), 3);
    rig.run(&wire("a=d,d=N,I=7", &[]));
    assert_eq!(rig.main_ids(), before[..2]);
    rig.run(&wire("a=d,d=N,I=8", &[]));
    assert_eq!(rig.main_ids(), before[..2]);
}

#[test]
fn deleting_a_range_of_ids() {
    let mut rig = Rig::new();
    for id in [1, 2, 3, 4, 5] {
        rig.run(&wire(&format!("a=t,f=32,s=1,v=1,i={id}"), &[0; 4]));
    }
    rig.run(&wire("a=d,d=R,x=2,y=4", &[]));
    assert_eq!(rig.main_ids(), [1, 5]);
}

#[test]
fn a_delete_with_an_unknown_specifier_is_refused() {
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("a=d,d=!,i=1", &[]));
    assert!(error_of(&answers).starts_with("\x1b_Gi=1;EINVAL:"));
}

// ─── the hub ────────────────────────────────────────────────────────────

#[test]
fn a_marker_with_no_record_does_nothing() {
    let hub = Hub::new();
    assert_eq!(hub.apply_next(&Screen::blank(false)), None);
    assert_eq!(hub.apply_next(&Screen::blank(true)), None);
    assert_eq!(hub.with_images(|images| images.main().len()), 0);
}

#[test]
fn records_are_applied_in_the_order_they_were_queued() {
    let mut rig = Rig::new();
    let mut stream = Vec::new();
    let now = Instant::now();
    // Both commands arrive before either marker is dispatched, as in a
    // synchronized update.
    let both = [
        wire("a=t,f=32,s=1,v=1,i=1", &[1; 4]),
        wire("a=t,f=32,s=1,v=1,i=1", &[2; 4]),
    ]
    .concat();
    let hub = &rig.hub;
    let receiver = &mut rig.intake.receiver;
    rig.intake.splitter.feed(&both, |piece| {
        if let Piece::Command(apc) = piece {
            receiver.receive(apc, now, ENV, &|_| None, &|| false, |outcome| {
                if let Outcome::Pending(pending) = outcome {
                    hub.queue(1, pending);
                    stream.extend_from_slice(&mark());
                }
            });
        }
    });
    assert_eq!(stream.len(), 2 * mark().len());
    assert!(hub.has_undecoded());
    hub.decode_pending();
    assert!(!hub.has_undecoded());
    assert_eq!(hub.apply_next(&Screen::blank(false)), Some(ok(1)));
    assert_eq!(rig.pixels(1), Some(vec![1; 4]));
    assert_eq!(rig.hub.apply_next(&Screen::blank(false)), Some(ok(1)));
    assert_eq!(rig.pixels(1), Some(vec![2; 4]));
}

/// Queues what `bytes` carry without decoding or applying anything, as a
/// read does before the loop decodes.
fn queue_only(rig: &mut Rig, bytes: &[u8]) {
    let mut buf = vec![0u8; 1 << 20];
    let now = Instant::now();
    for chunk in bytes.chunks(buf.len()) {
        rig.read(&mut buf, chunk, now);
    }
}

#[test]
fn text_waiting_for_its_marker_is_bounded_too() {
    // Two 16-byte images are 24 characters of text each.
    let mut rig = Rig::with_hub(Hub::with_limits(1 << 20, 40));
    let both = [
        wire("a=t,f=32,s=2,v=2,i=1", &[0; 16]),
        wire("a=t,f=32,s=2,v=2,i=2", &[0; 16]),
    ]
    .concat();
    queue_only(&mut rig, &both);
    rig.hub.decode_pending();
    assert_eq!(rig.hub.apply_next(&Screen::blank(false)), Some(ok(1)));
    let second = rig
        .hub
        .apply_next(&Screen::blank(false))
        .map(|bytes| error_of(&bytes));
    assert!(second.is_some_and(|text| text.starts_with("\x1b_Gi=2;ENOSPC:")));
    assert_eq!(rig.main_ids(), [1]);
}

#[test]
fn an_image_that_inflates_past_what_may_wait_is_refused_when_decoded() {
    // Little text, a lot of pixels: the bound has to be checked again once
    // the size is known.
    let mut rig = Rig::with_hub(Hub::with_limits(1 << 20, 40));
    let bomb = zlib(&[0; 64]);
    queue_only(&mut rig, &wire("a=t,f=32,o=z,s=4,v=4,i=3", &bomb));
    assert!(rig.hub.has_undecoded());
    rig.hub.decode_pending();
    let answer = rig
        .hub
        .apply_next(&Screen::blank(false))
        .map(|bytes| error_of(&bytes));
    assert!(answer.is_some_and(|text| text.starts_with("\x1b_Gi=3;ENOSPC:")));
    assert!(rig.main_ids().is_empty());
}

#[test]
fn decoding_goes_through_the_queue_in_place_and_keeps_its_order() {
    let mut rig = Rig::new();
    let bytes = [
        wire("a=t,f=32,s=1,v=1,i=1", &[1; 4]),
        wire("a=d,d=I,i=1", &[]),
        wire("a=t,f=32,s=1,v=1,i=1", &[1; 2]),
        wire("a=t,f=32,s=1,v=1,i=2", &[2; 4]),
    ]
    .concat();
    queue_only(&mut rig, &bytes);
    assert_eq!(rig.hub.waiting_records(), 4);
    assert!(rig.hub.has_undecoded());
    rig.hub.decode_pending();
    assert!(!rig.hub.has_undecoded());
    assert_eq!(
        rig.hub.waiting_records(),
        4,
        "decoding neither adds nor drops"
    );
    let answers: Vec<_> = (0..4)
        .map(|_| {
            rig.hub
                .apply_next(&Screen::blank(false))
                .map(|bytes| error_of(&bytes))
        })
        .collect();
    assert_eq!(answers[0].as_deref(), Some("\x1b_Gi=1;OK\x1b\\"));
    assert_eq!(answers[1], None);
    assert!(
        answers[2]
            .as_deref()
            .is_some_and(|text| text.contains("ENODATA"))
    );
    assert_eq!(answers[3].as_deref(), Some("\x1b_Gi=2;OK\x1b\\"));
    assert_eq!(rig.main_ids(), [2]);
}

#[test]
fn a_record_applied_before_it_was_decoded_is_decoded_on_the_spot() {
    let mut rig = Rig::new();
    queue_only(&mut rig, &wire("a=t,f=32,s=1,v=1,i=4", &[4; 4]));
    assert_eq!(rig.hub.apply_next(&Screen::blank(false)), Some(ok(4)));
    assert!(!rig.hub.has_undecoded());
    assert_eq!(rig.pixels(4), Some(vec![4; 4]));
}

#[test]
fn a_query_is_decoded_where_it_is_read_and_may_only_be_small() {
    let mut rig = Rig::new();
    let big = vec![0u8; QUERY_LIMIT];
    rig.run(&wire("i=5,a=q,f=32,s=100,v=100", &big));
    let text = error_of(&rig.queries.concat());
    assert!(text.starts_with("\x1b_Gi=5;EFBIG:"), "{text:?}");
    assert!(!rig.hub.has_undecoded());
}

#[test]
fn the_open_flag_and_the_wait_for_it() {
    let hub = Hub::new();
    let started = Instant::now();
    hub.wait_closed(Duration::from_secs(5));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "closed: no wait"
    );
    hub.set_open(true);
    let started = Instant::now();
    hub.wait_closed(Duration::from_millis(60));
    assert!(
        started.elapsed() >= Duration::from_millis(60),
        "open: waits the grace"
    );
    hub.set_open(false);
}

// ─── bytes ──────────────────────────────────────────────────────────────

#[test]
fn the_marker_is_a_dispatchable_private_mode() {
    let base = ARG_BASE;
    let expected = format!("\x1b[?{base};{base};{base};{base};{base};{MARK_MODE}h");
    assert_eq!(mark(), expected.as_bytes());
}

#[test]
fn replies_are_formed_as_the_protocol_says() {
    let echo = Echo {
        id: 1,
        number: 2,
        placement: 3,
        quiet: 0,
    };
    assert_eq!(
        Reply::ok(echo).encode(),
        Some(b"\x1b_Gi=1,I=2,p=3;OK\x1b\\".to_vec())
    );
    assert_eq!(
        Reply::failed(echo, Error::invalid("bad")).encode(),
        Some(b"\x1b_Gi=1,I=2,p=3;EINVAL:bad\x1b\\".to_vec())
    );
    assert_eq!(Reply::ok(Echo::default()).encode(), None);
}

#[test]
fn garbled_controls_never_reach_a_command() {
    let mut receiver = Receiver::new();
    let mut seen = 0;
    receiver.receive(
        Apc {
            garbled: true,
            ..Apc::default()
        },
        Instant::now(),
        ENV,
        &|_| None,
        &|| false,
        |_| seen += 1,
    );
    assert_eq!(seen, 0);
}

// ─── placing ─────────────────────────────────────────────────────────────

/// 20 × 40 pixels: two columns by two rows of the tests' 10 × 20 pixel cells.
fn tile() -> Vec<u8> {
    rgba(20, 40, 0)
}

fn placed(rig: &Rig) -> Vec<(u32, i32, u16)> {
    rig.hub.with_images(|images| images.main().placed())
}

#[test]
fn a_transmission_that_shows_places_the_image_at_the_cursor_and_moves_it() {
    let mut rig = Rig::new();
    rig.cursor = (3, 5);
    let (stream, answers) = rig.run(&wire("a=T,f=32,s=20,v=40,i=1", &tile()));
    // One row down before the marker (the image is two rows tall), two
    // columns across after it.
    assert_eq!(stream, marker(1, 2));
    assert_eq!(answers, ok(1));
    // At the marker the cursor is on the image's last row: the top is one up.
    assert_eq!(placed(&rig), [(1, 2, 5)]);
}

#[test]
fn a_cursor_that_is_asked_to_stay_does_not_move() {
    let mut rig = Rig::new();
    rig.cursor = (3, 5);
    let (stream, _) = rig.run(&wire("a=T,f=32,s=20,v=40,i=1,C=1", &tile()));
    assert_eq!(stream, mark());
    assert_eq!(placed(&rig), [(1, 3, 5)]);
}

#[test]
fn an_image_as_wide_as_the_screen_leaves_the_cursor_on_the_next_line() {
    let mut rig = Rig::new();
    let (stream, _) = rig.run(&wire("a=T,f=32,s=800,v=20,i=1,c=80,r=1", &rgba(800, 20, 0)));
    assert_eq!(stream, wrapped(0));
}

#[test]
fn a_png_is_sized_from_its_header_without_decoding_it() {
    let png = png_file(
        30,
        40,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        None,
        None,
        &rgba(30, 40, 3),
    );
    // 30 × 40 pixels are three columns by two rows.
    let around = marker(1, 3);
    let mut rig = Rig::new();
    let (stream, _) = rig.run(&wire("a=T,f=100,i=1", &png));
    assert_eq!(stream, around);
    // The same inside a zlib stream: the head of it is inflated.
    let mut rig = Rig::new();
    let (stream, _) = rig.run(&wire("a=T,f=100,o=z,i=1", &zlib(&png)));
    assert_eq!(stream, around);
    // The same in chunks.
    let mut rig = Rig::new();
    let text = base64(&png);
    let (head, tail) = text.split_at(64);
    let chunks = [
        ESC_G,
        b"a=T,f=100,i=1,m=1;",
        head.as_bytes(),
        ST,
        ESC_G,
        b"m=0;",
        tail.as_bytes(),
        ST,
    ]
    .concat();
    let (stream, _) = rig.run(&chunks);
    assert_eq!(stream, around);
    assert_eq!(placed(&rig), [(1, -1, 0)]);
}

#[test]
fn an_image_whose_size_cannot_be_told_leaves_the_cursor_alone() {
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire(
        "a=T,f=100,i=1",
        b"not a PNG, not even the start of one",
    ));
    assert_eq!(stream, mark());
    assert!(error_of(&answers).contains("EINVAL"));
    assert!(placed(&rig).is_empty());
}

#[test]
fn placing_a_stored_image_is_sized_from_the_store_or_from_the_command_before_it() {
    let both = [
        wire("a=t,f=32,s=20,v=40,i=1", &tile()),
        wire("a=p,i=1,p=9", &[]),
    ]
    .concat();
    let around = marker(1, 2);
    // Both in one read: the image is still waiting for its marker.
    let mut rig = Rig::new();
    rig.cursor = (4, 0);
    let (stream, _) = rig.run(&both);
    assert_eq!(stream, [&mark()[..], &around].concat());
    // The image was sent long ago, and the receiver has forgotten it.
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=20,v=40,i=1", &tile()));
    rig.intake.receiver = Receiver::new();
    let (stream, answers) = rig.run(&wire("a=p,i=1,p=9", &[]));
    assert_eq!(stream, around);
    assert_eq!(answers, b"\x1b_Gi=1,p=9;OK\x1b\\");
}

#[test]
fn placing_what_is_not_there_is_an_answer_and_moves_nothing() {
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire("a=p,i=9", &[]));
    assert_eq!(stream, mark());
    assert!(error_of(&answers).starts_with("\x1b_Gi=9;ENOENT:"));
    assert!(placed(&rig).is_empty());
}

#[test]
fn an_image_sent_by_number_is_placed_by_number() {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=20,v=40,I=5", &tile()));
    let (_, answers) = rig.run(&wire("a=p,I=5", &[]));
    let answer = error_of(&answers);
    assert!(
        answer.contains("I=5") && answer.ends_with(";OK\x1b\\"),
        "{answer:?}"
    );
    assert_eq!(placed(&rig).len(), 1);
}

#[test]
fn a_placement_id_moves_the_placement_it_names_and_no_id_adds_one() {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=20,v=40,i=1", &tile()));
    for (cursor, control) in [
        ((1, 1), "a=p,i=1,p=3,C=1"),
        ((7, 2), "a=p,i=1,p=3,C=1"),
        ((9, 9), "a=p,i=1,p=4,C=1"),
        ((12, 0), "a=p,i=1,C=1"),
        ((13, 0), "a=p,i=1,C=1"),
    ] {
        rig.cursor = cursor;
        rig.run(&wire(control, &[]));
    }
    assert_eq!(
        placed(&rig),
        [(1, 7, 2), (1, 9, 9), (1, 12, 0), (1, 13, 0)],
        "p=3 was moved, the two without an id were added"
    );
}

#[test]
fn sending_an_id_again_ends_the_placements_of_the_image_it_replaces() {
    let mut rig = Rig::new();
    rig.run(&wire("a=T,f=32,s=20,v=40,i=1,C=1", &tile()));
    rig.run(&wire("a=T,f=32,s=20,v=40,i=2,C=1", &tile()));
    assert_eq!(placed(&rig).len(), 2);
    rig.run(&wire("a=t,f=32,s=20,v=40,i=1", &tile()));
    assert_eq!(placed(&rig), [(2, 0, 0)]);
}

#[test]
fn a_layer_is_signed() {
    let mut rig = Rig::new();
    rig.run(&wire("a=T,f=32,s=20,v=40,i=1,z=-1073741825,C=1", &tile()));
    let z = rig
        .hub
        .with_images(|images| images.main().placements()[0].spec.z);
    assert_eq!(z, -1_073_741_825);
}

#[test]
fn the_layer_changes_at_the_protocols_two_thresholds() {
    let mut rig = Rig::new();
    let layers: Vec<_> = [i32::MIN, -1_073_741_825, -1_073_741_824, -1, 0, i32::MAX]
        .into_iter()
        .map(|z| {
            rig.cursor = (0, 0);
            rig.run(&wire(&format!("a=T,f=32,s=20,v=40,i=1,z={z},C=1"), &tile()));
            let mut pictures = Pictures::default();
            rig.hub.with_images(|images| {
                images.collect(
                    false,
                    (10, 20),
                    80,
                    Window {
                        first: 0,
                        last: 23,
                        shift: 0,
                    },
                    None,
                    &mut pictures,
                );
            });
            pictures.grid()[0].layer()
        })
        .collect();
    use crate::PictureLayer::{Ground, Over, Text};
    assert_eq!(layers, [Ground, Ground, Text, Text, Over, Over]);
}

#[test]
fn deleting_the_visible_placements_leaves_what_has_gone_up_into_the_history() {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=20,v=40,i=1", &tile()));
    for line in [-5, 0, 30] {
        rig.cursor = (line, 0);
        rig.run(&wire("a=p,i=1,C=1", &[]));
    }
    rig.run(&wire("a=d,d=a", &[]));
    assert_eq!(placed(&rig), [(1, -5, 0)]);
}

/// Three placements of one 2 × 2 cell image: A at line 2, column 2; B at line
/// 5, column 10, layer 5; C at line 2, column 20.
fn scene() -> Rig {
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=20,v=40,i=1", &tile()));
    for (cursor, control) in [
        ((2, 2), "a=p,i=1,p=1,C=1"),
        ((5, 10), "a=p,i=1,p=2,C=1,z=5"),
        ((2, 20), "a=p,i=1,p=3,C=1"),
    ] {
        rig.cursor = cursor;
        rig.run(&wire(control, &[]));
    }
    rig
}

fn left_after(control: &str) -> Vec<(i32, u16)> {
    let mut rig = scene();
    rig.cursor = (3, 3);
    rig.run(&wire(control, &[]));
    placed(&rig)
        .into_iter()
        .map(|(_, line, col)| (line, col))
        .collect()
}

#[test]
fn deleting_by_position_takes_the_placements_that_cover_it() {
    // Cells are 1-based on the wire: column 3, row 3 is line 2, column 2's
    // neighbour on the right and below A's corner.
    assert_eq!(left_after("a=d,d=p,x=3,y=3"), [(5, 10), (2, 20)]);
    assert_eq!(left_after("a=d,d=p,x=5,y=3"), [(2, 2), (5, 10), (2, 20)]);
    assert_eq!(left_after("a=d,d=q,x=11,y=6,z=5"), [(2, 2), (2, 20)]);
    assert_eq!(
        left_after("a=d,d=q,x=11,y=6,z=4").len(),
        3,
        "the layer differs"
    );
    assert_eq!(left_after("a=d,d=x,x=21"), [(2, 2), (5, 10)]);
    assert_eq!(left_after("a=d,d=y,y=4"), [(5, 10)], "A and C cover line 3");
    assert_eq!(left_after("a=d,d=z,z=5"), [(2, 2), (2, 20)]);
    // The cursor of `scene` is at line 3, column 3: A's.
    assert_eq!(left_after("a=d,d=c"), [(5, 10), (2, 20)]);
}

#[test]
fn what_is_deleted_by_position_frees_the_image_only_in_upper_case() {
    let mut rig = Rig::new();
    rig.run(&wire("a=T,f=32,s=20,v=40,i=1,C=1", &tile()));
    rig.run(&wire("a=d,d=z,z=0", &[]));
    assert_eq!(rig.main_ids(), [1], "the data stays for the next placement");
    rig.run(&wire("a=p,i=1,C=1", &[]));
    rig.run(&wire("a=d,d=Z,z=0", &[]));
    assert!(rig.main_ids().is_empty());
}

#[test]
fn the_frame_gets_the_pictures_in_drawing_order() {
    let mut rig = scene();
    // One in the history, one across the grid's top, one past the right edge.
    for cursor in [(-2, 4), (-1, 0), (0, 85)] {
        rig.cursor = cursor;
        rig.run(&wire("a=p,i=1,C=1", &[]));
    }
    let mut pictures = Pictures::default();
    rig.hub.with_images(|images| {
        images.collect(
            false,
            (10, 20),
            80,
            Window {
                first: 0,
                last: 23,
                shift: 0,
            },
            Some(Window {
                first: -4,
                last: -1,
                shift: 4,
            }),
            &mut pictures,
        );
    });
    let rows = |list: &[Picture]| -> Vec<(i32, u16, i32)> {
        list.iter()
            .map(|picture| (picture.row, picture.col, picture.z))
            .collect()
    };
    // By layer, then by age; the one at column 85 is off the screen.
    assert_eq!(
        rows(pictures.grid()),
        [(2, 2, 0), (2, 20, 0), (-1, 0, 0), (5, 10, 5)]
    );
    // Rows of the band count from its top (line -4).
    assert_eq!(rows(pictures.band()), [(2, 4, 0), (3, 0, 0)]);
    let first = &pictures.grid()[0];
    assert_eq!((first.width, first.height), (20, 40));
    assert_eq!(first.source, [0, 0, 20, 40]);
    assert_eq!(first.size, (20, 40));
    assert_eq!(first.pixels.len(), 20 * 40 * 4);
}

#[test]
fn a_placement_cut_by_its_source_rectangle_is_drawn_that_big() {
    let mut rig = Rig::new();
    rig.run(&wire(
        "a=T,f=32,s=20,v=40,i=1,C=1,x=10,y=20,w=10,h=20,X=3,Y=4",
        &tile(),
    ));
    let mut pictures = Pictures::default();
    rig.hub.with_images(|images| {
        images.collect(
            false,
            (10, 20),
            80,
            Window {
                first: 0,
                last: 23,
                shift: 0,
            },
            None,
            &mut pictures,
        );
    });
    let picture = &pictures.grid()[0];
    assert_eq!(picture.source, [10, 20, 10, 20]);
    assert_eq!((picture.width, picture.height), (10, 20));
    assert_eq!((picture.x_offset, picture.y_offset), (3, 4));
}

// ─── placing, with a grid ────────────────────────────────────────────────

mod screenful {
    use std::sync::Mutex;

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;
    use crate::handler::ClusterHandler;

    const CELL: (u16, u16) = (10, 20);

    struct Sinker {
        hub: Hub,
        answers: Mutex<Vec<u8>>,
        pixel_mouse: AtomicBool,
    }

    impl Sink for Sinker {
        fn hub(&self) -> &Hub {
            &self.hub
        }

        fn cell(&self) -> (u16, u16) {
            CELL
        }

        fn reply(&self, bytes: Vec<u8>) {
            self.answers.lock().expect("lock").extend(bytes);
        }

        fn pixel_mouse(&self) -> &AtomicBool {
            &self.pixel_mouse
        }
    }

    /// What a session does with the bytes a program writes, minus the PTY:
    /// the reader's separation, the decoding, and a real grid behind the
    /// parser with the marker's handler.
    struct Screenful {
        intake: Intake,
        sink: Sinker,
        term: Term<VoidListener>,
        parser: Processor,
        last_input: bool,
        /// What the parser was given: the stream without the commands.
        stream: Vec<u8>,
    }

    fn size(columns: usize, lines: usize) -> TermSize {
        TermSize::new(columns, lines)
    }

    impl Screenful {
        fn new(columns: usize, lines: usize, history: usize) -> Self {
            let config = Config {
                scrolling_history: history,
                ..Config::default()
            };
            Self {
                intake: Intake::new(),
                sink: Sinker {
                    hub: Hub::new(),
                    answers: Mutex::new(Vec::new()),
                    pixel_mouse: AtomicBool::new(false),
                },
                term: Term::new(config, &size(columns, lines), VoidListener),
                parser: Processor::new(),
                last_input: false,
                stream: Vec::new(),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let env = Env {
                cell: CELL,
                columns: u16::try_from(self.term.columns()).expect("columns"),
                lines: u16::try_from(self.term.screen_lines()).expect("lines"),
            };
            let hub = &self.sink.hub;
            let mut buf = vec![0u8; 1 << 20];
            let mut out = Vec::new();
            for chunk in bytes.chunks(buf.len()) {
                let now = Instant::now();
                out.extend(turn(
                    &mut self.intake,
                    hub,
                    &mut buf,
                    chunk,
                    now,
                    env,
                    &mut |_| {},
                ));
            }
            hub.decode_pending();
            self.stream.extend_from_slice(&out);
            let mut handler = ClusterHandler::new(&mut self.term, true, &mut self.last_input)
                .graphics(Some(&self.sink));
            self.parser.advance(&mut handler, &out);
        }

        /// Brings the placements to the grid as it is, as a frame does.
        fn look(&self) {
            self.sink.hub.look(&self.term, CELL);
        }

        fn placed(&self) -> Vec<(u32, i32, u16)> {
            self.look();
            self.sink.hub.with_images(|images| images.main().placed())
        }

        fn placed_alt(&self) -> Vec<(u32, i32, u16)> {
            self.look();
            self.sink.hub.with_images(|images| images.alt().placed())
        }

        fn cursor(&self) -> (i32, usize) {
            let point = self.term.grid().cursor.point;
            (point.line.0, point.column.0)
        }

        fn history(&self) -> usize {
            self.term.history_size()
        }
    }

    /// `n` line feeds.
    fn lines(n: usize) -> Vec<u8> {
        b"\r\n".repeat(n)
    }

    fn at(line: usize, col: usize) -> Vec<u8> {
        format!("\x1b[{};{}H", line + 1, col + 1).into_bytes()
    }

    fn tile_command(control: &str) -> Vec<u8> {
        wire(control, &tile())
    }

    #[test]
    fn the_cursor_goes_to_the_cell_after_the_image_on_its_last_row() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&at(3, 5));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1"));
        assert_eq!(screen.placed(), [(1, 3, 5)]);
        assert_eq!(screen.cursor(), (4, 7), "two columns across, one row down");
        // The cursor that is asked to stay stays.
        screen.feed(&at(0, 0));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=2,C=1"));
        assert_eq!(screen.cursor(), (0, 0));
        assert_eq!(screen.placed()[1], (2, 0, 0));
    }

    #[test]
    fn an_image_below_the_bottom_scrolls_the_screen_and_keeps_its_place() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&at(8, 0));
        // Four rows from line 8: two of them below the screen.
        screen.feed(&wire("a=T,f=32,s=10,v=80,i=1", &rgba(10, 80, 0)));
        assert_eq!(screen.history(), 2, "the screen scrolled by two");
        assert_eq!(screen.cursor(), (9, 1), "on the last row of the image");
        assert_eq!(screen.placed(), [(1, 6, 0)], "its top is three rows above");
    }

    #[test]
    fn a_synchronized_update_places_where_the_cursor_is_when_the_frame_ends() {
        let mut screen = Screenful::new(20, 10, 100);
        let mut update = b"\x1b[?2026h".to_vec();
        update.extend(at(2, 3));
        update.extend(tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        update.extend(b"\x1b[?2026l");
        screen.feed(&update);
        assert_eq!(screen.placed(), [(1, 2, 3)]);
        // And with the movement: the top is one row above the last one.
        let mut update = b"\x1b[?2026h".to_vec();
        update.extend(at(5, 6));
        update.extend(tile_command("a=T,f=32,s=20,v=40,i=2"));
        update.extend(b"\x1b[?2026l");
        screen.feed(&update);
        assert_eq!(screen.placed()[1], (2, 5, 6));
    }

    #[test]
    fn a_clear_in_the_same_read_deletes_what_came_before_it_and_not_what_comes_after() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        let mut read = b"\x1b[2J".to_vec();
        read.extend(at(4, 4));
        read.extend(tile_command("a=T,f=32,s=20,v=40,i=2,C=1"));
        screen.feed(&read);
        assert_eq!(screen.placed(), [(2, 4, 4)]);
    }

    #[test]
    fn the_journals_copy_of_the_screen_has_the_cursor_without_the_images() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(b"one\r\ntwo");
        screen.feed(&at(8, 3));
        screen.feed(&wire("a=T,f=32,s=10,v=80,i=1", &rgba(10, 80, 0)));
        // The copy reads the stream the journal recorded: no APC in it, the
        // marker read and left alone.
        assert!(!String::from_utf8_lossy(&screen.stream).contains("_G"));
        let mut copy = Term::new(
            Config {
                scrolling_history: 100,
                ..Config::default()
            },
            &size(20, 10),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        let mut last_input = false;
        parser.advance(
            &mut ClusterHandler::new(&mut copy, true, &mut last_input),
            &screen.stream,
        );
        let point = |term: &Term<VoidListener>| term.grid().cursor.point;
        assert_eq!(point(&copy), point(&screen.term));
        assert_eq!(copy.history_size(), screen.term.history_size());
        for line in -2..10 {
            let row = |term: &Term<VoidListener>| -> String {
                term.grid()[Line(line)]
                    .into_iter()
                    .map(|cell| cell.c)
                    .collect()
            };
            assert_eq!(row(&copy), row(&screen.term), "line {line}");
        }
    }

    #[test]
    fn a_placement_moves_up_with_the_output() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&at(3, 5));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1"));
        assert_eq!(screen.placed(), [(1, 3, 5)]);
        // The cursor is at line 4; five line feeds reach the bottom and seven
        // more scroll the screen.
        screen.feed(&lines(12));
        assert_eq!(screen.history(), 7);
        assert_eq!(screen.placed(), [(1, -4, 5)]);
    }

    #[test]
    fn a_placement_that_has_gone_up_off_the_history_is_gone() {
        let mut screen = Screenful::new(20, 10, 5);
        screen.feed(&at(3, 5));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1"));
        screen.feed(&lines(40));
        assert_eq!(screen.placed(), []);
        // Its image stays for the program to place again.
        assert_eq!(
            screen.sink.hub.with_images(|images| images.main().ids()),
            [1]
        );
    }

    #[test]
    fn at_the_bottom_of_a_full_history_a_placement_goes_up_with_the_output() {
        let mut screen = Screenful::new(20, 10, 10);
        // Fill the history: the text of every line is its own.
        for line in 0..60 {
            screen.feed(format!("line {line}\r\n").as_bytes());
        }
        assert_eq!(screen.history(), 10, "the history is full");
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1"));
        let before = screen.placed()[0].1;
        screen.feed(&lines(3));
        assert_eq!(screen.placed()[0].1, before - 3);
        // And much further, but not past the ring.
        screen.feed(&lines(5));
        assert_eq!(screen.placed()[0].1, before - 8);
    }

    #[test]
    fn a_placement_on_the_main_screen_waits_for_it_through_the_alternate_one() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&at(5, 5));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        screen.feed(b"\x1b[?1049h");
        screen.feed(&at(2, 2));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=7,C=1"));
        assert_eq!(screen.placed_alt(), [(7, 2, 2)]);
        screen.feed(b"\x1b[?1049l");
        assert_eq!(screen.placed_alt(), []);
        assert_eq!(screen.placed(), [(1, 5, 5)], "where it was");
    }

    #[test]
    fn a_placement_on_the_alternate_screen_scrolls_with_it() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(b"\x1b[?1049h");
        screen.feed(&at(5, 0));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        assert_eq!(screen.placed_alt(), [(1, 5, 0)]);
        // From line 5 to the bottom, then two more: the screen scrolls by two.
        screen.feed(&lines(4 + 2));
        assert_eq!(screen.history(), 0, "there is no history to look in");
        assert_eq!(screen.placed_alt(), [(1, 3, 0)]);
        // Wholly above the screen, it is gone.
        screen.feed(&lines(5));
        assert_eq!(screen.placed_alt(), []);
    }

    #[test]
    fn clearing_the_screen_takes_the_placements_that_reach_onto_it() {
        let mut screen = Screenful::new(20, 10, 100);
        // One that has gone up into the history, one on the screen.
        screen.feed(&at(0, 0));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        screen.feed(&lines(30));
        assert_eq!(screen.placed(), [(1, -21, 0)], "wholly in the history");
        screen.feed(&at(2, 2));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=2,C=1"));
        assert_eq!(screen.placed().len(), 2);
        // Erasing part of it does not touch an image.
        screen.feed(b"\x1b[J\x1b[1J\x1b[2K");
        assert_eq!(screen.placed().len(), 2);
        screen.feed(b"\x1b[2J");
        assert_eq!(screen.placed().len(), 1, "the one in the history stays");
        screen.feed(&at(2, 2));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=3,C=1"));
        screen.feed(b"\x1b[3J");
        assert_eq!(screen.placed(), [], "ED 3 takes all of them");
    }

    #[test]
    fn a_reset_takes_the_images_too() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        screen.feed(b"\x1b[?1049h");
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=2,C=1"));
        screen.feed(b"\x1bc");
        assert_eq!(screen.placed(), []);
        assert_eq!(screen.placed_alt(), []);
        screen.sink.hub.with_images(|images| {
            assert!(images.main().ids().is_empty() && images.alt().ids().is_empty());
        });
    }

    /// The bytes a program could write to be taken for a marker: ours with
    /// the ids it might guess, and each of them with one byte the parser acts
    /// on or steps over put in at every place.
    fn forged() -> Vec<Vec<u8>> {
        let mut forged = Vec::new();
        for id in [0, 1, 2, 3, 0x0fff_ffff] {
            for motion in [
                Motion::NONE,
                Motion {
                    rows: 3,
                    cols: 2,
                    wrap: false,
                },
            ] {
                let mut marker = Vec::new();
                motion.mark(u16::MAX, id, &mut marker);
                forged.push(marker);
            }
        }
        let mut padded = Vec::new();
        for marker in &forged[..2] {
            for at in 1..marker.len() {
                for extra in (0u8..0x20).chain([0x7f, 0x80, 0xff]) {
                    padded.push([&marker[..at], &[extra], &marker[at..]].concat());
                }
            }
        }
        forged.extend(padded);
        forged.extend([b"\x1b[?8166h".to_vec(), b"\x1b[?8166;8166h".to_vec()]);
        forged
    }

    #[test]
    fn a_marker_the_program_wrote_takes_no_record_and_the_real_one_takes_its_own() {
        // Through the splitter and the real parser, with a record waiting
        // (any marker dispatched with its id would take it): none does. Then
        // a real command is read, and its marker finds its record.
        for variant in forged() {
            let mut screen = Screenful::new(20, 5, 0);
            screen
                .sink
                .hub
                .queue_raw(777, "a=t,f=32,s=1,v=1,i=1", b"AAAAAA==");
            screen.feed(&variant);
            assert_eq!(screen.sink.hub.waiting_records(), 1, "{variant:?}");
        }
        let mut screen = Screenful::new(20, 5, 0);
        screen
            .sink
            .hub
            .queue_raw(777, "a=t,f=32,s=1,v=1,i=1", b"AAAAAA==");
        let mut own = Vec::new();
        Motion::NONE.mark(5, 777, &mut own);
        screen.feed(&own);
        assert_eq!(screen.sink.hub.waiting_records(), 0, "a marker with its id");
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=2,C=1"));
        assert_eq!(screen.placed(), [(2, 0, 0)]);
    }

    #[test]
    fn a_marker_the_program_wrote_moves_the_cursor_by_no_more_than_a_screen() {
        // Live, and in a replay with no sink: the same rows, no more than the
        // screen has, whatever the marker says.
        let mut marker = Vec::new();
        Motion {
            rows: u32::MAX,
            cols: 0,
            wrap: false,
        }
        .mark(u16::MAX, 9, &mut marker);
        let mut screen = Screenful::new(20, 5, 100);
        screen.feed(&marker);
        assert_eq!(screen.cursor(), (4, 0));
        assert!(screen.history() <= 5, "{} lines scrolled", screen.history());
        let mut term = Term::new(Config::default(), &size(20, 5), VoidListener);
        let mut parser: Processor = Processor::new();
        let mut last_input = false;
        parser.advance(
            &mut ClusterHandler::new(&mut term, true, &mut last_input),
            &marker,
        );
        assert_eq!(term.grid().cursor.point.line.0, 4);
        assert!(term.history_size() <= 5);
    }

    #[test]
    fn a_replay_with_no_command_to_apply_moves_the_cursor_all_the_same() {
        // The journal's copy and a handover's replay have no sink: the
        // marker applies nothing, and its movement is the stream's.
        let mut term = Term::new(Config::default(), &size(20, 10), VoidListener);
        let mut parser: Processor = Processor::new();
        let mut last_input = false;
        let mut run = |term: &mut Term<VoidListener>, bytes: &[u8]| {
            parser.advance(&mut ClusterHandler::new(term, true, &mut last_input), bytes);
            let point = term.grid().cursor.point;
            (point.line.0, point.column.0)
        };
        let (line, column) = run(&mut term, &at(2, 2));
        // One down and two right.
        assert_eq!(run(&mut term, &marker(1, 2)), (line + 1, column + 2));
        // An image as wide as the screen: down a line more, to its start.
        assert_eq!(run(&mut term, &wrapped(0)), (line + 2, 0));
        assert_eq!(
            run(&mut term, &mark()),
            (line + 2, 0),
            "a plain marker stays"
        );
    }

    #[test]
    fn a_marker_a_program_wrote_itself_does_not_move_a_placement() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&at(2, 2));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        screen.feed(&mark());
        screen.feed(&mark());
        assert_eq!(screen.placed(), [(1, 2, 2)]);
    }

    fn answers(screen: &Screenful) -> Vec<u8> {
        std::mem::take(&mut screen.sink.answers.lock().expect("lock"))
    }

    #[test]
    fn the_pixel_mouse_flag_follows_the_stream_and_the_mode_is_answered() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(b"\x1b[?1016$p");
        assert_eq!(answers(&screen), b"\x1b[?1016;2$y", "off: reset");
        screen.feed(b"\x1b[?1016h");
        assert!(screen.sink.pixel_mouse.load(Ordering::Acquire));
        screen.feed(b"\x1b[?1016$p");
        assert_eq!(answers(&screen), b"\x1b[?1016;1$y", "on: set");
        screen.feed(b"\x1b[?1016l");
        assert!(!screen.sink.pixel_mouse.load(Ordering::Acquire));
        // A reset takes the mode with the rest.
        screen.feed(b"\x1b[?1016h\x1bc");
        assert!(!screen.sink.pixel_mouse.load(Ordering::Acquire));
        // The other mouse modes are alacritty's and do not move it.
        screen.feed(b"\x1b[?1016h\x1b[?1000h\x1b[?1006h\x1b[?1000l");
        assert!(screen.sink.pixel_mouse.load(Ordering::Acquire));
    }

    #[test]
    fn a_replay_keeps_the_mode_in_its_own_flag_and_answers_nobody() {
        let flag = AtomicBool::new(false);
        let mut term = Term::new(Config::default(), &size(20, 10), VoidListener);
        let mut parser: Processor = Processor::new();
        let mut last_input = false;
        parser.advance(
            &mut ClusterHandler::new(&mut term, true, &mut last_input).pixel_mouse(&flag),
            b"\x1b[?1016h\x1b[?1016$p",
        );
        assert!(flag.load(Ordering::Acquire));
        // Without a flag to keep the mode is read and does nothing.
        let mut bare = Term::new(Config::default(), &size(20, 10), VoidListener);
        parser.advance(
            &mut ClusterHandler::new(&mut bare, true, &mut last_input),
            b"\x1b[?1016h",
        );
    }
}

// ─── files and shared memory ────────────────────────────────────────────

/// The whole answer to a command whose file or object could not be used: the
/// same words whatever the reason.
fn unreadable(id: u32) -> Vec<u8> {
    format!("\x1b_Gi={id};EBADF:Failed to read image file\x1b\\").into_bytes()
}

fn unique(stem: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!(
        "{stem}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A file for a test to name; it is removed when the guard goes.
struct Scratch(PathBuf);

impl Scratch {
    fn in_dir(dir: &Path, stem: &str, bytes: &[u8]) -> Self {
        let path = dir.join(unique(stem));
        std::fs::write(&path, bytes).expect("a scratch file");
        Self(path)
    }

    /// In the temporary directory, where a program leaves what it sends.
    fn temp(stem: &str, bytes: &[u8]) -> Self {
        Self::in_dir(&std::env::temp_dir(), stem, bytes)
    }

    /// A file that is in no temporary directory.
    fn elsewhere(stem: &str, bytes: &[u8]) -> Self {
        Self::in_dir(Path::new(env!("CARGO_MANIFEST_DIR")), stem, bytes)
    }

    fn name(&self) -> &[u8] {
        self.0.as_os_str().as_bytes()
    }

    fn exists(&self) -> bool {
        std::fs::symlink_metadata(&self.0).is_ok()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A shared memory object holding `bytes`, made the way a program that sends
/// pixels this way makes one. Removed when the guard goes, if it is still
/// there.
struct Object(String);

impl Object {
    fn new(bytes: &[u8]) -> Self {
        let name = format!("/{}", unique("bt-gfx"));
        let c_name = CString::new(name.as_str()).expect("a name");
        // SAFETY: `c_name` is NUL-terminated and outlives the calls; the
        // object is created, sized, written through a mapping of exactly
        // its size and closed here, and the mapping is released before the
        // function returns.
        unsafe {
            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            );
            assert!(fd >= 0, "shm_open: {}", std::io::Error::last_os_error());
            assert_eq!(libc::ftruncate(fd, bytes.len() as libc::off_t), 0);
            let map = libc::mmap(
                std::ptr::null_mut(),
                bytes.len(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert_ne!(map, libc::MAP_FAILED, "mmap");
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), map.cast::<u8>(), bytes.len());
            libc::munmap(map, bytes.len());
            libc::close(fd);
        }
        Self(name)
    }

    fn name(&self) -> &[u8] {
        self.0.as_bytes()
    }

    fn exists(&self) -> bool {
        let c_name = CString::new(self.0.as_str()).expect("a name");
        // SAFETY: a read-only open of a NUL-terminated name, closed at once.
        unsafe {
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0);
            if fd >= 0 {
                libc::close(fd);
            }
            fd >= 0
        }
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        let c_name = CString::new(self.0.as_str()).expect("a name");
        // SAFETY: a NUL-terminated name.
        unsafe { libc::shm_unlink(c_name.as_ptr()) };
    }
}

#[test]
fn a_file_is_read_where_it_lies_and_left_there() {
    let pixels = rgba(2, 1, 9);
    let file = Scratch::temp("bt-image", &pixels);
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire("a=t,t=f,f=32,s=2,v=1,i=7", file.name()));
    assert_eq!(stream, mark());
    assert_eq!(answers, ok(7));
    assert_eq!(rig.pixels(7), Some(pixels));
    assert!(file.exists(), "t=f does not delete");
}

#[test]
fn a_temporary_file_is_read_and_deleted() {
    let pixels = rgba(2, 1, 4);
    let file = Scratch::temp("tty-graphics-protocol-image", &pixels);
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("a=t,t=t,f=32,s=2,v=1,i=7", file.name()));
    assert_eq!(answers, ok(7));
    assert_eq!(rig.pixels(7), Some(pixels));
    assert!(!file.exists(), "t=t deletes once it is read");
}

#[test]
fn rgb_zlib_and_png_come_out_of_a_file_as_they_do_out_of_the_stream() {
    let mut rig = Rig::new();
    let rgb = Scratch::temp("bt-image", &[10, 20, 30, 40, 50, 60]);
    rig.run(&wire("a=t,t=f,f=24,s=2,v=1,i=1", rgb.name()));
    assert_eq!(rig.pixels(1), Some(vec![10, 20, 30, 255, 40, 50, 60, 255]));
    let packed = Scratch::temp("bt-image", &zlib(&rgba(8, 8, 1)));
    rig.run(&wire("a=t,t=f,f=32,o=z,s=8,v=8,i=2", packed.name()));
    assert_eq!(rig.pixels(2), Some(rgba(8, 8, 1)));
    let png = png_file(
        2,
        2,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        None,
        None,
        &rgba(2, 2, 5),
    );
    let file = Scratch::temp("bt-image", &png);
    rig.run(&wire("a=t,t=f,f=100,i=3", file.name()));
    assert_eq!(rig.pixels(3), Some(rgba(2, 2, 5)));
}

#[test]
fn a_named_png_moves_the_cursor_by_its_keys_and_never_by_its_file() {
    let png = png_file(
        30,
        40,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        None,
        None,
        &rgba(30, 40, 3),
    );
    // 30 × 40 pixels are three columns by two rows.
    for (name, medium) in [("bt-image", "f"), ("tty-graphics-protocol-image", "t")] {
        let file = Scratch::temp(name, &png);
        let mut rig = Rig::new();
        let (stream, _) = rig.run(&wire(
            &format!("a=T,t={medium},f=100,s=30,v=40,i=1"),
            file.name(),
        ));
        assert_eq!(stream, marker(1, 3), "t={medium}");
        assert_eq!(placed(&rig), [(1, -1, 0)], "t={medium}");
        // The keys are the program's word and the file is not looked at: a
        // wrong size moves the cursor by the wrong size.
        let mut rig = Rig::new();
        let (stream, _) = rig.run(&wire(
            &format!("a=T,t={medium},f=100,s=10,v=20,i=1"),
            file.name(),
        ));
        assert_eq!(stream, marker(0, 1), "t={medium}");
    }
    // Without them the cursor stays: the size is in the file, and the file is
    // read long after the stream moved on.
    let file = Scratch::temp("bt-image", &png);
    let mut rig = Rig::new();
    let (stream, _) = rig.run(&wire("a=T,t=f,f=100,i=1", file.name()));
    assert_eq!(stream, mark());
    // And the file is not even opened to find out.
    let temp = Scratch::temp("tty-graphics-protocol-image", &png);
    queue_only(&mut rig, &wire("a=T,t=t,f=100,i=2", temp.name()));
    assert!(temp.exists());
}

#[test]
fn what_kitten_icat_sends_for_a_file_moves_the_cursor_by_the_size_it_says() {
    // `kitten icat photo.png` as it goes out: the file, its kind and its size
    // — for a PNG too — and nothing about the cursor.
    let png = tall_png();
    let file = Scratch::temp("bt-image", &png);
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire("a=T,t=f,f=100,s=30,v=40,i=7,q=2", file.name()));
    assert_eq!(stream, marker(1, 3));
    assert!(answers.is_empty(), "q=2 asked not to hear");
    assert_eq!(rig.pixels(7), Some(rgba(30, 40, 3)));
    assert!(file.exists());
    // And through a temporary file it made for the occasion.
    let temp = Scratch::temp("tty-graphics-protocol-icat", &png);
    let (stream, _) = rig.run(&wire("a=T,t=t,f=100,s=30,v=40,i=8", temp.name()));
    assert_eq!(stream, marker(1, 3));
    assert!(!temp.exists());
}

#[test]
fn a_file_is_not_touched_while_the_stream_is_read_only_when_it_is_decoded() {
    let file = Scratch::temp("tty-graphics-protocol-image", &rgba(1, 1, 2));
    let mut rig = Rig::new();
    queue_only(&mut rig, &wire("a=t,t=t,f=32,s=1,v=1,i=3", file.name()));
    assert!(rig.hub.has_undecoded());
    assert!(file.exists(), "nothing is read or deleted in the read");
    rig.hub.decode_pending();
    assert!(!rig.hub.has_undecoded());
    assert!(!file.exists(), "the decode read it");
    let answers = rig.hub.apply_next(&rig.screen()).unwrap_or_default();
    assert_eq!(answers, ok(3));
}

#[test]
fn part_of_a_file_is_read_by_size_and_offset() {
    let pixels = rgba(2, 1, 7);
    let mut bytes = vec![0xee; 16];
    bytes.extend_from_slice(&pixels);
    bytes.extend_from_slice(&[0xdd; 5]);
    let file = Scratch::temp("bt-image", &bytes);
    let mut rig = Rig::new();
    rig.run(&wire("a=t,t=f,f=32,s=2,v=1,O=16,S=8,i=1", file.name()));
    assert_eq!(rig.pixels(1), Some(pixels.clone()));
    // Without a size the read goes to the end, and a raw image takes only
    // what its dimensions call for: the five bytes after it are not read.
    rig.run(&wire("a=t,t=f,f=32,s=2,v=1,O=16,i=2", file.name()));
    assert_eq!(rig.pixels(2), Some(pixels));
    // Too little is the same answer as no file at all, with no number in it.
    for control in [
        "a=t,t=f,f=32,s=2,v=1,O=16,S=4,i=3",
        "a=t,t=f,f=32,s=2,v=1,O=1000,i=3",
        "a=t,t=f,f=32,s=40,v=40,i=3",
    ] {
        let (_, answers) = rig.run(&wire(control, file.name()));
        assert_eq!(answers, unreadable(3), "{control}");
    }
}

#[test]
fn a_temporary_file_that_may_not_be_taken_is_refused_and_left_alone() {
    let pixels = rgba(1, 1, 1);
    let mut rig = Rig::new();
    let control = "a=t,t=t,f=32,s=1,v=1,i=5";
    // In the temporary directory but not named for the protocol.
    let plain = Scratch::temp("bt-image", &pixels);
    // Named for it but in a directory that is not a temporary one.
    let away = Scratch::elsewhere("tty-graphics-protocol-image", &pixels);
    // Named for it, in the temporary directory, but a link to a file that
    // is neither.
    let target = Scratch::elsewhere("bt-image", &pixels);
    let link = Scratch(std::env::temp_dir().join(unique("tty-graphics-protocol-link")));
    std::os::unix::fs::symlink(&target.0, &link.0).expect("a link");
    for file in [&plain, &away, &link] {
        let (_, answers) = rig.run(&wire(control, file.name()));
        assert_eq!(answers, unreadable(5), "{:?}", file.0);
        assert!(file.exists(), "{:?} was touched", file.0);
    }
    assert!(target.exists());
    assert!(rig.main_ids().is_empty());
    // The same file, asked for as an ordinary file, is read; the temporary
    // directory is no reason to refuse it.
    let (_, answers) = rig.run(&wire("a=t,t=f,f=32,s=1,v=1,i=5", plain.name()));
    assert_eq!(answers, ok(5));
}

#[test]
fn what_a_name_comes_to_after_it_was_judged_is_judged_again() {
    // A directory in the name is made a link to another one between the
    // judging of the name and the open of it: what is opened is a file where
    // a temporary file may not be, and the open handle says so. It is not
    // read, and not deleted.
    let dir = std::env::temp_dir().join(unique("tty-graphics-dir"));
    std::fs::create_dir(&dir).expect("a directory");
    let leaf = "tty-graphics-protocol-swapped";
    std::fs::write(dir.join(leaf), rgba(1, 1, 1)).expect("a file");
    let outside = Path::new(env!("CARGO_MANIFEST_DIR")).join(unique("bt-outside"));
    std::fs::create_dir(&outside).expect("a directory");
    std::fs::write(outside.join(leaf), rgba(1, 1, 2)).expect("a file");
    let name = dir.join(leaf).as_os_str().as_bytes().to_vec();
    let (moved, link, target) = (dir.with_extension("moved"), dir.clone(), outside.clone());
    let swapping = name.clone();
    source::on_open(Some(Box::new(move |named| {
        if named == swapping {
            let _ = std::fs::rename(&link, &moved);
            let _ = std::os::unix::fs::symlink(&target, &link);
        }
    })));
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("a=t,t=t,f=32,s=1,v=1,i=1", &name));
    source::on_open(None);
    assert_eq!(answers, unreadable(1));
    assert!(
        outside.join(leaf).exists(),
        "what the open handle was is deleted"
    );
    assert!(rig.main_ids().is_empty());
    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_dir_all(dir.with_extension("moved"));
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn a_file_the_protocol_may_not_name_is_refused_before_it_is_opened() {
    let mut rig = Rig::new();
    let names: [&[u8]; 7] = [
        b"/dev/null",
        b"/dev/zero",
        b"/dev/stdin",
        b"/proc/self/status",
        b"/sys/kernel",
        b"relative/image.png",
        b"~/image.png",
    ];
    for name in names {
        let (_, answers) = rig.run(&wire("a=t,t=f,f=32,s=1,v=1,i=1", name));
        assert_eq!(answers, unreadable(1), "{}", String::from_utf8_lossy(name));
    }
    // A pipe is no file, and opening it frees a writer waiting for a reader:
    // the writer here stays where it was.
    let fifo = std::env::temp_dir().join(unique("bt-fifo"));
    let c_path = CString::new(fifo.as_os_str().as_bytes()).expect("a path");
    // SAFETY: a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let released = Arc::new(AtomicBool::new(false));
    let writer = {
        let (fifo, released) = (fifo.clone(), Arc::clone(&released));
        std::thread::spawn(move || {
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
            released.store(true, Ordering::Release);
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    let (_, answers) = rig.run(&wire(
        "a=t,t=f,f=32,s=1,v=1,i=1",
        fifo.as_os_str().as_bytes(),
    ));
    assert_eq!(answers, unreadable(1));
    std::thread::sleep(Duration::from_millis(100));
    assert!(!released.load(Ordering::Acquire), "the writer was let go");
    // (Opened for reading, the pipe lets the writer go.)
    let _reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo);
    writer.join().expect("the writer");
    let _ = std::fs::remove_file(&fifo);
    // A descriptor of the terminal's own is no name for a file: the pipe
    // keeps what was put in it, which nothing read.
    let mut ends = [0; 2];
    // SAFETY: `ends` has room for the two descriptors `pipe` writes.
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
    // SAFETY: the descriptors were just made and belong to these two files.
    let (reader, mut writer) = unsafe {
        (
            std::fs::File::from_raw_fd(ends[0]),
            std::fs::File::from_raw_fd(ends[1]),
        )
    };
    std::io::Write::write_all(&mut writer, b"kept").expect("a pipe");
    let name = format!("/dev/fd/{}", ends[0]);
    let (_, answers) = rig.run(&wire("a=t,t=f,f=32,s=1,v=1,i=1", name.as_bytes()));
    assert_eq!(answers, unreadable(1));
    let mut kept = [0; 4];
    // SAFETY: a read into a buffer of that size.
    let got = unsafe { libc::read(ends[0], kept.as_mut_ptr().cast(), kept.len()) };
    assert_eq!((got, &kept), (4, b"kept"), "something read the pipe");
    drop((reader, writer));
    // A directory is no file either.
    let (_, answers) = rig.run(&wire(
        "a=t,t=f,f=32,s=1,v=1,i=1",
        std::env::temp_dir().as_os_str().as_bytes(),
    ));
    assert_eq!(answers, unreadable(1));
    assert!(rig.main_ids().is_empty());
}

#[test]
fn what_is_in_an_answer_is_never_the_programs_own_text() {
    // An answer is written into the PTY's input: a name repeated in it would
    // be typed into whatever reads that.
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire(
        "a=t,t=f,f=32,s=1,v=1,i=1",
        b"/nonexistent/evil\n; touch pwned",
    ));
    assert_eq!(answers, unreadable(1));
    let text = error_of(&answers);
    assert!(
        !text.contains("evil") && !text.contains("pwned"),
        "{text:?}"
    );
}

#[test]
fn a_name_that_cannot_be_one_is_refused_where_it_is_read() {
    let mut rig = Rig::new();
    let long = vec![b'a'; NAME_LIMIT + 1];
    let names: [&[u8]; 3] = [b"", b"/tmp/a\0b", &long];
    for name in names {
        let (_, answers) = rig.run(&wire("a=t,t=f,f=32,s=1,v=1,i=1", name));
        let text = error_of(&answers);
        assert!(text.starts_with("\x1b_Gi=1;EINVAL:"), "{text:?}");
    }
    let (_, answers) = rig.run(b"\x1b_Ga=t,t=f,f=32,s=1,v=1,i=1;not base64!\x1b\\");
    assert!(error_of(&answers).contains("EINVAL"));
    // A name past what the splitter keeps is cut and refused, not read.
    let (_, answers) = rig.run(&wire(
        "a=t,t=f,f=32,s=1,v=1,i=1",
        &vec![b'a'; NAME_LIMIT * 3],
    ));
    assert!(error_of(&answers).contains("EINVAL"));
    assert!(rig.main_ids().is_empty());
}

#[test]
fn a_name_is_whole_in_its_command_and_m_does_not_wait_for_more() {
    let file = Scratch::temp("bt-image", &rgba(1, 1, 3));
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("a=t,t=f,f=32,s=1,v=1,m=1,i=2", file.name()));
    assert_eq!(answers, ok(2));
    assert!(!rig.intake.receiver.is_open());
}

#[test]
fn an_image_too_big_to_read_is_refused_without_touching_its_file() {
    let file = Scratch::temp("tty-graphics-protocol-image", &[0; 16]);
    let mut rig = Rig::new();
    // 320 MB of pixels fits the quota but not a transmission.
    let (_, answers) = rig.run(&wire("a=t,t=t,f=32,s=10000,v=8000,i=1", file.name()));
    assert!(
        error_of(&answers).starts_with("\x1b_Gi=1;EFBIG:"),
        "{answers:?}"
    );
    assert!(file.exists(), "refused before the file was looked at");
}

#[test]
fn shared_memory_is_read_and_removed() {
    let pixels = rgba(2, 1, 6);
    let object = Object::new(&pixels);
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire("a=t,t=s,f=32,s=2,v=1,i=4", object.name()));
    assert_eq!(stream, mark());
    assert_eq!(answers, ok(4));
    assert_eq!(rig.pixels(4), Some(pixels));
    assert!(!object.exists(), "t=s removes the object once it is read");
}

#[test]
fn shared_memory_is_read_from_an_offset_far_past_a_page() {
    // Further in than any page is long, and not on a page boundary.
    let at = 16384 + 24;
    let pixels = rgba(2, 1, 8);
    let mut bytes = vec![0xaa; at];
    bytes.extend_from_slice(&pixels);
    let object = Object::new(&bytes);
    let mut rig = Rig::new();
    let control = format!("a=t,t=s,f=32,s=2,v=1,O={at},S=8,i=1");
    let (_, answers) = rig.run(&wire(&control, object.name()));
    assert_eq!(answers, ok(1));
    assert_eq!(rig.pixels(1), Some(pixels));
}

#[test]
fn an_object_that_cannot_be_used_is_removed_all_the_same() {
    // More pixels than the object has: even where an object is rounded up to
    // a page, 200 × 200 of them are more than one.
    let object = Object::new(&[1, 2, 3]);
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("a=t,t=s,f=32,s=200,v=200,i=1", object.name()));
    assert_eq!(answers, unreadable(1));
    assert!(!object.exists(), "the program handed it over");
    // One that is not there, and names that are not names of objects.
    for name in [&b"/bt-gfx-none"[..], b"no-slash", b"/a/b", b"/", b"/.."] {
        let (_, answers) = rig.run(&wire("a=t,t=s,f=32,s=2,v=1,i=1", name));
        assert_eq!(answers, unreadable(1), "{}", String::from_utf8_lossy(name));
    }
    assert!(rig.main_ids().is_empty());
}

#[test]
fn a_query_that_names_its_pixels_is_answered_at_its_marker_and_keeps_nothing() {
    let mut rig = Rig::new();
    let plain = Scratch::temp("bt-image", &[1, 2, 3]);
    let (stream, answers) = rig.run(&wire("i=9,a=q,t=f,f=24,s=1,v=1", plain.name()));
    assert_eq!(stream, mark(), "it waits for its place in the stream");
    assert_eq!(answers, ok(9));
    assert!(plain.exists());
    let temp = Scratch::temp("tty-graphics-protocol-image", &[1, 2, 3]);
    let (_, answers) = rig.run(&wire("i=9,a=q,t=t,f=24,s=1,v=1", temp.name()));
    assert_eq!(answers, ok(9));
    assert!(!temp.exists(), "a query takes a temporary file too");
    let object = Object::new(&[1, 2, 3]);
    let (_, answers) = rig.run(&wire("i=9,a=q,t=s,f=24,s=1,v=1", object.name()));
    assert_eq!(answers, ok(9));
    assert!(!object.exists());
    // What a terminal that cannot read it says, for the program to fall back.
    let (_, answers) = rig.run(&wire("i=9,a=q,t=f,f=24,s=1,v=1", b"/nonexistent/x"));
    assert_eq!(answers, unreadable(9));
    assert!(rig.queries.is_empty(), "none of these was answered early");
    assert!(rig.main_ids().is_empty(), "a query stores nothing");
}

#[test]
fn a_query_that_names_a_file_asks_about_the_whole_of_it() {
    // A query does not have a size of its own: it asks whether this image can
    // be shown, however big it is.
    let file = Scratch::temp("bt-image", &rgba(200, 200, 1));
    let mut rig = Rig::new();
    let (_, answers) = rig.run(&wire("i=2,a=q,t=f,f=32,s=200,v=200", file.name()));
    assert_eq!(answers, ok(2));
    assert!(rig.main_ids().is_empty());
}

#[test]
fn whatever_is_wrong_with_a_named_file_is_one_answer() {
    // Missing, corrupt, short, the wrong format: told apart, they would say
    // which files exist and what is in them.
    let junk = Scratch::temp("bt-image", b"not an image at all, not even close");
    let short = Scratch::temp("bt-image", &[1, 2, 3]);
    let mut rig = Rig::new();
    let cases = [
        ("a=t,t=f,f=32,s=2,v=1,i=1", b"/nonexistent/file".as_slice()),
        ("a=t,t=f,f=100,i=1", junk.name()),
        ("a=t,t=f,f=32,o=z,s=2,v=1,i=1", junk.name()),
        ("a=t,t=f,f=32,s=2,v=1,i=1", short.name()),
        ("a=t,t=f,f=24,s=100,v=100,i=1", short.name()),
    ];
    for (control, name) in cases {
        let (_, answers) = rig.run(&wire(control, name));
        assert_eq!(answers, unreadable(1), "{control}");
    }
    assert!(rig.main_ids().is_empty());
}

// ─── what a read becomes ────────────────────────────────────────────────

/// A 30 × 40 pixel PNG: three columns and two rows at the test cell size.
fn tall_png() -> Vec<u8> {
    png_file(
        30,
        40,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        None,
        None,
        &rgba(30, 40, 3),
    )
}

#[test]
fn a_named_images_size_is_remembered_for_the_placement_that_follows_it() {
    // `a=p` of an image whose pixels a file held, in the same read: the size
    // the keys gave.
    let file = Scratch::temp("bt-image", &tall_png());
    let mut rig = Rig::new();
    let input = [
        wire("a=T,t=f,f=100,s=30,v=40,i=1,C=1", file.name()),
        wire("a=p,i=1", &[]),
    ]
    .concat();
    let (stream, _) = rig.run(&input);
    // The first leaves the cursor; the second moves it by the size.
    assert_eq!(stream, [mark(), marker(1, 3)].concat());
}

#[test]
fn a_read_of_many_small_commands_is_read_to_its_end_whatever_the_buffer() {
    // The stream grows: `ESC _ G a=p,i=1 ESC \` is twelve bytes and its marker
    // is 38. A read is offered the whole buffer, so what it comes to may not
    // fit: the rest is the next thing the read side returns, before the PTY
    // is read again, and the loop's turn does not end without it.
    let mut input = wire("a=t,f=32,s=30,v=40,i=1", &tile_30x40());
    // Shortest commands that carry a movement (a known image placed), the
    // shortest that carry none, and a transmission whose answer is a marker.
    let movers = wire("a=p,i=1", &[]);
    let stayers = [ESC_G, b"a=d", ST].concat();
    let bare = [ESC_G, ST].concat();
    for at in 0..300 {
        input.extend_from_slice(match at % 3 {
            0 => &movers,
            1 => &stayers,
            _ => &bare,
        });
        input.push(b"x\x1b"[at % 2]);
    }
    // What a buffer too big to need it makes of the same read.
    let mut whole = Rig::new();
    let mut big = vec![0u8; 1 << 20];
    let raw = whole.read(&mut big, &input, Instant::now());
    let reference = anonymous(&raw);
    assert!(markers(&reference) > 250, "{} markers", markers(&reference));
    // Each command has an id of its own, and none has the id a marker with
    // no command has.
    let mut all = ids(&raw);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), markers(&raw));
    assert!(!all.contains(&0));
    for room in [mark().len(), 64, 257, 4096] {
        let mut rig = Rig::new();
        let mut buf = vec![0u8; room];
        let got = rig.read(&mut buf, &input, Instant::now());
        // Nothing was dropped on the way, and nothing is out of order.
        assert_eq!(anonymous(&got), reference, "room {room}");
        assert!(!rig.hub.backlog(), "room {room}: stream left behind");
    }
}

#[test]
fn a_command_becomes_at_most_two_markers() {
    // A command is one marker, or none, and two when it ended a chunked
    // transmission that had run out of time (a failure) and has its own.
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=30,v=40,i=1", &tile_30x40()));
    let mut stream = Vec::new();
    let mut now = Instant::now();
    rig.run_at(
        &[ESC_G, b"a=t,f=32,s=1,v=1,i=6,m=1;AAAA", ST].concat(),
        &mut now,
        &mut stream,
    );
    let before = stream.len();
    now += OPEN_TIMEOUT + Duration::from_secs(1);
    // A placement with the biggest numbers a marker has: down a screen's worth
    // and, as wide as 9000 cells are, to the start of the next line.
    rig.run_at(&wire("a=p,i=1,c=9000,r=500", &[]), &mut now, &mut stream);
    assert_eq!(markers(&stream[before..]), 2);
    assert_eq!(stream.len() - before, 2 * mark().len());
    // Everything else a command can do ends in one marker, or none.
    for control in [
        "a=p,i=1,c=9000,r=500",
        "a=T,f=32,s=1,v=1,i=2,c=9000,r=500",
        "a=d",
        "a=f,i=1",
        "garbage",
        "a=p,i=999",
    ] {
        let mut rig = Rig::new();
        rig.run(&wire("a=t,f=32,s=30,v=40,i=1", &tile_30x40()));
        let mut stream = Vec::new();
        rig.run_at(
            &wire(control, &rgba(1, 1, 0)),
            &mut Instant::now(),
            &mut stream,
        );
        assert!(markers(&stream) <= 1, "{control}");
        assert!(stream.len() <= mark().len(), "{control}: {}", stream.len());
    }
}

#[test]
fn a_record_whose_marker_was_lost_does_not_hold_back_the_ones_behind_it() {
    let rig = Rig::new();
    rig.hub.queue_raw(5, "a=t,f=32,s=1,v=1,i=1", b"AAAAAA==");
    rig.hub.queue_raw(6, "a=t,f=32,s=1,v=1,i=2", b"AAAAAA==");
    rig.hub.queue_raw(7, "a=t,f=32,s=1,v=1,i=3", b"AAAAAA==");
    rig.hub.decode_pending();
    let screen = Screen::blank(false);
    assert_eq!(rig.hub.apply(0, &screen), None, "no record has the id 0");
    assert_eq!(rig.hub.waiting_records(), 3);
    // The marker of 5 never came: 6's takes the way, and 5 is dropped.
    assert_eq!(rig.hub.apply(6, &screen), Some(ok(2)));
    assert_eq!(rig.hub.waiting_records(), 1);
    assert_eq!(rig.hub.apply(5, &screen), None);
    assert_eq!(rig.hub.apply(7, &screen), Some(ok(3)));
    assert_eq!(rig.main_ids(), [2, 3]);
}

#[test]
fn a_named_png_with_cells_and_no_size_moves_the_cursor_by_the_cells() {
    let file = Scratch::temp("bt-image", &tall_png());
    let mut rig = Rig::new();
    let (stream, _) = rig.run(&wire("a=T,t=f,f=100,c=20,r=5,i=1", file.name()));
    assert_eq!(stream, marker(4, 20));
    // A placement of it later is made from the store, which has the size by
    // then (the read that sent it had none to remember).
    let (stream, _) = rig.run(&wire("a=p,i=1", &[]));
    assert_eq!(stream, marker(1, 3));
}

fn tile_30x40() -> Vec<u8> {
    rgba(30, 40, 3)
}

// ─── a remote session ───────────────────────────────────────────────────

/// A rig, and the flag that makes its pane remote.
fn remote_rig() -> (Rig, Arc<AtomicBool>) {
    let rig = Rig::new();
    let remote = Arc::clone(&rig.remote);
    (rig, remote)
}

#[test]
fn in_a_remote_session_a_name_is_refused_without_touching_what_it_names() {
    let (mut rig, remote) = remote_rig();
    remote.store(true, Ordering::Release);
    let plain = Scratch::temp("bt-image", &rgba(1, 1, 1));
    let temp = Scratch::temp("tty-graphics-protocol-image", &rgba(1, 1, 1));
    let object = Object::new(&rgba(1, 1, 1));
    let png = Scratch::temp("bt-image", &tall_png());
    for (control, name) in [
        ("a=t,t=f,f=32,s=1,v=1,i=1", plain.name()),
        ("a=t,t=t,f=32,s=1,v=1,i=1", temp.name()),
        ("a=t,t=s,f=32,s=1,v=1,i=1", object.name()),
        // And one that would move the cursor.
        ("a=T,t=f,f=100,s=30,v=40,i=1", png.name()),
    ] {
        let (stream, answers) = rig.run(&wire(control, name));
        assert_eq!(answers, unreadable(1), "{control}");
        // Nothing moves: a refused command is only its marker.
        assert_eq!(stream, mark(), "{control}");
    }
    // A query is answered where it is read, as every query with its pixels
    // in the sequence is.
    for (control, name) in [
        ("i=1,a=q,t=f,f=32,s=1,v=1", plain.name()),
        ("i=1,a=q,t=t,f=32,s=1,v=1", temp.name()),
        ("i=1,a=q,t=s,f=32,s=1,v=1", object.name()),
    ] {
        rig.queries.clear();
        let (stream, answers) = rig.run(&wire(control, name));
        assert!(stream.is_empty() && answers.is_empty(), "{control}");
        assert_eq!(rig.queries, [unreadable(1)], "{control}");
    }
    assert!(plain.exists() && temp.exists() && png.exists());
    assert!(object.exists(), "shared memory is left alone too");
    assert!(rig.main_ids().is_empty());
    // What the pane's own program sends in the sequence is its own.
    let (_, answers) = rig.run(&wire("a=t,f=32,s=1,v=1,i=2", &rgba(1, 1, 1)));
    assert_eq!(answers, ok(2));
    // And the session ending is the end of the refusal.
    remote.store(false, Ordering::Release);
    let (_, answers) = rig.run(&wire("a=t,t=f,f=32,s=1,v=1,i=3", plain.name()));
    assert_eq!(answers, ok(3));
}

#[test]
fn a_session_that_ended_before_the_decode_still_refuses_what_was_read_in_it() {
    // The scanner has acted on the whole of a read before the decode runs: a
    // `D` and a prompt in it end the remote session, and the name a program
    // there sent in the same read would be read here, as local. The state at
    // the point of the stream is what counts.
    let (mut rig, remote) = remote_rig();
    let temp = Scratch::temp("tty-graphics-protocol-image", &rgba(1, 1, 1));
    remote.store(true, Ordering::Release);
    queue_only(&mut rig, &wire("a=t,t=t,f=32,s=1,v=1,i=1", temp.name()));
    // What the stream says next ends the session; the record is already
    // what it will be.
    remote.store(false, Ordering::Release);
    rig.hub.decode_pending();
    let answers = rig.hub.apply_next(&rig.screen()).unwrap_or_default();
    assert_eq!(answers, unreadable(1));
    assert!(temp.exists());
}

#[test]
fn a_remote_session_found_while_a_command_waits_refuses_that_command_too() {
    // The probe that finds a session remote runs apart from the stream: a
    // command read just before it is decoded just after, and must not be read.
    let (mut rig, remote) = remote_rig();
    let temp = Scratch::temp("tty-graphics-protocol-image", &rgba(1, 1, 1));
    let png = Scratch::temp("bt-image", &tall_png());
    queue_only(&mut rig, &wire("a=t,t=t,f=32,s=1,v=1,i=1", temp.name()));
    queue_only(&mut rig, &wire("i=2,a=q,t=f,f=32,s=1,v=1", png.name()));
    assert!(rig.hub.has_undecoded());
    remote.store(true, Ordering::Release);
    rig.hub.decode_pending();
    let screen = rig.screen();
    assert_eq!(
        rig.hub.apply_next(&screen).unwrap_or_default(),
        unreadable(1)
    );
    assert_eq!(
        rig.hub.apply_next(&screen).unwrap_or_default(),
        unreadable(2)
    );
    assert!(temp.exists() && png.exists(), "neither was opened");
    assert!(rig.main_ids().is_empty());
}
