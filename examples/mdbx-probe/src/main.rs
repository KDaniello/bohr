//! The smallest program that keeps something in a memory-mapped store and reads it back.
//!
//! It exists to be surveyed rather than to be hunted. An execution client keeps its chain in a
//! store of this kind, and a store of this kind is reached through a mapping rather than through
//! reads — so the set of calls it needs is the set that stands between this engine and every such
//! client, whatever else that client does.
//!
//! One table, a handful of keys, one commit and one read back. Small on purpose: what a survey
//! answers is which calls a build cannot model, and a larger program would only ask for the same
//! ones more times.

use std::{error::Error, path::Path, process::ExitCode};

use libmdbx::{DatabaseFlags, Environment, EnvironmentFlags, WriteFlags};

/// Where the store is kept. A directory, because that is what this store expects: a data file and
/// a lock file beside it.
const STORE: &str = "/store";

/// How many keys are written and read back.
const KEYS: u64 = 16;

fn main() -> ExitCode {
    match exercise() {
        Ok(found) => {
            println!("read back {found} of {KEYS}");
            if found == KEYS {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("mdbx: {e}");
            ExitCode::from(2)
        }
    }
}

fn exercise() -> Result<u64, Box<dyn Error>> {
    // Exclusive, which is what one process per node actually is. It is not a way around anything:
    // a store told it is the only one holding its files stops keeping a lock file for the readers
    // that do not exist, and that is the truth of a run here rather than a convenience.
    let environment = Environment::builder()
        .set_max_dbs(2)
        .set_flags(EnvironmentFlags { exclusive: true, ..EnvironmentFlags::default() })
        .open(Path::new(STORE))?;

    let writing = environment.begin_rw_sync()?;
    let table = writing.create_db(Some("chain"), DatabaseFlags::empty())?;
    for key in 0..KEYS {
        writing.put(table, key.to_be_bytes(), key.to_le_bytes(), WriteFlags::empty())?;
    }
    writing.commit()?;

    // A fresh transaction, so what is read comes off the store rather than out of the one that
    // wrote it: a write not yet visible to a new reader is exactly the defect this shape looks for.
    let reading = environment.begin_ro_sync()?;
    let table = reading.open_db(Some("chain"))?;
    let mut found = 0;
    for key in 0..KEYS {
        if let Some(held) = reading.get::<Vec<u8>>(table.dbi(), &key.to_be_bytes())? {
            if held == key.to_le_bytes() {
                found += 1;
            }
        }
    }
    Ok(found)
}
