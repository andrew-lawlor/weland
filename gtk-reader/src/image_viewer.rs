//! Full-size image inspection: clicking an inline image (see
//! `document.rs`'s `insert_image`) opens it at (near-)native resolution in
//! its own dialog, with scroll-wheel zoom and scrollbar panning.
//!
//! Deliberately reuses the already-decoded `gdk::Paintable` off the inline
//! `Picture` (`source.paintable()`) rather than re-fetching and re-decoding
//! the asset from the database -- the inline image is already full
//! resolution under the hood (only its *displayed* size is clamped, by
//! `document.rs`'s `wire_image_centering`), so there's nothing to re-decode.
//!
//! No click-drag panning: several implementations were tried (adjustment-
//! based panning via `ScrolledWindow`; manual `gtk::Fixed::move_`
//! positioning) across two different containers (`adw::Dialog`,
//! `adw::Window`), with the drag gesture's event sequence explicitly
//! claimed -- every combination produced the same reproducible ghosting (a
//! translucent drag-icon copy of the image, confirmed by direct report,
//! present only in this dialog and nowhere else on the desktop). Since
//! switching container *and* positioning mechanism independently both
//! failed to change the symptom, the one constant across every attempt was
//! a `GestureDrag` on the `Picture` itself -- so this drops click-drag
//! entirely rather than keep guessing at GTK/libadwaita internals no
//! available tooling could actually observe. Panning is scrollbars only,
//! which were never implicated in any of the failed attempts.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::{self as gtk, glib, prelude::*, Picture, ScrolledWindow};
use libadwaita::{self as adw, prelude::*};

const MIN_SCALE: f64 = 0.1;
const MAX_SCALE: f64 = 4.0;
const ZOOM_STEP: f64 = 1.1;
// The dialog's own size, not the image's -- large plates start scaled down
// to fit inside this; a small image just shows at its native size (see
// `open_viewer`'s `fit_scale`), with the dialog sized around it instead of
// stretched up and blurry.
const VIEWPORT_WIDTH: f64 = 900.0;
const VIEWPORT_HEIGHT: f64 = 700.0;

/// Wires `picture` (an inline reading-pane image) so clicking it opens a
/// full-size zoom view over `parent`. Safe to call before `picture` has
/// finished its lazy decode -- see `open_viewer`'s early return.
pub fn wire_click_to_open(parent: &impl IsA<gtk::Widget>, picture: &Picture) {
    picture.set_cursor_from_name(Some("zoom-in"));

    let parent = parent.clone().upcast::<gtk::Widget>();
    let click = gtk::GestureClick::new();
    let picture_c = picture.clone();
    click.connect_released(move |_, n_press, _, _| {
        if n_press == 1 {
            open_viewer(&parent, &picture_c);
        }
    });
    picture.add_controller(click);
}

fn open_viewer(parent: &gtk::Widget, source: &Picture) {
    // Not yet decoded (lazy loading hasn't reached this one yet) or decode
    // failed -- nothing to show.
    let Some(paintable) = source.paintable() else { return };
    let (iw, ih) = (paintable.intrinsic_width(), paintable.intrinsic_height());
    if iw <= 0 || ih <= 0 {
        return;
    }

    let picture = Picture::new();
    picture.set_paintable(Some(&paintable));
    picture.set_can_shrink(true);

    let fit_scale = (VIEWPORT_WIDTH / iw as f64).min(VIEWPORT_HEIGHT / ih as f64).min(1.0);
    let scale = Rc::new(Cell::new(fit_scale));
    let zoom_label = gtk::Label::new(Some(&format_zoom(fit_scale)));
    resize_picture(&picture, iw, ih, fit_scale);

    let scroller = ScrolledWindow::builder().child(&picture).hexpand(true).vexpand(true).build();

    let fit_btn = gtk::Button::with_label("Fit");
    let actual_btn = gtk::Button::with_label("1:1");

    let header = adw::HeaderBar::new();
    header.pack_start(&fit_btn);
    header.pack_start(&actual_btn);
    header.set_title_widget(Some(&zoom_label));

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&scroller));

    let dialog = adw::Dialog::new();
    dialog.set_presentation_mode(adw::DialogPresentationMode::Floating);
    dialog.set_content_width(VIEWPORT_WIDTH as i32);
    dialog.set_content_height(VIEWPORT_HEIGHT as i32);
    dialog.set_child(Some(&toolbar_view));

    let set_scale = {
        let picture = picture.clone();
        let scale = scale.clone();
        let zoom_label = zoom_label.clone();
        move |new_scale: f64| {
            let new_scale = new_scale.clamp(MIN_SCALE, MAX_SCALE);
            scale.set(new_scale);
            resize_picture(&picture, iw, ih, new_scale);
            zoom_label.set_label(&format_zoom(new_scale));
        }
    };

    {
        let set_scale = set_scale.clone();
        fit_btn.connect_clicked(move |_| set_scale(fit_scale));
    }
    {
        let set_scale = set_scale.clone();
        actual_btn.connect_clicked(move |_| set_scale(1.0));
    }

    // Scroll wheel zooms rather than scrolls -- panning is via the
    // scrollbars instead (see this file's top comment for why there's no
    // click-drag panning).
    let scroll_controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    {
        let scale = scale.clone();
        scroll_controller.connect_scroll(move |_, _dx, dy| {
            let factor = if dy < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
            set_scale(scale.get() * factor);
            glib::Propagation::Stop
        });
    }
    picture.add_controller(scroll_controller);

    dialog.present(Some(parent));
}

fn resize_picture(picture: &Picture, iw: i32, ih: i32, scale: f64) {
    let w = ((iw as f64) * scale).round().max(1.0) as i32;
    let h = ((ih as f64) * scale).round().max(1.0) as i32;
    picture.set_size_request(w, h);
}

fn format_zoom(scale: f64) -> String {
    format!("{:.0}%", scale * 100.0)
}
