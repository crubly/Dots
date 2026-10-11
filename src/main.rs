#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use directories::ProjectDirs;
use eframe::egui::{self, Color32, RichText, TextStyle};
use serde::{Deserialize, Serialize};
use ssh2::Session;

const ACCENT: Color32 = Color32::from_rgb(86, 156, 214);
const BG: Color32 = Color32::from_rgb(22, 25, 30);
const PANEL: Color32 = Color32::from_rgb(28, 32, 39);
const MUTED: Color32 = Color32::from_rgb(139, 148, 158);

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 620.0])
            .with_title("Dots")
            .with_decorations(true)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "Dots",
        options,
        Box::new(|cc| Ok(Box::new(DotsApp::new(cc)))),
    )
}

#[derive(Clone, Serialize, Deserialize)]
struct Host {
    id: u64,
    name: String,
    address: String,
    port: u16,
    user: String,
    key_path: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    auth_mode: AuthMode,
    #[serde(default)]
    jump_host: JumpHost,
    #[serde(skip)]
    password: String,
    #[serde(skip)]
    key_passphrase: String,
    #[serde(skip)]
    pasted_key: String,
    #[serde(default)]
    trusted_fingerprints: Vec<String>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
enum AuthMode {
    #[default]
    Key,
    Password,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct JumpHost {
    enabled: bool,
    protocol: String,
    address: String,
    port: u16,
    user: String,
    #[serde(skip)]
    password: String,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            id: 1,
            name: "New server".into(),
            address: "".into(),
            port: 22,
            user: "root".into(),
            key_path: "~/.ssh/id_ed25519".into(),
            folder: "Personal".into(),
            auth_mode: AuthMode::Key,
            jump_host: JumpHost {
                protocol: "SSH".into(),
                port: 22,
                ..Default::default()
            },
            password: String::new(),
            key_passphrase: String::new(),
            pasted_key: String::new(),
            trusted_fingerprints: vec![],
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SavedHosts {
    hosts: Vec<Host>,
}

#[derive(Clone)]
struct RemoteFile {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
}

#[derive(Clone)]
struct LocalFile {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
}

#[derive(Serialize, Deserialize)]
struct ClientSettings {
    dark_theme: bool,
    background_opacity: u8,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            dark_theme: true,
            background_opacity: 255,
        }
    }
}

enum WorkerCommand {
    TrustHost(bool),
    Run(String),
    List(PathBuf),
    Upload(PathBuf, PathBuf),
    Disconnect,
}
enum WorkerEvent {
    HostFingerprint(String),
    Connected,
    Output(String),
    Files(Vec<RemoteFile>),
    Error(String),
    Disconnected,
}

struct ActiveConnection {
    command_tx: Sender<WorkerCommand>,
    event_rx: Receiver<WorkerEvent>,
    started: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum WorkspaceTab {
    Home,
    Sftp,
    Terminal(u32),
}

impl WorkspaceTab {
    fn label(self) -> String {
        match self {
            Self::Home => "Home".into(),
            Self::Sftp => "SFTP".into(),
            Self::Terminal(number) => format!("Terminal {number}"),
        }
    }
}

struct DotsApp {
    hosts: SavedHosts,
    selected: Option<u64>,
    editing: Host,
    active: Option<ActiveConnection>,
    terminal: Vec<String>,
    command: String,
    files: Vec<RemoteFile>,
    remote_path: PathBuf,
    remote_path_text: String,
    local_path: PathBuf,
    local_path_text: String,
    local_files: Vec<LocalFile>,
    local_upload: String,
    remote_upload: String,
    status: String,
    show_settings: bool,
    show_client_settings: bool,
    client_settings: ClientSettings,
    pending_fingerprint: Option<String>,
    tabs: Vec<WorkspaceTab>,
    active_tab: usize,
    next_terminal: u32,
}

impl DotsApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_theme(&cc.egui_ctx);
        let hosts = load_hosts().unwrap_or_default();
        let editing = hosts.hosts.first().cloned().unwrap_or_default();
        let local_path = dirs_home().unwrap_or_else(|| PathBuf::from("."));
        let client_settings = load_client_settings().unwrap_or_default();
        let app = Self {
            hosts,
            selected: None,
            editing,
            active: None,
            terminal: vec![],
            command: String::new(),
            files: vec![],
            remote_path: PathBuf::from("."),
            remote_path_text: ".".into(),
            local_files: list_local_files(&local_path),
            local_path,
            local_path_text: dirs_home()
                .unwrap_or_else(|| PathBuf::from("."))
                .display()
                .to_string(),
            local_upload: String::new(),
            remote_upload: String::new(),
            status: "Ready".into(),
            show_settings: false,
            show_client_settings: false,
            client_settings,
            pending_fingerprint: None,
            tabs: vec![WorkspaceTab::Home, WorkspaceTab::Sftp],
            active_tab: 0,
            next_terminal: 1,
        };
        apply_client_theme(&cc.egui_ctx, &app.client_settings);
        app
    }

    fn select_host(&mut self, id: u64) {
        if let Some(host) = self.hosts.hosts.iter().find(|h| h.id == id) {
            self.editing = host.clone();
            self.selected = Some(id);
        }
    }

    fn save_edit(&mut self) {
        if self.editing.name.trim().is_empty() {
            self.status = "Server needs a name".into();
            return;
        }
        if let Some(host) = self
            .hosts
            .hosts
            .iter_mut()
            .find(|h| h.id == self.editing.id)
        {
            *host = self.editing.clone();
        } else {
            self.editing.id = next_id(&self.hosts);
            self.hosts.hosts.push(self.editing.clone());
            self.selected = Some(self.editing.id);
        }
        if let Err(error) = save_hosts(&self.hosts) {
            self.status = format!("Could not save: {error}");
        } else {
            self.status = "Saved locally".into();
        }
    }

    fn connect(&mut self) {
        if self.editing.address.trim().is_empty() {
            self.status = "Enter a host address first".into();
            return;
        }
        self.save_edit();
        let host = self.editing.clone();
        let (command_tx, command_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        self.status = format!("Connecting to {}…", host.address);
        self.terminal
            .push(format!("\nConnecting to {}@{}…", host.user, host.address));
        thread::spawn(move || worker(host, command_rx, event_tx));
        self.active = Some(ActiveConnection {
            command_tx,
            event_rx,
            started: Instant::now(),
        });
    }

    fn poll_connection(&mut self) {
        let mut events = Vec::new();
        if let Some(active) = &self.active {
            while let Ok(event) = active.event_rx.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                WorkerEvent::HostFingerprint(fingerprint) => {
                    self.pending_fingerprint = Some(fingerprint)
                }
                WorkerEvent::Connected => {
                    self.status = "Connected".into();
                    self.terminal
                        .push("Connected. Type a command below.".into());
                    self.list_remote();
                }
                WorkerEvent::Output(text) => self.terminal.push(text),
                WorkerEvent::Files(files) => self.files = files,
                WorkerEvent::Error(error) => {
                    self.status = format!("Error: {error}");
                    self.terminal.push(format!("Error: {error}"));
                }
                WorkerEvent::Disconnected => {
                    self.status = "Disconnected".into();
                    self.active = None;
                }
            }
        }
    }

    fn send_command(&mut self) {
        let command = self.command.trim().to_owned();
        if command.is_empty() {
            return;
        }
        if let Some(active) = &self.active {
            let _ = active.command_tx.send(WorkerCommand::Run(command.clone()));
            self.terminal.push(format!("$ {command}"));
            self.command.clear();
        } else {
            self.status = "Connect to a server first".into();
        }
    }

    fn list_remote(&mut self) {
        if let Some(active) = &self.active {
            let _ = active
                .command_tx
                .send(WorkerCommand::List(self.remote_path.clone()));
        }
    }
    fn disconnect(&mut self) {
        if let Some(active) = &self.active {
            let _ = active.command_tx.send(WorkerCommand::Disconnect);
        }
    }
    fn refresh_local(&mut self) {
        self.local_files = list_local_files(&self.local_path);
    }
    fn upload_file(&mut self) {
        let local = PathBuf::from(self.local_upload.trim());
        let remote = PathBuf::from(self.remote_upload.trim());
        if local.as_os_str().is_empty() || remote.as_os_str().is_empty() {
            self.status = "Enter both local and remote paths".into();
            return;
        }
        if let Some(active) = &self.active {
            let _ = active.command_tx.send(WorkerCommand::Upload(local, remote));
        } else {
            self.status = "Connect to a server first".into();
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("DOTS")
                    .strong()
                    .size(15.0)
                    .color(Color32::WHITE),
            );
            ui.separator();
            for (index, tab) in self.tabs.clone().into_iter().enumerate() {
                let selected = self.active_tab == index;
                if ui
                    .add(
                        egui::Button::new(tab.label())
                            .selected(selected)
                            .frame(false),
                    )
                    .clicked()
                {
                    self.active_tab = index;
                }
            }
            if ui
                .small_button("+")
                .on_hover_text("New terminal tab")
                .clicked()
            {
                self.tabs.push(WorkspaceTab::Terminal(self.next_terminal));
                self.next_terminal += 1;
                self.active_tab = self.tabs.len() - 1;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⚙").on_hover_text("Client settings").clicked() {
                    self.show_client_settings = true;
                }
                ui.label(
                    RichText::new(if self.active.is_some() { "●" } else { "○" }).color(
                        if self.active.is_some() {
                            Color32::from_rgb(105, 190, 118)
                        } else {
                            MUTED
                        },
                    ),
                );
            });
        });
    }

    fn home_panel(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(72.0);
            if ui
                .add_sized(
                    [240.0, 132.0],
                    egui::Button::new(RichText::new("+").size(36.0).color(ACCENT)).fill(PANEL),
                )
                .clicked()
            {
                self.editing = Host::default();
                self.selected = None;
                self.show_settings = true;
            }
        });
        ui.add_space(42.0);
        ui.horizontal_wrapped(|ui| {
            for host in self.hosts.hosts.clone() {
                let card = format!(
                    "{}\n{}@{}:{}",
                    host.name, host.user, host.address, host.port
                );
                if ui
                    .add_sized(
                        [240.0, 132.0],
                        egui::Button::new(RichText::new(card).size(16.0)).fill(PANEL),
                    )
                    .clicked()
                {
                    self.select_host(host.id);
                    self.connect();
                }
            }
        });
    }

    fn terminal_panel(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("TERMINAL").size(10.0).color(MUTED));
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.terminal {
                    ui.label(
                        RichText::new(line)
                            .family(egui::FontFamily::Monospace)
                            .color(Color32::from_rgb(211, 218, 227)),
                    );
                }
            });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("❯").color(ACCENT));
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.command)
                    .font(TextStyle::Monospace)
                    .hint_text("Run command…")
                    .desired_width(f32::INFINITY),
            );
            if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                self.send_command();
            }
            if ui.button("Run").clicked() {
                self.send_command();
            }
        });
    }

    fn files_panel(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |columns| {
            columns[0].vertical(|ui| {
                ui.label(RichText::new("LOCAL").size(10.0).color(MUTED));
                ui.horizontal(|ui| {
                    let path_edit = ui.add(
                        egui::TextEdit::singleline(&mut self.local_path_text)
                            .hint_text("Local path"),
                    );
                    if path_edit.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        self.local_path = expand_home(&self.local_path_text);
                        self.refresh_local();
                    }
                    if ui.small_button("↻").clicked() {
                        self.refresh_local();
                    }
                });
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for file in self.local_files.clone() {
                        let icon = if file.is_dir { "▸" } else { "·" };
                        if ui
                            .selectable_label(false, format!("{icon}  {}", file.name))
                            .double_clicked()
                            && file.is_dir
                        {
                            self.local_path = file.path;
                            self.local_path_text = self.local_path.display().to_string();
                            self.refresh_local();
                        }
                    }
                });
            });
            columns[1].vertical(|ui| {
                ui.label(RichText::new("REMOTE").size(10.0).color(MUTED));
                if self.active.is_none() {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for host in self.hosts.hosts.clone() {
                            if ui
                                .selectable_label(false, format!("●  {}", host.name))
                                .double_clicked()
                            {
                                self.select_host(host.id);
                                self.connect();
                            }
                        }
                    });
                } else {
                    ui.horizontal(|ui| {
                        let remote_edit = ui.add(
                            egui::TextEdit::singleline(&mut self.remote_path_text)
                                .hint_text("Remote path"),
                        );
                        if remote_edit.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter))
                        {
                            self.remote_path = PathBuf::from(&self.remote_path_text);
                            self.list_remote();
                        }
                        if ui.small_button("Up").clicked() {
                            self.remote_path.pop();
                            self.remote_path_text = self.remote_path.display().to_string();
                            self.list_remote();
                        }
                        if ui.small_button("↻").clicked() {
                            self.list_remote();
                        }
                    });
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for file in self.files.clone() {
                            let icon = if file.is_dir { "▸" } else { "·" };
                            if ui
                                .selectable_label(false, format!("{icon}  {}", file.name))
                                .double_clicked()
                                && file.is_dir
                            {
                                self.remote_path = PathBuf::from(file.path);
                                self.remote_path_text = self.remote_path.display().to_string();
                                self.list_remote();
                            }
                        }
                    });
                }
            });
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Server settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                field(ui, "Name", &mut self.editing.name);
                field(ui, "Host", &mut self.editing.address);
                ui.horizontal(|ui| {
                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut self.editing.port).range(1..=65535));
                });
                field(ui, "User", &mut self.editing.user);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Authentication");
                    ui.radio_value(&mut self.editing.auth_mode, AuthMode::Key, "Key");
                    ui.radio_value(&mut self.editing.auth_mode, AuthMode::Password, "Password");
                });
                match self.editing.auth_mode {
                    AuthMode::Password => {
                        ui.add(egui::TextEdit::singleline(&mut self.editing.password).password(true).hint_text("Password — not saved"));
                    }
                    AuthMode::Key => {
                        field(ui, "Key file", &mut self.editing.key_path);
                        ui.add(egui::TextEdit::singleline(&mut self.editing.key_passphrase).password(true).hint_text("Key passphrase — not saved"));
                        ui.add(egui::TextEdit::multiline(&mut self.editing.pasted_key).hint_text("Or paste a private key here (not saved)").desired_rows(3));
                    }
                }
                ui.separator();
                ui.checkbox(&mut self.editing.jump_host.enabled, "Jump host / proxy");
                if self.editing.jump_host.enabled {
                    field(ui, "Protocol", &mut self.editing.jump_host.protocol);
                    field(ui, "Proxy host", &mut self.editing.jump_host.address);
                    ui.horizontal(|ui| {
                        ui.label("Proxy port");
                        ui.add(egui::DragValue::new(&mut self.editing.jump_host.port).range(1..=65535));
                    });
                    field(ui, "Proxy user", &mut self.editing.jump_host.user);
                    ui.add(egui::TextEdit::singleline(&mut self.editing.jump_host.password).password(true).hint_text("Proxy password — not saved"));
                }
                ui.add_space(10.0);
                if ui.button("Save server").clicked() {
                    self.save_edit();
                }
                ui.label(
                    RichText::new(
                        "Passwords, passphrases, and pasted keys remain only in memory and are never written to the profile.",
                    )
                    .size(10.0)
                    .color(MUTED),
                );
            });
        self.show_settings = open;
    }

    fn client_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_client_settings;
        egui::Window::new("Client settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.checkbox(&mut self.client_settings.dark_theme, "Dark theme");
                ui.add(
                    egui::Slider::new(&mut self.client_settings.background_opacity, 150..=255)
                        .text("Background opacity"),
                );
                if ui.button("Save settings").clicked() {
                    let _ = save_client_settings(&self.client_settings);
                    apply_client_theme(ctx, &self.client_settings);
                }
            });
        self.show_client_settings = open;
    }

    fn fingerprint_window(&mut self, ctx: &egui::Context) {
        let Some(fingerprint) = self.pending_fingerprint.clone() else {
            return;
        };
        egui::Window::new("Verify server fingerprint")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
            .show(ctx, |ui| {
                ui.label("Verify this SHA-256 fingerprint with the server owner:");
                ui.monospace(&fingerprint);
                ui.horizontal(|ui| {
                    if ui.button("Reject").clicked() {
                        if let Some(active) = &self.active {
                            let _ = active.command_tx.send(WorkerCommand::TrustHost(false));
                        }
                        self.pending_fingerprint = None;
                    }
                    if ui
                        .add(egui::Button::new("Trust and connect").fill(ACCENT))
                        .clicked()
                    {
                        if let Some(active) = &self.active {
                            let _ = active.command_tx.send(WorkerCommand::TrustHost(true));
                        }
                        if !self.editing.trusted_fingerprints.contains(&fingerprint) {
                            self.editing.trusted_fingerprints.push(fingerprint);
                            self.save_edit();
                        }
                        self.pending_fingerprint = None;
                    }
                });
            });
    }
}

impl eframe::App for DotsApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll_connection();
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::TopBottomPanel::top("top")
            .exact_height(56.0)
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(ctx, |ui| self.top_bar(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(Color32::from_rgba_unmultiplied(
                        22,
                        25,
                        30,
                        self.client_settings.background_opacity,
                    ))
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                match self
                    .tabs
                    .get(self.active_tab)
                    .copied()
                    .unwrap_or(WorkspaceTab::Home)
                {
                    WorkspaceTab::Home => self.home_panel(ui),
                    WorkspaceTab::Sftp => self.files_panel(ui),
                    WorkspaceTab::Terminal(_) => self.terminal_panel(ui),
                }
            });
        if self.show_settings {
            self.settings_window(ctx);
        }
        if self.show_client_settings {
            self.client_settings_window(ctx);
        }
        self.fingerprint_window(ctx);
    }
}

fn worker(host: Host, commands: Receiver<WorkerCommand>, events: Sender<WorkerEvent>) {
    if let Err(error) = worker_inner(host, commands, events.clone()) {
        let _ = events.send(WorkerEvent::Error(error.to_string()));
        let _ = events.send(WorkerEvent::Disconnected);
    }
}

fn worker_inner(
    host: Host,
    commands: Receiver<WorkerCommand>,
    events: Sender<WorkerEvent>,
) -> Result<()> {
    let tcp = TcpStream::connect(format!("{}:{}", host.address, host.port))
        .context("TCP connection failed")?;
    let mut session = Session::new().context("Could not create SSH session")?;
    session.set_tcp_stream(tcp);
    session.handshake().context("SSH handshake failed")?;
    let fingerprint = session
        .host_key_hash(ssh2::HashType::Sha256)
        .map(|bytes| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        })
        .unwrap_or_else(|| "Server did not provide a SHA-256 host fingerprint".into());
    if !host.trusted_fingerprints.contains(&fingerprint) {
        let _ = events.send(WorkerEvent::HostFingerprint(fingerprint));
        match commands
            .recv()
            .context("Connection approval was cancelled")?
        {
            WorkerCommand::TrustHost(true) => {}
            WorkerCommand::TrustHost(false) | WorkerCommand::Disconnect => {
                return Err(anyhow::anyhow!("Host fingerprint rejected"));
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Host fingerprint must be approved before connecting"
                ));
            }
        }
    }
    match host.auth_mode {
        AuthMode::Password => session
            .userauth_password(&host.user, &host.password)
            .context("Password authentication failed")?,
        AuthMode::Key if !host.pasted_key.trim().is_empty() => session
            .userauth_pubkey_memory(
                &host.user,
                None,
                &host.pasted_key,
                (!host.key_passphrase.is_empty()).then_some(host.key_passphrase.as_str()),
            )
            .context("Pasted private key authentication failed")?,
        AuthMode::Key => {
            let key = expand_home(&host.key_path);
            if !key.exists() {
                return Err(anyhow::anyhow!("Private key not found: {}", key.display()));
            }
            session
                .userauth_pubkey_file(
                    &host.user,
                    None,
                    &key,
                    (!host.key_passphrase.is_empty()).then_some(host.key_passphrase.as_str()),
                )
                .context("SSH key authentication failed")?;
        }
    }
    if !session.authenticated() {
        return Err(anyhow::anyhow!("Authentication was rejected"));
    }
    let _ = events.send(WorkerEvent::Connected);
    while let Ok(command) = commands.recv() {
        match command {
            WorkerCommand::TrustHost(_) => {}
            WorkerCommand::Run(command) => run_command(&session, &command, &events)?,
            WorkerCommand::List(path) => list_files(&session, &path, &events)?,
            WorkerCommand::Upload(local, remote) => upload(&session, &local, &remote, &events)?,
            WorkerCommand::Disconnect => break,
        }
    }
    let _ = events.send(WorkerEvent::Disconnected);
    Ok(())
}

fn run_command(session: &Session, command: &str, events: &Sender<WorkerEvent>) -> Result<()> {
    let mut channel = session.channel_session()?;
    channel.exec(command)?;
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    let mut stderr = String::new();
    channel.stderr().read_to_string(&mut stderr)?;
    channel.wait_close()?;
    if !output.is_empty() {
        let _ = events.send(WorkerEvent::Output(output));
    }
    if !stderr.is_empty() {
        let _ = events.send(WorkerEvent::Output(format!("stderr: {stderr}")));
    }
    Ok(())
}

fn list_files(session: &Session, path: &Path, events: &Sender<WorkerEvent>) -> Result<()> {
    let sftp = session.sftp()?;
    let files = sftp
        .readdir(path)?
        .into_iter()
        .filter_map(|(path, stat)| {
            let name = path.file_name()?.to_string_lossy().to_string();
            if name == "." || name == ".." {
                return None;
            }
            Some(RemoteFile {
                name,
                path: path.to_string_lossy().to_string(),
                is_dir: stat.is_dir(),
                size: stat.size.unwrap_or(0),
            })
        })
        .collect();
    let _ = events.send(WorkerEvent::Files(files));
    Ok(())
}

fn upload(
    session: &Session,
    local: &Path,
    remote: &Path,
    events: &Sender<WorkerEvent>,
) -> Result<()> {
    let data = fs::read(local).with_context(|| format!("Cannot read {}", local.display()))?;
    let sftp = session.sftp()?;
    let mut remote_file = sftp.create(remote)?;
    remote_file.write_all(&data)?;
    let _ = events.send(WorkerEvent::Output(format!("Uploaded {}", local.display())));
    Ok(())
}

fn configure_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = PANEL;
    style.visuals.extreme_bg_color = Color32::from_rgb(18, 20, 24);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    ctx.set_style(style);
}
fn apply_client_theme(ctx: &egui::Context, settings: &ClientSettings) {
    let mut style = (*ctx.style()).clone();
    style.visuals = if settings.dark_theme {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    style.visuals.panel_fill =
        Color32::from_rgba_unmultiplied(28, 32, 39, settings.background_opacity);
    style.visuals.window_fill = style.visuals.panel_fill;
    ctx.set_style(style);
}
fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::TextEdit::singleline(value).desired_width(290.0));
    });
}
fn next_id(hosts: &SavedHosts) -> u64 {
    hosts.hosts.iter().map(|host| host.id).max().unwrap_or(0) + 1
}
fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "dots", "Dots").map(|dirs| dirs.config_dir().join("hosts.json"))
}
fn client_settings_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "dots", "Dots").map(|dirs| dirs.config_dir().join("settings.json"))
}
fn load_client_settings() -> Result<ClientSettings> {
    let Some(path) = client_settings_path() else {
        return Ok(ClientSettings::default());
    };
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn save_client_settings(settings: &ClientSettings) -> Result<()> {
    let Some(path) = client_settings_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(settings)?)?;
    Ok(())
}
fn load_hosts() -> Result<SavedHosts> {
    let Some(path) = config_path() else {
        return Ok(SavedHosts::default());
    };
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn save_hosts(hosts: &SavedHosts) -> Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(hosts)?)?;
    Ok(())
}
fn expand_home(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}
fn dirs_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}
fn list_local_files(path: &Path) -> Vec<LocalFile> {
    let mut files = fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            Some(LocalFile {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path(),
                is_dir: metadata.is_dir(),
                size: metadata.len(),
            })
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|file| (!file.is_dir, file.name.to_lowercase()));
    files
}
