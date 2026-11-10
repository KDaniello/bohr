//! What this host can do, what it cannot, and what to type about it.
//!
//! Every check reports pass, warn or fail, and every failure carries the command that shows the
//! setting behind it. Nothing here changes a host setting; the machine's owner decides. See
//! `docs/API.md` §7.5.

use crate::CliError;

/// How a check came out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Verdict {
    /// The capability is there.
    Pass,
    /// Runs work but lose speed or a guarantee, which is reported.
    Warn,
    /// Runs cannot be trusted here.
    Fail,
    /// Nothing could be checked, usually because this build has no interceptor.
    #[cfg_attr(all(target_os = "linux", target_arch = "x86_64"), allow(dead_code))]
    Skip,
}

impl Verdict {
    const fn mark(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "FAIL",
            Self::Skip => "skip",
        }
    }

    /// The verdict in lower case, as `--json` writes it.
    const fn status(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// One thing that was checked.
pub(crate) struct Check {
    /// What was checked.
    pub(crate) name: &'static str,
    /// How it came out.
    pub(crate) verdict: Verdict,
    /// What was found.
    pub(crate) detail: String,
    /// What to type to see the setting behind it, where a failure has one.
    pub(crate) remedy: Option<String>,
}

impl Check {
    fn new(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> Self {
        Self { name, verdict, detail: detail.into(), remedy: None }
    }

    fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

/// Runs every check and prints what it found.
///
/// Exits 0 when every check passed and 4 when one failed, the code a run uses for "determinism
/// could not be guaranteed".
pub(crate) fn run(target: Option<&str>, list_syscalls: bool) -> Result<u8, CliError> {
    if list_syscalls {
        return Ok(print_syscalls());
    }

    let mut checks = host_checks();
    if let Some(path) = target {
        checks.extend(target_checks(path)?);
    }

    let worst = worst_of(&checks);
    if crate::json::on() {
        crate::json::emit("doctor", checks_json(&checks, worst));
        return Ok(if worst == Verdict::Fail { crate::exit::DEGRADED } else { crate::exit::CLEAN });
    }

    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for check in &checks {
        println!("{:width$}  {}  {}", check.name, check.verdict.mark(), check.detail);
        // Printed only for a problem, so a passing check shows no instructions.
        if check.verdict != Verdict::Pass
            && let Some(remedy) = &check.remedy
        {
            println!("{:width$}        {remedy}", "");
        }
    }

    println!();
    match worst {
        Verdict::Fail => {
            println!("this host cannot run guests deterministically; see the failures above");
            Ok(crate::exit::DEGRADED)
        }
        Verdict::Warn => {
            println!("this host can run guests, with the losses noted above");
            Ok(crate::exit::CLEAN)
        }
        _ => {
            println!("this host can run guests deterministically");
            Ok(crate::exit::CLEAN)
        }
    }
}

/// The worst verdict among `checks`; a skipped check counts as passed.
fn worst_of(checks: &[Check]) -> Verdict {
    let mut worst = Verdict::Pass;
    for check in checks {
        if check.verdict == Verdict::Fail {
            worst = Verdict::Fail;
        } else if check.verdict == Verdict::Warn && worst == Verdict::Pass {
            worst = Verdict::Warn;
        }
    }
    worst
}

/// Every check as a row, and the worst of them as the host's verdict.
///
/// Every row carries its remedy; hiding it under a pass applies only to the text.
fn checks_json(checks: &[Check], worst: Verdict) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "status": c.verdict.status(),
                "detail": c.detail,
                "remedy": c.remedy,
            })
        })
        .collect();
    serde_json::json!({ "verdict": worst.status(), "checks": rows })
}

/// The support table, so a target can be compared against it by eye.
fn print_syscalls() -> u8 {
    if crate::json::on() {
        crate::json::emit("doctor", syscalls_json());
        return crate::exit::CLEAN;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    for (nr, name) in bohr_backend_ptrace::syscall_table() {
        println!("{nr:>4}  {name}");
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    println!("this build has no syscall backend, so it has no table to print");
    crate::exit::CLEAN
}

/// The support table as rows; `backend` is false, and the table empty, in a build without one.
fn syscalls_json() -> serde_json::Value {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let (backend, rows): (bool, Vec<serde_json::Value>) = (
        true,
        bohr_backend_ptrace::syscall_table()
            .map(|(nr, name)| serde_json::json!({ "nr": nr, "name": name }))
            .collect(),
    );
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let (backend, rows): (bool, Vec<serde_json::Value>) = (false, Vec::new());
    serde_json::json!({ "backend": backend, "syscalls": rows })
}

/// What a build without an interceptor reports: that it cannot check this host.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn host_checks() -> Vec<Check> {
    vec![
        Check::new(
            "syscall backend",
            Verdict::Skip,
            "this build has none; guests run only through `bohr selftest`",
        )
        .with_remedy("build on Linux x86-64 to intercept a program on disk"),
    ]
}

/// Everything a host is asked about.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn host_checks() -> Vec<Check> {
    use bohr_backend_ptrace::doctor::{self as probe, Probe};

    let verdict = |p: Probe, absent: Verdict| match p {
        Probe::Present => Verdict::Pass,
        Probe::Absent => absent,
        Probe::Unknown => Verdict::Warn,
    };
    let describe = |p: Probe, yes: &str, no: &str| match p {
        Probe::Present => yes.to_owned(),
        Probe::Absent => no.to_owned(),
        Probe::Unknown => "could not be established from here".to_owned(),
    };

    let mut checks = Vec::new();

    if probe::is_wsl() {
        checks.push(
            Check::new(
                "platform",
                Verdict::Fail,
                "this is a Windows subsystem, which cannot trap processor identification",
            )
            .with_remedy("run on Linux directly, or in a virtual machine that exposes cpuid_fault"),
        );
    }

    let tracing = probe::can_trace();
    checks.push(
        Check::new(
            "ptrace",
            verdict(tracing, Verdict::Fail),
            describe(tracing, "a child can be traced", "tracing was refused"),
        )
        .with_remedy("sysctl kernel.yama.ptrace_scope   # 0 or 1 is needed"),
    );

    let seccomp = probe::can_install_seccomp();
    checks.push(Check::new(
        "seccomp",
        verdict(seccomp, Verdict::Fail),
        describe(seccomp, "a filter can be installed", "filters are unavailable"),
    ));

    // A failure: without its own pid namespace a guest's threads get host numbers, the kernel
    // writes them into guest memory, and two runs of one seed differ.
    let namespaces = probe::can_use_namespaces();
    checks.push(
        Check::new(
            "namespaces",
            verdict(namespaces, Verdict::Fail),
            describe(
                namespaces,
                "a guest can be isolated",
                "a guest would see the host's own machine, and threaded runs diverge",
            ),
        )
        .with_remedy(namespace_remedy()),
    );

    let aslr = probe::can_disable_aslr();
    checks.push(Check::new(
        "address layout",
        verdict(aslr, Verdict::Fail),
        describe(aslr, "randomisation can be turned off", "randomisation cannot be turned off"),
    ));

    let tsc = probe::can_trap_rdtsc();
    checks.push(Check::new(
        "timestamp counter",
        verdict(tsc, Verdict::Fail),
        describe(tsc, "reads can be trapped", "reads cannot be trapped"),
    ));

    let cpuid = probe::can_trap_cpuid();
    checks.push(
        Check::new(
            "processor identification",
            verdict(cpuid, Verdict::Warn),
            describe(cpuid, "reads can be trapped", "reads reach the processor; runs are degraded"),
        )
        .with_remedy("grep cpuid_fault /proc/cpuinfo   # a hardware capability, not a setting"),
    );

    let memory = probe::can_read_memory_directly();
    checks.push(Check::new(
        "guest memory",
        verdict(memory, Verdict::Warn),
        describe(memory, "can be read directly", "falls back to a word at a time, far slower"),
    ));

    let branches = probe::can_count_branches();
    checks.push(
        Check::new(
            "branch counters",
            verdict(branches, Verdict::Warn),
            describe(branches, "available", "unavailable; progress is measured by syscalls"),
        )
        .with_remedy("sysctl kernel.perf_event_paranoid   # 2 or below"),
    );

    let affinity = probe::can_pin_to_one_core();
    checks.push(Check::new(
        "core pinning",
        verdict(affinity, Verdict::Warn),
        describe(affinity, "a guest can be pinned to one core", "a guest cannot be pinned"),
    ));

    // The host must support every feature the guest is told it has: the guest executes them on
    // the real processor and would crash on a missing one.
    let promised =
        probe::supports(bohr_world::cpu::identify(1, 0)[2], bohr_world::cpu::identify(7, 0)[1]);
    checks.push(
        Check::new(
            "presented processor",
            verdict(promised, Verdict::Fail),
            describe(
                promised,
                "this host can do what the guest is told it has",
                "the guest is told of features this host does not have",
            ),
        )
        .with_remedy("a machine from 2013 or later; the profile is the x86-64-v3 level"),
    );

    for name in
        ["kernel.yama.ptrace_scope", "kernel.perf_event_paranoid", "user.max_user_namespaces"]
    {
        if let Some(value) = probe::sysctl(name) {
            checks.push(Check::new("setting", Verdict::Pass, format!("{name} = {value}")));
        }
    }

    checks
}

/// What a program will need before it can be run.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn target_checks(path: &str) -> Result<Vec<Check>, CliError> {
    use bohr_backend_ptrace::doctor;

    let bytes = std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    let Some(report) = doctor::inspect_target(&bytes) else {
        return Ok(vec![Check::new(
            "target",
            Verdict::Fail,
            format!("{path} is not an x86-64 ELF program"),
        )]);
    };

    let mut checks = Vec::new();
    match &report.interpreter {
        Some(loader) => checks.push(
            Check::new(
                "target loader",
                // A dynamic program needs its loader in the guest's filesystem, which the world
                // does not provide, so the run would fail.
                Verdict::Warn,
                format!("needs {loader}, which the virtual filesystem does not provide"),
            )
            .with_remedy("link the program statically, as testdata/guests does"),
        ),
        None => checks.push(Check::new("target loader", Verdict::Pass, "statically linked")),
    }

    if report.hardware_random.is_empty() {
        checks.push(Check::new("hardware randomness", Verdict::Pass, "no RDRAND or RDSEED found"));
    } else {
        let first = report.hardware_random.first().copied().unwrap_or(0);
        checks.push(
            Check::new(
                "hardware randomness",
                Verdict::Warn,
                format!(
                    "{} site(s), first at +0x{first:x}; a run reaching one is degraded",
                    report.hardware_random.len()
                ),
            )
            .with_remedy("rebuild without hardware randomness, or accept a degraded run"),
        );
    }

    // A warning: the scan finds the string anywhere in the binary, including a linked library the
    // program never uses. `bohr survey` reports the calls the program actually makes.
    checks.push(if report.uses_io_uring {
        Check::new("io_uring", Verdict::Warn, "the program mentions io_uring somewhere in itself")
            .with_remedy("bohr survey -- <program>, to see whether it calls one")
    } else {
        Check::new("io_uring", Verdict::Pass, "not referenced")
    });

    Ok(checks)
}

/// The same, where nothing can inspect a program for this host.
///
/// The file is still read, so a mistyped path is reported.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn target_checks(path: &str) -> Result<Vec<Check>, CliError> {
    std::fs::read(path)
        .map_err(|e| CliError::Usage(format!("BOHR-1002: cannot read {path}: {e}")))?;
    Ok(vec![Check::new(
        "target",
        Verdict::Skip,
        "this build cannot inspect a program it could not run",
    )])
}

/// What to type where namespaces are refused, which depends on who refuses them.
///
/// Ubuntu 24.04 refuses them through AppArmor unless a profile names the program, so the remedy is
/// a profile for this binary. Elsewhere it is the per-user namespace limit.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn namespace_remedy() -> String {
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .is_ok_and(|v| v.trim() == "1");
    if !restricted {
        return "sysctl user.max_user_namespaces   # must be above zero".to_owned();
    }
    let exe = std::env::current_exe()
        .map_or_else(|_| "/usr/local/bin/bohr".to_owned(), |p| p.display().to_string());
    format!(
        "printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile bohr {exe} flags=(unconfined) {{\n  userns,\n}}\n' | sudo tee /etc/apparmor.d/bohr && sudo apparmor_parser -r /etc/apparmor.d/bohr   # AppArmor lets this binary, and no other, make namespaces"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every check is a row with its status in lower case, and the worst row is the verdict.
    #[test]
    fn a_diagnosis_is_a_row_per_check_and_the_worst_of_them() {
        let checks = [
            Check::new("ptrace", Verdict::Pass, "a child can be traced"),
            Check::new("branch counters", Verdict::Warn, "unavailable")
                .with_remedy("sysctl kernel.perf_event_paranoid"),
            Check::new("namespaces", Verdict::Fail, "refused"),
        ];
        let value = crate::json::object("doctor", checks_json(&checks, worst_of(&checks)));
        assert_eq!(value["schema"], "bohr.doctor/1");
        assert_eq!(value["verdict"], "fail");
        assert_eq!(value["checks"][0]["status"], "pass");
        assert_eq!(value["checks"][0]["remedy"], serde_json::Value::Null);
        assert_eq!(value["checks"][1]["remedy"], "sysctl kernel.perf_event_paranoid");
        assert_eq!(value["checks"][2]["status"], "fail");
        assert_eq!(worst_of(&checks[..2]), Verdict::Warn);
    }

    /// This host's checks come out as rows on any host.
    #[test]
    fn this_hosts_checks_are_rows() {
        let checks = host_checks();
        let value = checks_json(&checks, worst_of(&checks));
        assert_eq!(value["checks"].as_array().map(Vec::len), Some(checks.len()));
        assert!(["pass", "warn", "fail"].contains(&value["verdict"].as_str().unwrap_or("")));
    }

    /// The support table is rows of number and name, and a build without one says so.
    #[test]
    fn the_syscall_table_is_rows() {
        let value = crate::json::object("doctor", syscalls_json());
        assert_eq!(value["schema"], "bohr.doctor/1");
        let rows = value["syscalls"].as_array().expect("an array");
        if value["backend"] == true {
            assert!(!rows.is_empty());
            assert!(rows.iter().all(|r| r["nr"].is_u64() && r["name"].is_string()), "{value}");
        } else {
            assert!(rows.is_empty());
        }
    }
}
