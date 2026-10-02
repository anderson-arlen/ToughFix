// SPDX-License-Identifier: MIT
use crate::{
    backend::Shared,
    model::{Action, Phase, State},
};
use ksni::{
    blocking::{Handle, TrayMethods},
    menu::StandardItem,
};
use std::sync::{Arc, Mutex, mpsc::Sender};

pub struct Tray {
    pub state: State,
    pub actions: Sender<Action>,
    pub ui_actions: Arc<Mutex<Vec<Action>>>,
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "toughfix".into()
    }
    fn title(&self) -> String {
        format!("ToughFix · {}", self.state.unplug_message())
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }
    fn activate(&mut self, _: i32, _: i32) {
        self.ui_actions.lock().unwrap().push(Action::Open);
    }
    fn status(&self) -> ksni::Status {
        if self.state.phase.device_busy()
            || self.state.storage_preparing
            || self.state.camera_error.is_some()
            || self.state.storage_error.is_some()
            || self.state.reconnect_required
        {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![icon(&self.state)]
    }
    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon_pixmap()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: format!(
                "ToughFix · {}",
                self.state
                    .device
                    .as_ref()
                    .map_or("Olympus Tough", |d| d.model.name())
            ),
            description: format!(
                "{}\n{}",
                self.state.unplug_message(),
                self.state.phase.label()
            ),
            icon_pixmap: self.icon_pixmap(),
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open ToughFix".into(),
                activate: Box::new(|t: &mut Self| t.ui_actions.lock().unwrap().push(Action::Open)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.state.unplug_message().into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Refresh satellite checks".into(),
                enabled: !self.state.updating_sources
                    && self.state.gps_supported()
                    && !self.state.phase.device_busy()
                    && !self.state.demo,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.actions.send(Action::Refresh);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if self.state.phase.device_busy() {
                    "Quit after camera operation finishes"
                } else {
                    "Quit"
                }
                .into(),
                activate: Box::new(|t: &mut Self| t.ui_actions.lock().unwrap().push(Action::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn icon(state: &State) -> ksni::Icon {
    let color = if state.phase.device_busy() || state.storage_preparing {
        [249, 181, 71]
    } else if state.camera_error.is_some()
        || state.storage_error.is_some()
        || state.reconnect_required
        || state.phase == Phase::Failed
    {
        [243, 102, 110]
    } else {
        [101, 213, 187] // Application icon's crosshair green (#65d5bb).
    };
    let mut pixels = Vec::with_capacity(32 * 32 * 4);
    // Match the application's ring and four rounded arms, with a transparent
    // center/background so the silhouette stays clear on any panel theme.
    // Supersampling smooths the small circle without requiring a GUI renderer.
    for y in 0..32 {
        for x in 0..32 {
            let mut covered = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let dx = (x as f64 + (sx as f64 + 0.5) / 4. - 16.).abs();
                    let dy = (y as f64 + (sy as f64 + 0.5) / 4. - 16.).abs();
                    let ring = (dx.hypot(dy) - 9.).abs() <= 1.;
                    let horizontal = (dx - dx.clamp(5., 12.)).hypot(dy) <= 1.;
                    let vertical = dx.hypot(dy - dy.clamp(5., 12.)) <= 1.;
                    if ring || horizontal || vertical {
                        covered += 1;
                    }
                }
            }
            let alpha = (covered * 255 / 16) as u8;
            pixels.extend(if alpha == 0 {
                [0, 0, 0, 0]
            } else {
                [alpha, color[0], color[1], color[2]]
            });
        }
    }
    ksni::Icon {
        width: 32,
        height: 32,
        data: pixels,
    }
}

pub struct Manager {
    handle: Option<Handle<Tray>>,
    signature: Option<UpdateKey>,
    retry_at: std::time::Instant,
    pub error: Option<String>,
}

/// Every state input used by the tray's icon, title, tooltip or menu must
/// invalidate its cached snapshot, even when the upload phase stays Idle.
#[derive(Debug, PartialEq)]
struct UpdateKey {
    camera_model: Option<&'static str>,
    phase: Phase,
    storage_preparing: bool,
    storage_error: bool,
    camera_error: bool,
    reconnect_required: bool,
    storage_mounted: bool,
    updating_sources: bool,
    quit_pending: bool,
    demo: bool,
}
impl From<&State> for UpdateKey {
    fn from(state: &State) -> Self {
        Self {
            camera_model: state.device.as_ref().map(|d| d.model.name()),
            phase: state.phase.clone(),
            storage_preparing: state.storage_preparing,
            storage_error: state.storage_error.is_some(),
            camera_error: state.camera_error.is_some(),
            reconnect_required: state.reconnect_required,
            storage_mounted: state.storage_mounted(),
            updating_sources: state.updating_sources,
            quit_pending: state.quit_pending,
            demo: state.demo,
        }
    }
}
impl Manager {
    pub fn new() -> Self {
        Self {
            handle: None,
            signature: None,
            retry_at: std::time::Instant::now() - std::time::Duration::from_secs(10),
            error: None,
        }
    }
    pub fn sync(
        &mut self,
        shared: &Shared,
        actions: &Sender<Action>,
        ui_actions: &Arc<Mutex<Vec<Action>>>,
    ) {
        let state = shared.lock().unwrap().clone();
        if state.device.is_none() {
            if let Some(h) = self.handle.take() {
                h.shutdown().wait();
            }
            self.signature = None;
            self.error = None;
            self.retry_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
            return;
        }
        if self.handle.as_ref().is_some_and(|h| h.is_closed()) {
            self.handle = None;
            self.signature = None;
        }
        if self.handle.is_none() && self.retry_at.elapsed() >= std::time::Duration::from_secs(10) {
            match (Tray {
                state: state.clone(),
                actions: actions.clone(),
                ui_actions: ui_actions.clone(),
            })
            .spawn()
            {
                Ok(handle) => {
                    self.handle = Some(handle);
                    self.error = None;
                }
                Err(e) => {
                    self.error = Some(format!(
                        "System tray unavailable: {e}. Use the app window to manage updates."
                    ))
                }
            }
            self.retry_at = std::time::Instant::now();
        }
        let signature = UpdateKey::from(&state);
        if self.signature.as_ref() != Some(&signature)
            && let Some(handle) = self.handle.as_ref()
        {
            handle.update(|t| t.state = state);
            self.signature = Some(signature);
        }
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn busy_and_idle_icons_are_distinct_and_well_formed() {
        let idle = icon(&State::default());
        let busy = icon(&State {
            phase: Phase::Committing,
            ..Default::default()
        });
        assert_eq!(idle.data.len(), 32 * 32 * 4);
        assert_ne!(idle.data, busy.data);
        let preparing = icon(&State {
            storage_preparing: true,
            ..Default::default()
        });
        assert_eq!(preparing.data, busy.data);
    }
}
