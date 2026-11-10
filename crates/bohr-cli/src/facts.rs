//! The facts of one run, read off a completed run and the supervisor's own use of the host.

pub(crate) use bohr_findings::facts::Facts;

use crate::execute::Completed;

/// The facts of a completed run.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn of(completed: &Completed) -> Facts {
    let outcome = &completed.report.outcome;
    let (outcome, detail) = (outcome.kind().name().to_owned(), outcome.detail().to_owned());
    let (probe_asked, probe_answered) = bohr_findings::facts::probe_answers(&completed.sdk_records);
    let cpu = own_cpu_ms();
    Facts {
        codes: bohr_types::Code::all_in(&detail).iter().map(ToString::to_string).collect(),
        outcome,
        detail,
        digest: completed.report.digest.to_string(),
        events: completed.report.events,
        syscalls: completed.stats.syscalls,
        stops: completed.report.metrics.stops,
        virtual_ns: completed.report.ticks.get(),
        adversity: completed.report.adversity,
        probe_asked,
        probe_answered,
        stop: format!("{:?}", completed.report.stop_reason),
        log_bytes: completed.log.len() as u64,
        peak_rss_kib: own_peak_rss_kib(),
        cpu_user_ms: cpu.0,
        cpu_system_ms: cpu.1,
        guest_cpu_ms: cpu.2,
        degraded: completed.degradations.clone(),
    }
}

/// The supervisor's peak resident set, read from `/proc/self/status`; 0 where there is none.
///
/// Read after the run, so it covers the whole run at no cost to it.
fn own_peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
            })
        })
        .unwrap_or(0)
}

/// The supervisor's user and system time and its reaped guests' time, in milliseconds, read from
/// `/proc/self/stat`; zeros where there is none.
///
/// The fields count 10 ms ticks, the unit x86-64 Linux reports to user space.
fn own_cpu_ms() -> (u64, u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0, 0);
    };
    // The command name is in parentheses and may hold spaces; the fields that follow it are
    // numbered from 3, so utime, stime, cutime and cstime are the 12th to the 15th after it.
    let after = text.rsplit_once(')').map_or("", |(_, rest)| rest);
    let fields: Vec<u64> =
        after.split_whitespace().skip(11).take(4).map(|f| f.parse().unwrap_or(0)).collect();
    match fields[..] {
        [user, system, child_user, child_system] => {
            (user * 10, system * 10, (child_user + child_system) * 10)
        }
        _ => (0, 0, 0),
    }
}
