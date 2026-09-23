#![forbid(unsafe_code)]

use plaine_wallet_gui::app::WalletApp;
use plaine_wallet_gui::model;

struct App(WalletApp);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        // A panel paints the theme's background; drawn straight onto the root the
        // window keeps the dark clear colour under light-theme text.
        eframe::egui::CentralPanel::default().show(ui, |ui| self.0.show(ui));
    }
}

fn main() -> eframe::Result {
    plaine_wallet_gui::kdf::install();
    let settings = model::config_dir().join("wallet-gui.conf");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Plaine wallet")
            .with_inner_size([900.0, 760.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Plaine wallet",
        options,
        Box::new(move |_cc| Ok(Box::new(App(WalletApp::new(settings))))),
    )
}
