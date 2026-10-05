use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::i18n::{tr, trf, trn};
use crate::s3::{Entry, S3};
use crate::transfers::queue;
use crate::window::Window;

fn split(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

pub fn plan(entries: &[Entry], template: bool, find: &str, replace: &str, pattern: &str, start: u32) -> Vec<String> {
    let width = (start as usize + entries.len()).to_string().len().max(2);
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if template {
                let (stem, ext) = split(&e.name);
                let number = format!("{:0width$}", start as usize + i, width = width);
                let name = pattern.replace("{n}", &number).replace("{name}", stem);
                let name = if pattern.contains("{n}") { name } else { format!("{name} {number}") };
                format!("{name}{ext}")
            } else if find.is_empty() {
                e.name.clone()
            } else {
                e.name.replace(find, replace)
            }
        })
        .collect()
}

pub fn present(
    win: &Window,
    client: S3,
    bucket: String,
    prefix: String,
    entries: Vec<Entry>,
    existing: HashSet<String>,
) {
    let entries: Vec<Entry> = entries.into_iter().filter(|e| !e.is_folder).collect();
    if entries.is_empty() {
        return;
    }
    let dialog = adw::Dialog::builder()
        .title(trn("Rename {n} File", "Rename {n} Files", &[("n", &entries.len().to_string())]))
        .content_width(560)
        .content_height(620)
        .build();
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    let cancel = gtk::Button::with_label(&tr("Cancel"));
    let apply = gtk::Button::builder().label(tr("Rename")).css_classes(["suggested-action"]).build();
    header.pack_start(&cancel);
    header.pack_end(&apply);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    let page = adw::PreferencesPage::new();
    let options = adw::PreferencesGroup::new();
    let mode = adw::ComboRow::builder()
        .title(tr("Method"))
        .model(&gtk::StringList::new(&[&tr("Find and Replace"), &tr("Template with Numbers")]))
        .build();
    let find = adw::EntryRow::builder().title(tr("Find")).build();
    let replace = adw::EntryRow::builder().title(tr("Replace With")).build();
    let pattern = adw::EntryRow::builder()
        .title(tr("Template ({n} is the number, {name} the old name)"))
        .text(format!("{} {{n}}", tr("File")))
        .visible(false)
        .build();
    let start = adw::SpinRow::builder()
        .title(tr("First number"))
        .adjustment(&gtk::Adjustment::new(1.0, 0.0, 1_000_000.0, 1.0, 10.0, 0.0))
        .visible(false)
        .build();
    for row in [
        mode.upcast_ref::<gtk::Widget>(),
        find.upcast_ref(),
        replace.upcast_ref(),
        pattern.upcast_ref(),
        start.upcast_ref(),
    ] {
        options.add(row);
    }
    page.add(&options);
    let preview = adw::PreferencesGroup::builder().title(tr("Preview")).build();
    page.add(&preview);
    view.set_content(Some(&page));
    dialog.set_child(Some(&view));

    let entries = Rc::new(entries);
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    for entry in entries.iter().take(200) {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(&entry.name)).build();
        preview.add(&row);
        rows.borrow_mut().push(row);
    }
    if entries.len() > 200 {
        preview.set_description(Some(&trf("Showing the first {n} files", &[("n", "200")])));
    }
    let names: Rc<RefCell<Vec<String>>> = Rc::default();
    let update: Rc<dyn Fn()> = {
        let (entries, rows, names, mode, find, replace, pattern, start, apply, existing) = (
            entries.clone(),
            rows.clone(),
            names.clone(),
            mode.clone(),
            find.clone(),
            replace.clone(),
            pattern.clone(),
            start.clone(),
            apply.clone(),
            existing.clone(),
        );
        Rc::new(move || {
            let template = mode.selected() == 1;
            find.set_visible(!template);
            replace.set_visible(!template);
            pattern.set_visible(template);
            start.set_visible(template);
            let planned =
                plan(&entries, template, &find.text(), &replace.text(), &pattern.text(), start.value() as u32);
            let old: HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            let mut seen = HashSet::new();
            let mut problems = 0;
            let mut changes = 0;
            for (i, (entry, new)) in entries.iter().zip(&planned).enumerate() {
                let clash = !seen.insert(new.clone())
                    || (new != &entry.name && !old.contains(new.as_str()) && existing.contains(new))
                    || new.is_empty()
                    || new.contains('/');
                if clash {
                    problems += 1;
                }
                if new != &entry.name {
                    changes += 1;
                }
                if let Some(row) = rows.borrow().get(i) {
                    row.set_subtitle(&if new == &entry.name {
                        String::new()
                    } else {
                        format!("→ {}", glib::markup_escape_text(new))
                    });
                    if clash {
                        row.add_css_class("error");
                    } else {
                        row.remove_css_class("error");
                    }
                }
            }
            apply.set_sensitive(problems == 0 && changes > 0);
            names.replace(planned);
        })
    };
    update();
    for row in [&find, &replace, &pattern] {
        let u = update.clone();
        row.connect_changed(move |_| u());
    }
    let u = update.clone();
    mode.connect_selected_notify(move |_| u());
    let u = update.clone();
    start.connect_value_notify(move |_| u());
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    let parent = win.clone();
    let win = win.clone();
    apply.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
            let queue = win.queue();
            let batch = queue.batch(glib::clone!(
                #[weak]
                win,
                move |outcome| {
                    win.toast(&if outcome.failed > 0 {
                        trn(
                            "{n} file could not be renamed",
                            "{n} files could not be renamed",
                            &[("n", &outcome.failed.to_string())],
                        )
                    } else {
                        trn("{n} file renamed", "{n} files renamed", &[("n", &outcome.done.to_string())])
                    });
                    win.refresh();
                }
            ));
            // Go through a temp name when the new name is another entry's old name.
            let olds: HashSet<String> = entries.iter().map(|e| e.name.clone()).collect();
            for (entry, new) in entries.iter().zip(names.borrow().iter()) {
                if new == &entry.name {
                    continue;
                }
                let from = entry.key.clone();
                let to = format!("{prefix}{new}");
                let via = olds.contains(new).then(|| format!("{prefix}.ferry-rename-{}", glib::uuid_string_random()));
                let (client, bucket) = (client.clone(), bucket.clone());
                queue.add(
                    Some(batch),
                    "move",
                    &entry.name,
                    &format!("→ {new}"),
                    entry.size.max(0) as u64,
                    None,
                    queue::work(move |_| {
                        let (client, bucket, from, to, via) =
                            (client.clone(), bucket.clone(), from.clone(), to.clone(), via.clone());
                        async move {
                            match via {
                                Some(temp) => {
                                    client.rename(&bucket, &from, &temp).await?;
                                    client.rename(&bucket, &temp, &to).await
                                }
                                None => client.rename(&bucket, &from, &to).await,
                            }
                        }
                    }),
                );
            }
            queue.seal(batch);
        }
    ));
    dialog.present(Some(&parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| Entry { key: n.to_string(), name: n.to_string(), ..Default::default() }).collect()
    }

    #[test]
    fn plans() {
        let list = entries(&["IMG_1.jpg", "IMG_2.jpg", "notes"]);
        assert_eq!(plan(&list, false, "IMG_", "Photo ", "", 1), vec!["Photo 1.jpg", "Photo 2.jpg", "notes"]);
        assert_eq!(plan(&list, true, "", "", "Holiday {n}", 1), vec!["Holiday 01.jpg", "Holiday 02.jpg", "Holiday 03"]);
        assert_eq!(plan(&list, true, "", "", "{name}-x", 9), vec!["IMG_1-x 09.jpg", "IMG_2-x 10.jpg", "notes-x 11"]);
    }
}
