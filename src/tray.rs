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
            || self.state.camera_error.is_some()
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
            title: "ToughFix · Olympus TG-1".into(),
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
    let color = if state.phase.device_busy() {
        [249, 181, 71]
    } else if state.camera_error.is_some()
        || state.reconnect_required
        || state.phase == Phase::Failed
    {
        [243, 102, 110]
    } else {
        [65, 207, 165]
    };
    let mut pixels = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32i32 {
        for x in 0..32i32 {
            let dx = x - 16;
            let dy = y - 16;
            let circle = dx * dx + dy * dy < 225;
            let cross = (dx.abs() <= 1 && dy.abs() < 10) || (dy.abs() <= 1 && dx.abs() < 10);
            let lens = dx * dx + dy * dy < 30;
            let ring = (dx * dx + dy * dy > 105) && (dx * dx + dy * dy < 135);
            if circle && (cross || lens || ring) {
                pixels.extend([255, 15, 29, 34]);
            } else if circle {
                pixels.extend([255, color[0], color[1], color[2]]);
            } else {
                pixels.extend([0, 0, 0, 0]);
            }
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
    signature: String,
    retry_at: std::time::Instant,
    pub error: Option<String>,
}
impl Manager {
    pub fn new() -> Self {
        Self {
            handle: None,
            signature: String::new(),
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
            self.signature.clear();
            self.error = None;
            self.retry_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
            return;
        }
        if self.handle.as_ref().is_some_and(|h| h.is_closed()) {
            self.handle = None;
            self.signature.clear();
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
        let signature = format!(
            "{:?}:{}:{}:{}:{}",
            state.phase,
            state.reconnect_required,
            state.camera_error.is_some(),
            state.updating_sources,
            state.quit_pending
        );
        if signature != self.signature {
            if let Some(handle) = self.handle.as_ref() {
                handle.update(|t| t.state = state);
            }
            self.signature = signature;
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
    }
}
