#![forbid(unsafe_code)]

//! Spike: a minimal egui view that the headless test harness can drive.
//! Replaced by the real wallet in phase 3 of docs/ROADMAP.md.

pub struct Spike {
    pub count: u32,
}

impl Spike {
    pub fn ui(&mut self, ui: &mut eframe::egui::Ui) {
        ui.label(format!("count: {}", self.count));
        if ui.button("Increment").clicked() {
            self.count += 1;
        }
    }
}
