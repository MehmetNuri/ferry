use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;

use crate::runtime::bg;
use crate::s3::{Progress, S3};

const URI_LIST: &str = "text/uri-list";
const GNOME_FILES: &str = "x-special/gnome-copied-files";
const TEXT: &str = "text/plain;charset=utf-8";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct DragOut {
        pub client: RefCell<Option<S3>>,
        pub bucket: RefCell<String>,
        pub prefix: RefCell<String>,
        pub keys: RefCell<Vec<String>>,
        pub text: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DragOut {
        const NAME: &'static str = "FerryDragOut";
        type Type = super::DragOut;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for DragOut {}

    impl ContentProviderImpl for DragOut {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormatsBuilder::new()
                .add_mime_type(URI_LIST)
                .add_mime_type(GNOME_FILES)
                .add_mime_type(TEXT)
                .add_mime_type("text/plain")
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let stream = stream.clone();
            if mime_type != URI_LIST && mime_type != GNOME_FILES {
                let text = self.text.borrow().clone();
                return Box::pin(async move {
                    stream.write_all_future(text.into_bytes(), priority).await.map_err(|(_, e)| e)?;
                    Ok(())
                });
            }
            let gnome = mime_type == GNOME_FILES;
            let (client, bucket, prefix, keys) = (
                self.client.borrow().clone(),
                self.bucket.borrow().clone(),
                self.prefix.borrow().clone(),
                self.keys.borrow().clone(),
            );
            Box::pin(async move {
                let client = client.ok_or_else(|| glib::Error::new(gio::IOErrorEnum::Failed, "not connected"))?;
                let files = bg(super::download(client, bucket, prefix, keys))
                    .await
                    .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e))?;
                let uris: Vec<String> = files.iter().map(|p| gio::File::for_path(p).uri().to_string()).collect();
                let list = if gnome {
                    format!("copy\n{}", uris.join("\n"))
                } else {
                    uris.iter().map(|u| format!("{u}\r\n")).collect()
                };
                stream.write_all_future(list.into_bytes(), priority).await.map_err(|(_, e)| e)?;
                Ok(())
            })
        }
    }
}

glib::wrapper! {
    pub struct DragOut(ObjectSubclass<imp::DragOut>) @extends gdk::ContentProvider;
}

impl DragOut {
    pub fn new(client: S3, bucket: String, prefix: String, keys: Vec<String>, text: String) -> Self {
        let provider: Self = glib::Object::new();
        let imp = provider.imp();
        imp.client.replace(Some(client));
        imp.bucket.replace(bucket);
        imp.prefix.replace(prefix);
        imp.keys.replace(keys);
        imp.text.replace(text);
        provider
    }
}

pub async fn download(
    client: S3,
    bucket: String,
    prefix: String,
    keys: Vec<String>,
) -> Result<Vec<std::path::PathBuf>, String> {
    let root = glib::user_cache_dir().join("ferry").join("drag").join(glib::uuid_string_random().as_str());
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut top = Vec::new();
    for key in keys {
        let target = crate::s3::download_target(&root, &prefix, key.trim_end_matches('/'))?;
        if key.ends_with('/') {
            let (items, _) = client.list_all(&bucket, &key, usize::MAX).await?;
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            for item in items.into_iter().filter(|i| !i.key.ends_with('/')) {
                let path = crate::s3::download_target(&root, &prefix, &item.key)?;
                client.download_file(&bucket, &item.key, None, &path, &Progress::default()).await?;
            }
        } else {
            client.download_file(&bucket, &key, None, &target, &Progress::default()).await?;
        }
        top.push(target);
    }
    Ok(top)
}

pub fn cleanup() {
    let _ = std::fs::remove_dir_all(glib::user_cache_dir().join("ferry").join("drag"));
}
