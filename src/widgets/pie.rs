use adw::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone)]
pub struct Pie {
    pub widget: gtk::DrawingArea,
    shown: Rc<Cell<f64>>,
    animation: adw::SpringAnimation,
}

impl Pie {
    pub fn new(size: i32) -> Self {
        let widget =
            gtk::DrawingArea::builder().content_width(size).content_height(size).valign(gtk::Align::Center).build();
        widget.add_css_class("transfer-pie");
        let shown = Rc::new(Cell::new(0.0_f64));
        let draw_value = shown.clone();
        widget.set_draw_func(move |area, cr, width, height| {
            let color = area.color();
            let (cx, cy) = (width as f64 / 2.0, height as f64 / 2.0);
            let radius = cx.min(cy) - 1.0;
            cr.set_source_rgba(color.red() as f64, color.green() as f64, color.blue() as f64, 0.25);
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            let fraction = draw_value.get().clamp(0.0, 1.0);
            if fraction > 0.0 {
                cr.set_source_rgba(color.red() as f64, color.green() as f64, color.blue() as f64, color.alpha() as f64);
                cr.move_to(cx, cy);
                let start = -std::f64::consts::FRAC_PI_2;
                cr.arc(cx, cy, radius, start, start + fraction * std::f64::consts::TAU);
                cr.close_path();
                let _ = cr.fill();
            }
        });
        let target_value = shown.clone();
        let area = widget.downgrade();
        let target = adw::CallbackAnimationTarget::new(move |value| {
            target_value.set(value);
            if let Some(area) = area.upgrade() {
                area.queue_draw();
            }
        });
        let animation = adw::SpringAnimation::new(&widget, 0.0, 0.0, adw::SpringParams::new(1.0, 1.0, 120.0), target);
        animation.set_clamp(true);
        Pie { widget, shown, animation }
    }

    pub fn set_fraction(&self, fraction: f64) {
        let fraction = fraction.clamp(0.0, 1.0);
        if (fraction - self.animation.value_to()).abs() < 0.001 {
            return;
        }
        if fraction < self.shown.get() - 0.2 {
            self.shown.set(fraction);
            self.widget.queue_draw();
        }
        self.animation.set_value_from(self.shown.get());
        self.animation.set_value_to(fraction);
        self.animation.play();
    }
}

pub fn fade(widget: &impl IsA<gtk::Widget>, show: bool) {
    let widget = widget.as_ref().clone();
    let from = widget.opacity();
    let to = if show { 1.0 } else { 0.0 };
    let w = widget.downgrade();
    let target = adw::CallbackAnimationTarget::new(move |value| {
        if let Some(w) = w.upgrade() {
            w.set_opacity(value);
        }
    });
    let animation = adw::TimedAnimation::new(&widget, from, to, 180, target);
    animation.set_easing(adw::Easing::EaseOutCubic);
    animation.play();
}
