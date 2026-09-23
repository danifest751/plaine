#![forbid(unsafe_code)]
// A window, not a console program: no black console box beside it on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

use plaine_wallet_gui::app::WalletApp;
use plaine_wallet_gui::model;
use std::path::PathBuf;

struct App(WalletApp);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        // A panel paints the theme's background; drawn straight onto the root the
        // window keeps the dark clear colour under light-theme text.
        eframe::egui::CentralPanel::default().show(ui, |ui| self.0.show(ui));
    }
}

const USAGE: &str = "\
plaine-wallet-gui - the Plaine desktop wallet

  plaine-wallet-gui [--config <file>]

  --config <file>   settings file to use (created if missing). Default:
                    %APPDATA%\\Plaine\\wallet-gui.conf on Windows,
                    ~/.config/plaine/wallet-gui.conf elsewhere. A portable copy
                    can keep its settings beside the program this way.
";

/// The settings file: `--config <file>`, else the default beside the node's data.
fn settings_path() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => {
                path = Some(PathBuf::from(
                    args.next().ok_or("--config needs a file path")?,
                ))
            }
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(path.unwrap_or_else(|| model::config_dir().join("wallet-gui.conf")))
}

fn main() -> eframe::Result {
    let settings = match settings_path() {
        Ok(p) => p,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("plaine-wallet-gui: {e}");
            }
            eprint!("{USAGE}");
            std::process::exit(if e.is_empty() { 0 } else { 2 });
        }
    };
    plaine_wallet_gui::kdf::install();
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
