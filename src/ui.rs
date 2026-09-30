// SPDX-License-Identifier: MIT
use crate::{
    backend::Shared,
    model::{Action, State, age, bytes, gps_calendar},
    startup::{CameraStartup, should_exit},
    tray::Manager,
};
use gtk::{glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, mpsc::Sender},
    time::{Duration, Instant},
};

const STYLE: &str = r#"
window { background: #101b20; color: #e4eeed; }
headerbar { background: #142329; border-bottom: 1px solid #2b3d43; }
.page { padding: 28px; }
.hero { font-size: 27px; font-weight: 700; }
.eyebrow { color: #7edfc1; font-size: 11px; font-weight: 700; letter-spacing: 1px; }
.subtitle { color: #9aadb1; font-size: 13px; }
.card { background: #17272d; border: 1px solid #2c3d43; border-radius: 14px; padding: 20px; }
.card-title { font-size: 16px; font-weight: 700; margin-bottom: 10px; }
.key { color: #9bafb3; font-size: 12px; }
.value { color: #e1edeb; font-size: 13px; }
.mono { font-family: monospace; font-size: 11px; color: #acc4c4; }
.notice { background: #203a34; color: #9ae5c9; padding: 14px; border-radius: 10px; }
.warning { background: #483a21; color: #ffcf83; }
.error { background: #42272d; color: #ffb3ba; }
linkbutton, linkbutton label, link { color: #7edfc1; }
button { border-radius: 8px; padding: 8px 14px; }
button.suggested-action { background: #54cda7; color: #0d2922; }
progressbar trough { background: #243940; border-radius: 4px; min-height: 6px; }
progressbar progress { background: #f9b547; min-height: 6px; }
switch { min-width: 40px; min-height: 24px; }
switch slider { min-width: 20px; min-height: 20px; }
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
    automatic: gtk::Switch,
    tray_error: gtk::Label,
}
fn label(class: &str) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(0.);
    l.set_wrap(true);
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
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let key = label("key");
    key.set_text(title);
    key.set_width_chars(18);
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
    f.connection.set_text(
        &s.device
            .as_ref()
            .map(|d| format!("Olympus TG-1 · {}", d.path.display()))
            .unwrap_or_else(|| "No camera connected".into()),
    );
    f.camera.set_text(&s.camera_error.clone().unwrap_or_else(||s.camera.as_ref().map(|c|format!("Camera responds normally · firmware {}\nBattery: {} · GPS interface {}\nLast checked {}",c.firmware,c.battery.map(|b|format!("{b}%")).unwrap_or_else(||"not reported".into()),c.gps_chip,c.read_at.map(|t|t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default())).unwrap_or_else(||if s.device.is_some(){"Waiting for camera information"}else{"Connect the camera in Storage mode"}.into())));
    let storage = s
        .camera
        .as_ref()
        .map(|c| {
            c.storage
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
    f.storage.set_text(&format!("{storage}{mounted}"));
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
        } else {
            "Latest local predictions differ, or camera identity is still being checked."
        });
    } else {
        f.commit.set_text("No acknowledged commit recorded");
        f.commit_hash.set_text("");
        f.match_status
            .set_text("A commit is recorded only after the camera acknowledges saving the file.");
    }
    f.banner.set_text(&format!(
        "{} · {}{}",
        s.unplug_message(),
        s.phase.label(),
        if s.quit_pending {
            " · will quit when finished"
        } else {
            ""
        }
    ));
    for class in ["warning", "error"] {
        f.banner.remove_css_class(class);
    }
    if s.phase.device_busy() {
        f.banner.add_css_class("warning");
    } else if s.reconnect_required || s.camera_error.is_some() {
        f.banner.add_css_class("error");
    }
    f.progress.set_visible(s.phase.device_busy());
    f.progress.set_fraction(s.phase.fraction());
    let issues = if s.data.failures.is_empty() {
        String::new()
    } else {
        format!("\n{}", s.data.failures.join("\n"))
    };
    f.message.set_text(&format!(
        "{}{}{}",
        s.message,
        issues,
        if monitor_only {
            "\nMonitor mode · uploads and network refresh are disabled"
        } else {
            ""
        }
    ));
    f.upload.set_sensitive(
        s.device.is_some()
            && s.camera.is_some()
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
    } else if s.matches_latest() {
        "Upload again"
    } else {
        "Upload latest predictions"
    });
    f.refresh.set_sensitive(
        !s.phase.device_busy()
            && !s.updating_sources
            && !s.quit_pending
            && !s.demo
            && !monitor_only,
    );
    f.automatic.set_sensitive(!monitor_only && !s.quit_pending);
    if f.automatic.is_active() != s.automatic {
        f.automatic.set_active(s.automatic);
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
    let app = gtk::Application::builder()
        .application_id(if demo {
            "org.toughfix.Desktop.Demo"
        } else {
            "org.toughfix.Desktop"
        })
        .build();
    let existing: Rc<RefCell<Option<gtk::ApplicationWindow>>> = Rc::new(RefCell::new(None));
    let exiting = Rc::new(RefCell::new(false));
    let existing_for_activate = existing.clone();
    app.connect_activate(move|app|{
        if let Some(w)=existing_for_activate.borrow().as_ref(){w.present();return}
        let _hold=app.hold();
        let hold=Rc::new(RefCell::new(Some(_hold)));
        let provider=gtk::CssProvider::new();provider.load_from_data(STYLE);
        gtk::style_context_add_provider_for_display(&gtk::gdk::Display::default().expect("GTK display"),&provider,gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let window=gtk::ApplicationWindow::builder().application(app).title("ToughFix").default_width(820).default_height(820).hide_on_close(true).build();
        let header=gtk::HeaderBar::new();let title=gtk::Label::new(Some("ToughFix"));header.set_title_widget(Some(&title));
        let quit_button=gtk::Button::with_label("Quit");header.pack_end(&quit_button);window.set_titlebar(Some(&header));
        for (name,action) in [("quit",Action::Quit),("refresh",Action::Refresh),("upload",if demo{Action::DemoUpload}else{Action::Upload})] {
            let a=gtk::gio::SimpleAction::new(name,None);let shared=shared.clone();let tx=tx.clone();
            a.connect_activate(move|_,_|{if matches!(action,Action::Quit){quit(&shared,&tx)}else{let _=tx.send(action.clone());}});app.add_action(&a);
        }
        if demo {
            for (name,connected) in [("demo-connect",true),("demo-disconnect",false)] {
                let a=gtk::gio::SimpleAction::new(name,None);let tx=tx.clone();
                a.connect_activate(move|_,_|{let _=tx.send(Action::DemoConnect(connected));});app.add_action(&a);
            }
        }
        let page=gtk::Box::new(gtk::Orientation::Vertical,16);page.add_css_class("page");page.set_halign(gtk::Align::Center);page.set_width_request(780);
        let eyebrow=label("eyebrow");eyebrow.set_selectable(false);eyebrow.set_text(if demo{"DEMO · NO CAMERA OPERATIONS"}else{"OLYMPUS TG-1 / GPS ASSISTANCE"});page.append(&eyebrow);
        let hero=label("hero");hero.set_selectable(false);hero.set_text("Ready for the next trail.");page.append(&hero);
        let subtitle=label("subtitle");subtitle.set_text("Satellite predictions, camera status, and confirmed updates in one place.");page.append(&subtitle);
        let banner=label("notice");page.append(&banner);
        let progress=gtk::ProgressBar::new();page.append(&progress);
        let c=card(&page,"Camera");let connection=field(&c,"Connection");let camera=field(&c,"Health");let storage=field(&c,"Storage");
        let c=card(&page,"Satellite observations");let observed=field(&c,"Latest health observations");let fitted=field(&c,"Latest fitting data");let source=field(&c,"Source");
        let source_link=gtk::LinkButton::with_label("https://noaa-cors-pds.s3.amazonaws.com/","Open source archive");source_link.set_halign(gtk::Align::Start);c.append(&source_link);
        let health=field(&c,"Satellite health");let training=field(&c,"Prediction input arc");
        let c=card(&page,"Latest predictions");let validity=field(&c,"Validity");let satellites=field(&c,"Coverage");let quality=field(&c,"Model statistics");let clocks=field(&c,"Clock inputs");
        let hash=field(&c,"SHA-256");hash.add_css_class("mono");
        let c=card(&page,"Last confirmed camera commit");let commit=field(&c,"Recorded commit");let match_status=label("subtitle");c.append(&match_status);
        let commit_hash=field(&c,"Committed SHA-256");commit_hash.add_css_class("mono");
        let note=label("subtitle");note.set_text("This is the camera’s acknowledged upload history. The stored assistance file cannot currently be read back to verify its contents.");c.append(&note);
        let controls=gtk::Box::new(gtk::Orientation::Horizontal,10);let auto_label=label("value");auto_label.set_text("Update automatically when connected");auto_label.set_hexpand(true);
        let automatic=gtk::Switch::new();automatic.set_valign(gtk::Align::Center);controls.append(&auto_label);controls.append(&automatic);page.append(&controls);
        let settings=card(&page,"Settings");
        camera_startup_setting(&settings, demo || monitor_only, config_dir.clone());
        let buttons=gtk::Box::new(gtk::Orientation::Horizontal,10);let refresh=gtk::Button::with_label("Refresh satellite data");let upload=gtk::Button::with_label("Upload latest predictions");upload.add_css_class("suggested-action");buttons.append(&refresh);buttons.append(&upload);page.append(&buttons);
        if demo {
            for (name,connected) in [("Connect demo camera",true),("Disconnect demo camera",false)] {
                let button=gtk::Button::with_label(name);let tx=tx.clone();button.connect_clicked(move |_|{let _=tx.send(Action::DemoConnect(connected));});buttons.append(&button);
            }
        }
        let message=label("subtitle");page.append(&message);let tray_error=label("subtitle");page.append(&tray_error);
        let scroll=gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&page).build();window.set_child(Some(&scroll));scroll.set_focusable(true);gtk::prelude::GtkWindowExt::set_focus(&window,Some(&scroll));
        if demo{let hide=gtk::gio::SimpleAction::new("demo-hide",None);let window=window.clone();hide.connect_activate(move|_,_|window.hide());app.add_action(&hide);}
        let fields=Rc::new(Fields{connection,camera,storage,observed,fitted,clocks,source,source_link,health,training,validity,satellites,quality,hash,commit,match_status,commit_hash,banner,progress,message,upload,refresh,automatic,tray_error});
        {let shared=shared.clone();let tx=tx.clone();quit_button.connect_clicked(move |_|quit(&shared,&tx));}
        {let tx=tx.clone();fields.refresh.connect_clicked(move |_|{let _=tx.send(Action::Refresh);});}
        {let tx=tx.clone();fields.upload.connect_clicked(move |_|{let _=tx.send(if demo{Action::DemoUpload}else{Action::Upload});});}
        {let shared=shared.clone();let tx=tx.clone();fields.automatic.connect_active_notify(move|switch|{
            if shared.lock().unwrap().automatic!=switch.is_active(){let _=tx.send(Action::SetAutomatic(switch.is_active()));}
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
            let error=tray.borrow().error.clone();fields.tray_error.set_text(error.as_deref().unwrap_or_default());
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
