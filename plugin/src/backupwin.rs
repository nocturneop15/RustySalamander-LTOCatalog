//! The Back up or archive dialog, in a window the program draws: what to
//! do, the options, deleting (an archive), the files and folders, the tapes.
//! The command's thread waits for the choice, then runs the job (its steps
//! and Cancel in the status bar).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use lto_archive::{BackupOptions, DeleteSource, InterimIndex};
use sal_plugin_api::{tr, trf, Values};
use sal_window::egui::{self, RichText, ViewportCommand};
use sal_window::{Content, Windows};

/// The window's width, in points.
pub const WIDTH: f32 = 640.0;
/// The lists of files and folders, and of tapes, scroll past this height.
const LIST_HEIGHT: f32 = 120.0;
/// Room for the date's tick box beside the name.
const NAME_ROW_CHECK: f32 = 230.0;

/// The folder on the tapes when no name is given (as before names could
/// be chosen; not translated: it is a folder on the tapes).
pub fn default_name(archive: bool) -> &'static str {
    if archive {
        "Archive"
    } else {
        "Backup"
    }
}

/// The name typed for the folder on the tapes, checked: `\` makes folders
/// in it; empty: [`default_name`]. Why not, for people.
pub fn folder_name(name: &str, archive: bool) -> Result<String, String> {
    let trimmed = name.trim().trim_matches(['\\', '/']);
    let name = if trimmed.is_empty() { default_name(archive) } else { trimmed };
    let parts: Vec<&str> = name.split(['\\', '/']).map(str::trim).collect();
    let bad = |p: &&str| p.is_empty() || *p == "." || *p == ".." || p.ends_with('.') || p.chars().any(|c| c.is_control() || "<>:\"|?*".contains(c));
    if parts.iter().any(bad) {
        let shown: String = name.chars().filter(|c| !c.is_control()).take(100).collect();
        return Err(trf("\"{name}\" can't be a folder name on a tape: no empty parts, no < > : \" | ? * and no dot at the end.", &[("name", &shown)]));
    }
    Ok(parts.join("\\"))
}

/// The folder on the tapes for `items` under the folder `top`: a folder
/// alone, or items of one folder, under that folder's name; items from
/// several folders right in it.
pub fn target(top: &str, items: &[PathBuf]) -> String {
    let parents: std::collections::BTreeSet<&Path> = items.iter().filter_map(|p| p.parent()).collect();
    match items {
        [one] if one.is_dir() => lto_archive::set_folder(top, one),
        _ if parents.len() == 1 => lto_archive::set_folder(top, parents.first().expect("one")),
        _ => top.to_owned(),
    }
}

/// How an archive deletes the files from disk once they are on the tapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Without asking, as soon as they are on the tapes (with the check,
    /// those read back the same).
    Auto,
    /// Without asking, once they were read back the same: the check is on.
    AutoChecked,
    /// After the person confirms, once everything is written (and checked).
    Ask,
}

impl DeleteMode {
    /// As the form's fallback keeps it.
    pub fn key(self) -> &'static str {
        match self {
            DeleteMode::Auto => "auto",
            DeleteMode::AutoChecked => "checked",
            DeleteMode::Ask => "ask",
        }
    }

    /// From the form's fallback; anything else asks.
    pub fn from_key(k: &str) -> DeleteMode {
        match k {
            "auto" => DeleteMode::Auto,
            "checked" => DeleteMode::AutoChecked,
            _ => DeleteMode::Ask,
        }
    }
}

/// What the dialog was given: the files and folders (those selected in the
/// panel, or its folder), and the tapes.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub items: Vec<PathBuf>,
    /// (barcode, label, ticked)
    pub tapes: Vec<(String, String, bool)>,
    /// Why no tape is listed (none in a library, or the error reading them).
    pub tapes_note: Option<String>,
}

/// What the person chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub archive: bool,
    pub archive_only: bool,
    pub clear: bool,
    pub verify: bool,
    pub delete: DeleteMode,
    /// The folder on the tapes, as typed (empty: [`default_name`]).
    pub name: String,
    /// The date and time appended to it.
    pub dated: bool,
    pub items: Vec<PathBuf>,
    pub tapes: Vec<String>,
}

impl Choice {
    /// The job's options. Deleting once checked turns the check on.
    pub fn options(&self, interim_index: InterimIndex, read_ahead: u64) -> BackupOptions {
        let delete = match (self.archive, self.delete) {
            (false, _) => DeleteSource::Keep,
            (true, DeleteMode::Ask) => DeleteSource::Ask,
            (true, _) => DeleteSource::Auto,
        };
        let verify = self.verify || (self.archive && self.delete == DeleteMode::AutoChecked);
        BackupOptions { archive_only: self.archive_only, clear_archive: self.clear, interim_index, verify, delete, read_ahead }
    }
}

/// The dialog.
struct BackupWin {
    title: String,
    archive: bool,
    archive_only: bool,
    clear: bool,
    verify: bool,
    delete: DeleteMode,
    /// The folder on the tapes; it follows Back up / Archive until edited.
    name: String,
    name_edited: bool,
    dated: bool,
    items: Vec<(PathBuf, bool, bool)>,
    tapes: Vec<(String, String, bool)>,
    tapes_note: Option<String>,
    /// Why Start can't start yet.
    error: Option<String>,
    /// The Browse dialog is open (on a thread of its own): what was chosen.
    browsing: Option<Receiver<Option<PathBuf>>>,
    tx: Option<Sender<Option<Choice>>>,
    dialog_set: bool,
}

impl BackupWin {
    fn new(title: String, setup: Setup, tx: Sender<Option<Choice>>) -> BackupWin {
        BackupWin {
            title,
            archive: false,
            archive_only: false,
            clear: true,
            verify: false,
            delete: DeleteMode::Ask,
            name: default_name(false).into(),
            name_edited: false,
            dated: true,
            items: setup.items.into_iter().map(|p| (p.clone(), true, p.is_dir())).collect(),
            tapes: setup.tapes,
            tapes_note: setup.tapes_note,
            error: None,
            browsing: None,
            tx: Some(tx),
            dialog_set: false,
        }
    }

    fn answer(&mut self, c: Option<Choice>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(c);
        }
    }

    /// What was chosen, or why it can't start.
    fn choice(&self) -> Result<Choice, String> {
        let items: Vec<PathBuf> = self.items.iter().filter(|(_, on, _)| *on).map(|(p, _, _)| p.clone()).collect();
        if items.is_empty() {
            return Err(tr("Tick at least one file or folder, or add one with Browse…"));
        }
        folder_name(&self.name, self.archive)?;
        Ok(Choice {
            archive: self.archive,
            archive_only: self.archive_only,
            clear: self.clear,
            verify: self.verify || (self.archive && self.delete == DeleteMode::AutoChecked),
            delete: self.delete,
            name: self.name.clone(),
            dated: self.dated,
            items,
            tapes: self.tapes.iter().filter(|(_, _, on)| *on).map(|(b, _, _)| b.clone()).collect(),
        })
    }

    /// A path chosen with Browse: added ticked, or ticked when listed.
    fn add(&mut self, p: PathBuf) {
        match self.items.iter_mut().find(|(q, _, _)| q.as_os_str().eq_ignore_ascii_case(p.as_os_str())) {
            Some(it) => it.1 = true,
            None => {
                let dir = p.is_dir();
                self.items.push((p, true, dir));
            }
        }
        self.error = None;
    }
}

impl Drop for BackupWin {
    /// Closed by Esc or the close button, or the plugin stopped: nothing.
    fn drop(&mut self) {
        self.answer(None);
    }
}

/// A section's title over its group box.
fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(sal_ui::layout::GAP);
    ui.label(RichText::new(title).strong());
    ui.add_space(sal_ui::layout::GAP_SMALL);
    sal_ui::layout::group_frame(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

impl Content for BackupWin {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn show(&mut self, ctx: &egui::Context) -> bool {
        use sal_ui::layout::{GAP_SMALL, MARGIN};
        if let Some(rx) = &self.browsing {
            if let Ok(chosen) = rx.try_recv() {
                self.browsing = None;
                if let Some(p) = chosen {
                    self.add(p);
                }
            }
        }
        let busy = self.browsing.is_some();
        if !self.dialog_set {
            self.dialog_set = true;
            ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
            ctx.send_viewport_cmd(ViewportCommand::EnableButtons { close: true, minimized: false, maximize: false });
        }
        let mut start = !busy && ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let mut cancel = false;
        let mut browse = false;
        // never taller than the screen: room for the title bar, the buttons
        // and the taskbar
        let cap = ctx.input(|i| i.viewport().monitor_size).map(|m| (m.y - 240.0).max(300.0)).unwrap_or(f32::INFINITY);
        // the name follows Back up / Archive until it is edited
        if !self.name_edited {
            self.name = default_name(self.archive).into();
        }
        let preview = match folder_name(&self.name, self.archive) {
            Ok(top) => {
                let top = if self.dated { lto_archive::dated(&top) } else { top };
                let ticked: Vec<PathBuf> = self.items.iter().filter(|(_, on, _)| *on).map(|(p, _, _)| p.clone()).collect();
                trf("On the tapes: {folder}", &[("folder", &target(&top, &ticked))])
            }
            Err(e) => e,
        };
        let height = egui::CentralPanel::default().frame(sal_ui::layout::dialog_frame(&ctx.style())).show(ctx, |ui| {
            let room = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 100_000.0));
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(room), |ui| {
                // taller than the screen (a large font): the sections scroll,
                // the buttons stay
                egui::ScrollArea::vertical().id_salt("form").max_height(cap).auto_shrink([false, true]).show(ui, |ui| ui.add_enabled_ui(!busy, |ui| {
                    sal_ui::layout::controls(ui);
                    section(ui, &tr("Do"), |ui| {
                        ui.radio_value(&mut self.archive, false, tr("Back up: the files stay on disk"));
                        ui.radio_value(&mut self.archive, true, tr("Archive: the files will be deleted from their original location once they are on the tapes"));
                    });
                    section(ui, &tr("Name"), |ui| {
                        // the name and the date on one row
                        ui.horizontal(|ui| {
                            let room = (ui.available_width() - NAME_ROW_CHECK).max(120.0);
                            let field = ui.add(sal_ui::layout::text_field(&mut self.name).desired_width(room)).on_hover_text(tr("The folder on the tapes (\\ makes folders in it)"));
                            if field.changed() {
                                self.name_edited = true;
                                self.error = None;
                            }
                            ui.checkbox(&mut self.dated, tr("Append the date and time"));
                        });
                        ui.label(RichText::new(&preview).weak());
                    });
                    section(ui, &tr("Options"), |ui| {
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut self.archive_only, false, tr("All files"));
                            ui.radio_value(&mut self.archive_only, true, tr("Only files with the Archive attribute"));
                        });
                        ui.checkbox(&mut self.clear, tr("Clear the Archive attribute of the files backed up"));
                        // deleting once checked needs the check: on, and stays on
                        let forced = self.archive && self.delete == DeleteMode::AutoChecked;
                        if forced {
                            self.verify = true;
                        }
                        ui.add_enabled(!forced, egui::Checkbox::new(&mut self.verify, tr("Check the data right after it is written: each tape's files are read back and compared with the files on disk")));
                    });
                    section(ui, &tr("Delete"), |ui| {
                        ui.add_enabled_ui(self.archive, |ui| {
                            ui.radio_value(&mut self.delete, DeleteMode::Auto, tr("Delete without asking, as soon as the files are on the tapes"));
                            ui.radio_value(&mut self.delete, DeleteMode::AutoChecked, tr("Delete without asking, once the data is checked (turns Check the data on)"));
                            ui.radio_value(&mut self.delete, DeleteMode::Ask, tr("Ask before deleting, once everything is written"));
                            ui.add_space(GAP_SMALL);
                            ui.label(RichText::new(tr("Deleted for good, not moved to the Recycle Bin, and so are the folders left empty. A file that wasn't read back the same, or changed meanwhile, stays.")).weak());
                        });
                    });
                    section(ui, &tr("Files and folders"), |ui| {
                        if self.items.is_empty() {
                            ui.label(RichText::new(tr("Nothing yet: add files or folders with Browse…")).weak());
                        } else {
                            egui::ScrollArea::vertical().id_salt("items").max_height(LIST_HEIGHT).auto_shrink([false, true]).show(ui, |ui| {
                                for (p, on, dir) in &mut self.items {
                                    let shown = if *dir { format!("{}\\", p.display().to_string().trim_end_matches('\\')) } else { p.display().to_string() };
                                    if ui.checkbox(on, shown).changed() {
                                        self.error = None;
                                    }
                                }
                            });
                        }
                        ui.add_space(GAP_SMALL);
                        if ui.button(tr("Browse…")).clicked() {
                            browse = true;
                        }
                    });
                    section(ui, &tr("Tapes"), |ui| {
                        ui.label(tr("Tapes to use, filled in barcode order:"));
                        if let Some(n) = &self.tapes_note {
                            ui.label(RichText::new(n).weak());
                        }
                        if !self.tapes.is_empty() {
                            egui::ScrollArea::vertical().id_salt("tapes").max_height(LIST_HEIGHT).auto_shrink([false, true]).show(ui, |ui| {
                                for (_, label, on) in &mut self.tapes {
                                    ui.checkbox(on, label.as_str());
                                }
                            });
                        }
                    });
                    ui.add_space(GAP_SMALL);
                    ui.label(RichText::new(tr("When the tapes are full, a new tape is asked for; a blank one is formatted after you confirm. A file larger than a whole tape is left out. Without the date, a later backup of the same name goes into the same folder and replaces its files of the same name.")).weak());
                }));
                if let Some(e) = &self.error {
                    ui.add_space(GAP_SMALL);
                    ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
                }
                sal_ui::layout::before_buttons(ui);
                ui.add_enabled_ui(!busy, |ui| match sal_ui::layout::buttons(ui, &[(&tr("Start"), ""), (&tr("Cancel"), "")], Some(0)) {
                    Some(0) => start = true,
                    Some(_) => cancel = true,
                    None => {}
                });
            })
            .response
            .rect
            .height()
        });
        let want = egui::vec2(WIDTH, (height.inner + 2.0 * MARGIN).ceil());
        let have = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());
        if !have.is_some_and(|h| (h.y - want.y).abs() <= 2.0 && (h.x - want.x).abs() <= 2.0) {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(want));
        }
        if browse {
            let (tx, rx) = channel();
            let (title, ctx) = (self.title.clone(), ctx.clone());
            // the Browse dialog waits for the person: not on the program's thread
            let spawned = std::thread::Builder::new().name("browse".into()).spawn(move || {
                let _ = tx.send(pick_item(&title, &tr("Add a file or folder")));
                ctx.request_repaint();
            });
            if spawned.is_ok() {
                self.browsing = Some(rx);
            }
        }
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        if cancel {
            self.answer(None);
            return false;
        }
        if start {
            match self.choice() {
                Ok(c) => {
                    self.answer(Some(c));
                    return false;
                }
                Err(e) => self.error = Some(e),
            }
        }
        true
    }
}

/// The shell's Browse dialog, which picks a file or a folder, owned by the
/// window titled `owner`. Never in tests.
#[cfg(windows)]
fn pick_item(owner: &str, title: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::{SHBrowseForFolderW, SHGetPathFromIDListW, BIF_BROWSEINCLUDEFILES, BIF_NEWDIALOGSTYLE, BIF_NONEWFOLDERBUTTON, BROWSEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
    if cfg!(test) {
        return None;
    }
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (owner, title) = (wide(owner), wide(title));
    let mut name = vec![0u16; 260];
    let mut path = vec![0u16; 32768];
    // SAFETY: the dialog reads the strings and writes into the buffers, all
    // alive until it returns; the item list it returns is freed here
    unsafe {
        let com = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut bi: BROWSEINFOW = std::mem::zeroed();
        bi.hwndOwner = FindWindowW(std::ptr::null(), owner.as_ptr());
        bi.pszDisplayName = name.as_mut_ptr();
        bi.lpszTitle = title.as_ptr();
        bi.ulFlags = BIF_BROWSEINCLUDEFILES | BIF_NEWDIALOGSTYLE | BIF_NONEWFOLDERBUTTON;
        let pidl = SHBrowseForFolderW(&bi);
        let mut out = None;
        if !pidl.is_null() {
            if SHGetPathFromIDListW(pidl, path.as_mut_ptr()) != 0 {
                let n = path.iter().position(|&c| c == 0).unwrap_or(0);
                out = Some(PathBuf::from(std::ffi::OsString::from_wide(&path[..n])));
            }
            CoTaskMemFree(pidl as *const _);
        }
        if com >= 0 {
            CoUninitialize();
        }
        out.filter(|p| !p.as_os_str().is_empty())
    }
}

#[cfg(not(windows))]
fn pick_item(_: &str, _: &str) -> Option<PathBuf> {
    None
}

/// Show the dialog, modal, and wait for the choice: `Some(None)` when it
/// was cancelled; `None` when the program can't show the plugin's windows.
pub fn ask(windows: &Windows, look: Values, setup: Setup) -> Option<Option<Choice>> {
    let (tx, rx) = channel();
    let title = tr("Back up or archive");
    let make: sal_window::Make = Box::new(move || Box::new(BackupWin::new(title, setup, tx)));
    windows.open_modal(make, look).ok()?;
    Some(rx.recv().ok().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(items: Vec<PathBuf>) -> (BackupWin, Receiver<Option<Choice>>) {
        let (tx, rx) = channel();
        let setup = Setup { items, tapes: vec![("LTO001L5".into(), "LTO001L5 (free 1.4 TB)".into(), true), ("LTO002L5".into(), "LTO002L5".into(), false)], tapes_note: None };
        (BackupWin::new("Back up or archive".into(), setup, tx), rx)
    }

    fn frame(w: &mut BackupWin, ctx: &egui::Context, events: Vec<egui::Event>) -> (bool, egui::FullOutput) {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH, 900.0))), events, ..Default::default() };
        let mut open = true;
        let out = ctx.run(input, |ctx| open = w.show(ctx));
        (open, out)
    }

    /// Deleting once checked turns the check on; a backup keeps the files
    /// whatever the delete choice says.
    #[test]
    fn the_options_follow_the_choice() {
        let c = Choice { archive: true, archive_only: false, clear: true, verify: false, delete: DeleteMode::AutoChecked, name: String::new(), dated: true, items: vec![], tapes: vec![] };
        let o = c.options(InterimIndex::DataPartition, 0);
        assert!(o.verify && o.delete == DeleteSource::Auto);
        let o = Choice { delete: DeleteMode::Auto, ..c.clone() }.options(InterimIndex::DataPartition, 0);
        assert!(!o.verify && o.delete == DeleteSource::Auto);
        let o = Choice { delete: DeleteMode::Ask, verify: true, ..c.clone() }.options(InterimIndex::DataPartition, 0);
        assert!(o.verify && o.delete == DeleteSource::Ask);
        let o = Choice { archive: false, ..c }.options(InterimIndex::DataPartition, 0);
        assert!(!o.verify && o.delete == DeleteSource::Keep);
    }

    /// The dialog is fixed and fits its content; Enter starts with what is
    /// ticked; with nothing ticked it says so and stays open.
    #[test]
    fn enter_starts_with_what_is_ticked() {
        let ctx = egui::Context::default();
        sal_ui::layout::apply(&ctx);
        let (mut w, rx) = win(vec![PathBuf::from("C:\\data\\a.txt"), PathBuf::from("C:\\data\\b.txt")]);
        let (open, out) = frame(&mut w, &ctx, vec![]);
        assert!(open);
        let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(cmds.contains(&ViewportCommand::Resizable(false)));
        let size = cmds.iter().find_map(|c| if let ViewportCommand::InnerSize(s) = c { Some(*s) } else { None }).expect("sized to the content");
        assert!(size.y > 300.0 && size.y < 900.0, "{size:?}");
        let enter = || vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }];
        // nothing ticked: refused, still open
        for it in &mut w.items {
            it.1 = false;
        }
        let (open, _) = frame(&mut w, &ctx, enter());
        assert!(open && w.error.is_some());
        w.items[1].1 = true;
        w.archive = true;
        w.delete = DeleteMode::AutoChecked;
        let (open, _) = frame(&mut w, &ctx, enter());
        assert!(!open);
        let c = rx.recv().unwrap().expect("started");
        assert_eq!(c.items, [PathBuf::from("C:\\data\\b.txt")]);
        assert_eq!(c.tapes, ["LTO001L5"]);
        assert!(c.archive && c.verify, "deleting once checked checks: {c:?}");
        assert_eq!((c.name.as_str(), c.dated), ("Archive", true), "the name followed Archive");
    }

    /// Names for the folder on the tapes: empty is Backup or Archive, `\\`
    /// makes folders, what Windows forbids is refused; where items go.
    #[test]
    fn names_of_the_folder_on_the_tapes() {
        assert_eq!(folder_name("  ", false).unwrap(), "Backup");
        assert_eq!(folder_name("", true).unwrap(), "Archive");
        assert_eq!(folder_name(" Projekty \\ 2026 \\", false).unwrap(), "Projekty\\2026");
        for bad in ["a:b", "x\\\\y", "..", "end.", "q?"] {
            assert!(folder_name(bad, false).is_err(), "{bad}");
        }
        let dir = std::env::temp_dir();
        assert_eq!(target("T", &[dir.join("a.txt"), dir.join("b.txt")]), format!("T\\{}", dir.file_name().unwrap().to_string_lossy()));
        assert_eq!(target("T", &[PathBuf::from("C:\\x\\a"), PathBuf::from("D:\\y\\b")]), "T");
    }

    /// On a small screen the sections scroll and the window fits, with
    /// its buttons.
    #[test]
    fn it_fits_a_small_screen() {
        let ctx = egui::Context::default();
        sal_ui::layout::apply(&ctx);
        let many: Vec<PathBuf> = (0..40).map(|i| PathBuf::from(format!("C:\\data\\f{i}.txt"))).collect();
        let (mut w, _rx) = win(many);
        let mut input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH, 600.0))), ..Default::default() };
        input.viewports.insert(egui::ViewportId::ROOT, egui::ViewportInfo { monitor_size: Some(egui::vec2(1366.0, 768.0)), ..Default::default() });
        let out = ctx.run(input, |ctx| assert!(w.show(ctx)));
        let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
        let size = cmds.iter().find_map(|c| if let ViewportCommand::InnerSize(s) = c { Some(*s) } else { None }).expect("sized");
        assert!(size.y <= 768.0 - 80.0, "{size:?}");
    }

    /// Closing it is Cancel.
    #[test]
    fn closing_it_cancels() {
        let (w, rx) = win(vec![]);
        drop(w);
        assert_eq!(rx.recv().unwrap(), None);
    }
}
