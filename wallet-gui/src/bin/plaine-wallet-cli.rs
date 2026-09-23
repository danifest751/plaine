#![forbid(unsafe_code)]

//! plaine-wallet's command line, built with argon2id installed, so it can seal
//! (`--kdf argon2id`) and open the key files the desktop wallet writes. Every
//! command and flag is plaine-wallet's own.

use plaine_wallet::ui::Streams;

fn main() {
    plaine_wallet_gui::kdf::install();
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stderr = std::io::stderr();
    let mut err = stderr.lock();

    let code = {
        let mut s = Streams::new(&mut input, &mut out, &mut err);
        plaine_wallet::wallet_cli::run(&argv, &mut s)
    };

    if code == 0
        && matches!(
            argv.first().map(String::as_str),
            Some("version" | "--version")
        )
    {
        use std::io::Write;
        let _ = writeln!(out, "{}", env!("PLAINE_BUILD_LINE"));
    }

    use std::io::Write;
    let _ = out.flush();
    let _ = err.flush();
    std::process::exit(code);
}
