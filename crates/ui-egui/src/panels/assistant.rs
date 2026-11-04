//! The AI Assistant window: a chat with Claude, which works on the open session through the same
//! commands as the menus, shortcuts, control channel and MCP server.
//!
//! The conversation runs on its own thread (`soundcraft-assistant`). Each tool call it makes comes
//! back to the UI thread as a [`ControlRequest`] and is handled by `control::drain` exactly like a
//! control-channel request, so the assistant can do nothing a script couldn't, every change lands
//! in the undo history, and the audio thread is never touched.
//!
//! The web build has no assistant (no blocking HTTPS, no Keychain): the window says so, and the
//! chat code below is compiled out there.
#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]

use crate::SoundApp;
use crate::control::ControlRequest;
use crate::theme::{Tokens, bold, mono, regular};
use egui::{Color32, RichText, vec2};
use serde_json::json;
use std::sync::mpsc::Receiver;

/// One entry in the chat log.
enum Line {
    User(String),
    Claude(String),
    /// A tool call and, once known, whether it succeeded.
    Tool(String, Option<bool>),
    Note(String),
    Error(String),
}

/// The undo steps one reply added: the history length before it and, once it ended, after it.
struct UndoSpan {
    start: usize,
    end: Option<usize>,
}

#[derive(Default)]
pub struct AssistantState {
    pub show: bool,
    /// Requests from the assistant's thread, drained on the UI thread with the control channel's.
    pub(crate) requests: Option<Receiver<ControlRequest>>,
    #[cfg(not(target_arch = "wasm32"))]
    chat: Option<soundcraft_assistant::Assistant>,
    /// The API key in use and where it came from, once looked up.
    key: Option<(String, &'static str)>,
    key_looked_up: bool,
    key_input: String,
    settings_open: bool,
    model: usize,
    log: Vec<Line>,
    input: String,
    undo: Option<UndoSpan>,
}

/// UI commands handled here (`window.assistant`).
pub fn run(app: &mut SoundApp, id: &str, p: &serde_json::Value) -> Option<serde_json::Value> {
    (id == "window.assistant").then(|| {
        app.assistant.show = p.get("value").and_then(serde_json::Value::as_bool).unwrap_or(!app.assistant.show);
        json!({"value": app.assistant.show})
    })
}

pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    // Keep the log and busy state current while the window is closed, too.
    poll(app);
    let mut open = app.assistant.show;
    if !open {
        return;
    }
    egui::Window::new(RichText::new("AI Assistant").font(bold(13.0)))
        .open(&mut open)
        .default_size(vec2(380.0, 560.0))
        .min_size(vec2(300.0, 320.0))
        .resizable(true)
        .show(ctx, |ui| body(app, ui));
    app.assistant.show = open;
}

#[cfg(target_arch = "wasm32")]
fn poll(_app: &mut SoundApp) {}

#[cfg(target_arch = "wasm32")]
fn body(_app: &mut SoundApp, ui: &mut egui::Ui) {
    ui.label("The AI Assistant needs the desktop app.");
}

#[cfg(not(target_arch = "wasm32"))]
fn poll(app: &mut SoundApp) {
    use soundcraft_assistant::Event;
    let Some(chat) = app.assistant.chat.as_mut() else { return };
    for e in chat.poll() {
        let log = &mut app.assistant.log;
        match e {
            Event::Text(t) => log.push(Line::Claude(t)),
            Event::Tool(t) => log.push(Line::Tool(t, None)),
            Event::ToolDone { ok } => {
                if let Some(Line::Tool(_, done)) = log.iter_mut().rev().find(|l| matches!(l, Line::Tool(_, None))) {
                    *done = Some(ok);
                }
            }
            Event::Finished | Event::Failed(_) => {
                if let Event::Failed(why) = e {
                    log.push(Line::Error(why));
                }
                if let Some(span) = app.assistant.undo.as_mut() {
                    span.end = Some(app.engine.undo_history().len());
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn body(app: &mut SoundApp, ui: &mut egui::Ui) {
    use soundcraft_assistant::key;
    if !app.assistant.key_looked_up {
        app.assistant.key_looked_up = true;
        app.assistant.key = key::load().map(|(k, src)| {
            let label = match src {
                key::KeySource::Environment => "from ANTHROPIC_API_KEY",
                key::KeySource::Keychain => "saved in the Keychain",
                key::KeySource::Session => "for this session",
            };
            (k, label)
        });
    }
    if app.assistant.key.is_none() || app.assistant.settings_open {
        key_settings(app, ui);
        return;
    }
    toolbar(app, ui);
    ui.separator();
    let busy = app.assistant.chat.as_ref().is_some_and(|c| c.is_busy());
    // The log fills what the composer below leaves.
    let log_height = (ui.available_height() - 112.0).max(80.0);
    egui::ScrollArea::vertical().id_salt("assistant_log").max_height(log_height).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        if app.assistant.log.is_empty() {
            intro(ui);
        }
        for line in &app.assistant.log {
            line_ui(ui, line);
        }
    });
    ui.separator();
    status_row(app, ui, busy);
    composer(app, ui, busy);
}

#[cfg(not(target_arch = "wasm32"))]
fn intro(ui: &mut egui::Ui) {
    let t = Tokens::current();
    ui.add_space(6.0);
    ui.label(
        RichText::new("Ask for changes in plain words. Claude reads your session and makes the edits; you can undo all of them with one click.")
            .color(t.text_dim),
    );
    ui.add_space(4.0);
    for example in [
        "Add a stereo aux called Plate with reverb and send the keys and lead to it",
        "Pull the drums down 2 dB and pan the hats 30% right",
        "Put markers at every 8 bars and name them Intro, Verse, Chorus…",
        "Bounce the mix to my Desktop as a 24-bit WAV",
    ] {
        ui.label(RichText::new(format!("• {example}")).font(regular(11.5)).color(t.text_dim));
    }
}

fn line_ui(ui: &mut egui::Ui, line: &Line) {
    let t = Tokens::current();
    ui.add_space(3.0);
    match line {
        Line::User(s) => {
            ui.label(RichText::new("You").font(bold(11.0)).color(t.text_dim));
            ui.label(RichText::new(s).color(t.text));
        }
        Line::Claude(s) => {
            ui.label(RichText::new("Claude").font(bold(11.0)).color(t.accent));
            ui.label(RichText::new(s).color(t.text));
        }
        Line::Tool(s, done) => {
            let (mark, color) = match done {
                None => ("…", t.text_dim),
                Some(true) => ("✓", t.text_dim),
                Some(false) => ("✗", ui.visuals().warn_fg_color),
            };
            ui.label(RichText::new(format!("{mark} {s}")).font(mono(10.5)).color(color));
        }
        Line::Note(s) => {
            ui.label(RichText::new(s).italics().color(t.text_dim));
        }
        Line::Error(s) => {
            ui.label(RichText::new(s).color(ui.visuals().error_fg_color));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn toolbar(app: &mut SoundApp, ui: &mut egui::Ui) {
    let models = soundcraft_assistant::MODELS;
    ui.horizontal(|ui| {
        let current = models.get(app.assistant.model).map_or("?", |m| m.1);
        craft_ui::ComboBox::from_label("Model").selected_text(current).show_ui(ui, |ui| {
            for (i, (_, label)) in models.iter().enumerate() {
                ui.selectable_value(&mut app.assistant.model, i, *label);
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("API key").on_hover_text("Change or forget the Anthropic API key").clicked() {
                app.assistant.settings_open = true;
            }
            let busy = app.assistant.chat.as_ref().is_some_and(|c| c.is_busy());
            if ui.add_enabled(!busy, egui::Button::new("New chat")).on_hover_text("Start a fresh conversation").clicked() {
                if let Some(c) = &app.assistant.chat {
                    c.reset();
                }
                app.assistant.log.clear();
                app.assistant.undo = None;
            }
        });
    });
}

/// Busy indicator, Stop, and the one-click undo for the last reply's changes.
#[cfg(not(target_arch = "wasm32"))]
fn status_row(app: &mut SoundApp, ui: &mut egui::Ui, busy: bool) {
    let undo_len = app.engine.undo_history().len();
    // Undo is offered only while the history still ends where the reply left it: if anything was
    // done or undone since, "the assistant's changes" are no longer the top of the stack.
    let steps = match &app.assistant.undo {
        Some(UndoSpan { start, end: Some(end) }) if *end == undo_len && end > start => end - start,
        _ => 0,
    };
    ui.horizontal(|ui| {
        if busy {
            ui.add(egui::Spinner::new());
            ui.label(RichText::new("Working…").color(Tokens::current().text_dim));
            if ui.button("Stop").clicked()
                && let Some(c) = &app.assistant.chat
            {
                c.stop();
            }
        } else if steps > 0 {
            let label = if steps == 1 { "Undo assistant's change".to_string() } else { format!("Undo assistant's changes ({steps} steps)") };
            if ui.button(label).clicked() {
                let mut undone = 0;
                for _ in 0..steps {
                    if app.run("edit.undo", json!({})).is_err() {
                        break;
                    }
                    undone += 1;
                }
                app.assistant.undo = None;
                app.assistant.log.push(Line::Note(format!("Undid {undone} step{}.", if undone == 1 { "" } else { "s" })));
            }
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn composer(app: &mut SoundApp, ui: &mut egui::Ui, busy: bool) {
    let id = ui.make_persistent_id("assistant_input");
    // Enter sends, Shift+Enter starts a new line: take the plain Enter before the editor sees it.
    let enter = ui.memory(|m| m.has_focus(id)) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    let edit = egui::TextEdit::multiline(&mut app.assistant.input)
        .id(id)
        .desired_rows(3)
        .desired_width(f32::INFINITY)
        .hint_text("Ask for a change… (Enter to send, Shift+Enter for a new line)");
    ui.add(edit).widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Message to the assistant"));
    let send = ui.add_enabled(!busy && !app.assistant.input.trim().is_empty(), egui::Button::new("Send")).clicked();
    if (send || enter) && !busy {
        submit(app, ui.ctx());
        ui.memory_mut(|m| m.request_focus(id));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn submit(app: &mut SoundApp, ctx: &egui::Context) {
    let text = app.assistant.input.trim().to_string();
    if text.is_empty() {
        return;
    }
    if app.assistant.chat.is_none()
        && let Err(e) = start_chat(app, ctx)
    {
        app.assistant.log.push(Line::Error(e));
        return;
    }
    let models = soundcraft_assistant::MODELS;
    let Some((model, _)) = models.get(app.assistant.model).or(models.first()) else { return };
    let model = (*model).to_string();
    let undo_len = app.engine.undo_history().len();
    if let Some(chat) = app.assistant.chat.as_mut()
        && chat.ask(&text, &model)
    {
        app.assistant.log.push(Line::User(text));
        app.assistant.input.clear();
        app.assistant.undo = Some(UndoSpan { start: undo_len, end: None });
    }
}

/// Start the conversation thread with a backend that hands each call to the UI thread.
#[cfg(not(target_arch = "wasm32"))]
fn start_chat(app: &mut SoundApp, ctx: &egui::Context) -> Result<(), String> {
    let (key, _) = app.assistant.key.clone().ok_or("No API key set.")?;
    let (tx, rx) = std::sync::mpsc::channel();
    let backend = bridge::Bridge { tx, ctx: ctx.clone() };
    let repaint = ctx.clone();
    let chat = soundcraft_assistant::Assistant::start(backend, soundcraft_assistant::Client::new(key), move || repaint.request_repaint())?;
    app.assistant.requests = Some(rx);
    app.assistant.chat = Some(chat);
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn key_settings(app: &mut SoundApp, ui: &mut egui::Ui) {
    use soundcraft_assistant::key;
    let t = Tokens::current();
    ui.label(RichText::new("Connect to Claude").font(bold(13.0)));
    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "The assistant uses your own Anthropic API key. Create one at console.anthropic.com under API Keys; usage is billed to that account.",
        )
        .color(t.text_dim),
    );
    if let Some((k, source)) = &app.assistant.key {
        ui.add_space(6.0);
        ui.label(format!("Current key: {} ({source})", key::masked(k)));
    }
    ui.add_space(6.0);
    let field = ui.add(egui::TextEdit::singleline(&mut app.assistant.key_input).password(true).hint_text("sk-ant-…").desired_width(f32::INFINITY));
    field.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Anthropic API key"));
    if key::can_save() {
        ui.label(RichText::new("Saved in your Mac's Keychain, not in any SoundCraft file.").font(regular(11.0)).color(t.text_dim));
    } else {
        ui.label(RichText::new("Kept for this session only. Set ANTHROPIC_API_KEY to keep it.").font(regular(11.0)).color(t.text_dim));
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let entered = app.assistant.key_input.trim().to_string();
        if ui.add_enabled(!entered.is_empty(), egui::Button::new("Use this key")).clicked() {
            let source = if key::can_save() {
                match key::save(&entered) {
                    Ok(()) => "saved in the Keychain",
                    Err(e) => {
                        app.assistant.log.push(Line::Error(e));
                        "for this session"
                    }
                }
            } else {
                "for this session"
            };
            app.assistant.key = Some((entered, source));
            app.assistant.key_input.clear();
            app.assistant.settings_open = false;
            // A new key needs a new client; the conversation starts over with it.
            app.assistant.chat = None;
            app.assistant.requests = None;
        }
        if app.assistant.key.is_some() {
            if ui.button("Cancel").clicked() {
                app.assistant.key_input.clear();
                app.assistant.settings_open = false;
            }
            if key::can_save() && ui.button("Forget saved key").clicked() {
                key::forget();
                app.assistant.key = None;
                app.assistant.chat = None;
                app.assistant.requests = None;
                app.assistant.key_looked_up = false;
                app.assistant.settings_open = false;
            }
        }
    });
    // Keep earlier messages visible below, e.g. a Keychain error.
    if let Some(Line::Error(e)) = app.assistant.log.last() {
        ui.colored_label(Color32::from_rgb(220, 90, 80), e);
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod bridge {
    use crate::control::ControlRequest;
    use serde_json::Value;
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    /// How long one call may wait for the UI thread (a screenshot waits for a paint).
    const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

    /// The assistant's backend: each call becomes a control request for the UI thread.
    pub(super) struct Bridge {
        pub tx: Sender<ControlRequest>,
        pub ctx: egui::Context,
    }

    impl soundcraft_assistant::Backend for Bridge {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            let (req, reply) = ControlRequest::new(method, params);
            self.tx.send(req).map_err(|_| "SoundCraft's assistant window was closed".to_string())?;
            // Wake the UI so the request is handled even when nothing else is repainting.
            self.ctx.request_repaint();
            let v = reply.recv_timeout(REPLY_TIMEOUT).map_err(|_| format!("`{method}` got no reply from the app"))?;
            if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(v.get("error").and_then(Value::as_str).unwrap_or("the app refused the request").to_string())
            }
        }
        fn has_ui(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "the SoundCraft desktop app".into()
        }
    }
}
