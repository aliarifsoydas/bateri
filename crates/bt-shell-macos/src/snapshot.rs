//! A pane's still as an image: the pixels `bt_gpu::DisplayLink::snapshot` reads
//! back, as a `CGImage` a layer can take as its contents.
//!
//! The pixels are the sRGB-encoded values the GPU wrote (the renderer's
//! target is an sRGB format and the shrink chain averages in linear light), so
//! the image says so — a device-RGB tag would let the display re-interpret
//! them and the card's picture would drift from the pane's colours. Alpha is
//! skipped: a pane is opaque.

use bt_gpu::Snapshot;
use objc2_core_foundation::{CFData, CFRetained};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    kCGColorSpaceSRGB,
};

/// `still` as an image, `None` if Core Graphics refuses it (a size it cannot
/// make). The bytes are copied into the image's own data.
pub(crate) fn image(still: &Snapshot) -> Option<CFRetained<CGImage>> {
    let (width, height) = (still.width as usize, still.height as usize);
    if width == 0 || height == 0 || still.rgba.len() != width * height * 4 {
        return None;
    }
    let data = CFData::from_bytes(&still.rgba);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    // SAFETY: Core Graphics' constant name for the sRGB space, alive for the
    // process.
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: the sizes describe `provider`'s data exactly (checked above:
    // four bytes a pixel, rows tight) and `decode` is null, which Core
    // Graphics reads as "no decode array".
    unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            width * 4,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::NoneSkipLast.0),
            Some(&provider),
            std::ptr::null(),
            true,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}
