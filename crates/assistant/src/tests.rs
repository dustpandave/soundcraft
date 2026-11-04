//! The conversation loop against a scripted model and a real headless engine.

use super::*;
use soundcraft_automation::Headless;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A model that replays canned replies and records every request it was sent.
#[derive(Clone)]
struct Scripted {
    replies: Arc<Mutex<VecDeque<Result<Value, String>>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Scripted {
    fn new(replies: Vec<Result<Value, String>>) -> Self {
        Scripted { replies: Arc::new(Mutex::new(replies.into())), requests: Arc::default() }
    }
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Llm for Scripted {
    fn send(&self, body: &Value) -> Result<Value, String> {
        self.requests.lock().unwrap().push(body.clone());
        self.replies.lock().unwrap().pop_front().unwrap_or_else(|| Err("script ran out".into()))
    }
}

/// A scripted model whose reply waits until the test releases it.
struct Gated {
    inner: Scripted,
    gate: Mutex<Receiver<()>>,
}

impl Llm for Gated {
    fn send(&self, body: &Value) -> Result<Value, String> {
        let _ = self.gate.lock().unwrap().recv();
        self.inner.send(body)
    }
}

/// A headless engine the test can still look at after handing it to the assistant.
#[derive(Clone)]
struct Shared(Arc<Mutex<Headless>>);

impl Backend for Shared {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.0.lock().unwrap().call(method, params)
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "test engine".into()
    }
}

fn demo() -> Shared {
    Shared(Arc::new(Mutex::new(Headless::new(soundcraft_engine::demo::demo_engine()))))
}

fn tool_use(id: &str, name: &str, input: Value) -> Result<Value, String> {
    Ok(
        json!({"stop_reason": "tool_use", "content": [{"type": "text", "text": "On it."}, {"type": "tool_use", "id": id, "name": name, "input": input}]}),
    )
}

fn answer(text: &str) -> Result<Value, String> {
    Ok(json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": text}]}))
}

fn wait(a: &mut Assistant) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        let evs = a.poll();
        let done = evs.iter().any(|e| matches!(e, Event::Finished | Event::Failed(_)));
        all.extend(evs);
        if done {
            return all;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("assistant did not finish: {all:?}");
}

fn undo_steps(engine: &Shared) -> usize {
    engine.0.lock().unwrap().engine.undo_history().len()
}

#[test]
fn tool_calls_change_the_session_and_the_reply_finishes() {
    let engine = demo();
    let before = undo_steps(&engine);
    let llm = Scripted::new(vec![
        tool_use("t1", "execute", json!({"command": "mix.volume", "params": {"track": "Kick", "db": -6}})),
        answer("Turned the kick down 6 dB."),
    ]);
    let mut a = Assistant::start(engine.clone(), llm.clone(), || {}).unwrap();
    assert!(a.ask("turn the kick down 6 dB", "claude-opus-5-5"));
    assert!(a.is_busy());
    let evs = wait(&mut a);
    assert_eq!(
        evs,
        vec![
            Event::Text("On it.".into()),
            Event::Tool("mix.volume {\"db\":-6,\"track\":\"Kick\"}".into()),
            Event::ToolDone { ok: true },
            Event::Text("Turned the kick down 6 dB.".into()),
            Event::Finished,
        ]
    );
    assert!(!a.is_busy());
    assert_eq!(undo_steps(&engine), before + 1, "one command, one undo step");

    // The second request carries the tool result back, with the model and the cached prefix intact.
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1]["model"], "claude-opus-5-5");
    let roles: Vec<&str> = reqs[1]["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    let result = &reqs[1]["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "t1");
    assert_eq!(result["is_error"], false);
    assert_eq!(reqs[0]["tools"], reqs[1]["tools"]);
}

#[test]
fn session_replacing_commands_never_reach_the_app() {
    let engine = demo();
    let name = engine.0.lock().unwrap().engine.session().name.clone();
    let llm = Scripted::new(vec![tool_use("t1", "execute", json!({"command": "session.new", "params": {}})), answer("I can't do that.")]);
    let mut a = Assistant::start(engine.clone(), llm.clone(), || {}).unwrap();
    a.ask("start a new session", "claude-opus-5-5");
    let evs = wait(&mut a);
    assert!(evs.contains(&Event::Finished));
    assert_eq!(engine.0.lock().unwrap().engine.session().name, name, "session untouched");
    let result = &llm.requests()[1]["messages"][2]["content"][0];
    assert_eq!(result["is_error"], true);
    assert!(result["content"][0]["text"].as_str().unwrap().contains("Ask the user"));
}

#[test]
fn a_failed_request_reports_and_the_next_turn_is_well_formed() {
    let engine = demo();
    let llm = Scripted::new(vec![Err("Claude API error 500: boom".into()), answer("Hello again.")]);
    let mut a = Assistant::start(engine, llm.clone(), || {}).unwrap();
    a.ask("hi", "claude-sonnet-5-5");
    assert_eq!(wait(&mut a).last(), Some(&Event::Failed("Claude API error 500: boom".into())));
    a.ask("hi again", "claude-sonnet-5-5");
    assert_eq!(wait(&mut a).last(), Some(&Event::Finished));
    let roles: Vec<String> = llm.requests()[1]["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap().to_string()).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "user turns alternate after a failure");
}

#[test]
fn stop_answers_pending_tool_calls_without_running_them() {
    let engine = demo();
    let before = undo_steps(&engine);
    // The model's reply asks for a change, but it is held until Stop has been pressed.
    let (release, gate) = channel::<()>();
    let llm = Gated {
        inner: Scripted::new(vec![tool_use("t1", "execute", json!({"command": "mix.volume", "params": {"track": "Kick", "db": -6}}))]),
        gate: Mutex::new(gate),
    };
    let mut a = Assistant::start(engine.clone(), llm, || {}).unwrap();
    a.ask("turn the kick down", "claude-opus-5-5");
    a.stop();
    release.send(()).unwrap();
    let evs = wait(&mut a);
    assert!(matches!(evs.last(), Some(Event::Failed(_))), "{evs:?}");
    assert_eq!(undo_steps(&engine), before, "nothing ran");
}

#[test]
fn reset_forgets_the_conversation() {
    let llm = Scripted::new(vec![answer("one"), answer("two")]);
    let mut a = Assistant::start(demo(), llm.clone(), || {}).unwrap();
    a.ask("first", "claude-opus-5-5");
    wait(&mut a);
    a.reset();
    a.ask("second", "claude-opus-5-5");
    wait(&mut a);
    assert_eq!(llm.requests()[1]["messages"].as_array().unwrap().len(), 1);
}
