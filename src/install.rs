// SPDX-License-Identifier: MIT
//! Native per-user installation. Only the narrow udev/driver setup uses sudo.
use crate::files;
use anyhow::{Context, Result, bail, ensure};
use std::{
    env, fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Command,
};

const RULE: &str = "/etc/udev/rules.d/70-toughfix.rules";
const MODULE: &str = "/etc/modules-load.d/toughfix.conf";
const RULE_BYTES: &[u8] = include_bytes!("../desktop/70-toughfix.rules");
const MODULE_BYTES: &[u8] = include_bytes!("../desktop/toughfix.conf");
const ICON: &[u8] = include_bytes!("../desktop/toughfix.svg");

struct Options {
    prefix: PathBuf,
    data: PathBuf,
    config: PathBuf,
    stage: Option<PathBuf>,
    camera: bool,
}
fn staged(path: &Path, stage: Option<&Path>) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "Installation paths must be absolute: {}",
        path.display()
    );
    Ok(match stage {
        Some(root) => root.join(path.strip_prefix("/")?),
        None => path.into(),
    })
}
fn text(path: &Path) -> Result<&str> {
    let s = path.to_str().context("Installation paths must be UTF-8")?;
    ensure!(
        !s.chars().any(|c| matches!(c, '\n' | '\r' | '\0')),
        "Installation paths cannot contain newline or NUL"
    );
    Ok(s)
}
fn desktop_arg(path: &Path) -> Result<String> {
    let mut s = String::new();
    for c in text(path)?.chars() {
        if "\\\"`$".contains(c) {
            s.push('\\');
        }
        s.push(c);
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('%', "%%")
    ))
}
fn service_arg(path: &Path) -> Result<String> {
    Ok(format!(
        "\"{}\"",
        text(path)?
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    ))
}
fn launcher(binary: &Path, config: &Path) -> Result<Vec<u8>> {
    ensure!(
        !text(binary)?.contains('='),
        "Desktop executable path cannot contain ="
    );
    Ok(format!("[Desktop Entry]\nType=Application\nName=ToughFix\nComment=Olympus TG-1 GPS assistance and camera status\nExec=/usr/bin/env -- {} --config-dir {}\nIcon=toughfix\nTerminal=false\nCategories=Utility;\nStartupNotify=true\nDBusActivatable=false\n",desktop_arg(binary)?,desktop_arg(config)?).into_bytes())
}
fn service(binary: &Path, config: &Path) -> Result<Vec<u8>> {
    Ok(format!("[Unit]\nDescription=ToughFix camera connection\nRequisite=graphical-session.target\nAfter=graphical-session.target\nPartOf=graphical-session.target\nConditionUser=!root\nConditionEnvironment=|DISPLAY\nConditionEnvironment=|WAYLAND_DISPLAY\n\n[Service]\nType=exec\nExecCondition=/usr/bin/test ! -e {}\nExecStart=/usr/bin/env -- {} --config-dir {} --background --hotplug\nRestart=no\n",service_arg(&config.join("toughfix/camera-start-disabled"))?,service_arg(binary)?,service_arg(config)?).into_bytes())
}
fn run(args: &[String], privileged: bool) -> Result<()> {
    let mut cmd = if privileged && unsafe { libc::geteuid() } != 0 {
        let mut c = Command::new("sudo");
        c.arg("--").arg(&args[0]);
        c
    } else {
        Command::new(&args[0])
    };
    let status = cmd
        .args(&args[1..])
        .status()
        .with_context(|| format!("Running {}", args[0]))?;
    ensure!(status.success(), "{} failed with {status}", args[0]);
    Ok(())
}
fn camera_access(stage: Option<&Path>, force: bool) -> Result<()> {
    let inputs = [(RULE, RULE_BYTES), (MODULE, MODULE_BYTES)];
    if let Some(stage) = stage {
        for (path, bytes) in inputs {
            files::atomic_mode(&staged(Path::new(path), Some(stage))?, bytes, Some(0o644))?;
        }
        println!("Camera setup staged; no system commands run.");
        return Ok(());
    }
    let pending: Vec<_> = inputs
        .into_iter()
        .filter(|(p, b)| !fs::read(p).is_ok_and(|v| v == *b))
        .collect();
    let load = !Path::new("/sys/module/sg").exists();
    if pending.is_empty() && !load && !force {
        println!("Camera access is already configured.");
        return Ok(());
    }
    println!("Only camera rules and driver setup require sudo. The app runs as your desktop user.");
    for (path, bytes) in pending {
        // /tmp staging is private; sudo copies only these two reviewed embedded files.
        let staging = env::temp_dir().join(format!("toughfix-camera-{}", std::process::id()));
        // create (not create_dir_all) rejects a pre-existing path or symlink.
        fs::DirBuilder::new().mode(0o700).create(&staging)?;
        let source = staging.join(Path::new(path).file_name().unwrap());
        files::atomic_mode(&source, bytes, Some(0o644))?;
        let result = run(
            &[
                "install".into(),
                "-D".into(),
                "-m".into(),
                "0644".into(),
                "--".into(),
                text(&source)?.into(),
                path.into(),
            ],
            true,
        );
        fs::remove_file(source)?;
        fs::remove_dir(staging)?;
        result?;
    }
    if load {
        run(&["modprobe".into(), "sg".into()], true)?;
    }
    run(
        &["udevadm".into(), "control".into(), "--reload-rules".into()],
        true,
    )?;
    println!("Reconnect the camera to apply its access and launch rule.");
    Ok(())
}
fn install(options: &Options, binary: &Path) -> Result<()> {
    let installed = options.prefix.join("bin/toughfix");
    let launcher = launcher(&installed, &options.config)?;
    let service = service(&installed, &options.config)?;
    let stage = options.stage.as_deref();
    let paths = [
        staged(&installed, stage)?,
        staged(
            &options
                .data
                .join("applications/org.toughfix.Desktop.desktop"),
            stage,
        )?,
        staged(
            &options
                .data
                .join("icons/hicolor/scalable/apps/toughfix.svg"),
            stage,
        )?,
        staged(
            &options.config.join("systemd/user/toughfix-camera.service"),
            stage,
        )?,
    ];
    let bytes = fs::read(binary)?;
    for ((path, content), mode) in paths
        .iter()
        .zip([
            bytes.as_slice(),
            launcher.as_slice(),
            ICON,
            service.as_slice(),
        ])
        .zip([0o755, 0o644, 0o644, 0o644])
    {
        files::atomic_mode(path, content, Some(mode))?;
    }
    if stage.is_none() {
        run(
            &["systemctl".into(), "--user".into(), "daemon-reload".into()],
            false,
        )?;
    }
    if options.camera {
        camera_access(stage, false)?;
    }
    println!("Installed ToughFix: {}", paths[0].display());
    println!("Camera connection starts ToughFix; change this preference in the app.");
    println!(
        "Runtime data download automatically into your ToughFix state directory. No Python or checkout is required."
    );
    println!(
        "Quit an already running version after any camera operation, then reopen to use this update."
    );
    Ok(())
}
pub fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    let home = PathBuf::from(env::var_os("HOME").context("HOME is unavailable")?);
    let mut prefix = home.join(".local");
    let mut data = None;
    let mut config = None;
    let mut stage = None;
    let mut camera = true;
    let mut camera_only = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--prefix" => prefix = args.next().context("--prefix needs a path")?.into(),
            "--data-dir" => {
                data = Some(PathBuf::from(
                    args.next().context("--data-dir needs a path")?,
                ))
            }
            "--config-dir" => {
                config = Some(PathBuf::from(
                    args.next().context("--config-dir needs a path")?,
                ))
            }
            "--destdir" => {
                let value = args.next().context("--destdir needs a path")?;
                if !value.is_empty() {
                    let path = PathBuf::from(value);
                    ensure!(path.is_absolute(), "DESTDIR must be absolute");
                    stage = Some(path);
                }
            }
            "--camera-access" => {
                camera = match args.next().as_deref() {
                    Some("0") => false,
                    Some("1") => true,
                    _ => bail!("--camera-access must be 0 or 1"),
                }
            }
            "--camera-only" => camera_only = true,
            "--help" | "-h" => {
                println!(
                    "toughfix install [--prefix PATH] [--data-dir PATH] [--config-dir PATH] [--destdir PATH] [--camera-access 0|1] [--camera-only]\nPer-user installation; sudo is requested only for camera setup. Never starts an updater job."
                );
                return Ok(());
            }
            _ => bail!("Unknown installation option {arg}"),
        }
    }
    if camera_only {
        return camera_access(stage.as_deref(), true);
    }
    ensure!(
        unsafe { libc::geteuid() } != 0 || stage.is_some(),
        "Run make install as your desktop user, not root"
    );
    let options = Options {
        data: data
            .or_else(|| env::var_os("XDG_DATA_HOME").map(PathBuf::from))
            .unwrap_or_else(|| prefix.join("share")),
        config: config
            .or_else(|| env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .unwrap_or_else(|| home.join(".config")),
        prefix,
        stage,
        camera,
    };
    install(&options, &env::current_exe()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Temp;
    use gtk::{gio, prelude::*};
    use std::{
        fs::File,
        io::Read,
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };
    #[test]
    fn staged_native_install_preserves_startup_choice_and_running_inode() {
        let temp = Temp::new();
        let source = temp.path().join("source");
        fs::write(&source, b"old executable").unwrap();
        let options = Options {
            prefix: "/usr".into(),
            data: "/usr/share".into(),
            config: "/etc/xdg".into(),
            stage: Some(temp.path().join("stage")),
            camera: true,
        };
        install(&options, &source).unwrap();
        let stage = options.stage.as_ref().unwrap();
        let binary = stage.join("usr/bin/toughfix");
        let mut running = File::open(&binary).unwrap();
        let marker = stage.join("etc/xdg/toughfix/camera-start-disabled");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, b"user choice").unwrap();
        fs::write(&source, b"new executable").unwrap();
        install(&options, &source).unwrap();
        let mut old = Vec::new();
        running.read_to_end(&mut old).unwrap();
        assert_eq!(old, b"old executable");
        assert_eq!(fs::read(&binary).unwrap(), b"new executable");
        assert_eq!(
            fs::metadata(binary).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read(marker).unwrap(), b"user choice");
        assert!(!stage.join("etc/xdg/autostart").exists());
        assert_eq!(
            fs::read(stage.join(RULE.trim_start_matches('/'))).unwrap(),
            RULE_BYTES
        );
        let service =
            fs::read_to_string(stage.join("etc/xdg/systemd/user/toughfix-camera.service")).unwrap();
        assert!(service.contains("--background --hotplug"));
        assert!(!service.contains("--project"));
        assert!(!service.contains("[Install]"));
    }
    #[test]
    fn native_launcher_quotes_paths_using_real_gio_parser_without_python() {
        let temp = Temp::new();
        let binary = temp.path().join("app \"$cash`tick`\\percent%");
        let config = temp.path().join("config \"$cash`tick`\\percent%");
        let output = temp.path().join("argv");
        files::atomic_mode(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                output.display()
            )
            .as_bytes(),
            Some(0o755),
        )
        .unwrap();
        let desktop = temp.path().join("test.desktop");
        files::atomic(&desktop, &launcher(&binary, &config).unwrap()).unwrap();
        // gio-rs 0.22 moved DesktopAppInfo to gio-unix; use the already linked
        // GIO constructor for this parser regression without a new runtime dependency.
        unsafe extern "C" {
            fn g_desktop_app_info_new_from_filename(
                filename: *const libc::c_char,
            ) -> *mut gio::ffi::GAppInfo;
        }
        let filename = std::ffi::CString::new(desktop.to_str().unwrap()).unwrap();
        // SAFETY: live NUL-terminated filename; constructor returns a full GIO ref.
        let pointer = unsafe { g_desktop_app_info_new_from_filename(filename.as_ptr()) };
        assert!(!pointer.is_null());
        // SAFETY: GDesktopAppInfo implements GAppInfo; transfer ownership to Rust.
        let app: gio::AppInfo = unsafe { gtk::glib::translate::from_glib_full(pointer) };
        app.launch(&[], None::<&gio::AppLaunchContext>).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !output.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(output).unwrap(),
            format!("--config-dir\n{}\n", config.display())
        );
        assert!(launcher(Path::new("/invalid=exec"), &config).is_err());
        assert!(launcher(&binary, Path::new("/bad\npath")).is_err());
        assert!(staged(Path::new("relative"), None).is_err());
    }
    #[test]
    fn native_service_with_special_paths_passes_systemd_parser() {
        let temp = Temp::new();
        let binary = temp.path().join("app \"$cash`tick`\\percent%");
        let config = temp.path().join("config \"$cash`tick`\\percent%");
        files::atomic_mode(&binary, b"#!/bin/sh\nexit 0\n", Some(0o755)).unwrap();
        let path = temp.path().join("toughfix-camera.service");
        files::atomic(&path, &service(&binary, &config).unwrap()).unwrap();
        let result = Command::new("systemd-analyze")
            .args(["--user", "verify"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
