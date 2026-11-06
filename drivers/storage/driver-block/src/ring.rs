use std::cmp;
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fmt::Write;
use std::io::{self, Read, Seek, SeekFrom};
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::sync::{Mutex, RwLock};

use common::dma::Dma;
use event::EventFlags;
use executor::{yield_now, Hardware, JoinHandle, LocalExecutor, WorkQueue};
use libredox::{flag, Fd};
use partitionlib::LogicalBlockSize;
use redox_rings::op::{
    DiskOpCqe, DiskOpKind, DiskOpSqe, RingCallVerb, RingSetupFlags, RingSetupParams,
    RING_MAX_CQ_ENTRIES, RING_MAX_SQ_ENTRIES,
};
use redox_rings::raw::RingPushError;
use redox_rings::sync::{BlockingConsumer, BlockingProducer, FutexWaitResult, WaitNotifyAsync};
use redox_scheme::scheme::{register_scheme_inner, SchemeAsync, SchemeState};
use redox_scheme::{
    CallerCtx, OpenResult, RecvFdRequest, RequestKind, Response, SignalBehavior, Socket,
};
use scheme_utils::HandleMap;
use syscall::dirent::DirentBuf;
use syscall::schemev2::NewFdFlags;
use syscall::{
    Error, FmoveFdFlags, Result, Stat, TimeSpec, EACCES, EAGAIN, EBADF, EINTR, EINVAL, EISDIR,
    ENOENT, ENOLCK, EOPNOTSUPP, EOVERFLOW, EWOULDBLOCK, MODE_DIR, MODE_FILE, O_DIRECTORY, O_STAT,
};
use zerocopy::TryFromBytes;

use super::Disk;
use crate::{EventSource, PhysAddr};

struct RingEventSource<'a, Ev: EventSource>(Mutex<Pin<&'a mut Ev>>);

impl<'a, Ev: EventSource> WaitNotifyAsync for RingEventSource<'a, Ev> {
    async fn wait_on_tail(
        &self,
        _expected_tail: u32,
        _deadline_opt: Option<&TimeSpec>,
    ) -> FutexWaitResult {
        self.0.lock().unwrap().as_mut().next().await;
        FutexWaitResult::Waited
    }

    fn notify_on_tail(&self) {
        unimplemented!("notify_on_tail is not implemented for RingEventSource")
    }

    async fn wait_on_head(
        &self,
        _expected_head: u32,
        _deadline_opt: Option<&TimeSpec>,
    ) -> FutexWaitResult {
        unimplemented!("wait_on_head is not implemented for RingEventSource")
    }

    fn notify_on_head(&self) {
        unimplemented!("notify_on_head is not implemented for RingEventSource")
    }
}

#[derive(Clone)]
struct PartitionTable {
    partitions: Vec<Rc<partitionlib::Partition>>,
    kind: partitionlib::PartitionTableKind,
}

#[derive(Clone)]
pub struct RingDiskWrapper<T> {
    pub disk: T,
    pub pt: Option<PartitionTable>,
}

impl<T: Disk> RingDiskWrapper<T> {
    fn pt<Hw: Hardware>(disk: &mut T, executor: &Rc<LocalExecutor<Hw>>) -> Option<PartitionTable> {
        use super::ExecutorTrait;
        let bs = match disk.block_size() {
            512 => LogicalBlockSize::Lb512,
            4096 => LogicalBlockSize::Lb4096,
            _ => return None,
        };
        struct Device<'a, D: Disk, E: ExecutorTrait> {
            disk: &'a mut D,
            executor: &'a E,
            offset: u64,
        }

        impl<'a, D: Disk, E: ExecutorTrait> Seek for Device<'a, D, E> {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                let size = i64::try_from(self.disk.size()).or(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Disk larger than 2^63 - 1 bytes",
                )))?;

                self.offset = match from {
                    SeekFrom::Start(new_pos) => cmp::min(self.disk.size(), new_pos),
                    SeekFrom::Current(new_pos) => {
                        cmp::max(0, cmp::min(size, self.offset as i64 + new_pos)) as u64
                    }
                    SeekFrom::End(new_pos) => cmp::max(0, cmp::min(size + new_pos, size)) as u64,
                };

                Ok(self.offset)
            }
        }
        // TODO: Perhaps this impl should be used in the rest of the scheme.
        impl<'a, D: Disk, E: ExecutorTrait> Read for Device<'a, D, E> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let blksize = self.disk.block_size();
                let size_in_blocks = self.disk.size() / u64::from(blksize);

                let disk = &mut self.disk;

                let read_block = |block: u64, block_bytes: &mut [u8]| {
                    if block >= size_in_blocks {
                        return Err(io::Error::from_raw_os_error(syscall::EOVERFLOW));
                    }

                    let bytes = self.executor.block_on(disk.read(block, block_bytes))?;
                    assert_eq!(bytes, block_bytes.len());
                    Ok(())
                };
                let bytes_read = super::block_read(self.offset, blksize, buf, read_block)?;

                self.offset += bytes_read as u64;
                Ok(bytes_read)
            }
        }

        let table = partitionlib::get_partitions(
            &mut Device {
                disk,
                offset: 0,
                executor,
            },
            bs,
        )
        .ok()
        .flatten()?;

        Some(PartitionTable {
            partitions: table.partitions.into_iter().map(Rc::new).collect(),
            kind: table.kind,
        })
    }

    pub fn new<Hw: Hardware>(mut disk: T, executor: &Rc<LocalExecutor<Hw>>) -> Self {
        Self {
            pt: Self::pt(&mut disk, executor),
            disk,
        }
    }

    pub fn disk(&self) -> &T {
        &self.disk
    }

    pub fn disk_mut(&mut self) -> &mut T {
        &mut self.disk
    }

    pub fn block_size(&self) -> u32 {
        self.disk.block_size()
    }

    pub fn size(&self) -> u64 {
        self.disk.size()
    }

    pub async fn read_dma(
        &mut self,
        partition: Option<&partitionlib::Partition>,
        start_lba: u64,
        phys_addr: PhysAddr,
        num_sectors: u32,
    ) -> syscall::Result<()> {
        if let Some(partition) = partition {
            let end_lba = start_lba + u64::from(num_sectors);
            if end_lba > partition.size {
                return Err(Error::new(EOVERFLOW));
            }

            let abs_lba = partition.start_lba + start_lba;
            unsafe { self.disk.read_dma(abs_lba, phys_addr, num_sectors) }.await
        } else {
            unsafe { self.disk.read_dma(start_lba, phys_addr, num_sectors) }.await
        }
    }

    pub async fn write_dma(
        &mut self,
        partition: Option<&partitionlib::Partition>,
        start_lba: u64,
        phys_addr: PhysAddr,
        num_sectors: u32,
    ) -> syscall::Result<()> {
        if let Some(partition) = partition {
            let end_lba = start_lba + u64::from(num_sectors);
            if end_lba > partition.size {
                return Err(Error::new(EOVERFLOW));
            }

            let abs_lba = partition.start_lba + start_lba;
            unsafe { self.disk.write_dma(abs_lba, phys_addr, num_sectors) }.await
        } else {
            unsafe { self.disk.write_dma(start_lba, phys_addr, num_sectors) }.await
        }
    }

    pub async fn read(
        &mut self,
        part_num: Option<usize>,
        start_lba: u64,
        buf: &mut [u8],
    ) -> syscall::Result<usize> {
        if buf.len() as u64 % u64::from(self.disk.block_size()) != 0 {
            return Err(Error::new(EINVAL));
        }

        if let Some(part_num) = part_num {
            let part = self
                .pt
                .as_ref()
                .ok_or(syscall::Error::new(EBADF))?
                .partitions
                .get(part_num)
                .ok_or(syscall::Error::new(EBADF))?;

            let num_sectors = buf.len() as u32 / self.disk.block_size();
            let end_lba = start_lba + u64::from(num_sectors);
            if end_lba > part.size {
                return Err(syscall::Error::new(EOVERFLOW));
            }

            let abs_block = part.start_lba + start_lba;
            self.disk.read(abs_block, buf).await
        } else {
            self.disk.read(start_lba, buf).await
        }
    }

    pub async fn write(
        &mut self,
        part_num: Option<usize>,
        start_lba: u64,
        buf: &[u8],
    ) -> syscall::Result<usize> {
        if buf.len() as u64 % u64::from(self.disk.block_size()) != 0 {
            return Err(Error::new(EINVAL));
        }

        if let Some(part_num) = part_num {
            let part = self
                .pt
                .as_ref()
                .ok_or(syscall::Error::new(EBADF))?
                .partitions
                .get(part_num)
                .ok_or(syscall::Error::new(EBADF))?;

            let num_sectors = buf.len() as u32 / self.disk.block_size();
            let end_lba = start_lba + u64::from(num_sectors);
            if end_lba > part.size {
                return Err(syscall::Error::new(EOVERFLOW));
            }

            let abs_block = part.start_lba + start_lba;
            self.disk.write(abs_block, buf).await
        } else {
            self.disk.write(start_lba, buf).await
        }
    }
}

struct RingResource<D: Disk> {
    disk: RingDiskWrapper<D>,
    partition: Option<Rc<partitionlib::Partition>>,
}

enum RingState<Hw: Hardware, D: Disk> {
    Inactive,
    Active {
        ring_fds: [usize; 3], // (sq_fd, cq_fd, pipe_fd)
        fixed_ftbl: Rc<RwLock<Vec<RingResource<D>>>>,
        shm: Dma<[u8]>,
        join_handle: JoinHandle<Hw, ()>,
    },
}

enum Handle<Hw: Hardware, D: Disk> {
    List(Vec<u8>), // entries
    Disk { num: u32, pt: Option<usize> },
    Ring(RingState<Hw, D>),
    SchemeRoot,
}

pub struct RingDiskScheme<T: Disk, Hw: Hardware> {
    inner: RingDiskSchemeInner<T, Hw>,
    state: SchemeState,
}

impl<T: Disk + Clone + 'static, Hw: Hardware> RingDiskScheme<T, Hw> {
    pub fn new(
        daemon: Option<daemon::Daemon>,
        scheme_name: String,
        disks: BTreeMap<u32, T>,
        executor: Rc<LocalExecutor<Hw>>,
    ) -> Self {
        assert!(scheme_name.starts_with("disk"));
        let socket = Socket::nonblock().expect("failed to create disk scheme");
        let shm_dir_name = format!("/scheme/shm/{scheme_name}");

        let mut inner = RingDiskSchemeInner {
            scheme_name,
            socket,
            disks: BTreeMap::from_iter(
                disks
                    .into_iter()
                    .map(|(k, disk)| (k, RingDiskWrapper::new(disk, &executor))),
            ),
            handles: HandleMap::new(),
            executor,
            shm_dir: libredox::Fd::open(&shm_dir_name, flag::O_DIRECTORY | flag::O_CLOEXEC, 0)
                .expect("failed to open shm direcotry"),
            pipe_root: libredox::Fd::open("/scheme/pipe/scheme-root", flag::O_CLOEXEC, 0)
                .expect("failed to open pipe root"),
        };

        let cap_id = inner.scheme_root().expect("failed to get this scheme root");
        register_scheme_inner(&inner.socket, &inner.scheme_name, cap_id)
            .expect("failed to register disk scheme root");

        if let Some(daemon) = daemon {
            daemon.ready();
        }

        Self {
            inner,
            state: SchemeState::new(),
        }
    }

    pub fn event_handle(&self) -> &Fd {
        self.inner.socket.inner()
    }

    /// Process pending and new requests.
    ///
    /// This needs to be called each time there is a new event on the scheme.
    pub async fn tick(&mut self) -> io::Result<()> {
        // Handle new scheme requests
        loop {
            let request = match self.inner.socket.next_request(SignalBehavior::Interrupt) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    // Scheme likely got unmounted
                    // TODO: return this to caller instead
                    std::process::exit(0);
                }
                Err(error) if error.errno == EWOULDBLOCK || error.errno == EAGAIN => break,
                Err(err) if err.errno == EINTR => continue,
                Err(err) => return Err(err.into()),
            };

            let response = match request.kind() {
                RequestKind::Call(call_request) => {
                    // TODO: Spawn a separate task for each scheme call. This would however require the
                    // use of a smarter buffer pool (or direct IO, or a buffer per fd) in order to do
                    // parallel IO. It might also require async-aware locks so that a close() is
                    // correctly ordered wrt IO on the same fd.
                    call_request
                        .handle_async(&mut self.inner, &mut self.state)
                        .await
                }
                RequestKind::SendFd(request) => Response::err(EOPNOTSUPP, request),
                RequestKind::RecvFd(request) => {
                    Response::open_dup_like(self.inner.on_recvfd(&request), request)
                }
                RequestKind::Cancellation(_cancellation_request) => {
                    // FIXME implement cancellation
                    continue;
                }
                RequestKind::MsyncMsg | RequestKind::MunmapMsg | RequestKind::MmapMsg => {
                    unreachable!()
                }
                RequestKind::OnClose { id } => {
                    self.inner.on_close(id);
                    continue;
                }
                RequestKind::OnDetach { .. } => continue,
            };
            self.inner
                .socket
                .write_response(response, SignalBehavior::Restart)?;
        }

        Ok(())
    }
}

struct RingDiskSchemeInner<T: Disk, Hw: Hardware> {
    scheme_name: String,
    socket: Socket,
    disks: BTreeMap<u32, RingDiskWrapper<T>>,
    handles: HandleMap<Handle<Hw, T>>,
    executor: Rc<LocalExecutor<Hw>>,
    shm_dir: Fd,
    pipe_root: Fd,
}

impl<T: Disk + Clone + 'static, Hw: Hardware> RingDiskSchemeInner<T, Hw> {
    // Checks if any conflicting handles already exist
    fn check_locks(&self, disk_i: u32, part_i_opt: Option<usize>) -> Result<()> {
        for (_, handle) in self.handles.iter() {
            let Handle::Disk { num, pt } = handle else {
                continue;
            };

            let i = *num;
            if let Some(p) = pt {
                if disk_i == i {
                    match part_i_opt {
                        Some(part_i) => {
                            if part_i == *p {
                                return Err(Error::new(ENOLCK));
                            }
                        }
                        None => {
                            return Err(Error::new(ENOLCK));
                        }
                    }
                }
            } else if disk_i == i {
                return Err(Error::new(ENOLCK));
            }
        }
        Ok(())
    }

    fn on_close(&mut self, id: usize) {
        let Some(handle) = self.handles.remove(id) else {
            return;
        };

        match handle {
            Handle::Ring(RingState::Active { join_handle, .. }) => {
                join_handle.abort();
            }

            _ => {}
        }
    }

    fn on_recvfd(&mut self, recvfd_request: &RecvFdRequest) -> Result<OpenResult> {
        let id = recvfd_request.id();
        let handle = self.handles.get(id)?;

        let Handle::Ring(RingState::Active { ring_fds, .. }) = handle else {
            return Err(Error::new(EINVAL));
        };

        recvfd_request.move_fd(&self.socket, FmoveFdFlags::CLONE, ring_fds.as_slice())?;

        Ok(OpenResult::OtherSchemeMultiple {
            // TODO: This field seems to be unused in the kernel. Can it be removed?
            num_fds: recvfd_request.num_fds(),
        })
    }
}

impl<T: Disk + Clone + 'static, Hw: Hardware> SchemeAsync for RingDiskSchemeInner<T, Hw> {
    fn scheme_root(&mut self) -> Result<usize> {
        Ok(self.handles.insert(Handle::SchemeRoot))
    }

    async fn openat(
        &mut self,
        dirfd: usize,
        path_str: &str,
        flags: usize,
        _fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if !matches!(self.handles.get(dirfd)?, Handle::SchemeRoot) {
            return Err(Error::new(EACCES));
        }

        if ctx.uid != 0 {
            return Err(Error::new(EACCES));
        }

        let path_str = path_str.trim_matches('/');

        let handle = if path_str.is_empty() {
            if flags & O_DIRECTORY == 0 && flags & O_STAT == 0 {
                return Err(Error::new(EISDIR));
            }

            let mut list = String::new();

            for (nsid, disk) in self.disks.iter() {
                writeln!(list, "{nsid}").unwrap();

                if disk.pt.is_none() {
                    continue;
                }
                for part_num in 0..disk.pt.as_ref().unwrap().partitions.len() {
                    writeln!(list, "{nsid}p{part_num}").unwrap();
                }
            }

            Handle::List(list.into_bytes())
        } else {
            let (nsid, pt) = if let Some(p_pos) = path_str.chars().position(|c| c == 'p') {
                let nsid_str = &path_str[..p_pos];

                if p_pos + 1 >= path_str.len() {
                    return Err(Error::new(ENOENT));
                }
                let part_num_str = &path_str[p_pos + 1..];

                let nsid = nsid_str.parse::<u32>().or(Err(Error::new(ENOENT)))?;
                let part_num = part_num_str.parse::<usize>().or(Err(Error::new(ENOENT)))?;

                let disk = self.disks.get(&nsid).ok_or(Error::new(ENOENT))?;
                let partition_table = disk.pt.as_ref().ok_or(Error::new(ENOENT))?;
                let _partition = partition_table
                    .partitions
                    .get(part_num as usize)
                    .ok_or(Error::new(ENOENT))?;

                (nsid, Some(part_num))
            } else {
                let nsid = path_str.parse::<u32>().or(Err(Error::new(ENOENT)))?;
                if !self.disks.contains_key(&nsid) {
                    return Err(Error::new(ENOENT));
                }

                self.check_locks(nsid, None)?;
                (nsid, None)
            };

            Handle::Disk { num: nsid, pt }
        };

        let id = self.handles.insert(handle);
        Ok(OpenResult::ThisScheme {
            number: id,
            flags: NewFdFlags::POSITIONED,
        })
    }

    async fn dup(&mut self, _old_id: usize, buf: &[u8], _ctx: &CallerCtx) -> Result<OpenResult> {
        if buf == b"uring" {
            // TODO: should we only support this on the root handle?
            //
            // let handle = self.handles.get(old_id)?;
            //
            // if !matches!(handle, Handle::SchemeRoot) {
            //     return Err(Error::new(EOPNOTSUPP));
            // }

            let new_id = self.handles.insert(Handle::Ring(RingState::Inactive));
            Ok(OpenResult::ThisScheme {
                number: new_id,
                flags: NewFdFlags::empty(),
            })
        } else {
            Err(Error::new(EOPNOTSUPP))
        }
    }

    async fn call(
        &mut self,
        id: usize,
        payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        let Handle::Ring(ref mut state) = self.handles.get_mut(id)? else {
            return Err(Error::new(EOPNOTSUPP));
        };

        let verb = RingCallVerb::try_from_raw(metadata[0] as u8).ok_or(Error::new(EINVAL))?;

        match verb {
            RingCallVerb::Setup => {
                if !matches!(state, RingState::Inactive) {
                    return Err(Error::new(EOPNOTSUPP));
                }

                let params =
                    RingSetupParams::try_mut_from_bytes(payload).map_err(|_| Error::new(EINVAL))?;

                let flags = params.flags().ok_or(Error::new(EINVAL))?;

                if params.pool_size == 0 {
                    return Err(Error::new(EINVAL));
                }

                if params.nr_sq_entries == 0 || params.nr_sq_entries > RING_MAX_SQ_ENTRIES {
                    return Err(Error::new(EINVAL));
                }

                let nr_sq_entries = params.nr_sq_entries.next_power_of_two();
                let nr_cq_entries = if flags.contains(RingSetupFlags::CQSIZE) {
                    if params.nr_cq_entries == 0 || params.nr_cq_entries > RING_MAX_CQ_ENTRIES {
                        return Err(Error::new(EINVAL));
                    }

                    let nr_cq_entries = nr_sq_entries.next_power_of_two();
                    if nr_cq_entries < nr_sq_entries {
                        return Err(Error::new(EINVAL));
                    }

                    nr_cq_entries
                } else {
                    nr_sq_entries * 2
                };

                params.nr_sq_entries = nr_sq_entries;
                params.nr_cq_entries = nr_cq_entries;

                let sq_name = format!("{id}.sq");
                let cq_name = format!("{id}.cq");

                let queue_flags = flag::O_CREAT | flag::O_RDWR | flag::O_CLOEXEC;

                let sq_fd = self.shm_dir.openat(&sq_name, queue_flags, 0)?;
                let cq_fd = self.shm_dir.openat(&cq_name, queue_flags, 0)?;
                let pipe_fd = self.pipe_root.openat("", flag::O_CLOEXEC, 0)?;
                let ring_fds = [sq_fd.raw(), cq_fd.raw(), pipe_fd.raw()];

                let sq = BlockingConsumer::<DiskOpSqe>::from_fd(sq_fd, true, Some(nr_sq_entries))?;
                let cq = BlockingProducer::<DiskOpCqe>::from_fd(cq_fd, true, Some(nr_cq_entries))?;

                let pool_size = params.pool_size as usize;
                let shm = unsafe { Dma::<[u8]>::zeroed_slice(pool_size)?.assume_init() };
                let shm_base = PhysAddr(shm.physical());

                let fixed_ftbl = Rc::new(RwLock::new(Vec::new()));
                let fixed_ftbl_for_task = fixed_ftbl.clone();

                let exec_for_task = self.executor.clone();
                let join_handle = self.executor.spawn(ring_worker_task(
                    exec_for_task,
                    sq,
                    cq,
                    pipe_fd,
                    fixed_ftbl_for_task,
                    shm_base,
                    pool_size,
                ));

                *state = RingState::Active {
                    fixed_ftbl,
                    ring_fds,
                    shm,
                    join_handle,
                };

                Ok(0)
            }

            RingCallVerb::SetFileTable => Err(Error::new(EINVAL)),
        }
    }

    async fn call_multiple_ids(
        &mut self,
        ids: &[usize],
        _payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        let (&ring_fd, ids) = ids.split_first().ok_or(Error::new(EINVAL))?;

        let verb = RingCallVerb::try_from_raw(metadata[0] as u8).ok_or(Error::new(EINVAL))?;

        match verb {
            RingCallVerb::SetFileTable => {
                if ids.is_empty() {
                    log::error!("RingCallVerb::SetFileTable: got an empty file table");
                    return Err(Error::new(EINVAL));
                }

                let files = ids
                    .iter()
                    .map(|&id| {
                        self.handles.get(id).and_then(|handle| match handle {
                            Handle::Disk { num, pt } => {
                                let disk = self.disks.get(&num).ok_or(Error::new(EBADF))?.clone();
                                let partition = if let Some(part_num) = pt {
                                    Some(
                                        disk.pt
                                            .as_ref()
                                            .ok_or(Error::new(EBADF))?
                                            .partitions
                                            .get(*part_num)
                                            .ok_or(Error::new(EBADF))?
                                            .clone(),
                                    )
                                } else {
                                    None
                                };

                                Ok(RingResource { disk, partition })
                            }
                            _ => Err(Error::new(EINVAL)),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>();

                let Handle::Ring(RingState::Active { fixed_ftbl, .. }) =
                    self.handles.get_mut(ring_fd)?
                else {
                    return Err(Error::new(EINVAL));
                };

                *fixed_ftbl.write().unwrap() = files?;
                Ok(ids.len())
            }

            RingCallVerb::Setup => Err(Error::new(EINVAL)),
        }
    }

    async fn getdents<'buf>(
        &mut self,
        _id: usize,
        _buf: DirentBuf<&'buf mut [u8]>,
        _opaque_offset: u64,
    ) -> Result<DirentBuf<&'buf mut [u8]>> {
        // TODO
        Err(Error::new(EOPNOTSUPP))
    }

    async fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        match *self.handles.get(id)? {
            Handle::List(ref data) => {
                stat.st_mode = MODE_DIR;
                stat.st_size = data.len() as u64;
                Ok(())
            }
            Handle::Disk { num, pt, .. } => {
                let disk = self.disks.get(&num).ok_or(Error::new(EBADF))?;
                if let Some(part_num) = pt {
                    let block_size = disk.block_size();
                    let part = disk
                        .pt
                        .as_ref()
                        .ok_or(Error::new(EBADF))?
                        .partitions
                        .get(part_num as usize)
                        .ok_or(Error::new(EBADF))?;
                    stat.st_mode = MODE_FILE;
                    stat.st_size = part.size * u64::from(block_size);
                    stat.st_blocks = part.size;
                    stat.st_blksize = block_size;
                    Ok(())
                } else {
                    let size = disk.size();
                    let block_size = disk.block_size();
                    stat.st_mode = MODE_FILE;
                    stat.st_blocks = size / u64::from(block_size);
                    stat.st_blksize = block_size;
                    stat.st_size = size;
                    Ok(())
                }
            }
            Handle::Ring(RingState::Active { ref shm, .. }) => {
                stat.st_mode = MODE_FILE;
                stat.st_size = shm.len() as u64;
                Ok(())
            }
            Handle::Ring(RingState::Inactive) => Err(Error::new(EOPNOTSUPP)),
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }

    async fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        let handle = self.handles.get(id)?;

        let mut i = 0;

        let scheme_name = self.scheme_name.as_bytes();
        let mut j = 0;
        // TODO: copy_from_slice
        while i < buf.len() && j < scheme_name.len() {
            buf[i] = scheme_name[j];
            i += 1;
            j += 1;
        }

        if i < buf.len() {
            buf[i] = b':';
            i += 1;
        }

        match handle {
            Handle::List(_) => (),
            Handle::Disk { num, pt, .. } => {
                let number_str = if let Some(part_num) = pt {
                    format!("{num}p{part_num}")
                } else {
                    format!("{num}")
                };

                let number_bytes = number_str.as_bytes();
                j = 0;
                while i < buf.len() && j < number_bytes.len() {
                    buf[i] = number_bytes[j];
                    i += 1;
                    j += 1;
                }
            }

            Handle::Ring(_) => return Err(Error::new(EOPNOTSUPP)),
            Handle::SchemeRoot => return Err(Error::new(EBADF)),
        }

        Ok(i)
    }

    async fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        match *self.handles.get_mut(id)? {
            Handle::List(ref handle) => {
                let src = usize::try_from(offset)
                    .ok()
                    .and_then(|o| handle.get(o..))
                    .unwrap_or(&[]);
                let count = core::cmp::min(src.len(), buf.len());
                buf[..count].copy_from_slice(&src[..count]);
                Ok(count)
            }

            Handle::Disk { num, pt } => {
                let disk = self.disks.get_mut(&num).ok_or(Error::new(EBADF))?;
                let block = offset / u64::from(disk.block_size());
                disk.read(pt, block, buf).await
            }

            Handle::SchemeRoot | Handle::Ring { .. } => Err(Error::new(EOPNOTSUPP)),
        }
    }

    async fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        match *self.handles.get_mut(id)? {
            Handle::Disk { num, pt } => {
                let disk = self.disks.get_mut(&num).ok_or(Error::new(EBADF))?;
                let block = offset / u64::from(disk.block_size());
                disk.write(pt, block, buf).await
            }

            Handle::List(_) | Handle::SchemeRoot | Handle::Ring { .. } => {
                Err(Error::new(EOPNOTSUPP))
            }
        }
    }

    async fn fsize(&mut self, id: usize, _ctx: &CallerCtx) -> Result<u64> {
        match *self.handles.get_mut(id)? {
            Handle::List(ref handle) => Ok(handle.len() as u64),
            Handle::Disk { num, pt, .. } => {
                let disk = self.disks.get_mut(&num).ok_or(Error::new(EBADF))?;
                if let Some(part_num) = pt {
                    let part = disk
                        .pt
                        .as_ref()
                        .ok_or(Error::new(EBADF))?
                        .partitions
                        .get(part_num as usize)
                        .ok_or(Error::new(EBADF))?;

                    Ok(part.size * u64::from(disk.block_size()))
                } else {
                    Ok(disk.size())
                }
            }

            Handle::Ring(_) => Err(Error::new(EOPNOTSUPP)),
            Handle::SchemeRoot => return Err(Error::new(EBADF)),
        }
    }

    async fn mmap_prep(
        &mut self,
        id: usize,
        offset: u64,
        size: usize,
        _flags: syscall::MapFlags,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let handle = self.handles.get(id)?;
        match handle {
            Handle::Ring(RingState::Active { ref shm, .. }) => {
                let offset = offset as usize;
                if offset + size > shm.len() {
                    return Err(Error::new(EINVAL));
                }
                Ok(shm.virt_addr() + offset)
            }
            _ => Err(Error::new(EBADF)),
        }
    }
}

const BATCH_LIMIT: usize = 128;

async fn ring_worker_task<Hw: Hardware, D: Disk + Clone + 'static>(
    executor: Rc<LocalExecutor<Hw>>,
    mut sq: BlockingConsumer<DiskOpSqe>,
    cq: BlockingProducer<DiskOpCqe>,
    pipe_fd: Fd,
    fixed_ftbl: Rc<RwLock<Vec<RingResource<D>>>>,
    shm_base: PhysAddr,
    pool_size: usize,
) {
    let mut queue = Vec::<DiskOpSqe>::with_capacity(BATCH_LIMIT);
    let cq = Rc::new(Mutex::new(cq));
    let ev_handle = pin!(executor.register_external_event(pipe_fd.raw(), EventFlags::READ));
    let source = RingEventSource(Mutex::new(ev_handle));
    let wq = WorkQueue::<Hw>::new();
    let mut tmp_buf = [0; 1];

    loop {
        let mut spun = false;

        for _ in 0..100 {
            match sq.try_pop() {
                Ok(req) => {
                    queue.push(req);
                    while queue.len() < BATCH_LIMIT {
                        match sq.try_pop() {
                            Ok(req) => queue.push(req),
                            Err(_) => break,
                        }
                    }
                    spun = true;
                    break;
                }
                Err(redox_rings::raw::RingPopError::Empty) => {
                    std::hint::spin_loop();
                }
                Err(e) => {
                    log::error!("Failed to pop Sqe with error: {:?}", e);
                    spun = true;
                    break;
                }
            }
        }

        let fixed_ftbl = fixed_ftbl.read().unwrap();
        for req in queue.drain(..) {
            let Some(resource) = fixed_ftbl.get(req.file_idx as usize) else {
                log::error!("invalid fixed file descriptor: {}", req.file_idx);
                cq.lock()
                    .unwrap()
                    .push(
                        DiskOpCqe {
                            user_data: req.user_data,
                            count: 0,
                            status: EBADF as u16,
                            pad: 0,
                        },
                        None,
                    )
                    .unwrap();
                continue;
            };

            let cq = Rc::clone(&cq);
            let mut worker = DiskWorker {
                cq,
                partition: resource.partition.clone(),
                disk: resource.disk.clone(),
                shm_base,
                pool_size,
            };

            let join_handle = executor.spawn(async move {
                worker.handle_request(req).await.unwrap();
            });

            wq.add(join_handle);
        }
        drop(fixed_ftbl);

        if spun {
            continue;
        }

        if let Ok(req) = sq.inner.inner.pop_async(&source, None).await {
            // TODO: A write to the pipe is only done when the waiting bit is set in the ring
            // header. So, reading once from the pipe per wakeup should be okay for now. This can't
            // be avoided as eventually the pipe's buffer will fill up and any writes to the pipe
            // from the producer will block indefinitely. We probably want to use some other event
            // mechanism here in the future. `futex` (which is the default notification mechanism if
            // a waiter is unspecified) is not feasible here as that would block the whole process
            // and not just the current async task.
            let _ = pipe_fd.read(&mut tmp_buf);
            queue.push(req);
        }
    }
}

pub struct DiskWorker<D: Disk> {
    cq: Rc<Mutex<BlockingProducer<DiskOpCqe>>>,
    disk: RingDiskWrapper<D>,
    partition: Option<Rc<partitionlib::Partition>>,
    shm_base: PhysAddr,
    pool_size: usize,
}

impl<D: Disk> DiskWorker<D> {
    pub async fn handle_request(&mut self, req: DiskOpSqe) -> std::result::Result<(), String> {
        if req.buf_offset as usize + req.buf_len as usize > self.pool_size {
            log::error!(
                "Bounds Check Failed: Offset {} + Len {} > Pool {}",
                req.buf_offset,
                req.buf_len,
                self.pool_size
            );
            return Err("Request buffer out of bounds".into());
        }

        let partition = self.partition.as_ref().map(|partition| partition.as_ref());

        let block_size = self.disk.block_size();
        if !req.buf_len.is_multiple_of(block_size) {
            return Err(
                "`buf_len` must be a multiple of the disk's block size: {block_size}".into(),
            );
        }

        let phys_addr = PhysAddr(self.shm_base.as_usize() + req.buf_offset as usize);
        let num_sectors = req.buf_len / block_size;

        let result = if let Some(opcode) = DiskOpKind::try_from_raw(req.opcode) {
            match opcode {
                DiskOpKind::Read => {
                    self.disk
                        .read_dma(partition, req.block, phys_addr, num_sectors)
                        .await
                }
                DiskOpKind::Write => {
                    self.disk
                        .write_dma(partition, req.block, phys_addr, num_sectors)
                        .await
                }
            }
        } else {
            log::warn!("Unsupported opcode: {}", req.opcode);
            Err(syscall::Error::new(syscall::EOPNOTSUPP))
        };

        let (status, count) = match result {
            Ok(()) => (0, req.buf_len),
            Err(e) => {
                log::error!("Disk read Error: ID={} Errno={}", req.user_data, e.errno);
                (e.errno as u16, 0)
            }
        };

        let mut cqe = DiskOpCqe {
            user_data: req.user_data,
            status,
            count,
            pad: 0,
        };

        loop {
            match self.cq.lock().unwrap().try_push(cqe) {
                Ok(_) => break,
                Err(RingPushError::Full(entry)) => {
                    yield_now().await;
                    cqe = entry;
                }
                Err(RingPushError::Broken(_entry)) => {
                    return Err(format!("Failed to push response: broken"));
                }
            }
        }

        Ok(())
    }
}
