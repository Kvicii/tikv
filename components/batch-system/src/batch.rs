// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

//! This is the core implementation of a batch system. Generally there will be
//! two different kind of FSMs in TiKV's FSM system. One is normal FSM, which
//! usually represents a peer, the other is control FSM, which usually
//! represents something that controls how the former is created or metrics are
//! collected.

// #[PerformanceCriticalPath]
use std::{
    borrow::Cow,
    ops::{Deref, DerefMut},
    sync::{atomic::AtomicUsize, Arc, Mutex},
    thread::{self, current, JoinHandle, ThreadId},
    time::Duration,
};

use fail::fail_point;
use file_system::{set_io_type, IoType};
use resource_control::{
    channel::{unbounded, Receiver, Sender},
    ResourceController,
};
use tikv_util::{
    debug, error, info, mpsc, safe_panic, sys::thread::StdThreadBuildWrapper, thd_name,
    time::Instant,
};

use crate::{
    config::Config,
    fsm::{Fsm, FsmScheduler, Priority},
    mailbox::BasicMailbox,
    metrics::*,
    router::Router,
    scheduler::{ControlScheduler, NormalScheduler},
};

/// A unify type for FSMs so that they can be sent to channel easily.
pub enum FsmTypes<N, C> {
    Normal((Box<N>, Instant)),
    Control((Box<C>, Instant)),
    // Used as a signal that scheduler should be shutdown.
    Empty,
}

struct MetricsCollector<N: Fsm> {
    timer: Instant, // time since polled
    round: usize,   // how many round the fsm has been continuously polled
    _phantom: std::marker::PhantomData<N>,
}

impl<N: Fsm> MetricsCollector<N> {
    fn new() -> MetricsCollector<N> {
        MetricsCollector {
            timer: Instant::now_coarse(),
            round: 0,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<N: Fsm> Drop for MetricsCollector<N> {
    fn drop(&mut self) {
        FSM_POLL_ROUND.get(N::FSM_TYPE).observe(self.round as f64);
        FSM_POLL_DURATION
            .get(N::FSM_TYPE)
            .observe(self.timer.saturating_elapsed_secs());
    }
}

pub struct NormalFsm<N: Fsm> {
    fsm: Box<N>,
    metrics: MetricsCollector<N>,
    policy: Option<ReschedulePolicy>,
}

impl<N: Fsm> NormalFsm<N> {
    #[inline]
    fn new(fsm: Box<N>) -> NormalFsm<N> {
        NormalFsm {
            fsm,
            metrics: MetricsCollector::<N>::new(),
            policy: None,
        }
    }
}
impl<N: Fsm> Deref for NormalFsm<N> {
    type Target = N;

    #[inline]
    fn deref(&self) -> &N {
        &self.fsm
    }
}

impl<N: Fsm> DerefMut for NormalFsm<N> {
    #[inline]
    fn deref_mut(&mut self) -> &mut N {
        &mut self.fsm
    }
}

/// A basic struct for a round of polling.
#[allow(clippy::vec_box)]
pub struct Batch<N: Fsm, C: Fsm> {
    normals: Vec<Option<NormalFsm<N>>>,
    control: Option<Box<C>>,
}

impl<N: Fsm, C: Fsm> Batch<N, C> {
    /// Creates a batch with given batch size.
    pub fn with_capacity(cap: usize) -> Batch<N, C> {
        Batch {
            normals: Vec::with_capacity(cap),
            control: None,
        }
    }

    fn tick_round(&mut self) {
        FSM_COUNT_PER_POLL
            .get(N::FSM_TYPE)
            .observe(self.normals.len() as f64);
        for f in self.normals.iter_mut().filter_map(Option::as_mut) {
            f.metrics.round += 1;
        }
    }

    fn push(&mut self, fsm: FsmTypes<N, C>) -> bool {
        match fsm {
            FsmTypes::Normal((n, schedule_time)) => {
                FSM_SCHEDULE_WAIT_DURATION
                    .get(N::FSM_TYPE)
                    .observe(schedule_time.saturating_elapsed_secs());
                self.normals.push(Some(NormalFsm::new(n)));
            }
            FsmTypes::Control((c, schedule_time)) => {
                FSM_SCHEDULE_WAIT_DURATION
                    .get(C::FSM_TYPE)
                    .observe(schedule_time.saturating_elapsed_secs());
                assert!(self.control.is_none());
                self.control = Some(c);
            }
            FsmTypes::Empty => return false,
        }
        true
    }

    fn is_empty(&self) -> bool {
        self.normals.is_empty() && self.control.is_none()
    }

    fn clear(&mut self) {
        self.normals.clear();
        self.control.take();
    }

    /// Releases the ownership of `fsm` so that it can be scheduled in another
    /// poller.
    ///
    /// When pending messages of the FSM is different than `expected_len`,
    /// attempts to schedule it in this poller again. Returns the `fsm` if the
    /// re-scheduling succeeds.
    fn release(&mut self, mut fsm: NormalFsm<N>, expected_len: usize) -> Option<NormalFsm<N>> {
        let mailbox = fsm.take_mailbox().unwrap();
        mailbox.release(fsm.fsm);
        if mailbox.len() == expected_len {
            None
        } else {
            match mailbox.take_fsm() {
                // It's rescheduled by other thread.
                None => None,
                Some(mut s) => {
                    s.set_mailbox(Cow::Owned(mailbox));
                    fsm.fsm = s;
                    Some(fsm)
                }
            }
        }
    }

    /// Removes the normal FSM.
    ///
    /// This method should only be called when the FSM is stopped.
    /// If there are still messages in channel, the FSM is untouched and
    /// the function will return false to let caller to keep polling.
    fn remove(&mut self, mut fsm: NormalFsm<N>) -> Option<NormalFsm<N>> {
        let mailbox = fsm.take_mailbox().unwrap();
        if mailbox.is_empty() {
            // It will be removed only when it's already closed, so no new messages can
            // be scheduled, hence don't need to consider rescheduling.
            mailbox.release(fsm.fsm);
            None
        } else {
            fsm.set_mailbox(Cow::Owned(mailbox));
            Some(fsm)
        }
    }

    /// Schedules the normal FSM located at `index`.
    pub fn schedule(&mut self, router: &BatchRouter<N, C>, index: usize) {
        let to_schedule = match self.normals[index].take() {
            Some(f) => f,
            None => {
                return;
            }
        };
        let mut res = match to_schedule.policy {
            Some(ReschedulePolicy::Release(l)) => self.release(to_schedule, l),
            Some(ReschedulePolicy::Remove) => self.remove(to_schedule),
            Some(ReschedulePolicy::Schedule) => {
                FSM_RESCHEDULE_COUNTER.get(N::FSM_TYPE).inc();
                router.normal_scheduler.schedule(to_schedule.fsm);
                None
            }
            None => Some(to_schedule),
        };
        if let Some(f) = &mut res {
            // failed to reschedule
            f.policy.take();
            self.normals[index] = res;
        }
    }

    /// Reclaims the slot storage if there is no FSM located at `index`. It will
    /// alter the positions of some other FSMs with index larger than `index`.
    #[inline]
    pub fn swap_reclaim(&mut self, index: usize) {
        if self.normals[index].is_none() {
            self.normals.swap_remove(index);
        }
    }

    /// Same as [`release`], but works with control FSM.
    pub fn release_control(&mut self, control_box: &BasicMailbox<C>, checked_len: usize) -> bool {
        let s = self.control.take().unwrap();
        control_box.release(s);
        if control_box.len() == checked_len {
            true
        } else {
            match control_box.take_fsm() {
                None => true,
                Some(s) => {
                    self.control = Some(s);
                    false
                }
            }
        }
    }

    /// Same as [`remove`], but works with control FSM.
    pub fn remove_control(&mut self, control_box: &BasicMailbox<C>) {
        if control_box.is_empty() {
            let s = self.control.take().unwrap();
            control_box.release(s);
        }
    }
}

/// The result for `PollHandler::handle_control`.
pub enum HandleResult {
    /// The FSM still needs to be handled in the next run.
    KeepProcessing,
    /// The FSM should stop at the progress.
    StopAt {
        /// The amount of messages acknowledged by the handler. The FSM
        /// should be released unless new messages arrive.
        progress: usize,
        /// Whether the FSM should be passed in to `end` call.
        skip_end: bool,
    },
}

impl HandleResult {
    #[inline]
    pub fn stop_at(progress: usize, skip_end: bool) -> HandleResult {
        HandleResult::StopAt { progress, skip_end }
    }
}

/// A handler that polls all FSMs in ready.
///
/// A general process works like the following:
///
/// ```text
/// loop {
///     begin
///     if control is ready:
///         handle_control
///     foreach ready normal:
///         handle_normal
///     light_end
///     end
/// }
/// ```
///
/// A [`PollHandler`] doesn't have to be [`Sync`] because each poll thread has
/// its own handler.
/// fsm.rs # Fsm 状态机通过 PollHandler 来驱动
/// 每个状态机都有其绑定的消息和消息队列
/// ollHandler 负责驱动状态机，处理自身队列中的消息
/// 每个 PollHandler 对应一个线程, 其在 poll 函数中会持续地检测需要驱动的状态机并进行处理
/// 此外还可能将某些 hot region 路由给其他 PollHandler 来做一些负载均衡操作
/// 每个 region 对应一个 raft 组, 而每个 raft 组在一个 BatchSystem 里就对应一个 normal 状态机:
/// 对于 RaftBatchSystem:
/// 参照 raft-rs 接口, 每个 normal 状态机在一轮 loop 中被 PollHandler 获取一次 ready, 其中一般包含需要持久化的未提交日志, 需要发送的消息和需要应用的已提交日志等;
/// 对于需要持久化的未提交日志, 最直接的做法便是将其暂时缓存到内存中进行攒批, 然后在当前 loop 结尾的 end 函数中统一同步处理, 这无疑会影响每轮 loop 的效率, TiKV 的 6.x 版本已经将 loop 结尾的同步 IO 抽到了 loop 外交给了额外的线程池去做, 这进一步提升了 store loop 的效率;
/// 对于需要发送的消息, 则通过 Transport 异步发送给对应的 store;
/// 对于需要应用的已提交日志, 则通过 applyRouter 带着回调函数发给 ApplyBatchSystem
/// 对于 ApplyBatchSystem:
/// 每个 normal 状态机在一轮 loop 中被 PollHandler 获取 RaftBatchSystem 发来的若干已经提交需要应用的日志, 其需要将其攒批提交并在之后执行对应的回调函数返回客户端结果
/// 需要注意的是, 返回客户端结果之后 ApplyBatchSystem 还需要向 RaftBatchSystem 再 propose ApplyRes 的消息, 从而更新 RaftBatchSystem 的某些内存状态
/// 比如 applyIndex, 该字段的更新能够推动某些阻塞在某个 ReadIndex 上的读请求继续执行
pub trait PollHandler<N, C>: Send + 'static {
    /// This function is called at the very beginning of every round.
    fn begin<F>(&mut self, _batch_size: usize, update_cfg: F)
    where
        for<'a> F: FnOnce(&'a Config);

    /// This function is called when the control FSM is ready.
    ///
    /// If `Some(len)` is returned, this function will not be called again until
    /// there are more than `len` pending messages in `control` FSM.
    ///
    /// If `None` is returned, this function will be called again with the same
    /// FSM `control` in the next round, unless it is stopped.
    /// control 状态机
    /// 对于每一个 Batch System, 只有一个 control 状态机, 负责管理和处理一些需要全局视野的任务
    fn handle_control(&mut self, control: &mut C) -> Option<usize>;

    /// This function is called when some normal FSMs are ready.
    /// normal 状态机
    /// 其他 normal 状态机负责处理其自身相关的任务
    fn handle_normal(&mut self, normal: &mut impl DerefMut<Target = N>) -> HandleResult;

    /// This function is called after [`handle_normal`] is called for all FSMs
    /// and before calling [`end`]. The function is expected to run lightweight
    /// works.
    fn light_end(&mut self, _batch: &mut [Option<impl DerefMut<Target = N>>]) {}

    /// This function is called at the end of every round.
    fn end(&mut self, batch: &mut [Option<impl DerefMut<Target = N>>]);

    /// This function is called when batch system is going to sleep.
    fn pause(&mut self) {}

    /// This function returns the priority of this handler.
    fn get_priority(&self) -> Priority {
        Priority::Normal
    }
}

/// Internal poller that fetches batch and call handler hooks for readiness.
pub struct Poller<N: Fsm, C: Fsm, Handler> {
    pub router: Router<N, C, NormalScheduler<N, C>, ControlScheduler<N, C>>,
    pub fsm_receiver: Receiver<FsmTypes<N, C>>,
    pub handler: Handler,
    pub max_batch_size: usize,
    pub reschedule_duration: Duration,
    pub joinable_workers: Option<Arc<Mutex<Vec<ThreadId>>>>,
}

impl<N, C, Handler> Drop for Poller<N, C, Handler>
where
    N: Fsm,
    C: Fsm,
{
    fn drop(&mut self) {
        if let Some(joinable_workers) = &self.joinable_workers {
            joinable_workers.lock().unwrap().push(current().id());
        }
    }
}

enum ReschedulePolicy {
    Release(usize),
    Remove,
    Schedule,
}

impl<N: Fsm, C: Fsm, Handler: PollHandler<N, C>> Poller<N, C, Handler> {
    fn fetch_fsm(&mut self, batch: &mut Batch<N, C>) -> bool {
        if batch.control.is_some() {
            return true;
        }

        if let Ok(fsm) = self.fsm_receiver.try_recv() {
            return batch.push(fsm);
        }

        if batch.is_empty() {
            self.handler.pause();
            if let Ok(fsm) = self.fsm_receiver.recv() {
                return batch.push(fsm);
            }
        }
        !batch.is_empty()
    }

    /// Polls for readiness and forwards them to handler. Removes stale peers if
    /// necessary.
    pub fn poll(&mut self) {
        fail_point!("poll");
        let mut batch = Batch::with_capacity(self.max_batch_size);
        let mut reschedule_fsms = Vec::with_capacity(self.max_batch_size);
        let mut to_skip_end = Vec::with_capacity(self.max_batch_size);

        // Fetch batch after every round is finished. It's helpful to protect regions
        // from becoming hungry if some regions are hot points. Since we fetch new FSM
        // every time calling `poll`, we do not need to configure a large value for
        // `self.max_batch_size`.
        let mut run = true;
        while run && self.fetch_fsm(&mut batch) {
            // If there is some region wait to be deal, we must deal with it even if it has
            // overhead max size of batch. It's helpful to protect regions from becoming
            // hungry if some regions are hot points.
            let mut max_batch_size = std::cmp::max(self.max_batch_size, batch.normals.len());
            // Update some online config if needed.
            self.handler.begin(max_batch_size, |cfg| {
                self.max_batch_size = cfg.max_batch_size();
            });
            max_batch_size = std::cmp::max(self.max_batch_size, batch.normals.len());

            if batch.control.is_some() {
                let len = self.handler.handle_control(batch.control.as_mut().unwrap());
                if batch.control.as_ref().unwrap().is_stopped() {
                    batch.remove_control(&self.router.control_box);
                } else if let Some(len) = len {
                    batch.release_control(&self.router.control_box, len);
                }
            }

            let mut hot_fsm_count = 0;
            for (i, p) in batch.normals.iter_mut().enumerate() {
                let p = p.as_mut().unwrap();
                let res = self.handler.handle_normal(p);
                if p.is_stopped() {
                    p.policy = Some(ReschedulePolicy::Remove);
                    reschedule_fsms.push(i);
                } else if p.get_priority() != self.handler.get_priority() {
                    p.policy = Some(ReschedulePolicy::Schedule);
                    reschedule_fsms.push(i);
                } else {
                    if p.metrics.timer.saturating_elapsed() >= self.reschedule_duration {
                        hot_fsm_count += 1;
                        // We should only reschedule a half of the hot regions, otherwise,
                        // it's possible all the hot regions are fetched in a batch the
                        // next time.
                        if hot_fsm_count % 2 == 0 {
                            p.policy = Some(ReschedulePolicy::Schedule);
                            reschedule_fsms.push(i);
                            continue;
                        }
                    }
                    if let HandleResult::StopAt { progress, skip_end } = res {
                        p.policy = Some(ReschedulePolicy::Release(progress));
                        reschedule_fsms.push(i);
                        if skip_end {
                            to_skip_end.push(i);
                        }
                    }
                }
            }
            let mut fsm_cnt = batch.normals.len();
            while batch.normals.len() < max_batch_size {
                if let Ok(fsm) = self.fsm_receiver.try_recv() {
                    run = batch.push(fsm);
                }
                // When `fsm_cnt >= batch.normals.len()`:
                // - No more FSMs in `fsm_receiver`.
                // - We receive a control FSM. Break the loop because ControlFsm may change
                //   state of the handler, we shall deal with it immediately after calling
                //   `begin` of `Handler`.
                if !run || fsm_cnt >= batch.normals.len() {
                    break;
                }
                let p = batch.normals[fsm_cnt].as_mut().unwrap();
                let res = self.handler.handle_normal(p);
                if p.is_stopped() {
                    p.policy = Some(ReschedulePolicy::Remove);
                    reschedule_fsms.push(fsm_cnt);
                } else if let HandleResult::StopAt { progress, skip_end } = res {
                    p.policy = Some(ReschedulePolicy::Release(progress));
                    reschedule_fsms.push(fsm_cnt);
                    if skip_end {
                        to_skip_end.push(fsm_cnt);
                    }
                }
                fsm_cnt += 1;
            }
            self.handler.light_end(&mut batch.normals);
            for index in &to_skip_end {
                batch.schedule(&self.router, *index);
            }
            to_skip_end.clear();
            self.handler.end(&mut batch.normals);

            // Update round times for metrics.
            batch.tick_round();
            // Iterate larger index first, so that `swap_reclaim` won't affect other FSMs
            // in the list.
            for index in reschedule_fsms.iter().rev() {
                batch.schedule(&self.router, *index);
                batch.swap_reclaim(*index);
            }
            reschedule_fsms.clear();
        }
        if let Some(fsm) = batch.control.take() {
            self.router.control_scheduler.schedule(fsm);
            info!("poller will exit, release the left ControlFsm");
        }
        let left_fsm_cnt = batch.normals.len();
        if left_fsm_cnt > 0 {
            info!(
                "poller will exit, schedule {} left NormalFsms",
                left_fsm_cnt
            );
            for i in 0..left_fsm_cnt {
                let to_schedule = match batch.normals[i].take() {
                    Some(f) => f,
                    None => continue,
                };
                self.router.normal_scheduler.schedule(to_schedule.fsm);
            }
        }
        batch.clear();
    }
}

/// A builder trait that can build up poll handlers.
pub trait HandlerBuilder<N, C> {
    type Handler: PollHandler<N, C>;

    fn build(&mut self, priority: Priority) -> Self::Handler;
}

/// A system that can poll FSMs concurrently and in batch.
///
/// To use the system, two type of FSMs and their PollHandlers need to be
/// defined: Normal and Control. Normal FSM handles the general task while
/// Control FSM creates normal FSM instances.
/// Batch System 的职责就是检测哪些状态机需要驱动, 然后调用 PollHandler 去消费消息
/// 消费消息会产生副作用, 而这些副作用或要落盘, 或要网络交互
/// PollHandler 在一个批次中可以处理多个 normal 状态机
/// 在 RaftStore 里一共有两个 Batch System
/// 分别是 RaftBatchSystem 和 ApplyBatchSystem
/// RaftBatchSystem 用于驱动 Raft 状态机, 包括日志的分发, 落盘, 状态跃迁等
/// 已经提交的日志会被发往 ApplyBatchSystem 进行处理, ApplyBatchSystem 将日志解析并应用到底层 KV 数据库中, 执行回调函数, 所有的写操作都遵循着这个流程
pub struct BatchSystem<N: Fsm, C: Fsm> {
    name_prefix: Option<String>,
    router: BatchRouter<N, C>,
    receiver: Receiver<FsmTypes<N, C>>,
    low_receiver: Receiver<FsmTypes<N, C>>,
    pool_size: usize,
    max_batch_size: usize,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    joinable_workers: Arc<Mutex<Vec<ThreadId>>>,
    reschedule_duration: Duration,
    low_priority_pool_size: usize,
    pool_state_builder: Option<PoolStateBuilder<N, C>>,
}

impl<N, C> BatchSystem<N, C>
where
    N: Fsm + Send + 'static,
    C: Fsm + Send + 'static,
{
    pub fn router(&self) -> &BatchRouter<N, C> {
        &self.router
    }

    pub fn build_pool_state<H: HandlerBuilder<N, C>>(
        &mut self,
        handler_builder: H,
    ) -> PoolState<N, C, H> {
        let pool_state_builder = self.pool_state_builder.take().unwrap();
        pool_state_builder.build(
            self.name_prefix.as_ref().unwrap().clone(),
            self.low_priority_pool_size,
            self.workers.clone(),
            self.joinable_workers.clone(),
            handler_builder,
            self.pool_size,
        )
    }

    fn start_poller<B>(&mut self, name: String, priority: Priority, builder: &mut B)
    where
        B: HandlerBuilder<N, C>,
        B::Handler: Send + 'static,
    {
        let handler = builder.build(priority);
        let receiver = match priority {
            Priority::Normal => self.receiver.clone(),
            Priority::Low => self.low_receiver.clone(),
        };
        let mut poller = Poller {
            router: self.router.clone(),
            fsm_receiver: receiver,
            handler,
            max_batch_size: self.max_batch_size,
            reschedule_duration: self.reschedule_duration,
            joinable_workers: if priority == Priority::Normal {
                Some(Arc::clone(&self.joinable_workers))
            } else {
                None
            },
        };
        let props = tikv_util::thread_group::current_properties();
        let t = thread::Builder::new()
            .name(name)
            .spawn_wrapper(move || {
                tikv_util::thread_group::set_properties(props);
                set_io_type(IoType::ForegroundWrite);
                poller.poll();
            })
            .unwrap();
        self.workers.lock().unwrap().push(t);
    }

    /// Start the batch system.
    pub fn spawn<B>(&mut self, name_prefix: String, mut builder: B)
    where
        B: HandlerBuilder<N, C>,
        B::Handler: Send + 'static,
    {
        for i in 0..self.pool_size {
            self.start_poller(
                thd_name!(format!("{}-{}", name_prefix, i)),
                Priority::Normal,
                &mut builder,
            );
        }
        for i in 0..self.low_priority_pool_size {
            self.start_poller(
                thd_name!(format!("{}-low-{}", name_prefix, i)),
                Priority::Low,
                &mut builder,
            );
        }
        self.name_prefix = Some(name_prefix);
    }

    /// Shutdown the batch system and wait till all background threads exit.
    pub fn shutdown(&mut self) {
        if self.name_prefix.is_none() {
            return;
        }
        let name_prefix = self.name_prefix.take().unwrap();
        info!("shutdown batch system {}", name_prefix);
        self.router.broadcast_shutdown();
        let mut last_error = None;
        for h in self.workers.lock().unwrap().drain(..) {
            debug!("waiting for {}", h.thread().name().unwrap());
            if let Err(e) = h.join() {
                error!("failed to join worker thread: {:?}", e);
                last_error = Some(e);
            }
        }
        if let Some(e) = last_error {
            safe_panic!("failed to join worker thread: {:?}", e);
        }
        info!("batch system {} is stopped.", name_prefix);
    }
}

struct PoolStateBuilder<N: Fsm, C: Fsm> {
    max_batch_size: usize,
    reschedule_duration: Duration,
    fsm_receiver: Receiver<FsmTypes<N, C>>,
    fsm_sender: Sender<FsmTypes<N, C>>,
    pool_size: usize,
}

impl<N: Fsm, C: Fsm> PoolStateBuilder<N, C> {
    fn build<H: HandlerBuilder<N, C>>(
        self,
        name_prefix: String,
        low_priority_pool_size: usize,
        workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
        joinable_workers: Arc<Mutex<Vec<ThreadId>>>,
        handler_builder: H,
        id_base: usize,
    ) -> PoolState<N, C, H> {
        PoolState {
            name_prefix,
            handler_builder,
            fsm_receiver: self.fsm_receiver,
            fsm_sender: self.fsm_sender,
            low_priority_pool_size,
            workers,
            joinable_workers,
            expected_pool_size: self.pool_size,
            max_batch_size: self.max_batch_size,
            reschedule_duration: self.reschedule_duration,
            id_base,
        }
    }
}

pub struct PoolState<N: Fsm, C: Fsm, H: HandlerBuilder<N, C>> {
    pub name_prefix: String,
    pub handler_builder: H,
    pub fsm_receiver: Receiver<FsmTypes<N, C>>,
    pub fsm_sender: Sender<FsmTypes<N, C>>,
    pub low_priority_pool_size: usize,
    pub expected_pool_size: usize,
    pub workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    pub joinable_workers: Arc<Mutex<Vec<ThreadId>>>,
    pub max_batch_size: usize,
    pub reschedule_duration: Duration,
    pub id_base: usize,
}

pub type BatchRouter<N, C> = Router<N, C, NormalScheduler<N, C>, ControlScheduler<N, C>>;

/// Create a batch system with the given thread name prefix and pool size.
///
/// `sender` and `controller` should be paired: all messages sent on the
/// `sender` will become available to the `controller`.
pub fn create_system<N: Fsm, C: Fsm>(
    cfg: &Config,
    sender: mpsc::LooseBoundedSender<C::Message>,
    controller: Box<C>,
    resource_ctl: Option<Arc<ResourceController>>,
) -> (BatchRouter<N, C>, BatchSystem<N, C>) {
    let state_cnt = Arc::new(AtomicUsize::new(0));
    let control_box = BasicMailbox::new(sender, controller, state_cnt.clone());
    let (sender, receiver) = unbounded(resource_ctl);
    let (low_sender, low_receiver) = unbounded(None); // no resource control for low fsm
    let normal_scheduler = NormalScheduler {
        sender: sender.clone(),
        low_sender,
    };
    let control_scheduler = ControlScheduler {
        sender: sender.clone(),
    };
    let pool_state_builder = PoolStateBuilder {
        max_batch_size: cfg.max_batch_size(),
        reschedule_duration: cfg.reschedule_duration.0,
        fsm_receiver: receiver.clone(),
        fsm_sender: sender,
        pool_size: cfg.pool_size,
    };
    let router = Router::new(control_box, normal_scheduler, control_scheduler, state_cnt);
    let system = BatchSystem {
        name_prefix: None,
        router: router.clone(),
        receiver,
        low_receiver,
        pool_size: cfg.pool_size,
        max_batch_size: cfg.max_batch_size(),
        workers: Arc::new(Mutex::new(Vec::new())),
        joinable_workers: Arc::new(Mutex::new(Vec::new())),
        reschedule_duration: cfg.reschedule_duration.0,
        low_priority_pool_size: cfg.low_priority_pool_size,
        pool_state_builder: Some(pool_state_builder),
    };
    (router, system)
}
