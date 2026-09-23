//! The screens. Everything shown comes from `model`; everything done with the key
//! goes through `plaine_wallet::api`. The only secret held here is the open key,
//! and locking drops it.

use crate::model::{
    self, plan_send, rows, sent_log_for, Cmd, FeeLevel, FormErrors, HistoryState, NodeView,
    SendForm, SendPlan, SentRecord, Settings, Worker,
};
use crate::rpc::Transport;
use eframe::egui::{self, Color32, RichText};
use plaine_consensus::constants::Network;
use plaine_wallet::api::{self, Kdf, KeySummary, OpenKey};
use plaine_wallet::keyfile::Role;
use plaine_wallet::secret::SecretBytes;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Makes the node connection for the current settings; tests pass a mock.
pub type Connect = Arc<dyn Fn(&Settings) -> Box<dyn Transport> + Send + Sync>;

/// A copied backup string is wiped from the clipboard after this long.
pub const CLIPBOARD_SECRET_FOR: Duration = Duration::from_secs(60);

const WARN: Color32 = Color32::from_rgb(0xd0, 0x80, 0x00);
const BAD: Color32 = Color32::from_rgb(0xd0, 0x30, 0x30);
const GOOD: Color32 = Color32::from_rgb(0x30, 0xa0, 0x50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartMode {
    Open,
    Create,
    Import,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Home,
    Send,
    History,
    Settings,
}

#[derive(Default)]
struct StartForm {
    path: String,
    pass: String,
    pass2: String,
    backup: String,
    error: Option<String>,
}

struct UnlockForm {
    path: PathBuf,
    summary: KeySummary,
    pass: String,
    error: Option<String>,
}

#[derive(Default)]
struct ProtectForm {
    new_path: String,
    pass: String,
    pass2: String,
    outcome: Option<Result<String, String>>,
}

#[derive(Default)]
struct RevealForm {
    pass: String,
    shown: Option<String>,
    error: Option<String>,
}

struct Session {
    path: PathBuf,
    key: OpenKey,
    summary: KeySummary,
    worker: Worker,
    tab: Tab,
    send: SendForm,
    send_errors: FormErrors,
    confirm: Option<SendPlan>,
    protect: ProtectForm,
    reveal: RevealForm,
    /// Shown once after a key is created: the backup string, until acknowledged.
    fresh_backup: Option<String>,
}

enum Screen {
    Start { mode: StartMode, form: StartForm },
    Unlock(UnlockForm),
    Open(Box<Session>),
}

pub struct WalletApp {
    settings: Settings,
    settings_path: Option<PathBuf>,
    connect: Connect,
    /// Tests run node calls inline; the desktop wallet on a thread.
    inline: bool,
    screen: Screen,
    last_activity: Instant,
    clear_clipboard_at: Option<Instant>,
    settings_note: Option<String>,
}

impl WalletApp {
    /// The desktop wallet: settings from `settings_path`, a real node over HTTP.
    pub fn new(settings_path: PathBuf) -> WalletApp {
        let settings = Settings::load(&settings_path);
        let connect: Connect =
            Arc::new(|s: &Settings| Box::new(s.transport()) as Box<dyn Transport>);
        WalletApp::build(settings, Some(settings_path), connect, false)
    }

    /// For tests: no settings file, node calls answered inline.
    pub fn for_tests(settings: Settings, connect: Connect) -> WalletApp {
        WalletApp::build(settings, None, connect, true)
    }

    fn build(
        settings: Settings,
        settings_path: Option<PathBuf>,
        connect: Connect,
        inline: bool,
    ) -> WalletApp {
        let path = if settings.key_file.is_empty() {
            model::config_dir()
                .join("wallet.plnekey")
                .display()
                .to_string()
        } else {
            settings.key_file.clone()
        };
        let form = StartForm {
            path,
            ..StartForm::default()
        };
        WalletApp {
            settings,
            settings_path,
            connect,
            inline,
            screen: Screen::Start {
                mode: StartMode::Open,
                form,
            },
            last_activity: Instant::now(),
            clear_clipboard_at: None,
            settings_note: None,
        }
    }

    /// Whether a key is open (for tests and the window title).
    pub fn is_open(&self) -> bool {
        matches!(self.screen, Screen::Open(_))
    }

    /// Drops the open key and returns to the passphrase prompt, or to the start
    /// screen for a key file that has none.
    pub fn lock(&mut self) {
        let Screen::Open(s) = std::mem::replace(
            &mut self.screen,
            Screen::Start {
                mode: StartMode::Open,
                form: StartForm::default(),
            },
        ) else {
            return;
        };
        let path = s.path.clone();
        let summary = s.summary.clone();
        drop(s);
        if summary.encrypted {
            self.screen = Screen::Unlock(UnlockForm {
                path,
                summary,
                pass: String::new(),
                error: None,
            });
        } else {
            let form = StartForm {
                path: path.display().to_string(),
                ..StartForm::default()
            };
            self.screen = Screen::Start {
                mode: StartMode::Open,
                form,
            };
        }
    }

    fn worker_for(&self, ctx: &egui::Context, address: &str, sent_log: PathBuf) -> Worker {
        let node = (self.connect)(&self.settings);
        if self.inline {
            Worker::inline(node, address.to_string(), Some(sent_log))
        } else {
            let ctx = ctx.clone();
            Worker::spawn(node, address.to_string(), Some(sent_log), move || {
                ctx.request_repaint()
            })
        }
    }

    fn open_session(
        &mut self,
        ctx: &egui::Context,
        path: PathBuf,
        key: OpenKey,
        fresh_backup: Option<String>,
    ) {
        let summary = key.summary();
        let worker = self.worker_for(ctx, &summary.address, sent_log_for(&path));
        self.settings.key_file = path.display().to_string();
        self.save_settings();
        self.screen = Screen::Open(Box::new(Session {
            path,
            key,
            summary,
            worker,
            tab: Tab::Home,
            send: SendForm::default(),
            send_errors: FormErrors::default(),
            confirm: None,
            protect: ProtectForm::default(),
            reveal: RevealForm::default(),
            fresh_backup,
        }));
    }

    fn save_settings(&mut self) {
        if let Some(p) = &self.settings_path {
            if let Err(e) = self.settings.save(p) {
                self.settings_note = Some(format!("settings not saved: {e}"));
            }
        }
    }

    /// Draws the wallet. The desktop app and the tests both call this.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if ui.input(|i| !i.events.is_empty() || i.pointer.is_moving()) {
            self.last_activity = Instant::now();
        }
        if let Some(at) = self.clear_clipboard_at {
            if Instant::now() >= at {
                ctx.copy_text(String::new());
                self.clear_clipboard_at = None;
            } else {
                ctx.request_repaint_after(at - Instant::now());
            }
        }
        if self.is_open() && self.settings.lock_after_minutes > 0 {
            let limit = Duration::from_secs(self.settings.lock_after_minutes * 60);
            if self.last_activity.elapsed() >= limit {
                self.lock();
            } else {
                ctx.request_repaint_after(limit - self.last_activity.elapsed());
            }
        }

        match &mut self.screen {
            Screen::Start { .. } => self.start_screen(ui, &ctx),
            Screen::Unlock(_) => self.unlock_screen(ui, &ctx),
            Screen::Open(_) => self.wallet_screen(ui),
        }
    }

    fn start_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Screen::Start { mode, form } = &mut self.screen else {
            return;
        };
        ui.heading("Plaine wallet");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.selectable_value(mode, StartMode::Open, "Open a key file");
            ui.selectable_value(mode, StartMode::Create, "Create a new key");
            ui.selectable_value(mode, StartMode::Import, "Restore from backup");
        });
        ui.separator();
        field(ui, "Key file", &mut form.path, false);
        let mode = *mode;
        match mode {
            StartMode::Open => {}
            StartMode::Create => {
                field(ui, "Passphrase", &mut form.pass, true);
                field(ui, "Repeat passphrase", &mut form.pass2, true);
                ui.label(
                    "The key is sealed with argon2id (64 MiB per guess). Use a long passphrase \
                     and keep it apart from the backup.",
                );
                if ui.button("Suggest a passphrase").clicked() {
                    if let Ok(p) = suggest_passphrase() {
                        form.pass = p.clone();
                        form.pass2 = p;
                    }
                }
            }
            StartMode::Import => {
                field(ui, "Backup string (68 characters)", &mut form.backup, true);
                field(ui, "Passphrase", &mut form.pass, true);
                field(ui, "Repeat passphrase", &mut form.pass2, true);
            }
        }
        let go = ui
            .button(match mode {
                StartMode::Open => "Open",
                StartMode::Create => "Create",
                StartMode::Import => "Restore",
            })
            .clicked();
        if let Some(e) = &form.error {
            ui.colored_label(BAD, e);
        }
        if !go {
            return;
        }

        let path = PathBuf::from(form.path.trim());
        match mode {
            StartMode::Open => match api::inspect(&path) {
                Ok(summary) if summary.encrypted => {
                    self.screen = Screen::Unlock(UnlockForm {
                        path,
                        summary,
                        pass: String::new(),
                        error: None,
                    });
                }
                Ok(_) => match api::open(&path, None) {
                    Ok(key) => self.open_session(ctx, path, key, None),
                    Err(e) => form.error = Some(e.to_string()),
                },
                Err(e) => form.error = Some(e.to_string()),
            },
            StartMode::Create | StartMode::Import => {
                if let Some(e) = passphrase_problem(&form.pass, &form.pass2) {
                    form.error = Some(e);
                    return;
                }
                let seed = if mode == StartMode::Import {
                    match api::decode_seed_text(&form.backup, true) {
                        Ok((s, _)) => Some(s),
                        Err(e) => {
                            form.error = Some(e.to_string());
                            return;
                        }
                    }
                } else {
                    None
                };
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let pass = secret(&form.pass);
                let created =
                    api::create_key(&path, Role::Spend, seed, Some(&pass), Kdf::RECOMMENDED)
                        .and_then(|_| api::open(&path, Some(&pass)));
                form.pass.clear();
                form.pass2.clear();
                form.backup.clear();
                match created {
                    Ok(key) => {
                        let backup = (mode == StartMode::Create).then(|| key.backup_string());
                        self.open_session(ctx, path, key, backup);
                    }
                    Err(e) => form.error = Some(e.to_string()),
                }
            }
        }
    }

    fn unlock_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Screen::Unlock(form) = &mut self.screen else {
            return;
        };
        ui.heading("Unlock");
        ui.label(format!(
            "{}  ({})",
            form.summary.address,
            form.path.display()
        ));
        let r = field(ui, "Passphrase", &mut form.pass, true);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let unlock = ui.button("Unlock").clicked() || enter;
        let other = ui.button("Use another key file").clicked();
        if let Some(e) = &form.error {
            ui.colored_label(BAD, e);
        }
        if other {
            let form = StartForm {
                path: form.path.display().to_string(),
                ..StartForm::default()
            };
            self.screen = Screen::Start {
                mode: StartMode::Open,
                form,
            };
            return;
        }
        if unlock {
            let pass = secret(&form.pass);
            form.pass.clear();
            match api::open(&form.path, Some(&pass)) {
                Ok(key) => {
                    let path = form.path.clone();
                    self.open_session(ctx, path, key, None);
                }
                Err(e) => form.error = Some(e.to_string()),
            }
        }
    }

    fn wallet_screen(&mut self, ui: &mut egui::Ui) {
        let mut lock = false;
        let mut save = false;
        let mut copy_secret: Option<String> = None;
        {
            let Screen::Open(s) = &mut self.screen else {
                return;
            };
            let view = s.worker.view();

            ui.horizontal(|ui| {
                ui.heading("Plaine wallet");
                ui.selectable_value(&mut s.tab, Tab::Home, "Home");
                ui.selectable_value(&mut s.tab, Tab::Send, "Send");
                ui.selectable_value(&mut s.tab, Tab::History, "History");
                ui.selectable_value(&mut s.tab, Tab::Settings, "Settings");
                if ui.button("Lock").clicked() {
                    lock = true;
                }
            });
            node_line(ui, &view);
            ui.separator();

            if let Some(b) = &s.fresh_backup {
                ui.colored_label(
                    WARN,
                    "Write down this backup string now. It is the key itself.",
                );
                ui.monospace(b);
                ui.label("It is shown this once. Anyone who has it can spend from this wallet.");
                if ui.button("I have written it down").clicked() {
                    s.fresh_backup = None;
                }
                return;
            }

            match s.tab {
                Tab::Home => home_tab(ui, s, &view),
                Tab::Send => send_tab(ui, s, &view),
                Tab::History => history_tab(ui, s, &view),
                Tab::Settings => {
                    let (sv, cp) = settings_tab(ui, s, &mut self.settings, &self.settings_note);
                    save = sv;
                    copy_secret = cp;
                }
            }
        }
        if let Some(text) = copy_secret {
            ui.ctx().copy_text(text);
            self.clear_clipboard_at = Some(Instant::now() + CLIPBOARD_SECRET_FOR);
        }
        if save {
            self.save_settings();
            let ctx = ui.ctx().clone();
            if let Screen::Open(s) = &self.screen {
                let address = s.summary.address.clone();
                let log = sent_log_for(&s.path);
                let worker = self.worker_for(&ctx, &address, log);
                if let Screen::Open(s) = &mut self.screen {
                    s.worker = worker;
                }
            }
        }
        if lock {
            self.lock();
        }
    }
}

fn home_tab(ui: &mut egui::Ui, s: &mut Session, view: &NodeView) {
    ui.label("Your address");
    ui.horizontal(|ui| {
        ui.monospace(&s.summary.address);
        if ui.button("Copy address").clicked() {
            ui.ctx().copy_text(s.summary.address.clone());
        }
    });
    ui.add_space(8.0);
    match &view.account {
        Some(Ok(a)) => {
            egui::Grid::new("balance").num_columns(2).show(ui, |ui| {
                ui.label("Spendable");
                ui.label(RichText::new(format!("{} PLNE", api::format_plne(a.spendable))).strong());
                ui.end_row();
                ui.label("Maturing");
                ui.label(format!("{} PLNE", api::format_plne(a.immature)));
                ui.end_row();
                ui.label("Total");
                ui.label(format!("{} PLNE", api::format_plne(a.balance)));
                ui.end_row();
                ui.label("Pending sends");
                ui.label(format!("{}", view.pending.len()));
                ui.end_row();
            });
        }
        Some(Err(e)) => {
            ui.colored_label(BAD, e);
        }
        None => {
            ui.label("Balance: waiting for the node");
        }
    }
    if !s.summary.encrypted {
        ui.add_space(8.0);
        ui.colored_label(
            WARN,
            "This key file is not encrypted: anyone who reads it can spend. Settings can write \
             a protected copy.",
        );
    }
}

fn send_tab(ui: &mut egui::Ui, s: &mut Session, view: &NodeView) {
    if let Some(plan) = s.confirm.clone() {
        ui.heading("Confirm");
        egui::Grid::new("confirm").num_columns(2).show(ui, |ui| {
            ui.label("To");
            ui.monospace(&plan.to);
            ui.end_row();
            ui.label("Amount");
            ui.label(format!("{} PLNE", api::format_plne(plan.amount)));
            ui.end_row();
            ui.label("Fee");
            ui.label(format!("{} PLNE", api::format_plne(plan.fee)));
            ui.end_row();
            ui.label("Total");
            ui.label(RichText::new(format!("{} PLNE", api::format_plne(plan.total()))).strong());
            ui.end_row();
            ui.label("Nonce");
            ui.label(plan.nonce.to_string());
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Sign and send").clicked() {
                match s.key.sign_transfer(
                    Network::Main,
                    &plan.to,
                    plan.amount,
                    plan.fee,
                    plan.nonce,
                ) {
                    Ok(tx) => {
                        let record = SentRecord {
                            txid: plaine_consensus::hex::encode(&tx.txid),
                            nonce: tx.nonce,
                            amount: tx.amount,
                            fee: tx.fee,
                            to: tx.to.clone(),
                            time: now_secs(),
                        };
                        s.worker.send(Cmd::Submit {
                            hex: tx.hex,
                            record,
                        });
                        s.send = SendForm::default();
                    }
                    Err(e) => s.send_errors.other = Some(e.to_string()),
                }
                s.confirm = None;
            }
            if ui.button("Back").clicked() {
                s.confirm = None;
            }
        });
        return;
    }

    field(ui, "Recipient address", &mut s.send.to, false);
    if let Some(e) = &s.send_errors.to {
        ui.colored_label(BAD, e);
    }
    field(ui, "Amount (PLNE)", &mut s.send.amount, false);
    if let Some(e) = &s.send_errors.amount {
        ui.colored_label(BAD, e);
    }
    ui.horizontal(|ui| {
        ui.label("Fee");
        let f = view.fees;
        for (level, name) in [
            (FeeLevel::Low, "Low"),
            (FeeLevel::Normal, "Normal"),
            (FeeLevel::High, "High"),
        ] {
            let text = match f {
                Some(f) => format!("{name} ({} PLNE)", api::format_plne(level.pick(&f))),
                None => name.to_string(),
            };
            ui.radio_value(&mut s.send.level, level, text);
        }
    });
    if ui.button("Review").clicked() {
        match plan_send(&s.send, view.account(), view.fees.as_ref()) {
            Ok(plan) => {
                s.send_errors = FormErrors::default();
                s.confirm = Some(plan);
            }
            Err(e) => s.send_errors = e,
        }
    }
    if let Some(e) = &s.send_errors.other {
        ui.colored_label(BAD, e);
    }
    match &view.last_send {
        Some(Ok(txid)) => {
            ui.colored_label(GOOD, format!("Sent: {txid}"));
        }
        Some(Err(e)) => {
            ui.colored_label(BAD, format!("Not sent: {e}"));
        }
        None => {}
    }
}

fn history_tab(ui: &mut egui::Ui, s: &mut Session, view: &NodeView) {
    let sent = model::read_sent(&sent_log_for(&s.path));
    match &view.history {
        HistoryState::Unsupported(why) => {
            ui.colored_label(
                WARN,
                "This node keeps no address history, so only transfers sent from this wallet \
                 are listed. Incoming transfers need a node with `addrindex = true`.",
            );
            ui.small(why);
        }
        HistoryState::Listed { hint: Some(h), .. } => {
            ui.small(h);
        }
        _ => {}
    }
    if let Some(e) = &view.history_error {
        ui.colored_label(BAD, e);
    }
    let rows = rows(view, &sent);
    if rows.is_empty() {
        ui.label("No transactions yet.");
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("history")
            .striped(true)
            .num_columns(5)
            .show(ui, |ui| {
                if !rows.is_empty() {
                    for h in [
                        "Status",
                        "Time (UTC)",
                        "Type",
                        "Amount, PLNE",
                        "Counterparty",
                    ] {
                        ui.label(RichText::new(h).underline());
                    }
                    ui.end_row();
                }
                for r in &rows {
                    ui.label(&r.status);
                    ui.label(&r.when);
                    ui.label(format!("{} {}", r.kind, r.direction));
                    ui.label(&r.amount);
                    ui.monospace(if r.counterparty.is_empty() {
                        short_id(&r.txid)
                    } else {
                        r.counterparty.clone()
                    });
                    ui.end_row();
                }
            });
        if let HistoryState::Listed {
            next_cursor: Some(_),
            ..
        } = &view.history
        {
            if ui.button("Load more").clicked() {
                s.worker.send(Cmd::MoreHistory);
            }
        }
    });
}

/// Returns (settings changed, a secret to copy with a timed wipe).
fn settings_tab(
    ui: &mut egui::Ui,
    s: &mut Session,
    settings: &mut Settings,
    note: &Option<String>,
) -> (bool, Option<String>) {
    let mut save = false;
    let mut copy = None;
    ui.heading("Node");
    field(ui, "Node RPC address", &mut settings.node, false);
    field(
        ui,
        "RPC token (if the node needs one)",
        &mut settings.token,
        true,
    );
    ui.horizontal(|ui| {
        ui.label("Lock after (minutes, 0 = never)");
        let mut m = settings.lock_after_minutes.to_string();
        if ui.text_edit_singleline(&mut m).changed() {
            if let Ok(n) = m.trim().parse() {
                settings.lock_after_minutes = n;
            }
        }
    });
    if ui.button("Save and reconnect").clicked() {
        save = true;
    }
    if let Some(n) = note {
        ui.colored_label(WARN, n);
    }

    ui.separator();
    ui.heading("Key file");
    ui.label(format!("{}  kdf {}", s.path.display(), s.summary.kdf));
    ui.label(if s.summary.encrypted {
        "Write a copy under a new passphrase (argon2id). The current file is left as it is."
    } else {
        "Write a protected copy (argon2id). The current file stays, unencrypted: delete it \
         yourself once the copy opens and the backup is safe."
    });
    if s.protect.new_path.is_empty() {
        s.protect.new_path = protected_path(&s.path).display().to_string();
    }
    field(ui, "New key file", &mut s.protect.new_path, false);
    field(ui, "New passphrase", &mut s.protect.pass, true);
    field(ui, "Repeat new passphrase", &mut s.protect.pass2, true);
    if ui.button("Write protected copy").clicked() {
        s.protect.outcome = Some(
            match passphrase_problem(&s.protect.pass, &s.protect.pass2) {
                Some(e) => Err(e),
                None => {
                    let out = PathBuf::from(s.protect.new_path.trim());
                    let pass = secret(&s.protect.pass);
                    match s.key.rewrap(&out, Some(&pass), Kdf::RECOMMENDED) {
                        Ok(_) => {
                            settings.key_file = out.display().to_string();
                            save = true;
                            Ok(format!(
                                "Written: {}. It opens with the new passphrase from now on.",
                                out.display()
                            ))
                        }
                        Err(e) => Err(e.to_string()),
                    }
                }
            },
        );
        s.protect.pass.clear();
        s.protect.pass2.clear();
    }
    match &s.protect.outcome {
        Some(Ok(m)) => {
            ui.colored_label(GOOD, m);
        }
        Some(Err(e)) => {
            ui.colored_label(BAD, e);
        }
        None => {}
    }

    ui.separator();
    ui.heading("Backup");
    ui.label("The backup string restores this wallet anywhere. Showing it needs the passphrase.");
    if let Some(b) = s.reveal.shown.clone() {
        ui.monospace(&b);
        ui.horizontal(|ui| {
            if ui
                .button("Copy backup (wiped from the clipboard in 60 s)")
                .clicked()
            {
                copy = Some(b.clone());
            }
            if ui.button("Hide").clicked() {
                s.reveal = RevealForm::default();
            }
        });
    } else {
        if s.summary.encrypted {
            field(
                ui,
                "Passphrase to show the backup",
                &mut s.reveal.pass,
                true,
            );
        }
        if ui.button("Show backup").clicked() {
            let ok = if s.summary.encrypted {
                let pass = secret(&s.reveal.pass);
                s.reveal.pass.clear();
                api::open(&s.path, Some(&pass)).map(|_| ())
            } else {
                Ok(())
            };
            match ok {
                Ok(()) => s.reveal.shown = Some(s.key.backup_string()),
                Err(e) => s.reveal.error = Some(e.to_string()),
            }
        }
        if let Some(e) = &s.reveal.error {
            ui.colored_label(BAD, e);
        }
    }
    (save, copy)
}

fn node_line(ui: &mut egui::Ui, view: &NodeView) {
    match &view.chain {
        Some(Ok(c)) => {
            let colour = if c.sync == "synced" { GOOD } else { WARN };
            ui.colored_label(
                colour,
                format!(
                    "Node: {} at height {}, {} peer(s)",
                    c.sync, c.height, c.peers
                ),
            );
        }
        Some(Err(e)) => {
            ui.colored_label(BAD, format!("Node: {e}"));
        }
        None => {
            ui.label("Node: connecting");
        }
    }
}

/// A labelled single-line field; the label names it for screen readers and tests.
fn field(ui: &mut egui::Ui, label: &str, value: &mut String, secret: bool) -> egui::Response {
    ui.horizontal(|ui| {
        let l = ui.label(label);
        ui.add(
            egui::TextEdit::singleline(value)
                .password(secret)
                .desired_width(420.0),
        )
        .labelled_by(l.id)
    })
    .inner
}

fn secret(s: &str) -> SecretBytes {
    SecretBytes::from_vec(s.as_bytes().to_vec())
}

/// Why a new passphrase will not do, or `None`.
pub fn passphrase_problem(pass: &str, repeat: &str) -> Option<String> {
    if pass.chars().count() < 12 {
        return Some("use a passphrase of at least 12 characters".into());
    }
    if pass != repeat {
        return Some("the two passphrases differ".into());
    }
    None
}

/// Twenty random characters from an alphabet without look-alikes: about 100 bits.
pub fn suggest_passphrase() -> Result<String, String> {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    // Bytes at or above the largest multiple of the alphabet size are dropped, so
    // every character is equally likely.
    let limit = 256 - 256 % ALPHABET.len();
    let mut chars = Vec::with_capacity(20);
    while chars.len() < 20 {
        let seed = plaine_wallet::rng::generate_seed().map_err(|e| e.to_string())?;
        for b in seed.expose() {
            if (*b as usize) < limit && chars.len() < 20 {
                chars.push(ALPHABET[*b as usize % ALPHABET.len()] as char);
            }
        }
    }
    let mut out = String::new();
    for (i, c) in chars.into_iter().enumerate() {
        if i > 0 && i % 5 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    Ok(out)
}

fn protected_path(p: &std::path::Path) -> PathBuf {
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "wallet".into());
    p.with_file_name(format!("{stem}-protected.plnekey"))
}

fn short_id(txid: &str) -> String {
    if txid.len() > 16 {
        format!("{}...{}", &txid[..8], &txid[txid.len() - 8..])
    } else {
        txid.to_string()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
