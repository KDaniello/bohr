//! The submission ring a guest hands work to, and the shape of everything in it.
//!
//! **A ring is not unobservable, only its poller is.** Work leaves the guest through shared memory
//! and the supervisor sees none of it while a kernel thread drains the queue, and only then: a
//! guest whose queue nobody drains must call `io_uring_enter`, and at that call every submission
//! sits in memory this world can read. A polled queue is opened with its poller asleep
//! ([`SETUP_SQPOLL`]), which turns it into the same case.
//!
//! **Each submission is the plain call it stands for.** [`plain_call`] translates an entry into the
//! syscall that does the same work, and the world carries it out by the route that call takes when
//! the guest makes it directly — the same file table, the same disk marks, the same drawn faults.
//! A write through a ring that skipped the disk model would survive every crash.
//!
//! **The layout is ours.** A kernel tells the guest where each part of the queue sits, and every
//! caller reads the offsets rather than assuming them; what matters is that what is reported is
//! what is built.
//!
//! What this does not do: registered buffers and files, buffer selection, timeouts, cancellation,
//! any operation outside [`plain_call`], and anything asynchronous. Every submission is carried out
//! where it is read and its completion posted before the call returns — a ring that is always empty
//! by the time anyone looks, which is legal and the only one a replay can reconstruct.

/// Where the guest asks for each region, and what the kernel calls those numbers.
pub(crate) mod offset {
    /// The submission queue's head, tail and index array.
    pub(crate) const SQ_RING: u64 = 0;
    /// The completion queue's head, tail and entries.
    pub(crate) const CQ_RING: u64 = 0x0800_0000;
    /// The submission entries themselves.
    pub(crate) const SQES: u64 = 0x1000_0000;
}

/// Bytes in one submission entry, fixed by the interface.
pub(crate) const SQE_BYTES: u64 = 64;

/// Bytes in one completion entry: what the submission carried, the result, and flags.
pub(crate) const CQE_BYTES: u64 = 16;

/// What this ring promises, and only what is true here: a bit advertised and not kept is worse
/// than one withheld. `NODROP` (no entry is taken without a completion slot free), `SUBMIT_STABLE`
/// (entries are read where they are carried out) and `SQPOLL_NONFIXED` (nothing can be registered).
/// `SINGLE_MMAP` is withheld: the two queues are two regions.
const FEATURES: u32 = (1 << 1) | (1 << 2) | (1 << 7);

/// `IORING_SETUP_SQPOLL`: the queue is drained by a thread of the kernel's own.
///
/// The poller here is always asleep, so [`SQ_NEED_WAKEUP`] stays set and every caller wakes it
/// through `io_uring_enter` — a state a real kernel produces constantly, and the one this answers.
pub(crate) const SETUP_SQPOLL: u32 = 1 << 1;

/// Setup flags this ring honours: `SQPOLL`, `SQ_AFF`, `CQSIZE`, `CLAMP`, `ATTACH_WQ`, `SUBMIT_ALL`,
/// `COOP_TASKRUN`, `TASKRUN_FLAG`, `SINGLE_ISSUER` and `DEFER_TASKRUN` — each either changes
/// nothing a ring that completes everything at once does, or is carried out below.
const SETUP_HONOURED: u32 = (1 << 1)
    | (1 << 2)
    | SETUP_CQSIZE
    | SETUP_CLAMP
    | (1 << 5)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 12)
    | (1 << 13);
/// Setup flags a kernel knows and this ring does not carry, refused by name rather than answered.
const SETUP_REFUSED: [(u32, &str); 7] = [
    (1 << 0, "IORING_SETUP_IOPOLL"),
    (1 << 6, "IORING_SETUP_R_DISABLED"),
    (1 << 10, "IORING_SETUP_SQE128"),
    (1 << 11, "IORING_SETUP_CQE32"),
    (1 << 14, "IORING_SETUP_NO_MMAP"),
    (1 << 15, "IORING_SETUP_REGISTERED_FD_ONLY"),
    (1 << 16, "IORING_SETUP_NO_SQARRAY"),
];
/// `IORING_SETUP_CQSIZE`: the completion queue's size is the caller's, not twice the submissions.
const SETUP_CQSIZE: u32 = 1 << 3;
/// `IORING_SETUP_CLAMP`: sizes past the limit are clamped rather than refused.
const SETUP_CLAMP: u32 = 1 << 4;
/// The most submission slots a kernel gives, and the most completion slots.
const MAX_ENTRIES: u32 = 32_768;
const MAX_CQ_ENTRIES: u32 = 2 * MAX_ENTRIES;

/// Why a ring was not opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Setup {
    /// What a kernel answers `EINVAL`: a size of nought, past the limit, or a flag nobody defined.
    Invalid,
    /// A flag a kernel honours and this ring does not, by name.
    Unsupported(&'static str),
}

/// `IORING_SQ_NEED_WAKEUP`, in the submission queue's own flag word.
///
/// Kept set for the life of a polled ring. Clearing it would mean claiming a thread is draining the
/// queue, and none is.
const SQ_NEED_WAKEUP: u32 = 1 << 0;

/// `IOSQE_FIXED_FILE`: the descriptor is an index into registered files, which this ring refuses.
const SQE_FIXED_FILE: u8 = 1 << 0;
/// `IOSQE_IO_LINK`: the next entry runs only if this one succeeds.
const SQE_IO_LINK: u8 = 1 << 2;
/// `IOSQE_IO_HARDLINK`: the next entry runs after this one whatever it returned.
const SQE_IO_HARDLINK: u8 = 1 << 3;
/// `IOSQE_BUFFER_SELECT`: the buffer is picked from a registered group, which this ring refuses.
const SQE_BUFFER_SELECT: u8 = 1 << 5;
/// `IOSQE_CQE_SKIP_SUCCESS`: post a completion only if the operation failed.
const SQE_CQE_SKIP_SUCCESS: u8 = 1 << 6;

/// `ECANCELED`, what a linked entry completes with when an earlier link failed.
const ECANCELED: i64 = 125;

/// Where each field of the submission queue's control block sits, from the start of its region.
mod sq {
    pub(crate) const HEAD: u64 = 0;
    pub(crate) const TAIL: u64 = 4;
    pub(crate) const RING_MASK: u64 = 8;
    pub(crate) const RING_ENTRIES: u64 = 12;
    pub(crate) const FLAGS: u64 = 16;
    pub(crate) const DROPPED: u64 = 20;
    /// The index array, past the control block and aligned.
    pub(crate) const ARRAY: u64 = 32;
}

/// The same for the completion queue.
mod cq {
    pub(crate) const HEAD: u64 = 0;
    pub(crate) const TAIL: u64 = 4;
    pub(crate) const RING_MASK: u64 = 8;
    pub(crate) const RING_ENTRIES: u64 = 12;
    pub(crate) const OVERFLOW: u64 = 16;
    pub(crate) const FLAGS: u64 = 20;
    /// The entries, past the control block and aligned.
    pub(crate) const ENTRIES: u64 = 32;
}

/// One ring: how big it is, and where the guest has mapped each of its parts.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Ring {
    /// Slots in the submission queue; a power of two, so a mask can stand in for a modulo.
    pub(crate) entries: u32,
    /// Slots in the completion queue, also a power of two.
    pub(crate) cq_entries: u32,
    /// The setup flags, handed back in the parameter block as a kernel hands them back.
    pub(crate) flags: u32,
    /// Whether the guest asked for a queue a thread of the kernel's own would drain.
    pub(crate) polled: bool,
    /// Where the submission queue's control block was mapped, once it has been.
    pub(crate) sq_at: Option<u64>,
    /// Where the completion queue's was.
    pub(crate) cq_at: Option<u64>,
    /// Where the submission entries were.
    pub(crate) sqes_at: Option<u64>,
}

impl Ring {
    /// A ring of `asked` slots, rounded up the way the interface says.
    ///
    /// A power of two because the control block reports a mask rather than a count, and a caller
    /// that took the mask at its word on a queue that was not one would read the wrong slot.
    pub(crate) fn new(asked: u32, polled: bool) -> Self {
        let entries = asked.clamp(1, MAX_ENTRIES).next_power_of_two();
        Self {
            entries,
            cq_entries: entries * 2,
            flags: if polled { SETUP_SQPOLL } else { 0 },
            polled,
            sq_at: None,
            cq_at: None,
            sqes_at: None,
        }
    }

    /// The ring `io_uring_setup` asked for, or why a kernel would not open it.
    pub(crate) fn from_setup(asked: u32, flags: u32, asked_cq: u32) -> Result<Self, Setup> {
        if let Some((_, name)) = SETUP_REFUSED.iter().find(|(bit, _)| flags & bit != 0) {
            return Err(Setup::Unsupported(name));
        }
        let clamp = flags & SETUP_CLAMP != 0;
        let fit = |n: u32, max: u32| match n {
            0 => Err(Setup::Invalid),
            n if n > max && !clamp => Err(Setup::Invalid),
            n => Ok(n.min(max).next_power_of_two()),
        };
        if flags & !SETUP_HONOURED != 0 {
            return Err(Setup::Invalid);
        }
        let mut ring = Self::new(fit(asked, MAX_ENTRIES)?, flags & SETUP_SQPOLL != 0);
        if flags & SETUP_CQSIZE != 0 {
            ring.cq_entries = fit(asked_cq, MAX_CQ_ENTRIES)?;
            if ring.cq_entries < ring.entries {
                return Err(Setup::Invalid);
            }
        }
        ring.flags = flags;
        Ok(ring)
    }

    /// Completion slots: twice the submissions unless `CQSIZE` named a number.
    pub(crate) const fn completions(self) -> u32 {
        self.cq_entries
    }

    /// Bytes in each region, in the order the guest asks for them.
    pub(crate) const fn region_bytes(self, at: u64) -> Option<u64> {
        match at {
            offset::SQ_RING => Some(sq::ARRAY + 4 * self.entries as u64),
            offset::CQ_RING => Some(cq::ENTRIES + CQE_BYTES * self.completions() as u64),
            offset::SQES => Some(SQE_BYTES * self.entries as u64),
            _ => None,
        }
    }

    /// Remembers where a region landed, and says whether the offset named one at all.
    pub(crate) fn placed(&mut self, at: u64, address: u64) -> bool {
        match at {
            offset::SQ_RING => self.sq_at = Some(address),
            offset::CQ_RING => self.cq_at = Some(address),
            offset::SQES => self.sqes_at = Some(address),
            _ => return false,
        }
        true
    }

    /// Whether the guest has mapped everything it needs to use this.
    pub(crate) const fn mapped(self) -> bool {
        self.sq_at.is_some() && self.cq_at.is_some() && self.sqes_at.is_some()
    }

    /// The `io_uring_params` block the guest is handed back, filled in.
    ///
    /// A hundred and twenty bytes: what was asked for, what was given, what is promised, and where
    /// every part of both queues will be found.
    pub(crate) fn params(self) -> [u8; 120] {
        let mut out = [0u8; 120];
        let mut put = |at: usize, value: u32| {
            out[at..at + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(0, self.entries);
        put(4, self.completions());
        put(8, self.flags);
        put(20, FEATURES);

        // `sq_off`, forty bytes from byte forty.
        put(40, sq::HEAD as u32);
        put(44, sq::TAIL as u32);
        put(48, sq::RING_MASK as u32);
        put(52, sq::RING_ENTRIES as u32);
        put(56, sq::FLAGS as u32);
        put(60, sq::DROPPED as u32);
        put(64, sq::ARRAY as u32);

        // `cq_off`, forty bytes from byte eighty.
        put(80, cq::HEAD as u32);
        put(84, cq::TAIL as u32);
        put(88, cq::RING_MASK as u32);
        put(92, cq::RING_ENTRIES as u32);
        put(96, cq::OVERFLOW as u32);
        put(100, cq::ENTRIES as u32);
        put(104, cq::FLAGS as u32);
        out
    }

    /// The control block a freshly mapped submission queue starts with.
    ///
    /// Written by this world rather than left as the zeroes an anonymous mapping arrives as: the
    /// mask and the entry count are what the guest reads to know the shape of what it was given,
    /// and a caller that read zero for both would put every submission in slot nought.
    pub(crate) fn sq_control(self) -> [u8; sq::ARRAY as usize] {
        let mut out = [0u8; sq::ARRAY as usize];
        if self.polled {
            out[sq::FLAGS as usize..sq::FLAGS as usize + 4]
                .copy_from_slice(&SQ_NEED_WAKEUP.to_le_bytes());
        }
        out[sq::RING_MASK as usize..sq::RING_MASK as usize + 4]
            .copy_from_slice(&(self.entries - 1).to_le_bytes());
        out[sq::RING_ENTRIES as usize..sq::RING_ENTRIES as usize + 4]
            .copy_from_slice(&self.entries.to_le_bytes());
        out
    }

    /// The same for the completion queue.
    pub(crate) fn cq_control(self) -> [u8; cq::ENTRIES as usize] {
        let mut out = [0u8; cq::ENTRIES as usize];
        out[cq::RING_MASK as usize..cq::RING_MASK as usize + 4]
            .copy_from_slice(&(self.completions() - 1).to_le_bytes());
        out[cq::RING_ENTRIES as usize..cq::RING_ENTRIES as usize + 4]
            .copy_from_slice(&self.completions().to_le_bytes());
        out
    }

    /// Where the submission queue's head lives in the guest's memory.
    pub(crate) fn sq_head_at(self) -> Option<u64> {
        self.sq_at.map(|at| at + sq::HEAD)
    }

    /// Where the submission queue's tail lives.
    pub(crate) fn sq_tail_at(self) -> Option<u64> {
        self.sq_at.map(|at| at + sq::TAIL)
    }

    /// Where the count of entries dropped for naming no slot lives.
    pub(crate) fn sq_dropped_at(self) -> Option<u64> {
        self.sq_at.map(|at| at + sq::DROPPED)
    }

    /// Where the slot-to-entry index for one position lives.
    pub(crate) fn sq_array_at(self, position: u32) -> Option<u64> {
        let slot = u64::from(position & (self.entries - 1));
        self.sq_at.map(|at| at + sq::ARRAY + 4 * slot)
    }

    /// Where one submission entry lives, or nothing for an index past the queue.
    ///
    /// Not masked: a kernel drops an entry whose index names no slot and counts it, and a mask
    /// would quietly carry out whatever another slot held.
    pub(crate) fn sqe_at(self, index: u32) -> Option<u64> {
        if index >= self.entries {
            return None;
        }
        self.sqes_at.map(|at| at + SQE_BYTES * u64::from(index))
    }

    /// Where the completion queue's head lives: how far the guest has reaped.
    pub(crate) fn cq_head_at(self) -> Option<u64> {
        self.cq_at.map(|at| at + cq::HEAD)
    }

    /// Where the completion queue's tail lives.
    pub(crate) fn cq_tail_at(self) -> Option<u64> {
        self.cq_at.map(|at| at + cq::TAIL)
    }

    /// Where one completion entry lives.
    pub(crate) fn cqe_at(self, position: u32) -> Option<u64> {
        let slot = u64::from(position & (self.completions() - 1));
        self.cq_at.map(|at| at + cq::ENTRIES + CQE_BYTES * slot)
    }
}

/// What one submission asks for, as far as this world reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Submission {
    /// Which operation.
    pub(crate) opcode: u8,
    /// How it relates to its neighbours and to registered resources: the `IOSQE_*` bits.
    pub(crate) flags: u8,
    /// The descriptor it names.
    pub(crate) fd: i64,
    /// The offset, where the operation takes one.
    pub(crate) offset: u64,
    /// The buffer, or the path.
    pub(crate) address: u64,
    /// How many bytes, or which mode.
    pub(crate) len: u32,
    /// Operation-specific flags, in the word the interface puts them in.
    pub(crate) op_flags: u32,
    /// What the caller wants handed back with the completion, untouched.
    pub(crate) user_data: u64,
    /// A registered-file slot an open should fill, or zero for an ordinary descriptor.
    pub(crate) file_index: u32,
}

impl Submission {
    /// Reads one entry out of its sixty-four bytes.
    pub(crate) fn read(bytes: &[u8; SQE_BYTES as usize]) -> Self {
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let wide = |at: usize| {
            u64::from_le_bytes([
                bytes[at],
                bytes[at + 1],
                bytes[at + 2],
                bytes[at + 3],
                bytes[at + 4],
                bytes[at + 5],
                bytes[at + 6],
                bytes[at + 7],
            ])
        };
        Self {
            opcode: bytes[0],
            flags: bytes[1],
            fd: i64::from(word(4) as i32),
            offset: wide(8),
            address: wide(16),
            len: word(24),
            op_flags: word(28),
            user_data: wide(32),
            file_index: word(44),
        }
    }
}

/// The `IORING_OP_*` numbers this ring carries.
mod op {
    pub(crate) const NOP: u8 = 0;
    pub(crate) const FSYNC: u8 = 3;
    pub(crate) const OPENAT: u8 = 18;
    pub(crate) const CLOSE: u8 = 19;
    pub(crate) const STATX: u8 = 21;
    pub(crate) const READ: u8 = 22;
    pub(crate) const WRITE: u8 = 23;
    pub(crate) const UNLINKAT: u8 = 36;
}

/// The syscall numbers a submission is translated into.
mod call {
    pub(crate) const READ: u32 = 0;
    pub(crate) const WRITE: u32 = 1;
    pub(crate) const CLOSE: u32 = 3;
    pub(crate) const PREAD64: u32 = 17;
    pub(crate) const PWRITE64: u32 = 18;
    pub(crate) const FSYNC: u32 = 74;
    pub(crate) const FDATASYNC: u32 = 75;
    pub(crate) const OPENAT: u32 = 257;
    pub(crate) const UNLINKAT: u32 = 263;
    pub(crate) const STATX: u32 = 332;
}

/// What a submission is, once translated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plain {
    /// Nothing to carry out; completes with zero.
    Nothing,
    /// The syscall that does the same work, with its arguments.
    Call(u32, [u64; 6]),
}

/// The syscall a submission stands for, or why it stands for none this ring carries.
///
/// **The translation is the whole model of an operation.** Carrying a write out by any route other
/// than the one `pwrite64` takes would give the ring a file model of its own, and two file models
/// disagree. A position of `-1` means the descriptor's own, as in the interface, and becomes the
/// non-positional call.
pub(crate) fn plain_call(asked: &Submission) -> Result<Plain, String> {
    if asked.flags & SQE_FIXED_FILE != 0 {
        return Err("a registered file".to_owned());
    }
    if asked.flags & SQE_BUFFER_SELECT != 0 {
        return Err("a selected buffer".to_owned());
    }
    if asked.file_index != 0 && matches!(asked.opcode, op::OPENAT | op::CLOSE) {
        return Err("a registered slot".to_owned());
    }
    // `RWF_DSYNC`, `RWF_APPEND` and the rest change where a write lands or when it is durable; a
    // plain `pwrite64` would carry neither, so any of them is refused rather than dropped.
    if asked.op_flags != 0 && matches!(asked.opcode, op::READ | op::WRITE) {
        return Err(format!("read or write flags {:#x}", asked.op_flags));
    }
    let own_position = asked.offset == u64::MAX;
    let nr = match asked.opcode {
        op::NOP => return Ok(Plain::Nothing),
        op::READ if own_position => call::READ,
        op::READ => call::PREAD64,
        op::WRITE if own_position => call::WRITE,
        op::WRITE => call::PWRITE64,
        // `IORING_FSYNC_DATASYNC` is the only flag the operation has.
        op::FSYNC if asked.op_flags & 1 != 0 => call::FDATASYNC,
        op::FSYNC => call::FSYNC,
        op::OPENAT => call::OPENAT,
        op::CLOSE => call::CLOSE,
        op::STATX => call::STATX,
        op::UNLINKAT => call::UNLINKAT,
        other => return Err(format!("operation {other}")),
    };
    let (fd, address, len, flags) =
        (asked.fd as u64, asked.address, u64::from(asked.len), u64::from(asked.op_flags));
    let args = match asked.opcode {
        op::READ | op::WRITE => [fd, address, len, asked.offset, 0, 0],
        op::FSYNC | op::CLOSE => [fd, 0, 0, 0, 0, 0],
        // The mode rides in `len` and the open flags in the operation word.
        op::OPENAT => [fd, address, flags, len, 0, 0],
        // The mask rides in `len` and the result buffer in the offset word.
        op::STATX => [fd, address, flags, len, asked.offset, 0],
        _ => [fd, address, flags, 0, 0, 0],
    };
    Ok(Plain::Call(nr, args))
}

/// Whether an entry's descriptor is the thing it acts on, rather than a directory a path is under.
///
/// Those are the entries that must name a file: a write to a socket wakes its peer and a write to
/// a counter wakes a poller, and a submission carried out as a plain call has nowhere to put
/// either.
pub(crate) const fn acts_on_descriptor(asked: &Submission) -> bool {
    matches!(asked.opcode, op::READ | op::WRITE | op::FSYNC | op::CLOSE)
}

/// One completion to post: whose it is and what it returned.
pub(crate) type Completed = (u64, i64);

/// Carries out a batch in order, honouring links, and returns the completions to post.
///
/// **In order is a legal schedule and the only reproducible one.** A kernel may run unlinked
/// entries in any order, so no caller can depend on one, and running them as written satisfies
/// `IOSQE_IO_DRAIN` without doing anything. An entry that fails cancels the rest of its soft chain
/// with `ECANCELED`; a short read or write counts as failing there, because the next link would act
/// on bytes that never arrived.
pub(crate) fn carry_all<E>(
    batch: &[Submission],
    mut carry: impl FnMut(&Submission) -> Result<i64, E>,
) -> Result<Vec<Completed>, E> {
    let mut posted = Vec::with_capacity(batch.len());
    let mut cancelling = false;
    for asked in batch {
        let result = if cancelling { -ECANCELED } else { carry(asked)? };
        let short = matches!(asked.opcode, op::READ | op::WRITE)
            && result >= 0
            && (result as u64) < u64::from(asked.len);
        let failed = result < 0 || short;
        if failed || asked.flags & SQE_CQE_SKIP_SUCCESS == 0 {
            posted.push((asked.user_data, result));
        }
        // The chain ends at the first entry that does not link onward; a hard link carries on past
        // a failure, a soft one does not.
        if asked.flags & (SQE_IO_LINK | SQE_IO_HARDLINK) == 0 {
            cancelling = false;
        } else if failed && asked.flags & SQE_IO_HARDLINK == 0 {
            cancelling = true;
        }
    }
    Ok(posted)
}

/// One completion, as its sixteen bytes.
pub(crate) fn completion(user_data: u64, result: i64) -> [u8; CQE_BYTES as usize] {
    let mut out = [0u8; CQE_BYTES as usize];
    out[0..8].copy_from_slice(&user_data.to_le_bytes());
    // The result is an `int`, and a negative one is the errno the operation would have returned.
    out[8..12].copy_from_slice(&(result as i32).to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue is a power of two whatever was asked for, because the guest is handed a mask.
    #[test]
    fn a_queue_is_a_power_of_two() {
        assert_eq!(Ring::new(1, false).entries, 1);
        assert_eq!(Ring::new(3, false).entries, 4);
        assert_eq!(Ring::new(64, false).entries, 64);
        assert_eq!(Ring::new(100, false).entries, 128);
        assert_eq!(Ring::new(0, false).entries, 1, "and never nothing");
    }

    /// The mask the guest reads and the count it reads describe the same queue.
    ///
    /// A caller puts a submission at `tail & mask` and believes there are `entries` of them; the
    /// two disagreeing is how every submission lands in one slot.
    #[test]
    fn the_mask_and_the_count_agree() {
        let ring = Ring::new(8, false);
        let control = ring.sq_control();
        let mask = u32::from_le_bytes(control[8..12].try_into().unwrap());
        let count = u32::from_le_bytes(control[12..16].try_into().unwrap());
        assert_eq!(count, 8);
        assert_eq!(mask, 7);
        assert_eq!(mask + 1, count, "a mask is one less than a power of two, or it is not a mask");

        let control = ring.cq_control();
        let mask = u32::from_le_bytes(control[8..12].try_into().unwrap());
        let count = u32::from_le_bytes(control[12..16].try_into().unwrap());
        assert_eq!(count, 16, "twice the submissions");
        assert_eq!(mask + 1, count);
    }

    /// **What is reported is what is built.** A caller reads the offsets rather than assuming them,
    /// so the one thing that must hold is that the block describes the regions this hands out.
    #[test]
    fn the_reported_offsets_describe_the_regions_that_were_made() {
        let ring = Ring::new(4, false);
        let params = ring.params();
        let at = |i: usize| u32::from_le_bytes(params[i..i + 4].try_into().unwrap()) as u64;

        assert_eq!(at(0), 4, "submission slots");
        assert_eq!(at(4), 8, "completion slots");
        assert_eq!(at(20), u64::from(FEATURES), "and what this ring promises");

        // The index array must fit inside the region the submission queue is given.
        let array = at(64);
        assert!(
            array + 4 * u64::from(ring.entries) <= ring.region_bytes(offset::SQ_RING).unwrap(),
            "the array runs past the region it lives in"
        );
        // The entries must fit inside theirs.
        let entries = at(100);
        assert!(
            entries + CQE_BYTES * u64::from(ring.completions())
                <= ring.region_bytes(offset::CQ_RING).unwrap()
        );
        // And every control field must sit inside the block, ahead of what follows it.
        for field in [at(40), at(44), at(48), at(52), at(56), at(60)] {
            assert!(field + 4 <= array, "a control field overlaps the array");
        }
    }

    /// A region nobody defined is not one, and saying so is what keeps a stray mapping from
    /// silently becoming part of a ring.
    #[test]
    fn an_offset_that_names_no_region_is_refused() {
        let mut ring = Ring::new(4, false);
        assert!(ring.region_bytes(0x1234).is_none());
        assert!(!ring.placed(0x1234, 0x7000));
        assert!(ring.placed(offset::SQ_RING, 0x7000));
        assert!(!ring.mapped(), "one of three is not all of them");
    }

    /// Nothing is promised that this does not do.
    ///
    /// The two bits here are the two that follow from carrying every submission out before the
    /// call returns. Everything else -- one mapping for both queues, registered files, a stable
    /// buffer index -- is a promise about machinery this does not have, and a caller reading one of
    /// those takes a path that ends somewhere nobody wrote.
    #[test]
    fn only_what_is_true_is_promised() {
        const NODROP: u32 = 1 << 1;
        const SUBMIT_STABLE: u32 = 1 << 2;
        const SQPOLL_NONFIXED: u32 = 1 << 7;
        const SINGLE_MMAP: u32 = 1 << 0;
        assert_ne!(FEATURES & NODROP, 0);
        assert_ne!(FEATURES & SUBMIT_STABLE, 0);
        assert_ne!(FEATURES & SQPOLL_NONFIXED, 0, "read as a version, and stated as one");
        assert_eq!(FEATURES & SINGLE_MMAP, 0, "two regions are handed out, not one");
        assert_eq!(
            FEATURES & !(NODROP | SUBMIT_STABLE | SQPOLL_NONFIXED),
            0,
            "and nothing else at all"
        );
    }

    /// A polled queue is handed to the guest with its poller asleep, and an ordinary one is not.
    ///
    /// The flag is the whole of how a polled ring is made watchable: a caller that finds it set
    /// calls `io_uring_enter` before its work is looked at, which is the call this world answers.
    /// Setting it on a queue nobody asked to have polled would be inventing a state that queue
    /// never has.
    #[test]
    fn a_polled_queue_says_its_poller_is_asleep() {
        let flags = |polled| {
            let control = Ring::new(8, polled).sq_control();
            u32::from_le_bytes(control[16..20].try_into().unwrap())
        };
        assert_eq!(flags(true), SQ_NEED_WAKEUP);
        assert_eq!(flags(false), 0, "an ordinary queue has no poller to be asleep");
    }

    /// A submission reads back as what was written into it.
    #[test]
    fn a_submission_reads_back_whole() {
        let mut bytes = [0u8; SQE_BYTES as usize];
        bytes[0] = 23; // write
        bytes[4..8].copy_from_slice(&(-1i32).to_le_bytes());
        bytes[8..16].copy_from_slice(&4096u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0x7fff_0000u64.to_le_bytes());
        bytes[24..28].copy_from_slice(&512u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&0xdead_beefu64.to_le_bytes());

        let read = Submission::read(&bytes);
        assert_eq!(read.opcode, 23);
        assert_eq!(read.fd, -1, "a descriptor is an int, and the negative one is meaningful");
        assert_eq!(read.offset, 4096);
        assert_eq!(read.address, 0x7fff_0000);
        assert_eq!(read.len, 512);
        assert_eq!(read.user_data, 0xdead_beef);
        assert_eq!(read.flags, 0);
        assert_eq!(read.file_index, 0);
    }

    /// Setup hands its flags back, honours a completion size, and refuses what it cannot carry.
    #[test]
    fn setup_is_answered_as_a_kernel_answers_it() {
        let polled = Ring::from_setup(8, SETUP_SQPOLL, 0).unwrap();
        assert_eq!(u32::from_le_bytes(polled.params()[8..12].try_into().unwrap()), SETUP_SQPOLL);

        let sized = Ring::from_setup(8, SETUP_CQSIZE, 100).unwrap();
        assert_eq!(sized.completions(), 128);
        assert_eq!(Ring::from_setup(8, SETUP_CQSIZE, 4), Err(Setup::Invalid), "smaller than sq");

        assert_eq!(Ring::from_setup(0, 0, 0), Err(Setup::Invalid));
        assert_eq!(Ring::from_setup(MAX_ENTRIES + 1, 0, 0), Err(Setup::Invalid));
        assert_eq!(Ring::from_setup(MAX_ENTRIES + 1, SETUP_CLAMP, 0).unwrap().entries, MAX_ENTRIES);
        assert_eq!(Ring::from_setup(8, 1 << 10, 0), Err(Setup::Unsupported("IORING_SETUP_SQE128")));
        assert_eq!(Ring::from_setup(8, 1 << 30, 0), Err(Setup::Invalid), "a flag nobody defined");
    }

    /// Read and write flags are refused, not dropped: each changes the write a plain call makes.
    #[test]
    fn read_and_write_flags_are_refused() {
        let mut dsync = entry(op::WRITE, 0, 8);
        dsync.op_flags = 0x2;
        assert!(plain_call(&dsync).is_err());
    }

    /// An index past the queue names no entry, rather than the slot a mask would wrap it onto.
    #[test]
    fn an_index_past_the_queue_names_nothing() {
        let mut ring = Ring::new(4, false);
        ring.placed(offset::SQES, 0x9000);
        assert_eq!(ring.sqe_at(3), Some(0x9000 + 3 * SQE_BYTES));
        assert_eq!(ring.sqe_at(4), None);
    }

    fn entry(opcode: u8, flags: u8, len: u32) -> Submission {
        Submission {
            opcode,
            flags,
            fd: 5,
            offset: 0,
            address: 0x1000,
            len,
            op_flags: 0,
            user_data: u64::from(opcode),
            file_index: 0,
        }
    }

    /// A positional write is `pwrite64` and a write at the descriptor's own position is `write`.
    #[test]
    fn a_submission_is_the_plain_call_it_stands_for() {
        let mut asked = entry(op::WRITE, 0, 512);
        asked.offset = 4096;
        assert_eq!(
            plain_call(&asked),
            Ok(Plain::Call(call::PWRITE64, [5, 0x1000, 512, 4096, 0, 0]))
        );
        asked.offset = u64::MAX;
        assert_eq!(plain_call(&asked).map(|p| matches!(p, Plain::Call(call::WRITE, _))), Ok(true));

        let mut sync = entry(op::FSYNC, 0, 0);
        assert_eq!(plain_call(&sync), Ok(Plain::Call(call::FSYNC, [5, 0, 0, 0, 0, 0])));
        sync.op_flags = 1;
        assert_eq!(
            plain_call(&sync).map(|p| matches!(p, Plain::Call(call::FDATASYNC, _))),
            Ok(true)
        );
        assert_eq!(plain_call(&entry(op::NOP, 0, 0)), Ok(Plain::Nothing));
    }

    /// What this ring does not carry is named, never answered with a guess.
    #[test]
    fn what_is_not_carried_is_named() {
        assert!(plain_call(&entry(13, 0, 0)).is_err(), "accept is not a file operation");
        assert!(plain_call(&entry(op::READ, SQE_FIXED_FILE, 8)).is_err());
        assert!(plain_call(&entry(op::READ, SQE_BUFFER_SELECT, 8)).is_err());
        let mut slot = entry(op::OPENAT, 0, 0);
        slot.file_index = 3;
        assert!(plain_call(&slot).is_err());
    }

    /// A failed link cancels the rest of its chain, and the chain ends where linking stops.
    #[test]
    fn a_failed_link_cancels_its_chain_and_no_further() {
        let batch =
            [entry(op::WRITE, SQE_IO_LINK, 8), entry(op::FSYNC, 0, 0), entry(op::READ, 0, 8)];
        let done =
            carry_all(&batch, |asked| Ok::<_, ()>(if asked.opcode == op::WRITE { -28 } else { 0 }))
                .unwrap();
        assert_eq!(done[0].1, -28);
        assert_eq!(done[1].1, -ECANCELED, "the linked sync never ran");
        assert_eq!(done[2].1, 0, "the next chain is its own");
    }

    /// A hard link carries on past a failure; a short write breaks a soft one.
    #[test]
    fn a_hard_link_carries_on_and_a_short_write_breaks_a_soft_one() {
        let hard = [entry(op::WRITE, SQE_IO_HARDLINK, 8), entry(op::FSYNC, 0, 0)];
        let done =
            carry_all(&hard, |asked| Ok::<_, ()>(if asked.opcode == op::WRITE { -5 } else { 0 }))
                .unwrap();
        assert_eq!(done[1].1, 0);

        let soft = [entry(op::WRITE, SQE_IO_LINK, 8), entry(op::FSYNC, 0, 0)];
        let done =
            carry_all(&soft, |asked| Ok::<_, ()>(if asked.opcode == op::WRITE { 3 } else { 0 }))
                .unwrap();
        assert_eq!(done[1].1, -ECANCELED, "three bytes of eight is a failure to a link");
    }

    /// A success asked to be skipped posts nothing; a failure asked the same posts anyway.
    #[test]
    fn a_skipped_success_posts_nothing() {
        let batch =
            [entry(op::FSYNC, SQE_CQE_SKIP_SUCCESS, 0), entry(op::CLOSE, SQE_CQE_SKIP_SUCCESS, 0)];
        let done =
            carry_all(&batch, |asked| Ok::<_, ()>(if asked.opcode == op::CLOSE { -9 } else { 0 }))
                .unwrap();
        assert_eq!(done, vec![(u64::from(op::CLOSE), -9)]);
    }

    /// A completion carries back what the submission asked to have carried back.
    #[test]
    fn a_completion_carries_the_callers_own_number() {
        let posted = completion(0x1122_3344_5566_7788, -2);
        assert_eq!(u64::from_le_bytes(posted[0..8].try_into().unwrap()), 0x1122_3344_5566_7788);
        assert_eq!(i32::from_le_bytes(posted[8..12].try_into().unwrap()), -2, "the errno");
    }
}
