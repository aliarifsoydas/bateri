//! A title that does not fit its place: it fades out at its end instead of ending in "…", and with
//! the pointer resting on it slides to show the rest. The chip's title, the side row's and the
//! group button's name are one [`Title`]; the group headers of the column and the lists keep the
//! "…" of an ordinary label.
//!
//! **The arithmetic is `tabs::title_slide`** (whether it fits, how far it goes, how long that
//! takes); this is how it is drawn. The title is a label as wide as its text, in a host view that
//! is as wide as its place and clips: where the text does not fit, the host's layer carries a mask
//! that runs out over the last [`tabs::TITLE_FADE`] points, and the label stands at the left end
//! of it. A text that fits is a label the host's size, as before.
//!
//! **The slide is a Core Animation animation, played by the window server**: one
//! `CABasicAnimation` on the label's layer, begun [`tabs::TITLE_DWELL`] after the pointer comes,
//! linear at [`tabs::TITLE_SPEED`], held at its end by its fill mode. It stops on the pointer's
//! leaving (the label goes back over [`tabs::TITLE_RETURN`]), on the title's changing, on the
//! reduce-motion setting turning on, and on the animation's own end — it is not an app clock, asks
//! no frame of any pane and sets no timer, so a window with a title waiting for the pointer is as
//! idle as one without. With Reduce Motion there is no slide: the fade stays, and the chip says
//! the whole title in its tooltip (the accessibility setting adds no animation).
//!
//! The host never takes a press: the view under it decides what a press means.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSBezierPath, NSColor, NSLineBreakMode, NSTextAlignment, NSTextField, NSView};
use objc2_foundation::{NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, ns_string};
use objc2_quartz_core::{
    CABasicAnimation, CACurrentMediaTime, CALayer, CAMediaTiming, CAMediaTimingFunction,
    kCAFillModeForwards,
};

use super::{Ease, label, paint_in, play, still};
use crate::tabs::{self, Slide};

define_class!(
    // SAFETY: NSView is designed for subclassing; TitleHost has no ivars and implements no `Drop`
    // and is born with `initWithFrame:`.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "BateriTitleHost"]
    struct TitleHost;

    unsafe impl NSObjectProtocol for TitleHost {}

    impl TitleHost {
        /// A title never takes a press, wherever it stands: what is under it does.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<NSView>> {
            // No `return`: `define_class!` converts the body's last expression.
            None
        }
    }
);

impl TitleHost {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `initWithFrame:` is NSView's designated initializer and there are no ivars.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }
}

/// What the mask's picture was painted for: the host's size and the display's scale.
type Painted = (f64, f64, f64);

/// A title: a host view of the place's size, the label in it, and what the fade and the slide
/// have to remember.
pub(super) struct Title {
    host: Retained<TitleHost>,
    text: Retained<NSTextField>,
    /// How the text stands in its place while it fits: centred in a chip, at the left in a button.
    rest: NSTextAlignment,
    /// The host's mask while the text does not fit, and what its picture was painted for.
    mask: RefCell<Option<Retained<CALayer>>>,
    painted: Cell<Option<Painted>>,
    /// What the text does in its place now: `None` while it fits.
    slide: Cell<Option<Slide>>,
    /// The slide is begun (waiting for its dwell to pass, or moving, or at its end).
    going: Cell<bool>,
    /// Reduce Motion, as the last placing was told.
    reduce: Cell<bool>,
}

impl Title {
    /// A title whose text stands as `rest` while it fits.
    pub(super) fn new(mtm: MainThreadMarker, rest: NSTextAlignment) -> Self {
        let text = label(mtm);
        // The text is set as wide as it is: nothing truncates it, the host's edge and the mask
        // are what cut it.
        text.setLineBreakMode(NSLineBreakMode::ByClipping);
        text.setAlignment(rest);
        text.setWantsLayer(true);
        let host = TitleHost::new(mtm);
        host.setWantsLayer(true);
        host.setClipsToBounds(true);
        host.addSubview(&text);
        Self {
            host,
            text,
            rest,
            mask: RefCell::new(None),
            painted: Cell::new(None),
            slide: Cell::new(None),
            going: Cell::new(false),
            reduce: Cell::new(false),
        }
    }

    /// The view that goes into the chip or the button, where the label went.
    pub(super) fn view(&self) -> &NSView {
        &self.host
    }

    /// The label: its font, ink and text are set on it, and its fitting size is the text's width.
    pub(super) fn field(&self) -> &NSTextField {
        &self.text
    }

    /// Whether the text does not fit its place (it fades at its end).
    pub(super) fn overflows(&self) -> bool {
        self.slide.get().is_some()
    }

    /// Puts the title in `frame` (the host's, in its parent's space) with its text `natural`
    /// wide as the type sets it, and slides it if `hovered` and it can.
    ///
    /// A slide under way goes on while the distance is the same, so a program that rewrites its
    /// title every second leaves the name sliding. A distance that changed under a slide that is
    /// still wanted begins it again (from the left, after the dwell); under one that no longer is
    /// wanted the text goes back from where it has got to.
    pub(super) fn place(&self, frame: NSRect, natural: f64, hovered: bool, reduce: bool) {
        let slide = tabs::title_slide(natural, frame.size.width);
        let was = self.slide.replace(slide);
        self.reduce.set(reduce);
        let wanted = hovered && !reduce && slide.is_some();
        if self.going.get() && was != slide {
            if wanted {
                self.stop();
            } else {
                self.release();
            }
        }
        still(|| {
            self.host.setFrame(frame);
            let size = frame.size;
            match slide {
                Some(_) => {
                    self.text.setAlignment(NSTextAlignment::Left);
                    self.text.setFrame(NSRect::new(
                        NSPoint::ZERO,
                        NSSize::new(natural.ceil().max(size.width), size.height),
                    ));
                    self.fade(size);
                }
                None => {
                    self.text.setAlignment(self.rest);
                    self.text.setFrame(NSRect::new(NSPoint::ZERO, size));
                    self.unfade();
                }
            }
        });
        self.follow(wanted);
    }

    /// The pointer came to (`true`) or left the place the title stands in, for a title whose
    /// place is not laid out again by the pointer's coming ([`Title::place`] is).
    pub(super) fn hover(&self, hovered: bool, reduce: bool) {
        self.reduce.set(reduce);
        self.follow(hovered && !reduce && self.slide.get().is_some());
    }

    /// Begins the slide that is wanted and not begun, lets go of the one that is begun and not
    /// wanted.
    fn follow(&self, wanted: bool) {
        match (wanted, self.going.get(), self.slide.get()) {
            (true, false, Some(slide)) => self.begin(slide),
            (false, true, _) => self.release(),
            _ => {}
        }
    }

    /// The slide: from where the text stands to the distance it goes, after the dwell, linear,
    /// and held there — what the model has (the text at rest) comes back only when it is taken
    /// off.
    fn begin(&self, slide: Slide) {
        let Some(layer) = self.text.layer() else {
            return;
        };
        let animation =
            CABasicAnimation::animationWithKeyPath(Some(ns_string!("transform.translation.x")));
        // SAFETY: plain `NSNumber`s, what this key path takes; a constant `NSString` Core
        // Animation exposes, only read.
        unsafe {
            animation.setFromValue(Some(&NSNumber::numberWithDouble(0.0)));
            animation.setToValue(Some(&NSNumber::numberWithDouble(-slide.distance)));
            animation.setFillMode(kCAFillModeForwards);
        }
        animation.setDuration(slide.secs);
        animation.setTimingFunction(Some(&CAMediaTimingFunction::functionWithControlPoints(
            0.0, 0.0, 1.0, 1.0,
        )));
        animation.setBeginTime(
            layer.convertTime_fromLayer(CACurrentMediaTime(), None) + tabs::TITLE_DWELL,
        );
        animation.setRemovedOnCompletion(false);
        // Both ride the same key path: one begun while the way back is still playing (the pointer
        // came back at once) takes over only after the dwell, which is longer than the way back
        // ([`tabs::TITLE_DWELL`] against [`tabs::TITLE_RETURN`]) — the order that keeps the text
        // from jumping, and the reason the two are not tuned apart.
        layer.addAnimation_forKey(&animation, Some(ns_string!("slide")));
        self.going.set(true);
    }

    /// The slide is over: the text goes back to its place from where it has got to (nowhere, if
    /// the dwell had not passed) — at once under Reduce Motion, which adds no animation, not
    /// even the way back.
    fn release(&self) {
        if self.reduce.get() {
            self.stop();
            return;
        }
        self.going.set(false);
        let Some(layer) = self.text.layer() else {
            return;
        };
        // SAFETY: reading the presentation copy of a layer we hold, on the main thread.
        let at = unsafe { layer.presentationLayer() }.map_or(0.0, |shown| shown.transform().m41);
        layer.removeAnimationForKey(ns_string!("slide"));
        if at.abs() >= 0.5 {
            play(
                &layer,
                "transform.translation.x",
                at,
                0.0,
                tabs::TITLE_RETURN,
                Ease::Out,
                0.0,
            );
        }
    }

    /// The slide is over for good, and so is the way back: the text stands at its place at once.
    fn stop(&self) {
        self.going.set(false);
        if let Some(layer) = self.text.layer() {
            layer.removeAnimationForKey(ns_string!("slide"));
            layer.removeAnimationForKey(ns_string!("transform.translation.x"));
        }
    }

    /// The host's mask for a place of `size` that its text does not fit: opaque, running out over
    /// the last [`tabs::title_fade`] points at the right end. The picture is painted when the
    /// size or the scale changes, not at every placing.
    fn fade(&self, size: NSSize) {
        let Some(host) = self.host.layer() else {
            return;
        };
        let scale = self
            .host
            .window()
            .map_or(2.0, |window| window.backingScaleFactor());
        let mut slot = self.mask.borrow_mut();
        let fresh = slot.is_none();
        let layer = slot.get_or_insert_with(CALayer::new);
        layer.setFrame(NSRect::new(NSPoint::ZERO, size));
        let painted = (size.width, size.height, scale);
        if self.painted.replace(Some(painted)) != Some(painted) {
            let fade = tabs::title_fade(size.width);
            paint_in(layer, size, scale, move |rect| fade_picture(rect, fade));
        }
        if fresh {
            // SAFETY: a mask is any layer; this one is the host's alone and lives as long as the
            // slot holds it.
            unsafe { host.setMask(Some(layer)) };
        }
    }

    /// Takes the mask off: the text fits, and nothing needs hiding.
    fn unfade(&self) {
        if self.mask.take().is_none() {
            return;
        }
        self.painted.set(None);
        if let Some(host) = self.host.layer() {
            // SAFETY: taking a mask off, which any layer allows.
            unsafe { host.setMask(None) };
        }
    }
}

/// The mask's picture in `rect` (top-down): black — its alpha is what shows — from the left edge,
/// running out to nothing over the last `fade` points along a smooth step.
fn fade_picture(rect: NSRect, fade: f64) {
    let (width, height) = (rect.size.width, rect.size.height);
    let black = |alpha: f64| NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, alpha);
    let step = |t: f64| t * t * (3.0 - 2.0 * t);
    let solid = (width - fade).max(0.0);
    black(1.0).setFill();
    NSBezierPath::bezierPathWithRect(NSRect::new(NSPoint::ZERO, NSSize::new(solid, height))).fill();
    // A column each: the picture is painted when the place's size changes, not per frame.
    for column in 0..fade.ceil() as usize {
        let x = column as f64;
        black(1.0 - step(((x + 0.5) / fade).min(1.0))).setFill();
        NSBezierPath::bezierPathWithRect(NSRect::new(
            NSPoint::new(solid + x, 0.0),
            NSSize::new(1.0, height),
        ))
        .fill();
    }
}

#[cfg(test)]
mod tests {
    /// The slide is Core Animation's and nothing of ours: a title waiting for the pointer, sliding
    /// or at its end holds no timer, no thread and no frame of any pane (the app is idle with the
    /// pointer on it, `make smoke` cannot see it). A source scan, because a call added here
    /// breaks that silently.
    #[test]
    fn the_slide_asks_nothing_of_the_app() {
        let source = include_str!("tab_title.rs");
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("the source before its tests");
        let code: String = code
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "NSTimer",
            "dispatch",
            "Waker",
            "wake(",
            "resume(",
            "DisplayLink",
            "thread::",
        ] {
            assert!(
                !code.contains(forbidden),
                "tab_title.rs reaches for `{forbidden}`: the slide must stay in Core Animation"
            );
        }
        assert!(code.contains("CABasicAnimation"));
    }
}
