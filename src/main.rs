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
            .with_decorations(false),
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

enum WorkerCommand {
    Run(String),
    List(PathBuf),
    Upload(PathBuf, PathBuf),
    Disconnect,
}
enum WorkerEvent {
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
    local_upload: String,
    remote_upload: String,
    status: String,
    show_settings: bool,
    tabs: Vec<WorkspaceTab>,
    active_tab: usize,
    next_terminal: u32,
}

impl DotsApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_theme(&cc.egui_ctx);
        let hosts = load_hosts().unwrap_or_default();
        let editing = hosts.hosts.first().cloned().unwrap_or_default();
        Self {
            hosts,
            selected: None,
            editing,
            active: None,
            terminal: vec![
                "Dots  •  Native SSH workspace".into(),
                "Choose a server and press Connect.".into(),
            ],
            command: String::new(),
            files: vec![],
            remote_path: PathBuf::from("."),
            local_upload: String::new(),
            remote_upload: String::new(),
            status: "Ready".into(),
            show_settings: false,
            tabs: vec![WorkspaceTab::Home, WorkspaceTab::Sftp],
            active_tab: 0,
            next_terminal: 1,
        }
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

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.add_space(12.0);
            ui.label(
                RichText::new("DOTS")
                    .strong()
                    .size(18.0)
                    .color(Color32::WHITE),
            );
            ui.label(RichText::new("REMOTE WORKSPACE").size(9.0).color(MUTED));
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("SERVERS").size(10.0).color(MUTED));
                if ui.small_button("+").clicked() {
                    self.editing = Host::default();
                    self.selected = None;
                    self.show_settings = true;
                }
            });
            ui.add_space(6.0);
            for host in self.hosts.hosts.clone() {
                let selected = self.selected == Some(host.id);
                if ui
                    .selectable_label(selected, format!("●  {}", host.name))
                    .clicked()
                {
                    self.select_host(host.id);
                    self.show_settings = true;
                }
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.separator();
                ui.label(
                    RichText::new(if self.active.is_some() {
                        "● Connected"
                    } else {
                        "○ Offline"
                    })
                    .color(if self.active.is_some() {
                        Color32::from_rgb(105, 190, 118)
                    } else {
                        MUTED
                    }),
                );
            });
        });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let drag_region = ui.interact(
            ui.max_rect(),
            ui.id().with("window_drag_region"),
            egui::Sense::drag(),
        );
        if drag_region.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
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
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.show_settings = true;
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
        ui.label(RichText::new("HOME").size(10.0).color(MUTED));
        ui.add_space(18.0);
        ui.heading(if self.active.is_some() {
            "Connected workspace"
        } else {
            "Connect a server"
        });
        ui.label(
            RichText::new(format!("{}@{}", self.editing.user, self.editing.address)).color(MUTED),
        );
        ui.add_space(18.0);
        if self.active.is_some() {
            if ui.button("Disconnect").clicked() {
                self.disconnect();
            }
        } else if ui
            .add(egui::Button::new(RichText::new("Connect").color(Color32::WHITE)).fill(ACCENT))
            .clicked()
        {
            self.connect();
        }
        ui.add_space(24.0);
        ui.label(RichText::new("RECENT OUTPUT").size(10.0).color(MUTED));
        egui::ScrollArea::vertical().show(ui, |ui| {
            for line in self.terminal.iter().rev().take(8).rev() {
                ui.label(RichText::new(line).family(egui::FontFamily::Monospace));
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
        ui.horizontal(|ui| {
            ui.label(RichText::new("SFTP").size(10.0).color(MUTED));
            if ui.small_button("↻").clicked() {
                self.list_remote();
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.remote_path.display().to_string()).color(MUTED));
            if ui.small_button("Up").clicked() {
                self.remote_path.pop();
                self.list_remote();
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for file in self.files.clone() {
                let icon = if file.is_dir { "▸" } else { "·" };
                if ui
                    .selectable_label(false, format!("{icon}  {}", file.name))
                    .double_clicked()
                    && file.is_dir
                {
                    self.remote_path = PathBuf::from(file.path);
                    self.list_remote();
                }
                if !file.is_dir {
                    ui.label(
                        RichText::new(format!("    {} bytes", file.size))
                            .size(10.0)
                            .color(MUTED),
                    );
                }
            }
            if self.files.is_empty() {
                ui.add_space(20.0);
                ui.label(RichText::new("Connect to browse remote files.").color(MUTED));
            }
        });
        ui.separator();
        ui.label(RichText::new("UPLOAD VIA SFTP").size(10.0).color(MUTED));
        ui.add(egui::TextEdit::singleline(&mut self.local_upload).hint_text("Local file path"));
        ui.add(
            egui::TextEdit::singleline(&mut self.remote_upload)
                .hint_text("Remote destination path"),
        );
        if ui.button("Upload file").clicked() {
            self.upload_file();
        }
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
                field(ui, "Private key", &mut self.editing.key_path);
                ui.add_space(10.0);
                if ui.button("Save server").clicked() {
                    self.save_edit();
                }
                ui.label(
                    RichText::new(
                        "Credentials and keys stay on your machine; Dots stores only the key path.",
                    )
                    .size(10.0)
                    .color(MUTED),
                );
            });
        self.show_settings = open;
    }
}

impl eframe::App for DotsApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll_connection();
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .default_width(190.0)
            .frame(egui::Frame::default().fill(PANEL))
            .show(ctx, |ui| self.sidebar(ui));
        egui::TopBottomPanel::top("top")
            .exact_height(56.0)
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(ctx, |ui| self.top_bar(ui));
        egui::TopBottomPanel::bottom("status")
            .exact_height(28.0)
            .frame(
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&self.status).size(10.0).color(MUTED));
                    if let Some(active) = &self.active {
                        ui.separator();
                        ui.label(
                            RichText::new(format!("{}s", active.started.elapsed().as_secs()))
                                .size(10.0)
                                .color(MUTED),
                        );
                    }
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(BG)
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
    let key = expand_home(&host.key_path);
    if key.exists() {
        session
            .userauth_pubkey_file(&host.user, None, &key, None)
            .context("SSH key authentication failed")?;
    } else {
        return Err(anyhow::anyhow!("Private key not found: {}", key.display()));
    }
    if !session.authenticated() {
        return Err(anyhow::anyhow!("Authentication was rejected"));
    }
    let _ = events.send(WorkerEvent::Connected);
    while let Ok(command) = commands.recv() {
        match command {
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
