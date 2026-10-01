// SPDX-License-Identifier: MIT
mod backend;
mod camera;
mod files;
mod install;
mod model;
mod predictor;
mod startup;
mod storage;
#[cfg(test)]
mod test_support;
mod tray;
mod ui;

use anyhow::{Context, Result, bail};
use gtk::prelude::*;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("ToughFix: {error:#}");
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!(
            "ToughFix {}",
            option_env!("TOUGHFIX_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
        );
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("storage-release") {
        return storage::fallback();
    }
    if std::env::args().nth(1).as_deref() == Some("install") {
        return install::cli(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("predict") {
        return predictor::cli(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("refresh") {
        return predictor::engine::cli(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("audit") {
        return predictor::audit::cli(std::env::args().skip(2));
    }
    let mut project = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut state_dir = None;
    let mut config_dir = None;
    let mut demo = false;
    let mut background = false;
    let mut monitor_only = false;
    let mut hotplug = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => project = args.next().context("--project needs a path")?.into(),
            "--config-dir" => {
                config_dir = Some(PathBuf::from(
                    args.next().context("--config-dir needs a path")?,
                ))
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(
                    args.next().context("--state-dir needs a path")?,
                ))
            }
            "--demo" => demo = true,
            "--background" => background = true,
            "--hotplug" => {
                hotplug = true;
                background = true;
            }
            "--monitor-only" => monitor_only = true,
            "--help" | "-h" => {
                println!(
                    "ToughFix — Olympus TG-1 GPS assistance\n\npredict         Generate an offline CEP candidate (see predict --help)\nrefresh         Download, generate, validate and publish assistance; no camera access\naudit           Independent numerical trajectory check; no camera access\ninstall         Install/update the app (see install --help)\n--background    Start without opening a window\n--hotplug       Background camera session; exit when disconnected and window hidden\n--monitor-only  Disable network refresh and all uploads\n--demo          Simulated camera; no device access or network requests\n--project PATH  Optional legacy research-history import\n--state-dir PATH  Override app state directory\n--config-dir PATH  Override desktop preferences/service directory\n\nClosing the window hides it while connected and quits when no camera is connected. Hidden instances exit after disconnection. Quit waits for an active camera operation."
                );
                return Ok(());
            }
            _ => bail!("Unknown option {arg}; use --help"),
        }
    }
    // Legacy history import is optional; runtime operation needs no checkout.
    let project = project.canonicalize().unwrap_or(project);
    let state_dir = state_dir.unwrap_or_else(|| {
        if demo {
            return default_state_dir().join("demo");
        }
        default_state_dir()
    });
    let config = backend::Config {
        project,
        state_dir,
        demo,
        monitor_only,
    };
    let lock = match backend::lock_instance(&config) {
        Ok(file) => file,
        Err(error) => {
            // Relaunching from the application menu opens the existing window.
            if error.to_string().contains("already running") {
                let app = gtk::Application::builder()
                    .application_id(if demo {
                        "org.toughfix.Desktop.Demo"
                    } else {
                        "org.toughfix.Desktop"
                    })
                    .build();
                app.register(None::<&gtk::gio::Cancellable>)?;
                if app.is_remote() {
                    if !hotplug {
                        app.activate();
                    }
                    return Ok(());
                }
            }
            return Err(error);
        }
    };
    let shared = Arc::new(Mutex::new(model::State::default()));
    let (tx, rx) = mpsc::channel();
    backend::start(config, shared.clone(), rx);
    ui::run(
        shared,
        tx,
        background,
        demo,
        monitor_only,
        hotplug,
        config_dir,
    );
    drop(lock);
    Ok(())
}

pub fn default_state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    let current = base.join("toughfix");
    let legacy = base.join("tg-agps");
    if !current.exists() && legacy.exists() {
        legacy
    } else {
        current
    }
}
