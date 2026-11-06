use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::CString;
use std::rc::Rc;
use std::time::{Duration, Instant};

use async_lock::RwLock;
use fuse_abi::linux::{
    FATTR_ATIME, FATTR_ATIME_NOW, FATTR_FH, FATTR_GID, FATTR_MODE, FATTR_MTIME, FATTR_MTIME_NOW,
    FATTR_SIZE, FATTR_UID, FUSE_ATOMIC_O_TRUNC, fuse_setattr_in,
};
use libredox::flag::RENAME_NOREPLACE;
use log::warn;
use scheme_utils::HandleMap;
use syscall::error::{ESTALE, Error, Result};
use syscall::{
    EBADF, EEXIST, EINVAL, EISDIR, ELOOP, ENOENT, ENOTDIR, EPERM, MODE_DIR, MODE_FILE,
    MODE_SYMLINK, MODE_TYPE, O_ACCMODE, O_APPEND, O_CREAT, O_DIRECTORY, O_EXCL, O_NONBLOCK,
    O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY, TimeSpec,
};

use crate::fuse::{
    Caller, FileHandle, FuseEntryOut, FuseReaddirOut, FuseSession, FuseTransporter, NodeId,
    fuse_attr, fuse_attr_out, fuse_kstatfs,
};
use crate::linux_abi;

// futimens puts these in TimeSpec.tv_nsec:
// - UTIME_NOW = "use the current time",
// - UTIME_OMIT = "leave this stamp alone".
// They are not nanoseconds (NOW is > 1e9), so they cannot go on the wire as atimensec/mtimensec.
// FUSE uses FATTR_ATIME_NOW / FATTR_MTIME_NOW for NOW, and no bit at all for OMIT.
const UTIME_NOW: i32 = 0x3fffffff;
const UTIME_OMIT: i32 = 0x3ffffffe;

// TODO: Make scheme accept a config option to set the maximum number of inodes.
// Consequently selftests would need to be made aware of the value dynamically.
#[cfg(not(test))]
const INODES_MAX_LEN: usize = 64 * 1024;
#[cfg(test)]
const INODES_MAX_LEN: usize = 8;

const SYMLOOP_MAX: u32 = 40;
const FORGET_QUEUE_MIN_CAP: usize = 1024;

const LRU_EVICT_LOG_CAP: usize = 8192;
pub const LRU_DEBUG_EVICTED_NAME: &str = "lru_evicted";

#[derive(Debug)]
pub struct WalkResult {
    pub nodeid: NodeId,
    pub node_mode: u16,
    /// The directory `nodeid` was looked up in. `None` unless the last step was a lookup,
    /// so a walk ending in `.` or `..` does not pass for a name in a directory.
    parent_nodeid: Option<NodeId>,
    pub rel_path: Vec<String>,
    pub last_to_be_created: bool,
}

/// A name in a directory, resolved for an operation on the name itself (unlink, link,
/// rename). Pins the parent, and the target if it exists, until dropped, so a second
/// resolve can await without them being evicted.
pub struct Entry<'a, T: FuseTransporter> {
    parent: NodeId,
    name: String,
    /// `None` if the name does not exist.
    nodeid: Option<NodeId>,
    /// From the share root, for rewriting the paths of open handles after a rename.
    path: String,
    _parent_pin: InodePin<'a, T>,
    node_pin: Option<InodePin<'a, T>>,
}

struct Inode {
    generation: u64,
    parent: NodeId,
    attr: fuse_attr,
    attr_expiry: Instant,
    nlookup: u64,
    forget_pending: bool,
    open_fd_count: u64,
    pin_count: u64,
    lru_timestamp: u64,
    /// Shared for reads, exclusive for writes. Held for the whole call so a
    /// write cannot interleave with other I/O on this inode, and so the
    /// `O_APPEND` getattr-then-write sequence is atomic.
    io_lock: Rc<RwLock<()>>,
}

impl Inode {
    fn attr_valid(&self) -> bool {
        Instant::now() < self.attr_expiry
    }

    fn mode_type(&self) -> u16 {
        self.attr.mode as u16 & MODE_TYPE
    }
}

struct Dentry {
    nodeid: NodeId,
    entry_expiry: Instant,
}

impl Dentry {
    fn is_valid(&self) -> bool {
        Instant::now() < self.entry_expiry
    }
}

// `caller` is who opened the handle. Requests on an open handle carry it instead of
// whoever calls read/write later, since the fd may have been passed to another process.
enum Handle {
    File {
        nodeid: NodeId,
        fh: FileHandle,
        path: String,
        caller: Caller,
    },
    Directory {
        nodeid: NodeId,
        fh: FileHandle,
        path: String,
        caller: Caller,
    },
    StatOnly {
        nodeid: NodeId,
        path: String,
        caller: Caller,
    },
    Symlink {
        nodeid: NodeId,
        path: String,
        target: String,
        caller: Caller,
    },
    /// Redox doesn't have a dedicated symlink creation scheme call,
    /// relibc splits that into a create plus a write.
    SymlinkPreCreate {
        parent_nodeid: NodeId,
        name: String,
        path: String,
        caller: Caller,
    },
    SchemeRoot,
    /// Root of the debug scheme. It only contains the synthetic debug nodes
    /// and never reaches the FUSE filesystem.
    DebugRoot,
    LruLog,
}

impl Handle {
    fn opener(&self) -> Caller {
        match self {
            Handle::File { caller, .. }
            | Handle::Directory { caller, .. }
            | Handle::StatOnly { caller, .. }
            | Handle::Symlink { caller, .. }
            | Handle::SymlinkPreCreate { caller, .. } => *caller,
            // The scheme root is opened by the daemon itself, so it has no opener.
            Handle::SchemeRoot | Handle::DebugRoot | Handle::LruLog => Caller::NONE,
        }
    }

    fn nodeid(&self) -> Result<NodeId> {
        match self {
            Handle::File { nodeid, .. }
            | Handle::Directory { nodeid, .. }
            | Handle::StatOnly { nodeid, .. }
            | Handle::Symlink { nodeid, .. } => Ok(*nodeid),
            Handle::SchemeRoot => Ok(NodeId::ROOT),
            Handle::SymlinkPreCreate { .. } | Handle::DebugRoot | Handle::LruLog => {
                Err(Error::new(EBADF))
            }
        }
    }

    fn fh(&self) -> Option<FileHandle> {
        match self {
            Handle::File { fh, .. } | Handle::Directory { fh, .. } => Some(*fh),
            _ => None,
        }
    }
}

struct VfsState {
    inodes: BTreeMap<NodeId, Inode>,
    /// Cached names per directory. A key is a cached inode with at least one name, so a
    /// directory has children exactly when it has a key here.
    dcache: BTreeMap<NodeId, BTreeMap<String, Dentry>>,
    /// Reverse index: child nodeid -> (parent, name) for every live dentry.
    dentry_index: BTreeMap<NodeId, BTreeSet<(NodeId, String)>>,
    handles: HandleMap<Handle>,
    /// `(nodeid, nlookup)` pairs waiting to be sent as `FUSE_FORGET`.
    forget_queue: VecDeque<(NodeId, u64)>,
    /// Logical clock for LRU eviction. It ticks on every `touch`, not with wall time.
    lru_clock: u64,
    /// `(lru_timestamp, nodeid)` of every evictable inode, oldest first, so picking a victim
    /// never scans the cache.
    ///
    /// Invariant: it holds exactly the inodes for which `evictable` is true, keyed by their
    /// current `lru_timestamp`. Hence every write to `lru_timestamp`, `pin_count` or
    /// `open_fd_count` goes through `update_lru`, and so does every change to a directory's
    /// entry in `dcache` (`insert_dentry`, `drop_dentry`).
    lru_evictables: BTreeSet<(u64, NodeId)>,
    lru_full_warned: bool,
    evicted_names: VecDeque<String>,
}

pub struct Vfs<T: FuseTransporter> {
    fuse: FuseSession<T>,
    // We interleave requests on the same thread, so we only need
    // interior mutability, not thread safety.
    state: RefCell<VfsState>,
}

/// Pins `nodeid` against LRU eviction until dropped.
struct InodePin<'a, T: FuseTransporter> {
    vfs: &'a Vfs<T>,
    nodeid: NodeId,
}

impl<T: FuseTransporter> Drop for InodePin<'_, T> {
    fn drop(&mut self) {
        self.vfs.state.borrow_mut().unpin(self.nodeid);
    }
}

impl VfsState {
    fn touch(&mut self, nodeid: NodeId) {
        self.lru_clock = self
            .lru_clock
            .checked_add(1)
            .expect("virtio-fsd: LRU clock overflow");
        let now = self.lru_clock;
        self.update_lru(nodeid, |i| i.lru_timestamp = now);
    }

    fn idle(inode: &Inode) -> bool {
        inode.open_fd_count == 0 && inode.pin_count == 0
    }

    fn evictable(nodeid: NodeId, inode: &Inode, has_children: bool) -> bool {
        nodeid != NodeId::ROOT && Self::idle(inode) && !has_children
    }

    /// Removes the inode from `lru_evictables` and applies `f` to it.
    /// If the inode is still evictable after `f` is applied, it is re-inserted into `lru_evictables`.
    ///
    /// Returns `Some(R)` where `R` is the return value of `f` if `nodeid` is cached, or `None` if it is not cached.
    fn update_lru<R>(&mut self, nodeid: NodeId, f: impl FnOnce(&mut Inode) -> R) -> Option<R> {
        let inode = self.inodes.get_mut(&nodeid)?;
        self.lru_evictables.remove(&(inode.lru_timestamp, nodeid));

        let out = f(inode);

        if Self::evictable(nodeid, inode, self.dcache.contains_key(&nodeid)) {
            self.lru_evictables.insert((inode.lru_timestamp, nodeid));
        }
        Some(out)
    }

    fn pin(&mut self, nodeid: NodeId) -> Result<()> {
        self.update_lru(nodeid, |i| {
            i.pin_count = count_inc(i.pin_count, "pin count", nodeid);
        })
        .ok_or(Error::new(ESTALE))
    }

    fn unpin(&mut self, nodeid: NodeId) {
        self.update_lru(nodeid, |i| {
            i.pin_count = count_dec(i.pin_count, "pin count", nodeid);
        });
        self.drop_if_unused(nodeid);
    }

    // The only place that finalizes the removal of an unlinked inode.
    fn drop_if_unused(&mut self, nodeid: NodeId) {
        if self
            .inodes
            .get(&nodeid)
            .is_some_and(|i| i.forget_pending && Self::idle(i))
        {
            self.remove_inode(nodeid);
        }
    }

    fn insert_inode(&mut self, parent: NodeId, entry: &FuseEntryOut, attr_expiry: Instant) {
        self.populate_forgets();
        self.inodes.insert(
            entry.nodeid,
            Inode {
                generation: entry.generation,
                parent,
                attr: entry.attr,
                attr_expiry,
                nlookup: 1,
                forget_pending: false,
                open_fd_count: 0,
                pin_count: 0,
                lru_timestamp: 0,
                io_lock: Rc::new(RwLock::new(())),
            },
        );

        self.update_lru(entry.nodeid, |_| ());
    }

    fn unindex_dentry(&mut self, nodeid: NodeId, parent: NodeId, name: &str) {
        let Some(set) = self.dentry_index.get_mut(&nodeid) else {
            return;
        };
        set.remove(&(parent, name.to_owned()));
        if set.is_empty() {
            self.dentry_index.remove(&nodeid);
        }
    }

    fn insert_dentry(&mut self, parent: NodeId, name: &str, nodeid: NodeId, entry_expiry: Instant) {
        if !self.inodes.contains_key(&parent) {
            return;
        }
        let old = self.dcache.entry(parent).or_default().insert(
            name.to_owned(),
            Dentry {
                nodeid,
                entry_expiry,
            },
        );

        // `parent` has children at this point, so it is no longer evictable.
        self.update_lru(parent, |_| ());
        match old {
            Some(old) if old.nodeid == nodeid => return,
            Some(old) => self.unindex_dentry(old.nodeid, parent, name),
            None => {}
        }
        self.dentry_index
            .entry(nodeid)
            .or_default()
            .insert((parent, name.to_owned()));
    }

    fn drop_dentry(&mut self, parent: NodeId, name: &str) -> Option<Dentry> {
        let map = self.dcache.get_mut(&parent)?;
        let dentry = map.remove(name)?;
        if map.is_empty() {
            self.dcache.remove(&parent);
        }
        self.unindex_dentry(dentry.nodeid, parent, name);

        // `parent` may have lost its last child, which makes it evictable again.
        self.update_lru(parent, |_| ());
        Some(dentry)
    }

    fn unmerge_entry(&mut self, parent: NodeId, name: &str, nodeid: NodeId) {
        self.drop_dentry(parent, name);
        let Some(inode) = self.inodes.get_mut(&nodeid) else {
            return;
        };

        inode.nlookup = count_dec(inode.nlookup, "nlookup", nodeid);
        inode.forget_pending |= inode.nlookup == 0;
        self.forget_queue.push_back((nodeid, 1));
        self.drop_if_unused(nodeid);
    }

    fn unlink_name(&mut self, parent: NodeId, name: &str, nodeid: NodeId) {
        self.drop_dentry(parent, name);
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            inode.forget_pending = true;
            inode.attr_expiry = Instant::now();
        }
        self.drop_if_unused(nodeid);
    }

    fn merge_entry(&mut self, parent: NodeId, name: &str, entry: FuseEntryOut) -> Result<()> {
        let now = Instant::now();
        let attr_expiry = fuse_expiry(now, entry.attr_valid, entry.attr_valid_nsec);
        let entry_expiry = fuse_expiry(now, entry.entry_valid, entry.entry_valid_nsec);

        let nodeid = entry.nodeid;

        let mismatch = if let Some(inode) = self.inodes.get_mut(&nodeid) {
            if inode.generation != entry.generation {
                warn!(
                    "virtio-fsd: generation mismatch nodeid={nodeid} have={} got={}",
                    inode.generation, entry.generation
                );
                true
            } else {
                inode.nlookup = count_inc(inode.nlookup, "nlookup", nodeid);
                inode.attr = entry.attr;
                inode.attr_expiry = attr_expiry;
                if inode.mode_type() == MODE_DIR {
                    inode.parent = parent;
                }
                false
            }
        } else {
            self.insert_inode(parent, &entry, attr_expiry);
            false
        };
        if mismatch {
            self.forget_queue.push_back((nodeid, 1));
            return Err(Error::new(ESTALE));
        }

        self.insert_dentry(parent, name, nodeid, entry_expiry);
        self.touch(nodeid);
        Ok(())
    }

    fn increment_fd_count(&mut self, nodeid: NodeId) -> Result<()> {
        self.update_lru(nodeid, |i| {
            i.open_fd_count = count_inc(i.open_fd_count, "open fd count", nodeid);
        })
        .ok_or_else(|| {
            warn!("virtio-fsd: increment_fd_count: no inode for nodeid={nodeid}");
            Error::new(EBADF)
        })
    }

    fn decrement_fd_count(&mut self, nodeid: NodeId) {
        let updated = self.update_lru(nodeid, |i| {
            i.open_fd_count = count_dec(i.open_fd_count, "open fd count", nodeid);
        });
        if updated.is_none() {
            warn!("virtio-fsd: decrement_fd_count: no inode for nodeid={nodeid}");
            return;
        }
        self.drop_if_unused(nodeid);
    }

    fn insert_open_handle(&mut self, handle: Handle) -> Result<usize> {
        self.increment_fd_count(handle.nodeid()?)?;
        Ok(self.handles.insert(handle))
    }

    fn commit_symlink(
        &mut self,
        fd: usize,
        parent: NodeId,
        nodeid: NodeId,
        path: String,
        target: String,
        caller: Caller,
    ) -> Result<()> {
        self.increment_fd_count(nodeid)?;
        match self.handles.get_mut(fd) {
            Ok(handle) => {
                *handle = Handle::Symlink {
                    nodeid,
                    path,
                    target,
                    caller,
                };
                self.decrement_fd_count(parent);
                Ok(())
            }
            Err(err) => {
                self.decrement_fd_count(nodeid);
                Err(err)
            }
        }
    }

    fn cached_child(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        let dentry = self.dcache.get(&parent)?.get(name)?;
        if !dentry.is_valid() || !self.inodes.contains_key(&dentry.nodeid) {
            return None;
        }
        Some(dentry.nodeid)
    }

    fn dcache_name(&self, nodeid: NodeId) -> Option<String> {
        self.dentry_index
            .get(&nodeid)
            .and_then(|set| set.iter().next().map(|(_, name)| name.clone()))
    }

    /// The eviction log as served by `lru_evicted`: one name per line.
    fn eviction_log(&self) -> String {
        let mut log = String::new();
        for (i, name) in self.evicted_names.iter().enumerate() {
            if i > 0 {
                log.push('\n');
            }
            log.push_str(name);
        }
        log
    }

    fn refresh_attr(&mut self, nodeid: NodeId, payload: fuse_attr_out) -> u16 {
        let mode = payload.attr.mode as u16 & MODE_TYPE;
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            inode.attr = payload.attr;
            inode.attr_expiry =
                fuse_expiry(Instant::now(), payload.attr_valid, payload.attr_valid_nsec);
        }
        mode
    }

    fn pop_lru_victim(&mut self) -> Option<NodeId> {
        while let Some((ts, id)) = self.lru_evictables.pop_first() {
            let has_children = self.dcache.contains_key(&id);
            match self.inodes.get(&id) {
                Some(i) if i.lru_timestamp == ts && Self::evictable(id, i, has_children) => {
                    return Some(id);
                }
                _ => {
                    warn!("virtio-fsd: dropping stale LRU entry for nodeid={id}");
                    debug_assert!(false, "stale LRU entry for nodeid={id}");
                }
            }
        }
        None
    }

    fn populate_forgets(&mut self) {
        while self.inodes.len() >= INODES_MAX_LEN {
            let Some(id) = self.pop_lru_victim() else {
                if !self.lru_full_warned {
                    self.lru_full_warned = true;
                    warn!(
                        "virtio-fsd: LRU full ({}/{}), nothing evictable",
                        self.inodes.len(),
                        INODES_MAX_LEN
                    );
                }
                return;
            };

            if let Some(name) = self.dcache_name(id) {
                if self.evicted_names.len() >= LRU_EVICT_LOG_CAP {
                    self.evicted_names.pop_front();
                }
                self.evicted_names.push_back(name);
            }
            self.remove_inode(id);
        }
        self.lru_full_warned = false;
    }

    fn remove_inode(&mut self, nodeid: NodeId) {
        if nodeid == NodeId::ROOT {
            return;
        }
        let Some(inode) = self.inodes.remove(&nodeid) else {
            return;
        };

        self.lru_evictables.remove(&(inode.lru_timestamp, nodeid));
        if inode.nlookup > 0 {
            self.forget_queue.push_back((nodeid, inode.nlookup));
        }

        // Names pointing at this inode, removed from their directories.
        if let Some(names) = self.dentry_index.remove(&nodeid) {
            for (parent, name) in names {
                self.drop_dentry(parent, &name);
            }
        }

        // Names inside this inode, if it is a directory.
        if let Some(children) = self.dcache.remove(&nodeid) {
            for (name, d) in children {
                self.unindex_dentry(d.nodeid, nodeid, &name);
            }
        }
    }

    fn inode_mode(&self, nodeid: NodeId) -> Result<u16> {
        self.inodes
            .get(&nodeid)
            .map(Inode::mode_type)
            .ok_or(Error::new(ESTALE))
    }

    fn invalidate_attr(&mut self, nodeid: NodeId) {
        if let Some(inode) = self.inodes.get_mut(&nodeid) {
            inode.attr_expiry = Instant::now();
        }
    }

    fn parent_of(&mut self, nodeid: NodeId) -> Result<(NodeId, u16)> {
        let parent_id = self.inodes.get(&nodeid).ok_or(Error::new(ESTALE))?.parent;
        let parent_mode_type = self
            .inodes
            .get(&parent_id)
            .ok_or(Error::new(ESTALE))?
            .mode_type();

        self.touch(nodeid);
        self.touch(parent_id);

        Ok((parent_id, parent_mode_type))
    }
}

impl<T: FuseTransporter> Vfs<T> {
    async fn flush_forgets(&self, caller: Caller) {
        loop {
            let item = self.state.borrow_mut().forget_queue.pop_front();
            let Some((nodeid, nlookup)) = item else {
                break;
            };
            if let Err(err) = self.fuse.forget(nodeid, nlookup, caller).await {
                warn!("virtio-fsd: FUSE_FORGET: {err}");
                self.state
                    .borrow_mut()
                    .forget_queue
                    .push_front((nodeid, nlookup));
                break;
            }
        }
    }

    pub fn new(fuse_session: FuseSession<T>) -> Self {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            NodeId::ROOT,
            Inode {
                // The root is never returned by a lookup, so this is never compared.
                generation: 0,
                parent: NodeId::ROOT,
                attr: fuse_attr {
                    ino: NodeId::ROOT.into(),
                    mode: u32::from(MODE_DIR),
                    ..fuse_attr::default()
                },
                attr_expiry: Instant::now(),
                nlookup: 0,
                forget_pending: false,
                open_fd_count: 0,
                pin_count: 0,
                lru_timestamp: 0,
                io_lock: Rc::new(RwLock::new(())),
            },
        );

        Self {
            fuse: fuse_session,
            state: RefCell::new(VfsState {
                inodes,
                dcache: BTreeMap::new(),
                dentry_index: BTreeMap::new(),
                handles: HandleMap::new(),
                forget_queue: VecDeque::with_capacity(FORGET_QUEUE_MIN_CAP),
                lru_clock: 0,
                lru_evictables: BTreeSet::new(),
                lru_full_warned: false,
                evicted_names: VecDeque::new(),
            }),
        }
    }

    fn pin(&self, nodeid: NodeId) -> Result<InodePin<'_, T>> {
        self.state.borrow_mut().pin(nodeid)?;
        Ok(InodePin { vfs: self, nodeid })
    }

    fn io_lock(&self, nodeid: NodeId) -> Result<Rc<RwLock<()>>> {
        let g = self.state.borrow();
        Ok(g.inodes
            .get(&nodeid)
            .ok_or(Error::new(ESTALE))?
            .io_lock
            .clone())
    }

    async fn rollback_entry(&self, parent: NodeId, name: CString, is_dir: bool, caller: Caller) {
        let opcode = if is_dir { "FUSE_RMDIR" } else { "FUSE_UNLINK" };
        if let Err(err) = self.fuse.rm(parent, name, is_dir, caller).await {
            warn!("virtio-fsd: {opcode} failed during rollback: {err}");
        }
    }

    pub async fn lookup(
        &self,
        parent: NodeId,
        name: &str,
        caller: Caller,
    ) -> Result<(NodeId, u16)> {
        if name.is_empty() {
            return Err(Error::new(ENOENT));
        }

        let _parent_pin = self.pin(parent)?;

        // `cached_child` guarantees the inode exists; `None` mode means its
        // attrs expired and need a GETATTR refresh.
        let cached = {
            let mut g = self.state.borrow_mut();
            g.cached_child(parent, name).map(|nodeid| {
                let mode = g
                    .inodes
                    .get(&nodeid)
                    .filter(|inode| inode.attr_valid())
                    .map(Inode::mode_type);
                if mode.is_some() {
                    g.touch(nodeid);
                }
                (nodeid, mode)
            })
        };

        match cached {
            Some((nodeid, Some(mode))) => return Ok((nodeid, mode)),
            Some((nodeid, None)) => match self.pin(nodeid) {
                // The pin keeps the inode alive across the GETATTR, so the nodeid
                // we return cannot have been forgotten meanwhile.
                Ok(_pin) => {
                    let payload = match self.fuse.getattr(nodeid, None, caller).await {
                        Ok(payload) => payload,
                        Err(e) => {
                            let err = e.into_syscall("FUSE_GETATTR");
                            if err.errno == ENOENT {
                                self.state.borrow_mut().drop_dentry(parent, name);
                            }
                            return Err(err);
                        }
                    };

                    let mut g = self.state.borrow_mut();
                    let mode = g.refresh_attr(nodeid, payload);
                    g.touch(nodeid);
                    return Ok((nodeid, mode));
                }
                Err(_) => {
                    self.state.borrow_mut().drop_dentry(parent, name);
                }
            },
            None => {}
        }

        let name_cstring = cstring(name, "lookup: dir name")?;

        let payload = self
            .fuse
            .lookup(parent, name_cstring, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_LOOKUP"))?;
        let nodeid = payload.nodeid;
        let mode = payload.attr.mode as u16 & MODE_TYPE;
        self.state.borrow_mut().merge_entry(parent, name, payload)?;
        let _published = self.pin(nodeid)?;
        self.flush_forgets(caller).await;
        Ok((nodeid, mode))
    }

    pub async fn mkdir(
        &self,
        entry: Entry<'_, T>,
        mode: u32,
        caller: Caller,
    ) -> Result<(NodeId, u16)> {
        if entry.nodeid.is_some() {
            return Err(Error::new(EEXIST));
        }
        let parent = entry.parent;
        let name = entry.name.as_str();

        let name_cstring = cstring(name, "mkdir: dir name")?;

        let payload = self
            .fuse
            .mkdir(parent, name_cstring.clone(), mode, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_MKDIR"))?;

        let nodeid = payload.nodeid;
        let mode = payload.attr.mode as u16 & MODE_TYPE;

        let merge_err = {
            let mut g = self.state.borrow_mut();
            g.invalidate_attr(parent);
            g.merge_entry(parent, name, payload).err()
        };
        if let Some(err) = merge_err {
            self.rollback_entry(parent, name_cstring, true, caller)
                .await;
            return Err(err);
        }

        let _published = self.pin(nodeid)?;
        self.flush_forgets(caller).await;
        Ok((nodeid, mode))
    }

    pub async fn create(
        &self,
        entry: Entry<'_, T>,
        flags: usize,
        mode: u32,
        caller: Caller,
    ) -> Result<(NodeId, u16)> {
        if entry.nodeid.is_some() {
            return Err(Error::new(EEXIST));
        }
        let parent_nodeid = entry.parent;
        let name = entry.name.as_str();

        let fuse_flags = fuse_open_flags(flags, false, true)?;

        let name_cstring = cstring(name, "create: file name")?;

        let payload = self
            .fuse
            .create(
                parent_nodeid,
                name_cstring.clone(),
                fuse_flags,
                mode,
                caller,
            )
            .await
            .map_err(|e| e.into_syscall("FUSE_CREATE"))?;

        let nodeid = payload.entry.nodeid;
        let fh = payload.open.fh;
        let mode = payload.entry.attr.mode as u16 & MODE_TYPE;

        let merge_err = {
            let mut g = self.state.borrow_mut();
            g.invalidate_attr(parent_nodeid);
            g.merge_entry(parent_nodeid, name, payload.entry).err()
        };

        // FUSE_CREATE also opens. The scheme decides the fd after this returns,
        // so drop the host fh here. A later `open` or `insert_stat_only` publishes it.
        if let Err(rel_err) = self.fuse.release(nodeid, fh, false, caller).await {
            warn!("virtio-fsd: FUSE_RELEASE: {rel_err}");
        }

        if let Some(err) = merge_err {
            self.rollback_entry(parent_nodeid, name_cstring, false, caller)
                .await;
            return Err(err);
        }

        let _published = self.pin(nodeid)?;
        self.flush_forgets(caller).await;
        Ok((nodeid, mode))
    }

    pub async fn open(
        &self,
        nodeid: NodeId,
        flags: usize,
        is_dir: bool,
        path: Vec<String>,
        caller: Caller,
    ) -> Result<usize> {
        let _pin = self.pin(nodeid)?;
        let opcode = if is_dir { "FUSE_OPENDIR" } else { "FUSE_OPEN" };
        let mut open_flags = fuse_open_flags(flags, is_dir, false)?;
        let trunc = !is_dir && open_flags & linux_abi::O_TRUNC != 0;
        let atomic_trunc = self.fuse.capable(FUSE_ATOMIC_O_TRUNC);
        if trunc && !atomic_trunc {
            open_flags &= !linux_abi::O_TRUNC;
        }

        let payload = self
            .fuse
            .open(nodeid, open_flags, is_dir, caller)
            .await
            .map_err(|e| e.into_syscall(opcode))?;

        let fh = payload.fh;

        let handle = if is_dir {
            Handle::Directory {
                nodeid,
                fh,
                path: path.join("/"),
                caller,
            }
        } else {
            Handle::File {
                nodeid,
                fh,
                path: path.join("/"),
                caller,
            }
        };

        let inserted = self.state.borrow_mut().insert_open_handle(handle);
        let fd = match inserted {
            Ok(fd) => fd,
            Err(err) => {
                if let Err(rel_err) = self.fuse.release(nodeid, fh, is_dir, caller).await {
                    warn!("virtio-fsd: FUSE_RELEASE: {rel_err}");
                }
                return Err(err);
            }
        };

        if trunc && !atomic_trunc {
            if let Err(err) = self.ftruncate(fd, 0).await {
                self.close(fd).await;
                return Err(err);
            }
        } else if trunc {
            self.state.borrow_mut().invalidate_attr(nodeid);
        }

        Ok(fd)
    }

    pub async fn getattr(&self, fd: usize) -> Result<fuse_attr> {
        let (nodeid, fh, caller) = {
            let g = self.state.borrow();
            if matches!(g.handles.get(fd)?, Handle::LruLog) {
                return Ok(fuse_attr {
                    ino: 0,
                    size: g.eviction_log().len() as u64,
                    mode: u32::from(MODE_FILE) | 0o400,
                    ..fuse_attr::default()
                });
            }
            let handle = g.handles.get(fd)?;
            (handle.nodeid()?, handle.fh(), handle.opener())
        };

        let _pin = self.pin(nodeid)?;

        {
            let mut g = self.state.borrow_mut();
            if let Some(inode) = g.inodes.get(&nodeid)
                && inode.attr_valid()
            {
                let attr = inode.attr;
                g.touch(nodeid);
                return Ok(attr);
            }
        }

        self.getattr_uncached(nodeid, fh, caller).await
    }

    async fn getattr_uncached(
        &self,
        nodeid: NodeId,
        fh: Option<FileHandle>,
        caller: Caller,
    ) -> Result<fuse_attr> {
        let payload = self
            .fuse
            .getattr(nodeid, fh, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_GETATTR"))?;

        let mut g = self.state.borrow_mut();
        if !g.inodes.contains_key(&nodeid) {
            return Err(Error::new(ESTALE));
        }
        g.refresh_attr(nodeid, payload);
        g.touch(nodeid);
        Ok(payload.attr)
    }

    pub async fn statfs(&self, fd: usize) -> Result<fuse_kstatfs> {
        let (nodeid, caller) = {
            let g = self.state.borrow();
            let handle = g.handles.get(fd)?;
            (handle.nodeid()?, handle.opener())
        };
        let _pin = self.pin(nodeid)?;

        Ok(self
            .fuse
            .statfs(nodeid, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_STATFS"))?
            .st)
    }

    pub async fn read(
        &self,
        fd: usize,
        buf: &mut [u8],
        offset: u64,
        fcntl_flags: u32,
    ) -> Result<usize> {
        let can_read = matches!(fcntl_flags as usize & O_ACCMODE, O_RDONLY | O_RDWR);

        let (nodeid, fh, caller) = {
            let g = self.state.borrow();
            if matches!(g.handles.get(fd)?, Handle::LruLog) {
                if !can_read {
                    return Err(Error::new(EBADF));
                }
                let data = g.eviction_log();
                let off = offset as usize;
                if off >= data.len() {
                    return Ok(0);
                }
                let n = (data.len() - off).min(buf.len());
                buf[..n].copy_from_slice(&data.as_bytes()[off..off + n]);
                return Ok(n);
            }
            match g.handles.get(fd)? {
                Handle::File {
                    nodeid, fh, caller, ..
                } => {
                    if !can_read {
                        return Err(Error::new(EBADF));
                    }
                    if buf.is_empty() {
                        return Ok(0);
                    }
                    (*nodeid, *fh, *caller)
                }
                Handle::Symlink { target, .. } => {
                    if !can_read {
                        return Err(Error::new(EBADF));
                    }
                    let data = target.as_bytes();
                    let off = offset as usize;
                    if off >= data.len() {
                        return Ok(0);
                    }
                    let n = (data.len() - off).min(buf.len());
                    buf[..n].copy_from_slice(&data[off..off + n]);
                    return Ok(n);
                }
                Handle::LruLog => unreachable!("handled above"),
                Handle::Directory { .. } => return Err(Error::new(EISDIR)),
                Handle::StatOnly { .. }
                | Handle::SchemeRoot
                | Handle::DebugRoot
                | Handle::SymlinkPreCreate { .. } => {
                    return Err(Error::new(EBADF));
                }
            }
        };

        let _pin = self.pin(nodeid)?;
        let io = self.io_lock(nodeid)?;
        let _io = io.read().await;
        self.fuse
            .read(
                nodeid,
                fh,
                offset,
                buf,
                0,
                fuse_io_flags(fcntl_flags),
                caller,
            )
            .await
            .map_err(|e| e.into_syscall("FUSE_READ"))
    }

    pub async fn write(
        &self,
        fd: usize,
        buf: &[u8],
        offset: u64,
        fcntl_flags: u32,
    ) -> Result<usize> {
        let can_write = matches!(fcntl_flags as usize & O_ACCMODE, O_WRONLY | O_RDWR);

        enum WriteTarget {
            File {
                nodeid: NodeId,
                fh: FileHandle,
            },
            SymLink {
                parent_nodeid: NodeId,
                name: String,
                path: String,
            },
        }

        let (target, caller) = {
            let g = self.state.borrow();
            match g.handles.get(fd)? {
                Handle::File {
                    nodeid, fh, caller, ..
                } => (
                    WriteTarget::File {
                        nodeid: *nodeid,
                        fh: *fh,
                    },
                    *caller,
                ),
                Handle::SymlinkPreCreate {
                    parent_nodeid,
                    name,
                    path,
                    caller,
                } => (
                    WriteTarget::SymLink {
                        parent_nodeid: *parent_nodeid,
                        name: name.clone(),
                        path: path.clone(),
                    },
                    *caller,
                ),
                Handle::Directory { .. } => return Err(Error::new(EISDIR)),
                Handle::Symlink { .. }
                | Handle::StatOnly { .. }
                | Handle::SchemeRoot
                | Handle::DebugRoot
                | Handle::LruLog => {
                    return Err(Error::new(EBADF));
                }
            }
        };

        match target {
            WriteTarget::File { nodeid, fh } => {
                if !can_write {
                    return Err(Error::new(EBADF));
                }
                if buf.is_empty() {
                    return Ok(0);
                }

                let _pin = self.pin(nodeid)?;
                let io = self.io_lock(nodeid)?;
                let _io = io.write().await;
                let offset = if fcntl_flags as usize & O_APPEND != 0 {
                    self.getattr_uncached(nodeid, Some(fh), caller).await?.size
                } else {
                    offset
                };
                let payload = self
                    .fuse
                    .write(
                        nodeid,
                        fh,
                        offset,
                        buf,
                        0,
                        fuse_io_flags(fcntl_flags),
                        caller,
                    )
                    .await
                    .map_err(|e| e.into_syscall("FUSE_WRITE"))?;
                self.state.borrow_mut().invalidate_attr(nodeid);
                Ok(payload.size as usize)
            }
            WriteTarget::SymLink {
                parent_nodeid,
                name,
                path,
            } => {
                if !can_write {
                    return Err(Error::new(EBADF));
                }
                if offset != 0 {
                    return Err(Error::new(EINVAL));
                }

                let name_cstring = cstring(name.as_str(), "write symlink: file name")?;

                let target = std::str::from_utf8(buf)
                    .map_err(|e| {
                        warn!("virtio-fsd: write symlink: target is not valid utf8: {e:?}");
                        Error::new(EINVAL)
                    })?
                    .to_owned();
                let target_cstring = cstring(target.as_str(), "write symlink: target")?;

                let _parent_pin = self.pin(parent_nodeid)?;

                let payload = self
                    .fuse
                    .symlink(parent_nodeid, &name_cstring, &target_cstring, caller)
                    .await
                    .map_err(|e| e.into_syscall("FUSE_SYMLINK"))?;

                let nodeid = payload.nodeid;
                let merged = {
                    let mut g = self.state.borrow_mut();
                    if let Err(err) = g.merge_entry(parent_nodeid, &name, payload) {
                        Err(err)
                    } else {
                        g.invalidate_attr(parent_nodeid);
                        g.commit_symlink(fd, parent_nodeid, nodeid, path, target, caller)
                            .inspect_err(|_| {
                                g.unmerge_entry(parent_nodeid, &name, nodeid);
                            })
                    }
                };
                if let Err(err) = merged {
                    self.rollback_entry(parent_nodeid, name_cstring, false, caller)
                        .await;
                    return Err(err);
                }
                let _published = self.pin(nodeid)?;
                self.flush_forgets(caller).await;
                Ok(buf.len())
            }
        }
    }

    pub async fn fsync(&self, fd: usize) -> Result<()> {
        let (nodeid, fh, is_dir, caller) = {
            let g = self.state.borrow();
            match g.handles.get(fd)? {
                Handle::File {
                    nodeid, fh, caller, ..
                } => (*nodeid, *fh, false, *caller),
                Handle::Directory {
                    nodeid, fh, caller, ..
                } => (*nodeid, *fh, true, *caller),
                _ => return Err(Error::new(EBADF)),
            }
        };

        let _pin = self.pin(nodeid)?;
        let opcode = if is_dir {
            "FUSE_FSYNCDIR"
        } else {
            "FUSE_FSYNC"
        };

        self.fuse
            .fsync(nodeid, fh, is_dir, caller)
            .await
            .map_err(|e| e.into_syscall(opcode))
    }

    async fn setattr(
        &self,
        nodeid: NodeId,
        fuse_in: fuse_setattr_in,
        caller: Caller,
    ) -> Result<()> {
        let _pin = self.pin(nodeid)?;
        let payload = self
            .fuse
            .setattr(nodeid, fuse_in, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_SETATTR"))?;
        let mut g = self.state.borrow_mut();
        g.refresh_attr(nodeid, payload);
        g.touch(nodeid);
        Ok(())
    }

    pub async fn ftruncate(&self, fd: usize, len: u64) -> Result<()> {
        let (nodeid, fh, caller) = {
            let g = self.state.borrow();
            match g.handles.get(fd)? {
                Handle::File {
                    nodeid, fh, caller, ..
                } => (*nodeid, *fh, *caller),
                Handle::Directory { .. } => return Err(Error::new(EISDIR)),
                _ => return Err(Error::new(EBADF)),
            }
        };

        let mut fuse_in = fuse_setattr_in {
            size: len,
            ..Default::default()
        };
        fuse_in.valid = FATTR_SIZE;
        fuse_in.valid |= FATTR_FH;
        fuse_in.fh = fh.into();

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn futimens(&self, fd: usize, times: &[TimeSpec], caller: Caller) -> Result<()> {
        let atime = times.first().ok_or(Error::new(EINVAL))?;
        let mtime = times.get(1).ok_or(Error::new(EINVAL))?;

        let (nodeid, fh) = {
            let g = self.state.borrow();
            let handle = g.handles.get(fd)?;
            (handle.nodeid()?, handle.fh())
        };

        let mut fuse_in = fuse_setattr_in::default();
        apply_timespec(&mut fuse_in, atime, true)?;
        apply_timespec(&mut fuse_in, mtime, false)?;
        if fuse_in.valid == 0 {
            return Ok(());
        }
        if let Some(fh) = fh {
            fuse_in.valid |= FATTR_FH;
            fuse_in.fh = fh.into();
        }

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn fchmod(&self, fd: usize, new_mode: u16, caller: Caller) -> Result<()> {
        let (nodeid, fh, type_bits) = {
            let g = self.state.borrow();
            let handle = g.handles.get(fd)?;
            let (nodeid, fh) = (handle.nodeid()?, handle.fh());
            let type_bits = g
                .inodes
                .get(&nodeid)
                .map(|i| i.attr.mode & u32::from(MODE_TYPE))
                .unwrap_or(0);
            (nodeid, fh, type_bits)
        };

        let mut fuse_in = fuse_setattr_in {
            mode: (u32::from(new_mode) & 0o7777) | type_bits,
            ..Default::default()
        };
        fuse_in.valid = FATTR_MODE;
        if let Some(fh) = fh {
            fuse_in.valid |= FATTR_FH;
            fuse_in.fh = fh.into();
        }

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn fchown(&self, fd: usize, uid: u32, gid: u32, caller: Caller) -> Result<()> {
        let (nodeid, fh) = {
            let g = self.state.borrow();
            let handle = g.handles.get(fd)?;
            (handle.nodeid()?, handle.fh())
        };

        let mut fuse_in = fuse_setattr_in::default();
        // POSIX: uid/gid -1 (here u32::MAX) means "leave this id alone".
        if uid != u32::MAX {
            fuse_in.valid |= FATTR_UID;
            fuse_in.uid = uid;
        }
        if gid != u32::MAX {
            fuse_in.valid |= FATTR_GID;
            fuse_in.gid = gid;
        }
        if fuse_in.valid == 0 {
            return Ok(());
        }
        if let Some(fh) = fh {
            fuse_in.valid |= FATTR_FH;
            fuse_in.fh = fh.into();
        }

        self.setattr(nodeid, fuse_in, caller).await
    }

    pub async fn close(&self, fd: usize) {
        let handle = self.state.borrow_mut().handles.remove(fd);
        match handle {
            Some(handle) => match handle {
                Handle::File {
                    nodeid,
                    fh,
                    caller: opener,
                    ..
                } => {
                    if let Err(err) = self.fuse.flush(nodeid, fh, opener).await {
                        warn!("virtio-fsd: FUSE_FLUSH: {err}");
                    }
                    if let Err(err) = self.fuse.release(nodeid, fh, false, opener).await {
                        warn!("virtio-fsd: FUSE_RELEASE: {err}");
                    }
                    self.state.borrow_mut().decrement_fd_count(nodeid);
                }
                Handle::Directory {
                    nodeid,
                    fh,
                    caller: opener,
                    ..
                } => {
                    if let Err(err) = self.fuse.release(nodeid, fh, true, opener).await {
                        warn!("virtio-fsd: FUSE_RELEASE: {err}");
                    }
                    self.state.borrow_mut().decrement_fd_count(nodeid);
                }
                Handle::StatOnly { nodeid, .. } | Handle::Symlink { nodeid, .. } => {
                    self.state.borrow_mut().decrement_fd_count(nodeid);
                }
                Handle::SchemeRoot | Handle::DebugRoot | Handle::LruLog => {}
                Handle::SymlinkPreCreate { parent_nodeid, .. } => {
                    self.state.borrow_mut().decrement_fd_count(parent_nodeid);
                }
            },
            None => {
                warn!("virtio-fsd: close: fd '{fd}' was not found in handles");
            }
        };

        self.flush_forgets(Caller::NONE).await;
    }

    pub async fn shutdown(&self) {
        let ids: Vec<usize> = self.state.borrow().handles.keys().copied().collect();
        for id in ids {
            self.close(id).await;
        }

        {
            let mut g = self.state.borrow_mut();
            let extra: Vec<(NodeId, u64)> = g
                .inodes
                .iter()
                .filter(|(nodeid, inode)| **nodeid != NodeId::ROOT && inode.nlookup > 0)
                .map(|(nodeid, inode)| (*nodeid, inode.nlookup))
                .collect();
            g.forget_queue.extend(extra);
        }
        self.flush_forgets(Caller::NONE).await;
    }

    pub fn scheme_root(&self) -> usize {
        self.state.borrow_mut().handles.insert(Handle::SchemeRoot)
    }

    /// Root handle of the debug scheme (`virtio-fs-debug.<tag>`).
    pub fn debug_root(&self) -> usize {
        self.state.borrow_mut().handles.insert(Handle::DebugRoot)
    }

    pub fn is_debug(&self, fd: usize) -> bool {
        matches!(
            self.state.borrow().handles.get(fd),
            Ok(Handle::DebugRoot | Handle::LruLog)
        )
    }

    /// Open a node of the debug scheme. `fd` must be a debug scheme handle.
    ///
    /// Only the root has children, and the only child is `lru_evicted`. These
    /// names are not FUSE inodes, so nothing here reaches the host.
    /// `O_TRUNC` clears the in-memory eviction log.
    pub fn open_debug(&self, fd: usize, path: &str, flags: usize) -> Result<usize> {
        let mut g = self.state.borrow_mut();
        match g.handles.get(fd)? {
            Handle::DebugRoot => {}
            Handle::LruLog => return Err(Error::new(ENOTDIR)),
            _ => return Err(Error::new(EBADF)),
        }
        if path_components::<&str>(path).as_slice() != [LRU_DEBUG_EVICTED_NAME] {
            return Err(Error::new(ENOENT));
        }
        if flags & syscall::O_TRUNC != 0 {
            g.evicted_names.clear();
        }
        Ok(g.handles.insert(Handle::LruLog))
    }

    pub(crate) async fn follow_chain(
        &self,
        mut walk: WalkResult,
        caller: Caller,
    ) -> Result<WalkResult> {
        let mut hops = 0u32;
        while walk.node_mode == MODE_SYMLINK {
            hops += 1;
            if hops > SYMLOOP_MAX {
                return Err(Error::new(ELOOP));
            }
            let link_path = self.readlink(walk.nodeid, caller).await?;
            walk = self.walk_link(walk, &link_path, caller).await?;
        }
        Ok(walk)
    }

    pub async fn walk_path(
        &self,
        fd: usize,
        path: &str,
        open_flags: usize,
        caller: Caller,
    ) -> Result<WalkResult> {
        // Debug scheme handles have no FUSE path. Reject them before the absolute
        // path shortcut below, which never looks at `fd`.
        if self.is_debug(fd) {
            return Err(Error::new(EPERM));
        }

        let (nodeid, node_mode, rel_path) = if path.starts_with('/') {
            (NodeId::ROOT, MODE_DIR, Vec::new())
        } else {
            let g = self.state.borrow();
            match g.handles.get(fd)? {
                Handle::File { .. } | Handle::Symlink { .. } | Handle::SymlinkPreCreate { .. } => {
                    Err(Error::new(ENOTDIR))
                }
                // Rejected above.
                Handle::DebugRoot | Handle::LruLog => Err(Error::new(EPERM)),
                Handle::Directory { nodeid, path, .. } => {
                    let mode = g.inode_mode(*nodeid)?;
                    Ok((*nodeid, mode, path_components(path)))
                }
                Handle::StatOnly { nodeid, path, .. } => {
                    let mode = g.inode_mode(*nodeid)?;
                    if mode != MODE_DIR {
                        return Err(Error::new(ENOTDIR));
                    }
                    Ok((*nodeid, mode, path_components(path)))
                }
                Handle::SchemeRoot => Ok((NodeId::ROOT, MODE_DIR, Vec::new())),
            }?
        };

        let mut walk = WalkResult {
            nodeid,
            node_mode,
            parent_nodeid: None,
            rel_path,
            last_to_be_created: false,
        };

        let mut _cur_pin = self.pin(walk.nodeid)?;
        let path_parts: Vec<&str> = path_components(path);

        if path.is_empty() || path == "/" {
            return Ok(walk);
        }

        for (i, name) in path_parts.iter().copied().enumerate() {
            let is_last_element = i == path_parts.len() - 1;

            if walk.node_mode != MODE_DIR {
                return Err(Error::new(ENOTDIR));
            }

            if name.is_empty() || name == "." {
                walk.parent_nodeid = None;
                continue;
            }

            if name == ".." {
                (walk.nodeid, walk.node_mode) = self.state.borrow_mut().parent_of(walk.nodeid)?;
                _cur_pin = self.pin(walk.nodeid)?;
                walk.rel_path.pop();
                walk.parent_nodeid = None;
                continue;
            }

            walk.rel_path.push(name.to_owned());

            let parent = walk.nodeid;
            walk.parent_nodeid = Some(parent);
            match self.lookup(parent, name, caller).await {
                Ok((id, mode)) => {
                    walk.nodeid = id;
                    walk.node_mode = mode;
                    _cur_pin = self.pin(walk.nodeid)?;
                }
                Err(err) => {
                    if is_last_element
                        && err.errno == ENOENT
                        && (open_flags & syscall::O_CREAT != 0)
                    {
                        walk.last_to_be_created = true;
                        break;
                    }
                    return Err(err);
                }
            }

            if !is_last_element && walk.node_mode == MODE_SYMLINK {
                walk = self.follow_chain(walk, caller).await?;
                _cur_pin = self.pin(walk.nodeid)?;
                if walk.node_mode != MODE_DIR {
                    return Err(Error::new(ENOTDIR));
                }
            }
        }

        Ok(walk)
    }

    pub async fn walk_link(
        &self,
        mut walk: WalkResult,
        path: &str,
        caller: Caller,
    ) -> Result<WalkResult> {
        if path.starts_with('/') {
            walk.nodeid = NodeId::ROOT;
            walk.node_mode = MODE_DIR;
            walk.parent_nodeid = Some(NodeId::ROOT);
            walk.rel_path.clear();
        } else {
            // relative targets are relative to the directory that contains the link
            walk.rel_path.pop();
            if let Some(parent) = walk.parent_nodeid {
                walk.nodeid = parent;
                walk.node_mode = MODE_DIR;
            } else {
                (walk.nodeid, walk.node_mode) = self.state.borrow_mut().parent_of(walk.nodeid)?;
            }
        }

        let mut _cur_pin = self.pin(walk.nodeid)?;
        let path_parts: Vec<&str> = path_components(path);

        if path.is_empty() || path == "/" {
            return Ok(walk);
        }

        for name in path_parts.iter().copied() {
            if walk.node_mode != MODE_DIR {
                return Err(Error::new(ENOTDIR));
            }

            if name.is_empty() || name == "." {
                walk.parent_nodeid = None;
                continue;
            }

            if name == ".." {
                (walk.nodeid, walk.node_mode) = self.state.borrow_mut().parent_of(walk.nodeid)?;
                _cur_pin = self.pin(walk.nodeid)?;
                walk.rel_path.pop();
                walk.parent_nodeid = None;
                continue;
            }

            walk.rel_path.push(name.to_owned());

            let parent = walk.nodeid;
            walk.parent_nodeid = Some(parent);
            match self.lookup(parent, name, caller).await {
                Ok((id, mode)) => {
                    walk.nodeid = id;
                    walk.node_mode = mode;
                    _cur_pin = self.pin(walk.nodeid)?;
                }
                Err(err) => return Err(err),
            }
        }

        Ok(walk)
    }

    pub async fn readdir(&self, fd: usize, offset: u64) -> Result<FuseReaddirOut> {
        let (nodeid, fh, caller) = {
            let g = self.state.borrow();
            match g.handles.get(fd)? {
                Handle::Directory {
                    nodeid, fh, caller, ..
                } => (*nodeid, *fh, *caller),
                Handle::File { .. } => return Err(Error::new(ENOTDIR)),
                _ => {
                    warn!("virtio-fsd: readdir: invalid fd '{fd}'");
                    return Err(Error::new(EBADF));
                }
            }
        };

        let _pin = self.pin(nodeid)?;
        self.fuse
            .readdir(nodeid, fh, offset, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_READDIR"))
    }

    /// Resolves `path` against `dirfd` to a name in a directory. A symlink in the last
    /// component is not followed, and the name does not have to exist.
    pub async fn entry_at(&self, dirfd: usize, path: &str, caller: Caller) -> Result<Entry<'_, T>> {
        let walk = self.walk_path(dirfd, path, O_CREAT, caller).await?;
        self.entry_of_walk(&walk)
    }

    /// The name `walk` ended on. `EINVAL` if it did not end on one: an empty path, `/`,
    /// or a last component of `.` or `..`, which reach a directory through another name.
    pub fn entry_of_walk(&self, walk: &WalkResult) -> Result<Entry<'_, T>> {
        let parent = walk.parent_nodeid.ok_or(Error::new(EINVAL))?;
        let name = walk.rel_path.last().cloned().ok_or(Error::new(EINVAL))?;
        let nodeid = (!walk.last_to_be_created).then_some(walk.nodeid);

        Ok(Entry {
            parent,
            name,
            nodeid,
            path: walk.rel_path.join("/"),
            _parent_pin: self.pin(parent)?,
            node_pin: nodeid.and_then(|id| self.pin(id).ok()),
        })
    }

    /// The name `fd` was opened by. Best effort: an fd names an inode, so this goes by the
    /// handle's path, which can be stale and, for a hard link, is only one of its names.
    pub async fn entry_of_fd(&self, fd: usize, caller: Caller) -> Result<Entry<'_, T>> {
        let (nodeid, path) = {
            let g = self.state.borrow();
            match g.handles.get(fd)? {
                Handle::File { nodeid, path, .. }
                | Handle::Directory { nodeid, path, .. }
                | Handle::StatOnly { nodeid, path, .. }
                | Handle::Symlink { nodeid, path, .. } => (*nodeid, path.clone()),
                _ => return Err(Error::new(EBADF)),
            }
        };

        let node_pin = self.pin(nodeid).ok();

        let mut parts: Vec<String> = path_components(&path);
        let name = parts.pop().ok_or(Error::new(EINVAL))?;
        let parent = if parts.is_empty() {
            NodeId::ROOT
        } else {
            let parent_path = format!("/{}", parts.join("/"));
            self.walk_path(fd, &parent_path, 0, caller).await?.nodeid
        };

        Ok(Entry {
            parent,
            name,
            nodeid: Some(nodeid),
            path,
            _parent_pin: self.pin(parent)?,
            node_pin,
        })
    }

    pub async fn rm(&self, entry: Entry<'_, T>, is_dir: bool, caller: Caller) -> Result<()> {
        let Entry {
            parent,
            name,
            nodeid,
            _parent_pin,
            node_pin: target_pin,
            ..
        } = entry;
        let nodeid = nodeid.ok_or(Error::new(ENOENT))?;

        let name_cstring = cstring(&name, "rm: file name")?;

        let opcode = if is_dir { "FUSE_RMDIR" } else { "FUSE_UNLINK" };

        self.fuse
            .rm(parent, name_cstring, is_dir, caller)
            .await
            .map_err(|e| e.into_syscall(opcode))?;

        // Unpin first, so `unlink_name` can drop the inode right away and
        // the FUSE_FORGET goes out with the flush below.
        drop(target_pin);

        {
            let mut g = self.state.borrow_mut();
            g.unlink_name(parent, &name, nodeid);
            g.invalidate_attr(parent);
        }

        self.flush_forgets(caller).await;
        Ok(())
    }

    pub async fn rename(
        &self,
        src: Entry<'_, T>,
        dest: Entry<'_, T>,
        flags: u32,
        caller: Caller,
    ) -> Result<()> {
        let Entry {
            parent: old_parent,
            name: old_name,
            nodeid: src_nodeid,
            path: src_path,
            _parent_pin: _old_parent_pin,
            node_pin: _src_pin,
        } = src;
        let Entry {
            parent: new_parent,
            name: new_name,
            nodeid: dest_nodeid,
            path: new_path,
            _parent_pin: _new_parent_pin,
            node_pin: dest_pin,
        } = dest;

        let src_nodeid = src_nodeid.ok_or(Error::new(ENOENT))?;
        // TODO: send FUSE_RENAME2 with the flag, so the check is atomic on the host.
        if flags & RENAME_NOREPLACE as u32 != 0 && dest_nodeid.is_some() {
            return Err(Error::new(EEXIST));
        }

        let old_cstring = cstring(old_name.as_str(), "rename: old name")?;
        let new_cstring = cstring(new_name.as_str(), "rename: new name")?;

        self.fuse
            .rename(old_parent, old_cstring, new_parent, new_cstring, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_RENAME"))?;

        // Unpin first, so `unlink_name` can drop a replaced inode right away
        drop(dest_pin);

        {
            let mut g = self.state.borrow_mut();
            if let Some(dest_nodeid) = dest_nodeid
                && dest_nodeid != src_nodeid
            {
                g.unlink_name(new_parent, &new_name, dest_nodeid);
            }

            if let Some(inode) = g.inodes.get_mut(&src_nodeid) {
                inode.parent = new_parent;
            }

            let expiry = g
                .drop_dentry(old_parent, &old_name)
                .map(|d| d.entry_expiry)
                .unwrap_or_else(Instant::now);
            g.insert_dentry(new_parent, &new_name, src_nodeid, expiry);
            g.invalidate_attr(old_parent);
            g.invalidate_attr(new_parent);
            g.invalidate_attr(src_nodeid);

            for handle in g.handles.values_mut() {
                let path = match handle {
                    Handle::File { path, .. }
                    | Handle::Directory { path, .. }
                    | Handle::StatOnly { path, .. }
                    | Handle::Symlink { path, .. }
                    | Handle::SymlinkPreCreate { path, .. } => path,
                    Handle::SchemeRoot | Handle::DebugRoot | Handle::LruLog => continue,
                };
                if path == &src_path {
                    *path = new_path.clone();
                } else if let Some(rest) = path.strip_prefix(&src_path)
                    && rest.starts_with('/')
                {
                    *path = format!("{new_path}{rest}");
                }
            }
        }

        self.flush_forgets(caller).await;
        Ok(())
    }

    pub async fn link(&self, fd: usize, dest: Entry<'_, T>, caller: Caller) -> Result<()> {
        let src_nodeid = {
            let g = self.state.borrow();
            match g.handles.get(fd)? {
                Handle::Directory { .. } => return Err(Error::new(EPERM)),
                Handle::File { nodeid, .. } | Handle::Symlink { nodeid, .. } => *nodeid,
                Handle::StatOnly { nodeid, .. } => {
                    if g.inode_mode(*nodeid)? == MODE_DIR {
                        return Err(Error::new(EPERM));
                    }
                    *nodeid
                }
                _ => return Err(Error::new(EBADF)),
            }
        };

        let _src_pin = self.pin(src_nodeid)?;

        if dest.nodeid.is_some() {
            return Err(Error::new(EEXIST));
        }
        let new_parent = dest.parent;
        let new_name = dest.name.as_str();

        let name_cstring = cstring(new_name, "link: new name")?;

        let entry = self
            .fuse
            .link(new_parent, src_nodeid, name_cstring.clone(), caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_LINK"))?;

        let merge_err = {
            let mut g = self.state.borrow_mut();
            let err = g.merge_entry(new_parent, new_name, entry).err();
            if err.is_none() {
                g.invalidate_attr(new_parent);
            }
            err
        };
        if let Some(err) = merge_err {
            self.rollback_entry(new_parent, name_cstring, false, caller)
                .await;
            return Err(err);
        }
        self.flush_forgets(caller).await;
        Ok(())
    }

    pub async fn readlink(&self, nodeid: NodeId, caller: Caller) -> Result<String> {
        let _pin = self.pin(nodeid)?;
        self.fuse
            .readlink(nodeid, caller)
            .await
            .map_err(|e| e.into_syscall("FUSE_READLINK"))
    }

    pub fn has_fd(&self, fd: usize) -> Result<()> {
        self.state.borrow().handles.get(fd).map(|_| ())
    }

    pub fn get_inode(&self, fd: usize) -> Result<NodeId> {
        self.state.borrow().handles.get(fd)?.nodeid()
    }

    pub fn get_path(&self, fd: usize) -> Result<String> {
        let g = self.state.borrow();
        match g.handles.get(fd)? {
            Handle::File { path, .. }
            | Handle::Directory { path, .. }
            | Handle::StatOnly { path, .. }
            | Handle::Symlink { path, .. }
            | Handle::SymlinkPreCreate { path, .. } => Ok(path.clone()),
            Handle::SchemeRoot | Handle::DebugRoot => Ok(String::new()),
            Handle::LruLog => Ok(LRU_DEBUG_EVICTED_NAME.to_owned()),
        }
    }

    pub fn insert_stat_only(
        &self,
        nodeid: NodeId,
        path: Vec<String>,
        caller: Caller,
    ) -> Result<usize> {
        self.state
            .borrow_mut()
            .insert_open_handle(Handle::StatOnly {
                nodeid,
                path: path.join("/"),
                caller,
            })
    }

    pub fn insert_symlink(
        &self,
        nodeid: NodeId,
        path: Vec<String>,
        target: String,
        caller: Caller,
    ) -> Result<usize> {
        self.state.borrow_mut().insert_open_handle(Handle::Symlink {
            nodeid,
            path: path.join("/"),
            target,
            caller,
        })
    }

    /// The link is created by the write of its target. Until then the handle holds the
    /// parent like an open fd, so it cannot be evicted between the two calls.
    pub fn pre_create_symlink(&self, entry: Entry<'_, T>, caller: Caller) -> Result<usize> {
        if entry.nodeid.is_some() {
            return Err(Error::new(EEXIST));
        }

        let mut g = self.state.borrow_mut();
        g.increment_fd_count(entry.parent)?;

        Ok(g.handles.insert(Handle::SymlinkPreCreate {
            parent_nodeid: entry.parent,
            name: entry.name,
            path: entry.path,
            caller,
        }))
    }
}

// Helpers

fn cstring(name: &str, what: &str) -> Result<CString> {
    CString::new(name).map_err(|_| {
        warn!("virtio-fsd: {what} contained an internal 0 byte.");
        Error::new(EINVAL)
    })
}

fn path_components<'a, T: From<&'a str>>(path: &'a str) -> Vec<T> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(T::from)
        .collect()
}

/// `flags` for `FUSE_OPEN` / `FUSE_CREATE`. `O_APPEND` is left out: FUSE has
/// no opcode to change flags later, so the host fd would keep appending even
/// after `F_SETFL` cleared it. We emulate append ourselves on each write.
fn fuse_open_flags(redox_flags: usize, is_dir: bool, for_create: bool) -> Result<u32> {
    let accmode = redox_flags & O_ACCMODE;

    if is_dir && (accmode == O_WRONLY || accmode == O_RDWR) {
        return Err(Error::new(EISDIR));
    }
    if redox_flags & O_DIRECTORY != 0 && !is_dir {
        return Err(Error::new(ENOTDIR));
    }

    let mut linux_flags: u32 = match accmode {
        O_RDWR => linux_abi::O_RDWR,
        O_WRONLY => linux_abi::O_WRONLY,
        _ => linux_abi::O_RDONLY,
    };

    if !is_dir {
        if redox_flags & O_CREAT != 0 {
            linux_flags |= linux_abi::O_CREAT;
        }
        if redox_flags & O_EXCL != 0 {
            linux_flags |= linux_abi::O_EXCL;
        }
        if redox_flags & O_TRUNC != 0 {
            linux_flags |= linux_abi::O_TRUNC;
        }
        if !for_create {
            linux_flags &= !(linux_abi::O_CREAT | linux_abi::O_EXCL);
        }
    } else if redox_flags & O_DIRECTORY != 0 {
        linux_flags |= linux_abi::O_DIRECTORY;
    }

    Ok(linux_flags)
}

/// `flags` for `FUSE_READ` / `FUSE_WRITE`. These requests carry the current
/// open-file flags, so access mode shows up again, along with `O_APPEND`
/// and `O_NONBLOCK` which `F_SETFL` can change after open. The kernel keeps
/// those bits and passes them here as `fcntl_flags`.
fn fuse_io_flags(fcntl_flags: u32) -> u32 {
    let flags = fcntl_flags as usize;
    let mut linux = match flags & O_ACCMODE {
        O_RDWR => linux_abi::O_RDWR,
        O_WRONLY => linux_abi::O_WRONLY,
        _ => linux_abi::O_RDONLY,
    };
    if flags & O_NONBLOCK != 0 {
        linux |= linux_abi::O_NONBLOCK;
    }
    if flags & O_APPEND != 0 {
        linux |= linux_abi::O_APPEND;
    }
    linux
}

fn apply_timespec(fuse_in: &mut fuse_setattr_in, ts: &TimeSpec, atime: bool) -> Result<()> {
    match ts.tv_nsec {
        UTIME_OMIT => Ok(()),
        UTIME_NOW => {
            fuse_in.valid |= if atime {
                FATTR_ATIME | FATTR_ATIME_NOW
            } else {
                FATTR_MTIME | FATTR_MTIME_NOW
            };
            Ok(())
        }
        nsec if nsec >= 0 && nsec < 1_000_000_000 && ts.tv_sec >= 0 => {
            if atime {
                fuse_in.valid |= FATTR_ATIME;
                fuse_in.atime = ts.tv_sec as u64;
                fuse_in.atimensec = nsec as u32;
            } else {
                fuse_in.valid |= FATTR_MTIME;
                fuse_in.mtime = ts.tv_sec as u64;
                fuse_in.mtimensec = nsec as u32;
            }
            Ok(())
        }
        _ => Err(Error::new(EINVAL)),
    }
}

/// Cache expiry from a FUSE `*_valid`/`*_valid_nsec` pair. Clamps nsec to
/// `0..=999_999_999` like the kernel does (no carry into seconds, so
/// `Duration::new` can't panic). Saturates to a large TTL if `sec` is
/// huge (e.g. `u64::MAX` for "forever"); if that still overflows, falls
/// back to `now` (zero TTL, not infinite, not true saturation, but safe).
fn fuse_expiry(now: Instant, sec: u64, nsec: u32) -> Instant {
    const NS_PER_SEC: u32 = 1_000_000_000;
    let ttl = Duration::new(sec, nsec.min(NS_PER_SEC - 1));
    now.checked_add(ttl)
        .or_else(|| now.checked_add(Duration::from_secs(u64::from(u32::MAX))))
        .unwrap_or(now)
}

// `pin_count`, `open_fd_count`, and `nlookup` must be exact: they decide eviction and what
// `FUSE_FORGET` reports. Overflow or underflow here is a bug, so we crash at the call.
#[track_caller]
fn count_inc(count: u64, what: &str, nodeid: NodeId) -> u64 {
    match count.checked_add(1) {
        Some(n) => n,
        None => panic!("virtio-fsd: {what} overflow for nodeid={nodeid}"),
    }
}
#[track_caller]
fn count_dec(count: u64, what: &str, nodeid: NodeId) -> u64 {
    match count.checked_sub(1) {
        Some(n) => n,
        None => panic!("virtio-fsd: {what} underflow for nodeid={nodeid}"),
    }
}

#[cfg(test)]
#[path = "vfs_tests.rs"]
mod vfs_tests;
