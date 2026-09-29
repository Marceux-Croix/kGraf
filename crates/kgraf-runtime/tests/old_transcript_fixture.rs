//! Rebuilds a transcript checked in as `tests/fixtures/old_transcript_7074e93.jsonl`,
//! captured by running the pre-0067 code at commit `7074e93` (`kgraf-runtime`'s own
//! `JsonlTranscript`, so the bytes are exactly what that code wrote, not a
//! hand-written approximation of them).
//!
//! Unlike `rebuild.rs`'s in-memory byte-for-byte test, this one exercises the
//! actual wire bytes an older process left on disk: a rename of a field on any
//! `WorkerEvent` variant this fixture uses (`Notice.text` included) breaks
//! deserialization of the fixture, which fails this test loudly, before it can
//! ever silently rebuild something else instead.

use kgraf_provider::Message;
use kgraf_runtime::TranscriptBuilder;
use kgraf_session::transcript::TranscriptEntry;

const FIXTURE: &str = include_str!("fixtures/old_transcript_7074e93.jsonl");

#[test]
fn an_old_transcript_from_before_injected_and_round_abandoned_still_rebuilds() {
    let events: Vec<kgraf_runtime::WorkerEvent> = FIXTURE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<TranscriptEntry<kgraf_runtime::WorkerEvent>>(line)
                .unwrap_or_else(|e| panic!("fixture line did not parse: {e}\n{line}"))
                .event
        })
        .collect();
    assert_eq!(events.len(), 12, "the fixture's own event count");

    let opening = vec![Message::user("port the parser to Rust")];
    let mut builder = TranscriptBuilder::new(opening);
    for event in &events {
        builder.apply(event);
    }
    assert_eq!(builder.rounds(), 2);

    let rebuilt = serde_json::to_string(builder.messages()).expect("serialize");
    assert_eq!(
        rebuilt,
        concat!(
            r#"[{"role":"user","content":[{"type":"text","text":"port the parser to Rust"}]},"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"I will read the file first"},"#,
            r#"{"type":"tool_use","id":"t1","name":"read_file","input":{"path":"src/lib.rs"}}]},"#,
            r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"pub fn parse() {}"},"#,
            r#"{"type":"text","text":"3 rounds left"}]},"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"The file already has a parse function."}]}]"#
        ),
        "the wire-recorded fixture must rebuild to exactly this, byte for byte"
    );
}
