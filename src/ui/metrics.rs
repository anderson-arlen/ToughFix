// SPDX-License-Identifier: MIT
use crate::model::{State, bytes};
use gtk::{cairo, prelude::*};
use std::{cell::Cell, rc::Rc};

/// Camera properties are snapshots; filesystem space is refreshed passively.
pub struct Resources {
    pub widget: gtk::Box,
    battery: gtk::DrawingArea,
    level: Rc<Cell<Option<u8>>>,
    usb: Rc<Cell<bool>>,
    battery_note: gtk::Label,
    card: gtk::DrawingArea,
    capacity: Rc<Cell<u64>>,
    usage: gtk::ProgressBar,
    space: gtk::Label,
}

fn color(cr: &cairo::Context, rgb: (f64, f64, f64)) {
    cr.set_source_rgb(rgb.0, rgb.1, rgb.2);
}
fn text(cr: &cairo::Context, value: &str, x: f64, y: f64, size: f64) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(size);
    let width = cr.text_extents(value).map(|e| e.width()).unwrap_or(0.);
    cr.move_to(x - width / 2., y);
    let _ = cr.show_text(value);
}
fn battery_color(level: Option<u8>) -> (f64, f64, f64) {
    match level {
        Some(0..=20) => (0.98, 0.43, 0.46),
        Some(21..=50) => (0.98, 0.71, 0.28),
        Some(_) => (0.33, 0.80, 0.65),
        None => (0.40, 0.49, 0.52),
    }
}
fn used_fraction(capacity: u64, free: u64) -> Option<f64> {
    (capacity > 0).then(|| capacity.saturating_sub(free) as f64 / capacity as f64)
}

impl Resources {
    pub fn new() -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 22);
        let battery_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let title = super::label("key");
        title.set_text("BATTERY");
        battery_box.append(&title);
        let level = Rc::new(Cell::new(None::<u8>));
        let usb = Rc::new(Cell::new(false));
        let battery = gtk::DrawingArea::builder()
            .content_width(118)
            .content_height(48)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        {
            let level = level.clone();
            let usb = usb.clone();
            battery.set_draw_func(move |_, cr, _, _| {
                color(cr, (0.55, 0.66, 0.67));
                cr.set_line_width(2.);
                cr.rectangle(1., 5., 106., 36.);
                let _ = cr.stroke();
                cr.rectangle(109., 16., 6., 14.);
                let _ = cr.fill();
                color(cr, (0.09, 0.16, 0.18));
                cr.rectangle(5., 9., 98., 28.);
                let _ = cr.fill();
                color(cr, battery_color(level.get()));
                cr.rectangle(5., 9., 98. * level.get().unwrap_or(0) as f64 / 100., 28.);
                let _ = cr.fill();
                // Dark backing keeps the percentage legible at every fill level.
                color(cr, (0.09, 0.16, 0.18));
                cr.rectangle(22., 13., 48., 20.);
                let _ = cr.fill();
                color(cr, (0.90, 0.95, 0.93));
                text(
                    cr,
                    &level
                        .get()
                        .map(|l| format!("{l}%"))
                        .unwrap_or_else(|| "—".into()),
                    46.,
                    28.,
                    13.,
                );
                if usb.get() {
                    cr.move_to(88., 11.);
                    cr.line_to(77., 24.);
                    cr.line_to(84., 24.);
                    cr.line_to(80., 35.);
                    cr.line_to(94., 20.);
                    cr.line_to(86., 20.);
                    cr.close_path();
                    let _ = cr.fill();
                }
            });
        }
        battery_box.append(&battery);
        let battery_note = super::label("subtitle");
        battery_note.set_selectable(false);
        battery_box.append(&battery_note);
        widget.append(&battery_box);

        let storage = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        storage.set_hexpand(true);
        let capacity = Rc::new(Cell::new(0));
        let card = gtk::DrawingArea::builder()
            .content_width(62)
            .content_height(80)
            .accessible_role(gtk::AccessibleRole::Img)
            .build();
        {
            let capacity = capacity.clone();
            card.set_draw_func(move |_, cr, _, _| {
                color(cr, (0.15, 0.23, 0.27));
                cr.move_to(3., 3.);
                cr.line_to(42., 3.);
                cr.line_to(58., 19.);
                cr.line_to(58., 76.);
                cr.line_to(3., 76.);
                cr.close_path();
                let _ = cr.fill_preserve();
                color(cr, (0.40, 0.56, 0.59));
                cr.set_line_width(1.5);
                let _ = cr.stroke();
                color(cr, (0.83, 0.69, 0.34));
                for x in [11., 20., 29., 38.] {
                    cr.rectangle(x, 10., 6., 11.);
                }
                let _ = cr.fill();
                color(cr, (0.90, 0.95, 0.93));
                let value = if capacity.get() > 0 {
                    bytes(capacity.get()).replace(" GiB", "")
                } else {
                    "—".into()
                };
                text(cr, &value, 30., 45., 13.);
                text(cr, "GiB", 30., 61., 10.);
            });
        }
        storage.append(&card);
        let storage_text = gtk::Box::new(gtk::Orientation::Vertical, 7);
        storage_text.set_valign(gtk::Align::Center);
        storage_text.set_hexpand(true);
        let title = super::label("key");
        title.set_text("SD CARD");
        storage_text.append(&title);
        let usage = gtk::ProgressBar::new();
        usage.add_css_class("storage-usage");
        storage_text.append(&usage);
        let space = super::label("subtitle");
        space.set_max_width_chars(24);
        storage_text.append(&space);
        storage.append(&storage_text);
        widget.append(&storage);
        Self {
            widget,
            battery,
            level,
            usb,
            battery_note,
            card,
            capacity,
            usage,
            space,
        }
    }

    pub fn render(&self, s: &State) {
        let level = s
            .camera
            .as_ref()
            .and_then(|c| c.battery)
            .filter(|v| *v <= 100);
        let connected = s.device.is_some();
        self.level.set(if connected { level } else { None });
        self.usb.set(connected);
        let note = if connected && level.is_none() {
            if s.camera.is_some() {
                "Not reported".into()
            } else {
                "Not read yet".into()
            }
        } else if connected {
            s.camera
                .as_ref()
                .map(|c| c.snapshot_age_at(chrono::Utc::now()))
                .unwrap_or_else(|| "Unknown age".into())
        } else {
            "Not reported".into()
        };
        self.battery_note.set_text(&note);
        let battery_text = format!(
            "Battery: {}. Use the camera refresh icon to take a new reading; this briefly unmounts and remounts the card. The bolt indicates USB connection, not active charging.",
            self.level
                .get()
                .map(|l| format!("{l}%"))
                .unwrap_or_else(|| "not reported".into())
        );
        self.battery.set_tooltip_text(Some(&battery_text));
        self.battery.queue_draw();

        let storage = s
            .device
            .as_ref()
            .and_then(|d| d.mounted_storage.first())
            .or_else(|| s.camera.as_ref().and_then(|c| c.storage.first()));
        let capacity = if connected {
            storage
                .map(|d| d.capacity)
                .unwrap_or_else(|| s.device.as_ref().map(|d| d.capacity).unwrap_or(0))
        } else {
            0
        };
        self.capacity.set(capacity);
        let fraction = connected
            .then_some(storage)
            .flatten()
            .and_then(|d| used_fraction(d.capacity, d.free));
        self.usage.set_fraction(fraction.unwrap_or(0.));
        self.usage
            .set_opacity(if fraction.is_some() { 1. } else { 0.35 });
        let space = if let Some(storage) = storage.filter(|_| connected && fraction.is_some()) {
            format!(
                "{} free\n{} used",
                bytes(storage.free),
                bytes(storage.capacity.saturating_sub(storage.free))
            )
        } else if connected {
            "Usage not available yet".into()
        } else {
            "No card information".into()
        };
        self.space.set_text(&space);
        let card_text = format!(
            "SD card: {} capacity; {}",
            if capacity > 0 {
                bytes(capacity)
            } else {
                "unknown".into()
            },
            space.replace('\n', ", ")
        );
        self.card.set_tooltip_text(Some(&card_text));
        self.usage.set_tooltip_text(Some(&card_text));
        self.card.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_fraction_handles_empty_full_unknown_and_inconsistent_readings() {
        assert_eq!(used_fraction(100, 100), Some(0.));
        assert_eq!(used_fraction(100, 0), Some(1.));
        assert_eq!(used_fraction(100, 25), Some(0.75));
        assert_eq!(used_fraction(0, 0), None);
        assert_eq!(used_fraction(100, 101), Some(0.));
    }
}
