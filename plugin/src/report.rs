//! A backup's report: a summary that fits on the screen, in a window the
//! program draws, its result in green (everything was backed up) or red
//! (not everything was), and the full report with every file, saved to a
//! text file when asked for.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

use lto_archive::{BackupOptions, BackupReport};
use sal_plugin_api::{tr, trf, Values};
use sal_window::egui::{self, Color32, RichText, ViewportCommand};
use sal_window::{Content, Windows};

use crate::size_text;

/// The window's width, in points (the program's error question is 560).
pub const WIDTH: f32 = 560.0;
/// The summary scrolls past this height, so the window never runs off the
/// screen.
const MAX_SUMMARY_HEIGHT: f32 = 360.0;
/// Tapes (written, failed) listed in the summary; the full report has all.
const FIRST_TAPES: usize = 10;

/// What a backup did, for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Everything found was backed up.
    pub ok: bool,
    /// The result, in one sentence or two.
    pub headline: String,
    /// What was found and written, the tapes, the tapes that failed, and
    /// how many files had each kind of problem (no file names).
    pub summary: Vec<String>,
    /// The whole report, every file listed, as a text file holds it.
    pub full: String,
    /// The name the full report is offered to be saved under.
    pub file_name: String,
}

impl Report {
    /// The headline and the summary as one text (a message, where the
    /// program can't show the window).
    pub fn short(&self) -> String {
        let mut lines = vec![self.headline.clone()];
        if !self.summary.is_empty() {
            lines.push(String::new());
            lines.extend(self.summary.iter().cloned());
        }
        lines.join("\n")
    }
}

/// The report of the backup of `src` that ended with `r`.
pub fn backup(r: &BackupReport, opts: BackupOptions, src: &Path) -> Report {
    let file_name = format!("{} - {}.txt", r.set.replace(['\\', '/'], " - "), tr("report"));
    if r.total_files == 0 && r.total_folders == 0 && opts.archive_only {
        let headline = tr("Nothing to back up: no file in the folder has the Archive attribute set.");
        let full = [tr("LTO Catalog: backup report"), trf("Folder: {folder}", &[("folder", &src.display())]), headline.clone()].join("\r\n") + "\r\n";
        return Report { ok: true, headline, summary: Vec::new(), full, file_name };
    }
    let (files, bytes) = r.written();
    let ok = r.error.is_none() && !r.cancelled && r.unreadable.is_empty() && r.too_big.is_empty() && files >= r.total_files;
    let headline = if let Some(e) = &r.error {
        trf("The backup stopped: {error}\n{files} of {total} files ({size}) were backed up into \"{set}\" before; they are on the tapes.", &[("error", e), ("files", &files), ("total", &r.total_files), ("size", &size_text(bytes)), ("set", &r.set)])
    } else if r.cancelled {
        trf("Cancelled. {files} of {total} files ({size}) were backed up into \"{set}\" before; they are on the tapes.", &[("files", &files), ("total", &r.total_files), ("size", &size_text(bytes)), ("set", &r.set)])
    } else if ok {
        trf("{files} files ({size}) backed up into \"{set}\".", &[("files", &files), ("size", &size_text(bytes)), ("set", &r.set)])
    } else {
        trf("{files} of {total} files ({size}) backed up into \"{set}\", but not everything: see below.", &[("files", &files), ("total", &r.total_files), ("size", &size_text(bytes)), ("set", &r.set)])
    };
    let found = trf("Found: {folders} folders, {files} files ({size}).", &[("folders", &r.total_folders), ("files", &r.total_files), ("size", &size_text(r.total_bytes))]);
    let tapes: Vec<String> = r.tapes.iter().map(|t| trf("{barcode}: {files} files, {size}", &[("barcode", &t.barcode), ("files", &t.files), ("size", &size_text(t.bytes))])).collect();
    let problems: [(String, Vec<String>); 4] = [
        (trf("Tapes that failed, noted as damaged in the catalog ({n}):", &[("n", &r.failed_tapes.len())]), r.failed_tapes.iter().map(|(b, why)| format!("{b}: {why}")).collect()),
        (trf("Left out, larger than a whole tape ({n}):", &[("n", &r.too_big.len())]), r.too_big.iter().map(|p| p.display().to_string()).collect()),
        (trf("Couldn't be read ({n}):", &[("n", &r.unreadable.len())]), r.unreadable.iter().map(|(p, why)| format!("{}: {why}", p.display())).collect()),
        (
            trf("Archive attribute left set: changed while backed up, or it can't be changed ({n}):", &[("n", &r.not_cleared.len())]),
            r.not_cleared.iter().map(|p| p.display().to_string()).collect(),
        ),
    ];

    // the summary: the tapes (and those that failed, with why), and how
    // many files had each kind of problem, not which: their names are in
    // the full report
    let mut summary = vec![found.clone()];
    let first = |out: &mut Vec<String>, items: &[String], n: usize| {
        out.extend(items.iter().take(n).map(|i| format!("  {i}")));
        if items.len() > n {
            out.push(format!("  {}", trf("… and {n} more (in the full report)", &[("n", &(items.len() - n))])));
        }
    };
    if !tapes.is_empty() {
        summary.push(trf("Tapes ({n}):", &[("n", &tapes.len())]));
        first(&mut summary, &tapes, FIRST_TAPES);
    }
    if !r.failed_tapes.is_empty() {
        summary.push(problems[0].0.clone());
        first(&mut summary, &problems[0].1, FIRST_TAPES);
    }
    let counts = [
        (r.too_big.len(), trf("Left out, larger than a whole tape: {n}", &[("n", &r.too_big.len())])),
        (r.unreadable.len(), trf("Couldn't be read: {n}", &[("n", &r.unreadable.len())])),
        (r.not_cleared.len(), trf("Archive attribute left set: {n}", &[("n", &r.not_cleared.len())])),
    ];
    if counts.iter().any(|(n, _)| *n > 0) {
        summary.push(String::new());
        summary.extend(counts.into_iter().filter(|(n, _)| *n > 0).map(|(_, line)| line));
        summary.push(tr("Which files they are: in the full report."));
    }

    // the full report: everything
    let what = if opts.archive_only { tr("only files with the Archive attribute") } else { tr("all files") };
    let clear = if opts.clear_archive { tr("yes") } else { tr("no") };
    let mut full = vec![
        tr("LTO Catalog: backup report"),
        String::new(),
        trf("Folder: {folder}", &[("folder", &src.display())]),
        trf("On the tapes: {set}", &[("set", &r.set)]),
        trf("Backed up: {what}; Archive attribute cleared: {clear}", &[("what", &what), ("clear", &clear)]),
        String::new(),
        if ok { tr("Result: everything was backed up.") } else { tr("Result: NOT everything was backed up.") },
        headline.clone(),
        found,
        trf("Written: {files} files ({size}).", &[("files", &files), ("size", &size_text(bytes))]),
    ];
    if !tapes.is_empty() {
        full.push(String::new());
        full.push(trf("Tapes ({n}):", &[("n", &tapes.len())]));
        full.extend(tapes.iter().map(|t| format!("  {t}")));
    }
    for (title, items) in problems.iter().filter(|(_, items)| !items.is_empty()) {
        full.push(String::new());
        full.push(title.clone());
        full.extend(items.iter().map(|i| format!("  {i}")));
    }
    let full = full.join("\r\n") + "\r\n";
    Report { ok, headline, summary, full, file_name }
}

/// The green of a backup that went well, readable on the theme's
/// background.
fn green(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(110, 205, 125)
    } else {
        Color32::from_rgb(25, 125, 50)
    }
}

/// The report in a window: the result in green or red, the summary, OK and
/// Save full report….
struct ReportWin {
    title: String,
    report: Report,
    /// The Save dialog is open (on a thread of its own): where it saved, or why not.
    saving: Option<Receiver<Result<Option<PathBuf>, String>>>,
    /// What saving did, said under the summary: (text, an error).
    saved: Option<(String, bool)>,
    /// The window was made a fixed dialog.
    dialog_set: bool,
}

impl Content for ReportWin {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn show(&mut self, ctx: &egui::Context) -> bool {
        use sal_ui::layout::{GAP, MARGIN};
        if let Some(rx) = &self.saving {
            if let Ok(done) = rx.try_recv() {
                self.saving = None;
                self.saved = match done {
                    Ok(Some(path)) => Some((trf("The full report was saved to {file}.", &[("file", &path.display())]), false)),
                    Ok(None) => None,
                    Err(e) => Some((trf("The report couldn't be saved: {error}", &[("error", &e)]), true)),
                };
            }
        }
        let busy = self.saving.is_some();
        let mut close = !busy && ctx.input(|i| i.key_pressed(egui::Key::Enter));
        if !self.dialog_set {
            self.dialog_set = true;
            ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
            ctx.send_viewport_cmd(ViewportCommand::EnableButtons { close: true, minimized: false, maximize: false });
        }
        let mut save = false;
        let height = egui::CentralPanel::default().frame(sal_ui::layout::dialog_frame(&ctx.style())).show(ctx, |ui| {
            let room = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 100_000.0));
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(room), |ui| {
                let color = if self.report.ok { green(ui.visuals().dark_mode) } else { ui.visuals().error_fg_color };
                let mark = if self.report.ok { "✔" } else { "✖" };
                ui.label(RichText::new(format!("{mark} {}", self.report.headline)).strong().color(color));
                if !self.report.summary.is_empty() {
                    ui.add_space(GAP);
                    egui::ScrollArea::vertical().max_height(MAX_SUMMARY_HEIGHT).auto_shrink([false, true]).show(ui, |ui| {
                        for line in &self.report.summary {
                            ui.label(line.as_str());
                        }
                    });
                }
                if let Some((text, error)) = &self.saved {
                    ui.add_space(GAP);
                    let color = if *error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                    ui.label(RichText::new(text).color(color));
                }
                sal_ui::layout::before_buttons(ui);
                ui.add_enabled_ui(!busy, |ui| match sal_ui::layout::buttons(ui, &[(&tr("OK"), ""), (&tr("Save full report…"), "")], Some(0)) {
                    Some(0) => close = true,
                    Some(_) => save = true,
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
        if save {
            self.saved = None;
            let (tx, rx) = channel();
            let (title, name, text, ctx) = (self.title.clone(), self.report.file_name.clone(), self.report.full.clone(), ctx.clone());
            // the Save dialog waits for the person: not on the program's thread
            let spawned = std::thread::Builder::new().name("save report".into()).spawn(move || {
                let _ = tx.send(save_as(&title, &name, &text));
                ctx.request_repaint();
            });
            if spawned.is_ok() {
                self.saving = Some(rx);
            }
        }
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        !close
    }
}

/// Ask where to save the full report (the Save dialog, over the report's
/// window titled `title`) and save it there, as UTF-8 with its mark so
/// every editor reads the diacritics. `None`: not saved, the dialog was
/// cancelled.
fn save_as(title: &str, name: &str, text: &str) -> Result<Option<PathBuf>, String> {
    let Some(path) = pick_save(title, &tr("Save the full report"), name) else { return Ok(None) };
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(text.as_bytes());
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

/// The standard Save dialog for a text file named `name`, owned by the
/// window titled `owner`; asks before replacing a file. Never in tests.
#[cfg(windows)]
fn pick_save(owner: &str, title: &str, name: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::UI::Controls::Dialogs::{GetSaveFileNameW, OFN_DONTADDTORECENT, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW};
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
    if cfg!(test) {
        return None;
    }
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let filter: Vec<u16> = format!("{} (*.txt)\0*.txt\0{} (*.*)\0*.*\0\0", tr("Text files"), tr("All files")).encode_utf16().collect();
    let (title, ext, owner) = (wide(title), wide("txt"), wide(owner));
    let mut file = vec![0u16; 32768];
    // a name Windows takes: no characters it forbids
    let safe: String = name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    for (i, c) in safe.encode_utf16().take(file.len() - 1).enumerate() {
        file[i] = c;
    }
    // SAFETY: the dialog reads the strings and writes the file name into
    // `file`, all alive and zero-terminated until it returns
    unsafe {
        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = FindWindowW(std::ptr::null(), owner.as_ptr());
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = file.as_mut_ptr();
        ofn.nMaxFile = file.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        ofn.lpstrDefExt = ext.as_ptr();
        ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT;
        if GetSaveFileNameW(&mut ofn) == 0 {
            return None;
        }
    }
    let n = file.iter().position(|&c| c == 0).unwrap_or(0);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&file[..n])))
}

#[cfg(not(windows))]
fn pick_save(_: &str, _: &str, _: &str) -> Option<PathBuf> {
    None
}

/// Show the report in a modal window, and return at once. False when the
/// program can't show the plugin's windows.
pub fn show(windows: &Windows, look: Values, report: Report) -> bool {
    let title = trf("Backup report: {set}", &[("set", &report.file_name.trim_end_matches(".txt"))]);
    let make: sal_window::Make = Box::new(move || Box::new(ReportWin { title, report, saving: None, saved: None, dialog_set: false }));
    windows.open_modal(make, look).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lto_archive::{InterimIndex, TapeDone};

    fn opts() -> BackupOptions {
        BackupOptions { archive_only: false, clear_archive: true, interim_index: InterimIndex::from_key("") }
    }

    /// A backup that left 300 files out: the summary names the first few,
    /// the full report every one; the result is red.
    #[test]
    fn the_summary_is_short_the_full_report_whole() {
        let r = BackupReport {
            set: "Backup 2026-11-16 10.00\\Data".into(),
            tapes: vec![TapeDone { barcode: "LTO001L5".into(), files: 700, bytes: 7 << 30 }],
            unreadable: (0..300).map(|i| (PathBuf::from(format!("D:\\Data\\locked {i}.db")), "access denied".into())).collect(),
            total_folders: 12,
            total_files: 1000,
            total_bytes: 10 << 30,
            ..BackupReport::default()
        };
        let rep = backup(&r, opts(), Path::new("D:\\Data"));
        assert!(!rep.ok);
        assert!(rep.headline.contains("700 of 1000 files"), "{}", rep.headline);
        assert!(rep.summary.len() < 10, "{:#?}", rep.summary);
        assert!(!rep.summary.iter().any(|l| l.contains("locked")), "no file names in the summary: {:#?}", rep.summary);
        assert!(rep.summary.iter().any(|l| l == "Couldn't be read: 300"), "{:#?}", rep.summary);
        assert!(rep.summary.iter().any(|l| l.contains("LTO001L5: 700 files")), "the tapes are: {:#?}", rep.summary);
        assert!(rep.summary[0].contains("12 folders, 1000 files"));
        for i in 0..300 {
            assert!(rep.full.contains(&format!("locked {i}.db: access denied\r\n")), "{i}");
        }
        assert!(rep.full.contains("NOT everything"));
        assert_eq!(rep.file_name, "Backup 2026-11-16 10.00 - Data - report.txt");
    }

    /// Everything backed up: green; a failed tape the backup went on past
    /// is listed but doesn't make it red.
    #[test]
    fn everything_backed_up_is_ok() {
        let r = BackupReport {
            set: "Backup 2026-11-16 10.00\\Data".into(),
            tapes: vec![TapeDone { barcode: "LTO002L5".into(), files: 5, bytes: 500 }],
            failed_tapes: vec![("LTO001L5".into(), "medium error".into())],
            total_files: 5,
            total_bytes: 500,
            ..BackupReport::default()
        };
        let rep = backup(&r, opts(), Path::new("D:\\Data"));
        assert!(rep.ok);
        assert!(rep.short().contains("LTO001L5: medium error"));
        let stopped = backup(&BackupReport { error: Some("drive gone".into()), ..r.clone() }, opts(), Path::new("D:\\Data"));
        assert!(!stopped.ok && stopped.headline.contains("drive gone"));
        assert!(!backup(&BackupReport { cancelled: true, ..r }, opts(), Path::new("D:\\Data")).ok);
    }

    /// The window: fixed, never taller than the screen however long the
    /// report, the result in green or red.
    #[test]
    fn the_window_fits_the_screen() {
        let r = BackupReport {
            set: "Backup 2026-11-16 10.00\\Data".into(),
            tapes: (0..40).map(|i| TapeDone { barcode: format!("LTO{i:03}L5"), files: 1, bytes: 1 }).collect(),
            unreadable: (0..5000).map(|i| (PathBuf::from(format!("D:\\Data\\{i}")), "denied".into())).collect(),
            not_cleared: (0..5000).map(|i| PathBuf::from(format!("D:\\Data\\c{i}"))).collect(),
            total_files: 40,
            ..BackupReport::default()
        };
        for (report, dark) in [(backup(&r, opts(), Path::new("D:\\Data")), true), (backup(&BackupReport { unreadable: Vec::new(), ..r.clone() }, opts(), Path::new("D:\\Data")), false)] {
            let ok = report.ok;
            let mut w = ReportWin { title: "Backup report".into(), report, saving: None, saved: None, dialog_set: false };
            let ctx = egui::Context::default();
            ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
            sal_ui::layout::apply(&ctx);
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH, 300.0))), ..Default::default() };
            let out = ctx.run(input, |ctx| assert!(w.show(ctx)));
            let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
            assert!(cmds.contains(&ViewportCommand::Resizable(false)));
            let size = cmds.iter().find_map(|c| if let ViewportCommand::InnerSize(s) = c { Some(*s) } else { None }).expect("sized to the content");
            assert!(size.y < MAX_SUMMARY_HEIGHT + 200.0, "{size:?}");
            // the headline's color
            let want = if ok { green(dark) } else { ctx.style().visuals.error_fg_color };
            let colors: Vec<Color32> = out
                .shapes
                .iter()
                .filter_map(|s| if let egui::epaint::Shape::Text(t) = &s.shape { Some(t.galley.job.sections.first().map(|s| s.format.color)?) } else { None })
                .collect();
            assert!(colors.contains(&want), "ok {ok}: {colors:?}");
            // Enter closes it
            let enter = egui::RawInput { events: vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }], ..Default::default() };
            let _ = ctx.run(enter, |ctx| assert!(!w.show(ctx)));
        }
    }
}
