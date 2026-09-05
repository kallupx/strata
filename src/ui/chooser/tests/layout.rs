// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires a GTK display and isolated XDG directories; run this test alone"]
fn options_share_a_compact_row_and_wrap_in_narrow_windows() {
    gtk::init().expect("GTK display");
    crate::ui::prepare_portal_ui();
    let context = glib::MainContext::default();
    let _owner = context.acquire().expect("exclusive GTK context");
    for (width, wraps) in [(900, false), (320, true)] {
        let options = chooser_options();
        options.set_valign(gtk::Align::Start);
        let filter = ChooserDropdown::new(&["Text files", "Images"], 0);
        append_option(
            &options,
            &labeled_row("Filter", Some(filter.button.upcast_ref())),
        );
        let choices = build_choices(
            &[
                Choice::new("encoding", "Encoding", "utf8")
                    .insert("utf8", "UTF-8")
                    .insert("latin1", "Latin-1"),
                Choice::boolean("compress", "Compress files", false),
            ],
            &options,
        );
        let window = gtk::Window::builder()
            .default_width(width)
            .default_height(160)
            .child(&options)
            .build();
        window.present();
        let first = options.first_child().expect("filter group");
        let second = first.next_sibling().expect("encoding group");
        let third = second.next_sibling().expect("compression group");
        let deadline = Instant::now() + Duration::from_secs(5);
        while first.width() == 0 || third.height() == 0 {
            while context.pending() {
                context.iteration(false);
            }
            assert!(Instant::now() < deadline, "options are allocated");
            std::thread::sleep(Duration::from_millis(5));
        }
        let bounds = [&first, &second, &third]
            .map(|child| child.compute_bounds(&options).expect("option bounds"));
        if wraps {
            assert!(
                bounds[2].y() > bounds[0].y() + bounds[0].height(),
                "options wrap instead of clipping"
            );
        } else {
            let centers = bounds.map(|bounds| bounds.y() + bounds.height() / 2.0);
            assert!(
                centers
                    .iter()
                    .all(|center| (*center - centers[0]).abs() <= 1.0),
                "all options share one row"
            );
            assert!(
                options.height() < 42,
                "the options row is shorter than a standard form field"
            );
        }
        assert_eq!(choices[0].value(), ("encoding".into(), "utf8".into()));
        assert_eq!(choices[1].value(), ("compress".into(), "false".into()));
        for child in [&first, &second, &third] {
            assert!(
                !child.is_focusable(),
                "Tab targets controls, not layout wrappers"
            );
        }
        window.destroy();
    }
}
