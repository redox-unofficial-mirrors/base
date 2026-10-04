use core::sync::atomic::{AtomicU16, Ordering};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Poll, Waker};

use common::dma::Dma;
use event::RawEventQueue;
use pcid_interface::irq_helpers::InterruptVector;

use crate::spec::*;
use crate::utils::align;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("syscall failed")]
    SyscallError(#[from] libredox::error::Error),
    #[error("the device is incapable of {0:?}")]
    InCapable(CfgType),
}

/// Returns the queue part sizes in bytes.
///
/// ## Reference
/// Section 2.7 Split Virtqueues of the specfication v1.2 describes the alignment
/// and size of the queue parts.
///
/// ## Panics
/// If `queue_size` is not a power of two or is zero.
pub const fn queue_part_sizes(queue_size: usize) -> (usize, usize, usize) {
    assert!(queue_size.is_power_of_two() && queue_size != 0);

    const DESCRIPTOR_ALIGN: usize = 16;
    const AVAILABLE_ALIGN: usize = 2;
    const USED_ALIGN: usize = 4;

    let queue_size = queue_size as usize;
    let desc = size_of::<Descriptor>() * queue_size;

    // `avail_header`: Size of the available ring header and the footer.
    let avail_header = size_of::<AvailableRing>() + size_of::<AvailableRingExtra>();
    let avail = avail_header + size_of::<AvailableRingElement>() * queue_size;

    // `used_header`: Size of the used ring header and the footer.
    let used_header = size_of::<UsedRing>() + size_of::<UsedRingExtra>();
    let used = used_header + size_of::<UsedRingElement>() * queue_size;

    (
        align(desc, DESCRIPTOR_ALIGN).next_multiple_of(syscall::PAGE_SIZE),
        align(avail, AVAILABLE_ALIGN).next_multiple_of(syscall::PAGE_SIZE),
        align(used, USED_ALIGN).next_multiple_of(syscall::PAGE_SIZE),
    )
}

/// Queues that share one MSI-X table entry. One thread watches that vector
/// and drains every queue in the group.
struct IrqGroup {
    queues: Mutex<Vec<Arc<Queue>>>,
}

pub trait NotifyBell: Send + Sync {
    fn ring(&self, queue_index: u16);
}

/// A chain the device has used, and how many bytes it wrote.
pub struct Retired {
    pub buffers: Vec<Buffer>,
    pub written: u32,
}

impl Retired {
    /// The byte count and the buffers, in chain order.
    ///
    /// # Panics
    /// The chain does not have exactly `N` buffers.
    pub fn into_parts<const N: usize>(self) -> (u32, [Buffer; N]) {
        let buffers = self
            .buffers
            .try_into()
            .unwrap_or_else(|_| panic!("virtio-core: unexpected chain length"));
        (self.written, buffers)
    }
}

/// What a waiting future and the queue share for one request.
#[derive(Default)]
struct Completion {
    waker: Option<Waker>,
    done: Option<Retired>,
}

type Ticket = Arc<Mutex<Completion>>;

/// A chain the device has not used yet. Keyed by its first descriptor, and removed
/// as soon as the device retires it, so the descriptor can be reused right after.
struct InFlight {
    // held until the device has used the chain
    buffers: Vec<Buffer>,
    // `None` goes to `Queue::pop_completed` instead of waking a future
    ticket: Option<Ticket>,
}

enum PendingState {
    PendingSubmit(Vec<Buffer>),
    Submitted(Ticket),
    Done,
}

pub struct PendingRequest {
    queue: Arc<Queue>,
    state: PendingState,
}

impl Future for PendingRequest {
    type Output = Retired;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        this.queue.drain_used_ring();

        let ticket = match core::mem::replace(&mut this.state, PendingState::Done) {
            PendingState::Done => panic!("virtio-core: request polled after completion"),
            PendingState::Submitted(ticket) => ticket,
            PendingState::PendingSubmit(chain) => {
                match this.queue.submit_waiting(chain, cx.waker()) {
                    Ok(ticket) => ticket,
                    Err(chain) => {
                        this.state = PendingState::PendingSubmit(chain);
                        return Poll::Pending;
                    }
                }
            }
        };

        // Race-safety note:
        // - IRQ fires before `poll` acquires the ticket lock:
        //    `poll` observes `done` and returns `Poll::Ready` (no waker needed).
        // - IRQ fires after `poll` acquires the lock:
        //    `poll` observes the incomplete state, registers the waker, releases the lock,
        //    and returns `Poll::Pending`. The IRQ thread then takes the lock and invokes the waker.
        //
        // This guarantees that lost completions/wakeups cannot occur.
        let retired = {
            let mut completion = ticket.lock().unwrap();
            if let Some(retired) = completion.done.take() {
                Some(retired)
            } else {
                completion.waker = Some(cx.waker().clone());
                None
            }
        };
        if let Some(retired) = retired {
            return Poll::Ready(retired);
        }

        this.state = PendingState::Submitted(ticket);
        Poll::Pending
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        let PendingState::Submitted(ticket) = &self.state else {
            return;
        };
        // only the waker goes, the queue keeps the chain until the device is done with it
        ticket.lock().unwrap().waker = None;
    }
}

pub struct Queue {
    queue_index: u16,
    used: Used,
    descriptor: Dma<[Descriptor]>,
    available: Available,
    vector: u16,

    consumed_index: AtomicU16,
    completions: Mutex<HashMap<u32, InFlight>>,
    ready: Mutex<VecDeque<Retired>>,
    drain_lock: Mutex<()>,
    submit_lock: Mutex<()>,
    submit_waiters: Mutex<Vec<Waker>>,

    notification_bell: Box<dyn NotifyBell>,
    descriptor_stack: crossbeam_queue::SegQueue<u16>,
    sref: Weak<Self>,
}

impl Queue {
    pub fn new<N>(
        descriptor: Dma<[Descriptor]>,
        available: Available,
        used: Used,

        notification_bell: N,
        queue_index: u16,
        vector: u16,
    ) -> Arc<Self>
    where
        N: NotifyBell + 'static,
    {
        let descriptor_stack = crossbeam_queue::SegQueue::new();
        (0..descriptor.len() as u16).for_each(|i| descriptor_stack.push(i));

        Arc::new_cyclic(|sref| Self {
            notification_bell: Box::new(notification_bell),
            available,
            descriptor,
            used,
            queue_index,
            descriptor_stack,
            consumed_index: AtomicU16::new(0),
            completions: Mutex::new(HashMap::new()),
            ready: Mutex::new(VecDeque::new()),
            drain_lock: Mutex::new(()),
            submit_lock: Mutex::new(()),
            submit_waiters: Mutex::new(Vec::new()),
            sref: sref.clone(),
            vector,
        })
    }

    fn reinit(&self) {
        self.consumed_index.store(0, Ordering::SeqCst);
        // this frees the buffers held by these, so the caller must have reset the device first
        self.completions.lock().unwrap().clear();
        self.ready.lock().unwrap().clear();
        self.submit_waiters.lock().unwrap().clear();
        self.available.set_head_idx(0);

        // Drain all of the available descriptors.
        while let Some(_) = self.descriptor_stack.pop() {}

        // Refill the descriptor stack.
        (0..self.descriptor.len() as u16).for_each(|i| self.descriptor_stack.push(i));
    }

    fn recycle_chain(&self, mut table_index: u32) {
        loop {
            let idx = table_index as usize;
            if idx >= self.descriptor.len() {
                log::error!("virtio-core: used ring table_index {table_index} out of range");
                return;
            }

            let has_next = self.descriptor[idx].flags().contains(DescriptorFlags::NEXT);
            let next = self.descriptor[idx].next();
            self.descriptor_stack.push(table_index as u16);
            if !has_next {
                break;
            }
            table_index = u32::from(next);
        }
    }

    fn check_chain(&self, chain: &[Buffer]) {
        if chain.is_empty() {
            panic!("virtio-core: submitted chain is empty");
        }
        if chain.len() > self.descriptor_len() {
            panic!("virtio-core: submitted chain is longer than the virtqueue");
        }
    }

    /// Places the chain on the ring. The chain comes back if the virtqueue has no free descriptors.
    fn try_submit(&self, chain: Vec<Buffer>, ticket: Option<Ticket>) -> Result<u32, Vec<Buffer>> {
        let first_descriptor = {
            let _guard = self.submit_lock.lock().unwrap();

            let mut allocated = Vec::with_capacity(chain.len());
            for buffer in &chain {
                let Some(descriptor) = self.descriptor_stack.pop() else {
                    for desc in allocated {
                        self.descriptor_stack.push(desc);
                    }
                    return Err(chain);
                };
                allocated.push(descriptor);

                let idx = descriptor as usize;
                self.descriptor[idx].set_addr(buffer.buffer as u64);
                self.descriptor[idx].set_flags(buffer.flags);
                self.descriptor[idx].set_size(buffer.size as u32);
            }

            for pair in allocated.windows(2) {
                self.descriptor[pair[0] as usize].set_next(Some(pair[1]));
            }
            let first_descriptor = u32::from(allocated[0]);
            let last_descriptor = allocated[allocated.len() - 1];
            self.descriptor[last_descriptor as usize].set_next(None);

            self.completions.lock().unwrap().insert(
                first_descriptor,
                InFlight {
                    buffers: chain,
                    ticket,
                },
            );

            let avail_idx = self.available.head_index();
            self.available
                .get_element_at(avail_idx as usize)
                .set_table_index(allocated[0]);
            self.available.set_head_idx(avail_idx.wrapping_add(1));
            self.notification_bell.ring(self.queue_index);
            first_descriptor
        };

        self.drain_used_ring();
        Ok(first_descriptor)
    }

    /// Submits `chain`, or gives it back if the virtqueue stays full.
    /// The waker is registered before the retry, so a recycle in between wakes us.
    fn submit_waiting(&self, chain: Vec<Buffer>, waker: &Waker) -> Result<Ticket, Vec<Buffer>> {
        let ticket = Ticket::default();
        match self.try_submit(chain, Some(ticket.clone())) {
            Ok(_) => Ok(ticket),
            Err(chain) => {
                self.submit_waiters.lock().unwrap().push(waker.clone());
                self.drain_used_ring();
                match self.try_submit(chain, Some(ticket.clone())) {
                    Ok(_) => Ok(ticket),
                    Err(chain) => Err(chain),
                }
            }
        }
    }

    /// Harvest every newly-used descriptor chain. Individual futures must not
    /// advance the used-ring bookmark which can race and drop completions.
    pub fn drain_used_ring(&self) {
        let guard = self.drain_lock.lock().unwrap();
        let mut to_wake = Vec::new();
        let mut recycled = false;

        loop {
            let consumed = self.consumed_index.load(Ordering::SeqCst);
            let device_head = self.used.head_index();
            if consumed == device_head {
                break;
            }

            let element = self.used.get_element_at(consumed as usize);
            let table_index = element.table_index.get();
            let written = element.written.get();

            let in_flight = self.completions.lock().unwrap().remove(&table_index);
            if let Some(InFlight { buffers, ticket }) = in_flight {
                self.recycle_chain(table_index);
                recycled = true;
                match ticket {
                    None => self
                        .ready
                        .lock()
                        .unwrap()
                        .push_back(Retired { buffers, written }),
                    Some(ticket) => {
                        let mut completion = ticket.lock().unwrap();
                        completion.done = Some(Retired { buffers, written });
                        if let Some(waker) = completion.waker.take() {
                            to_wake.push(waker);
                        }
                    }
                }
            }

            self.consumed_index
                .store(consumed.wrapping_add(1), Ordering::SeqCst);
        }

        drop(guard);
        to_wake.into_iter().for_each(Waker::wake);
        if recycled {
            let waiters = core::mem::take(&mut *self.submit_waiters.lock().unwrap());
            waiters.into_iter().for_each(Waker::wake);
        }
    }

    /// Submit a chain without waiting for it. Once the device has used it, the
    /// chain comes back through [`Self::pop_completed`] (virtio-net RX).
    ///
    /// ## Panics
    /// Empty chain, chain longer than the virtqueue, or no free descriptors.
    /// The RX path is expected to post at most [`Self::descriptor_len`] buffers.
    pub fn post(&self, chain: Vec<Buffer>) -> u32 {
        self.check_chain(&chain);
        self.try_submit(chain, None)
            .unwrap_or_else(|_| panic!("virtio-core: virtqueue is out of descriptors"))
    }

    /// Takes the oldest chain the device has used from those submitted with [`Self::post`].
    pub fn pop_completed(&self) -> Option<Retired> {
        self.drain_used_ring();
        self.ready.lock().unwrap().pop_front()
    }

    /// Returns how many used chains are waiting for [`Self::pop_completed`].
    pub fn completed_len(&self) -> usize {
        self.drain_used_ring();
        self.ready.lock().unwrap().len()
    }

    /// Submit a chain and wait until the device has used it.
    ///
    /// The chain is placed on the ring on first poll. If the virtqueue is
    /// temporarily out of descriptors, the future stays pending until drain
    /// recycles enough, then submits.
    ///
    /// The returned chain is the one that was submitted. The queue keeps it until
    /// the device has used it, even if the returned future is dropped before that.
    ///
    /// ## Panics
    /// Empty chain, or chain longer than the virtqueue.
    #[must_use = "The function returns a future that must be awaited to ensure the sent request is completed."]
    pub fn send(&self, chain: Vec<Buffer>) -> PendingRequest {
        self.check_chain(&chain);
        PendingRequest {
            queue: self.sref.upgrade().unwrap(),
            state: PendingState::PendingSubmit(chain),
        }
    }

    /// Returns the number of descriptors in the descriptor table of this queue.
    pub fn descriptor_len(&self) -> usize {
        self.descriptor.len()
    }
}

static_assertions::assert_impl_all!(Queue: Send, Sync);

pub struct Available {
    mem: Dma<[u8]>,
    queue_size: usize,
}

impl<'a> Available {
    pub fn ring(&self) -> &AvailableRing {
        unsafe { &*self.mem.as_ptr().cast() }
    }
    pub fn ring_mut(&mut self) -> &mut AvailableRing {
        unsafe { &mut *self.mem.as_mut_ptr().cast() }
    }
    pub fn new(queue_size: usize) -> Result<Self, Error> {
        let (_, _, size) = queue_part_sizes(queue_size);
        let mem = unsafe {
            Dma::zeroed_slice(size)
                .map_err(Error::SyscallError)?
                .assume_init()
        };

        unsafe { Self::from_raw(mem, queue_size) }
    }

    /// `addr` is the physical address of the ring.
    pub unsafe fn from_raw(mem: Dma<[u8]>, queue_size: usize) -> Result<Self, Error> {
        let ring = Self { mem, queue_size };

        for i in 0..queue_size {
            // Setting them to `u16::MAX` helps with debugging since qemu reports them
            // as illegal values.
            ring.get_element_at(i)
                .table_index
                .store(u16::MAX, Ordering::SeqCst);
        }

        Ok(ring)
    }

    /// ## Panics
    /// This function panics if the index is out of bounds.
    pub fn get_element_at(&self, index: usize) -> &AvailableRingElement {
        // SAFETY: We have exclusive access to the elements and the number of elements
        //         is correct; same as the queue size.
        unsafe {
            self.ring()
                .elements
                .as_slice(self.queue_size)
                .get(index % self.queue_size)
                .expect("virtio-core::available: index out of bounds")
        }
    }

    pub fn head_index(&self) -> u16 {
        self.ring().head_index.load(Ordering::SeqCst)
    }

    pub fn set_head_idx(&self, index: u16) {
        self.ring().head_index.store(index, Ordering::SeqCst);
    }

    pub fn phys_addr(&self) -> usize {
        self.mem.physical()
    }
}

impl<'a> Drop for Available {
    fn drop(&mut self) {
        log::warn!(
            "virtio-core: dropping 'available' ring at {:#x}",
            self.phys_addr()
        );
    }
}

pub struct Used {
    mem: Dma<[u8]>,
    queue_size: usize,
}

impl Used {
    fn ring(&self) -> &UsedRing {
        unsafe { &*self.mem.as_ptr().cast() }
    }
    fn ring_mut(&mut self) -> &mut UsedRing {
        unsafe { &mut *self.mem.as_mut_ptr().cast() }
    }

    pub fn new(queue_size: usize) -> Result<Self, Error> {
        let (_, _, size) = queue_part_sizes(queue_size);
        let mem = unsafe {
            Dma::zeroed_slice(size)
                .map_err(Error::SyscallError)?
                .assume_init()
        };

        unsafe { Self::from_raw(mem, queue_size) }
    }

    /// `addr` is the physical address of the ring.
    pub unsafe fn from_raw(mem: Dma<[u8]>, queue_size: usize) -> Result<Self, Error> {
        let mut ring = Self { mem, queue_size };

        for i in 0..queue_size {
            // Setting them to `u32::MAX` helps with debugging since qemu reports them
            // as illegal values.
            ring.get_mut_element_at(i).table_index.set(u32::MAX);
        }

        Ok(ring)
    }

    /// ## Panics
    /// This function panics if the index is out of bounds.
    pub fn get_element_at(&self, index: usize) -> &UsedRingElement {
        // SAFETY: We have exclusive access to the elements and the number of elements
        //         is correct; same as the queue size.
        unsafe {
            self.ring()
                .elements
                .as_slice(self.queue_size)
                .get(index % self.queue_size)
                .expect("virtio-core::used: index out of bounds")
        }
    }

    /// ## Panics
    /// This function panics if the index is out of bounds.
    pub fn get_mut_element_at(&mut self, index: usize) -> &mut UsedRingElement {
        // SAFETY: We have exclusive access to the elements and the number of elements
        //         is correct; same as the queue size.
        let queue_size = self.queue_size;
        unsafe {
            self.ring_mut()
                .elements
                .as_mut_slice(queue_size)
                .get_mut(index % queue_size)
                .expect("virtio-core::used: index out of bounds")
        }
    }

    pub fn flags(&self) -> u16 {
        self.ring().flags.get()
    }

    pub fn head_index(&self) -> u16 {
        self.ring().head_index.get()
    }

    pub fn phys_addr(&self) -> usize {
        self.mem.physical()
    }
}

impl Drop for Used {
    fn drop(&mut self) {
        log::warn!(
            "virtio-core: dropping 'used' ring at {:#x}",
            self.phys_addr()
        );
    }
}

pub trait Transport: Sync + Send {
    /// `size` specifies the size of the read in bytes.
    ///
    /// ## Panics
    /// This function panics if the provided `size` is more than `size_of::<u64>()`.
    fn load_config(&self, offset: u8, size: u8) -> u64;

    /// Resets the device.
    fn reset(&self);

    /// Returns whether the device supports the specified feature.
    fn check_device_feature(&self, feature: u32) -> bool;

    /// Acknowledges the specified feature.
    ///
    /// **Note**: [`Transport::check_device_feature`] must be used to check whether
    /// the device supports the feature before acknowledging it.
    fn ack_driver_feature(&self, feature: u32);

    /// Finalizes the acknowledged features by setting the `FEATURES_OK` bit in the
    /// device status flags.
    fn finalize_features(&self);

    /// Sets `DRIVER_OK`. Queues must already exist and features must be finalized.
    fn run_device(&self) {
        self.insert_status(DeviceStatusFlags::DRIVER_OK);
    }

    /// Ask the device to fire `vector` on configuration changes.
    ///
    /// Writes `config_msix_vector` only; no IRQ thread. Take `vector` from
    /// [`crate::Device::alloc_irq`] and wait on that file yourself.
    fn setup_config_notify(&self, irq_vec: &InterruptVector);

    /// Each time the device configuration changes this number will be updated.
    fn config_generation(&self) -> u32;

    /// Returns the maximum number of virtqueues the device supports.
    fn num_queues(&self) -> u16;

    /// Creates a virtqueue on `irq`'s vector and watches that vector.
    ///
    /// Prefer [`crate::Device::setup_queue`], which allocates `irq` from this
    /// device's [`pcid_interface::irq_helpers::Msix`] table. `InterruptVector`
    /// is what keeps the table index and IRQ file paired.
    fn setup_queue(&self, irq: InterruptVector) -> Result<Arc<Queue>, Error>;

    // TODO(andypython): Should this function be unsafe?
    fn reinit_queue(&self, queue: Arc<Queue>);
    fn insert_status(&self, status: DeviceStatusFlags);
}

struct StandardBell<'a>(&'a mut AtomicU16);

impl NotifyBell for StandardBell<'_> {
    #[inline]
    fn ring(&self, queue_index: u16) {
        self.0.store(queue_index, Ordering::SeqCst);
    }
}

pub struct StandardTransport<'a> {
    pub(crate) common: Mutex<&'a mut CommonCfg>,
    notify: *const u8,
    notify_mul: u32,
    device_space: *const u8,

    queue_index: AtomicU16,
    irq_groups: Mutex<HashMap<u16, Arc<IrqGroup>>>,
}

impl<'a> StandardTransport<'a> {
    pub fn new(
        common: &'a mut CommonCfg,
        notify: *const u8,
        notify_mul: u32,
        device_space: *const u8,
    ) -> Arc<Self> {
        Arc::new(Self {
            common: Mutex::new(common),
            notify,
            notify_mul,

            queue_index: AtomicU16::new(0),
            irq_groups: Mutex::new(HashMap::new()),
            device_space,
        })
    }

    fn attach_irq(&self, irq: InterruptVector, queue: Arc<Queue>) {
        let group = {
            let vector = queue.vector;
            let mut groups = self.irq_groups.lock().unwrap();

            if let Some(group) = groups.get(&vector) {
                group.queues.lock().unwrap().push(queue);
                return;
            }

            let group = Arc::new(IrqGroup {
                queues: Mutex::new(vec![queue]),
            });
            groups.insert(vector, group.clone());

            group
        };

        // `Device::setup_queue` drops `InterruptVector` on return, the thread
        // still needs this fd to subscribe and ACK
        let mut irq_file = irq
            .irq_handle()
            .try_clone()
            .expect("virtio-core: failed to clone IRQ handle");

        let event_queue = RawEventQueue::new().unwrap();
        event_queue
            .subscribe(irq_file.as_raw_fd() as usize, 0, event::EventFlags::READ)
            .unwrap();

        std::thread::spawn(move || {
            for _ in event_queue.map(Result::unwrap) {
                let queues = group.queues.lock().unwrap().clone();
                for queue in &queues {
                    queue.drain_used_ring();
                }

                let mut buf = [0u8; size_of::<usize>()];
                if irq_file.read(&mut buf).unwrap_or(0) == size_of::<usize>() {
                    let _ = irq_file.write(&buf);
                }

                // ACK can unmask completions that arrived during the read.
                for queue in &queues {
                    queue.drain_used_ring();
                }
            }
        });
    }
}

impl Transport for StandardTransport<'_> {
    fn load_config(&self, offset: u8, size: u8) -> u64 {
        unsafe {
            let ptr = self.device_space.add(offset as usize);
            let size = size as usize;

            if size == size_of::<u8>() {
                ptr.cast::<u8>().read() as u64
            } else if size == size_of::<u16>() {
                ptr.cast::<u16>().read() as u64
            } else if size == size_of::<u32>() {
                ptr.cast::<u32>().read() as u64
            } else if size == size_of::<u64>() {
                ptr.cast::<u64>().read() as u64
            } else {
                unreachable!()
            }
        }
    }

    fn reset(&self) {
        let mut common = self.common.lock().unwrap();

        common.device_status.set(DeviceStatusFlags::empty());
        // Upon reset, the device must initialize device status to 0.
        assert_eq!(common.device_status.get(), DeviceStatusFlags::empty());
    }

    fn check_device_feature(&self, feature: u32) -> bool {
        let mut common = self.common.lock().unwrap();

        common.device_feature_select.set(feature >> 5);
        (common.device_feature.get() & (1 << (feature & 31))) != 0
    }

    fn ack_driver_feature(&self, feature: u32) {
        let mut common = self.common.lock().unwrap();

        common.driver_feature_select.set(feature >> 5);

        let current = common.driver_feature.get();
        common.driver_feature.set(current | (1 << (feature & 31)));
    }

    fn finalize_features(&self) {
        // Check VirtIO version 1 compliance.
        assert!(self.check_device_feature(VIRTIO_F_VERSION_1));
        self.ack_driver_feature(VIRTIO_F_VERSION_1);

        let mut common = self.common.lock().unwrap();

        let status = common.device_status.get();
        common
            .device_status
            .set(status | DeviceStatusFlags::FEATURES_OK);

        // Re-read device status to ensure the `FEATURES_OK` bit is still set: otherwise,
        // the device does not support our subset of features and the device is unusable.
        let confirm = common.device_status.get();
        assert!((confirm & DeviceStatusFlags::FEATURES_OK) == DeviceStatusFlags::FEATURES_OK);
    }

    fn setup_config_notify(&self, irq_vec: &InterruptVector) {
        self.common
            .lock()
            .unwrap()
            .config_msix_vector
            .set(irq_vec.vector());
    }

    fn config_generation(&self) -> u32 {
        u32::from(self.common.lock().unwrap().config_generation.get())
    }

    fn num_queues(&self) -> u16 {
        self.common.lock().unwrap().num_queues.get()
    }

    fn setup_queue(&self, irq: InterruptVector) -> Result<Arc<Queue>, Error> {
        let mut common = self.common.lock().unwrap();

        let queue_index = self.queue_index.fetch_add(1, Ordering::SeqCst);
        common.queue_select.set(queue_index);

        let queue_size = common.queue_size.get() as usize;
        let queue_notify_idx = common.queue_notify_off.get();

        // Allocate memory for the queue structues.
        let descriptor = unsafe {
            Dma::<[Descriptor]>::zeroed_slice(queue_size)
                .map_err(Error::SyscallError)?
                .assume_init()
        };

        let avail = Available::new(queue_size)?;
        let used = Used::new(queue_size)?;

        common.queue_desc.set(descriptor.physical() as u64);
        common.queue_driver.set(avail.phys_addr() as u64);
        common.queue_device.set(used.phys_addr() as u64);

        // Set the MSI-X vector.
        let vector = irq.vector();
        common.queue_msix_vector.set(vector);
        assert!(common.queue_msix_vector.get() == vector);

        // Enable the queue.
        common.queue_enable.set(1);

        let notification_bell = unsafe {
            let offset = self.notify_mul * queue_notify_idx as u32;
            &mut *(self.notify.add(offset as usize) as *mut AtomicU16)
        };

        log::debug!(
            "virtio-core: enabled queue #{queue_index} (size={queue_size} vector={vector})"
        );

        let queue = Queue::new(
            descriptor,
            avail,
            used,
            StandardBell(notification_bell),
            queue_index,
            vector,
        );

        drop(common);
        self.attach_irq(irq, queue.clone());
        Ok(queue)
    }

    fn insert_status(&self, status: DeviceStatusFlags) {
        let mut common = self.common.lock().unwrap();
        let old = common.device_status.get();

        common.device_status.set(old | status);
    }

    /// Re-initializes a queue; usually done after a device reset.
    fn reinit_queue(&self, queue: Arc<Queue>) {
        let mut common = self.common.lock().unwrap();
        queue.reinit();

        common.queue_select.set(queue.queue_index);

        common.queue_desc.set(queue.descriptor.physical() as u64);
        common.queue_driver.set(queue.available.phys_addr() as u64);
        common.queue_device.set(queue.used.phys_addr() as u64);

        // Set the MSI-X vector.
        common.queue_msix_vector.set(queue.vector);
        assert!(common.queue_msix_vector.get() == queue.vector);

        // Enable the queue.
        common.queue_enable.set(1);
    }
}

unsafe impl Send for StandardTransport<'_> {}
unsafe impl Sync for StandardTransport<'_> {}
