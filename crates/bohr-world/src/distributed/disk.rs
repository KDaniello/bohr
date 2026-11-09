//! The virtual disk's crash semantics: what survives a kill, and what it survives as.
//!
//! Reads always see the live disk — visibility and durability are different promises, and only the
//! second survives a power cut. Durability is `fsync`'s, and it is **that file's**: syncing one
//! descriptor says nothing about any other file, which is what makes a write-ahead log worth
//! having and what makes a store that syncs the wrong one lose data.
//!
//! **A crash leaves a state the disk was never in.** Between a file's last sync and the crash,
//! each sector it was written on either landed or did not, independently of its neighbours. That
//! is the whole reason crash consistency is a discipline: a model that restored some earlier
//! moment would only ever produce states the store had already survived once. The two axes are
//! atomicity — how much of a write persists — and ordering, from Pillai et al., *All File Systems
//! Are Not Created Equal* (OSDI '14). Semantics are normative in `docs/API.md` §5.1.

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
/// Its own code rather than the general internal one, because what it means is narrower and more
/// useful: whatever the guest did next is not evidence about the guest. Three of the four defects
/// this tool was blamed for on its first real target were of exactly this shape, and each cost a
/// reading of the target's source to disprove.
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
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            stall_probability: (0, 1),
            stall_mean_ticks: 0,
            eio_probability: (0, 1),
            enospc_probability: (0, 1),
            snapshot_depth: 4,
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
/// Drawn once per crash and applied to every dirty sector of every file, so that the number of
/// draws is a fact about the crash rather than about the disk. A draw per sector would make one
/// mutated byte shift every decision after it, which is the property the whole draw ABI protects
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
    /// Weighted rather than uniform because the extremes are what a store is usually written
    /// against, and a model that only ever produced mixtures would rarely exercise either.
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
/// Images are held behind a counted pointer rather than by value. A node's disk is the largest
/// thing this model keeps, the promise and the newest ring entry are the same bytes, and a rewind
/// moves an image rather than producing one.
#[derive(Debug)]
pub(crate) struct NodeDisk {
    /// What a crash starts from: for each file, what that file's last completed `fsync` promised.
    ///
    /// Per file and not per disk. A sync on one descriptor promises nothing about any other file,
    /// and a model that promised otherwise would keep a data file's writes alive because a log was
    /// synced — hiding exactly the defect a write-ahead log exists to prevent.
    durable: Arc<DiskImage>,
    /// Byte ranges written to each file since that file's own last completed sync.
    ///
    /// Ranges rather than content: the bytes are in the live file, and holding a second copy of
    /// every write would cost the run its memory. Coalesced on insert, because a store writing a
    /// page at a time would otherwise accumulate an entry per call.
    pending: BTreeMap<u64, Vec<(u64, u64)>>,
    /// Whole-disk images at each completed sync, newest last, for restart-from-snapshot.
    snapshots: VecDeque<Arc<DiskImage>>,
    /// The stalled `fsync`, if one is in flight.
    /// Syncs that stalled and are waiting for their timers, by the task that made each.
    ///
    /// **One slot held one, and a store syncs from more than one thread.** Two stalls in flight
    /// and the second overwrote the first: the first timer committed the second's file early
    /// and woke the second's task, the second timer fired into nothing, and the first task
    /// stayed parked for ever -- a deadlock this model invented, in exactly the configuration
    /// a stall campaign runs.
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
    /// The window a crash has to land in to lose an acknowledged write, and therefore the moment
    /// guided fault targeting aims at.
    pub(crate) fn is_dirty(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Records bytes that reached a file and that no `fsync` has promised yet.
    ///
    /// Coalesced against the last range recorded for the file, because a store writing a page at a
    /// time in ascending order is the common case and would otherwise leave one entry per call.
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
    /// **A truncation takes the bytes above it, and the marks on them go too.** Left behind, a mark
    /// named a sector the file no longer has: the crash image then tried to copy a sector out of a
    /// file shorter than the mark said, which is a panic in the sans-io core. The write that made
    /// the mark did happen -- what a crash could bring back of it is nothing, because the bytes are
    /// gone whether the truncation was durable or not.
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
    /// By task, because several can be waiting at once. A stall whose node crashed meanwhile has
    /// already been consumed by the crash, which is what tore the file.
    pub(crate) fn take_inflight(&mut self, task: TaskId) -> Option<InFlightFsync> {
        self.inflight.remove(&task)
    }

    /// Everything currently durable, as an image a world can be restored to.
    pub(crate) fn durable_image(&self) -> DiskImage {
        (*self.durable).clone()
    }

    /// Completes a sync of one file: its live content becomes durable, and the disk joins the ring.
    ///
    /// Only that file. Names and numbering are taken whole, because directory operations are an
    /// axis of their own that this model does not separate — a limitation stated in
    /// `docs/API.md` §5.1 rather than left to be discovered.
    pub(crate) fn commit(&mut self, world: &MinimalWorld, inode: Option<u64>, depth: usize) {
        let mut image = self.durable_image();
        let mut live = world.disk_image();
        // The namespace goes over whole, names and the directories they sit in together. Half of it
        // is worse than neither: a directory left behind comes back as a name with no content,
        // which is a file — and a store that made itself a directory finds a file there instead,
        // decides it is looking at something else entirely, and starts again from nothing.
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
    /// Linux reports a failed sync once and drops the dirty pages with it, so a second `fsync` on
    /// the same descriptor reports success having written nothing. Only the named file is affected,
    /// which is what makes the failure worth injecting: a store that retries and carries on has
    /// acknowledged data it no longer has.
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
    /// **The ring is truncated, and that is the whole point.** A rewind that left the newer
    /// images in place would not produce a fork point: the future it stepped past would still be
    /// there, and a second rewind would count back from it. That is not a chain, it is a stutter.
    ///
    /// Clamped rather than refused, unlike the node-level rewind this grew out of. A cluster has
    /// as many independent sync histories as it has nodes, so a depth that is valid on the leader
    /// may be past the ring on a node that synced twice — and aborting the run there would make
    /// the fault unusable at exactly the scenarios it exists for.
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
    /// Everything each file's own last sync promised, plus whatever of the writes since then
    /// happened to land. One drawn value decides the regime and, where the regime is a mixture,
    /// every sector of it: a sector that landed holds what the guest wrote, and one that did not
    /// holds what was there before. A file reached by a surviving sector past its durable length
    /// grows to it, and the gap below reads as zeroes, which is what a sparse file gives.
    ///
    /// A tear — some sectors landing and others not — is narrated, because a file that came back
    /// neither as it was nor as it should be, with nothing on the timeline to say why, is a run
    /// nobody can read; and a campaign cannot claim to have exercised a fault it left no evidence
    /// of. Stated in `docs/API.md` §5.1.
    pub(crate) fn crash_image(
        &mut self,
        node: NodeId,
        live: &MinimalWorld,
        ctx: &mut WorldCtx<'_, '_>,
    ) -> Result<DiskImage, KernelError> {
        // One draw, always, whatever the disk holds. Its width is a fact about the crash rather
        // than about how many sectors happened to be dirty.
        let mut drawn = [0u8; 8];
        ctx.draw_into(&mut drawn, DrawSite::DiskTear);
        let seed = u64::from_le_bytes(drawn);
        // Every stall in flight goes with the crash: it is what tore their files, and none of
        // their timers will find anything to commit.
        self.inflight.clear();

        let mut image = self.durable_image();
        let landing = Landing::of(seed);
        let mut torn = false;

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
                    // **A length is metadata, and it survives whole.** The size that comes back is
                    // the one the file had or the one it grew to, never a figure between them, and
                    // under the ordered mode every mainstream filesystem defaults to the data a
                    // grown size refers to is written before the size that refers to it. So a file
                    // that grew brought its tail with it, and sectors decide for themselves only
                    // *below* the length that was already durable — which is where an overwrite
                    // lands, and where a database does nearly all of its writing.
                    //
                    // Without this the model produces lengths no journalling filesystem can leave,
                    // and a store that trusts its own length is accused of a state it will never
                    // meet.
                    let durable_len = out.len() as u64;
                    if landed_at(seed, *inode, u64::MAX) && current.len() as u64 > durable_len {
                        let tail = current.range_to_vec(durable_len as usize..current.len());
                        out.write_at(durable_len as usize, &tail);
                    }

                    // **Bounded by both files, not by one of them.** A sector is copied out of
                    // the live file and into the durable image, so a range past the end of either
                    // is not a sector there is anything to copy. The live file can be the shorter
                    // of the two: a write marks its sectors pending, and a later `ftruncate` cuts
                    // the file without taking the marks with it. Bounded by the durable length
                    // alone, this indexed a live file that had been cut down -- a panic in the
                    // sans-io core, from a guest that wrote, synced, wrote and truncated, and one
                    // drawn crash. A panic there destroys the run it exists to record, and the
                    // audit that would have named an impossible image runs after this arithmetic.
                    let reach = durable_len.min(current.len() as u64);
                    let (mut landed, mut lost) = (false, false);
                    for (offset, len) in ranges {
                        for sector in
                            (*offset / SECTOR)..=((offset + len).saturating_sub(1) / SECTOR)
                        {
                            let start = sector * SECTOR;
                            let end = ((sector + 1) * SECTOR).min(reach);
                            if end <= start {
                                // Either the sector is past what is durable, or past what the live
                                // file still holds. A sector nobody can read did not land.
                                lost = true;
                                continue;
                            }
                            if landed_at(seed, *inode, sector) {
                                let sector = current.range_to_vec(start as usize..end as usize);
                                out.write_at(start as usize, &sector);
                                landed = true;
                            } else {
                                lost = true;
                            }
                        }
                    }
                    torn |= landed && lost;
                    image.content.insert(*inode, out);
                }
            }
        }

        // Before the pending set is cleared, because what the audit reads is which files this
        // crash was entitled to touch at all.
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
    /// **Stated independently of the code that built it, or it would prove nothing.** An audit
    /// derived from the same arithmetic says only that the arithmetic is repeatable. What this
    /// says is what a disk is allowed to do — a file comes back at a length it held, holding bytes
    /// somebody wrote, and nothing the kernel answers out of itself is on it at all — and what it
    /// therefore rules out is a class of findings that are this world's rather than the guest's.
    ///
    /// Nearly free, because a file no write touched is the same counted pointer the durable image
    /// holds: proving that is one comparison, and only the files this crash could have changed are
    /// read through.
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
            // Every byte is one somebody put there: what the last sync promised, what a write
            // since then put in its place, or nothing at all where the file grew past what was
            // durable and the hole below reads as a sparse file does.
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
            // The name and not only the content, because a namespace goes into the durable image
            // whole at every sync while content goes in one file at a time — so a file the kernel
            // answers reaches a disk image as a name long before it could reach one as bytes, and
            // what a crash then restores is a name with nothing behind it. That is the shape the
            // defect actually had.
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
/// A pure function of the crash's single drawn value, so that a disk with a thousand dirty sectors
/// and one with two consume the same tape. The mixing is `splitmix64`, chosen because it is short,
/// frozen, and has no state to carry between sectors.
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
    /// Built through the world rather than around it, so what the ring holds is what a run would
    /// have put there.
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

    /// The rule that would have cost nothing and saved a day: a length nothing produced.
    ///
    /// Between what the last sync promised and what the file had grown to, and no third value:
    /// a size is metadata and it survives whole. A store that trusts its own length being accused
    /// of a state no journalling filesystem can leave is a finding against this tool, and the
    /// point of stating the rule is that it is caught here rather than in somebody's source.
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

    /// And a byte nothing wrote: what comes back is what somebody put there.
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

    /// And what the kernel answers about a process, which is on no disk at all.
    ///
    /// The defect this tool actually had. A generated `/proc` file was on the modelled disk, a
    /// crash rolled it back, and an execution client panicked reading its own status — a syscall
    /// away from being reported as the client's bug.
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

    /// The property the reorg rests on: a rewind forgets what it stepped past, so the next one
    /// counts from where this one left the history rather than from where it found it.
    ///
    /// Without the truncation the fork point is not a fork point — the future is still in the ring,
    /// and a second rewind lands in the same place as the first. That is not a chain, it is a
    /// stutter.
    #[test]
    fn a_rewind_forgets_the_history_it_stepped_past() {
        let mut disk = with_history(4);
        assert_eq!(length(&disk), 4);

        assert_eq!(disk.rewind(1), 1);
        assert_eq!(length(&disk), 3);

        assert_eq!(disk.rewind(1), 1, "the second step is taken from where the first landed");
        assert_eq!(length(&disk), 2);
    }

    /// Deeper than the history is clamped and says so, rather than ending the run: a cluster has
    /// as many sync histories as it has nodes, and a depth valid on one may be past another's.
    #[test]
    fn a_rewind_past_the_history_goes_as_far_as_there_is() {
        let mut disk = with_history(2);

        assert_eq!(disk.rewind(9), 1, "one step is all there was");
        assert_eq!(length(&disk), 1);
        assert_eq!(disk.rewind(9), 0, "and the oldest image is where it stops");
        assert_eq!(length(&disk), 1);
    }

    /// A sync copies the file it names and shares the rest, which is what keeps its cost a fact
    /// about the write rather than about the disk.
    ///
    /// Checked by identity rather than by a clock. The behaviour is the same either way — what
    /// changes is only how much is copied — so nothing else in the suite would notice a sync that
    /// went back to copying the whole disk, and a test that timed one would be flaky.
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

    /// A node that never synced has nothing to rewind to, and asking for one is not an error.
    #[test]
    fn a_disk_with_no_history_rewinds_nowhere() {
        let world = MinimalWorld::new();
        let mut disk = NodeDisk::new(&world);
        assert_eq!(disk.rewind(3), 0);
    }
}
