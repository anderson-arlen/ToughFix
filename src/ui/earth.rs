// SPDX-License-Identifier: MIT
//! A small illustrative globe, independent of prediction or camera activity.
use gtk::{cairo, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    f64::consts::{PI, TAU},
    rc::Rc,
};

// Simplified coastlines in longitude/latitude degrees. They rotate on a sphere
// rather than spinning a flat image; the far hemisphere is hidden.
const LAND: &[&[(f64, f64)]] = &[
    &[
        (-168., 71.),
        (-141., 69.),
        (-135., 60.),
        (-127., 53.),
        (-124., 42.),
        (-116., 32.),
        (-106., 23.),
        (-97., 17.),
        (-84., 9.),
        (-77., 9.),
        (-88., 20.),
        (-80., 25.),
        (-80., 34.),
        (-66., 46.),
        (-57., 52.),
        (-62., 60.),
        (-83., 67.),
        (-104., 73.),
        (-139., 74.),
    ],
    &[
        (-81., 12.),
        (-64., 10.),
        (-52., 4.),
        (-35., -6.),
        (-40., -22.),
        (-50., -29.),
        (-56., -38.),
        (-68., -55.),
        (-75., -42.),
        (-72., -17.),
        (-81., -4.),
    ],
    &[
        (-10., 36.),
        (-9., 43.),
        (-2., 48.),
        (6., 54.),
        (10., 60.),
        (26., 71.),
        (55., 70.),
        (80., 74.),
        (120., 72.),
        (155., 59.),
        (179., 65.),
        (170., 54.),
        (142., 45.),
        (130., 33.),
        (119., 22.),
        (109., 18.),
        (105., 6.),
        (100., 10.),
        (94., 23.),
        (81., 8.),
        (73., 22.),
        (57., 25.),
        (43., 12.),
        (35., 31.),
        (25., 35.),
        (12., 38.),
    ],
    &[
        (-17., 15.),
        (-16., 29.),
        (-2., 36.),
        (12., 33.),
        (32., 31.),
        (40., 17.),
        (51., 11.),
        (44., 1.),
        (40., -16.),
        (32., -30.),
        (20., -35.),
        (12., -17.),
        (6., -3.),
        (-5., 5.),
        (-15., 5.),
    ],
    &[
        (112., -11.),
        (130., -11.),
        (141., -15.),
        (153., -26.),
        (146., -40.),
        (131., -33.),
        (115., -34.),
    ],
    &[
        (-55., 60.),
        (-43., 60.),
        (-22., 73.),
        (-23., 81.),
        (-43., 84.),
        (-62., 76.),
    ],
    &[(44., -13.), (48., -15.), (49., -24.), (45., -26.)],
    &[(130., 31.), (136., 34.), (144., 45.), (140., 40.)],
];

#[derive(Default)]
struct Motion {
    seconds: Cell<f64>,
    previous: Cell<i64>,
    painted: Cell<i64>,
}

pub fn new() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(96)
        .content_height(106)
        .valign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    area.set_tooltip_text(Some("Earth and GPS satellite orbits"));
    let motion = Rc::new(Motion::default());
    let paint = motion.clone();
    area.set_draw_func(move |_, cr, width, height| {
        let _ = cr.save();
        let scale = (width as f64 / 112.).min(height as f64 / 112.);
        cr.translate(width as f64 / 2., height as f64 / 2.);
        cr.scale(scale, scale);
        let time = paint.seconds.get();
        orbits(cr, time, false);
        globe(cr, 95f64.to_radians() + time * TAU / 120.);
        orbits(cr, time, true);
        let _ = cr.restore();
    });
    let tick = Rc::new(RefCell::new(None));
    if let Some(settings) = gtk::Settings::default() {
        animate(&area, &motion, &tick, settings.is_gtk_enable_animations());
        let weak = area.downgrade();
        settings.connect_gtk_enable_animations_notify(move |settings| {
            if let Some(area) = weak.upgrade() {
                animate(&area, &motion, &tick, settings.is_gtk_enable_animations());
            }
        });
    }
    area
}

fn animate(
    area: &gtk::DrawingArea,
    motion: &Rc<Motion>,
    tick: &RefCell<Option<gtk::TickCallbackId>>,
    enabled: bool,
) {
    if let Some(id) = tick.borrow_mut().take() {
        id.remove();
    }
    motion.previous.set(0);
    if enabled {
        let motion = motion.clone();
        *tick.borrow_mut() = Some(area.add_tick_callback(move |area, clock| {
            if area.is_mapped() {
                let now = clock.frame_time();
                let previous = motion.previous.replace(now);
                if previous > 0 {
                    // Hidden windows don't advance or jump on reopening.
                    motion.seconds.set(
                        motion.seconds.get()
                            + ((now - previous) as f64 / 1_000_000.).clamp(0., 0.1),
                    );
                }
                if now - motion.painted.get() >= 33_333 {
                    motion.painted.set(now);
                    area.queue_draw();
                }
            }
            glib::ControlFlow::Continue
        }));
    }
    area.queue_draw();
}

fn orbit_point(angle: f64, tilt: f64) -> (f64, f64) {
    let x = 48. * angle.cos();
    let y = 17. * angle.sin();
    (
        x * tilt.cos() - y * tilt.sin(),
        x * tilt.sin() + y * tilt.cos(),
    )
}
fn orbits(cr: &cairo::Context, time: f64, front: bool) {
    for (plane, tilt) in [-0.55, 0.55, 1.55].into_iter().enumerate() {
        let start = if front { 0. } else { PI };
        for i in 0..=40 {
            let (x, y) = orbit_point(start + PI * i as f64 / 40., tilt);
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        cr.set_source_rgba(0.38, 0.73, 0.79, if front { 0.35 } else { 0.17 });
        cr.set_line_width(0.9);
        let _ = cr.stroke();
        for satellite in 0..2 {
            let angle = (time * TAU / (18. + plane as f64 * 5.)
                + plane as f64 * 1.2
                + satellite as f64 * PI)
                .rem_euclid(TAU);
            if (angle.sin() >= 0.) != front {
                continue;
            }
            let (x, y) = orbit_point(angle, tilt);
            cr.set_source_rgba(0.48, 0.95, 0.80, if front { 0.16 } else { 0.08 });
            cr.arc(x, y, 6., 0., TAU);
            let _ = cr.fill();
            cr.set_source_rgba(0.57, 0.96, 0.82, if front { 1. } else { 0.48 });
            cr.arc(x, y, 2.8, 0., TAU);
            let _ = cr.fill();
            cr.set_source_rgba(0.93, 1., 0.98, if front { 0.9 } else { 0.4 });
            cr.arc(x - 0.5, y - 0.5, 1., 0., TAU);
            let _ = cr.fill();
        }
    }
}

fn globe(cr: &cairo::Context, rotation: f64) {
    let _ = cr.save();
    cr.rotate(-0.16);
    cr.scale(28., 28.);
    let atmosphere = cairo::RadialGradient::new(0., 0., 0.9, 0., 0., 1.3);
    atmosphere.add_color_stop_rgba(0., 0.30, 0.78, 0.85, 0.22);
    atmosphere.add_color_stop_rgba(1., 0.30, 0.78, 0.85, 0.);
    let _ = cr.set_source(&atmosphere);
    cr.arc(0., 0., 1.3, 0., TAU);
    let _ = cr.fill();
    cr.arc(0., 0., 1., 0., TAU);
    cr.clip();
    let ocean = cairo::RadialGradient::new(-0.45, -0.45, 0., 0., 0., 1.5);
    ocean.add_color_stop_rgb(0., 0.21, 0.65, 0.83);
    ocean.add_color_stop_rgb(0.6, 0.10, 0.38, 0.57);
    ocean.add_color_stop_rgb(1., 0.04, 0.16, 0.26);
    let _ = cr.set_source(&ocean);
    let _ = cr.paint();
    cr.set_source_rgb(0.37, 0.77, 0.59);
    for coastline in LAND {
        land(cr, coastline, rotation);
    }
    let shade = cairo::LinearGradient::new(-0.7, -0.7, 0.9, 0.9);
    shade.add_color_stop_rgba(0., 0.6, 0.9, 1., 0.10);
    shade.add_color_stop_rgba(0.5, 0., 0.07, 0.14, 0.);
    shade.add_color_stop_rgba(1., 0., 0.06, 0.13, 0.65);
    let _ = cr.set_source(&shade);
    let _ = cr.paint();
    cr.reset_clip();
    cr.set_source_rgba(0.48, 0.86, 0.88, 0.48);
    cr.set_line_width(0.035);
    cr.arc(0., 0., 1., 0., TAU);
    let _ = cr.stroke();
    let _ = cr.restore();
}

fn land(cr: &cairo::Context, coastline: &[(f64, f64)], rotation: f64) {
    let sphere: Vec<[f64; 3]> = coastline
        .iter()
        .map(|(longitude, latitude)| {
            let lon = longitude.to_radians() + rotation;
            let lat = latitude.to_radians();
            [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()]
        })
        .collect();
    let mut visible = Vec::new();
    let mut previous = *sphere.last().unwrap();
    for point in sphere {
        if (point[2] >= 0.) != (previous[2] >= 0.) {
            let t = previous[2] / (previous[2] - point[2]);
            let x = previous[0] + t * (point[0] - previous[0]);
            let y = previous[1] + t * (point[1] - previous[1]);
            let length = x.hypot(y);
            visible.push([x / length, y / length, 0.]);
        }
        if point[2] >= 0. {
            visible.push(point);
        }
        previous = point;
    }
    let Some(first) = visible.first() else {
        return;
    };
    cr.move_to(first[0], first[1]);
    for i in 1..=visible.len() {
        let a = visible[i - 1];
        let b = visible[i % visible.len()];
        if a[2] == 0. && b[2] == 0. {
            let start = a[1].atan2(a[0]);
            let delta = (b[1].atan2(b[0]) - start + PI).rem_euclid(TAU) - PI;
            if delta >= 0. {
                cr.arc(0., 0., 1., start, start + delta);
            } else {
                cr.arc_negative(0., 0., 1., start, start + delta);
            }
        } else {
            cr.line_to(b[0], b[1]);
        }
    }
    cr.close_path();
    let _ = cr.fill();
}
