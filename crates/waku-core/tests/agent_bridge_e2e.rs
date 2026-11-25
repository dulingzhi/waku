//! End-to-end proof that a Claude lead can start a delegate.
//!
//! This wires the real stack together: a real [`WakuBackend`] behind a real
//! [`DaemonServer`], a scoped token minted for a seeded lead task, and the real
//! `waku --agent-bridge` process talking MCP over stdio exactly as Claude does.
//! Only the model call is avoided, because the bridge's own request is what is
//! being proved — and that is the part that was silently missing before.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use uuid::Uuid;
use waku_core::daemon::WakuBackend;
use waku_core::model::{AgentSession, Project, ProviderKind};
use waku_core::persistence::StateStore;
use waku_core::{DaemonServer, DaemonSettingsStore, ServerOptions};

/// The Waku executable, found next to this test binary.
///
/// `cargo test` puts integration tests in `target/<profile>/deps`, and the
/// program itself one level up, so this resolves to whatever was built with
/// them rather than a path baked in at compile time.
fn waku_executable() -> PathBuf {
    let mut path = std::env::current_exe().expect("test binary has a path");
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    let executable = path.join(if cfg!(windows) { "waku.exe" } else { "waku" });
    assert!(
        executable.is_file(),
        "the waku executable must be built for this probe: {}",
        executable.display()
    );
    executable
}

#[test]
fn a_real_bridge_creates_a_delegate_through_the_real_daemon() {
    // A private state directory, so the probe never touches the user's tasks.
    let root = std::env::temp_dir().join(format!("waku-bridge-e2e-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    // Seed one lead task and its project: a delegate is derived from them.
    let store = StateStore::new(root.join("app.db"));
    let mut state = store.load().unwrap();
    let project = Project::from_path(root.clone());
    let mut lead = AgentSession::new(project.id, ProviderKind::Claude);
    lead.title = "lead".into();
    // A real lead always has a transcript, and the store only writes sessions
    // that own one, so the seed needs a turn for the same reason.
    lead.begin_turn("start the group");
    let lead_id = lead.id;
    state.projects.push(project);
    state.push_session(lead);
    store.save(&mut state).unwrap();

    let settings = DaemonSettingsStore::open(root.join("daemon.json")).unwrap();
    let backend = Arc::new(WakuBackend::new(settings, store).unwrap());

    // A real server, so the token is really checked the way a bridge's is.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = Arc::new(DaemonServer::new(
        "desktop-token".into(),
        Arc::new(waku_core::daemon_identity::DaemonIdentity::generate()),
        backend.clone(),
        ServerOptions::default(),
    ));
    let runtime_id = Uuid::new_v4();
    let scoped = server.mint_agent_token(lead_id, runtime_id);
    let stop = Arc::new(AtomicBool::new(false));
    let serving = {
        let server = server.clone();
        let stop = stop.clone();
        std::thread::spawn(move || server.serve(listener, stop).unwrap())
    };

    // The real bridge binary, driven over MCP stdio.
    let mut bridge = Command::new(waku_executable())
        .arg("--agent-bridge")
        .env(waku_core::DAEMON_ADDRESS_ENV, address.to_string())
        .env(waku_core::DAEMON_TOKEN_ENV, &scoped)
        .env(waku_core::DAEMON_SESSION_ENV, lead_id.to_string())
        .env(waku_core::DAEMON_RUNTIME_ENV, runtime_id.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the agent bridge must launch");

    let mut stdin = bridge.stdin.take().unwrap();
    let mut stdout = BufReader::new(bridge.stdout.take().unwrap());
    let mut call = |request: serde_json::Value| -> serde_json::Value {
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };

    let initialized = call(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}}
    }));
    assert_eq!(initialized["result"]["serverInfo"]["name"], "waku_agents");

    let spawned = call(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "spawn_agent", "arguments": {"task": "list the tests"}}
    }));
    assert!(
        spawned.get("error").is_none(),
        "the bridge refused to spawn: {spawned}"
    );
    let text = spawned["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !text.contains("unavailable"),
        "the daemon refused the lead: {text}"
    );

    // Being told an id proves nothing on its own, so the same daemon is asked
    // what tasks it now has. Two means the delegate became a real task.
    let listed = waku_core::Backend::handle(
        backend.as_ref(),
        waku_core::Request {
            request_id: Uuid::new_v4(),
            session_id: lead_id,
            runtime_id,
            command: waku_core::Command::LoadTaskState,
        },
        server.event_sink_for(lead_id, runtime_id),
    )
    .unwrap();
    let waku_core::ResponsePayload::TaskState { sessions, .. } = listed else {
        panic!("the daemon did not return its task list");
    };
    assert_eq!(
        sessions.len(),
        2,
        "the delegate has to exist as an ordinary task: {sessions:?}"
    );
    // It also has to survive a restart. The store only writes a task that owns a
    // transcript step, so a delegate created without one would look created here
    // and be gone the next time the daemon opened its database.
    let delegate_id = sessions
        .iter()
        .map(|session| session.id)
        .find(|id| *id != lead_id)
        .expect("the delegate is the task that is not the lead");
    let reloaded = StateStore::new(root.join("app.db")).load().unwrap();
    assert!(
        reloaded
            .sessions
            .iter()
            .any(|session| session.id == delegate_id),
        "the delegate must be stored, not only resident"
    );

    // A delegate the lead cannot read is only half a feature, so the same real
    // bridge is asked about the task the same real daemon started. The check is
    // deliberately "not the stub": the old tool answered every id with
    // "unknown", which is what this replaces.
    let checked = call(serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "check_agent", "arguments": {"agent_id": delegate_id.to_string()}}
    }));
    assert!(
        checked.get("error").is_none(),
        "the bridge refused to read its own delegate: {checked}"
    );
    let checked_text = checked["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !checked_text.contains("unknown"),
        "check_agent is still the placeholder: {checked_text}"
    );
    assert!(
        checked_text.contains(&delegate_id.to_string()),
        "a report has to name the agent it is about: {checked_text}"
    );
    assert!(
        ["still working", "finished", "failed"]
            .iter()
            .any(|state| checked_text.contains(state)),
        "a report has to carry the delegate's state: {checked_text}"
    );

    // Steering has to reach the daemon too. Which of the two shapes it takes is
    // the daemon's decision, so both answers are accepted and the old echo is
    // not.
    let steered = call(serde_json::json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "send_to_agent", "arguments": {
            "agent_id": delegate_id.to_string(),
            "message": "also list the benches"
        }}
    }));
    assert!(
        steered.get("error").is_none(),
        "the bridge could not reach its own delegate: {steered}"
    );
    let steered_text = steered["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        steered_text.contains("steered") || steered_text.contains("started a new turn"),
        "send_to_agent has to report how the message landed: {steered_text}"
    );

    let _ = bridge.kill();
    let _ = bridge.wait();
    stop.store(true, Ordering::Release);
    serving.join().unwrap();
    std::fs::remove_dir_all(root).ok();
}
