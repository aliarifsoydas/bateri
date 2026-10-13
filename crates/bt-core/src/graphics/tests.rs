use std::sync::atomic::{AtomicBool, Ordering};
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

fn base64(bytes: &[u8]) -> String {
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
fn wire(control: &str, payload: &[u8]) -> Vec<u8> {
    [
        ESC_G,
        control.as_bytes(),
        b";",
        base64(payload).as_bytes(),
        ST,
    ]
    .concat()
}

fn rgba(width: u32, height: u32, seed: u8) -> Vec<u8> {
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
struct Rig {
    splitter: Splitter,
    receiver: Receiver,
    hub: Hub,
    /// Which screen the parser is on when a marker arrives.
    alt: bool,
    /// Where the cursor is when a marker arrives.
    cursor: (i32, u16),
    /// The answers to queries, which are the reader's own.
    queries: Vec<Vec<u8>>,
}

impl Rig {
    fn new() -> Self {
        Self::with_hub(Hub::new())
    }

    fn with_hub(hub: Hub) -> Self {
        Self {
            splitter: Splitter::new(PAYLOAD_LIMIT),
            receiver: Receiver::new(),
            hub,
            alt: false,
            cursor: (0, 0),
            queries: Vec::new(),
        }
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

    fn run_at(
        &mut self,
        bytes: &[u8],
        now: &mut Instant,
        stream: &mut Vec<u8>,
    ) -> (Vec<u8>, Vec<u8>) {
        let hub = &self.hub;
        let queries = &mut self.queries;
        let receiver = &mut self.receiver;
        let mut out = Vec::new();
        self.splitter.feed(bytes, |piece| match piece {
            Piece::Bytes(bytes) => out.extend_from_slice(bytes),
            Piece::Command(apc) => {
                receiver.receive(apc, *now, ENV, &|echo| hub.stored_size(echo), |outcome| {
                    match outcome {
                        Outcome::Pending(pending) => {
                            let motion = pending.motion();
                            motion.lead(ENV.lines, &mut out);
                            hub.queue(pending);
                            out.extend_from_slice(MARK);
                            motion.trail(&mut out);
                        }
                        Outcome::Query(reply) => queries.extend(reply.encode()),
                    }
                })
            }
        });
        stream.extend_from_slice(&out);
        // The read loop decodes between a read and its parse.
        hub.decode_pending();
        let mut answers = Vec::new();
        let screen = self.screen();
        let mut rest = out.as_slice();
        while let Some(at) = rest.windows(MARK.len()).position(|window| window == MARK) {
            answers.extend(hub.apply_next(&screen).unwrap_or_default());
            rest = &rest[at + MARK.len()..];
        }
        (stream.clone(), answers)
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
    assert_eq!(stream, MARK, "the command is replaced by the marker");
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
    assert_eq!(stream, MARK, "the chunks make one marker, at the last");
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
    assert!(rig.receiver.is_open());
    assert!(rig.main_ids().is_empty());
    rig.run_at(&[ESC_G, b"m=0;AA==", ST].concat(), &mut now, &mut stream);
    assert!(!rig.receiver.is_open());
    assert_eq!(rig.main_ids(), [1]);
}

#[test]
fn a_command_that_names_its_action_ends_a_transmission_in_progress() {
    let mut rig = Rig::new();
    rig.run(&[ESC_G, b"a=t,f=32,s=1,v=1,i=1,m=1;AAAA", ST].concat());
    assert!(rig.receiver.is_open());
    rig.run(&wire("a=t,f=32,s=1,v=1,i=2", &[1, 2, 3, 4]));
    assert!(!rig.receiver.is_open());
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
    rig.splitter = Splitter::new(8);
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
    let receiver = &mut rig.receiver;
    rig.splitter.feed(&both, |piece| {
        if let Piece::Command(apc) = piece {
            receiver.receive(apc, now, ENV, &|_| None, |outcome| {
                if let Outcome::Pending(pending) = outcome {
                    hub.queue(pending);
                    stream.extend_from_slice(MARK);
                }
            });
        }
    });
    assert_eq!(stream.len(), 2 * MARK.len());
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
    let now = Instant::now();
    let hub = &rig.hub;
    let receiver = &mut rig.receiver;
    rig.splitter.feed(bytes, |piece| {
        if let Piece::Command(apc) = piece {
            receiver.receive(apc, now, ENV, &|_| None, |outcome| {
                if let Outcome::Pending(pending) = outcome {
                    hub.queue(pending);
                }
            });
        }
    });
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

#[test]
fn what_does_not_fit_the_readers_buffer_is_served_in_order_on_the_next_reads() {
    let mut spill = Spill::default();
    assert!(spill.is_empty());
    spill.begin().extend_from_slice(b"0123456789");
    assert_eq!(spill.rest(), b"0123456789");
    let mut buf = [0u8; 4];
    let mut got = Vec::new();
    while !spill.is_empty() {
        let n = spill.serve(&mut buf);
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, b"0123456789");
    assert!(spill.rest().is_empty());
    // A new separation starts from nothing, whatever was left.
    spill.begin().extend_from_slice(b"abc");
    spill.serve(&mut buf[..1]);
    assert_eq!(spill.rest(), b"bc");
    spill.begin().extend_from_slice(b"xyz");
    assert_eq!(spill.rest(), b"xyz");
}

// ─── bytes ──────────────────────────────────────────────────────────────

#[test]
fn the_marker_is_a_dispatchable_private_mode() {
    assert_eq!(MARK, format!("\x1b[?{MARK_MODE}h").as_bytes());
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
    assert_eq!(stream, [b"\x1bD".as_slice(), MARK, b"\x1b[2C"].concat());
    assert_eq!(answers, ok(1));
    // At the marker the cursor is on the image's last row: the top is one up.
    assert_eq!(placed(&rig), [(1, 2, 5)]);
}

#[test]
fn a_cursor_that_is_asked_to_stay_does_not_move() {
    let mut rig = Rig::new();
    rig.cursor = (3, 5);
    let (stream, _) = rig.run(&wire("a=T,f=32,s=20,v=40,i=1,C=1", &tile()));
    assert_eq!(stream, MARK);
    assert_eq!(placed(&rig), [(1, 3, 5)]);
}

#[test]
fn an_image_as_wide_as_the_screen_leaves_the_cursor_on_the_next_line() {
    let mut rig = Rig::new();
    let (stream, _) = rig.run(&wire("a=T,f=32,s=800,v=20,i=1,c=80,r=1", &rgba(800, 20, 0)));
    assert_eq!(stream, [MARK, b"\x1bD\r"].concat());
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
    let around = [b"\x1bD".as_slice(), MARK, b"\x1b[3C"].concat();
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
    assert_eq!(stream, MARK);
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
    let around = [b"\x1bD".as_slice(), MARK, b"\x1b[2C"].concat();
    // Both in one read: the image is still waiting for its marker.
    let mut rig = Rig::new();
    rig.cursor = (4, 0);
    let (stream, _) = rig.run(&both);
    assert_eq!(stream, [MARK, &around].concat());
    // The image was sent long ago, and the receiver has forgotten it.
    let mut rig = Rig::new();
    rig.run(&wire("a=t,f=32,s=20,v=40,i=1", &tile()));
    rig.receiver = Receiver::new();
    let (stream, answers) = rig.run(&wire("a=p,i=1,p=9", &[]));
    assert_eq!(stream, around);
    assert_eq!(answers, b"\x1b_Gi=1,p=9;OK\x1b\\");
}

#[test]
fn placing_what_is_not_there_is_an_answer_and_moves_nothing() {
    let mut rig = Rig::new();
    let (stream, answers) = rig.run(&wire("a=p,i=9", &[]));
    assert_eq!(stream, MARK);
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
        splitter: Splitter,
        receiver: Receiver,
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
                splitter: Splitter::new(PAYLOAD_LIMIT),
                receiver: Receiver::new(),
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
            let receiver = &mut self.receiver;
            let mut out = Vec::new();
            self.splitter.feed(bytes, |piece| match piece {
                Piece::Bytes(bytes) => out.extend_from_slice(bytes),
                Piece::Command(apc) => receiver.receive(
                    apc,
                    Instant::now(),
                    env,
                    &|echo| hub.stored_size(echo),
                    |outcome| {
                        if let Outcome::Pending(pending) = outcome {
                            let motion = pending.motion();
                            motion.lead(env.lines, &mut out);
                            hub.queue(pending);
                            out.extend_from_slice(MARK);
                            motion.trail(&mut out);
                        }
                    },
                ),
            });
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

    #[test]
    fn a_marker_a_program_wrote_itself_does_not_move_a_placement() {
        let mut screen = Screenful::new(20, 10, 100);
        screen.feed(&at(2, 2));
        screen.feed(&tile_command("a=T,f=32,s=20,v=40,i=1,C=1"));
        screen.feed(MARK);
        screen.feed(MARK);
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
