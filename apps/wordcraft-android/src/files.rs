//! The file service of the Android app: an in-app file chooser over `std::fs`.
//!
//! craft-dialogs (native panels, `rfd`) does not build for Android, so this implements craft-host's
//! `Files` itself. Pickers are queued; `AndroidFiles::show` draws the chooser for the first one on
//! every frame, and its answer is read later through `take_answer`, as on the desktop.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use craft::host::{FileError, FileFilter, Files, PickId, PickKind, PickRequest, Picked};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The places the chooser offers as shortcuts: (label, folder). The first existing one is where a
/// picker starts when its request names no folder.
pub fn default_roots(external: Option<PathBuf>, internal: Option<PathBuf>) -> Vec<(String, PathBuf)> {
    let shared = PathBuf::from("/storage/emulated/0");
    let documents = shared.join("Documents").join("WordCraft");
    let mut roots = Vec::new();
    if std::fs::create_dir_all(&documents).is_ok() {
        roots.push(("Documents".to_string(), documents));
    }
    if let Some(path) = external {
        let _ = std::fs::create_dir_all(&path);
        roots.push(("App folder".to_string(), path));
    }
    roots.push(("Download".to_string(), shared.join("Download")));
    roots.push(("Storage".to_string(), shared));
    if let Some(path) = internal {
        roots.push(("Private".to_string(), path));
    }
    roots
}

struct Ticket {
    id: PickId,
    request: PickRequest,
    /// A `save` carries its bytes; they are written to the path the user chooses.
    bytes: Option<Vec<u8>>,
}

#[derive(Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

enum Outcome {
    Open,
    Done(Picked),
}

struct Chooser {
    ticket: Ticket,
    dir: PathBuf,
    entries: Vec<Entry>,
    error: Option<String>,
    name: String,
    selected: Vec<PathBuf>,
    filter: usize,
    /// A Save over this existing file waits for the answer to "Replace it?".
    replace: Option<PathBuf>,
}

struct Inner {
    next: u64,
    queue: VecDeque<Ticket>,
    current: Option<Chooser>,
    answered: HashMap<PickId, Picked>,
    last_folder: HashMap<String, PathBuf>,
    ctx: Option<egui::Context>,
}

/// The file service installed in the Host (`Host::builder(..).files(..)`); the app calls
/// [`AndroidFiles::show`] once per frame.
pub struct AndroidFiles {
    inner: Mutex<Inner>,
    roots: Vec<(String, PathBuf)>,
}

impl AndroidFiles {
    pub fn new(roots: Vec<(String, PathBuf)>) -> Self {
        let inner = Inner { next: 0, queue: VecDeque::new(), current: None, answered: HashMap::new(), last_folder: HashMap::new(), ctx: None };
        Self { inner: Mutex::new(inner), roots }
    }

    fn enqueue(&self, request: PickRequest, bytes: Option<Vec<u8>>) -> PickId {
        let mut inner = lock(&self.inner);
        inner.next += 1;
        let id = PickId(inner.next);
        inner.queue.push_back(Ticket { id, request, bytes });
        if let Some(ctx) = &inner.ctx {
            ctx.request_repaint();
        }
        id
    }

    fn start_folder(&self, request: &PickRequest, last: Option<&PathBuf>) -> PathBuf {
        let candidates = [request.directory.as_ref(), last, request.default_directory.as_ref()];
        for dir in candidates.into_iter().flatten() {
            if dir.is_dir() {
                return dir.clone();
            }
        }
        self.roots.iter().map(|(_, p)| p).find(|p| p.is_dir()).cloned().unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Draws the chooser of the pending picker, if any. Call it every frame.
    pub fn show(&self, ctx: &egui::Context) {
        let mut inner = lock(&self.inner);
        if inner.ctx.is_none() {
            inner.ctx = Some(ctx.clone());
        }
        if inner.current.is_none() {
            let Some(ticket) = inner.queue.pop_front() else { return };
            let start = self.start_folder(&ticket.request, inner.last_folder.get(&ticket.request.purpose));
            inner.current = Some(Chooser::new(ticket, start));
        }
        let Some(mut chooser) = inner.current.take() else { return };
        match chooser.draw(ctx, &self.roots) {
            Outcome::Open => inner.current = Some(chooser),
            Outcome::Done(picked) => {
                let Ticket { id, request, bytes } = chooser.ticket;
                let picked = finish(picked, bytes);
                let folder = match (&picked, request.kind) {
                    (Picked::Paths(paths), PickKind::Folder) => paths.first().cloned(),
                    (Picked::Paths(paths), _) => paths.first().and_then(|p| p.parent()).map(Path::to_path_buf),
                    _ => None,
                };
                if let Some(folder) = folder {
                    inner.last_folder.insert(request.purpose.clone(), folder);
                }
                inner.answered.insert(id, picked);
                ctx.request_repaint();
            }
        }
    }
}

/// A save's bytes are written to the chosen path; a pick answers as chosen.
fn finish(picked: Picked, bytes: Option<Vec<u8>>) -> Picked {
    let (Picked::Paths(paths), Some(bytes)) = (&picked, bytes) else { return picked };
    let Some(path) = paths.first() else { return picked };
    match write_file(path, &bytes) {
        Ok(()) => picked,
        Err(e) => Picked::Failed(e.to_string()),
    }
}

impl Files for AndroidFiles {
    fn pick(&self, request: PickRequest) -> PickId {
        self.enqueue(request, None)
    }

    fn take_answer(&self, id: PickId) -> Option<Picked> {
        lock(&self.inner).answered.remove(&id)
    }

    fn forget(&self, id: PickId) {
        let mut inner = lock(&self.inner);
        inner.answered.remove(&id);
        inner.queue.retain(|t| t.id != id);
        if inner.current.as_ref().is_some_and(|c| c.ticket.id == id) {
            inner.current = None;
        }
    }

    fn pick_pending(&self) -> bool {
        let inner = lock(&self.inner);
        inner.current.is_some() || !inner.queue.is_empty()
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        std::fs::read(path).map_err(|e| io_error(e, path))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), FileError> {
        write_file(path, bytes)
    }

    fn last_folder(&self, purpose: &str) -> Option<PathBuf> {
        lock(&self.inner).last_folder.get(purpose).cloned()
    }

    fn save(&self, request: PickRequest, bytes: Vec<u8>) -> PickId {
        self.enqueue(request, Some(bytes))
    }

    fn can_pick(&self) -> bool {
        true
    }

    fn browser_saves(&self) -> bool {
        false
    }
}

fn io_error(e: std::io::Error, path: &Path) -> FileError {
    match e.kind() {
        std::io::ErrorKind::NotFound => FileError::NotFound(path.to_owned()),
        std::io::ErrorKind::PermissionDenied => FileError::PermissionDenied(path.to_owned()),
        _ => FileError::Other(e.to_string()),
    }
}

/// Writes through a temporary file in the same folder, then renames it over the target.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), FileError> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let tmp = path.with_extension(format!("{ext}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|e| io_error(e, path))?;
    std::fs::rename(&tmp, path).map_err(|e| io_error(e, path))
}

fn matches_filter(filter: Option<&FileFilter>, name: &str) -> bool {
    let Some(filter) = filter else { return true };
    if filter.extensions.is_empty() || filter.extensions.iter().any(|e| e == "*") {
        return true;
    }
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    ext.is_some_and(|ext| filter.extensions.iter().any(|e| *e == ext))
}

fn list_dir(dir: &Path, kind: PickKind, filter: Option<&FileFilter>) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("Cannot read {}: no permission. In Android settings, allow WordCraft to manage all files.", dir.display())
        } else {
            format!("Cannot read {}: {e}", dir.display())
        }
    })?;
    let mut out = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = item.path();
        let is_dir = path.is_dir();
        if !is_dir && (kind == PickKind::Folder || !matches_filter(filter, &name)) {
            continue;
        }
        out.push(Entry { name, path, is_dir });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl Chooser {
    fn new(ticket: Ticket, dir: PathBuf) -> Self {
        let name = ticket.request.file_name.clone().unwrap_or_default();
        let mut chooser = Chooser { ticket, dir, entries: Vec::new(), error: None, name, selected: Vec::new(), filter: 0, replace: None };
        chooser.refresh();
        chooser
    }

    fn refresh(&mut self) {
        let kind = self.ticket.request.kind;
        match list_dir(&self.dir, kind, self.ticket.request.filters.get(self.filter)) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
            }
            Err(message) => {
                self.entries.clear();
                self.error = Some(message);
            }
        }
    }

    /// The path a Save would write: the typed name, with the suggested extension when it has none
    /// and a lowercase extension for a new file.
    fn save_path(&self) -> PathBuf {
        let mut name = self.name.trim().to_string();
        let existing = self.dir.join(&name).exists();
        if Path::new(&name).extension().is_none() {
            let request = &self.ticket.request;
            let ext = request
                .suggested_extension
                .clone()
                .or_else(|| request.filters.get(self.filter).and_then(|f| f.extensions.iter().find(|e| *e != "*").cloned()));
            if let Some(ext) = ext {
                name = format!("{name}.{ext}");
            }
        } else if !existing {
            let path = Path::new(&name);
            if let (Some(stem), Some(ext)) = (path.file_stem().and_then(|s| s.to_str()), path.extension().and_then(|e| e.to_str())) {
                name = format!("{stem}.{}", ext.to_ascii_lowercase());
            }
        }
        self.dir.join(name)
    }

    fn draw(&mut self, ctx: &egui::Context, roots: &[(String, PathBuf)]) -> Outcome {
        let request = self.ticket.request.clone();
        let kind = request.kind;
        let title = request.title.clone().unwrap_or_else(|| match kind {
            PickKind::Open | PickKind::OpenMany => "Open".to_string(),
            PickKind::Folder => "Choose a folder".to_string(),
            PickKind::Save => "Save as".to_string(),
        });

        let dir = self.dir.clone();
        let entries = self.entries.clone();
        let error = self.error.clone();
        let selected = self.selected.clone();
        let replacing = self.replace.clone();
        let mut name = self.name.clone();
        let mut filter = self.filter;

        let mut result: Option<Picked> = None;
        let mut go: Option<PathBuf> = None;
        let mut clicked: Option<PathBuf> = None;
        let mut double_clicked: Option<PathBuf> = None;
        let mut accept = false;
        let mut replace_answer: Option<bool> = None;

        let _ = egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .default_size([640.0, 520.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (label, path) in roots {
                        if ui.button(label.as_str()).clicked() {
                            go = Some(path.clone());
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Up").clicked()
                        && let Some(parent) = dir.parent()
                    {
                        go = Some(parent.to_path_buf());
                    }
                    ui.label(dir.display().to_string());
                });
                ui.separator();

                if let Some(path) = &replacing {
                    ui.label(format!("\"{}\" already exists. Replace it?", file_name_of(path)));
                    ui.horizontal(|ui| {
                        if ui.button("Replace").clicked() {
                            replace_answer = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            replace_answer = Some(false);
                        }
                    });
                    return;
                }

                if let Some(message) = &error {
                    ui.colored_label(egui::Color32::LIGHT_RED, message.as_str());
                }
                egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, false]).show(ui, |ui| {
                    for entry in &entries {
                        if entry.is_dir {
                            if ui.selectable_label(false, format!("{}/", entry.name)).clicked() {
                                go = Some(entry.path.clone());
                            }
                        } else {
                            let on = selected.contains(&entry.path);
                            let response = ui.selectable_label(on, entry.name.as_str());
                            if response.double_clicked() {
                                double_clicked = Some(entry.path.clone());
                            } else if response.clicked() {
                                clicked = Some(entry.path.clone());
                            }
                        }
                    }
                });
                ui.separator();

                if !request.filters.is_empty() {
                    let shown = request.filters[filter.min(request.filters.len() - 1)].name.clone();
                    ui.horizontal(|ui| {
                        ui.label("Type:");
                        egui::ComboBox::from_id_salt("android_file_filter").selected_text(shown).show_ui(ui, |ui| {
                            for (i, f) in request.filters.iter().enumerate() {
                                ui.selectable_value(&mut filter, i, f.name.as_str());
                            }
                        });
                    });
                }
                if kind == PickKind::Save {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        ui.text_edit_singleline(&mut name);
                    });
                }
                ui.horizontal(|ui| {
                    let (label, ready) = match kind {
                        PickKind::Open | PickKind::OpenMany => ("Open", !selected.is_empty()),
                        PickKind::Folder => ("Choose this folder", true),
                        PickKind::Save => ("Save", !name.trim().is_empty()),
                    };
                    if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
                        accept = true;
                    }
                    if ui.button("Cancel").clicked() {
                        result = Some(Picked::Cancelled);
                    }
                });
            });

        self.name = name;
        if filter != self.filter {
            self.filter = filter;
            self.refresh();
        }
        if let Some(path) = go {
            self.dir = path;
            self.selected.clear();
            self.refresh();
        }
        if let Some(path) = clicked {
            match kind {
                PickKind::OpenMany => {
                    if let Some(at) = self.selected.iter().position(|p| *p == path) {
                        self.selected.remove(at);
                    } else {
                        self.selected.push(path);
                    }
                }
                PickKind::Save => self.name = file_name_of(&path),
                _ => self.selected = vec![path],
            }
        }
        if let Some(path) = double_clicked {
            match kind {
                PickKind::Open | PickKind::OpenMany => result = Some(Picked::Paths(vec![path])),
                PickKind::Save => self.name = file_name_of(&path),
                PickKind::Folder => {}
            }
        }
        match replace_answer {
            Some(true) => {
                if let Some(path) = self.replace.take() {
                    result = Some(Picked::Paths(vec![path]));
                }
            }
            Some(false) => self.replace = None,
            None => {}
        }
        if accept {
            match kind {
                PickKind::Open | PickKind::OpenMany => {
                    if !self.selected.is_empty() {
                        result = Some(Picked::Paths(self.selected.clone()));
                    }
                }
                PickKind::Folder => result = Some(Picked::Paths(vec![self.dir.clone()])),
                PickKind::Save => {
                    let path = self.save_path();
                    if path.exists() {
                        self.replace = Some(path);
                    } else {
                        result = Some(Picked::Paths(vec![path]));
                    }
                }
            }
        }
        match result {
            Some(picked) => Outcome::Done(picked),
            None => Outcome::Open,
        }
    }
}
