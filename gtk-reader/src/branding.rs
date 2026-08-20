//! The forge-anvil GIF, carried over from the old Tauri reader
//! (`reader/dist/forge-anvil.gif`) — on-theme for "Weland" (see the crate
//! doc comment / README), shown in the library's empty state and next to
//! the import status while an EPUB is being compiled.
//!
//! GTK4 has no built-in animated-GIF widget: `gtk::Picture` takes a single
//! `gdk::Paintable` frame, and `gtk::MediaFile` (GStreamer-backed, already
//! used for voice-note playback) can't decode GIF on this system — confirmed
//! via `gst-discoverer-1.0`, which reports a missing `decoder-image/gif`
//! plugin. Requiring a second GStreamer plugin install just for a decorative
//! animation isn't worth it when `gdk_pixbuf::PixbufAnimation` (already a
//! hard gtk4 dependency, no new install) does the job directly: it decodes
//! the GIF's own per-frame timing, and this just drives a `gtk::Picture`
//! from it on a repeating timeout.

use gdk_pixbuf::prelude::*;
use gtk4::{gdk_pixbuf, gio, glib, Picture};

static FORGE_ANVIL_GIF: &[u8] = include_bytes!("../resources/forge-anvil.gif");

/// Builds a `Picture` playing the forge-anvil animation on a loop. Returns
/// `None` (logging why) if the bundled GIF somehow fails to decode — the
/// caller should treat that as "skip the decoration," not an error worth
/// surfacing to the user.
pub fn forge_anvil_picture() -> Option<Picture> {
    let bytes = glib::Bytes::from_static(FORGE_ANVIL_GIF);
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    let animation = match gdk_pixbuf::PixbufAnimation::from_stream(&stream, gio::Cancellable::NONE) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("failed to decode bundled forge-anvil.gif: {e}");
            return None;
        }
    };

    let iter = animation.iter(None);
    // Sizing (can-shrink, content-fit, size-request) is left to the caller
    // — it's placed differently depending on where this ends up.
    let picture = Picture::for_pixbuf(&iter.pixbuf());

    // A plain `move || { picture.clone() ... }` closure would hold a
    // *strong* ref to `picture` for as long as the timeout source runs —
    // since nothing here ever calls `remove()` on the source, that would
    // keep the widget (and everything it's parented under) alive for the
    // rest of the process even after it's removed from the UI. Capturing a
    // weak ref and stopping the timer once it fails to upgrade (the widget
    // has been dropped) avoids that leak.
    let weak_picture = picture.downgrade();
    glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
        let Some(picture) = weak_picture.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if iter.advance(std::time::SystemTime::now()) {
            picture.set_pixbuf(Some(&iter.pixbuf()));
        }
        glib::ControlFlow::Continue
    });

    Some(picture)
}
