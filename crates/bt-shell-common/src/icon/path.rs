//! SVG path data, read into the few segments every toolkit can draw.
//!
//! An icon's outline is stored the way its source publishes it (the `d` of an
//! SVG `<path>`), and this is the one place that reads it: every command of
//! the grammar — `M L H V C S Q T A Z`, absolute and relative, with the
//! implicit repeat of the last command — comes out as a move, a line, a cubic
//! Bézier or a close. A quadratic curve is raised to a cubic and an elliptical
//! arc is cut into cubics of at most a quarter turn (the SVG implementation
//! notes' endpoint-to-centre conversion), so a platform shell needs a
//! `NSBezierPath`-like builder with four calls and nothing else.
//!
//! The reading is strict about what it cannot draw and lenient about how the
//! data is written: separators are optional wherever the tokens cannot run
//! together (`0-.5`, `1.5.5`, and an arc's two flags are one character each, so
//! `a1 1 0 011 1` is flags `0` and `1` followed by `1 1`), and a segment after a
//! close starts at the subpath's start, as the specification has it. Anything
//! else — a number where a command should be, a missing argument, a number that
//! is not finite — is an error with the offset, never a panic and never a guess.

use std::f64::consts::{FRAC_PI_2, TAU};
use std::fmt;
use std::ops::{Add, Mul, Sub};

/// A point in the path's own units (a view box's, before [`Fit`] places it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ORIGIN: Self = Self::new(0.0, 0.0);

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// The point mirrored through `centre` — the implied control point of `S`
    /// and `T`.
    fn mirrored_through(self, centre: Self) -> Self {
        centre + (centre - self)
    }
}

impl Add for Point {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y)
    }
}

impl Sub for Point {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y)
    }
}

impl Mul<f64> for Point {
    type Output = Self;

    fn mul(self, factor: f64) -> Self {
        Self::new(self.x * factor, self.y * factor)
    }
}

/// One drawing step. The path's current point is where the last step ended
/// ([`Self::Close`] returns it to the subpath's start).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Segment {
    /// Start a subpath.
    Move(Point),
    Line(Point),
    /// A cubic Bézier curve from the current point.
    Cubic {
        c1: Point,
        c2: Point,
        to: Point,
    },
    /// Join the current point to the subpath's start.
    Close,
}

impl Segment {
    /// The step with every point mapped through `f`.
    pub fn map_points(self, f: impl Fn(Point) -> Point) -> Self {
        match self {
            Self::Move(to) => Self::Move(f(to)),
            Self::Line(to) => Self::Line(f(to)),
            Self::Cubic { c1, c2, to } => Self::Cubic {
                c1: f(c1),
                c2: f(c2),
                to: f(to),
            },
            Self::Close => Self::Close,
        }
    }
}

/// The rectangle of an icon's own coordinates that its slot shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl ViewBox {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// A square box at the origin.
    pub const fn square(side: f64) -> Self {
        Self::new(0.0, 0.0, side, side)
    }

    /// Where the box lands in a square slot of `side`: scaled to fit its
    /// longer edge and centred on the other (SVG's `xMidYMid meet`). The box's
    /// edges are positive.
    pub fn fit(self, side: f64) -> Fit {
        let scale = side / self.w.max(self.h);
        Fit {
            scale,
            dx: (side - self.w * scale) / 2.0 - self.x * scale,
            dy: (side - self.h * scale) / 2.0 - self.y * scale,
        }
    }
}

/// A view box placed in a slot: a uniform scale and an offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub scale: f64,
    pub dx: f64,
    pub dy: f64,
}

impl Fit {
    pub fn point(self, p: Point) -> Point {
        Point::new(p.x * self.scale + self.dx, p.y * self.scale + self.dy)
    }

    /// A length in the box's units (a stroke's width) in the slot's.
    pub fn length(self, units: f64) -> f64 {
        units * self.scale
    }

    pub fn segment(self, segment: Segment) -> Segment {
        segment.map_points(|p| self.point(p))
    }
}

/// Path data the reader could not take, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathError {
    /// Byte offset into the path data.
    pub offset: usize,
    pub message: &'static str,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "path data, byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for PathError {}

/// Read SVG path data into segments. Empty data is a valid empty path.
pub fn parse(data: &str) -> Result<Vec<Segment>, PathError> {
    let mut scan = Scanner { text: data, pos: 0 };
    let mut pen = Pen::default();
    let mut command = None;
    loop {
        scan.skip_separators();
        let Some(next) = scan.peek() else { break };
        let fresh = next.is_ascii_alphabetic();
        if fresh {
            command = Some(next);
            scan.pos += 1;
        } else if !next.is_ascii_digit() && !matches!(next, b'+' | b'-' | b'.') {
            return Err(scan.error("unexpected character"));
        }
        let Some(command) = command.as_mut() else {
            return Err(scan.error("data must begin with a command"));
        };
        if pen.segments.is_empty() && !matches!(*command, b'M' | b'm') {
            return Err(scan.error("data must begin with a move"));
        }
        if matches!(*command, b'Z' | b'z') {
            // `Z` takes no argument, so a number after it is not a repeat.
            if !fresh {
                return Err(scan.error("a number after a close"));
            }
            pen.close();
            continue;
        }
        pen.apply(*command, &mut scan)?;
        // What follows a move without a new command is a line (the same
        // relativeness).
        match *command {
            b'M' => *command = b'L',
            b'm' => *command = b'l',
            _ => {}
        }
    }
    Ok(pen.segments)
}

/// The cursor over the data.
struct Scanner<'a> {
    text: &'a str,
    pos: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn error(&self, message: &'static str) -> PathError {
        PathError {
            offset: self.pos,
            message,
        }
    }

    fn skip_separators(&mut self) {
        while matches!(
            self.peek(),
            Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0c | b',')
        ) {
            self.pos += 1;
        }
    }

    fn digits(&mut self) -> usize {
        let from = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        self.pos - from
    }

    /// A number: sign, integer part, fraction, exponent — each optional but the
    /// digits. A second `.` ends the number (`1.5.5` is two).
    fn number(&mut self) -> Result<f64, PathError> {
        self.skip_separators();
        let from = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let mut digits = self.digits();
        if self.peek() == Some(b'.') {
            self.pos += 1;
            digits += self.digits();
        }
        if digits == 0 {
            self.pos = from;
            return Err(self.error("expected a number"));
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let mark = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                self.pos = mark;
            }
        }
        let value = self
            .text
            .get(from..self.pos)
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|value| value.is_finite());
        value.ok_or_else(|| {
            self.pos = from;
            self.error("not a finite number")
        })
    }

    fn point(&mut self) -> Result<Point, PathError> {
        let x = self.number()?;
        let y = self.number()?;
        Ok(Point::new(x, y))
    }

    /// An arc's flag: one character, `0` or `1`, which may touch the next
    /// number.
    fn flag(&mut self) -> Result<bool, PathError> {
        self.skip_separators();
        let flag = match self.peek() {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return Err(self.error("expected an arc flag, 0 or 1")),
        };
        self.pos += 1;
        Ok(flag)
    }
}

/// The path being built: its steps, the current point and what the next
/// smooth curve reflects.
#[derive(Default)]
struct Pen {
    segments: Vec<Segment>,
    current: Point,
    /// Where the open subpath began, for `Z`.
    start: Point,
    /// Whether a subpath is open. After a close it is not, and the next drawing
    /// step starts a subpath at the current point.
    open: bool,
    /// The previous step's second control point when it was `C`/`S` (what `S`
    /// reflects), and its control point when it was `Q`/`T` (what `T` does).
    cubic_control: Option<Point>,
    quad_control: Option<Point>,
}

impl Pen {
    fn push(&mut self, segment: Segment) {
        if !self.open {
            self.segments.push(Segment::Move(self.current));
            self.open = true;
        }
        self.segments.push(segment);
    }

    fn close(&mut self) {
        if self.open {
            self.segments.push(Segment::Close);
        }
        self.open = false;
        self.current = self.start;
        self.cubic_control = None;
        self.quad_control = None;
    }

    /// One step of `command`, its arguments read from `scan`. (`Z` never gets
    /// here.)
    fn apply(&mut self, command: u8, scan: &mut Scanner) -> Result<(), PathError> {
        // Relative arguments are offsets from the current point.
        let relative = command.is_ascii_lowercase();
        let base = if relative {
            self.current
        } else {
            Point::ORIGIN
        };
        let mut cubic_control = None;
        let mut quad_control = None;
        match command.to_ascii_uppercase() {
            b'M' => {
                let to = base + scan.point()?;
                self.segments.push(Segment::Move(to));
                self.open = true;
                self.current = to;
                self.start = to;
            }
            b'L' => {
                let to = base + scan.point()?;
                self.push(Segment::Line(to));
                self.current = to;
            }
            b'H' => {
                let x = scan.number()?;
                let to = Point::new(base.x + x, self.current.y);
                self.push(Segment::Line(to));
                self.current = to;
            }
            b'V' => {
                let y = scan.number()?;
                let to = Point::new(self.current.x, base.y + y);
                self.push(Segment::Line(to));
                self.current = to;
            }
            b'C' => {
                let c1 = base + scan.point()?;
                let c2 = base + scan.point()?;
                let to = base + scan.point()?;
                self.push(Segment::Cubic { c1, c2, to });
                self.current = to;
                cubic_control = Some(c2);
            }
            b'S' => {
                let c2 = base + scan.point()?;
                let to = base + scan.point()?;
                let c1 = self
                    .cubic_control
                    .map_or(self.current, |c| c.mirrored_through(self.current));
                self.push(Segment::Cubic { c1, c2, to });
                self.current = to;
                cubic_control = Some(c2);
            }
            b'Q' => {
                let control = base + scan.point()?;
                let to = base + scan.point()?;
                self.quad(control, to);
                quad_control = Some(control);
            }
            b'T' => {
                let to = base + scan.point()?;
                let control = self
                    .quad_control
                    .map_or(self.current, |c| c.mirrored_through(self.current));
                self.quad(control, to);
                quad_control = Some(control);
            }
            b'A' => {
                let rx = scan.number()?;
                let ry = scan.number()?;
                let rotation = scan.number()?;
                let large = scan.flag()?;
                let sweep = scan.flag()?;
                let to = base + scan.point()?;
                self.arc(Radii { rx, ry, rotation }, Flags { large, sweep }, to);
            }
            _ => return Err(scan.error("unknown command")),
        }
        self.cubic_control = cubic_control;
        self.quad_control = quad_control;
        Ok(())
    }

    /// A quadratic curve to `to`, as the cubic with the same shape: each
    /// control point a third of the way from its end back toward the quadratic
    /// one's.
    fn quad(&mut self, control: Point, to: Point) {
        let from = self.current;
        self.push(Segment::Cubic {
            c1: from + (control - from) * (2.0 / 3.0),
            c2: to + (control - to) * (2.0 / 3.0),
            to,
        });
        self.current = to;
    }

    /// An elliptical arc to `to`, as cubics of at most a quarter turn each.
    fn arc(&mut self, radii: Radii, flags: Flags, to: Point) {
        let from = self.current;
        let (mut rx, mut ry) = (radii.rx.abs(), radii.ry.abs());
        if from == to {
            // No arc at all, not a full circle.
            return;
        }
        if rx == 0.0 || ry == 0.0 {
            self.push(Segment::Line(to));
            self.current = to;
            return;
        }
        let (sin, cos) = radii.rotation.to_radians().sin_cos();
        // The endpoint-to-centre conversion, SVG implementation notes F.6.5.
        let half = (from - to) * 0.5;
        let x1 = cos * half.x + sin * half.y;
        let y1 = -sin * half.x + cos * half.y;
        // Radii too small to span the endpoints grow until they just do.
        let spread = (x1 / rx).powi(2) + (y1 / ry).powi(2);
        if spread > 1.0 {
            let grow = spread.sqrt();
            rx *= grow;
            ry *= grow;
        }
        let across = (rx * y1).powi(2) + (ry * x1).powi(2);
        let sign = if flags.large == flags.sweep {
            -1.0
        } else {
            1.0
        };
        let reach = ((rx * ry).powi(2) - across).max(0.0) / across;
        let coefficient = sign * reach.sqrt();
        let cx1 = coefficient * rx * y1 / ry;
        let cy1 = -coefficient * ry * x1 / rx;
        let centre = Point::new(
            cos * cx1 - sin * cy1 + (from.x + to.x) / 2.0,
            sin * cx1 + cos * cy1 + (from.y + to.y) / 2.0,
        );
        let start_vector = ((x1 - cx1) / rx, (y1 - cy1) / ry);
        let end_vector = ((-x1 - cx1) / rx, (-y1 - cy1) / ry);
        let start = start_vector.1.atan2(start_vector.0);
        let mut sweep = (start_vector.0 * end_vector.1 - start_vector.1 * end_vector.0)
            .atan2(start_vector.0 * end_vector.0 + start_vector.1 * end_vector.1);
        if flags.sweep && sweep < 0.0 {
            sweep += TAU;
        } else if !flags.sweep && sweep > 0.0 {
            sweep -= TAU;
        }
        let pieces = ((sweep.abs() / FRAC_PI_2) - 1e-9).ceil().max(1.0);
        let step = sweep / pieces;
        // The tangent handle of a unit-circle arc of `step` radians.
        let handle = 4.0 / 3.0 * (step / 4.0).tan();
        let place = |u: f64, v: f64| {
            Point::new(
                centre.x + rx * u * cos - ry * v * sin,
                centre.y + rx * u * sin + ry * v * cos,
            )
        };
        let count = pieces as usize;
        for i in 0..count {
            let (s0, c0) = (start + step * i as f64).sin_cos();
            let (s1, c1) = (start + step * (i + 1) as f64).sin_cos();
            let end = if i + 1 == count {
                // Exactly where the data says, not where rounding got to.
                to
            } else {
                place(c1, s1)
            };
            self.push(Segment::Cubic {
                c1: place(c0 - handle * s0, s0 + handle * c0),
                c2: place(c1 + handle * s1, s1 - handle * c1),
                to: end,
            });
        }
        self.current = to;
    }
}

/// An arc's radii and the rotation of its ellipse in degrees.
struct Radii {
    rx: f64,
    ry: f64,
    rotation: f64,
}

/// An arc's two flags: whether it is the larger of the two arcs between its
/// endpoints, and whether it is drawn in the positive-angle direction.
struct Flags {
    large: bool,
    sweep: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    fn near(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6
    }

    /// The point of a cubic at `t`.
    fn at(from: Point, c1: Point, c2: Point, to: Point, t: f64) -> Point {
        let u = 1.0 - t;
        from * (u * u * u) + c1 * (3.0 * u * u * t) + c2 * (3.0 * u * t * t) + to * (t * t * t)
    }

    fn ends(segments: &[Segment]) -> Vec<Point> {
        segments
            .iter()
            .filter_map(|s| match *s {
                Segment::Move(to) | Segment::Line(to) | Segment::Cubic { to, .. } => Some(to),
                Segment::Close => None,
            })
            .collect()
    }

    #[test]
    fn absolute_and_relative_commands_agree() {
        let absolute = parse("M1 1L4 1V3H2Z").unwrap();
        let relative = parse("m1 1l3 0v2h-2z").unwrap();
        assert_eq!(absolute, relative);
        assert_eq!(
            absolute,
            [
                Segment::Move(p(1.0, 1.0)),
                Segment::Line(p(4.0, 1.0)),
                Segment::Line(p(4.0, 3.0)),
                Segment::Line(p(2.0, 3.0)),
                Segment::Close,
            ]
        );
    }

    #[test]
    fn a_command_repeats_and_a_move_repeats_as_a_line() {
        let repeated = parse("M0 0 1 1 2 0").unwrap();
        assert_eq!(
            repeated,
            [
                Segment::Move(p(0.0, 0.0)),
                Segment::Line(p(1.0, 1.0)),
                Segment::Line(p(2.0, 0.0)),
            ]
        );
        // Relative: each pair is an offset from the one before.
        let relative = parse("m1 1 1 1 1 -1").unwrap();
        assert_eq!(ends(&relative), [p(1.0, 1.0), p(2.0, 2.0), p(3.0, 1.0)]);
        let curves = parse("M0 0c1 1 2 1 3 0 1 -1 2 -1 3 0").unwrap();
        assert_eq!(ends(&curves), [p(0.0, 0.0), p(3.0, 0.0), p(6.0, 0.0)]);
    }

    #[test]
    fn numbers_run_together_where_the_grammar_allows() {
        // A sign and a second point both end a number.
        let tight = parse("M0-.5L1.5.5-2-3").unwrap();
        assert_eq!(ends(&tight), [p(0.0, -0.5), p(1.5, 0.5), p(-2.0, -3.0)]);
        let spaced = parse("M 0 , -0.5 L 1.5,0.5  -2 ,-3").unwrap();
        assert_eq!(tight, spaced);
        let exponent = parse("M1e1 2E-1").unwrap();
        assert_eq!(ends(&exponent), [p(10.0, 0.2)]);
    }

    #[test]
    fn a_smooth_cubic_reflects_the_previous_control_point() {
        let path = parse("M0 0C0 10 10 10 10 0S20 -10 20 0").unwrap();
        let Segment::Cubic { c1, .. } = path[2] else {
            panic!("a cubic, got {:?}", path[2]);
        };
        assert!(near(c1, p(10.0, -10.0)), "{c1:?}");
        // After anything but C or S, the control is the current point.
        let after_line = parse("M0 0L10 0S20 -10 20 0").unwrap();
        let Segment::Cubic { c1, .. } = after_line[2] else {
            panic!("a cubic, got {:?}", after_line[2]);
        };
        assert!(near(c1, p(10.0, 0.0)), "{c1:?}");
    }

    #[test]
    fn a_quadratic_is_raised_to_a_cubic_and_a_smooth_one_reflects() {
        let path = parse("M0 0Q5 5 10 0T20 0").unwrap();
        let Segment::Cubic { c1, c2, to } = path[1] else {
            panic!("a cubic, got {:?}", path[1]);
        };
        assert!(near(c1, p(10.0 / 3.0, 10.0 / 3.0)), "{c1:?}");
        assert!(near(c2, p(20.0 / 3.0, 10.0 / 3.0)), "{c2:?}");
        assert_eq!(to, p(10.0, 0.0));
        // The reflection of (5, 5) through (10, 0) is (15, -5).
        let Segment::Cubic { c1, c2, to } = path[2] else {
            panic!("a cubic, got {:?}", path[2]);
        };
        assert!(near(c1, p(10.0 + 5.0 * (2.0 / 3.0), -5.0 * (2.0 / 3.0))));
        assert!(near(
            c2,
            p(20.0 + (15.0 - 20.0) * (2.0 / 3.0), -5.0 * (2.0 / 3.0))
        ));
        assert_eq!(to, p(20.0, 0.0));
    }

    #[test]
    fn a_step_after_a_close_starts_where_the_subpath_did() {
        let path = parse("M1 1L5 1 5 5zl1 0").unwrap();
        assert_eq!(
            path,
            [
                Segment::Move(p(1.0, 1.0)),
                Segment::Line(p(5.0, 1.0)),
                Segment::Line(p(5.0, 5.0)),
                Segment::Close,
                Segment::Move(p(1.0, 1.0)),
                Segment::Line(p(2.0, 1.0)),
            ]
        );
        // A second close closes nothing.
        let twice = parse("M0 0L1 0zz").unwrap();
        assert_eq!(twice.iter().filter(|s| **s == Segment::Close).count(), 1);
    }

    #[test]
    fn a_quarter_arc_is_one_cubic_on_the_circle() {
        // Positive-angle sweep from the top of a circle centred (0, 10) to the
        // right of it.
        let path = parse("M0 0A10 10 0 0 1 10 10").unwrap();
        assert_eq!(path.len(), 2);
        let Segment::Cubic { c1, c2, to } = path[1] else {
            panic!("a cubic, got {:?}", path[1]);
        };
        assert_eq!(to, p(10.0, 10.0));
        let middle = at(p(0.0, 0.0), c1, c2, to, 0.5);
        let radius = 10.0;
        let expected = p(
            radius * (-FRAC_PI_2 / 2.0).cos(),
            10.0 + radius * (-FRAC_PI_2 / 2.0).sin(),
        );
        assert!((middle.x - expected.x).abs() < 1e-3, "{middle:?}");
        assert!((middle.y - expected.y).abs() < 1e-3, "{middle:?}");
    }

    #[test]
    fn the_sweep_and_large_flags_choose_the_arc() {
        let on_circle = |path: &[Segment], centre: Point, radius: f64| {
            let mut from = Point::ORIGIN;
            for segment in path {
                match *segment {
                    Segment::Move(to) => from = to,
                    Segment::Cubic { c1, c2, to } => {
                        for t in [0.25, 0.5, 0.75] {
                            let q = at(from, c1, c2, to, t) - centre;
                            let distance = (q.x * q.x + q.y * q.y).sqrt();
                            assert!(
                                (distance - radius).abs() < radius * 1e-3,
                                "{distance} vs {radius} at {t}"
                            );
                        }
                        from = to;
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        };
        // The same endpoints and radius: the four arcs of two circles.
        for (large, sweep, centre) in [
            (0, 0, p(10.0, 0.0)),
            (0, 1, p(0.0, 10.0)),
            (1, 0, p(0.0, 10.0)),
            (1, 1, p(10.0, 0.0)),
        ] {
            let path = parse(&format!("M0 0A10 10 0 {large} {sweep} 10 10")).unwrap();
            on_circle(&path, centre, 10.0);
        }
        // A half circle is two cubics and passes through the apex.
        let half = parse("M0 0A5 5 0 0 1 10 0").unwrap();
        assert_eq!(half.len(), 3);
        let Segment::Cubic { to, .. } = half[1] else {
            panic!("a cubic, got {:?}", half[1]);
        };
        assert!(near(to, p(5.0, -5.0)), "{to:?}");
        // A large arc of three quarters takes three cubics.
        let three = parse("M10 0A10 10 0 1 1 0 10").unwrap();
        assert_eq!(three.len(), 4);
    }

    #[test]
    fn radii_too_small_grow_and_degenerate_arcs_degrade() {
        // Radius 1 cannot span ten units: it becomes the half circle of radius 5.
        let grown = parse("M0 0A1 1 0 0 1 10 0").unwrap();
        let half = parse("M0 0A5 5 0 0 1 10 0").unwrap();
        assert_eq!(grown.len(), half.len());
        for (a, b) in grown.iter().zip(&half) {
            match (a, b) {
                (
                    Segment::Cubic { c1, c2, to },
                    Segment::Cubic {
                        c1: d1,
                        c2: d2,
                        to: dt,
                    },
                ) => {
                    assert!(near(*c1, *d1) && near(*c2, *d2) && near(*to, *dt));
                }
                (a, b) => assert_eq!(a, b),
            }
        }
        // A zero radius is a straight line; the same endpoints are no arc.
        let flat = parse("M0 0A0 5 0 0 1 10 0").unwrap();
        assert_eq!(
            flat,
            [Segment::Move(p(0.0, 0.0)), Segment::Line(p(10.0, 0.0))]
        );
        let nothing = parse("M3 3A5 5 0 0 1 3 3").unwrap();
        assert_eq!(nothing, [Segment::Move(p(3.0, 3.0))]);
    }

    #[test]
    fn arc_flags_may_touch_the_numbers_after_them() {
        // Flags `0` and `1`, then the endpoint `10 0`, with nothing between.
        let touching = parse("M0 0a5 5 0 0110 0").unwrap();
        let spaced = parse("M0 0a5 5 0 0 1 10 0").unwrap();
        assert_eq!(touching, spaced);
        let rotated = parse("M0 0A8 4 30 0 1 10 0").unwrap();
        for segment in &rotated {
            if let Segment::Cubic { c1, c2, to } = segment {
                for point in [c1, c2, to] {
                    assert!(point.x.is_finite() && point.y.is_finite());
                }
            }
        }
        assert_eq!(ends(&rotated).last(), Some(&p(10.0, 0.0)));
    }

    #[test]
    fn bad_data_is_an_error_with_a_place() {
        for (data, offset) in [
            ("L1 1", 1),
            ("1 1", 0),
            ("M1", 2),
            ("M1 1X", 5),
            ("M1 1z 5 5", 6),
            ("M0 0A1 1 0 2 0 5 5", 11),
            ("M1 1 L1e999 0", 6),
            ("M# 1", 1),
        ] {
            let error = parse(data).unwrap_err();
            assert_eq!(error.offset, offset, "{data:?}: {error}");
            assert!(error.to_string().contains("path data"));
        }
        assert_eq!(parse("").unwrap(), []);
        assert_eq!(parse("  ,").unwrap(), []);
    }

    #[test]
    fn a_view_box_fits_the_longer_edge_and_centres_the_other() {
        let square = ViewBox::square(24.0).fit(12.0);
        assert_eq!((square.scale, square.dx, square.dy), (0.5, 0.0, 0.0));
        assert_eq!(square.point(p(24.0, 12.0)), p(12.0, 6.0));
        assert_eq!(square.length(1.35), 0.675);

        let wide = ViewBox::new(0.0, 0.0, 20.0, 10.0).fit(10.0);
        assert_eq!((wide.scale, wide.dx, wide.dy), (0.5, 0.0, 2.5));
        assert_eq!(wide.point(p(20.0, 10.0)), p(10.0, 7.5));

        let offset = ViewBox::new(9.5, 11.0, 92.0, 92.0).fit(46.0);
        assert_eq!(offset.point(p(9.5, 11.0)), p(0.0, 0.0));
        assert_eq!(offset.point(p(101.5, 103.0)), p(46.0, 46.0));
        assert_eq!(
            offset.segment(Segment::Line(p(55.5, 57.0))),
            Segment::Line(p(23.0, 23.0))
        );
        assert_eq!(offset.segment(Segment::Close), Segment::Close);
    }
}
