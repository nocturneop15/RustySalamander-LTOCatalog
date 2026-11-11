//! The question a job asks while it runs ("Insert tape …"), in a small
//! window the program draws in its own look. The job's thread waits for the
//! answer.

use std::sync::mpsc::{channel, Sender};

use sal_plugin_api::Values;
use sal_window::egui::{self, RichText};
use sal_window::{Content, Windows};

/// A question with buttons.
struct AskWin {
    title: String,
    text: String,
    detail: String,
    buttons: Vec<String>,
    /// The button Enter presses.
    default: usize,
    /// What closing the window without a button means.
    dismiss: usize,
    tx: Option<Sender<usize>>,
}

impl AskWin {
    fn answer(&mut self, i: usize) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(i);
        }
    }
}

impl Drop for AskWin {
    /// Closed by Esc, F10 or the close button, or the plugin stopped.
    fn drop(&mut self) {
        let d = self.dismiss;
        self.answer(d);
    }
}

impl Content for AskWin {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn show(&mut self, ctx: &egui::Context) -> bool {
        let mut chosen = None;
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            chosen = Some(self.default);
        }
        egui::CentralPanel::default().frame(sal_ui::layout::dialog_frame(&ctx.style())).show(ctx, |ui| {
            ui.label(RichText::new(&self.text).strong());
            if !self.detail.is_empty() {
                ui.add_space(sal_ui::layout::GAP_SMALL);
                ui.label(&self.detail);
            }
            sal_ui::layout::before_buttons(ui);
            let labels: Vec<(&str, &str)> = self.buttons.iter().map(|b| (b.as_str(), "")).collect();
            if let Some(i) = sal_ui::layout::buttons(ui, &labels, Some(self.default)) {
                chosen = Some(i);
            }
        });
        match chosen {
            Some(i) => {
                self.answer(i);
                false
            }
            None => true,
        }
    }
}

/// Ask, and wait for the answer: the index of the button pressed;
/// `dismiss` when the window was closed. `None` when the program can't show
/// the plugin's windows.
pub fn ask(windows: &Windows, look: Values, title: String, text: String, detail: String, buttons: Vec<String>, default: usize, dismiss: usize) -> Option<usize> {
    let (tx, rx) = channel();
    let make: sal_window::Make = Box::new(move || Box::new(AskWin { title, text, detail, buttons, default, dismiss, tx: Some(tx) }));
    windows.open(make, look).ok()?;
    rx.recv().ok()
}
