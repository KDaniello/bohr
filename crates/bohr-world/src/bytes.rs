//! A file's bytes, shared block by block with every disk image taken of them.
//!
//! A write copies only the blocks it touches. Every `fsync` takes an image that shares blocks with
//! the live file, so a store syncing each transaction copies one block.
//!
//! Every block but the last holds [`BLOCK`] bytes. A file grown with zeros shares one zero block
//! until written, so reserving space with `ftruncate` is free.

use std::sync::{Arc, LazyLock};

/// The block of zeros every grown file shares until it writes into one.
static ZEROS: LazyLock<Arc<Vec<u8>>> = LazyLock::new(|| Arc::new(vec![0; BLOCK]));

/// The span a collected mapping is compared and written in: the page the guest dirties.
const PAGE: usize = 4096;

/// Bytes per block: few pointers for a big file, little copying per write.
pub(crate) const BLOCK: usize = 64 * 1024;

/// A file's content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileBytes {
    blocks: Vec<Arc<Vec<u8>>>,
    len: usize,
}

impl FileBytes {
    /// The content of a buffer.
    pub(crate) fn from_vec(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        let blocks = bytes.chunks(BLOCK).map(|chunk| Arc::new(chunk.to_vec())).collect();
        Self { blocks, len }
    }

    /// How many bytes it holds.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// The byte at `at`, where there is one.
    pub(crate) fn get(&self, at: usize) -> Option<u8> {
        if at >= self.len {
            return None;
        }
        self.blocks.get(at / BLOCK).and_then(|block| block.get(at % BLOCK)).copied()
    }

    /// Whether two contents share the same blocks, as after a sync that changed nothing.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.len == other.len
            && self.blocks.len() == other.blocks.len()
            && self.blocks.iter().zip(&other.blocks).all(|(a, b)| Arc::ptr_eq(a, b))
    }

    /// Grows with zeros or cuts to `len` bytes.
    pub(crate) fn resize(&mut self, len: usize) {
        if len < self.len {
            let keep = len.div_ceil(BLOCK);
            self.blocks.truncate(keep);
            if let Some(last) = self.blocks.last_mut() {
                let tail = len - (keep - 1) * BLOCK;
                if last.len() != tail {
                    Arc::make_mut(last).truncate(tail);
                }
            }
        } else {
            let mut missing = len - self.len;
            if let Some(last) = self.blocks.last_mut()
                && last.len() < BLOCK
                && missing > 0
            {
                let grow = (BLOCK - last.len()).min(missing);
                let block = Arc::make_mut(last);
                block.resize(block.len() + grow, 0);
                missing -= grow;
            }
            while missing > 0 {
                let take = missing.min(BLOCK);
                self.blocks.push(if take == BLOCK {
                    Arc::clone(&ZEROS)
                } else {
                    Arc::new(vec![0; take])
                });
                missing -= take;
            }
        }
        self.len = len;
    }

    /// Writes `bytes` at `at`, growing the content with zeros to reach it.
    pub(crate) fn write_at(&mut self, at: usize, bytes: &[u8]) {
        let end = at + bytes.len();
        if end > self.len {
            self.resize(end);
        }
        let mut done = 0;
        while done < bytes.len() {
            let pos = at + done;
            let (index, offset) = (pos / BLOCK, pos % BLOCK);
            let take = (BLOCK - offset).min(bytes.len() - done);
            let block = Arc::make_mut(&mut self.blocks[index]);
            block[offset..offset + take].copy_from_slice(&bytes[done..done + take]);
            done += take;
        }
    }

    /// Writes `bytes` at `at`, page by page, where they differ from what is held.
    ///
    /// For writing a mapping back, so unchanged pages stay shared with disk images.
    pub(crate) fn write_changed(&mut self, at: usize, bytes: &[u8]) {
        let mut done = 0;
        while done < bytes.len() {
            let take = (PAGE - (at + done) % PAGE).min(bytes.len() - done);
            let page = &bytes[done..done + take];
            let start = at + done;
            let same = start + take <= self.len
                && self
                    .segments(start..start + take)
                    .all(|(from, held)| held == &page[from - start..from - start + held.len()]);
            if !same {
                self.write_at(start, page);
            }
            done += take;
        }
    }

    /// The stretches of a range as the blocks hold them, each with the offset it starts at.
    ///
    /// For a caller that copies a range out without joining it first.
    pub(crate) fn segments(
        &self,
        range: std::ops::Range<usize>,
    ) -> impl Iterator<Item = (usize, &[u8])> {
        let end = range.end.min(self.len);
        let mut pos = range.start.min(end);
        std::iter::from_fn(move || {
            if pos >= end {
                return None;
            }
            let (index, offset) = (pos / BLOCK, pos % BLOCK);
            let block = &self.blocks[index];
            let take = (block.len() - offset).min(end - pos);
            let at = pos;
            pos += take;
            Some((at, &block[offset..offset + take]))
        })
    }

    /// A range as one buffer.
    pub(crate) fn range_to_vec(&self, range: std::ops::Range<usize>) -> Vec<u8> {
        let mut out = Vec::with_capacity(range.len());
        for (_, part) in self.segments(range) {
            out.extend_from_slice(part);
        }
        out
    }

    /// The whole content as one buffer.
    pub(crate) fn to_vec(&self) -> Vec<u8> {
        self.range_to_vec(0..self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write copies only the blocks it touches; the rest stay shared.
    #[test]
    fn a_write_copies_only_the_blocks_it_touches() {
        let image = FileBytes::from_vec(vec![7; BLOCK * 4]);
        let mut live = image.clone();
        live.write_at(BLOCK + 10, b"xyz");
        let shared =
            live.blocks.iter().zip(&image.blocks).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
        assert_eq!(shared, 3, "one block of four was written");
        assert_eq!(image.get(BLOCK + 10), Some(7), "the image kept its bytes");
        assert_eq!(live.get(BLOCK + 10), Some(b'x'));
        assert!(!live.same(&image));
        assert!(image.clone().same(&image));
    }

    /// A file grown with zeros shares one block of them, and a write gives it its own.
    #[test]
    fn grown_zeros_are_shared_until_written() {
        let mut a = FileBytes::default();
        a.resize(BLOCK * 3);
        let mut b = FileBytes::default();
        b.resize(BLOCK * 2);
        assert!(Arc::ptr_eq(&a.blocks[0], &b.blocks[1]), "one block of zeros for both");
        a.write_at(5, b"x");
        assert!(!Arc::ptr_eq(&a.blocks[0], &ZEROS));
        assert_eq!(ZEROS.iter().filter(|z| **z != 0).count(), 0, "the shared zeros stay zeros");
    }

    /// Writing back a window copies only the blocks whose pages changed.
    #[test]
    fn a_window_written_back_copies_only_what_changed() {
        let image = FileBytes::from_vec(vec![1; BLOCK * 4]);
        let mut live = image.clone();
        let mut window = vec![1; BLOCK * 4];
        window[BLOCK * 2 + 100] = 9;
        live.write_changed(0, &window);
        let shared =
            live.blocks.iter().zip(&image.blocks).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
        assert_eq!(shared, 3, "one page changed, one block copied");
        assert_eq!(live.get(BLOCK * 2 + 100), Some(9));
    }

    #[test]
    fn it_reads_and_resizes_like_a_buffer() {
        let mut bytes = FileBytes::default();
        bytes.write_at(5, b"hello");
        assert_eq!(bytes.to_vec(), b"\0\0\0\0\0hello");
        bytes.resize(BLOCK * 2 + 3);
        assert_eq!(bytes.len(), BLOCK * 2 + 3);
        assert_eq!(bytes.get(BLOCK * 2 + 2), Some(0));
        bytes.write_at(BLOCK - 1, b"ab");
        assert_eq!(bytes.range_to_vec(BLOCK - 1..BLOCK + 1), b"ab");
        let pieces: Vec<usize> =
            bytes.segments(BLOCK - 1..BLOCK + 1).map(|(at, p)| at + p.len()).collect();
        assert_eq!(pieces, [BLOCK, BLOCK + 1], "split at the block boundary");
        bytes.resize(7);
        assert_eq!(bytes.to_vec(), b"\0\0\0\0\0he");
        bytes.resize(9);
        assert_eq!(bytes.to_vec(), b"\0\0\0\0\0he\0\0", "growth past a cut reads as zeros");
        assert_eq!(FileBytes::default().len(), 0);
    }
}
