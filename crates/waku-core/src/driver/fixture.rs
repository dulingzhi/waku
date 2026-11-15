//! Provider fixture replay.
//!
//! A fixture is the provider's side of a recorded conversation: the JSON-RPC
//! replies it gave Waku's requests, then everything it pushed unprompted.
//! Replaying one feeds the real driver's parser the recorded wire messages, so
//! an upstream CLI changing its shape fails a test instead of a user's task.
//!
//! The format is `docs/provider-fixtures.md`. This module implements exactly
//! that shape; the recorder writes it and the drivers read it, so both sides
//! cannot drift.

use std::collections::VecDeque;
use std::path::Path;

use anyhow::{Context as _, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};

/// A recorded provider conversation.
///
/// Field names track the documented JSON exactly, and payloads are kept as
/// `Value`: a fixture that is normalized on the way in would stop being
/// evidence of what the provider actually sent.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Fixture {
    pub(crate) provider: String,
    pub(crate) scenario: String,
    /// Provenance. Documents which CLI produced this recording, and is
    /// deliberately not used by replay.
    #[serde(default)]
    pub(crate) recorded_with: Option<String>,
    #[serde(default)]
    pub(crate) recorded_at: Option<String>,
    #[serde(default)]
    pub(crate) transport: Option<String>,
    /// Replies the replay must serve, in the order the driver asked for them.
    #[serde(default)]
    pub(crate) exchanges: Vec<Exchange>,
    /// Everything the provider pushed unprompted, in recorded order.
    ///
    /// Kept as raw `Value` because a provider's notifications are its own
    /// shape. A rejected turn is recorded here too, in the documented
    /// `{"method": "error", "params": {"message": ...}}` form; see
    /// [`Fixture::rejections`] for the check that catches a mistyped one.
    #[serde(default)]
    pub(crate) events: Vec<Value>,
}

/// One request the provider answered, with the reply it sent.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Exchange {
    /// The JSON-RPC method the reply answers.
    pub(crate) on_request: String,
    /// Stored verbatim; replay only re-stamps `id` so it correlates with the
    /// request, which is what JSON-RPC requires of any real provider.
    pub(crate) reply: Value,
}

impl Fixture {
    /// Reads and parses one fixture file.
    pub(crate) fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read fixture {}", path.display()))?;
        Self::from_json(&text)
            .with_context(|| format!("could not parse fixture {}", path.display()))
    }

    pub(crate) fn from_json(text: &str) -> anyhow::Result<Self> {
        let fixture: Self = serde_json::from_str(text).context("could not parse fixture")?;
        fixture.validate()?;
        Ok(fixture)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.provider.trim().is_empty() {
            return Err(anyhow!("fixture has no provider name"));
        }
        if self.scenario.trim().is_empty() {
            return Err(anyhow!("fixture has no scenario name"));
        }
        for (index, exchange) in self.exchanges.iter().enumerate() {
            if exchange.on_request.trim().is_empty() {
                return Err(anyhow!(
                    "fixture {} exchange {index} has an empty on_request",
                    self.scenario
                ));
            }
        }
        // A rejected turn is the most valuable recording there is (Kimi reports
        // a clean `end_turn` while the real reason rides in a separate error
        // notification), so a malformed one is rejected loudly rather than
        // replayed as a provider that simply said nothing.
        for (index, event) in self.events.iter().enumerate() {
            let is_error = event.get("method").and_then(Value::as_str) == Some("error");
            if !is_error {
                continue;
            }
            let message = event
                .get("params")
                .and_then(|params| params.get("message"))
                .and_then(Value::as_str);
            if message.is_none_or(|message| message.trim().is_empty()) {
                return Err(anyhow!(
                    "fixture {} event {index} is a provider error without a non-empty params.message",
                    self.scenario
                ));
            }
        }
        Ok(())
    }

    /// The provider errors this recording contains, in order.
    ///
    /// A rejected turn is a first-class scenario, so tests can assert that one
    /// reached the user rather than only that the stream ended.
    pub(crate) fn rejections(&self) -> impl Iterator<Item = &Value> {
        self.events
            .iter()
            .filter(|event| event.get("method").and_then(Value::as_str) == Some("error"))
    }
}

/// Replays a fixture against a driver's parser.
///
/// Replay substitutes the provider's *stream*, not its process: the driver
/// keeps its real parser, and every message handed to it is the recorded
/// bytes. Events land on the same `test_event_channel()` a reader thread would
/// write to, so assertions run against the real driver code.
pub(crate) struct Replayer {
    fixture: Fixture,
    /// Index of the next exchange to serve. Recording order is meaning, so this
    /// only moves forward.
    cursor: usize,
    /// Events not yet pushed.
    events: VecDeque<Value>,
    /// Methods the driver sent that the recording does not answer. A driver
    /// probing an optional feature lands here, and so does a real regression.
    unmatched: Vec<String>,
}

impl Replayer {
    pub(crate) fn new(fixture: Fixture) -> anyhow::Result<Self> {
        fixture.validate()?;
        let events = fixture.events.iter().cloned().collect();
        Ok(Self {
            fixture,
            cursor: 0,
            events,
            unmatched: Vec::new(),
        })
    }

    pub(crate) fn from_json(text: &str) -> anyhow::Result<Self> {
        Self::new(Fixture::from_json(text)?)
    }

    pub(crate) fn fixture(&self) -> &Fixture {
        &self.fixture
    }

    /// Events not yet pushed.
    pub(crate) fn pending_events(&self) -> usize {
        self.events.len()
    }

    /// Methods the driver sent that the recording does not answer.
    pub(crate) fn unmatched(&self) -> &[String] {
        &self.unmatched
    }

    /// Feeds one request the driver sent, and returns what the provider would
    /// write in response, in wire order.
    ///
    /// Notifications (no `id`, such as `initialized`) produce nothing: the
    /// provider owes them no reply.
    pub(crate) fn respond(&mut self, request: &Value) -> Vec<Value> {
        let Some(method) = request
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Vec::new();
        };
        let Some(id) = request.get("id").cloned() else {
            return Vec::new();
        };

        let mut out = Vec::new();
        match self.take_reply(&method) {
            Some(mut reply) => {
                if let Some(object) = reply.as_object_mut() {
                    object.insert("id".to_owned(), id);
                }
                out.push(reply);
            }
            None => {
                self.unmatched.push(method);
                // What a provider answers for a method it does not have. Real,
                // and the drivers already classify it (for example Codex's
                // goal probe turning off goal support quietly).
                out.push(json!({
                    "id": id,
                    "error": {"code": -32601, "message": "method not found"},
                }));
            }
        }

        // Documented order: the replies are answered first, then the provider
        // starts pushing. Everything the driver asked for is answered by then.
        if self.cursor >= self.fixture.exchanges.len() {
            out.extend(self.events.drain(..));
        }
        out
    }

    /// Convenience wrapper for a request built from its method and id.
    pub(crate) fn respond_to(&mut self, method: &str, id: impl Into<Value>) -> Vec<Value> {
        self.respond(&json!({"method": method, "id": id.into()}))
    }

    fn take_reply(&mut self, method: &str) -> Option<Value> {
        if self
            .fixture
            .exchanges
            .get(self.cursor)
            .is_some_and(|exchange| exchange.on_request == method)
        {
            let reply = self.fixture.exchanges[self.cursor].reply.clone();
            self.cursor += 1;
            return Some(reply);
        }
        // A request that would need a later exchange skipped an earlier one.
        // Serving it anyway would hide the drift the fixture exists to catch.
        if self.fixture.exchanges[self.cursor..]
            .iter()
            .any(|exchange| exchange.on_request == method)
        {
            panic!(
                "fixture {}: `{method}` was recorded after `{}`, but the driver sent it first",
                self.fixture.scenario, self.fixture.exchanges[self.cursor].on_request
            );
        }
        None
    }

    /// Fails unless the whole recording was consumed: every exchange answered
    /// and every event delivered. A fixture that stops halfway is a driver that
    /// no longer gets that far.
    pub(crate) fn assert_complete(&self) {
        assert_eq!(
            self.cursor,
            self.fixture.exchanges.len(),
            "fixture {}: {} of {} exchanges unserved",
            self.fixture.scenario,
            self.fixture.exchanges.len() - self.cursor,
            self.fixture.exchanges.len()
        );
        assert_eq!(
            self.events.len(),
            0,
            "fixture {}: {} recorded events never delivered",
            self.fixture.scenario,
            self.events.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like the app-server's real traffic rather than like the prose
    /// sketch in the design doc: `turn/completed` carries its status under
    /// `params.turn`, and `turn/started` carries the turn id the driver tracks.
    const CODEX: &str = r#"
    {
      "provider": "codex",
      "scenario": "synthetic-turn",
      "recorded_with": "codex 0.52.0",
      "recorded_at": "2026-11-15",
      "transport": "codex-app-server",
      "exchanges": [
        { "on_request": "initialize", "reply": { "id": 0, "result": {} } },
        { "on_request": "thread/resume",
          "reply": { "id": 1, "result": { "thread": { "id": "th_1", "turns": [] } } } },
        { "on_request": "turn/start",
          "reply": { "id": 11, "result": { "turn": { "id": "turn_1" } } } }
      ],
      "events": [
        { "method": "turn/started",
          "params": { "threadId": "th_1", "turn": { "id": "turn_1" } } },
        { "method": "item/agentMessage/delta", "params": { "delta": "Hello" } },
        { "method": "turn/completed",
          "params": { "threadId": "th_1", "turn": { "id": "turn_1", "status": "completed" } } }
      ]
    }
    "#;

    /// Drives a fixture through the request sequence a Codex session makes.
    fn drive(replayer: &mut Replayer) -> Vec<Value> {
        let mut wire = Vec::new();
        wire.extend(replayer.respond_to("initialize", 0));
        wire.extend(replayer.respond(&json!({"method": "initialized", "params": {}})));
        wire.extend(replayer.respond_to("thread/resume", 1));
        wire.extend(replayer.respond_to("turn/start", 11));
        wire
    }

    #[test]
    fn replies_are_matched_by_method_name_not_by_position() {
        let mut replayer = Replayer::from_json(CODEX).unwrap();
        assert_eq!(replayer.fixture().provider, "codex");

        let initialize = replayer.respond_to("initialize", 0);
        assert_eq!(initialize.len(), 1);
        assert_eq!(initialize[0]["result"], json!({}));

        // A probe the recording does not cover gets the real "no such method"
        // answer instead of a neighbouring reply being handed over.
        let probe = replayer.respond_to("thread/goal/get", 10);
        assert_eq!(probe[0]["error"]["code"], json!(-32601));
        assert_eq!(replayer.unmatched(), ["thread/goal/get"]);

        let resume = replayer.respond_to("thread/resume", 1);
        assert_eq!(resume[0]["result"]["thread"]["id"], json!("th_1"));

        // The reply carries the id the request used, not the recorded one.
        let start = replayer.respond_to("turn/start", 11);
        assert_eq!(start[0]["id"], json!(11));
        assert_eq!(start[0]["result"]["turn"]["id"], json!("turn_1"));
        replayer.assert_complete();
    }

    #[test]
    fn events_are_pushed_after_the_replies_in_recorded_order() {
        let mut replayer = Replayer::from_json(CODEX).unwrap();
        let wire = drive(&mut replayer);

        let methods = wire
            .iter()
            .map(|message| {
                message
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("reply:{}", message["id"]))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            [
                "reply:0".to_owned(),
                "reply:1".to_owned(),
                "reply:11".to_owned(),
                "turn/started".to_owned(),
                "item/agentMessage/delta".to_owned(),
                "turn/completed".to_owned(),
            ]
        );
        assert_eq!(wire[3]["params"]["turn"]["id"], json!("turn_1"));
        assert_eq!(replayer.pending_events(), 0);
        replayer.assert_complete();
    }

    #[test]
    fn a_missing_reply_or_a_reordered_request_fails_the_fixture() {
        // Missing: the driver stops asking partway, so exchanges are left
        // unserved and the scenario silently never reaches its end.
        let mut truncated = Replayer::from_json(CODEX).unwrap();
        truncated.respond_to("initialize", 0);
        truncated.respond_to("thread/resume", 1);
        // `turn/start` never happens: its exchange and every event after it
        // stay undelivered.
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                truncated.assert_complete();
            }))
            .is_err(),
            "an unserved exchange and undelivered events must fail the fixture"
        );

        // Reordered: asking for a later exchange before an earlier one is the
        // exact regression a fixture is supposed to catch.
        let mut reordered = Replayer::from_json(CODEX).unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reordered.respond_to("turn/start", 11);
        }));
        assert!(
            outcome.is_err(),
            "a request recorded later must not be served first"
        );
    }

    #[test]
    fn a_rejected_turn_is_recorded_as_a_first_class_error_event() {
        // The Kimi shape: the protocol reports a clean end of turn, and the
        // reason the turn actually failed arrives as a separate notification.
        // That is the recording worth keeping, so it must replay like one.
        let rejected = r#"
        {
          "provider": "codex",
          "scenario": "turn-rejected",
          "recorded_with": "synthetic",
          "exchanges": [
            { "on_request": "turn/start",
              "reply": { "id": 11, "result": { "turn": { "id": "turn_2" } } } }
          ],
          "events": [
            { "method": "turn/started", "params": { "turn": { "id": "turn_2" } } },
            { "method": "error",
              "params": { "message": "You've hit your usage limit" } },
            { "method": "turn/completed",
              "params": { "turn": { "id": "turn_2", "status": "completed" } } }
          ]
        }
        "#;
        let mut replayer = Replayer::from_json(rejected).unwrap();

        let recorded = replayer
            .fixture()
            .rejections()
            .map(|event| event["params"]["message"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(recorded, ["You've hit your usage limit"]);

        let wire = replayer.respond_to("turn/start", 11);
        let methods = wire
            .iter()
            .map(|message| message["method"].as_str().unwrap_or("reply").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            ["reply", "turn/started", "error", "turn/completed"],
            "a rejection keeps its recorded position: {wire:?}"
        );
        replayer.assert_complete();
    }

    #[test]
    fn an_error_event_without_a_message_is_rejected_at_load() {
        let malformed = CODEX.replace(
            r#"{ "method": "turn/completed","#,
            r#"{ "method": "error", "params": {} },{ "method": "turn/completed","#,
        );
        let error = Fixture::from_json(&malformed)
            .expect_err("an error event without a message is not a usable recording");
        assert!(
            error.to_string().contains("params.message"),
            "the failure must name the missing field: {error}"
        );
    }

    #[test]
    fn the_committed_codex_fixture_parses_and_documents_its_provenance() {
        let fixture = Fixture::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/driver/fixtures/codex/fork-preserves-history.json"),
        )
        .expect("the committed Codex fixture parses");

        assert_eq!(fixture.provider, "codex");
        assert_eq!(fixture.scenario, "fork-preserves-history");
        assert_eq!(fixture.transport.as_deref(), Some("codex-app-server"));
        assert!(
            fixture.recorded_with.is_some() && fixture.recorded_at.is_some(),
            "a fixture records which CLI made it"
        );
        assert!(
            fixture
                .exchanges
                .iter()
                .any(|exchange| exchange.on_request == "turn/start"),
            "the scenario drives a turn"
        );
    }
}
