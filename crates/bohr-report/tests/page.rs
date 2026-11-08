//! The page, checked for the things a reader would never notice and an attacker would.
//!
//! Rendering is exempt from being written test-first — how a report looks is settled by showing it
//! to somebody, not by asserting on it. What is not exempt is everything a test can decide: that a
//! guest cannot write markup into it, that the elements close, that two renderings of one bundle
//! are the same bytes, and that the file stays small enough to attach to an issue.

use std::path::PathBuf;

use bohr_trace::Bundle;

fn stored(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bundles").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn page(name: &str) -> String {
    let bytes = stored(name);
    let bundle = Bundle::parse(&bytes).expect("a stored bundle parses");
    bohr_report::render(&bundle, Some("events.jsonl")).expect("a stored bundle renders").html
}

/// Every element that opens closes, in order. A page that does not is one a browser repairs by
/// guessing, and two browsers guess differently.
#[test]
fn every_element_closes_in_the_order_it_opened() {
    const VOID: [&str; 6] = ["meta", "br", "hr", "img", "input", "link"];
    let html = page("two-tasks-seed0.bohr");

    let mut open: Vec<String> = Vec::new();
    let mut rest = html.as_str();
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        // A style or script element holds raw text, not markup: its body is full of angle
        // brackets that are comparisons rather than tags, and a scanner that read them as tags
        // would be checking a different document than a browser does.
        let raw_text = ["style>", "script>"].into_iter().find(|raw| rest.starts_with(raw));
        if let Some(raw) = raw_text {
            let closing = format!("</{raw}");
            let after = rest.find(&closing).map_or(rest.len(), |at| at + closing.len());
            rest = &rest[after..];
            continue;
        }
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if tag.starts_with('!') || tag.ends_with('/') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            let expected = open.pop().unwrap_or_default();
            assert_eq!(expected, name, "closed <{name}> while <{expected}> was open");
            continue;
        }
        let name = tag.split([' ', '\n']).next().unwrap_or(tag).to_owned();
        if !VOID.contains(&name.as_str()) {
            open.push(name);
        }
    }
    assert!(open.is_empty(), "left open: {open:?}");
}

/// Anchors resolve. A "go to the violation" that goes nowhere is worse than not offering it.
#[test]
fn every_identifier_is_unique() {
    let html = page("clock-and-random-seed0.bohr");
    let mut seen: Vec<&str> = Vec::new();
    let mut rest = html.as_str();
    while let Some(at) = rest.find("id=\"") {
        rest = &rest[at + 4..];
        let Some(end) = rest.find('"') else { break };
        let id = &rest[..end];
        assert!(!seen.contains(&id), "two elements are called {id}");
        seen.push(id);
    }
}

/// The one that matters. Everything a page shows came out of a guest, and a report is the file
/// that circulates — so a run must not be able to make the page it is described by do anything.
#[test]
fn a_guest_cannot_write_markup_into_the_page() {
    let bytes = stored("two-tasks-seed0.bohr");
    let bundle = Bundle::parse(&bytes).expect("parses");
    let mut model = bohr_report::RunModel::of(&bundle).expect("a model");

    let hostile = "</script><img src=x onerror=alert(1)><b>\"'&";
    model.config = format!("argv = \"{hostile}\"");
    model.notes = vec![hostile.to_owned()];
    for event in &mut model.events {
        if let bohr_trace::EventKind::Syscall { .. } = event.kind {
            event.kind = bohr_trace::EventKind::Diagnostic {
                code: hostile.to_owned(),
                message: hostile.to_owned(),
            };
        }
    }

    let html = bohr_report::render_model(&model, None);
    // The sequence never survives intact, wherever it landed — as text, as an attribute value, or
    // as data in the script block. Forbidding the characters instead would mean a page could never
    // contain the word, which is not the property that matters.
    assert!(!html.contains(hostile), "a guest's own markup reached the page whole");
    assert!(!html.contains("<img src=x"), "a guest opened an element");
    assert_eq!(
        html.matches("</script>").count(),
        html.matches("<script>").count(),
        "a guest closed a script block"
    );
    assert!(html.contains("&lt;/script&gt;") || html.contains("\\u003c"), "nothing was escaped");
}

/// Two renderings of one bundle are the same bytes. Anything else means something in the page is
/// a fact about when it was made rather than about the run.
#[test]
fn one_bundle_renders_the_same_twice() {
    assert_eq!(page("two-tasks-seed0.bohr"), page("two-tasks-seed0.bohr"));
    assert_eq!(page("large-random-seed0.bohr"), page("large-random-seed0.bohr"));
}

/// A page too large to attach to an issue has stopped being a report.
#[test]
fn a_page_stays_small_enough_to_send() {
    for name in ["two-tasks-seed0.bohr", "clock-and-random-seed0.bohr", "large-random-seed0.bohr"] {
        let html = page(name);
        assert!(html.len() <= bohr_report::MOST_BYTES, "{name} rendered to {} bytes", html.len());
    }
}

/// A short run is drawn whole and needs nothing beside it; the sidecar exists for the runs that
/// are not.
#[test]
fn a_run_drawn_whole_needs_nothing_beside_it() {
    let bytes = stored("two-tasks-seed0.bohr");
    let bundle = Bundle::parse(&bytes).expect("parses");
    let rendered = bohr_report::render(&bundle, Some("events.jsonl")).expect("renders");
    assert!(rendered.sidecar.is_none(), "a whole run was given a sidecar it does not need");
}

/// What a reader is looking for is above what the tool is proud of.
#[test]
fn the_verdict_comes_before_the_machinery() {
    let html = page("two-tasks-seed0.bohr");
    let verdict = html.find("This run found").expect("a verdict");
    let repro = html.find("bohr replay").expect("a reproduction line");
    let glance = html.find("The run at a glance").expect("the facts");
    assert!(verdict < repro, "the machinery came before the verdict");
    assert!(repro < glance, "the facts came before what to do about them");
}

/// A chip carries the slot it stands for, and the slot is not its position in the document.
///
/// **Lanes are written one node at a time and the timeline runs across them**, so with a cluster
/// the third chip in the document is not the third event of the run. With one node the two orders
/// coincide, which is why only a cluster can tell a page selecting by slot from one selecting by
/// document position -- and on a cluster the second opens somebody else's event.
///
/// Checked on the rendered page rather than through a browser: what the script reads is
/// `data-index`, so what a test can decide is that the attribute says the slot and that the slots
/// of one lane are not the positions of its chips.
#[test]
fn a_chip_says_which_event_it_is_and_not_where_it_sits() {
    let bytes = stored("two-tasks-seed0.bohr");
    let bundle = Bundle::parse(&bytes).expect("parses");
    let mut model = bohr_report::RunModel::of(&bundle).expect("a model");

    // Two nodes, alternating: the run's order and the document's order now differ.
    for (at, event) in model.events.iter_mut().enumerate() {
        event.node = bohr_types::NodeId(at as u32 % 2);
    }
    let html = bohr_report::render_model(&model, None);

    // Every chip names a slot, and the slots seen in document order are not 0, 1, 2, …
    let slots: Vec<usize> = html
        .match_indices("data-index=\"")
        .filter_map(|(at, marker)| {
            let rest = &html[at + marker.len()..];
            rest.split('"').next()?.parse().ok()
        })
        .collect();

    assert!(slots.len() > 3, "the page drew {} chips", slots.len());
    assert_ne!(
        slots,
        (0..slots.len()).collect::<Vec<_>>(),
        "the document order and the timeline order are the same, so this proves nothing"
    );

    // And every slot the page names is one the script can look up in EVENTS.
    let drawn = html.matches("\"id\":").count();
    for slot in &slots {
        assert!(*slot < drawn, "a chip names slot {slot} of {drawn} events");
    }
}
