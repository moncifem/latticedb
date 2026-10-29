//! Runs the conformance suite against the reference plugin, proves each check can fail with a
//! deliberately broken plugin, and replays the worked examples of `formats/plugin-contract-v1.md`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod support;

use lattice_sdk::contract::{decode_line, encode_line};
use support::reference::{Fault, ReferencePlugin};
use support::suite::{self, PluginUnderTest};

const DOC: &str = include_str!("../../../formats/plugin-contract-v1.md");

fn make(fault: Fault) -> Box<dyn PluginUnderTest> {
    Box::new(ReferencePlugin::new(fault))
}

#[test]
fn reference_plugin_passes_every_check() {
    let results = suite::run_all(&|| make(Fault::None));
    assert_eq!(results.len(), suite::CHECKS.len());
    for (name, result) in results {
        assert_eq!(result, Ok(()), "check {name}");
    }
}

/// Each fault breaks exactly the rule it names, so every check is shown able to fail.
const BROKEN: &[(Fault, &str)] = &[
    (Fault::CreateNotIdempotent, "create_is_idempotent"),
    (
        Fault::DuplicateRecordedAgain,
        "append_receipts_and_duplicates",
    ),
    (Fault::SupersededAccepted, "superseded_turn_is_fenced"),
    (Fault::FencedStillWrites, "superseded_turn_is_fenced"),
    (Fault::DuplicateBeforeFence, "superseded_turn_is_fenced"),
    (Fault::NoLive, "watch_ready_items_live"),
    (Fault::LiveBeforeItems, "watch_ready_items_live"),
    (Fault::ResumeIgnoresCursor, "watch_ready_items_live"),
    (Fault::NoIncremental, "watch_ready_items_live"),
    (Fault::CursorBeyondHeadAccepted, "watch_ready_items_live"),
    (Fault::ConflictIgnored, "child_sessions"),
    (Fault::ParentNotEchoed, "child_sessions"),
    (Fault::SilentOnBadLine, "rejects_bad_lines"),
    (Fault::WrongVersionCode, "rejects_bad_lines"),
];

#[test]
fn each_broken_plugin_fails_its_check() {
    for (fault, check) in BROKEN {
        let results = suite::run_all(&|| make(*fault));
        let (_, result) = results.iter().find(|(name, _)| name == check).unwrap();
        assert!(result.is_err(), "{fault:?} must fail {check}");
    }
}

#[test]
fn every_check_has_a_broken_plugin() {
    for (name, _) in suite::CHECKS {
        assert!(
            BROKEN.iter().any(|(_, c)| c == name),
            "no fault proves {name} can fail"
        );
    }
}

/// The bodies of the fenced code blocks under "Worked examples", one transcript each.
fn transcripts() -> Vec<Vec<(bool, String)>> {
    let examples = DOC
        .split("## Worked examples")
        .nth(1)
        .expect("examples section");
    let mut out = Vec::new();
    let mut current: Option<Vec<(bool, String)>> = None;
    for line in examples.lines() {
        if line.starts_with("```text") {
            current = Some(Vec::new());
        } else if line.starts_with("```") {
            out.extend(current.take());
        } else if let Some(block) = current.as_mut() {
            if let Some(rest) = line.strip_prefix("> ") {
                block.push((true, rest.to_owned()));
            } else if let Some(rest) = line.strip_prefix("< ") {
                block.push((false, rest.to_owned()));
            } else {
                panic!("transcript line without a direction: {line:?}");
            }
        }
    }
    out
}

#[test]
fn document_has_at_least_ten_transcripts() {
    assert!(transcripts().len() >= 10);
}

#[test]
fn plugin_lines_in_the_document_are_canonical() {
    for (n, t) in transcripts().iter().enumerate() {
        for (is_client, line) in t {
            // Client lines in the "refused" examples are deliberately not valid messages.
            let Ok(msg) = decode_line(line.as_bytes()) else {
                assert!(
                    *is_client,
                    "transcript {}: plugin line does not decode: {line}",
                    n + 1
                );
                continue;
            };
            assert_eq!(&encode_line(&msg).unwrap(), line, "transcript {}", n + 1);
        }
    }
}

#[test]
fn transcripts_replay_against_the_reference_plugin() {
    for (n, t) in transcripts().iter().enumerate() {
        let mut plugin = ReferencePlugin::correct();
        let mut i = 0;
        while i < t.len() {
            let (is_client, line) = &t[i];
            assert!(*is_client, "transcript {}: reply without a request", n + 1);
            let mut expected = Vec::new();
            i += 1;
            while i < t.len() && !t[i].0 {
                expected.push(t[i].1.clone());
                i += 1;
            }
            let got: Vec<String> = plugin
                .send(line.as_bytes())
                .into_iter()
                .map(|l| String::from_utf8(l).unwrap())
                .collect();
            assert_eq!(got, expected, "transcript {} after {line}", n + 1);
        }
    }
}
