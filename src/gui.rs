//! Native GUI (egui/eframe). The download loop runs on a background tokio
//! runtime and reports progress into a shared [`Shared`] state that the UI
//! snapshots each frame.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use eframe::egui;
use futures::StreamExt;

use crate::download::{
    download_one, load_all_entries, plan_tasks, Outcome, Progress, USER_AGENT_VALUE,
};
use crate::index::{self, IndexReport};
use crate::manifest::Asset;
use crate::{fmt_bytes, DEFAULT_DAT_DIR};

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobState {
    Idle,
    LoadingManifests,
    Ready,
    Downloading,
    Paused,
    Cancelling,
    Indexing,
    Done,
    Failed,
}

#[derive(Clone)]
struct CategoryInfo {
    name: String,
    count: usize,
    bytes: u64,
}

/// A file currently being downloaded, with per-file progress for the live view.
#[derive(Clone)]
struct ActiveFile {
    display: String,
    size: u64,
    done: u64,
}

struct Shared {
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    state: JobState,
    error: Option<String>,
    inventory: Vec<CategoryInfo>,
    entries: Option<Arc<Vec<Asset>>>,
    manifest_name: Option<String>,
    manifest_mtime: Option<SystemTime>,
    present_files: Option<usize>,
    missing_files: usize,
    missing_bytes: u64,
    db: Option<Result<IndexReport, String>>,
    db_busy: bool,
    total_files: usize,
    total_bytes: u64,
    done_files: usize,
    skip_files: usize,
    fail_files: usize,
    bytes_done: u64,
    active: Vec<ActiveFile>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            state: JobState::Idle,
            error: None,
            inventory: Vec::new(),
            entries: None,
            manifest_name: None,
            manifest_mtime: None,
            present_files: None,
            missing_files: 0,
            missing_bytes: 0,
            db: None,
            db_busy: false,
            total_files: 0,
            total_bytes: 0,
            done_files: 0,
            skip_files: 0,
            fail_files: 0,
            bytes_done: 0,
            active: Vec::new(),
        }
    }
}

/// GUI progress sink: byte counts into `Shared`, pause/cancel from the flags.
#[derive(Clone)]
struct GuiProgress {
    shared: Arc<Mutex<Shared>>,
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    /// Display name of the file this sink is counting bytes for.
    key: String,
}

impl Progress for GuiProgress {
    fn on_bytes(&self, n: u64) {
        let mut sh = self.shared.lock().unwrap();
        sh.bytes_done += n;
        for f in &mut sh.active {
            if f.display == self.key {
                f.done += n;
                break;
            }
        }
    }
    fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

struct App {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    shared: Arc<Mutex<Shared>>,
    dat_dir: String,
    workers: usize,
    selected: HashSet<String>,
    /// Rolling (time, cumulative bytes) samples for a windowed download speed.
    speed_history: VecDeque<(Instant, u64)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
        cc.egui_ctx.set_style(style);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT_VALUE)
            .build()
            .expect("http client");
        let app = App {
            runtime,
            client,
            shared: Arc::new(Mutex::new(Shared::new())),
            dat_dir: DEFAULT_DAT_DIR.to_string(),
            workers: 8,
            selected: HashSet::new(),
            speed_history: VecDeque::new(),
        };
        app.spawn_load();
        app
    }

    /// Fetch the manifest tree in the background and populate the inventory.
    fn spawn_load(&self) {
        let shared = Arc::clone(&self.shared);
        let client = self.client.clone();
        let dat_dir = self.dat_dir.clone();
        self.runtime.spawn(async move {
            {
                let mut sh = shared.lock().unwrap();
                sh.state = JobState::LoadingManifests;
                sh.error = None;
            }
            match load_all_entries(client.clone(), dat_dir.clone(), None).await {
                Ok(set) => {
                    let assets = Arc::new(set.assets);
                    {
                        let mut sh = shared.lock().unwrap();
                        sh.inventory = build_inventory(&assets);
                        sh.entries = Some(Arc::clone(&assets));
                        sh.manifest_name = Some(set.root_name);
                        sh.manifest_mtime = set.root_mtime;
                        // Only report back if a concurrent job has not taken over.
                        if sh.state == JobState::LoadingManifests {
                            sh.state = JobState::Ready;
                        }
                    }
                    scan_files(Arc::clone(&shared), assets, dat_dir.clone()).await;
                    run_index(shared, dat_dir, false).await;
                }
                Err(e) => {
                    let mut sh = shared.lock().unwrap();
                    sh.error = Some(format!("{e:#}"));
                    if sh.state == JobState::LoadingManifests {
                        sh.state = JobState::Failed;
                    }
                }
            }
        });
    }

    fn spawn_download(&self, all: bool) {
        let shared = Arc::clone(&self.shared);
        let client = self.client.clone();
        let dat_dir = self.dat_dir.clone();
        let workers = self.workers;
        let selected = if all {
            HashSet::new()
        } else {
            self.selected.clone()
        };
        self.runtime.spawn(async move {
            download_job(shared, client, dat_dir, workers, selected, all).await;
        });
    }

    /// Mark size-matching files as downloaded in the game's meta database.
    fn spawn_index(&self) {
        let shared = Arc::clone(&self.shared);
        let dat_dir = self.dat_dir.clone();
        self.runtime.spawn(async move {
            run_index(shared, dat_dir, true).await;
        });
    }

    fn snapshot(&self) -> Snapshot {
        let sh = self.shared.lock().unwrap();
        Snapshot {
            state: sh.state,
            error: sh.error.clone(),
            inventory: sh.inventory.clone(),
            manifest_name: sh.manifest_name.clone(),
            manifest_mtime: sh.manifest_mtime,
            present_files: sh.present_files,
            missing_files: sh.missing_files,
            missing_bytes: sh.missing_bytes,
            db: sh.db.clone(),
            db_busy: sh.db_busy,
            total_assets: sh.entries.as_ref().map_or(0, |e| e.len()),
            total_files: sh.total_files,
            total_bytes: sh.total_bytes,
            done_files: sh.done_files,
            skip_files: sh.skip_files,
            fail_files: sh.fail_files,
            bytes_done: sh.bytes_done,
            active: sh.active.clone(),
        }
    }
}

struct Snapshot {
    state: JobState,
    error: Option<String>,
    inventory: Vec<CategoryInfo>,
    manifest_name: Option<String>,
    manifest_mtime: Option<SystemTime>,
    present_files: Option<usize>,
    missing_files: usize,
    missing_bytes: u64,
    db: Option<Result<IndexReport, String>>,
    db_busy: bool,
    total_assets: usize,
    total_files: usize,
    total_bytes: u64,
    done_files: usize,
    skip_files: usize,
    fail_files: usize,
    bytes_done: u64,
    active: Vec<ActiveFile>,
}

async fn download_job(
    shared: Arc<Mutex<Shared>>,
    client: reqwest::Client,
    dat_dir: String,
    workers: usize,
    selected: HashSet<String>,
    all: bool,
) {
    // Fresh run: clear any stale pause/cancel flags.
    {
        let sh = shared.lock().unwrap();
        sh.paused.store(false, Ordering::SeqCst);
        sh.cancelled.store(false, Ordering::SeqCst);
    }

    // Reuse cached entries, or load them if this is the first run. Keep the
    // lock scope tight so no `MutexGuard` is held across an `.await`.
    let cached = {
        let sh = shared.lock().unwrap();
        sh.entries.clone()
    };
    let assets = match cached {
        Some(a) => a,
        None => match load_all_entries(client.clone(), dat_dir.clone(), None).await {
            Ok(set) => {
                let a = Arc::new(set.assets);
                let mut sh = shared.lock().unwrap();
                sh.inventory = build_inventory(&a);
                sh.entries = Some(Arc::clone(&a));
                sh.manifest_name = Some(set.root_name);
                sh.manifest_mtime = set.root_mtime;
                a
            }
            Err(e) => {
                let mut sh = shared.lock().unwrap();
                sh.error = Some(format!("{e:#}"));
                sh.state = JobState::Failed;
                return;
            }
        },
    };

    // Build the task list (SHA1 hashing) and skip already-complete files
    // (filesystem metadata) on the blocking pool. Both are synchronous and can
    // touch up to ~191k files, so running them inline would stall an async
    // worker thread for seconds at a time.
    let assets_for_scan = Arc::clone(&assets);
    let plan_dat_dir = dat_dir.clone();
    let (todo, skip) = match tokio::task::spawn_blocking(move || {
        plan_tasks(&assets, &plan_dat_dir, |a| {
            all || selected.contains(String::from_utf8_lossy(&a.category).as_ref())
        })
    })
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let mut sh = shared.lock().unwrap();
            sh.error = Some(format!("task planning failed: {e}"));
            sh.state = JobState::Failed;
            return;
        }
    };

    {
        let mut sh = shared.lock().unwrap();
        // A category subset says nothing about the overall file count; keep the
        // last full scan's numbers until the post-download rescan.
        if all {
            sh.present_files = Some(skip);
            sh.missing_files = todo.len();
            sh.missing_bytes = todo.iter().map(|t| t.size).sum();
        }
        sh.total_files = todo.len();
        sh.total_bytes = todo.iter().map(|t| t.size).sum();
        sh.done_files = 0;
        sh.skip_files = skip;
        sh.fail_files = 0;
        sh.bytes_done = 0;
        sh.active.clear();
        sh.state = JobState::Downloading;
        sh.error = None;
    }

    let (paused, cancelled) = {
        let sh = shared.lock().unwrap();
        (Arc::clone(&sh.paused), Arc::clone(&sh.cancelled))
    };

    let stop = Arc::clone(&cancelled);
    let futs = futures::stream::iter(todo.into_iter())
        // Stop submitting new work the instant cancel is requested, so we
        // never have to drain the whole (up to ~191k) task list.
        .take_while(move |_| futures::future::ready(!stop.load(Ordering::SeqCst)))
        .map(|t| {
            let shared = Arc::clone(&shared);
            let client = client.clone();
            let progress = GuiProgress {
                shared: Arc::clone(&shared),
                paused: Arc::clone(&paused),
                cancelled: Arc::clone(&cancelled),
                key: t.display.clone(),
            };
            async move {
                {
                    let mut sh = shared.lock().unwrap();
                    sh.active.push(ActiveFile {
                        display: t.display.clone(),
                        size: t.size,
                        done: 0,
                    });
                }
                let outcome = download_one(&client, &t.url, &t.dest, t.size, &progress).await;
                let mut sh = shared.lock().unwrap();
                sh.active.retain(|f| f.display != t.display);
                match outcome {
                    Outcome::Ok => sh.done_files += 1,
                    Outcome::Skip => sh.skip_files += 1,
                    Outcome::Fail => sh.fail_files += 1,
                    Outcome::Cancelled => {}
                }
            }
        });

    futs.buffer_unordered(workers).collect::<Vec<()>>().await;

    {
        let mut sh = shared.lock().unwrap();
        sh.active.clear();
    }
    let cancelled = shared.lock().unwrap().cancelled.load(Ordering::SeqCst);
    if !cancelled {
        // Mark the new files as downloaded, otherwise the game re-fetches them.
        shared.lock().unwrap().state = JobState::Indexing;
        run_index(Arc::clone(&shared), dat_dir.clone(), true).await;
        scan_files(Arc::clone(&shared), assets_for_scan, dat_dir).await;
    }

    let mut sh = shared.lock().unwrap();
    if cancelled {
        sh.state = JobState::Ready; // resume/restart with cached inventory
    } else {
        sh.state = JobState::Done;
    }
    sh.paused.store(false, Ordering::SeqCst);
    sh.cancelled.store(false, Ordering::SeqCst);
}

/// Count files already on disk (blocking: one stat per asset).
async fn scan_files(shared: Arc<Mutex<Shared>>, assets: Arc<Vec<Asset>>, dat_dir: String) {
    let scan = tokio::task::spawn_blocking(move || plan_tasks(&assets, &dat_dir, |_| true)).await;
    if let Ok((todo, skip)) = scan {
        let mut sh = shared.lock().unwrap();
        sh.present_files = Some(skip);
        sh.missing_files = todo.len();
        sh.missing_bytes = todo.iter().map(|t| t.size).sum();
    }
}

/// Scan (or with `apply`, mark) the game's meta database on the blocking pool.
async fn run_index(shared: Arc<Mutex<Shared>>, dat_dir: String, apply: bool) {
    {
        let mut sh = shared.lock().unwrap();
        if sh.db_busy {
            return;
        }
        sh.db_busy = true;
    }
    let result =
        tokio::task::spawn_blocking(move || index::index_meta(Path::new(&dat_dir), apply)).await;
    let mut sh = shared.lock().unwrap();
    sh.db_busy = false;
    sh.db = Some(match result {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("index task failed: {e}")),
    });
}

fn build_inventory(assets: &[Asset]) -> Vec<CategoryInfo> {
    let mut map: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for a in assets {
        let name = String::from_utf8_lossy(&a.category).into_owned();
        let e = map.entry(name).or_insert((0, 0));
        e.0 += 1;
        e.1 += a.size;
    }
    map.into_iter()
        .map(|(name, (count, bytes))| CategoryInfo { name, count, bytes })
        .collect()
}

fn state_label(state: JobState) -> &'static str {
    match state {
        JobState::Idle => "Idle",
        JobState::LoadingManifests => "Loading manifests…",
        JobState::Ready => "Ready",
        JobState::Downloading => "Downloading",
        JobState::Paused => "Paused",
        JobState::Cancelling => "Cancelling…",
        JobState::Indexing => "Indexing game database…",
        JobState::Done => "Done",
        JobState::Failed => "Error",
    }
}

/// The game stores no resource version, so the cached root manifest is the
/// version stand-in; show how long ago the game last synced it.
fn synced_label(t: Option<SystemTime>) -> String {
    let secs = t
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

fn state_color(state: JobState) -> egui::Color32 {
    match state {
        JobState::Done => egui::Color32::LIGHT_GREEN,
        JobState::Failed => egui::Color32::LIGHT_RED,
        JobState::Paused | JobState::Cancelling => egui::Color32::YELLOW,
        _ => egui::Color32::GRAY,
    }
}

const BANNER_OK: egui::Color32 = egui::Color32::from_rgb(30, 52, 36);
const BANNER_WARN: egui::Color32 = egui::Color32::from_rgb(58, 50, 24);
const BANNER_ERR: egui::Color32 = egui::Color32::from_rgb(58, 30, 30);

fn card(ui: &mut egui::Ui, fill: egui::Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .corner_radius(8)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui);
        });
}

fn tile(ui: &mut egui::Ui, title: &str, value: String, sub: String, color: egui::Color32) {
    let fill = ui.visuals().faint_bg_color;
    card(ui, fill, |ui| {
        ui.label(egui::RichText::new(title).small().weak());
        ui.add_space(1.0);
        ui.label(egui::RichText::new(value).size(18.0).strong().color(color));
        ui.label(egui::RichText::new(sub).small().weak());
    });
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let snap = self.snapshot();

        // egui only repaints on input events (mouse motion, focus, etc.), so a
        // progress screen would freeze while the window is unfocused or the
        // cursor is still. Request a steady tick while work is in flight so the
        // bars, per-file progress, and speed/ETA keep animating regardless.
        if matches!(
            snap.state,
            JobState::LoadingManifests | JobState::Downloading | JobState::Cancelling
        ) || snap.db_busy
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("Umamusume Downloader");
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(state_label(snap.state)).color(state_color(snap.state)),
                );
                if snap.db_busy {
                    ui.spinner();
                }
            });
            if let Some(err) = &snap.error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }
            ui.add_space(8.0);
        });

        egui::SidePanel::left("config")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("SETTINGS").small().strong().weak());
                ui.label("Data directory (parent of dat/):");
                ui.add(
                    egui::TextEdit::singleline(&mut self.dat_dir).desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Workers:");
                    ui.add(egui::Slider::new(&mut self.workers, 1..=64));
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.strong("Categories");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("None").clicked() {
                            self.selected.clear();
                        }
                        if ui.small_button("All").clicked() {
                            for c in &snap.inventory {
                                self.selected.insert(c.name.clone());
                            }
                        }
                        ui.weak(format!(
                            "{} / {} selected",
                            snap.inventory
                                .iter()
                                .filter(|c| self.selected.contains(&c.name))
                                .count(),
                            snap.inventory.len()
                        ));
                    });
                });
                ui.add_space(2.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for c in &snap.inventory {
                            let mut checked = self.selected.contains(&c.name);
                            let label = format!(
                                "{}  ·  {} files, {}",
                                c.name,
                                thousands(c.count as u64),
                                fmt_bytes(c.bytes)
                            );
                            if ui.checkbox(&mut checked, label).changed() {
                                if checked {
                                    self.selected.insert(c.name.clone());
                                } else {
                                    self.selected.remove(&c.name);
                                }
                            }
                        }
                        if snap.inventory.is_empty() {
                            ui.weak(if snap.state == JobState::LoadingManifests {
                                "Loading…"
                            } else {
                                "No categories loaded."
                            });
                        }
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let can_start = matches!(
                snap.state,
                JobState::Idle | JobState::Ready | JobState::Done | JobState::Failed
            );
            // Manifest loading has no cancel path, so do not offer one there.
            let can_cancel = matches!(snap.state, JobState::Downloading | JobState::Paused);

            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                let accent = ui.visuals().selection.bg_fill;
                if ui
                    .add_enabled(can_start, egui::Button::new("Start"))
                    .clicked()
                {
                    // Enter the busy state before spawning: planning takes seconds,
                    // and Start must not launch a second job on the same files.
                    self.shared.lock().unwrap().state = JobState::Downloading;
                    self.spawn_download(false);
                }
                let mut update = egui::Button::new(egui::RichText::new("Update all").strong());
                if can_start {
                    update = update.fill(accent);
                }
                if ui.add_enabled(can_start, update).clicked() {
                    self.shared.lock().unwrap().state = JobState::Downloading;
                    self.spawn_download(true);
                }
                ui.separator();
                if ui
                    .add_enabled(snap.state == JobState::Downloading, egui::Button::new("Pause"))
                    .clicked()
                {
                    let mut sh = self.shared.lock().unwrap();
                    sh.paused.store(true, Ordering::SeqCst);
                    sh.state = JobState::Paused;
                }
                if ui
                    .add_enabled(snap.state == JobState::Paused, egui::Button::new("Resume"))
                    .clicked()
                {
                    let mut sh = self.shared.lock().unwrap();
                    sh.paused.store(false, Ordering::SeqCst);
                    sh.state = JobState::Downloading;
                }
                if ui
                    .add_enabled(can_cancel, egui::Button::new("Cancel"))
                    .clicked()
                {
                    let mut sh = self.shared.lock().unwrap();
                    sh.cancelled.store(true, Ordering::SeqCst);
                    sh.state = JobState::Cancelling;
                }
                ui.separator();
                if ui
                    .add_enabled(can_start, egui::Button::new("Reload manifests"))
                    .clicked()
                {
                    self.spawn_load();
                }
                // The game must be closed; if it is not, the error shows in the
                // banner below.
                if ui
                    .add_enabled(
                        can_start && !snap.db_busy,
                        egui::Button::new("Fix game DB"),
                    )
                    .clicked()
                {
                    self.spawn_index();
                }
            });

            // Headline status: the one thing the user should take away.
            ui.add_space(12.0);
            let present = snap.present_files;
            let db = snap.db.as_ref();
            let (fill, color, title, detail) = if let Some(Err(e)) = db {
                (
                    BANNER_ERR,
                    egui::Color32::LIGHT_RED,
                    "Game database error".to_string(),
                    e.clone(),
                )
            } else if snap.manifest_name.is_none() {
                (
                    ui.visuals().faint_bg_color,
                    egui::Color32::GRAY,
                    "Loading manifests…".to_string(),
                    "Reading the game's cached manifest tree from dat/.".to_string(),
                )
            } else if present.is_none() {
                (
                    ui.visuals().faint_bg_color,
                    egui::Color32::GRAY,
                    "Scanning files…".to_string(),
                    "Checking which assets are already on disk.".to_string(),
                )
            } else if snap.missing_files > 0 {
                (
                    BANNER_WARN,
                    egui::Color32::YELLOW,
                    format!(
                        "{} files missing ({})",
                        thousands(snap.missing_files as u64),
                        fmt_bytes(snap.missing_bytes)
                    ),
                    "Press Update all to sync with the current game data.".to_string(),
                )
            } else if matches!(db, Some(Ok(r)) if !r.is_fully_indexed()) {
                (
                    BANNER_WARN,
                    egui::Color32::YELLOW,
                    "Game database out of sync".to_string(),
                    "All files are on disk — press Fix game DB so the game sees them."
                        .to_string(),
                )
            } else if matches!(db, Some(Ok(_))) {
                (
                    BANNER_OK,
                    egui::Color32::LIGHT_GREEN,
                    "Game data is up to date".to_string(),
                    format!(
                        "{} files · synced {} · database indexed",
                        thousands(snap.total_assets as u64),
                        synced_label(snap.manifest_mtime)
                    ),
                )
            } else {
                (
                    ui.visuals().faint_bg_color,
                    egui::Color32::GRAY,
                    "Checking game database…".to_string(),
                    String::new(),
                )
            };
            card(ui, fill, |ui| {
                ui.label(egui::RichText::new(title).size(19.0).strong().color(color));
                if !detail.is_empty() {
                    ui.label(egui::RichText::new(detail).weak());
                }
            });

            // Summary tiles.
            ui.add_space(10.0);
            let files_value = match present {
                Some(p) => format!(
                    "{} / {}",
                    thousands(p as u64),
                    thousands(snap.total_assets as u64)
                ),
                None => "—".to_string(),
            };
            let (files_color, files_sub) = if present.is_none() {
                (egui::Color32::GRAY, "scanning…".to_string())
            } else if snap.missing_files == 0 {
                (egui::Color32::LIGHT_GREEN, "all files present".to_string())
            } else {
                (
                    egui::Color32::YELLOW,
                    format!(
                        "{} missing · {}",
                        thousands(snap.missing_files as u64),
                        fmt_bytes(snap.missing_bytes)
                    ),
                )
            };
            let (db_value, db_sub, db_color) = match db {
                None => ("—".to_string(), "checking…".to_string(), egui::Color32::GRAY),
                Some(Err(_)) => (
                    "error".to_string(),
                    "see message above".to_string(),
                    egui::Color32::LIGHT_RED,
                ),
                Some(Ok(r)) => (
                    format!(
                        "{} / {}",
                        thousands(r.indexed.max(0) as u64),
                        thousands(r.assets.max(0) as u64)
                    ),
                    if r.is_fully_indexed() {
                        "fully indexed".to_string()
                    } else {
                        "press Fix game DB".to_string()
                    },
                    if r.is_fully_indexed() {
                        egui::Color32::LIGHT_GREEN
                    } else {
                        egui::Color32::YELLOW
                    },
                ),
            };
            let root = snap
                .manifest_name
                .as_deref()
                .map_or("—", |n| &n[..n.len().min(8)]);
            let synced = synced_label(snap.manifest_mtime);
            let data_color = if snap.manifest_name.is_some() {
                ui.visuals().strong_text_color()
            } else {
                egui::Color32::GRAY
            };
            ui.columns(3, |cols| {
                tile(&mut cols[0], "Game data", root.to_string(), format!("synced {synced}"), data_color);
                tile(&mut cols[1], "Files on disk", files_value, files_sub, files_color);
                tile(&mut cols[2], "Game database", db_value, db_sub, db_color);
            });

            // Stats: a windowed download speed that drops to zero as soon as
            // bytes stop flowing (paused, stalled, or finished), plus an ETA
            // shown only while actively downloading.
            let downloading = snap.state == JobState::Downloading;
            let now = Instant::now();
            if self
                .speed_history
                .back()
                .map(|&(_, b)| b > snap.bytes_done)
                .unwrap_or(false)
            {
                self.speed_history.clear(); // bytes_done reset by a fresh run
            }
            self.speed_history.push_back((now, snap.bytes_done));
            while self.speed_history.len() > 2
                && now
                    .duration_since(self.speed_history.front().unwrap().0)
                    .as_secs_f64()
                    > 2.0
            {
                self.speed_history.pop_front();
            }
            let rate = if downloading {
                match self.speed_history.front() {
                    Some(&(t0, b0)) => {
                        let dt = now.duration_since(t0).as_secs_f64();
                        if dt > 0.05 {
                            snap.bytes_done.saturating_sub(b0) as f64 / dt
                        } else {
                            0.0
                        }
                    }
                    None => 0.0,
                }
            } else {
                0.0
            };
            let remaining = snap.total_bytes.saturating_sub(snap.bytes_done);
            let eta = if rate > 0.0 { remaining as f64 / rate } else { 0.0 };

            // Progress card.
            ui.add_space(10.0);
            let frac = if snap.total_bytes > 0 {
                snap.bytes_done as f64 / snap.total_bytes as f64
            } else {
                0.0
            };
            card(ui, ui.visuals().faint_bg_color, |ui| {
                ui.add(
                    egui::ProgressBar::new(frac.clamp(0.0, 1.0) as f32)
                        .desired_height(18.0)
                        .show_percentage()
                        .text(format!(
                            "{} / {}",
                            fmt_bytes(snap.bytes_done),
                            fmt_bytes(snap.total_bytes)
                        )),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} done · {} skipped · {} failed · {} total",
                        thousands(snap.done_files as u64),
                        thousands(snap.skip_files as u64),
                        thousands(snap.fail_files as u64),
                        thousands(snap.total_files as u64),
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(if downloading && rate > 0.0 {
                            format!(
                                "{}/s · ETA {}",
                                fmt_bytes(rate as u64),
                                fmt_duration(eta as u64)
                            )
                        } else {
                            "0 B/s · ETA —".to_string()
                        });
                    });
                });
            });

            // Live view of files currently being downloaded, filling the rest
            // of the window.
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.strong("Downloading now");
                if !snap.active.is_empty() {
                    ui.weak(format!("({})", snap.active.len()));
                }
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if snap.active.is_empty() {
                        ui.add_space(6.0);
                        ui.weak("No active downloads.");
                        ui.weak(
                            "Pick categories on the left and press Start, or press \
                             Update all to sync everything.",
                        );
                    } else {
                        for f in &snap.active {
                            let ffrac = if f.size > 0 {
                                (f.done as f64 / f.size as f64).clamp(0.0, 1.0) as f32
                            } else {
                                0.0
                            };
                            ui.horizontal(|ui| {
                                ui.add(egui::ProgressBar::new(ffrac).desired_width(220.0));
                                ui.label(&f.display);
                            });
                        }
                    }
                });
        });
    }
}

fn fmt_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 680.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("Umamusume Downloader"),
        ..Default::default()
    };
    eframe::run_native(
        "Umamusume Downloader",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("GUI failed: {e}"))
}
