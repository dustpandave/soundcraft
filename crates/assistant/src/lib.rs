//! SoundCraft's built-in AI assistant.
//!
//! A Claude conversation that runs on its own thread and acts on SoundCraft through the same
//! tools as the MCP server ([`soundcraft_automation::mcp`]): every change is a command, goes
//! through the app's normal command path and lands in the undo history. The UI hands it a
//! [`Backend`] that forwards each call to the UI thread; tests hand it a headless engine.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod claude;
pub mod key;
mod tools;

pub use claude::{Client, Llm};
/// What the assistant acts through (re-exported so the UI needn't depend on the automation crate).
pub use soundcraft_automation::Backend;

use serde_json::{Value, json};
use soundcraft_automation::mcp;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

/// The models offered in the window: (id, label). The first is the default.
pub const MODELS: &[(&str, &str)] =
    &[("claude-opus-5-5", "Claude Opus 5.5"), ("claude-sonnet-5-5", "Claude Sonnet 5.5"), ("claude-haiku-5-5", "Claude Haiku 5.5")];

/// API requests one user message may make before the assistant stops and reports back.
const MAX_STEPS: usize = 40;

const SYSTEM: &str = "You are the assistant built into SoundCraft, a digital audio workstation (DAW) in the style of Pro Tools. \
You are talking to a musician or engineer who is working in the app right now. The session you can inspect and change is the one open on their screen, and they see your changes as you make them.

How to work:
- Everything in SoundCraft is a command. Use `list_commands` with a filter to find command ids and their params, then `execute` (or `batch` for several). Look ids and params up rather than guessing them.
- Inspect the session (`inspect_session`, detail \"summary\" first) before changing it, so you use the real track names, and check the result afterwards.
- Positions are samples (integers), {\"seconds\": x}, or time strings like \"bars_beats:5|1|000\" or \"0:12.500\". Tracks are referenced by id or name.
- Your changes go into the app's undo history and the user can undo everything you did for a request with one button. Make the changes they asked for and no others.
- You can't open, close, replace or revert sessions or quit the app; ask the user to do that.
- Take a `screenshot` only when you need to see the layout on screen.
- When the user attaches audio files, their absolute paths are listed in the message: bring them in with `import_audio` (one new track per file unless they say otherwise), then do what they asked with them.
- Reply briefly in plain musician language: what you changed, or one question when the request is unclear. Talk in dB, bars and beats rather than samples.";

/// What the conversation reports to the window.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Text from Claude.
    Text(String),
    /// A tool call is starting.
    Tool(String),
    /// The last tool call finished.
    ToolDone { ok: bool },
    /// The reply to the user's message is complete.
    Finished,
    /// The reply ended early.
    Failed(String),
}

enum Job {
    Ask { text: String, model: String },
    Reset,
}

/// A running conversation. Dropping it ends the worker thread once it is idle.
pub struct Assistant {
    jobs: Sender<Job>,
    events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    busy: bool,
}

impl Assistant {
    /// Start a conversation with Claude over `llm` that acts through `backend`. `notify` runs
    /// after every event (the UI asks for a repaint there).
    pub fn start<B, L>(backend: B, llm: L, notify: impl Fn() + Send + 'static) -> Result<Self, String>
    where
        B: Backend + Send + 'static,
        L: Llm + 'static,
    {
        let (jobs, job_rx) = channel();
        let (ev_tx, events) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("soundcraft-assistant".into())
            .spawn(move || {
                let emit = move |e: Event| {
                    let _ = ev_tx.send(e);
                    notify();
                };
                Worker { server: mcp::Server::new(backend), llm, tools: tools::definitions(), history: Vec::new(), cancel: flag, emit: &emit }
                    .run(&job_rx);
            })
            .map_err(|e| format!("could not start the assistant: {e}"))?;
        Ok(Assistant { jobs, events, cancel, busy: false })
    }

    /// Send the user's message. Ignored while a reply is in progress.
    pub fn ask(&mut self, text: &str, model: &str) -> bool {
        if self.busy || text.trim().is_empty() {
            return false;
        }
        self.cancel.store(false, Ordering::SeqCst);
        self.busy = self.jobs.send(Job::Ask { text: text.trim().to_string(), model: model.to_string() }).is_ok();
        self.busy
    }

    /// Stop the reply in progress after the current step.
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Forget the conversation so far.
    pub fn reset(&self) {
        let _ = self.jobs.send(Job::Reset);
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Events since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(e) => {
                    if matches!(e, Event::Finished | Event::Failed(_)) {
                        self.busy = false;
                    }
                    out.push(e);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.busy {
                        self.busy = false;
                        out.push(Event::Failed("the assistant stopped unexpectedly".into()));
                    }
                    break;
                }
            }
        }
        out
    }
}

struct Worker<'a, B: Backend, L: Llm> {
    server: mcp::Server<B>,
    llm: L,
    tools: Vec<Value>,
    /// The conversation, append-only (thinking blocks stay valid and the prefix stays cached).
    history: Vec<Value>,
    cancel: Arc<AtomicBool>,
    emit: &'a dyn Fn(Event),
}

impl<B: Backend, L: Llm> Worker<'_, B, L> {
    fn run(&mut self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Reset => self.history.clear(),
                Job::Ask { text, model } => {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.turn(&text, &model)))
                        .unwrap_or_else(|_| Err("the assistant hit an internal error".into()));
                    if let Err(e) = &r {
                        self.close_turn(e);
                    }
                    (self.emit)(match r {
                        Ok(()) => Event::Finished,
                        Err(e) => Event::Failed(e),
                    });
                }
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// One user message: ask Claude, run the tools it calls, repeat until it answers.
    fn turn(&mut self, text: &str, model: &str) -> Result<(), String> {
        self.history.push(json!({"role": "user", "content": [{"type": "text", "text": text}]}));
        for _ in 0..MAX_STEPS {
            if self.cancelled() {
                return Err("Stopped.".into());
            }
            let reply = self.llm.send(&claude::request(model, SYSTEM, &self.tools, &self.history))?;
            let content = reply.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
            let stop = reply.get("stop_reason").and_then(Value::as_str).unwrap_or("");
            if content.is_empty() {
                return Err(match stop {
                    "refusal" => "Claude declined this request.".into(),
                    _ => "Claude sent an empty reply.".into(),
                });
            }
            self.history.push(json!({"role": "assistant", "content": content}));
            for text in content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()) {
                if !text.trim().is_empty() {
                    (self.emit)(Event::Text(text.trim().to_string()));
                }
            }
            match stop {
                "tool_use" => {
                    let results = self.run_tools(&content);
                    self.history.push(json!({"role": "user", "content": results}));
                }
                "pause_turn" => {}
                "max_tokens" => return Err("The reply was cut off. Try asking for less at once.".into()),
                "refusal" => return Err("Claude declined this request.".into()),
                _ => return Ok(()),
            }
        }
        Err(format!("Stopped after {MAX_STEPS} steps. Ask again to continue."))
    }

    /// Run every tool call in one reply; every call gets a result, as the API requires.
    fn run_tools(&mut self, content: &[Value]) -> Vec<Value> {
        let mut results = Vec::new();
        for call in content.iter().filter(|b| b["type"] == "tool_use") {
            let id = call.get("id").and_then(Value::as_str).unwrap_or("");
            let name = call.get("name").and_then(Value::as_str).unwrap_or("");
            let input = call.get("input").cloned().unwrap_or(json!({}));
            if self.cancelled() {
                results.push(tools::error_block(id, "Not run: the user pressed Stop."));
                continue;
            }
            if let Some(why) = tools::refusal(name, &input) {
                results.push(tools::error_block(id, &why));
                continue;
            }
            (self.emit)(Event::Tool(tools::describe(name, &input)));
            let (block, ok) = tools::result_block(id, &self.server.call_tool(name, &input));
            (self.emit)(Event::ToolDone { ok });
            results.push(block);
        }
        results
    }

    /// After a turn ends early the history may end on a user message; close it with a note so
    /// the next message starts a well-formed turn.
    fn close_turn(&mut self, why: &str) {
        if self.history.last().and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user") {
            self.history.push(json!({"role": "assistant", "content": [{"type": "text", "text": format!("(My reply ended early: {why})")}]}));
        }
    }
}

#[cfg(test)]
mod tests;
