//! File launcher shown when no input image was supplied.

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use anyhow::Result;
use relm4::gtk::{self, gdk_pixbuf::Pixbuf, prelude::*};

/// Run the launcher before starting the editor. Closing it returns no image;
/// failed loads stay here so the user can choose another file.
pub fn choose_image() -> Result<Option<(Pixbuf, PathBuf)>> {
    gtk::init()?;
    let window = gtk::Window::builder()
        .title("Tensaku")
        .default_width(480)
        .default_height(320)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_halign(gtk::Align::Center);
    content.set_valign(gtk::Align::Center);
    content.set_margin_start(24);
    content.set_margin_end(24);
    let open = gtk::Button::with_label("Open Image…");
    open.add_css_class("suggested-action");
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.set_max_width_chars(48);
    error.set_visible(false);
    content.append(&open);
    content.append(&error);
    window.set_child(Some(&content));

    let chooser = gtk::FileChooserNative::builder()
        .title("Open Image")
        .transient_for(&window)
        .modal(true)
        .action(gtk::FileChooserAction::Open)
        .accept_label("Open")
        .cancel_label("Cancel")
        .build();
    let images = gtk::FileFilter::new();
    images.set_name(Some("Images"));
    images.add_pixbuf_formats();
    chooser.add_filter(&images);
    let all_files = gtk::FileFilter::new();
    all_files.set_name(Some("All files"));
    all_files.add_pattern("*");
    chooser.add_filter(&all_files);

    let result = Rc::new(RefCell::new(None));
    let selected = result.clone();
    let weak_window = window.downgrade();
    chooser.connect_response(move |chooser, response| {
        chooser.hide();
        if response != gtk::ResponseType::Accept {
            return;
        }
        let Some(path) = chooser.file().and_then(|file| file.path()) else {
            error.set_text("Choose an image stored on this computer.");
            error.set_visible(true);
            return;
        };
        match Pixbuf::from_file(&path) {
            Ok(image) => {
                *selected.borrow_mut() = Some((image, path));
                if let Some(window) = weak_window.upgrade() {
                    window.close();
                }
            }
            Err(reason) => {
                error.set_text(&format!("Could not open image: {reason}"));
                error.set_visible(true);
            }
        }
    });
    let open_chooser = chooser.clone();
    open.connect_clicked(move |_| open_chooser.show());

    let main_loop = gtk::glib::MainLoop::new(None, false);
    let close_loop = main_loop.clone();
    window.connect_close_request(move |_| {
        chooser.destroy();
        close_loop.quit();
        gtk::glib::Propagation::Proceed
    });
    window.present();
    main_loop.run();
    Ok(result.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires a GTK display"]
    fn closing_launcher_without_a_file_exits_cleanly() {
        gtk::init().unwrap();
        gtk::glib::idle_add_local_once(|| {
            let window = gtk::Window::list_toplevels()
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
                .find(|window| window.title().as_deref() == Some("Tensaku"))
                .expect("launcher should be visible");
            assert!(window.is_visible());
            window.close();
        });
        assert!(choose_image().unwrap().is_none());
    }
}
