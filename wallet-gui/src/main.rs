#![forbid(unsafe_code)]

use plaine_wallet_gui::Spike;

struct App(Spike);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.0.ui(ui);
    }
}

fn main() -> eframe::Result {
    plaine_wallet_gui::kdf::install();
    eframe::run_native(
        "Plaine wallet",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(App(Spike { count: 0 })))),
    )
}
