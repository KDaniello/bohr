//! What the guest wrote to standard output and error, kept for a reader.
//!
//! The events already digest every byte written; this keeps the text. Each incarnation of a node
//! keeps the last [`OUTPUT_TAIL_BYTES`] it wrote, because a crash or a fatal line is at the end.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The most each incarnation of a node keeps.
pub const OUTPUT_TAIL_BYTES: usize = 64 * 1024;

/// The end of what one incarnation of a node wrote to standard output and error.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OutputTail {
    /// The node that wrote it.
    pub node: u32,
    /// 0 for the node's first start, one more for each restart.
    pub incarnation: u32,
    /// The virtual moment the incarnation started, in ticks.
    pub started_tick: u64,
    /// Bytes written before the kept text began.
    pub dropped: u64,
    /// The kept text; bytes that are not UTF-8 are replaced.
    pub text: String,
}

/// The tails of every incarnation in a run, as they are written.
#[derive(Debug, Default)]
pub struct OutputTails {
    kept: BTreeMap<(u32, u32), Tail>,
    current: BTreeMap<u32, (u32, u64)>,
}

#[derive(Debug, Default)]
struct Tail {
    started_tick: u64,
    bytes: Vec<u8>,
    written: u64,
}

impl OutputTails {
    /// Nothing kept yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new incarnation of `node` at `tick`; its first start needs no call.
    pub fn restarted(&mut self, node: u32, tick: u64) {
        let next = self.current.get(&node).map_or(1, |(incarnation, _)| incarnation + 1);
        self.current.insert(node, (next, tick));
    }

    /// Adds what `node` wrote to the tail of its current incarnation.
    pub fn wrote(&mut self, node: u32, bytes: &[u8]) {
        let (incarnation, started_tick) = self.current.get(&node).copied().unwrap_or((0, 0));
        let tail = self
            .kept
            .entry((node, incarnation))
            .or_insert_with(|| Tail { started_tick, ..Tail::default() });
        tail.written += bytes.len() as u64;
        tail.bytes.extend_from_slice(bytes);
        // Trimmed in batches, so a guest writing one byte at a time is not copied each time.
        if tail.bytes.len() > 2 * OUTPUT_TAIL_BYTES {
            let excess = tail.bytes.len() - OUTPUT_TAIL_BYTES;
            tail.bytes.drain(..excess);
        }
    }

    /// Every incarnation's tail, by node and then incarnation.
    #[must_use]
    pub fn records(&self) -> Vec<OutputTail> {
        self.kept
            .iter()
            .map(|(&(node, incarnation), tail)| {
                let from = tail.bytes.len().saturating_sub(OUTPUT_TAIL_BYTES);
                let kept = &tail.bytes[from..];
                OutputTail {
                    node,
                    incarnation,
                    started_tick: tail.started_tick,
                    dropped: tail.written - kept.len() as u64,
                    text: String::from_utf8_lossy(kept).into_owned(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_incarnation_keeps_its_own_tail() {
        let mut tails = OutputTails::new();
        tails.wrote(0, b"first start\n");
        tails.restarted(0, 500);
        tails.wrote(0, b"after restart\n");
        tails.wrote(1, b"other node\n");
        let records = tails.records();
        assert_eq!(records.len(), 3);
        assert_eq!((records[0].node, records[0].incarnation, records[0].started_tick), (0, 0, 0));
        assert_eq!(records[1].text, "after restart\n");
        assert_eq!((records[1].incarnation, records[1].started_tick), (1, 500));
        assert_eq!(records[2].node, 1);
    }

    #[test]
    fn a_long_output_keeps_its_end_and_counts_the_rest() {
        let mut tails = OutputTails::new();
        let line = [b'x'; 1000];
        for _ in 0..300 {
            tails.wrote(0, &line);
        }
        tails.wrote(0, b"panic: here\n");
        let tail = &tails.records()[0];
        assert_eq!(tail.text.len(), OUTPUT_TAIL_BYTES);
        assert!(tail.text.ends_with("panic: here\n"));
        assert_eq!(tail.dropped, 300_000 + 12 - OUTPUT_TAIL_BYTES as u64);
    }
}
