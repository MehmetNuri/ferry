pub mod code_view;
pub mod pie;

// decompression bomb guard
const MAX_PIXELS: i64 = 50_000_000;

pub fn image_fits(bytes: &[u8]) -> bool {
    use gtk::gdk_pixbuf::prelude::*;
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    let size = std::rc::Rc::new(std::cell::Cell::new(None::<(i32, i32)>));
    let seen = size.clone();
    loader.connect_size_prepared(move |loader, width, height| {
        seen.set(Some((width, height)));
        loader.set_size(1, 1);
    });
    let _ = loader.write(&bytes[..bytes.len().min(256 * 1024)]);
    let _ = loader.close();
    size.get().is_some_and(|(w, h)| w > 0 && h > 0 && (w as i64) * (h as i64) <= MAX_PIXELS)
}

#[cfg(test)]
mod tests {
    #[test]
    fn image_bombs_are_refused() {
        let small: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0, 144,
            119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0, 3, 1, 1, 0, 201, 254, 146,
            239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ];
        let bomb: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 156, 64, 0, 0, 156, 64, 8, 2, 0, 0, 0,
            222, 110, 153, 82, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ];
        assert!(super::image_fits(small));
        assert!(!super::image_fits(bomb));
    }
}
