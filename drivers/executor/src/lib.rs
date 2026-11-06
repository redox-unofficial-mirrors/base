#![feature(negative_impls)]

mod task;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::fs::File;
use std::future::Future;
use std::hash::Hash;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::os::fd::AsRawFd;
use std::pin::Pin;
use std::ptr::NonNull;
use std::rc::Rc;
use std::task::{Context, Poll, RawWakerVTable};

use event::{EventFlags, RawEventQueue};
use intrusive_collections::{LinkedList, LinkedListLink, UnsafeRef, intrusive_adapter};
pub use task::vtable;
use task::*;

pub async fn yield_now() {
    struct YieldNow {
        yielded: bool,
    }

    impl Future for YieldNow {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.yielded {
                Poll::Ready(())
            } else {
                self.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    YieldNow { yielded: false }.await
}

pub struct JoinHandle<Hw: Hardware, T> {
    task_ref: TaskRef,
    _phantom: PhantomData<(Hw, T)>,
}

impl<Hw: Hardware, T> JoinHandle<Hw, T> {
    pub fn abort(self) {
        self.task_ref.cancel();
    }
}

type EventUserData = usize;

pub trait Hardware: Sized + 'static {
    type CmdId: Clone + Copy + Debug + Hash + Eq + PartialEq;
    type CqId: Clone + Copy + Debug + Hash + Eq + PartialEq;
    type SqId: Clone + Copy + Debug + Hash + Eq + PartialEq;
    type Sqe: Debug + Clone + Copy;
    type Cqe;
    type Iv: Clone + Copy + Debug;

    type GlobalCtxt;

    // TODO: the kernel should also do this automatically before sending EOI messages to the IC
    fn mask_vector(ctxt: &Self::GlobalCtxt, iv: Self::Iv);
    fn unmask_vector(ctxt: &Self::GlobalCtxt, iv: Self::Iv);

    fn set_sqe_cmdid(sqe: &mut Self::Sqe, id: Self::CmdId);
    fn get_cqe_cmdid(cqe: &Self::Cqe) -> Self::CmdId;

    // TODO: support multiple SQs per CQ or vice versa?
    fn sq_cq(ctxt: &Self::GlobalCtxt, id: Self::CqId) -> Self::SqId;

    fn current() -> Rc<LocalExecutor<Self>>;
    fn vtable() -> &'static RawWakerVTable;

    fn try_submit(
        _ctxt: &Self::GlobalCtxt,
        _sq_id: Self::SqId,
        _success: impl FnOnce(Self::CmdId) -> Self::Sqe,
        _fail: impl FnOnce(),
    ) -> Option<(Self::CqId, Self::CmdId)> {
        unimplemented!("try_submit is unimplemented");
    }

    fn push_sqe(
        _ctxt: &Self::GlobalCtxt,
        _sq_id: Self::SqId,
        _success: impl FnOnce(Self::CmdId) -> Self::Sqe,
        _fail: impl FnOnce(),
    ) -> Option<(Self::CqId, Self::CmdId)> {
        unimplemented!("push_sqe is unimplemented");
    }
    fn submit(_ctxt: &Self::GlobalCtxt, _sq_id: Self::SqId) {
        unimplemented!("submit is unimplemented");
    }
    fn poll_cqes(ctxt: &Self::GlobalCtxt, handle: impl FnMut(Self::CqId, Self::Cqe));
}

intrusive_adapter!(WorkQueueAdapter = UnsafeRef<TaskHeader>: TaskHeader { wq_link => LinkedListLink });

pub struct WorkQueue<Hw: Hardware> {
    runnable_tasks: Box<RefCell<LinkedList<WorkQueueAdapter>>>,
    _hw_and_not_send: PhantomData<(Hw, *const ())>,
}

impl<Hw: Hardware> WorkQueue<Hw> {
    pub fn new() -> Self {
        Self {
            runnable_tasks: Box::new(RefCell::new(LinkedList::new(WorkQueueAdapter::new()))),
            _hw_and_not_send: PhantomData,
        }
    }

    pub fn add<T>(&self, handle: JoinHandle<Hw, T>) {
        handle
            .task_ref
            .wq
            .set(Some(NonNull::from(&*self.runnable_tasks)));

        let task = unsafe { UnsafeRef::from_raw(handle.task_ref.into_raw().as_ptr()) };
        self.runnable_tasks.borrow_mut().push_back(task);
    }
}

impl<Hw: Hardware> Drop for WorkQueue<Hw> {
    fn drop(&mut self) {
        let mut n = 0;
        let mut runnable_tasks = self.runnable_tasks.borrow_mut();

        while let Some(node) = runnable_tasks.pop_front() {
            let header = unsafe { NonNull::new_unchecked(UnsafeRef::into_raw(node)) };
            let task = unsafe { TaskRef::from_raw(header) };
            task.cancel();
            n += 1;
        }

        if n != 0 {
            log::warn!("executor: cancelled {n} tasks");
        }
    }
}

/// Async executor, single IV, thread-per-core architecture
pub struct LocalExecutor<Hw: Hardware> {
    global_ctxt: Hw::GlobalCtxt,

    queue: RawEventQueue,
    vector: Hw::Iv,
    irq_handle: File,
    intx: bool,

    // TODO: One IV and SQ/CQ per core (where the admin queue can be managed by the main thread).
    awaiting_submission: RefCell<HashMap<Hw::SqId, VecDeque<TaskRef>>>,
    awaiting_completion:
        RefCell<HashMap<Hw::CqId, HashMap<Hw::CmdId, (TaskRef, NonNull<Option<Hw::Cqe>>)>>>,

    pending_submits: RefCell<HashSet<Hw::SqId>>,

    external_event: RefCell<HashMap<EventUserData, (TaskRef, NonNull<EventFlags>)>>,
    next_user_data: Cell<usize>,

    ready_queue: RefCell<LinkedList<ReadyAdapter>>,
    is_polling: Cell<bool>,
}

impl<Hw: Hardware> LocalExecutor<Hw> {
    /// Subscribe to events produced by `fd`. The event type is specified via `flags`. The function
    /// returns an [`ExternalEventHandle`] that can be polled for events. When the handle is
    /// dropped, it unsubscribes from the event.
    pub fn register_external_event(
        &self,
        fd: usize,
        flags: event::EventFlags,
    ) -> ExternalEventHandle<Hw> {
        let user_data = self.next_user_data.get();
        self.next_user_data.set(user_data.checked_add(1).unwrap());

        self.queue
            .subscribe(fd, user_data, flags)
            .expect("failed to subscribe to event");

        ExternalEventHandle {
            flags: event::EventFlags::empty(),
            user_data,
            fd,
            _not_send: PhantomData,
        }
    }

    pub fn current() -> Rc<Self> {
        Hw::current()
    }

    pub fn poll(&self) -> usize {
        assert!(!self.is_polling.replace(true));

        let mut polled = 0;
        let mut ready_queue = self.ready_queue.borrow_mut().take();

        while let Some(node) = ready_queue.pop_front() {
            let header = unsafe { NonNull::new_unchecked(UnsafeRef::into_raw(node)) };
            let task_ref = unsafe { TaskRef::from_raw(header) };

            if task_ref.is_cancelled() {
                continue;
            }

            task_ref.poll();

            if let Some(wq) = task_ref.wq.get()
                && task_ref.is_finished()
            {
                let node = {
                    let mut list = unsafe { wq.as_ref() }.borrow_mut();
                    unsafe { list.cursor_mut_from_ptr(task_ref.as_ptr()) }
                        .remove()
                        .unwrap()
                };
                let _ =
                    unsafe { TaskRef::from_raw(NonNull::new_unchecked(UnsafeRef::into_raw(node))) };

                task_ref.wq.set(None);
                continue;
            }

            polled += 1;
        }

        let mut pending_submits = self.pending_submits.borrow_mut();
        for sq_id in pending_submits.drain() {
            Hw::submit(&self.global_ctxt, sq_id);
        }
        self.is_polling.set(false);

        polled
    }

    pub fn spawn<F>(&self, fut: F) -> JoinHandle<Hw, F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let task = Task::<Hw, F>::alloc(fut);
        let task_for_handle = task.clone();

        enqueue::<Hw>(task);

        JoinHandle {
            task_ref: task_for_handle,
            _phantom: PhantomData,
        }
    }

    pub fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: Future,
    {
        let retval = Rc::new(RefCell::new(None));
        let retval2 = Rc::clone(&retval);

        let task = Task::<Hw, _>::alloc(async move {
            *retval2.borrow_mut() = Some(fut.await);
        });
        enqueue::<Hw>(task);

        loop {
            let finished = self.poll();
            if retval.borrow().is_some() {
                break;
            }

            if finished == 0 && self.ready_queue.borrow().is_empty() {
                if self.poll_cqes() != 0 {
                    continue;
                }

                self.react();
            }
        }

        retval.borrow_mut().take().unwrap()
    }
    fn react(&self) {
        let event = self.queue.next_event().expect("failed to get next event");

        if event.user_data != 0 {
            let Some((task, flags_ptr)) = self.external_event.borrow_mut().remove(&event.user_data)
            else {
                // Spurious event
                return;
            };
            unsafe {
                flags_ptr
                    .as_ptr()
                    .write(event::EventFlags::from_bits_retain(event.flags));
            }

            enqueue::<Hw>(task);
            return;
        }

        if self.intx {
            let mut buf = [0_u8; size_of::<usize>()];
            if (&self.irq_handle).read(&mut buf).unwrap() != 0 {
                let amount = (&self.irq_handle).write(&buf).unwrap();
                assert!(amount == size_of::<usize>());
            }
        }

        // If the CQ is empty then this IRQ may be for a CQE which we have already dequeued in
        // `block_on`.
        if self.poll_cqes() == 0 {
            return;
        }

        // TODO: The kernel should probably do the masking (when using MSI/MSI-X at least), which
        // should happen before EOI messages to the interrupt controller.
        Hw::mask_vector(&self.global_ctxt, self.vector);

        while self.poll_cqes() != 0 {}

        Hw::unmask_vector(&self.global_ctxt, self.vector);
    }

    fn poll_cqes(&self) -> usize {
        let mut to_wake = Vec::new();

        Hw::poll_cqes(&self.global_ctxt, |cq_id, cqe| {
            if let Some((task, comp_ptr)) = self
                .awaiting_completion
                .borrow_mut()
                .get_mut(&cq_id)
                .and_then(|per_cmd| per_cmd.remove(&Hw::get_cqe_cmdid(&cqe)))
            {
                unsafe {
                    comp_ptr.as_ptr().write(Some(cqe));
                }
                to_wake.push(task);

                if let Some(submitting) = self
                    .awaiting_submission
                    .borrow_mut()
                    .get_mut(&Hw::sq_cq(&self.global_ctxt, cq_id))
                    .and_then(|q| q.pop_front())
                {
                    to_wake.push(submitting);
                }
            }
        });

        let woken = to_wake.len();

        for task in to_wake {
            enqueue::<Hw>(task);
        }

        woken
    }

    pub async fn submit<I>(&self, sq_id: Hw::SqId, cmd_init: I) -> Hw::Cqe
    where
        I: FnMut(Hw::CmdId) -> Hw::Sqe,
    {
        CqeFuture::<Hw, I> {
            state: State::Submitting {
                sq_id,
                cmd_init,
                awaiting_submission_task: None,
            },
            comp: None,
            _not_send: PhantomData,
        }
        .await
    }
}

enum State<Hw: Hardware, I: FnMut(Hw::CmdId) -> Hw::Sqe> {
    Submitting {
        sq_id: Hw::SqId,
        cmd_init: I,
        awaiting_submission_task: Option<TaskRef>,
    },
    Completing {
        cq_id: Hw::CqId,
        cmd_id: Hw::CmdId,
    },
    Done,
}

struct CqeFuture<Hw: Hardware, I: FnMut(Hw::CmdId) -> Hw::Sqe> {
    state: State<Hw, I>,
    comp: Option<Hw::Cqe>,
    _not_send: PhantomData<*const ()>,
}

impl<Hw: Hardware, I: FnMut(Hw::CmdId) -> Hw::Sqe> Future for CqeFuture<Hw, I> {
    type Output = Hw::Cqe;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };

        let (executor, task) = current_executor_and_task::<Hw>(cx);

        match this.state {
            State::Submitting {
                sq_id,
                ref mut cmd_init,
                ref mut awaiting_submission_task,
            } => {
                let submitted = Hw::push_sqe(
                    &executor.global_ctxt,
                    sq_id,
                    |cmd_id| {
                        let mut cmd = cmd_init(cmd_id);
                        Hw::set_sqe_cmdid(&mut cmd, cmd_id);
                        cmd
                    },
                    || {
                        executor
                            .awaiting_submission
                            .borrow_mut()
                            .entry(sq_id)
                            .or_default()
                            .push_back(task.clone());
                    },
                );

                if let Some((cq_id, cmd_id)) = submitted {
                    executor.pending_submits.borrow_mut().insert(sq_id);
                    executor
                        .awaiting_completion
                        .borrow_mut()
                        .entry(cq_id)
                        .or_default()
                        .insert(cmd_id, (task, (&mut this.comp).into()));
                    this.state = State::Completing { cq_id, cmd_id };
                } else {
                    *awaiting_submission_task = Some(task);
                }
                Poll::Pending
            }

            State::Completing { cq_id, cmd_id } => match this.comp.take() {
                Some(comp) => {
                    this.state = State::Done;
                    Poll::Ready(comp)
                }

                // Shouldn't technically be possible
                None => {
                    log::error!("spurious poll");
                    executor
                        .awaiting_completion
                        .borrow_mut()
                        .entry(cq_id)
                        .or_default()
                        .insert(cmd_id, (task, (&mut this.comp).into()));
                    Poll::Pending
                }
            },

            State::Done => unreachable!("`CqeFuture` polled after completion"),
        }
    }
}

impl<Hw: Hardware, I: FnMut(Hw::CmdId) -> Hw::Sqe> Drop for CqeFuture<Hw, I> {
    fn drop(&mut self) {
        let executor = LocalExecutor::<Hw>::current();

        match self.state {
            State::Submitting {
                sq_id,
                ref awaiting_submission_task,
                ..
            } => {
                if let Some(awaiting_submission_task) = awaiting_submission_task
                    && let Some(queue) = executor.awaiting_submission.borrow_mut().get_mut(&sq_id)
                {
                    queue.retain(|task| task.as_ptr() != awaiting_submission_task.as_ptr());
                }
            }

            State::Completing { cq_id, cmd_id } => {
                if let Some(cq) = executor.awaiting_completion.borrow_mut().get_mut(&cq_id) {
                    let _ = cq.remove(&cmd_id);
                }
            }

            State::Done => {}
        }
    }
}

pub struct Event {
    flags: event::EventFlags,
    _not_send: PhantomData<*const ()>,
}

impl Event {
    pub fn flags(&self) -> event::EventFlags {
        self.flags
    }
}

pub struct NextEventFuture<'a, Hw: Hardware>(Pin<&'a mut ExternalEventHandle<Hw>>);

impl<Hw: Hardware> Future for NextEventFuture<'_, Hw> {
    type Output = Option<Event>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let event_handle = unsafe { this.0.as_mut().get_unchecked_mut() };

        let flags = std::mem::take(&mut event_handle.flags);

        if flags.is_empty() {
            let (executor, task) = current_executor_and_task::<Hw>(cx);
            // NOTE: [`LocalExecutor::register_external_event`] returns a unique
            // [`ExternalEventHandle`] every time. If an entry in the `external_event` list already
            // exists, then this was a spurious poll.
            let _ = executor.external_event.borrow_mut().insert(
                event_handle.user_data,
                (task, (&mut event_handle.flags).into()),
            );

            return Poll::Pending;
        }

        Poll::Ready(Some(Event {
            flags,
            _not_send: PhantomData,
        }))
    }
}

impl<Hw: Hardware> Drop for NextEventFuture<'_, Hw> {
    fn drop(&mut self) {
        let executor = LocalExecutor::<Hw>::current();
        let _pending = executor
            .external_event
            .borrow_mut()
            .remove(&self.0.user_data);
    }
}

pub struct ExternalEventHandle<Hw: Hardware> {
    flags: event::EventFlags,
    fd: usize,
    user_data: EventUserData,
    _not_send: PhantomData<(*const (), fn() -> Hw)>,
}

// The executor writes to the `flags` field on an event. Moving the struct would thereby make the
// registered pointer invalid.
impl<Hw: Hardware> !Unpin for ExternalEventHandle<Hw> {}

impl<Hw: Hardware> ExternalEventHandle<Hw> {
    pub fn next(self: Pin<&mut Self>) -> NextEventFuture<'_, Hw> {
        NextEventFuture(self)
    }
}

impl<Hw: Hardware> Drop for ExternalEventHandle<Hw> {
    fn drop(&mut self) {
        let executor = LocalExecutor::<Hw>::current();
        let _pending = executor.external_event.borrow_mut().remove(&self.user_data);
        let fd = self.fd;
        if let Err(err) = executor.queue.unsubscribe(fd) {
            log::error!("failed to unsubscribe from external events produced by fd {fd}: {err}");
        }
    }
}

pub fn init_raw<Hw: Hardware>(
    global_ctxt: Hw::GlobalCtxt,
    vector: Hw::Iv,
    intx: bool,
    irq_handle: File,
) -> LocalExecutor<Hw> {
    let queue = RawEventQueue::new().expect("failed to allocate event queue for local executor");

    // TODO: Multiple CPUs
    queue
        .subscribe(irq_handle.as_raw_fd() as usize, 0, EventFlags::READ)
        .expect("failed to subscribe to IRQ event");

    LocalExecutor {
        global_ctxt,

        queue,
        vector,
        intx,
        irq_handle,

        awaiting_submission: RefCell::new(HashMap::new()),
        awaiting_completion: RefCell::new(HashMap::new()),
        pending_submits: RefCell::new(HashSet::new()),
        external_event: RefCell::new(HashMap::new()),
        next_user_data: Cell::new(1),
        is_polling: Cell::new(false),
        ready_queue: RefCell::new(LinkedList::new(ReadyAdapter::new())),
    }
}
