//! The tabs in a column down the window's left edge (`[appearance] tab_position = "side"`): the
//! bar's other mode.
//!
//! **One bar, two places.** The column is not a second view with its own copy of the bar's
//! doings: it is the same [`TabBar`] — the tabs it was given, its chips, the transfers' ends, the
//! ⌘ keys, the clock, the name field, the menus — put in the window's full height at the left
//! instead of in the title row ([`TabBar::set_side`], told by the window's root view with every
//! placement). What changes is where things stand, and that is the pure model's: the rows from
//! `tabs::Side::layout`, the parts of a row from `tabs::Row` ([`Chip::set`] reads it), so the
//! Linux shell can lay the same list out. This file is a child of `tab_bar` for the bar's
//! private state, which a sibling could not reach without every field going public.
//!
//! **Where it sits.** The column is [`tabs::SIDE_WIDTH`] wide and the window's height tall, the
//! traffic lights at its head: the list starts a title row down (`top`, read from the window, the
//! same in full screen so the rows do not move as it comes and goes) and ends above New Tab,
//! which sits in the window's bottom-left corner and keeps that corner's centre
//! ([`tabs::Fit`]). The rows scroll by the wheel when they do not fit, run under the list's ends
//! and dissolve there by a mask on the list ([`Mask`]) — not a band of the ground's colour
//! painted over them, which would lighten a card's shadow falling under the column.
//!
//! **A press on the column's empty part moves the window** (the bar's `mouseDown:`,
//! `performWindowDragWithEvent:`), as it does on the bar's. The band of the title row over the
//! content is claimed by a [`RowClaim`] of the root view, so a press there is the pane's.
//!
//! **The summary card** opens at the row's right, [`CARD_BESIDE`](super::CARD_BESIDE) off the
//! column and level with the row, slid to fit the window (`card_beside`): the same card, the
//! same wait and the same map as the title row's, arriving from the row's side
//! ([`ROW_LEFT`](super::ROW_LEFT)).
//!
//! **Dragging** is the strip's, turned upright: a press on a row is a [`Grip`](crate::tabs::Grip)
//! read along y, the row follows the pointer and the others slide to make room, and the list
//! changes once, when the row is let go. Pulled out of the column it comes away in a drag session
//! (`tab_drag`), as from the bar, and the column takes a row from another window the way the bar
//! takes a chip: rows part under the pointer and the landing is `tabs::landing`'s. A pane carried
//! over the column (⌥⌘) lights the row it is over the middle of and, between two rows, draws a
//! line ([`Insertion`]) instead of opening room: a pane's drop is a mark, a tab's is a seat.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly};
use objc2_app_kit::{NSAccessibility, NSBezierPath, NSBox, NSColor, NSTextAlignment, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CALayer, CATransaction};

use super::{
    Chip, Dragging, IconStyle, Motion, Palette, Seen, Shown, Standing, TabBar, Tint, paint_in,
    plain_box,
};
use crate::card;
use crate::split::Rect;
use crate::tabs::{self, Column, FADE, Side, TAB_RADIUS};

/// A model rectangle as the view's: both are top-down, in points.
pub(super) fn ns_rect(rect: Rect) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.x, rect.y),
        NSSize::new(rect.width, rect.height),
    )
}

/// A rectangle with its own radius at each corner — top-left, top-right, bottom-right,
/// bottom-left, as a top-down view draws them — filled, and its inside edge stroked with `line` if
/// any. [`super::rounded`] for a shape whose corners differ: New Tab in the column keeps the
/// window's corner at its foot.
pub(super) fn rounded_corners(rect: NSRect, radii: [f64; 4], fill: Tint, line: Option<Tint>) {
    let path = corners_path(rect, radii);
    fill.color().setFill();
    path.fill();
    if let Some(line) = line {
        let inner = NSRect::new(
            NSPoint::new(rect.origin.x + 0.5, rect.origin.y + 0.5),
            NSSize::new(rect.size.width - 1.0, rect.size.height - 1.0),
        );
        let path = corners_path(inner, radii.map(|radius| (radius - 0.5).max(0.0)));
        path.setLineWidth(1.0);
        line.color().setStroke();
        path.stroke();
    }
}

/// The outline of [`rounded_corners`]: each corner an arc tangent to its two sides.
fn corners_path(
    rect: NSRect,
    [top_left, top_right, bottom_right, bottom_left]: [f64; 4],
) -> Retained<NSBezierPath> {
    let (left, top) = (rect.origin.x, rect.origin.y);
    let (right, bottom) = (left + rect.size.width, top + rect.size.height);
    let path = NSBezierPath::bezierPath();
    path.moveToPoint(NSPoint::new(left + top_left, top));
    path.lineToPoint(NSPoint::new(right - top_right, top));
    for (corner, towards, radius) in [
        (
            NSPoint::new(right, top),
            NSPoint::new(right, top + top_right),
            top_right,
        ),
        (
            NSPoint::new(right, bottom),
            NSPoint::new(right - bottom_right, bottom),
            bottom_right,
        ),
        (
            NSPoint::new(left, bottom),
            NSPoint::new(left, bottom - bottom_left),
            bottom_left,
        ),
        (
            NSPoint::new(left, top),
            NSPoint::new(left + top_left, top),
            top_left,
        ),
    ] {
        // The line from where the last arc ended is this arc's own tangent line.
        path.appendBezierPathWithArcFromPoint_toPoint_radius(corner, towards, radius);
    }
    path.closePath();
    path
}

/// The mask that fades the column's list where rows run under its ends, and what it was painted
/// for: a picture is made only when the list's size or its ends change.
#[derive(Default)]
pub(super) struct Mask {
    layer: RefCell<Option<Retained<CALayer>>>,
    painted: Cell<Option<Painted>>,
}

/// What a mask's picture was painted for.
#[derive(Clone, Copy, PartialEq)]
struct Painted {
    size: (f64, f64),
    top: bool,
    bottom: bool,
    scale: f64,
}

impl Mask {
    /// The list's mask for `column` at `scale`: opaque where rows show, running out to nothing
    /// over [`FADE`] at an end where more lies beyond (`Column::fade_top`/`fade_bottom`); no mask
    /// where nothing runs under an end.
    fn set(&self, list: &NSView, column: &Column, scale: f64) {
        let (top, bottom) = (column.fade_top, column.fade_bottom);
        if !top && !bottom {
            self.clear(list);
            return;
        }
        list.setWantsLayer(true);
        let Some(host) = list.layer() else {
            return;
        };
        let size = NSSize::new(column.list.width, column.list.height);
        let state = Painted {
            size: (size.width, size.height),
            top,
            bottom,
            scale,
        };
        let mut slot = self.layer.borrow_mut();
        let fresh = slot.is_none();
        let layer = slot.get_or_insert_with(CALayer::new);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        layer.setFrame(NSRect::new(NSPoint::ZERO, size));
        if self.painted.replace(Some(state)) != Some(state) {
            paint_in(layer, size, scale, move |rect| {
                fade_picture(rect, top, bottom)
            });
        }
        if fresh {
            // SAFETY: a mask is any layer; this one is the host's alone and lives as long as
            // the mask slot holds it.
            unsafe { host.setMask(Some(layer)) };
        }
        CATransaction::commit();
    }

    /// Takes the mask off: every row shows to the list's ends.
    pub(super) fn clear(&self, list: &NSView) {
        if self.layer.take().is_none() {
            return;
        }
        self.painted.set(None);
        if let Some(host) = list.layer() {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            // SAFETY: taking a mask off, which any layer allows.
            unsafe { host.setMask(None) };
            CATransaction::commit();
        }
    }
}

/// The mask's picture in `rect` (top-down): black — its alpha is what shows — from end to end,
/// less [`FADE`] at the ends that fade, where the alpha runs from 0 at the edge to 1 inside along
/// a smooth step.
fn fade_picture(rect: NSRect, top: bool, bottom: bool) {
    let (width, height) = (rect.size.width, rect.size.height);
    let fade = FADE.min(height / 2.0);
    let step = |t: f64| t * t * (3.0 - 2.0 * t);
    let black = |alpha: f64| NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, alpha);
    let from = if top { fade } else { 0.0 };
    let to = if bottom { height - fade } else { height };
    black(1.0).setFill();
    NSBezierPath::bezierPathWithRect(NSRect::new(
        NSPoint::new(0.0, from),
        NSSize::new(width, (to - from).max(0.0)),
    ))
    .fill();
    // A line each: the picture is painted when the list's size changes, not per frame.
    let lines = fade.ceil() as usize;
    for line in 0..lines {
        let y = line as f64;
        let alpha = step(((y + 0.5) / fade).min(1.0));
        black(alpha).setFill();
        if top {
            NSBezierPath::bezierPathWithRect(NSRect::new(
                NSPoint::new(0.0, y),
                NSSize::new(width, 1.0),
            ))
            .fill();
        }
        if bottom {
            NSBezierPath::bezierPathWithRect(NSRect::new(
                NSPoint::new(0.0, height - y - 1.0),
                NSSize::new(width, 1.0),
            ))
            .fill();
        }
    }
}

/// What is in flight over the column while it is laid out ([`TabBar::lay_out`] works the strip's
/// the same way): a row held by the pointer, and room a tab carried from another window opens.
#[derive(Clone, Copy)]
pub(super) struct Flight {
    pub(super) drag: Option<Dragging>,
    pub(super) gap: Option<usize>,
}

/// The ring's side and its stroke, and how far it hangs out to the left of the line it starts.
const RING: f64 = 8.0;
const RING_LINE: f64 = 2.0;

/// The line that marks "a new tab here" between two rows while a pane is carried over them: a ring
/// at the left and a line along the row's width, in the accent, standing in the gap
/// ([`Column::between`]). It lives in the list's view, so the list's edges cut it as they cut the
/// rows.
#[derive(Default)]
pub(super) struct Insertion {
    parts: RefCell<Option<(Retained<NSBox>, Retained<NSBox>)>>,
}

impl Insertion {
    /// Takes the line away.
    pub(super) fn clear(&self) {
        if let Some((line, ring)) = self.parts.take() {
            line.removeFromSuperview();
            ring.removeFromSuperview();
        }
    }

    /// Stands the line with its middle at `y` (the list's space), or takes it away (`None`).
    fn set(&self, list: &NSView, at: Option<f64>, palette: Palette) {
        let Some(y) = at else {
            self.clear();
            return;
        };
        let mut parts = self.parts.borrow_mut();
        let (line, ring) = parts.get_or_insert_with(|| {
            let mtm = list.mtm();
            let line = plain_box(mtm, Tint::of(palette.accent, 1.0), tabs::GAP / 2.0);
            let ring = plain_box(mtm, Tint::of(palette.accent, 0.0), RING / 2.0);
            ring.setBorderWidth(RING_LINE);
            list.addSubview(&line);
            list.addSubview(&ring);
            (line, ring)
        });
        let accent = Tint::of(palette.accent, 1.0).color();
        line.setFillColor(&accent);
        ring.setBorderColor(&accent);
        // The ring hangs out of the row's edge by its half, the line begins where it ends.
        let ring_x = tabs::EDGE - RING_LINE - 1.0;
        let line_x = ring_x + RING;
        ring.setFrame(NSRect::new(
            NSPoint::new(ring_x, y - RING / 2.0),
            NSSize::new(RING, RING),
        ));
        line.setFrame(NSRect::new(
            NSPoint::new(line_x, y - tabs::GAP / 2.0),
            NSSize::new((tabs::EDGE + tabs::ROW_WIDTH - line_x).max(0.0), tabs::GAP),
        ));
    }
}

impl TabBar {
    /// Puts the bar in the side column (`Some(top)`: the title row's height, where the list
    /// starts) or back in the title row (`None`). A change of place forgets what belonged to the
    /// other: the scroll, the layout, a drag, a gap, the card.
    pub(crate) fn set_side(&self, top: Option<f64>) {
        let iv = self.ivars();
        let before = iv.side.replace(top);
        if before.is_some() == top.is_some() {
            return;
        }
        self.close_card();
        iv.scroll.set(0.0);
        iv.revealed.set(None);
        iv.overflowed.set(false);
        iv.width.set(0.0);
        iv.laid.replace(None);
        iv.column.replace(None);
        iv.drag.set(None);
        iv.gap.set(None);
        iv.target.set(None);
        iv.dwelling.set(None);
        iv.mark.set(None);
        iv.insertion.clear();
        if top.is_some() {
            iv.fade_leading.setHidden(true);
            iv.fade_trailing.setHidden(true);
            iv.list.setHidden(true);
        } else {
            // The bar's own: the list had no mask and New Tab a square foot.
            iv.mask.clear(&iv.strip);
            iv.new_tab.set_foot(None);
            iv.strip.setNeedsDisplay(true);
        }
    }

    /// Places every part from the pure column ([`Side::layout`]) and gives the chips their tabs
    /// as rows. [`TabBar::lay_out`]'s other half: the same things in the same order, a list that
    /// scrolls to the selection and slides there when the applier changed the list, and the
    /// clock set again after it.
    pub(super) fn lay_out_column(
        &self,
        top: f64,
        shown: &Shown,
        chips: &[Retained<Chip>],
        glowing: &[bool],
        reflow: bool,
        flight: Flight,
    ) {
        let iv = self.ivars();
        let (Some(palette), Some(theme)) = (iv.palette.get(), iv.theme.get()) else {
            return;
        };
        let count = shown.tabs.len();
        let bounds = self.bounds();
        let position = |tab: u64| shown.tabs.iter().position(|label| label.tab == tab);
        let hovered = iv.hovered.get().and_then(position);
        // The held row, at the place it would take (`shown` is in that order already).
        let dragged = flight.drag.and_then(|drag| position(drag.tab));
        // The row's place in the layout: past the gap a carried tab opens, one on.
        let seat = |index: usize| tabs::seat(index, flight.gap);
        let side = self.side_for(top, count + usize::from(flight.gap.is_some()));
        let mut column = side.layout();
        let selected_tab = shown.tabs.get(shown.selected).map(|label| label.tab);
        let was_overflowing = iv.overflowed.replace(column.overflow);
        let resized = (iv.width.replace(bounds.size.height) - bounds.size.height).abs() > 0.5;
        let selection_changed = iv.revealed.replace(selected_tab) != selected_tab;
        // The selection comes into sight when it changes, and when the rows stop fitting or the
        // room they scroll in changes while they do not: only a change of the selection slides.
        let kept_in_sight = column.overflow && (!was_overflowing || resized);
        let mut revealed = false;
        if selection_changed || kept_in_sight {
            let target = column.revealing(seat(shown.selected));
            if target != column.scroll {
                column = Side {
                    scroll: target,
                    ..side
                }
                .layout();
                revealed = selection_changed;
            }
        }
        iv.scroll.set(column.scroll);

        let reduce = self.reduce_motion();
        let motion = Motion::of(reduce);
        let glide = iv.glide.take();
        let slide = ((reflow || revealed || glide) && motion.reflow > 0.0).then_some(motion.reflow);
        let visible = self
            .terminal_window()
            .is_some_and(|window| window.window_visible());
        let seen = Seen {
            theme,
            count,
            visible,
            reduce,
            style: IconStyle::current(self.mtm()),
            hints: iv.hints.get(),
        };
        // The rows live in the list's space: its top-left corner is theirs.
        iv.strip.setFrame(ns_rect(column.list));
        for (index, chip) in chips.iter().enumerate() {
            let label = &shown.tabs[index];
            let naming = iv.renaming.get() == Some(label.tab);
            let title = if naming {
                String::new()
            } else {
                label.title.clone()
            };
            let row = column.rows[seat(index)];
            // A row held and moving is at the pointer, at once; the rest slide.
            let carried = flight
                .drag
                .filter(|drag| !drag.torn && dragged == Some(index))
                .and_then(|drag| drag.at);
            let frame = NSRect::new(
                NSPoint::new(
                    row.x - column.list.x,
                    carried.map_or(row.y, |(top, _)| top) - column.list.y,
                ),
                NSSize::new(row.width, row.height),
            );
            let look = seen.look(
                label,
                &title,
                Standing {
                    index,
                    naming,
                    selected: index == shown.selected,
                    hovered: (hovered == Some(index)
                        || carried.is_some()
                        || iv.target.get() == Some(label.tab))
                        && !naming,
                    glowing: glowing[index],
                    single: false,
                    lifted: carried.is_some(),
                    ghost: flight
                        .drag
                        .is_some_and(|drag| drag.torn && dragged == Some(index)),
                    side: true,
                },
            );
            chip.set(
                frame,
                &look,
                palette,
                motion,
                slide.filter(|_| carried.is_none()),
            );
            if naming {
                self.place_row_field(row, palette);
            }
        }
        iv.strip.set_separators(Vec::new(), palette);
        let scale = self
            .window()
            .map_or(2.0, |window| window.backingScaleFactor());
        iv.mask.set(&iv.strip, &column, scale);

        iv.new_tab.setFrame(ns_rect(column.new_tab));
        iv.new_tab.set_look(palette, TAB_RADIUS);
        iv.new_tab.set_foot(Some(column.fit.radius));
        iv.list.setHidden(true);
        match column.warning {
            Some(button) => {
                iv.warning.setFrame(ns_rect(button));
                iv.warning.set_look(palette, TAB_RADIUS);
                let text = NSString::from_str(&shown.notice);
                iv.warning.setToolTip(Some(&text));
                iv.warning.setAccessibilityLabel(Some(&text));
                iv.warning.setHidden(false);
            }
            None => iv.warning.setHidden(true),
        }
        iv.insertion.set(
            &iv.strip,
            // `between` is the gap's top; the line's middle is half a gap down.
            iv.mark
                .get()
                .map(|gap| column.between(gap) + tabs::GAP / 2.0 - column.list.y),
            palette,
        );
        iv.column.replace(Some(column));
        // An open card follows its row (a scroll, a new height, a closed tab above).
        self.place_card();
        self.arm_clock();
    }

    /// The pure column's input for `count` places: the window's height, where the list starts, the
    /// settings diagnostic and the corner, and the scroll as it stands
    /// ([`TabBar::bar_for`]'s upright half).
    pub(super) fn side_for(&self, top: f64, count: usize) -> Side {
        let iv = self.ivars();
        Side {
            height: self.bounds().size.height,
            top,
            count,
            scroll: iv.scroll.get(),
            warning: !iv.shown.borrow().notice.is_empty(),
            corner: card::window_corner_pt(self.window().as_deref()),
        }
    }

    /// The column as the window's list lays it out — without a drag's preview or a gap — with
    /// `room` more places than it has tabs: 0 is the column as it is, 1 the column a carried tab
    /// would make ([`TabBar::strip_with_room`]'s upright half).
    pub(super) fn column_with_room(&self, room: usize) -> Option<Column> {
        let top = self.ivars().side.get()?;
        let count = self.ivars().shown.borrow().tabs.len();
        (count > 0).then(|| self.side_for(top, count + room).layout())
    }

    /// Puts the name field over `row` (the bar's space), in the type the selected row's title
    /// wears, left-aligned where the row's leading element starts: the field covers icon and
    /// title alike.
    fn place_row_field(&self, row: Rect, palette: Palette) {
        let field = &self.ivars().field;
        field.setAlignment(NSTextAlignment::Left);
        self.dress_field(palette, false);
        let height = field.intrinsicContentSize().height;
        let covers = tabs::Row::field();
        field.setFrame(NSRect::new(
            NSPoint::new(
                row.x + covers.x,
                row.y + ((row.height - height) / 2.0).max(0.0),
            ),
            NSSize::new(covers.width, height),
        ));
    }
}
