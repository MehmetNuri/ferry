use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use crate::i18n::tr;
use crate::window::Window;

#[derive(Clone, Default)]
pub struct TabState {
    pub profile: String,
    pub bucket: String,
    pub prefix: String,
    pub back: Vec<(String, String)>,
    pub forward: Vec<(String, String)>,
}

impl Window {
    pub(crate) fn setup_tabs(&self) {
        let imp = self.imp();
        let page = self.add_tab_page(None);
        imp.current_tab.replace(Some(page.clone()));
        self.move_browser_into(&page);
        imp.tab_view.connect_selected_page_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| win.tab_switched()
        ));
        imp.tab_view.connect_page_detached(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, page, _| {
                win.imp().tabs.borrow_mut().retain(|(p, _)| p != page);
            }
        ));
    }

    fn add_tab_page(&self, state: Option<TabState>) -> adw::TabPage {
        let imp = self.imp();
        let holder = adw::Bin::new();
        let position = imp.tab_view.selected_page().map(|p| imp.tab_view.page_position(&p) + 1).unwrap_or(0);
        let page = imp.tab_view.insert(&holder, position);
        page.set_title("Ferry");
        page.set_icon(Some(&gtk::gio::ThemedIcon::new("folder-symbolic")));
        imp.tabs.borrow_mut().push((page.clone(), state.unwrap_or_default()));
        page
    }

    fn move_browser_into(&self, page: &adw::TabPage) {
        let imp = self.imp();
        let Some(holder) = page.child().downcast::<adw::Bin>().ok() else { return };
        let browser: gtk::Widget = imp.browser_stack.get().upcast();
        if browser.parent().as_ref() == Some(holder.upcast_ref()) {
            return;
        }
        if let Some(parent) = browser.parent() {
            match parent.downcast::<adw::Bin>() {
                Ok(bin) => bin.set_child(None::<&gtk::Widget>),
                Err(other) => {
                    if let Ok(container) = other.downcast::<gtk::Box>() {
                        container.remove(&browser);
                    }
                }
            }
        }
        holder.set_child(Some(&browser));
    }

    fn current_state(&self) -> TabState {
        let imp = self.imp();
        TabState {
            profile: imp.client.borrow().as_ref().map(|c| c.profile.id.clone()).unwrap_or_default(),
            bucket: imp.bucket.borrow().clone(),
            prefix: imp.prefix.borrow().clone(),
            back: imp.history_back.borrow().clone(),
            forward: imp.history_forward.borrow().clone(),
        }
    }

    pub(crate) fn new_tab(&self) {
        let state = self.current_state();
        self.open_tab(state.bucket, state.prefix);
    }

    pub(crate) fn open_tab(&self, bucket: String, prefix: String) {
        let imp = self.imp();
        if let Some(current) = imp.current_tab.borrow().clone() {
            let state = self.current_state();
            if let Some(entry) = imp.tabs.borrow_mut().iter_mut().find(|(p, _)| *p == current) {
                entry.1 = state;
            }
        }
        let profile = imp.client.borrow().as_ref().map(|c| c.profile.id.clone()).unwrap_or_default();
        let page = self.add_tab_page(Some(TabState { profile, bucket, prefix, ..Default::default() }));
        imp.tab_view.set_selected_page(&page);
    }

    pub(crate) fn close_tab(&self) {
        let imp = self.imp();
        match imp.tab_view.selected_page() {
            Some(page) if imp.tab_view.n_pages() > 1 => imp.tab_view.close_page(&page),
            _ => self.close(),
        }
    }

    fn tab_switched(&self) {
        let imp = self.imp();
        let Some(page) = imp.tab_view.selected_page() else { return };
        let previous = imp.current_tab.replace(Some(page.clone()));
        if previous.as_ref() == Some(&page) {
            return;
        }
        if let Some(previous) = previous {
            let state = self.current_state();
            if let Some(entry) = imp.tabs.borrow_mut().iter_mut().find(|(p, _)| *p == previous) {
                entry.1 = state;
            }
        }
        self.move_browser_into(&page);
        let Some(state) = imp.tabs.borrow().iter().find(|(p, _)| *p == page).map(|(_, s)| s.clone()) else { return };
        let here = imp.client.borrow().as_ref().map(|c| c.profile.id.clone()).unwrap_or_default();
        if state.profile.is_empty() || state.bucket.is_empty() {
            return;
        }
        if state.profile != here {
            self.open_object(state.profile.clone(), state.bucket.clone(), state.prefix.clone());
            return;
        }
        imp.history_back.replace(state.back.clone());
        imp.history_forward.replace(state.forward.clone());
        imp.location.replace((state.bucket.clone(), state.prefix.clone()));
        self.go_to(&state.bucket, &state.prefix);
        self.update_history_buttons();
    }

    pub(crate) fn update_tab_title(&self) {
        let imp = self.imp();
        let Some(page) = imp.tab_view.selected_page() else { return };
        let bucket = imp.bucket.borrow().clone();
        let prefix = imp.prefix.borrow().clone();
        let title = prefix
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| if bucket.is_empty() { tr("No Bucket Open") } else { bucket.clone() });
        page.set_title(&title);
        page.set_tooltip(&glib::markup_escape_text(&format!("{bucket}/{prefix}")));
    }
}
