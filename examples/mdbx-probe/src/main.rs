//! The smallest program that keeps something in a memory-mapped store and reads it back.
//!
//! For surveys. Execution clients keep their chain in such a store, reached through mappings.
//!
//! One table, a few keys, one commit, one read back.

use std::{error::Error, path::Path, process::ExitCode};

use libmdbx::{DatabaseFlags, Environment, EnvironmentFlags, WriteFlags};

/// The store's directory, holding its data and lock files.
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
    // Exclusive, as one process per node is.
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

    // A fresh transaction reads from the store.
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
