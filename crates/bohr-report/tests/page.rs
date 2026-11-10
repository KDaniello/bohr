//! The page, checked for the things a reader would never notice and an attacker would.
//!
//! Appearance is reviewed by eye. These check what a test can: no injected markup, closed elements,
//! identical bytes for one bundle, and a size small enough to attach.

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

/// Every element that opens closes, in order.
#[test]
fn every_element_closes_in_the_order_it_opened() {
    const VOID: [&str; 6] = ["meta", "br", "hr", "img", "input", "link"];
    let html = page("two-tasks-seed0.bohr");

    let mut open: Vec<String> = Vec::new();
    let mut rest = html.as_str();
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        // Style and script bodies are raw text, as a browser reads them.
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

/// Every anchor resolves.
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

/// A guest cannot inject markup into its page.
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
    // The sequence never survives intact, as text, attribute or script data.
    assert!(!html.contains(hostile), "a guest's own markup reached the page whole");
    assert!(!html.contains("<img src=x"), "a guest opened an element");
    assert_eq!(
        html.matches("</script>").count(),
        html.matches("<script>").count(),
        "a guest closed a script block"
    );
    assert!(html.contains("&lt;/script&gt;") || html.contains("\\u003c"), "nothing was escaped");
}

/// Two renderings of one bundle are the same bytes.
#[test]
fn one_bundle_renders_the_same_twice() {
    assert_eq!(page("two-tasks-seed0.bohr"), page("two-tasks-seed0.bohr"));
    assert_eq!(page("large-random-seed0.bohr"), page("large-random-seed0.bohr"));
}

/// A page stays small enough to attach to an issue.
#[test]
fn a_page_stays_small_enough_to_send() {
    for name in ["two-tasks-seed0.bohr", "clock-and-random-seed0.bohr", "large-random-seed0.bohr"] {
        let html = page(name);
        assert!(html.len() <= bohr_report::MOST_BYTES, "{name} rendered to {} bytes", html.len());
    }
}

/// A short run is drawn whole, with no sidecar.
#[test]
fn a_run_drawn_whole_needs_nothing_beside_it() {
    let bytes = stored("two-tasks-seed0.bohr");
    let bundle = Bundle::parse(&bytes).expect("parses");
    let rendered = bohr_report::render(&bundle, Some("events.jsonl")).expect("renders");
    assert!(rendered.sidecar.is_none(), "a whole run was given a sidecar it does not need");
}

/// The verdict and command come before the machinery.
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
/// Lanes are written one node at a time, so in a cluster document order differs from run order.
/// The script reads `data-index`, which this checks names the slot.
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

    // Every slot named is in EVENTS.
    let drawn = html.matches("\"id\":").count();
    for slot in &slots {
        assert!(*slot < drawn, "a chip names slot {slot} of {drawn} events");
    }
}
