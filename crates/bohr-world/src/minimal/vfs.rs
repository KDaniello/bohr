//! Vfs: files, directories, links and descriptors: the filesystem a guest reads and writes.

use super::*;

/// The calls this subsystem answers.
pub(super) const HANDLES: &[u32] = &[
    sys::INOTIFY[0],
    sys::INOTIFY[1],
    sys::INOTIFY[2],
    sys::WRITE,
    sys::WRITEV,
    sys::PWRITEV,
    sys::PWRITE64,
    sys::READ,
    sys::PREAD64,
    sys::OPEN,
    sys::OPENAT,
    sys::CLOSE,
    sys::LSEEK,
    sys::FSTAT,
    sys::FTRUNCATE,
    sys::FSYNC,
    sys::FDATASYNC,
    sys::FLOCK,
    sys::UNLINK,
    sys::UNLINKAT,
    sys::RMDIR,
    sys::RENAME,
    sys::RENAMEAT,
    sys::RENAMEAT2,
    sys::FADVISE64,
    sys::IOCTL,
    sys::ACCESS,
    sys::FACCESSAT2,
    sys::READLINK,
    sys::DUP,
    sys::DUP2,
    sys::DUP3,
    sys::SYMLINK,
    sys::SYMLINKAT,
    sys::FCNTL,
    sys::GETCWD,
    sys::GETDENTS64,
    sys::MKDIR,
    sys::MKDIRAT,
    sys::PIPE2,
    sys::POLL,
    sys::READAHEAD,
    sys::STATX,
    sys::CHMOD,
    sys::FCHMODAT,
    sys::FCHMOD,
    sys::FALLOCATE,
    sys::SYNC_FILE_RANGE,
    sys::NEWFSTATAT,
    sys::FSTATFS,
    sys::STATFS,
    sys::READLINKAT,
];

impl MinimalWorld {
    /// Answers one of [`HANDLES`].
    pub(super) fn answer_vfs(
        &mut self,
        nr: u32,
        args: [u64; 6],
        ctx: &mut StopCtx<'_, '_>,
    ) -> Result<Answer, KernelError> {
        Ok(Answer::Reply(match nr {
            sys::WRITE => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let fd = args[0] as i64;
                let (ret, d) = if self.file_behind(fd).is_none() && (fd == 1 || fd == 2) {
                    self.write_out(ctx, GuestAddr(args[1]), len)?
                } else {
                    self.write_file(ctx, fd, GuestAddr(args[1]), len, None)?
                };
                (ret, Resume::Emulate { ret }, d)
            }
            // A gathering write, with or without its own position, is joined and written once.
            sys::WRITEV | sys::PWRITEV if too_many_iovecs(args[2]) => {
                (-errno::EINVAL, Resume::Emulate { ret: -errno::EINVAL }, None)
            }
            sys::WRITEV | sys::PWRITEV => {
                let fd = args[0] as i64;
                let joined = Self::gather(ctx, GuestAddr(args[1]), args[2])?;
                let at = (nr == sys::PWRITEV).then_some(args[3]);
                let (ret, d) = self.write_bytes(fd, &joined, at);
                (ret, Resume::Emulate { ret }, d)
            }
            sys::PWRITE64 => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let (ret, d) =
                    self.write_file(ctx, args[0] as i64, GuestAddr(args[1]), len, Some(args[3]))?;
                (ret, Resume::Emulate { ret }, d)
            }
            sys::READ => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let (ret, d) = self.read_in(ctx, args[0] as i64, GuestAddr(args[1]), len, None)?;
                (ret, Resume::Emulate { ret }, d)
            }
            sys::PREAD64 => {
                let len = usize::try_from(args[2]).unwrap_or(0);
                let (ret, d) =
                    self.read_in(ctx, args[0] as i64, GuestAddr(args[1]), len, Some(args[3]))?;
                (ret, Resume::Emulate { ret }, d)
            }
            // `open` names its path one argument earlier than `openat`.
            sys::OPEN | sys::OPENAT => {
                let (dirfd, at) = if nr == sys::OPEN {
                    (AT_FDCWD, (args[0], args[1]))
                } else {
                    (dirfd_of(args[0]), (args[1], args[2]))
                };
                let ret = self.open_at(ctx, dirfd, GuestAddr(at.0), at.1)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::CLOSE => {
                let ret = self.close_number(args[0] as i64);
                (ret, Resume::Emulate { ret }, None)
            }
            sys::LSEEK => {
                let ret = self.seek(args[0] as i64, args[1] as i64, args[2]);
                (ret, Resume::Emulate { ret }, None)
            }
            sys::FSTAT => {
                let ret = self.write_stat(ctx, args[0] as i64, GuestAddr(args[1]))?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::FTRUNCATE => {
                let ret = self.truncate(args[0] as i64, args[1]);
                (ret, Resume::Emulate { ret }, None)
            }
            // Nothing crashes in this model; the call is recorded for the crash model above it.
            sys::FSYNC | sys::FDATASYNC => {
                let ret = if self.open.contains_key(&(args[0] as i64)) { 0 } else { -errno::EBADF };
                (ret, Resume::Emulate { ret }, None)
            }
            // One task runs at a time, so advisory locks are always granted.
            sys::FLOCK => {
                let ret = if self.open.contains_key(&(args[0] as i64)) { 0 } else { -errno::EBADF };
                (ret, Resume::Emulate { ret }, None)
            }
            // `rmdir` always means a directory.
            sys::UNLINK | sys::UNLINKAT | sys::RMDIR => {
                let at = if nr == sys::UNLINKAT { args[1] } else { args[0] };
                // The content stays for open descriptors. Without `AT_REMOVEDIR` a directory is
                // refused, as on Linux.
                const AT_REMOVEDIR: u64 = 0x200;
                let removing_dir =
                    nr == sys::RMDIR || (nr == sys::UNLINKAT && args[2] & AT_REMOVEDIR != 0);
                let dirfd = if nr == sys::UNLINKAT { dirfd_of(args[0]) } else { AT_FDCWD };
                let ret = match read_c_string(ctx, GuestAddr(at))? {
                    PathArg::Named(name) => match self.under(dirfd, &name) {
                        Err(errno) => errno,
                        // A link is a name of its own: `unlink` removes it, `rmdir` refuses it.
                        Ok(path) if self.links.contains_key(&path) => {
                            if removing_dir {
                                -errno::ENOTDIR
                            } else {
                                self.links.remove(&path);
                                0
                            }
                        }
                        Ok(path) => match self.names.get(&path).copied() {
                            Some(inode) if self.directories.contains(&inode) && !removing_dir => {
                                -errno::EISDIR
                            }
                            Some(inode) if !self.directories.contains(&inode) && removing_dir => {
                                -errno::ENOTDIR
                            }
                            // A directory goes only once nothing is in it.
                            Some(_) if removing_dir && self.has_children(&path) => {
                                -errno::ENOTEMPTY
                            }
                            Some(inode) => {
                                self.names.remove(&path);
                                // Removed from the directory set too.
                                self.directories.remove(&inode);
                                self.collect(inode);
                                0
                            }
                            None => -errno::ENOENT,
                        },
                    },
                    other => other.errno(),
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Three spellings: one that takes no directories and no flags, one that takes
            // directories, and one that takes both.
            sys::RENAME | sys::RENAMEAT | sys::RENAMEAT2 => {
                let (from, to) =
                    if nr == sys::RENAME { (args[0], args[1]) } else { (args[1], args[3]) };
                // `renameat2` flags.
                let flags = if nr == sys::RENAMEAT2 { args[4] } else { 0 };
                let dirs = if nr == sys::RENAME {
                    (AT_FDCWD, AT_FDCWD)
                } else {
                    (dirfd_of(args[0]), dirfd_of(args[2]))
                };
                let ret = self.rename(ctx, dirs, GuestAddr(from), GuestAddr(to), flags)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Access advice has no observable effect.
            sys::FADVISE64 => (0, Resume::Emulate { ret: 0 }, None),
            // Nothing here is a terminal.
            sys::IOCTL => (-errno::ENOTTY, Resume::Emulate { ret: -errno::ENOTTY }, None),
            // Answered from the same names as `stat` and `openat`.
            sys::ACCESS | sys::FACCESSAT2 => {
                let (dirfd, at) = if nr == sys::ACCESS {
                    (AT_FDCWD, args[0])
                } else {
                    (dirfd_of(args[0]), args[1])
                };
                let ret = match read_c_string(ctx, GuestAddr(at))? {
                    PathArg::Named(name) => match self.under(dirfd, &name) {
                        Ok(path) if self.names.contains_key(&self.resolve(&path)) => 0,
                        Ok(_) => -errno::ENOENT,
                        Err(errno) => errno,
                    },
                    other => other.errno(),
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Not a symbolic link.
            sys::READLINK => {
                let ret =
                    self.read_link(ctx, AT_FDCWD, GuestAddr(args[0]), GuestAddr(args[1]), args[2])?;
                (ret, Resume::Emulate { ret }, None)
            }
            // A duplicate onto an open number closes what was there.
            sys::DUP | sys::DUP2 | sys::DUP3 => {
                let onto = (nr != sys::DUP).then(|| args[1] as i64);
                let ret = self.duplicate(args[0] as i64, onto, nr == sys::DUP3, args[2]);
                (ret, Resume::Emulate { ret }, None)
            }
            // `symlinkat` names its link in the third argument, `symlink` in the second.
            sys::SYMLINK | sys::SYMLINKAT => {
                let (dirfd, at) = if nr == sys::SYMLINK {
                    (AT_FDCWD, args[1])
                } else {
                    (dirfd_of(args[1]), args[2])
                };
                let ret = self.make_link(ctx, dirfd, GuestAddr(args[0]), GuestAddr(at))?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::FCNTL => {
                let ret = self.fcntl(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // From `state.cwd`.
            sys::GETCWD => {
                let mut answer = self.cwd.clone().into_bytes();
                answer.push(0);
                let ret = Self::write_bytes_out(ctx, GuestAddr(args[0]), args[1], &answer)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::GETDENTS64 => {
                let ret =
                    self.list_directory(ctx, args[0] as i64, GuestAddr(args[1]), args[2] as usize)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // `mkdirat` names its path one argument along.
            sys::MKDIR | sys::MKDIRAT => {
                let (dirfd, at) = if nr == sys::MKDIR {
                    (AT_FDCWD, args[0])
                } else {
                    (dirfd_of(args[0]), args[1])
                };
                let ret = self.make_directory(ctx, dirfd, GuestAddr(at))?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Pipes are stream pairs, which only a cluster provides.
            sys::PIPE2 => {
                let ret = self.unimplemented(ctx, nr)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // Files, directories and standard descriptors are always ready; unknown ones are
            // `POLLNVAL`. Otherwise the timeout is spent in virtual time, or the wait is forever.
            sys::POLL => {
                let ready = self.poll_ready(ctx, GuestAddr(args[0]), args[1])?;
                let timeout = args[2] as i32;
                if ready != 0 || timeout == 0 {
                    (ready, Resume::Emulate { ret: ready }, None)
                } else {
                    ctx.emit(EventKind::Syscall {
                        nr,
                        args,
                        ret: 0,
                        emulated: true,
                        data_digest: None,
                    })?;
                    if timeout < 0 {
                        return Ok(Answer::Done(Effects::wait(SYSCALL_COST)));
                    }
                    let ahead = Tick(u64::from(timeout.unsigned_abs()).saturating_mul(1_000_000));
                    return Ok(Answer::Done(Effects::go(Resume::Emulate { ret: 0 }, ahead)));
                }
            }
            // Read-ahead hints are accepted and ignored.
            sys::READAHEAD => {
                let ret = if self.is_file(args[0] as i64) { 0 } else { -errno::EBADF };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::STATX => {
                let ret = self.statx_at(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::CHMOD | sys::FCHMODAT => {
                let (dirfd, path, mode) = if nr == sys::CHMOD {
                    (AT_FDCWD as u64, args[0], args[1])
                } else {
                    (args[0], args[1], args[2])
                };
                let ret = match self.stat_target(ctx, dirfd, path, 0)? {
                    Ok((Some(inode), _)) => {
                        self.modes.insert(inode, (mode as u32) & 0o7777);
                        0
                    }
                    Ok((None, _)) => 0,
                    Err(errno) => errno,
                };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::FCHMOD => {
                let ret = match self.open.get(&(args[0] as i64)) {
                    Some(file) => {
                        if let Some(inode) = file.inode {
                            self.modes.insert(inode, (args[1] as u32) & 0o7777);
                        }
                        0
                    }
                    None => -errno::EBADF,
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // Reserve or extend with zeros; hole punching and collapsing are refused.
            sys::FALLOCATE => {
                /// `FALLOC_FL_KEEP_SIZE`.
                const KEEP_SIZE: u64 = 1;
                let fd = args[0] as i64;
                let ret = if !self.open.contains_key(&fd) {
                    -errno::EBADF
                } else if !self.is_file(fd) {
                    -errno::ENODEV
                } else {
                    match args[1] {
                        0 => {
                            let end = args[2].saturating_add(args[3]);
                            let length = self.length_of(fd).unwrap_or(0);
                            if end > length { self.truncate(fd, end) } else { 0 }
                        }
                        KEEP_SIZE => {
                            if let Some(inode) = self.inode_of(fd) {
                                let end = args[2].saturating_add(args[3]);
                                let reserved = self.reserved.entry(inode).or_insert(0);
                                *reserved = (*reserved).max(end);
                            }
                            0
                        }
                        _ => -errno::EOPNOTSUPP,
                    }
                };
                (ret, Resume::Emulate { ret }, None)
            }
            // `sync_file_range` promises nothing durable.
            sys::SYNC_FILE_RANGE => {
                let fd = args[0] as i64;
                let ret = if !self.open.contains_key(&fd) {
                    -errno::EBADF
                } else if self.is_file(fd) {
                    0
                } else {
                    -errno::ESPIPE
                };
                (ret, Resume::Emulate { ret }, None)
            }
            sys::NEWFSTATAT => {
                let ret = self.stat_at(ctx, args)?;
                (ret, Resume::Emulate { ret }, None)
            }
            // An unheld descriptor is `EBADF`.
            sys::FSTATFS if !self.open.contains_key(&(args[0] as i64)) => {
                (-errno::EBADF, Resume::Emulate { ret: -errno::EBADF }, None)
            }
            sys::STATFS | sys::FSTATFS => {
                let named = if nr == sys::FSTATFS {
                    self.open.contains_key(&(args[0] as i64))
                } else {
                    match read_c_string(ctx, GuestAddr(args[0]))? {
                        PathArg::Named(name) => self
                            .under(AT_FDCWD, &name)
                            .is_ok_and(|path| self.names.contains_key(&self.resolve(&path))),
                        _ => false,
                    }
                };
                let ret = self.write_statfs(ctx, named, GuestAddr(args[1]))?;
                (ret, Resume::Emulate { ret }, None)
            }
            sys::READLINKAT => {
                let ret = self.read_link(
                    ctx,
                    dirfd_of(args[0]),
                    GuestAddr(args[1]),
                    GuestAddr(args[2]),
                    args[3],
                )?;
                (ret, Resume::Emulate { ret }, None)
            }
            // File watching is refused; programs fall back to polling.
            nr if sys::INOTIFY.contains(&nr) => {
                let ret = self.unimplemented(ctx, nr)?;
                (ret, Resume::Emulate { ret }, None)
            }
            _ => {
                return Err(KernelError::Invariant(
                    "a call routed to a subsystem that does not answer it",
                ));
            }
        }))
    }
}
