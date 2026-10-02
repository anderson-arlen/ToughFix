// SPDX-License-Identifier: MIT
use crate::{
    backend::Shared,
    model::{Action, State, age, bytes, gps_calendar},
    startup::{CameraStartup, should_exit},
    tray::Manager,
};
use adw::prelude::*;
use gtk::glib;
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, mpsc::Sender},
    time::{Duration, Instant},
};

mod earth;
mod floating;
mod metrics;

// Keep native navigation and controls themed by Libadwaita. Only the
// application's content cards, typography and storage gauge are customized.
const STYLE: &str = r#"
.toughfix-content { background: #101b20; color: #e4eeed; }
.toughfix-content .page { padding: 16px 24px; }
.toughfix-content .status-area { padding: 20px 24px 0; }
.toughfix-content .activity-title { font-size: 20px; font-weight: 700; }
.toughfix-content .camera-name { font-size: 21px; font-weight: 700; }
.toughfix-content .connected { color: #7edfc1; font-weight: 700; }
.toughfix-content .disconnected { color: #9aadb1; }
.toughfix-content .summary { font-size: 14px; }
.toughfix-content .subtitle { color: #9aadb1; font-size: 13px; }
.toughfix-content .card { background: #17272d; border: 1px solid #2c3d43; border-radius: 14px; padding: 16px; }
.toughfix-content .card-title { font-size: 16px; font-weight: 700; margin-bottom: 4px; }
.toughfix-content .key { color: #9bafb3; font-size: 12px; }
.toughfix-content .value { color: #e1edeb; font-size: 13px; }
.toughfix-content .mono { font-family: monospace; font-size: 11px; color: #acc4c4; }
.toughfix-content .warning { background: #483a21; color: #ffcf83; }
.toughfix-content .error { background: #42272d; color: #ffb3ba; }
.toughfix-content progressbar.storage-usage progress { background: #54cda7; }
"#;

struct Fields {
    connection: gtk::Label,
    camera: gtk::Label,
    storage: gtk::Label,
    observed: gtk::Label,
    fitted: gtk::Label,
    clocks: gtk::Label,
    source: gtk::Label,
    source_link: gtk::LinkButton,
    health: gtk::Label,
    training: gtk::Label,
    validity: gtk::Label,
    satellites: gtk::Label,
    quality: gtk::Label,
    hash: gtk::Label,
    commit: gtk::Label,
    match_status: gtk::Label,
    commit_hash: gtk::Label,
    banner: gtk::Label,
    progress: gtk::ProgressBar,
    message: gtk::Label,
    upload: gtk::Button,
    refresh: gtk::Button,
    update_gps: gtk::Button,
    camera_refresh: gtk::Button,
    automatic: gtk::Switch,
    upload_interval: adw::SpinRow,
    tray_error: gtk::Label,
    activity_title: gtk::Label,
    activity_detail: gtk::Label,
    spinner: gtk::Spinner,
    activity_card: gtk::Box,
    camera_image: gtk::Picture,
    camera_8010_image: gtk::Picture,
    camera_name: gtk::Label,
    gps_card: gtk::Box,
    gps_details: Vec<gtk::Box>,
    health_badge: gtk::Label,
    health_caption: gtk::Label,
    resources: Vec<metrics::Resources>,
    assistance_summary: gtk::Label,
    last_update_summary: gtk::Label,
}
fn label(class: &str) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(0.);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_selectable(true);
    l.add_css_class(class);
    l
}
fn card(parent: &gtk::Box, title: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 10);
    b.add_css_class("card");
    let t = label("card-title");
    t.set_text(title);
    b.append(&t);
    parent.append(&b);
    b
}
fn field(parent: &gtk::Box, title: &str) -> gtk::Label {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 3);
    let key = label("key");
    key.set_text(title);
    key.set_valign(gtk::Align::Start);
    key.set_selectable(false);
    let value = label("value");
    value.set_hexpand(true);
    value.set_max_width_chars(65);
    row.append(&key);
    row.append(&value);
    parent.append(&row);
    value
}
fn date(text: &str) -> String {
    if text.is_empty() {
        "Unavailable".into()
    } else if text.ends_with("GPS") {
        text.into()
    } else {
        format!("{} GPS", text.replace('T', " "))
    }
}
fn render(f: &Fields, s: &State, monitor_only: bool) {
    let activity = s.activity();
    f.activity_title.set_text(&activity.title);
    f.activity_detail.set_text(&activity.detail);
    f.activity_detail
        .set_lines(if s.data.refresh_error.is_some() && activity.warning {
            3
        } else {
            2
        });
    f.activity_detail.set_tooltip_text(Some(&activity.detail));
    f.spinner.set_spinning(activity.busy);
    f.spinner.set_visible(activity.busy);
    f.progress.set_visible(activity.busy);
    if let Some(fraction) = activity.fraction {
        f.progress.set_fraction(fraction.clamp(0., 1.));
    } else if activity.busy {
        f.progress.pulse();
    }
    for class in ["warning", "error"] {
        f.activity_card.remove_css_class(class);
    }
    if activity.warning {
        f.activity_card.add_css_class("error");
    }
    f.camera_image
        .set_opacity(if s.device.is_some() { 1. } else { 0.35 });
    let (health, caption, healthy) = if s.device.is_none() {
        ("Not checked", "No camera health snapshot".to_owned(), false)
    } else if s.camera_error.is_some() {
        (
            "Needs attention",
            "See Advanced for details".to_owned(),
            false,
        )
    } else if let Some(camera) = s.camera.as_ref() {
        (
            if s.camera_cached {
                "Last check healthy"
            } else {
                "Healthy"
            },
            camera
                .read_at
                .map(|_| format!("Snapshot · {}", camera.snapshot_age_at(chrono::Utc::now())))
                .unwrap_or_else(|| "Last camera-session reading".into()),
            true,
        )
    } else {
        (
            "Not checked",
            "Refresh to read camera information".to_owned(),
            false,
        )
    };
    f.health_badge.set_text(health);
    f.health_badge.remove_css_class("connected");
    f.health_badge.remove_css_class("disconnected");
    f.health_badge
        .add_css_class(if healthy { "connected" } else { "disconnected" });
    f.health_caption.set_text(&caption);
    f.health_caption.set_tooltip_text(
        s.camera
            .as_ref()
            .and_then(|c| c.read_at)
            .map(|t| {
                format!(
                    "Battery and camera health read {}",
                    t.with_timezone(&chrono::Local).format("%b %-d at %H:%M:%S")
                )
            })
            .as_deref(),
    );
    f.health_badge.set_tooltip_text(Some("Camera communication health from the last camera session. Use the refresh icon to take a new snapshot; there is no periodic camera polling."));
    for resource in &f.resources {
        resource.render(s);
    }
    f.assistance_summary.set_text(&s.assistance_summary());
    f.last_update_summary.set_text(
        &s.receipt
            .as_ref()
            .map(|r| {
                format!(
                    "{} · {}{}",
                    if s.receipt_matches_camera() {
                        "Last confirmed camera update"
                    } else {
                        "Last recorded upload"
                    },
                    r.committed_at
                        .with_timezone(&chrono::Local)
                        .format("%b %-d, %Y at %H:%M"),
                    if s.device.is_some() && !s.receipt_matches_camera() {
                        " · camera identity unverified"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_else(|| {
                if s.device.is_some() {
                    "No saved upload linked to this camera"
                } else {
                    "No camera upload recorded"
                }
                .into()
            }),
    );
    f.connection.set_text(
        &s.device
            .as_ref()
            .map(|d| format!("{} · {}", d.model.name(), d.path.display()))
            .unwrap_or_else(|| "No camera connected".into()),
    );
    f.camera.set_text(&s.camera_error.clone().unwrap_or_else(||s.camera.as_ref().map(|c|format!("Camera responded normally · firmware {}\nBattery: {} · {}\nLast checked {} · snapshot",c.firmware,c.battery.map(|b|format!("{b}%")).unwrap_or_else(||"not reported".into()),if s.gps_supported(){format!("GPS interface {}",c.gps_chip)}else{"No GPS receiver".into()},c.read_at.map(|t|t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default())).unwrap_or_else(||if s.storage_mounted(){"USB storage connected · camera checks paused while storage is mounted"}else if s.device.is_some(){"Camera information will be checked on connection"}else{"Connect the camera in Storage mode"}.into())));
    f.camera_name.set_text(
        s.device
            .as_ref()
            .map_or("Olympus Tough", |d| d.model.name()),
    );
    f.camera_image.set_visible(s.gps_supported());
    f.camera_8010_image.set_visible(!s.gps_supported());
    f.gps_card.set_visible(s.gps_supported());
    for card in &f.gps_details {
        card.set_visible(s.gps_supported());
    }
    f.upload.set_visible(s.gps_supported());
    let storage = s
        .device
        .as_ref()
        .filter(|d| !d.mounted_storage.is_empty())
        .map(|d| d.mounted_storage.as_slice())
        .or_else(|| s.camera.as_ref().map(|c| c.storage.as_slice()))
        .map(|storage| {
            storage
                .iter()
                .map(|d| {
                    format!(
                        "{} · {} capacity · {} free · {}",
                        d.label,
                        bytes(d.capacity),
                        bytes(d.free),
                        if d.writable {
                            "read/write"
                        } else {
                            "read only"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            s.device
                .as_ref()
                .map(|d| {
                    format!(
                        "{} capacity · free space not yet reported",
                        bytes(d.capacity)
                    )
                })
                .unwrap_or_else(|| "Unavailable while disconnected".into())
        });
    let mounted = s
        .device
        .as_ref()
        .filter(|d| !d.mounts.is_empty())
        .map(|d| format!("\nMounted at {}", d.mounts.join(", ")))
        .unwrap_or_default();
    let preparing = if s.storage_preparing {
        "\nWaiting for the initial GPS update; storage will mount automatically."
    } else {
        ""
    };
    f.storage
        .set_text(&format!("{storage}{mounted}{preparing}"));
    f.observed.set_text(&format!(
        "{}{}",
        age(s.data.observed_gps),
        s.data
            .observed_gps
            .map(|t| format!(" · {}", gps_calendar(t)))
            .unwrap_or_default()
    ));
    f.fitted.set_text(&format!(
        "{}{}",
        age(s.data.fitted_observed_gps),
        s.data
            .fitted_observed_gps
            .map(|t| format!(" · {}", gps_calendar(t)))
            .unwrap_or_default()
    ));
    f.clocks.set_text(&s.data.clock_summary);
    f.source.set_text(&s.data.source);
    f.source_link.set_uri(if s.data.source_url.is_empty() {
        "https://noaa-cors-pds.s3.amazonaws.com/"
    } else {
        &s.data.source_url
    });
    f.health.set_text(&format!(
        "{}{}",
        if s.updating_sources {
            "Refreshing satellite data and predictions…".into()
        } else {
            format!("Last checked {}", age(s.data.checked_gps))
        },
        if s.data.excluded.is_empty() {
            String::new()
        } else {
            format!("\nExcluded from predictions: {}", s.data.excluded)
        }
    ));
    f.training.set_text(&format!(
        "{} → {}",
        date(&s.data.training_start),
        date(&s.data.training_end)
    ));
    f.validity.set_text(&format!(
        "{} → {}",
        date(&s.data.start_gps),
        date(&s.data.end_gps)
    ));
    f.satellites.set_text(&format!(
        "{} · {} bytes",
        s.data
            .available
            .iter()
            .take(2)
            .enumerate()
            .map(|(i, n)| format!("Week {}: {n} GPS satellites", i + 1))
            .collect::<Vec<_>>()
            .join(" · "),
        s.data.bytes
    ));
    f.quality.set_text(&format!("Observed-arc fit: {} · CEP encoding: {}\nOrbit propagation: {}\nThese are satellite-model statistics, not camera position accuracy.",s.data.fit_rms.map(|v|format!("{v:.2} m RMS")).unwrap_or_else(||"unavailable".into()),s.data.decoded_rms.map(|v|format!("{v:.2} m RMS")).unwrap_or_else(||"unavailable".into()),s.data.propagation_seconds.map(|v|format!("{v:.1} seconds")).unwrap_or_else(||"unavailable".into())));
    f.hash.set_text(&s.data.sha256);
    if let Some(r) = s.receipt.as_ref() {
        f.commit.set_text(&format!(
            "{} · {}\n{} → {}\n{}",
            r.committed_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %Z"),
            r.origin,
            date(&r.start_gps),
            date(&r.end_gps),
            if r.session_closed {
                "Commit acknowledged · session closed"
            } else {
                "Commit acknowledged · session close not confirmed"
            }
        ));
        f.commit_hash.set_text(&r.sha256);
        f.match_status.set_text(if s.matches_latest() {
            "This camera’s last acknowledged commit matches the latest local predictions."
        } else if s.device.is_none() {
            "Saved history · camera is disconnected."
        } else if s
            .camera
            .as_ref()
            .is_some_and(|c| r.camera_key.as_ref() != Some(&c.key))
        {
            "Saved history belongs to another camera or has no verified identity."
        } else if s.receipt_matches_camera() {
            "Newer local predictions are available for this camera."
        } else {
            "Saved upload history has not been linked to this camera's identity."
        });
    } else {
        f.commit.set_text("No acknowledged commit recorded");
        f.commit_hash.set_text("");
        f.match_status
            .set_text("A commit is recorded only after the camera acknowledges saving the file.");
    }
    f.banner
        .set_text(&s.storage_error.clone().unwrap_or_else(|| {
            format!(
                "{}{}",
                s.unplug_message(),
                if s.quit_pending {
                    " · will quit when finished"
                } else {
                    ""
                }
            )
        }));
    f.banner.set_visible(
        s.phase.device_busy()
            || s.storage_preparing
            || s.reconnect_required
            || s.storage_error
                .as_deref()
                .is_some_and(|error| error != activity.detail),
    );
    f.banner.set_lines(2);
    f.banner.set_ellipsize(gtk::pango::EllipsizeMode::End);
    f.activity_detail.set_tooltip_text(Some(&activity.detail));
    for class in ["warning", "error"] {
        f.banner.remove_css_class(class);
    }
    if s.phase.device_busy() || s.storage_preparing {
        f.banner.add_css_class("warning");
    } else if s.reconnect_required || s.camera_error.is_some() || s.storage_error.is_some() {
        f.banner.add_css_class("error");
    }
    let issues = if s.data.failures.is_empty() {
        String::new()
    } else {
        format!("\n{}", s.data.failures.join("\n"))
    };
    f.message.set_text(&format!(
        "{}{}{}{}",
        s.message,
        issues,
        if s.storage_mounted() { "\nCamera checks and GPS uploads are paused. Unmount camera storage while leaving USB connected to update GPS assistance." } else { "" },
        if monitor_only {
            "\nMonitor mode · uploads and network refresh are disabled"
        } else {
            ""
        }
    ));
    f.upload.set_sensitive(
        s.gps_supported()
            && s.device.is_some()
            && !s.storage_mounted()
            && s.camera_error.is_none()
            && s.data.upload_allowed
            && !s.phase.device_busy()
            && !s.updating_sources
            && !s.reconnect_required
            && !s.quit_pending
            && !monitor_only,
    );
    f.upload.set_label(if s.demo {
        "Simulate upload"
    } else if s.storage_mounted() {
        "Unmount storage to upload"
    } else if s.matches_latest() {
        "Upload again"
    } else {
        "Upload latest predictions"
    });
    f.refresh.set_sensitive(
        s.gps_supported()
            && !s.phase.device_busy()
            && !s.updating_sources
            && !s.quit_pending
            && !s.demo
            && !monitor_only,
    );
    f.update_gps
        .set_sensitive(s.can_update_gps() && !monitor_only);
    f.camera_refresh.set_sensitive(s.can_refresh_camera());
    f.automatic
        .set_sensitive(s.gps_supported() && !monitor_only && !s.quit_pending);
    if f.automatic.is_active() != s.automatic {
        f.automatic.set_active(s.automatic);
    }
    f.upload_interval
        .set_sensitive(s.gps_supported() && !monitor_only && !s.quit_pending);
    if f.upload_interval.value() as u32 != s.upload_interval_hours() {
        f.upload_interval
            .set_value(s.upload_interval_hours() as f64);
    }
}

fn quit(shared: &Shared, tx: &Sender<Action>) {
    shared.lock().unwrap().quit_pending = true;
    let _ = tx.send(Action::Quit);
}

pub fn run(
    shared: Shared,
    tx: Sender<Action>,
    background: bool,
    demo: bool,
    monitor_only: bool,
    hotplug: bool,
    config_dir: Option<PathBuf>,
) {
    let app = adw::Application::builder()
        .application_id(if demo {
            "org.toughfix.Desktop.Demo"
        } else {
            "org.toughfix.Desktop"
        })
        .build();
    let existing: Rc<RefCell<Option<adw::ApplicationWindow>>> = Rc::new(RefCell::new(None));
    let exiting = Rc::new(RefCell::new(false));
    let existing_for_activate = existing.clone();
    app.connect_activate(move|app|{
        if let Some(w)=existing_for_activate.borrow().as_ref(){w.present();return}
        let _hold=app.hold();
        let hold=Rc::new(RefCell::new(Some(_hold)));
        let provider=gtk::CssProvider::new();provider.load_from_data(STYLE);
        gtk::style_context_add_provider_for_display(&gtk::gdk::Display::default().expect("GTK display"),&provider,gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        let window=adw::ApplicationWindow::builder().application(app).title("ToughFix").default_width(560).default_height(660).width_request(400).height_request(360).hide_on_close(true).build();
        floating::compact_default(window.upcast_ref::<gtk::ApplicationWindow>(),560,660);
        let toolbar = adw::ToolbarView::new();
        let header=adw::HeaderBar::new();
        header.set_centering_policy(adw::CenteringPolicy::Loose);
        toolbar.add_top_bar(&header);
        {
            let shared = shared.clone(); let tx = tx.clone();
            window.connect_close_request(move |_| {
                if shared.lock().unwrap().device.is_none() {
                    quit(&shared,&tx);
                }
                glib::Propagation::Proceed
            });
        }
        for (name,action) in [("quit",Action::Quit),("refresh",Action::Refresh),("refresh-camera",Action::RefreshCamera),("update-gps",Action::UpdateGps),("upload",if demo{Action::DemoUpload}else{Action::Upload})] {
            let a=gtk::gio::SimpleAction::new(name,None);let shared=shared.clone();let tx=tx.clone();
            a.connect_activate(move|_,_|{if matches!(action,Action::Quit){quit(&shared,&tx)}else{let _=tx.send(action.clone());}});app.add_action(&a);
        }
        if demo {
            for (name,connected) in [("demo-connect",true),("demo-disconnect",false)] {
                let a=gtk::gio::SimpleAction::new(name,None);let tx=tx.clone();
                a.connect_activate(move|_,_|{let _=tx.send(Action::DemoConnect(connected));});app.add_action(&a);
            }
        }
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("toughfix-content");
        let status_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
        status_area.add_css_class("status-area");
        let activity_card = gtk::Box::new(gtk::Orientation::Vertical, 10);
        activity_card.add_css_class("card");
        let activity_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(22, 22);
        let activity_title = label("activity-title");
        activity_title.set_hexpand(true);
        activity_title.set_max_width_chars(32);
        activity_title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        activity_title.set_selectable(false);
        activity_row.append(&spinner);
        activity_row.append(&activity_title);
        activity_card.append(&activity_row);
        let activity_detail = label("subtitle");
        activity_detail.set_lines(2);
        activity_detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        activity_card.append(&activity_detail);
        let progress = gtk::ProgressBar::new();
        progress.set_pulse_step(0.04);
        activity_card.append(&progress);
        let banner = label("subtitle");
        activity_card.append(&banner);
        let tray_error = label("subtitle");
        tray_error.set_lines(2);
        tray_error.set_ellipsize(gtk::pango::EllipsizeMode::End);
        activity_card.append(&tray_error);
        status_area.append(&activity_card);

        let page = gtk::Box::new(gtk::Orientation::Vertical, 14);
        page.add_css_class("page");
        page.set_hexpand(true);
        let camera_card = gtk::Box::new(gtk::Orientation::Vertical, 14);
        let camera_row = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        camera_card.add_css_class("card");
        let image_stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(include_bytes!("../desktop/assets/tg-1.png")));
        let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_stream(&image_stream, None::<&gtk::gio::Cancellable>).expect("Embedded camera illustration");
        let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
        let camera_image = gtk::Picture::for_paintable(&texture);
        camera_image.set_can_shrink(true);
        camera_image.set_size_request(180, 112);
        camera_image.set_valign(gtk::Align::Center);
        camera_image.set_alternative_text(Some("Illustration of an Olympus Tough TG-1 camera"));
        let image_stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from_static(include_bytes!("../desktop/assets/tough-8010.png")));
        let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_stream(&image_stream, None::<&gtk::gio::Cancellable>).expect("Embedded Tough-8010 illustration");
        let camera_8010_image = gtk::Picture::for_paintable(&gtk::gdk::Texture::for_pixbuf(&pixbuf));
        camera_8010_image.set_can_shrink(true);
        camera_8010_image.set_size_request(180, 112);
        camera_8010_image.set_valign(gtk::Align::Center);
        camera_8010_image.set_alternative_text(Some("Illustration of an Olympus Stylus Tough-8010 camera"));
        camera_8010_image.set_visible(false);
        let image_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        image_slot.append(&camera_image);
        image_slot.append(&camera_8010_image);
        let illustration = adw::Clamp::builder().maximum_size(180).tightening_threshold(180).child(&image_slot).build();
        camera_row.append(&illustration);
        let summary = gtk::Box::new(gtk::Orientation::Vertical, 10);
        summary.set_hexpand(true);
        let name = label("camera-name");
        name.set_text("Olympus Tough TG-1");
        summary.append(&name);
        let health_badge = label("summary");
        health_badge.set_hexpand(true);
        summary.append(&health_badge);
        let snapshot_row = gtk::Box::new(gtk::Orientation::Horizontal,8);
        let health_caption = label("value");
        health_caption.set_valign(gtk::Align::Center);
        snapshot_row.append(&health_caption);
        let camera_refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        camera_refresh.set_tooltip_text(Some("Refresh battery, health and SD card information. Briefly unmounts mounted camera storage; a busy card is left alone."));
        camera_refresh.set_valign(gtk::Align::Center);
        snapshot_row.append(&camera_refresh);
        summary.append(&snapshot_row);
        camera_row.append(&summary);
        camera_card.append(&camera_row);
        let resources_main = metrics::Resources::new();
        camera_card.append(&resources_main.widget);
        page.append(&camera_card);
        let gps_card = gtk::Box::new(gtk::Orientation::Horizontal,14);
        gps_card.add_css_class("card");
        gps_card.append(&earth::new());
        let gps_content = gtk::Box::new(gtk::Orientation::Vertical,10);
        gps_content.set_hexpand(true);
        let gps_title = label("card-title");
        gps_title.set_text("GPS assistance");
        gps_content.append(&gps_title);
        let assistance_summary = label("value");
        gps_content.append(&assistance_summary);
        let last_update_summary = label("subtitle");
        gps_content.append(&last_update_summary);
        let gps_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let update_gps = gtk::Button::with_label("Update GPS now");
        update_gps.add_css_class("suggested-action");
        update_gps.set_tooltip_text(Some("Fetch the latest satellite data and update this camera, ignoring the automatic interval. Briefly unmounts storage; a busy card is left alone. Identical assistance data is not rewritten."));
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Refresh satellite data and predictions without updating the camera"));
        gps_actions.append(&update_gps);
        gps_actions.append(&refresh);
        gps_content.append(&gps_actions);
        gps_card.append(&gps_content);
        page.append(&gps_card);

        let advanced = gtk::Box::new(gtk::Orientation::Vertical, 14);
        let c = card(&advanced, "Camera details");
        let connection = field(&c, "Connection");
        let camera = field(&c, "Health snapshot");
        let storage = field(&c, "Storage and mount paths");
        let c = card(&advanced, "Satellite observations");
        let mut gps_details = vec![c.clone()];
        let observed = field(&c, "Latest health observations");
        let fitted = field(&c, "Latest fitting data");
        let source = field(&c, "Source");
        let source_link = gtk::LinkButton::with_label("https://noaa-cors-pds.s3.amazonaws.com/", "Open source archive");
        source_link.set_halign(gtk::Align::Start);
        c.append(&source_link);
        let health = field(&c, "Satellite health");
        let training = field(&c, "Prediction input arc");
        let c = card(&advanced, "Prediction details");
        gps_details.push(c.clone());
        let validity = field(&c, "Validity");
        let satellites = field(&c, "Coverage");
        let quality = field(&c, "Model statistics");
        let clocks = field(&c, "Clock inputs");
        let hash = field(&c, "SHA-256");
        hash.add_css_class("mono");
        hash.set_wrap_mode(gtk::pango::WrapMode::Char);
        let c = card(&advanced, "Last confirmed camera commit");
        gps_details.push(c.clone());
        let commit = field(&c, "Recorded commit");
        let match_status = label("subtitle");
        c.append(&match_status);
        let commit_hash = field(&c, "Committed SHA-256");
        commit_hash.add_css_class("mono");
        commit_hash.set_wrap_mode(gtk::pango::WrapMode::Char);
        let note = label("subtitle");
        note.set_text("Acknowledged upload history. The stored assistance file cannot be read back to verify its contents.");
        c.append(&note);
        let upload = gtk::Button::with_label("Upload latest predictions");
        upload.set_halign(gtk::Align::Start);
        upload.add_css_class("suggested-action");
        advanced.append(&upload);
        let c = card(&advanced, "Details and diagnostics");
        let message = label("subtitle");
        c.append(&message);

        let settings = gtk::Box::new(gtk::Orientation::Vertical, 14);
        settings.add_css_class("card");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let auto_label = label("value");
        auto_label.set_text("Update automatically when connected");
        auto_label.set_hexpand(true);
        let automatic = gtk::Switch::new();
        automatic.set_valign(gtk::Align::Center);
        controls.append(&auto_label);
        controls.append(&automatic);
        settings.append(&controls);
        let upload_interval = adw::SpinRow::builder()
            .title("Minimum time between GPS uploads")
            .subtitle("Hours · default 48 · expiry and new satellite exclusions override this")
            .adjustment(&gtk::Adjustment::new(48., 1., 168., 1., 24., 0.))
            .build();
        settings.append(&upload_interval);
        camera_startup_setting(&settings, demo || monitor_only, config_dir.clone());
        advanced.add_css_class("page");
        let resources_details = metrics::Resources::new();
        let resources_card = gtk::Box::new(gtk::Orientation::Vertical,0);
        resources_card.add_css_class("card");
        resources_card.append(&resources_details.widget);
        advanced.prepend(&resources_card);
        let advanced_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&advanced).build();
        let settings_page = gtk::Box::new(gtk::Orientation::Vertical,14);
        settings_page.add_css_class("page");
        settings_page.append(&settings);
        let settings_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&settings_page).build();
        let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&page).build();
        let stack = adw::ViewStack::builder().hhomogeneous(false).vhomogeneous(false).vexpand(true).build();
        stack.add_titled_with_icon(&scroll,Some("camera"),"Camera","camera-photo-symbolic");
        stack.add_titled_with_icon(&advanced_scroll,Some("advanced"),"Advanced","view-list-symbolic");
        stack.add_titled_with_icon(&settings_scroll,Some("settings"),"Settings","preferences-system-symbolic");
        stack.set_visible_child_name("camera");
        let window_weak = window.downgrade();
        stack.connect_visible_child_name_notify(move |stack| {
            if let Some(window) = window_weak.upgrade() {
                window.set_title(Some(match stack.visible_child_name().as_deref() {
                    Some("settings") => "ToughFix — Settings",
                    Some("advanced") => "ToughFix — Advanced",
                    _ => "ToughFix",
                }));
            }
        });
        let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
        header.set_title_widget(Some(&switcher));
        let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
        toolbar.add_bottom_bar(&switcher_bar);
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 520sp").expect("Navigation breakpoint"));
        breakpoint.add_setter(&header,"title-widget",Some(&None::<gtk::Widget>.to_value()));
        breakpoint.add_setter(&switcher_bar,"reveal",Some(&true.to_value()));
        window.add_breakpoint(breakpoint);
        root.append(&status_area);
        root.append(&stack);
        if demo {
            let action = gtk::gio::SimpleAction::new("demo-auto-mounted",None);
            let mounted_shared = shared.clone();
            action.connect_activate(move |_,_| {
                let mut s = mounted_shared.lock().unwrap();
                s.storage_preparing = false;
                s.storage_error = None;
                s.camera_note = Some("Storage is ready for browsing · use camera refresh for a current snapshot".into());
                if let Some(d) = s.device.as_mut() { d.mounts=vec!["/media/demo-camera".into()]; }
            });
            app.add_action(&action);
            let action = gtk::gio::SimpleAction::new("demo-recent-update",None);
            let recent_shared = shared.clone();
            action.connect_activate(move |_,_| {
                let mut s = recent_shared.lock().unwrap();
                let gps = crate::model::now_gps();
                let date = |offset: i64| chrono::DateTime::from_timestamp((gps+315964800.) as i64+offset,0).unwrap().format("%Y-%m-%dT%H:%M:%S").to_string();
                s.receipt = Some(crate::model::Receipt {
                    camera_key: Some("demo".into()), usb_key: Some("demo".into()), sha256: "earlier-demo-predictions".into(), bytes:130720,
                    start_gps:date(-86400), end_gps:date(13*86400), committed_at:chrono::Utc::now()-chrono::Duration::hours(6),
                    session_closed:true, origin:"Demo only".into(), excluded_prns:Some(s.data.excluded_prns.clone()),
                });
            });
            app.add_action(&action);
            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            for (name, connected) in [("Connect demo camera", true), ("Disconnect demo camera", false)] {
                let button = gtk::Button::with_label(name);
                let tx = tx.clone();
                button.connect_clicked(move |_| { let _ = tx.send(Action::DemoConnect(connected)); });
                buttons.append(&button);
            }
            settings_page.append(&buttons);
        }
        toolbar.set_content(Some(&root));
        window.set_content(Some(&toolbar));
        if demo {
            let action = gtk::gio::SimpleAction::new("demo-refresh-failed", None);
            let demo_state = shared.clone();
            action.connect_activate(move |_, _| {
                let mut state = demo_state.lock().unwrap();
                let error = "Downloading https://maia.usno.navy.mil/ser7/finals2000A.all: client error (Connect): Connection reset by peer (os error 104)".to_owned();
                state.data.refresh_error = Some(error.clone());
                state.data.failures = vec![error, "Satellite health checks need refreshing".into()];
                state.data.upload_allowed = false;
                state.refresh_retry_seconds = Some(30);
                state.updating_sources = false;
                state.storage_preparing = false;
                state.phase = crate::model::Phase::Idle;
            });
            app.add_action(&action);
            let action = gtk::gio::SimpleAction::new("demo-8010", None);
            let demo_state = shared.clone();
            action.connect_activate(move |_, _| {
                let mut state = demo_state.lock().unwrap();
                if let Some(device) = state.device.as_mut() {
                    device.model = crate::model::CameraModel::Tough8010;
                }
                if let Some(camera) = state.camera.as_mut() {
                    camera.gps_chip = 0;
                    camera.transfer_limit = 0;
                    camera.battery = Some(100);
                }
                state.receipt = None;
            });
            app.add_action(&action);
            for name in ["demo-preparing", "demo-storage-error", "demo-storage-ready"] {
                let action = gtk::gio::SimpleAction::new(name, None);
                let shared = shared.clone();
                action.connect_activate(move |_, _| {
                    let mut state = shared.lock().unwrap();
                    state.storage_preparing = name == "demo-preparing";
                    state.storage_error = (name == "demo-storage-error")
                        .then(|| "Demo mount failure".into());
                });
                app.add_action(&action);
            }
            let stale = gtk::gio::SimpleAction::new("demo-camera-old",None);
            let stale_state = shared.clone();
            stale.connect_activate(move |_,_| {
                let mut s = stale_state.lock().unwrap();
                if let Some(c) = s.camera.as_mut() { c.read_at = Some(chrono::Utc::now()-chrono::Duration::hours(1)); }
                s.camera_cached = true;
            });
            app.add_action(&stale);
            for (action_name, expanded) in [("demo-advanced", true), ("demo-basic", false)] {
                let action = gtk::gio::SimpleAction::new(action_name, None);
                let stack = stack.clone();
                action.connect_activate(move |_, _| {
                    stack.set_visible_child_name(if expanded { "advanced" } else { "camera" });
                });
                app.add_action(&action);
            }
            for (name, level) in [("demo-battery-low",Some(12)),("demo-battery-full",Some(100)),("demo-battery-unknown",None)] {
                let action = gtk::gio::SimpleAction::new(name,None);
                let shared = shared.clone();
                action.connect_activate(move |_,_| {
                    if let Some(camera) = shared.lock().unwrap().camera.as_mut() { camera.battery = level; }
                });
                app.add_action(&action);
            }
            let action = gtk::gio::SimpleAction::new("demo-settings",None);
            let settings_stack = stack.clone();
            action.connect_activate(move |_,_| {settings_stack.set_visible_child_name("settings");});
            app.add_action(&action);
            let mounted = gtk::gio::SimpleAction::new("demo-mounted",None);
            let shared_mounted = shared.clone();
            mounted.connect_activate(move |_,_| {
                let mut s = shared_mounted.lock().unwrap();
                s.camera = None; s.camera_cached = false;
                if let Some(d) = s.device.as_mut() {
                    d.mounts = vec!["/demo/SD-card".into()];
                    d.mounted_storage = vec![crate::model::Storage{capacity:32014073856,free:29278076928,writable:true,label:"SD card".into()}];
                }
            });
            app.add_action(&mounted);
            let cached = gtk::gio::SimpleAction::new("demo-cached",None);
            let shared_cached = shared.clone();
            cached.connect_activate(move |_,_| {
                let mut s = shared_cached.lock().unwrap();
                s.camera_cached = s.camera.is_some();
                if let Some(d) = s.device.as_mut() { d.mounts = vec!["/demo/SD-card".into()]; }
            });
            app.add_action(&cached);
            for (name,enabled) in [("demo-motion-off",false),("demo-motion-on",true)] {
                let action = gtk::gio::SimpleAction::new(name,None);
                action.connect_activate(move |_,_| { if let Some(settings) = gtk::Settings::default() { settings.set_gtk_enable_animations(enabled); } });
                app.add_action(&action);
            }
            let hide = gtk::gio::SimpleAction::new("demo-hide", None);
            let hidden_window = window.clone();
            hide.connect_activate(move |_, _| { hidden_window.hide(); });
            app.add_action(&hide);
            let close = gtk::gio::SimpleAction::new("demo-close", None);
            let window = window.clone();
            close.connect_activate(move |_, _| { window.close(); });
            app.add_action(&close);
            for (action_name, stage) in [("demo-observations", crate::predictor::RefreshStage::Observations), ("demo-calculating", crate::predictor::RefreshStage::Calculating), ("demo-idle", crate::predictor::RefreshStage::Validating)] {
                let action = gtk::gio::SimpleAction::new(action_name, None);
                let shared = shared.clone();
                action.connect_activate(move |_, _| {
                    let mut state = shared.lock().unwrap();
                    state.updating_sources = action_name != "demo-idle";
                    state.source_progress = Some(crate::predictor::Progress {
                        stage, detail: "Downloading observed GPS orbits from NOAA / IGS".into(),
                        completed: (stage == crate::predictor::RefreshStage::Calculating).then_some((12, 32)),
                    });
                });
                app.add_action(&action);
            }
        }
        let fields = Rc::new(Fields { connection, camera, storage, observed, fitted, clocks, source, source_link, health, training, validity, satellites, quality, hash, commit, match_status, commit_hash, banner, progress, message, upload, refresh, update_gps, camera_refresh, automatic, upload_interval, tray_error, activity_title, activity_detail, spinner, activity_card, camera_image, camera_8010_image, camera_name: name, gps_card, gps_details, health_badge, health_caption, resources: vec![resources_main,resources_details], assistance_summary, last_update_summary });
        let initial_state = shared.lock().unwrap().clone();
        render(&fields, &initial_state, monitor_only);
        {let tx=tx.clone();fields.refresh.connect_clicked(move |_|{let _=tx.send(Action::Refresh);});}
        {let tx=tx.clone();fields.update_gps.connect_clicked(move |_|{let _=tx.send(Action::UpdateGps);});}
        {let tx=tx.clone();fields.camera_refresh.connect_clicked(move |_|{let _=tx.send(Action::RefreshCamera);});}
        {let tx=tx.clone();fields.upload.connect_clicked(move |_|{let _=tx.send(if demo{Action::DemoUpload}else{Action::Upload});});}
        {let shared=shared.clone();let tx=tx.clone();fields.automatic.connect_active_notify(move|switch|{
            if shared.lock().unwrap().automatic!=switch.is_active(){let _=tx.send(Action::SetAutomatic(switch.is_active()));}
        });}
        {let shared=shared.clone();let tx=tx.clone();fields.upload_interval.connect_value_notify(move|row|{
            let hours = row.value() as u32;
            if shared.lock().unwrap().upload_interval_hours() != hours {
                let _ = tx.send(Action::SetUploadInterval(hours));
            }
        });}
        let ui_actions=Arc::new(Mutex::new(Vec::new()));let tray=Rc::new(RefCell::new(Manager::new()));
        let app=app.clone();let window_timer=window.clone();let shared=shared.clone();let tx=tx.clone();let exiting=exiting.clone();
        let mut absent_since=Instant::now();let mut seen_camera=false;
        glib::timeout_add_local(Duration::from_millis(200),move||{
            for action in std::mem::take(&mut *ui_actions.lock().unwrap()) {
                match action{Action::Open=>window_timer.present(),Action::Quit=>quit(&shared,&tx),_=>()}
            }
            let s=shared.lock().unwrap().clone();render(&fields,&s,monitor_only);
            if s.device.is_some(){seen_camera=true;absent_since=Instant::now();}
            if should_exit(hotplug, window_timer.is_visible(), s.phase.device_busy(), s.updating_sources, absent_since.elapsed().as_secs(), seen_camera) && !s.quit_pending {quit(&shared,&tx);}
            tray.borrow_mut().sync(&shared,&tx,&ui_actions);
            let error=tray.borrow().error.clone();fields.tray_error.set_text(error.as_deref().unwrap_or_default());fields.tray_error.set_visible(error.is_some());
            if error.is_some() && !window_timer.is_visible(){window_timer.present();}
            if s.stopped{*exiting.borrow_mut()=true;hold.borrow_mut().take();app.quit();return glib::ControlFlow::Break}
            glib::ControlFlow::Continue
        });
        *existing_for_activate.borrow_mut()=Some(window.clone());
        if !background{window.present();}
    });
    // GTK parses only our empty argument list; our own CLI options are handled first.
    app.run_with_args::<&str>(&[]);
}

fn camera_startup_setting(parent: &gtk::Box, disabled: bool, config_dir: Option<PathBuf>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let title = label("value");
    title.set_text("Start when camera connects");
    title.set_hexpand(true);
    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    row.append(&title);
    row.append(&switch);
    parent.append(&row);
    let status = label("subtitle");
    parent.append(&status);
    let setup = (|| -> anyhow::Result<_> {
        anyhow::ensure!(
            !disabled,
            "Startup settings are unavailable in demo or monitor-only mode."
        );
        let startup = match config_dir {
            Some(path) => CameraStartup::at(path)?,
            None => CameraStartup::for_user()?,
        };
        anyhow::ensure!(
            startup.installed(),
            "Install ToughFix to enable automatic startup on camera connection."
        );
        let enabled = startup.enabled()?;
        Ok((startup, enabled))
    })();
    match setup {
        Err(e) => {
            switch.set_sensitive(false);
            status.set_text(&e.to_string());
        }
        Ok((startup, enabled)) => {
            switch.set_active(enabled);
            status.set_text(
                "Starts in the background in USB Storage mode. The tray appears while connected.",
            );
            let saved = Rc::new(Cell::new(enabled));
            switch.connect_active_notify(move |switch| {
                let enabled = switch.is_active();
                if enabled == saved.get() {
                    return;
                }
                match startup.set_enabled(enabled) {
                    Ok(()) => {
                        saved.set(enabled);
                        status.set_text(if enabled {
                            "Saved. Starts on the next camera connection."
                        } else {
                            "Saved. Open ToughFix from the application menu to use it."
                        });
                    }
                    Err(e) => {
                        switch.set_active(saved.get());
                        status.set_text(&format!("Could not save startup preference: {e:#}"));
                    }
                }
            });
        }
    }
}
