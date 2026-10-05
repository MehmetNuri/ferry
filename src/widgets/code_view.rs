#[allow(unused_imports)]
use gtk::prelude::*;

pub fn new(text: &str, file_name: &str) -> gtk::TextView {
    #[cfg(feature = "sourceview")]
    {
        use gtk::glib;
        use sourceview5::prelude::*;
        let buffer = sourceview5::Buffer::new(None::<&gtk::TextTagTable>);
        let head = &text.as_bytes()[..text.len().min(4096)];
        let (content_type, _) = gtk::gio::content_type_guess(Some(file_name), Some(head));
        if let Some(language) =
            sourceview5::LanguageManager::default().guess_language(Some(file_name), Some(content_type.as_str()))
        {
            buffer.set_language(Some(&language));
        }
        let style = adw::StyleManager::default();
        let apply = |buffer: &sourceview5::Buffer, dark: bool| {
            let scheme =
                sourceview5::StyleSchemeManager::default().scheme(if dark { "Adwaita-dark" } else { "Adwaita" });
            buffer.set_style_scheme(scheme.as_ref());
        };
        apply(&buffer, style.is_dark());
        style.connect_dark_notify(glib::clone!(
            #[weak]
            buffer,
            move |style| apply(&buffer, style.is_dark())
        ));
        buffer.set_highlight_matching_brackets(false);
        buffer.begin_irreversible_action();
        buffer.set_text(text);
        buffer.end_irreversible_action();
        let view = sourceview5::View::with_buffer(&buffer);
        view.set_show_line_numbers(text.lines().count() > 1);
        view.set_monospace(true);
        view.upcast()
    }
    #[cfg(not(feature = "sourceview"))]
    {
        let _ = file_name;
        let view = gtk::TextView::builder().monospace(true).build();
        view.buffer().set_text(text);
        view
    }
}

pub fn label_icon_buttons(root: &impl IsA<gtk::Widget>) {
    let mut stack = vec![root.as_ref().clone()];
    while let Some(widget) = stack.pop() {
        let icon_only = match widget.downcast_ref::<gtk::Button>() {
            Some(button) => button.label().is_none_or(|l| l.is_empty()),
            None => widget.downcast_ref::<gtk::MenuButton>().is_some_and(|b| b.label().is_none_or(|l| l.is_empty())),
        };
        if icon_only && let Some(tip) = widget.tooltip_text() {
            widget.update_property(&[gtk::accessible::Property::Label(&tip)]);
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
}
