//! The virtual disk's crash semantics: what survives a kill, and what it survives as.
//!
//! Reads see the live disk. Durability comes from `fsync` and covers only the synced file.
//!
//! A crash decides each sector written since the file's last sync independently, so the result
//! can be a state the disk never passed through. The axes are atomicity and ordering, after Pillai
//! et al., *All File Systems Are Not Created Equal* (OSDI '14). Normative: `docs/API.md` §5.1.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use bohr_kernel::{KernelError, WorldCtx};
use bohr_tape::DrawSite;
use bohr_trace::{EventKind, FaultKind, FaultTarget};
use bohr_types::{NodeId, TaskId};

use crate::minimal::{DiskImage, MinimalWorld};

/// The sector size a torn write tears at.
const SECTOR: u64 = 512;

/// What this world says when it has presented the guest with something no machine could.
///
/// Its own code: whatever the guest did next is not evidence about the guest.
pub(crate) const IMPOSSIBLE_STATE: &str = "BOHR-5005";

/// The disk's tunables. Defaults: nothing stalls, nothing fails, four snapshots kept.
#[derive(Clone, Copy, Debug)]
pub struct DiskConfig {
    /// Probability an `fsync` stalls before completing.
    pub stall_probability: (u32, u32),
    /// Mean of the exponential stall duration.
    pub stall_mean_ticks: u64,
    /// Probability an `fsync` fails with `EIO`, dropping everything unsynced.
    pub eio_probability: (u32, u32),
    /// Probability a write fails with `ENOSPC`.
    pub enospc_probability: (u32, u32),
    /// Disk images kept for restart-from-snapshot, newest last.
    pub snapshot_depth: usize,
    /// Whether a crash may keep some unsynced writes and lose others — a later one surviving an
    /// earlier — or only all of them or none.
    pub reorder_unsynced: bool,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            stall_probability: (0, 1),
            stall_mean_ticks: 0,
            eio_probability: (0, 1),
            enospc_probability: (0, 1),
            snapshot_depth: 4,
            reorder_unsynced: true,
        }
    }
}

/// An `fsync` that stalled and has not completed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InFlightFsync {
    /// The task blocked in the call.
    pub(crate) task: TaskId,
    /// The file being synced, by inode.
    pub(crate) inode: Option<u64>,
}

/// What one crash does to everything a file's last sync did not promise.
///
/// Drawn once per crash, so tape use does not depend on the dirty sector count
/// (`docs/ARCHITECTURE.md` §5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Landing {
    /// Nothing unsynced reached the platter. The harshest legal state, and the likeliest.
    Nothing,
    /// Everything did. Also legal: the write was in flight and the power held long enough.
    Everything,
    /// Each sector decided for itself, which is the state the disk was never in.
    PerSector,
}

impl Landing {
    /// Half of all crashes lose everything unsynced, a quarter keep it, a quarter tear.
    ///
    /// Weighted toward the extremes, which stores are written against.
    const fn of(seed: u64) -> Self {
        match seed & 0b11 {
            0 | 1 => Self::Nothing,
            2 => Self::Everything,
            _ => Self::PerSector,
        }
    }
}

/// One node's durability state.
///
/// Images are shared behind counted pointers, since the promise and the newest ring entry are the
/// same bytes.
#[derive(Debug)]
pub(crate) struct NodeDisk {
    /// What a crash starts from: for each file, what that file's last completed `fsync` promised.
    ///
    /// Per file, since a sync covers only its own file.
    durable: Arc<DiskImage>,
    /// Byte ranges written to each file since that file's own last completed sync.
    ///
    /// Ranges, since the bytes are in the live file; coalesced on insert.
    pending: BTreeMap<u64, Vec<(u64, u64)>>,
    /// Whole-disk images at each completed sync, newest last, for restart-from-snapshot.
    snapshots: VecDeque<Arc<DiskImage>>,
    /// Stalled syncs waiting for their timers, by task, since a store syncs from several
    /// threads.
    inflight: BTreeMap<TaskId, InFlightFsync>,
}

impl NodeDisk {
    /// A disk whose every file is durable as it stands — which at creation is nothing at all.
    pub(crate) fn new(world: &MinimalWorld) -> Self {
        Self {
            durable: Arc::new(world.disk_image()),
            pending: BTreeMap::new(),
            snapshots: VecDeque::new(),
            inflight: BTreeMap::new(),
        }
    }

    /// Whether any file holds something no completed `fsync` has promised.
    ///
    /// The window guided fault targeting aims at.
    pub(crate) fn is_dirty(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Records bytes that reached a file and that no `fsync` has promised yet.
    ///
    /// Coalesced with the file's last range, for ascending page writes.
    pub(crate) fn mark_written(&mut self, inode: u64, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let ranges = self.pending.entry(inode).or_default();
        if let Some((at, held)) = ranges.last_mut()
            && offset <= *at + *held
            && offset + len >= *at
        {
            let end = (*at + *held).max(offset + len);
            *at = (*at).min(offset);
            *held = end - *at;
            return;
        }
        ranges.push((offset, len));
    }

    /// Cuts the pending ranges of a file down to a new length.
    ///
    /// Marks above the new length go with the bytes, so a crash never reads past the file.
    pub(crate) fn mark_truncated(&mut self, inode: u64, length: u64) {
        let Some(ranges) = self.pending.get_mut(&inode) else {
            return;
        };
        ranges.retain_mut(|(offset, held)| {
            if *offset >= length {
                return false;
            }
            *held = (*held).min(length - *offset);
            *held > 0
        });
        if ranges.is_empty() {
            self.pending.remove(&inode);
        }
    }

    /// Parks a `fsync` that stalled, to be completed when its timer fires.
    pub(crate) fn stall(&mut self, task: TaskId, inode: Option<u64>) {
        self.inflight.insert(task, InFlightFsync { task, inode });
    }

    /// Takes the stall a timer belongs to, where it is still in flight.
    ///
    /// By task. A crash has already consumed the stalls of its node.
    pub(crate) fn take_inflight(&mut self, task: TaskId) -> Option<InFlightFsync> {
        self.inflight.remove(&task)
    }

    /// Everything currently durable, as an image a world can be restored to.
    pub(crate) fn durable_image(&self) -> DiskImage {
        (*self.durable).clone()
    }

    /// Completes a sync of one file: its live content becomes durable, and the disk joins the ring.
    ///
    /// Only that file's content; the namespace is taken whole (`docs/API.md` §5.1).
    pub(crate) fn commit(&mut self, world: &MinimalWorld, inode: Option<u64>, depth: usize) {
        let mut image = self.durable_image();
        let mut live = world.disk_image();
        // Names and their directories go together, so no directory returns as an empty file.
        image.names = std::mem::take(&mut live.names);
        image.directories = std::mem::take(&mut live.directories);
        image.next_inode = live.next_inode;
        if let Some(inode) = inode {
            match live.content.remove(&inode) {
                Some(content) => {
                    image.content.insert(inode, content);
                }
                None => {
                    image.content.remove(&inode);
                }
            }
            self.pending.remove(&inode);
        }
        let image = Arc::new(image);
        self.snapshots.push_back(Arc::clone(&image));
        self.durable = image;
        while self.snapshots.len() > depth.max(1) {
            self.snapshots.pop_front();
        }
    }

    /// A sync that reported failure: what it was asked to make durable is gone.
    ///
    /// As on Linux, the dirty pages are dropped and a retried `fsync` succeeds with nothing
    /// written. Only the named file is affected.
    pub(crate) fn fail_sync(&mut self, world: &mut MinimalWorld, inode: Option<u64>) {
        let Some(inode) = inode else {
            return;
        };
        self.pending.remove(&inode);
        world.restore_file(inode, self.durable.content.get(&inode).cloned());
    }

    /// Rewinds the durability baseline to an older synced image, and forgets what came after it.
    ///
    /// `back` counts from the newest, so `0` is the latest `fsync` and `1` the one before it.
    /// Answers what it actually rewound to, which may be shallower than what was asked for.
    ///
    /// Newer images are dropped, so the next rewind counts from the fork point.
    ///
    /// Clamped, since each node has its own sync history.
    pub(crate) fn rewind(&mut self, back: usize) -> usize {
        let deepest = self.snapshots.len().saturating_sub(1);
        let applied = back.min(deepest);
        for _ in 0..applied {
            self.snapshots.pop_back();
        }
        if let Some(image) = self.snapshots.back() {
            self.durable = Arc::clone(image);
        }
        self.pending.clear();
        applied
    }

    /// The image a crash leaves behind.
    ///
    /// Each file's last sync plus whichever later writes landed. One draw decides the regime and,
    /// for a mixture, each sector. A surviving sector past the durable length grows the file, with
    /// zeros below.
    ///
    /// A tear is narrated (`docs/API.md` §5.1).
    pub(crate) fn crash_image(
        &mut self,
        node: NodeId,
        live: &MinimalWorld,
        reorder: bool,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<DiskImage, KernelError> {
        // One draw, always.
        let mut drawn = [0u8; 8];
        ctx.draw_into(&mut drawn, DrawSite::DiskTear);
        let seed = u64::from_le_bytes(drawn);
        // Stalls in flight end with the crash.
        self.inflight.clear();

        let mut image = self.durable_image();
        // Without reordering, a mixed crash loses everything; the draw is unchanged.
        let landing = match Landing::of(seed) {
            Landing::PerSector if !reorder => Landing::Nothing,
            other => other,
        };
        let mut torn = false;
        let mut reordered = false;

        for (inode, ranges) in &self.pending {
            let Some(current) = live.file_content(*inode) else {
                continue;
            };
            match landing {
                Landing::Nothing => {}
                Landing::Everything => {
                    image.content.insert(*inode, current.clone());
                }
                Landing::PerSector => {
                    let mut out = image.content.get(inode).cloned().unwrap_or_default();
                    // The length survives whole: the old one or the grown one. As in ordered mode,
                    // a grown file keeps its tail, and sectors decide independently only below the
                    // durable length.
                    let durable_len = out.len() as u64;
                    let tail_landed = landed_at(seed, *inode, u64::MAX);
                    if tail_landed && current.len() as u64 > durable_len {
                        let tail = current.range_to_vec(durable_len as usize..current.len());
                        out.write_at(durable_len as usize, &tail);
                    }

                    // Bounded by both the live and the durable length, since either can be
                    // shorter.
                    let reach = durable_len.min(current.len() as u64);
                    // Whether an earlier write lost a sector; this one landing is reordering.
                    let mut earlier_lost = false;
                    // Below the durable length by sector, above it by the tail, past the live file
                    // not at all.
                    let live_len = current.len() as u64;
                    for (offset, len) in ranges {
                        let (mut range_landed, mut range_lost) = (false, false);
                        let write_end = offset + len;
                        for sector in
                            (*offset / SECTOR)..=((offset + len).saturating_sub(1) / SECTOR)
                        {
                            let start = sector * SECTOR;
                            let end = ((sector + 1) * SECTOR).min(reach);
                            // The part of this write inside this sector.
                            let (lo, hi) =
                                (start.max(*offset), ((sector + 1) * SECTOR).min(write_end));
                            if end > start {
                                let kept = landed_at(seed, *inode, sector);
                                if kept {
                                    let sector = current.range_to_vec(start as usize..end as usize);
                                    out.write_at(start as usize, &sector);
                                }
                                if lo < reach {
                                    if kept {
                                        range_landed = true;
                                    } else {
                                        range_lost = true;
                                    }
                                }
                            }
                            if hi > durable_len.max(lo) {
                                if tail_landed && lo.max(durable_len) < live_len {
                                    range_landed = true;
                                } else {
                                    range_lost = true;
                                }
                            }
                        }
                        reordered |= range_landed && earlier_lost;
                        earlier_lost |= range_lost;
                        torn |= range_landed && range_lost;
                    }
                    image.content.insert(*inode, out);
                }
            }
        }

        // Before clearing pending, which the audit reads.
        if let Some(reason) = self.impossible(&image, live) {
            return Err(KernelError::World {
                code: IMPOSSIBLE_STATE,
                message: format!(
                    "the crash on node {} left a disk no machine could: {reason}. Whatever the \
                     guest does next is this world's doing, so the run is not evidence about the \
                     guest and is refused rather than reported",
                    node.0
                ),
            });
        }

        self.pending.clear();
        if reordered {
            ctx.emit_for(
                node,
                TaskId::ZERO,
                EventKind::FaultFired {
                    fault: FaultKind::DiskReorder,
                    target: FaultTarget::Node(node),
                },
            )?;
        }
        // One fault per crash and kind: tear, reorder, or both.
        if torn {
            ctx.emit_for(
                node,
                TaskId::ZERO,
                EventKind::FaultFired {
                    fault: FaultKind::DiskTear,
                    target: FaultTarget::Node(node),
                },
            )?;
        }
        Ok(image)
    }

    /// Why the image a crash produced is not one a real machine could have left, where it is not.
    ///
    /// Written independently of the crash code: a file returns at a length it held, with bytes
    /// someone wrote, and no kernel-generated file is on the disk.
    ///
    /// Untouched files are the same pointer, so only changed files are read.
    fn impossible(&self, image: &DiskImage, live: &MinimalWorld) -> Option<String> {
        for (inode, content) in &image.content {
            if live.is_synthetic(*inode) {
                return Some(format!(
                    "inode {inode} is answered by the kernel and is not a file on any disk"
                ));
            }
            let durable = self.durable.content.get(inode);
            if durable.is_some_and(|held| held.same(content)) {
                continue;
            }
            let live_content = live.file_content(*inode);
            let (durable_len, live_len) =
                (durable.map_or(0, |b| b.len()), live_content.map_or(0, |b| b.len()));
            if content.len() != durable_len && content.len() != live_len {
                return Some(format!(
                    "inode {inode} came back {} bytes long, having been {durable_len} durable and \
                     {live_len} live",
                    content.len()
                ));
            }
            // Every byte is from the last sync, a later write, or a sparse hole.
            for (at, byte) in
                content.segments(0..content.len()).flat_map(|(from, part)| (from..).zip(part))
            {
                let held = durable.and_then(|b| b.get(at));
                let written = live_content.and_then(|b| b.get(at));
                if held != Some(*byte)
                    && written != Some(*byte)
                    && !(at >= durable_len && *byte == 0)
                {
                    return Some(format!(
                        "inode {inode} came back holding a byte at {at} that nothing wrote"
                    ));
                }
            }
        }
        for (name, inode) in &image.names {
            // The name too, since the namespace is synced whole and content per file.
            if live.is_synthetic(*inode) {
                return Some(format!(
                    "{name} is answered by the kernel and is not a name on any disk"
                ));
            }
            if image.directories.contains(inode) && image.content.contains_key(inode) {
                return Some(format!("{name} came back a directory and a file at once"));
            }
        }
        None
    }
}

/// Whether one sector of one file reached the platter before the power went.
///
/// A pure function of the crash's one draw, mixed with `splitmix64`, which is short and
/// stateless.
fn landed_at(seed: u64, inode: u64, sector: u64) -> bool {
    let mut z = seed
        .wrapping_add(inode.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(sector.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)) & 1 == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes::FileBytes;

    /// A disk holding `count` synced images, the newest last, each one byte longer than the last.
    ///
    /// Built through the world, as a run would.
    fn with_history(count: usize) -> NodeDisk {
        let world = MinimalWorld::new();
        let mut disk = NodeDisk::new(&world);
        for length in 1..=count {
            let mut grown = MinimalWorld::new();
            grown.restore_disk(crate::minimal::DiskImage {
                content: [(2, FileBytes::from_vec(vec![b'x'; length]))].into_iter().collect(),
                names: [("/log".to_owned(), 2)].into_iter().collect(),
                directories: std::collections::BTreeSet::new(),
                next_inode: 3,
            });
            disk.commit(&grown, Some(2), count.max(1));
        }
        disk
    }

    fn length(disk: &NodeDisk) -> usize {
        disk.durable.content.values().next().map_or(0, |held| held.len())
    }

    /// A world holding one file of `bytes`, under the name `/log`.
    fn holding(bytes: &[u8]) -> MinimalWorld {
        let mut world = MinimalWorld::new();
        world.restore_disk(crate::minimal::DiskImage {
            content: [(2, FileBytes::from_vec(bytes.to_vec()))].into_iter().collect(),
            names: [("/log".to_owned(), 2)].into_iter().collect(),
            directories: std::collections::BTreeSet::new(),
            next_inode: 3,
        });
        world
    }

    /// The image a crash would have produced, edited by hand into one it must not.
    fn edited(disk: &NodeDisk, inode: u64, content: Vec<u8>) -> DiskImage {
        let mut image = disk.durable_image();
        image.content.insert(inode, FileBytes::from_vec(content));
        image
    }

    /// A length that is neither the synced nor the grown one is caught.
    #[test]
    fn a_length_between_the_durable_one_and_the_live_one_is_refused() {
        let mut disk = NodeDisk::new(&MinimalWorld::new());
        let durable = holding(&[b'a'; 64]);
        disk.commit(&durable, Some(2), 4);
        let live = holding(&[b'a'; 256]);

        assert!(disk.impossible(&edited(&disk, 2, vec![b'a'; 64]), &live).is_none());
        assert!(disk.impossible(&edited(&disk, 2, vec![b'a'; 256]), &live).is_none());
        assert!(
            disk.impossible(&edited(&disk, 2, vec![b'a'; 128]), &live).is_some(),
            "a length that is neither what was durable nor what was live was accepted"
        );
    }

    /// A byte nothing wrote is caught.
    #[test]
    fn a_byte_nothing_wrote_is_refused() {
        let mut disk = NodeDisk::new(&MinimalWorld::new());
        disk.commit(&holding(&[b'a'; 64]), Some(2), 4);
        let live = holding(&[b'b'; 64]);

        assert!(disk.impossible(&edited(&disk, 2, vec![b'b'; 64]), &live).is_none());
        let mut invented = vec![b'a'; 64];
        invented[7] = b'z';
        assert!(
            disk.impossible(&edited(&disk, 2, invented), &live).is_some(),
            "a byte neither sync nor write produced was accepted"
        );
    }

    /// A `/proc` file on the disk image is caught.
    #[test]
    fn a_file_the_kernel_answers_out_of_itself_is_refused() {
        let mut disk = NodeDisk::new(&MinimalWorld::new());
        let mut live = holding(&[b'a'; 16]);
        let synthetic = live.synthesise("/proc/self/status", b"State: R".to_vec());
        disk.commit(&live, Some(2), 4);

        let image = edited(&disk, synthetic, b"State: R".to_vec());
        assert!(
            disk.impossible(&image, &live).is_some(),
            "a file the kernel answers out of itself was accepted onto a disk image"
        );
    }

    /// A rewind drops what it stepped past, so the next one counts from the fork point.
    #[test]
    fn a_rewind_forgets_the_history_it_stepped_past() {
        let mut disk = with_history(4);
        assert_eq!(length(&disk), 4);

        assert_eq!(disk.rewind(1), 1);
        assert_eq!(length(&disk), 3);

        assert_eq!(disk.rewind(1), 1, "the second step is taken from where the first landed");
        assert_eq!(length(&disk), 2);
    }

    /// A depth past the history is clamped and reported.
    #[test]
    fn a_rewind_past_the_history_goes_as_far_as_there_is() {
        let mut disk = with_history(2);

        assert_eq!(disk.rewind(9), 1, "one step is all there was");
        assert_eq!(length(&disk), 1);
        assert_eq!(disk.rewind(9), 0, "and the oldest image is where it stops");
        assert_eq!(length(&disk), 1);
    }

    /// A sync copies the file it names and shares the rest, so its cost follows the write.
    ///
    /// Checked by pointer identity.
    #[test]
    fn a_sync_shares_every_file_it_was_not_about() {
        let mut world = MinimalWorld::new();
        world.restore_disk(crate::minimal::DiskImage {
            content: [
                (2, FileBytes::from_vec(vec![b'a'; 64])),
                (3, FileBytes::from_vec(vec![b'b'; 64])),
            ]
            .into_iter()
            .collect(),
            names: [("/data".to_owned(), 2), ("/log".to_owned(), 3)].into_iter().collect(),
            directories: std::collections::BTreeSet::new(),
            next_inode: 4,
        });

        let mut disk = NodeDisk::new(&world);
        disk.commit(&world, Some(2), 4);

        let held = disk.durable.content.get(&3).expect("the untouched file is durable");
        let live = world.file_content(3).expect("and the world still holds it");
        assert!(held.same(live), "syncing one file copied another");
    }

    /// A node that never synced rewinds to nothing, without error.
    #[test]
    fn a_disk_with_no_history_rewinds_nowhere() {
        let world = MinimalWorld::new();
        let mut disk = NodeDisk::new(&world);
        assert_eq!(disk.rewind(3), 0);
    }
}
