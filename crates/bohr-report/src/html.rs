//! The page, written top to bottom in the order somebody reads it.
//!
//! Written for a maintainer new to the tool: the verdict first, the reproduction command second,
//! the machinery below.
//!
//! One file, no framework, no build step, nothing fetched, so it opens from a bug tracker years
//! later. See `docs/ARCHITECTURE.md` §5.13.

use std::fmt::Write as _;

use bohr_trace::{Event, EventKind};

use crate::{describe, escape, model::RunModel, window::Window};

/// The stylesheet and the behaviour, included as written.
const STYLE: &str = include_str!("../assets/report.css");
const BEHAVIOUR: &str = include_str!("../assets/report.js");

/// Events shown before the point of violation in the card that explains it.
///
/// A handful of steps, enough to tell the story.
const LEADING_UP: usize = 10;

/// Renders the whole page.
#[must_use]
/// A rendered page, and whether the timeline on it left anything out.
pub(crate) struct Page {
    pub(crate) html: String,
    /// Whether the window elided events, which is also whether a sidecar is worth writing.
    pub(crate) partial: bool,
}

pub(crate) fn render(model: &RunModel, sidecar: Option<&str>) -> String {
    render_with(model, sidecar, None, &[]).html
}

pub(crate) fn render_with(
    model: &RunModel,
    sidecar: Option<&str>,
    bundle_name: Option<&str>,
    lanes: &[crate::chain::Lane],
) -> Page {
    let violation_at = model
        .violation
        .as_ref()
        .and_then(|violation| model.events.iter().position(|e| e.id == violation.event));
    let window = Window::of(&model.events, violation_at);

    let mut page = String::with_capacity(64 * 1024);
    let title = format!("bohr · {}", describe::outcome_word(&model.manifest.outcome));
    let _ = writeln!(
        page,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>\n{STYLE}</style>\n</head>\n<body>",
        escape::text(&title)
    );

    degraded_strip(model, &mut page);
    page.push_str("<main>\n");
    verdict(model, &mut page);
    reproduction(model, bundle_name, &mut page);
    orientation(&mut page);
    violation_card(model, violation_at, &mut page);
    chain_timeline(model, lanes, &mut page);
    at_a_glance(model, &mut page);
    timeline(model, &window, violation_at, sidecar, &mut page);
    faults(model, &mut page);
    draws(model, &mut page);
    promises(model, &mut page);
    shown_to_the_guest(model, &mut page);
    programs(model, &mut page);
    configuration(model, &mut page);
    notes(model, &mut page);
    footer(model, sidecar, &window, &mut page);
    page.push_str("</main>\n");

    events_data(model, &window, &mut page);
    let _ = write!(page, "<script>\n{BEHAVIOUR}</script>\n</body>\n</html>\n");
    Page { partial: window.is_partial(), html: page }
}

/// Above everything, and impossible to skip.
///
/// Degraded determinism weakens every number below, so it is stated first.
fn degraded_strip(model: &RunModel, page: &mut String) {
    let mut reasons: Vec<String> = model.manifest.determinism.reasons.clone();
    if model.host.as_ref().is_some_and(|host| !host.traps_rdrand) {
        reasons.push(
            "the processor's hardware random-number instruction was not trapped in this run"
                .to_owned(),
        );
    }
    if model.manifest.determinism.level == "full" && reasons.is_empty() {
        return;
    }
    page.push_str(
        "<div class=\"degraded\">This run was not fully deterministic. What is below \
                   may not reproduce.\n<ul>\n",
    );
    for reason in reasons {
        let _ = writeln!(page, "<li>{}</li>", escape::text(&reason));
    }
    page.push_str("</ul>\n</div>\n");
}

/// One sentence, in the words somebody who has never used this tool would use.
fn verdict(model: &RunModel, page: &mut String) {
    // Each arm writes the whole heading, since it carries the verdict.
    match (&model.manifest.outcome, &model.violation) {
        (bohr_trace::Outcome::Clean, _) => {
            let _ = writeln!(
                page,
                "<h1><span class=\"good\">This run found nothing.</span></h1>\n\
                 <p class=\"verdict\">{} events, {} decisions, all as recorded.</p>",
                model.manifest.counters.events, model.manifest.counters.draws
            );
        }
        (_, Some(violation)) => {
            let where_at = model
                .events
                .iter()
                .find(|e| e.id == violation.event)
                .map_or_else(String::new, |e| format!(" on node {}", e.node.get()));
            let _ = writeln!(
                page,
                "<h1><span class=\"bad\">This run found a bug.</span></h1>\n\
                 <p class=\"verdict\">The promise <code>{}</code> did not hold{}.</p>",
                escape::text(&violation.name),
                escape::text(&where_at)
            );
        }
        (outcome, None) => {
            // The same verdict every other view prints.
            let verdict = crate::narrate::verdict(outcome);
            let _ = writeln!(
                page,
                "<h1><span class=\"bad\">This run found a bug.</span></h1>\n\
                 <p class=\"verdict\">{} <code>{}</code></p>",
                escape::text(&verdict.headline),
                escape::text(&verdict.code)
            );
        }
    }
}

/// The chain, node by node: what each answered the probe over virtual time, beside its faults.
fn chain_timeline(model: &RunModel, lanes: &[crate::chain::Lane], page: &mut String) {
    let Some(svg) = crate::chain::svg(model, lanes) else {
        return;
    };
    page.push_str(
        "<h2>The chain, node by node</h2>\n<div class=\"card\">\n<p>Each dot is an answer the \
         probe got, drawn the first time a node gave it: across is virtual time, up is the key it \
         answered for, the colour is the value. A dot at a key a lane already showed, in another \
         colour, is a value that changed; shaded stretches are a node down. Hover a dot for its \
         words.</p>\n",
    );
    page.push_str(&svg);
    page.push_str("</div>\n");
}

/// The line a maintainer runs, above the fold and copyable.
fn reproduction(model: &RunModel, bundle_name: Option<&str>, page: &mut String) {
    // The bundle's real name where the caller knows it, else a placeholder. Escaped either way,
    // since the placeholder has angle brackets.
    let bundle = escape::text(bundle_name.unwrap_or("<bundle>"));
    let _ = write!(
        page,
        "<div class=\"repro\"><code id=\"repro\">bohr replay {}</code>\
         <button data-copy=\"repro\">copy</button></div>\n\
         <dl class=\"facts\"><dt>seed</dt><dd class=\"mono\">{}</dd>\
         <dt>trace digest</dt><dd class=\"mono\">{}</dd></dl>\n",
        bundle,
        model.manifest.seed,
        escape::text(&model.manifest.trace_digest)
    );
}

/// Sixty words that make the rest of the page readable by somebody who arrived from an issue.
fn orientation(page: &mut String) {
    page.push_str(
        "<p class=\"orientation\">This file is one run of a program under Bohr, which executes it \
         in a simulated world where every decision that would normally be left to chance — thread \
         order, network delays, disk failures — was drawn from a single seed. That is why the run \
         can be replayed exactly. What follows is the whole run, not a sample of it.</p>\n",
    );
}

/// The most important card: what broke, where it is written, and the steps that led there.
fn violation_card(model: &RunModel, at: Option<usize>, page: &mut String) {
    let (Some(violation), Some(at)) = (&model.violation, at) else {
        return;
    };
    let event = &model.events[at];
    page.push_str("<h2>The point of violation</h2>\n<div class=\"card violation\">\n");
    let _ = write!(
        page,
        "<h3>{}</h3>\n<p>{}</p>\n<dl class=\"facts\">\
         <dt>event</dt><dd class=\"mono\">{}</dd>\
         <dt>virtual time</dt><dd class=\"mono\">{}</dd>\
         <dt>node</dt><dd>{}</dd>",
        escape::text(&violation.name),
        escape::text(describe::assertion_promise(violation.kind)),
        event.id.get(),
        event.tick.get(),
        event.node.get()
    );
    if let Some(location) = &violation.location {
        let _ = write!(
            page,
            "<dt>written at</dt><dd class=\"mono\">{}:{} in {}</dd>",
            escape::text(&location.file),
            location.line,
            escape::text(&location.function)
        );
    }
    page.push_str("</dl>\n");

    let from = at.saturating_sub(LEADING_UP);
    page.push_str(
        "<p>What happened immediately before it:</p>\n<div class=\"scroll\"><table>\
                   <tr><th>event</th><th>time</th><th>node</th><th>what</th></tr>\n",
    );
    for event in &model.events[from..at] {
        row_for(event, page);
    }
    page.push_str("</table></div>\n</div>\n");
}

fn row_for(event: &Event, page: &mut String) {
    let _ = writeln!(
        page,
        "<tr><td class=\"num mono\">{}</td><td class=\"num mono\">{}</td><td>{}</td><td>{}</td></tr>",
        event.id.get(),
        event.tick.get(),
        event.node.get(),
        escape::text(said_about(event).trim())
    );
}

/// The run's own account of itself, including the numbers any bug report needs.
fn at_a_glance(model: &RunModel, page: &mut String) {
    let manifest = &model.manifest;
    let counters = &manifest.counters;
    page.push_str("<h2>The run at a glance</h2>\n<div class=\"card\"><dl class=\"facts\">\n");
    let _ = write!(
        page,
        "<dt>outcome</dt><dd>{}</dd>\
         <dt>run</dt><dd class=\"mono\">{}</dd>\
         <dt>written</dt><dd>{}</dd>\
         <dt>recorded</dt><dd>{} events · {} syscalls · {} decisions · {} tasks on {} nodes</dd>\
         <dt>interleaved by</dt><dd>{}{}</dd>\
         <dt>produced by</dt><dd>{} / {} on {}</dd>\
         <dt>tool</dt><dd class=\"mono\">{} · tape abi {} · bundle format {} · sdk schema {}</dd>",
        escape::text(describe::outcome_word(&manifest.outcome)),
        escape::text(&manifest.run_id),
        escape::text(&manifest.created_utc),
        counters.events,
        counters.syscalls,
        counters.draws,
        counters.tasks,
        counters.nodes,
        escape::text(&manifest.policy.scheduler),
        manifest.policy.pct_depth.map(|d| format!(" at depth {d}")).unwrap_or_default(),
        escape::text(&manifest.provenance.world_model.to_string()),
        escape::text(&manifest.provenance.oracle.to_string()),
        escape::text(&manifest.provenance.backend),
        escape::text(&manifest.tool_version),
        manifest.tape_abi,
        manifest.bundle_format,
        manifest.sdk_schema
    );
    if let Some(from) = &manifest.reduced_from {
        let _ = write!(page, "<dt>reduced from</dt><dd class=\"mono\">{}</dd>", escape::text(from));
    }
    page.push_str("</dl></div>\n");
}

/// One lane per node, every event in the same column across lanes.
fn timeline(
    model: &RunModel,
    window: &Window,
    violation_at: Option<usize>,
    sidecar: Option<&str>,
    page: &mut String,
) {
    page.push_str("<h2>Timeline</h2>\n");
    page.push_str(
        "<p class=\"orientation\">Position is order, not elapsed time: a run spends almost all of \
         its virtual time waiting, and a clock axis would draw the whole of it as one stripe. The \
         clock is on every event. Events in the same column happened next to each other.</p>\n",
    );

    let nodes = model.nodes();
    page.push_str("<div class=\"controls\"><span><b id=\"shown-count\">0</b> events shown</span>");
    for node in &nodes {
        let _ = write!(
            page,
            "<label><input class=\"f-node\" type=\"checkbox\" value=\"{}\" checked> node {}</label>",
            node.get(),
            node.get()
        );
    }
    for family in ["syscall", "packet", "fault", "assertion", "schedule", "node"] {
        let _ = write!(
            page,
            "<label><input class=\"f-kind\" type=\"checkbox\" value=\"{family}\" checked> \
             {family}</label>"
        );
    }
    if let Some(at) = violation_at {
        let shown = window.shown().iter().position(|position| *position == at);
        if let Some(shown) = shown {
            let _ = write!(page, "<button data-jump=\"{shown}\">go to the violation</button>");
        }
    }
    page.push_str("</div>\n<div class=\"scroll\"><div class=\"lanes\">\n");

    let gaps = window.gaps();
    for node in &nodes {
        let _ = write!(page, "<div class=\"lane\" data-node=\"{}\">", node.get());
        let _ = write!(page, "<span class=\"name\">node {}</span>", node.get());
        for (slot, at) in window.shown().iter().enumerate() {
            let missing = gaps.get(slot).copied().unwrap_or(0);
            if missing > 0 {
                let _ = write!(page, "<span class=\"gap\">{missing} more</span>");
            }
            let event = &model.events[*at];
            if event.node != *node {
                page.push_str("<span></span>");
                continue;
            }
            let _ = write!(
                page,
                "<button class=\"ev k-{family}\" data-node=\"{node}\" data-family=\"{family}\" \
                 data-index=\"{slot}\"{violation} title=\"{title}\"></button>",
                family = family_of(&event.kind),
                node = event.node.get(),
                slot = slot,
                violation = if Some(*at) == violation_at { " data-violation=\"1\"" } else { "" },
                title = escape::attr(said_about(event).trim())
            );
        }
        // The gap after the last shown event: `gaps()` has one more entry than there are slots.
        if let Some(missing) = gaps.last().copied().filter(|missing| *missing > 0) {
            let _ = write!(page, "<span class=\"gap\">{missing} more</span>");
        }
        page.push_str("</div>\n");
    }
    page.push_str(
        "</div></div>\n<div class=\"card\" id=\"detail\">\
                   <span class=\"none\">Choose an event to see what it was.</span></div>\n",
    );

    if window.is_partial() {
        let where_rest = sidecar.map_or_else(
            || " The whole log is in the bundle this page was made from.".to_owned(),
            |name| format!(" The whole log is beside this file, in {}.", escape::text(name)),
        );
        let _ = writeln!(
            page,
            "<p class=\"empty\">{} of {} events are not drawn above; everything the world did and \
             everything the guest said about itself is kept wherever it happened.{}</p>",
            window.elided(),
            model.events.len(),
            where_rest
        );
    }
}

/// What was done to the run, which is usually the answer.
fn faults(model: &RunModel, page: &mut String) {
    page.push_str("<h2>The faults that fired</h2>\n");
    let fired: Vec<&Event> = model
        .events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::FaultFired { .. }))
        .collect();
    if fired.is_empty() {
        page.push_str(
            "<p class=\"empty\">No faults were injected in this run: whatever happened, the world \
             did not make it happen.</p>\n",
        );
        return;
    }
    page.push_str(
        "<div class=\"scroll\"><table><tr><th>event</th><th>time</th><th>what</th>\
                   <th>to</th></tr>\n",
    );
    for event in fired {
        if let EventKind::FaultFired { fault, target } = &event.kind {
            let _ = writeln!(
                page,
                "<tr><td class=\"num mono\">{}</td><td class=\"num mono\">{}</td><td>{}</td>\
                 <td>{}</td></tr>",
                event.id.get(),
                event.tick.get(),
                escape::text(describe::fault_action(*fault)),
                escape::text(&describe::fault_target(target))
            );
        }
    }
    page.push_str("</table></div>\n");
}

/// The decisions that produced what is above, in the words of what they decided.
fn draws(model: &RunModel, page: &mut String) {
    page.push_str("<h2>The decisions that mattered</h2>\n");
    let by_event = model.draws_by_event();
    let interesting: Vec<(&Event, &bohr_tape::DrawRecord)> = model
        .events
        .iter()
        .filter_map(|event| by_event.get(&event.id).map(|draws| (event, draws)))
        .flat_map(|(event, draws)| draws.iter().map(move |draw| (event, *draw)))
        .filter(|(_, draw)| worth_showing(draw.site))
        .take(200)
        .collect();

    if interesting.is_empty() {
        page.push_str("<p class=\"empty\">Nothing in this run turned on a drawn decision.</p>\n");
        return;
    }
    page.push_str(
        "<div class=\"scroll\"><table><tr><th>event</th><th>time</th><th>decided</th>\
                   <th>came out as</th></tr>\n",
    );
    for (event, draw) in interesting {
        let named = bohr_tape::DrawSite::from_code(draw.site);
        let _ = writeln!(
            page,
            "<tr><td class=\"num mono\">{}</td><td class=\"num mono\">{}</td><td>{}</td>\
             <td class=\"num mono\">{}</td></tr>",
            event.id.get(),
            event.tick.get(),
            escape::text(named.map_or("a decision this build has no name for", |site| site.name())),
            draw.value
        );
    }
    page.push_str("</table></div>\n");
}

/// Whether a draw site is one a reader is looking for, or scheduling noise.
const fn worth_showing(site: u16) -> bool {
    !matches!(
        bohr_tape::DrawSite::from_code(site),
        Some(bohr_tape::DrawSite::SchedPick | bohr_tape::DrawSite::SchedPriority)
    )
}

/// What the guest promised about itself, including what it never managed to reach.
fn promises(model: &RunModel, page: &mut String) {
    page.push_str("<h2>What the guest said about itself</h2>\n");
    let assertions = model.assertions();
    if assertions.is_empty() {
        page.push_str(
            "<p class=\"empty\">This guest states nothing about itself. Everything above was \
             found by watching what it did rather than by asking it.</p>\n",
        );
        return;
    }
    page.push_str(
        "<div class=\"scroll\"><table><tr><th>promise</th><th>kind</th><th>held</th>\
                   <th>written at</th></tr>\n",
    );
    for assertion in assertions {
        let _ = writeln!(
            page,
            "<tr><td>{}</td><td>{:?}</td><td>{}</td><td class=\"mono\">{}:{}</td></tr>",
            escape::text(&assertion.message),
            assertion.assert_type,
            if assertion.condition { "yes" } else { "NO" },
            escape::text(&assertion.location.file),
            assertion.location.begin_line
        );
    }
    page.push_str("</table></div>\n");
}

/// What the guest was shown of the machine, which is never what the machine is.
fn shown_to_the_guest(model: &RunModel, page: &mut String) {
    let Some(host) = &model.host else {
        return;
    };
    page.push_str("<h2>What the guest was shown</h2>\n<div class=\"card\">\n");
    page.push_str(
        "<p class=\"orientation\">Not what this machine is — what the guest was told it was, so \
         the same run reproduces on a different one.</p>\n<dl class=\"facts\">\n",
    );
    let _ = write!(
        page,
        "<dt>kernel</dt><dd>{}</dd><dt>page size</dt><dd>{}</dd>\
         <dt>processors</dt><dd>{}</dd>\
         <dt>randomisation</dt><dd>{}</dd>\
         <dt>timestamp counter</dt><dd>{}</dd>\
         <dt>processor identification</dt><dd>{}</dd>\
         <dt>hardware randomness</dt><dd>{}</dd>",
        escape::text(&host.kernel_release_presented),
        host.page_size,
        host.cpu_count_presented,
        if host.aslr_disabled { "off" } else { "on" },
        if host.traps_rdtsc { "trapped" } else { "NOT trapped" },
        if host.traps_cpuid { "trapped" } else { "NOT trapped" },
        if host.traps_rdrand { "trapped" } else { "NOT trapped" }
    );
    page.push_str("</dl></div>\n");
}

fn programs(model: &RunModel, page: &mut String) {
    if model.binaries.is_empty() {
        return;
    }
    page.push_str(
        "<h2>The programs</h2>\n<div class=\"scroll\"><table>\
                   <tr><th>role</th><th>path</th><th>fingerprint</th></tr>\n",
    );
    for binary in &model.binaries {
        let _ = writeln!(
            page,
            "<tr><td>{}</td><td class=\"mono\">{}</td><td class=\"mono\">{}</td></tr>",
            escape::text(&binary.role),
            escape::text(&binary.path),
            escape::text(&binary.blake3)
        );
    }
    page.push_str("</table></div>\n");
}

fn configuration(model: &RunModel, page: &mut String) {
    if model.config.trim().is_empty() {
        return;
    }
    page.push_str("<h2>The world it ran in</h2>\n");
    let _ = writeln!(page, "<pre class=\"config\">{}</pre>", escape::text(&model.config));
}

fn notes(model: &RunModel, page: &mut String) {
    if model.notes.is_empty() {
        return;
    }
    page.push_str("<h2>Notes</h2>\n<div class=\"card\">\n");
    for note in &model.notes {
        let _ = writeln!(page, "<p>{}</p>", escape::text(note));
    }
    page.push_str("</div>\n");
}

/// No timestamp, so one bundle always renders to the same bytes.
fn footer(model: &RunModel, sidecar: Option<&str>, window: &Window, page: &mut String) {
    let _ = write!(
        page,
        "<footer>Generated by bohr {} from a stored run. Everything above came out of that one \
         file.",
        escape::text(&model.manifest.tool_version)
    );
    if let Some(name) = sidecar.filter(|_| window.is_partial()) {
        let _ =
            write!(page, " The events not drawn here are in {}, one per line.", escape::text(name));
    }
    if let Some(damage) = &model.truncated {
        let _ = write!(
            page,
            " <b>The stored log ends early ({}), so this run is shown as far as it was recorded \
             and no further.</b>",
            escape::text(damage)
        );
    }
    page.push_str("</footer>\n");
}

/// The events the page can show details for, as data.
fn events_data(model: &RunModel, window: &Window, page: &mut String) {
    let by_event = model.draws_by_event();
    let mut json = String::from("[");
    for (at, position) in window.shown().iter().enumerate() {
        let event = &model.events[*position];
        if at > 0 {
            json.push(',');
        }
        let decided: Vec<String> = by_event
            .get(&event.id)
            .map(|draws| {
                draws
                    .iter()
                    .filter_map(|draw| bohr_tape::DrawSite::from_code(draw.site))
                    .map(|site| site.name().to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let _ = write!(
            json,
            "{{\"id\":{},\"tick\":{},\"node\":{},\"task\":{},\"said\":{},\"draws\":{}}}",
            event.id.get(),
            event.tick.get(),
            event.node.get(),
            event.task.get(),
            serde_json::to_string(said_about(event).trim()).unwrap_or_else(|_| "\"\"".to_owned()),
            serde_json::to_string(&decided).unwrap_or_else(|_| "[]".to_owned())
        );
    }
    json.push(']');
    let _ =
        write!(page, "<script>\nconst EVENTS = {};\n</script>\n", escape::json_in_script(&json));
}

/// What an event is, in the words the rest of the tool uses for it.
fn said_about(event: &Event) -> String {
    let line = crate::text::event_line(event);
    line.split_once("task").map_or(line.clone(), |(_, rest)| {
        rest.split_once(' ').map_or(rest.to_owned(), |(_, what)| what.to_owned())
    })
}

/// Which shape a chip is drawn as, so a printed page and a colour-blind reader both work.
const fn family_of(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Syscall { .. } | EventKind::DiskOp { .. } => "syscall",
        EventKind::PacketSent { .. } | EventKind::PacketFate { .. } => "packet",
        EventKind::FaultFired { .. } => "fault",
        EventKind::NodeState { .. }
        | EventKind::TaskSpawned { .. }
        | EventKind::TaskExited { .. } => "node",
        EventKind::Assertion { .. }
        | EventKind::SdkRecord { .. }
        | EventKind::Diagnostic { .. } => "assertion",
        EventKind::Schedule { .. }
        | EventKind::Waited { .. }
        | EventKind::Woke { .. }
        | EventKind::ClockRead { .. }
        | EventKind::Domain { .. } => "schedule",
    }
}
