//! The question a job asks while it runs ("Insert tape …"), in a small
//! window the program draws in its own look. The job's thread waits for the
//! answer.

use std::sync::mpsc::{channel, Sender};

use sal_plugin_api::Values;
use sal_window::egui::{self, RichText, ViewportCommand};
use sal_window::{Content, Windows};

/// The question's width, in points (the program's error question is 560,
/// its smaller questions 480).
pub const WIDTH: f32 = 480.0;

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
    /// The window was made a fixed dialog (not resizable, no minimize or
    /// maximize button).
    dialog_set: bool,
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
        use sal_ui::layout::{GAP, MARGIN};
        let mut chosen = None;
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            chosen = Some(self.default);
        }
        // a dialog as the program's (dialogwin): a fixed width, the height of
        // its content, not resizable, no minimize or maximize button
        if !self.dialog_set {
            self.dialog_set = true;
            ctx.send_viewport_cmd(ViewportCommand::Resizable(false));
            ctx.send_viewport_cmd(ViewportCommand::EnableButtons { close: true, minimized: false, maximize: false });
        }
        let height = egui::CentralPanel::default().frame(sal_ui::layout::dialog_frame(&ctx.style())).show(ctx, |ui| {
            // measured in a room as tall as it wants, not in the window as it
            // is (the panel always fills it)
            let room = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 100_000.0));
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(room), |ui| {
                ui.label(RichText::new(&self.text).strong());
                if !self.detail.is_empty() {
                    ui.add_space(GAP);
                    ui.label(&self.detail);
                }
                sal_ui::layout::before_buttons(ui);
                let labels: Vec<(&str, &str)> = self.buttons.iter().map(|b| (b.as_str(), "")).collect();
                if let Some(i) = sal_ui::layout::buttons(ui, &labels, Some(self.default)) {
                    chosen = Some(i);
                }
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
    let make: sal_window::Make = Box::new(move || Box::new(AskWin { title, text, detail, buttons, default, dismiss, tx: Some(tx), dialog_set: false }));
    windows.open(make, look).ok()?;
    rx.recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The question is a dialog as the program's: fixed, as tall as its
    /// text and buttons, with the margins of `sal_ui::layout`.
    #[test]
    fn the_question_fits_its_content() {
        let (tx, _rx) = channel();
        let mut w = AskWin {
            title: "Insert tape".into(),
            text: "Tape 301AFGL5 is not in the library. Put it into the mail slot (or a magazine), then press Retry.".into(),
            detail: "Volume: 302AFGL5".into(),
            buttons: vec!["Retry".into(), "Skip this tape".into(), "Cancel".into()],
            default: 0,
            dismiss: 2,
            tx: Some(tx),
            dialog_set: false,
        };
        let ctx = egui::Context::default();
        sal_ui::layout::apply(&ctx);
        let input = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WIDTH, 210.0))), ..Default::default() };
        let out = ctx.run(input(), |ctx| assert!(w.show(ctx)));
        let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(cmds.contains(&ViewportCommand::Resizable(false)), "{cmds:?}");
        assert!(cmds.contains(&ViewportCommand::EnableButtons { close: true, minimized: false, maximize: false }), "{cmds:?}");
        let size = cmds.iter().find_map(|c| if let ViewportCommand::InnerSize(s) = c { Some(*s) } else { None }).expect("sized to the content");
        assert_eq!(size.x, WIDTH);
        // two lines of text, a detail line, the buttons, the margins
        assert!(size.y > 90.0 && size.y < 160.0, "{size:?}");
        // once set, the next frame asks for nothing more
        let out = ctx.run(egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..input() }, |ctx| assert!(w.show(ctx)));
        let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(!cmds.iter().any(|c| matches!(c, ViewportCommand::Resizable(_))), "{cmds:?}");
    }
}
