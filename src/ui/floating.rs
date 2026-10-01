// SPDX-License-Identifier: MIT
//! GTK/Wayland cannot choose tiling policy. Ask Hyprland to float only our own
//! newly mapped window, without changing user configuration or later resizes.
use gtk::prelude::*;
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub fn compact_default(window: &gtk::ApplicationWindow, width: i32, height: i32) {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    let requested = std::rc::Rc::new(std::cell::Cell::new(false));
    window.connect_map(move |window| {
        if requested.replace(true) {
            return;
        }
        let title = window.title().unwrap_or_default().to_string();
        std::thread::spawn(move || {
            for _ in 0..12 {
                let Some(clients) = command(&["-j", "clients"]) else {
                    return;
                };
                let Some(address) = own_window(&clients, std::process::id(), &title) else {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                let selector = format!("address:{address}");
                let float =
                    format!("hl.dsp.window.float({{window=\"{selector}\",action=\"enable\"}})");
                // Hyprland's Lua dispatchers replaced the earlier CLI syntax.
                if command(&["dispatch", &float]).is_some() {
                    let resize = format!(
                        "hl.dsp.window.resize({{window=\"{selector}\",x={width},y={height}}})"
                    );
                    let _ = command(&["dispatch", &resize]);
                } else if command(&["dispatch", "setfloating", &selector]).is_some() {
                    let _ = command(&[
                        "dispatch",
                        "resizewindowpixel",
                        &format!("exact {width} {height},{selector}"),
                    ]);
                }
                return;
            }
        });
    });
}

fn own_window(json: &str, pid: u32, title: &str) -> Option<String> {
    let clients: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    clients.iter().find_map(|c| {
        if c["pid"].as_u64()? != u64::from(pid) || c["title"].as_str()? != title {
            return None;
        }
        let address = c["address"].as_str()?;
        let hex = address.strip_prefix("0x")?;
        (!hex.is_empty() && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
            .then(|| address.to_owned())
    })
}

/// Bound local compositor IPC and discard errors: other desktops retain their
/// normal window placement. No shell, global rules, or active-window selectors.
fn command(args: &[&str]) -> Option<String> {
    let mut child = Command::new("hyprctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placement_matches_only_our_pid_title_and_a_valid_address() {
        let clients = r#"[{"pid":4,"title":"ToughFix","address":"0xabc"},{"pid":5,"title":"ToughFix — Details","address":"0xdef"},{"pid":5,"title":"ToughFix","address":"0x123"}]"#;
        assert_eq!(own_window(clients, 5, "ToughFix"), Some("0x123".into()));
        assert_eq!(
            own_window(clients, 5, "ToughFix — Details"),
            Some("0xdef".into())
        );
        assert_eq!(own_window(clients, 6, "ToughFix"), None);
        assert_eq!(
            own_window(
                r#"[{"pid":5,"title":"ToughFix","address":"0x1\";bad"}]"#,
                5,
                "ToughFix"
            ),
            None
        );
    }
}
