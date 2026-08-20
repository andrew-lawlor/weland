//! TOC sidebar: a flat, indented list of buttons built from
//! `table_of_contents`, grouped by `parent_id` — same grouping the web
//! reader's `renderToc` uses to build nested `<ol>`s, just rendered here as
//! indentation on a flat `GtkBox` rather than a real tree widget
//! (`GtkTreeListModel` is overkill for a first pass — see the rewrite plan).

use std::collections::HashMap;
use std::rc::Rc;

use gtk4::{self as gtk, prelude::*, Align, Box as GtkBox, Button, Orientation, PolicyType, ScrolledWindow};
use weland::schema::TocEntry;

const INDENT_PER_LEVEL: i32 = 16;

/// Builds the TOC sidebar. `on_jump` fires with a `target_node_id` when an
/// entry is clicked; entries with no target (heading-less href-only entries)
/// render disabled since there's no GTK equivalent of following an href.
pub fn build_toc<F>(entries: &[TocEntry], on_jump: F) -> ScrolledWindow
where
    F: Fn(i64) + 'static,
{
    let on_jump: Rc<dyn Fn(i64)> = Rc::new(on_jump);

    let mut by_parent: HashMap<Option<i64>, Vec<&TocEntry>> = HashMap::new();
    for entry in entries {
        by_parent.entry(entry.parent_id).or_default().push(entry);
    }

    let list = GtkBox::new(Orientation::Vertical, 2);
    list.set_margin_top(8);
    list.set_margin_bottom(8);
    list.set_margin_start(8);
    list.set_margin_end(8);

    append_children(&list, &by_parent, None, 0, &on_jump);

    ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(PolicyType::Never)
        .width_request(220)
        .build()
}

fn append_children(
    list: &GtkBox,
    by_parent: &HashMap<Option<i64>, Vec<&TocEntry>>,
    parent: Option<i64>,
    depth: i32,
    on_jump: &Rc<dyn Fn(i64)>,
) {
    let Some(children) = by_parent.get(&parent) else { return };
    for entry in children {
        let button = Button::builder().label(&entry.title).has_frame(false).build();
        if let Some(label) = button.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
            label.set_halign(Align::Start);
            label.set_wrap(true);
        }
        button.set_margin_start(depth * INDENT_PER_LEVEL);

        match entry.target_node_id {
            Some(target) => {
                let on_jump = on_jump.clone();
                button.connect_clicked(move |_| on_jump(target));
            }
            None => button.set_sensitive(false),
        }

        list.append(&button);
        append_children(list, by_parent, Some(entry.id), depth + 1, on_jump);
    }
}
