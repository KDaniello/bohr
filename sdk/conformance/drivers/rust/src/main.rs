//! Makes the calls a conformance case asks for, in Rust.
//!
//! A driver knows how to call its own SDK and nothing else: the case says what to call, the
//! checker says whether the records were right, and this file is the only language-specific part.
//!
//! The case is parsed with the SDK's own JSON, which is the only parser available to something
//! that carries no dependencies — and parsing the case with the code under test is fine, because
//! what is under test is the records, and a case that failed to parse fails loudly.

use bohr_sdk::Value;
use bohr_sdk_proto::json;

fn main() -> std::process::ExitCode {
    let Some(spec) = std::env::args().nth(1) else {
        eprintln!("usage: conformance-driver <case json>");
        return std::process::ExitCode::from(2);
    };
    let parsed = match json::parse(&spec) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("the case does not parse: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let Some(Value::Array(calls)) = parsed.get("calls").cloned() else {
        eprintln!("the case names no calls");
        return std::process::ExitCode::from(2);
    };

    for call in calls {
        let name = call.get("fn").and_then(Value::as_str).unwrap_or_default().to_owned();
        let args = match call.get("args") {
            Some(Value::Array(args)) => args.clone(),
            _ => Vec::new(),
        };
        let flag = args.first().and_then(Value::as_bool).unwrap_or(false);
        let text = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or_default().to_owned();

        match name.as_str() {
            "always" => bohr_sdk::always!(flag, &text(1)),
            "always_or_unreachable" => bohr_sdk::always_or_unreachable!(flag, &text(1)),
            "sometimes" => bohr_sdk::sometimes!(flag, &text(1)),
            "reachable" => bohr_sdk::reachable!(&text(0)),
            "unreachable" => bohr_sdk::unreachable_point!(&text(0)),
            "fault_point" => {
                let _ = bohr_sdk::fault_point(&text(0));
            }
            "setup_complete" => {
                bohr_sdk::setup_complete(args.first().cloned().unwrap_or(Value::Null));
            }
            "send_event" => {
                bohr_sdk::send_event(&text(0), args.get(1).cloned().unwrap_or(Value::Null));
            }
            "random_bytes" => {
                let n = args.first().and_then(Value::as_int).unwrap_or(0) as usize;
                let mut buf = vec![0u8; n];
                bohr_sdk::random_bytes(&mut buf);
            }
            other => {
                eprintln!("this driver has no call named {other}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    std::process::ExitCode::SUCCESS
}
