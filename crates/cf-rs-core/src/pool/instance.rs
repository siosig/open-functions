//! `InstancePool`: manages the running [`crate::runtime::InstanceHandle`]s for
//! ONE function, per plan.md's "InstancePool" Design Notes and
//! data-model.md's "Instance (memory-only)" / "InstancePool (internal)" entities.
//!
//! One pool exists per function name, created/destroyed by a higher layer
//! (the Registry service, T040) — this module only implements the pool's own
//! behavior: acquiring an instance for a request (starting one if needed,
//! single-flight to avoid a thundering herd), reaping idle instances, and
//! reactively forgetting about crashed ones.
//!
//! # Crash detection design
//!
//! [`crate::runtime::InstanceHandle::wait`] and
//! [`crate::runtime::InstanceHandle::stop`] both consume `self` and resolve
//! from the *same* underlying `exit_rx` that `ProcessDriver`'s own
//! background task drives — so there is no need for `InstancePool` to spawn
//! a duplicate "watch this instance" task per instance. Instead, each
//! instance's `InstanceHandle` is stored directly in [`PoolState`] once it is
//! ready, and whichever pool code path decides the instance is done (the
//! idle reaper, or [`InstancePool::report_dead`]) is the one that takes the
//! handle out and calls `.stop()`/`.wait()` on it, in a short-lived spawned
//! task so the decision-making caller never blocks on it.
//!
//! Spontaneous crashes are detected **reactively**: the pool has no
//! standing "watch every instance" task. Whoever is forwarding a request
//! (T039, out of scope here) notices a connection failure and calls
//! [`InstancePool::report_dead`], which removes the instance from rotation
//! so `acquire()` never hands it out again. A crash on an instance that is
//! sitting fully idle (no in-flight request to notice the failure) is only
//! discovered on its next use — this is a deliberate, documented
//! simplification: no task in tasks.md asks for active health-checking, and
//! the acceptance criteria for T038 only exercise crash-during-active-use.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};

use crate::runtime::{Driver, DriverError, InstanceHandle, InstanceSpec};

/// What to do with a request that arrives once every instance is at its
/// per-instance `concurrency` limit and the pool is already at
/// `max_instances` (or the process-wide `max_total_instances` limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePolicy {
    /// Wait (up to `queue_max_wait`) for a slot to free up.
    Wait,
    /// Fail immediately.
    Reject,
}

/// Static, per-function pool configuration. Mirrors the `concurrency`,
/// `min_instances`, `max_instances`, `idle_timeout_secs`, `queue_policy`,
/// `queue_max_wait_secs` fields of `Function.exec_config` in data-model.md.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Per-instance concurrent request limit.
    pub concurrency: u32,
    pub min_instances: u32,
    pub max_instances: u32,
    /// How long an instance may sit fully idle (zero in-flight requests)
    /// before the idle reaper stops it.
    pub idle_timeout: Duration,
    pub queue_policy: QueuePolicy,
    /// Only consulted when `queue_policy == Wait`.
    pub queue_max_wait: Duration,
    /// Passed through to `InstanceSpec::start_timeout` for new instances,
    /// and used to bound how long a caller waits for a concurrent
    /// in-progress start before retrying (see `acquire`'s single-flight
    /// note below).
    pub start_timeout: Duration,
    /// Grace period given to an instance between SIGTERM and SIGKILL when
    /// the pool stops it (idle reaper, drain).
    pub stop_grace: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("failed to start a new instance: {0}")]
    Spawn(#[from] DriverError),
    #[error("all instances at capacity and queue wait exceeded {0:?}")]
    QueueTimeout(Duration),
    #[error("all instances at capacity and queue_policy is reject")]
    Rejected,
    #[error("pool is draining and cannot accept new work")]
    Draining,
}

/// A permit representing "one in-flight request against one specific
/// instance". Dropping it releases that instance's concurrency slot.
pub struct AcquiredInstance {
    pub addr: SocketAddr,
    _permit: OwnedSemaphorePermit,
}

/// Per-instance bookkeeping. Holds the [`InstanceHandle`] directly: whichever
/// pool code path (idle reaper, `report_dead`) decides to stop or forget an
/// instance takes it out of here and drives it to completion itself.
struct InstanceState {
    handle: Option<InstanceHandle>,
    /// `concurrency` permits; a free permit means the instance can take
    /// another concurrent request.
    semaphore: Arc<Semaphore>,
    last_used: Instant,
    /// One permit from the pool's process-wide `global_limit`, held for
    /// this instance's entire lifetime and released (back to the global
    /// pool of all `InstancePool`s in this process) when this
    /// `InstanceState` is dropped.
    _global_permit: OwnedSemaphorePermit,
}

struct PoolState {
    /// `InstanceSpec` template (env/entry_point/signature_type/artifact_path/
    /// memory_mib) for the CURRENT revision. Swapped atomically by
    /// `InstancePool::set_spec_template` on a version switch; already-running
    /// instances are unaffected, only instances started *after* the swap use
    /// the new template.
    spec_template: InstanceSpec,
    instances: HashMap<SocketAddr, InstanceState>,
    /// `Some` while one caller is in the middle of `driver.spawn()` for a
    /// new instance. Other callers that also want to start one instead wait
    /// on this `Notify` and then retry "find a free slot" — this is the
    /// thundering-herd prevention plan.md's InstancePool section calls for.
    starting: Option<Arc<Notify>>,
    /// Set by `begin_drain`; new `acquire()` calls fail with
    /// `AcquireError::Draining` once set. Actually stopping the drained
    /// instances (once every in-flight request completes) is a Registry-
    /// level (T040) orchestration concern, per plan.md's "revision cutover" note —
    /// this pool only refuses new work once told to drain.
    draining: bool,
}

/// Manages instances for one function. See the module docs for the overall
/// design and the crash-detection rationale.
pub struct InstancePool {
    function_name: String,
    /// Label value for `cf_rs_cold_start_seconds{driver}` (T082/US5):
    /// `driver.kind()`, captured once at construction since a pool's driver
    /// never changes over its lifetime.
    driver_kind: &'static str,
    driver: Arc<dyn Driver>,
    config: PoolConfig,
    inner: Mutex<PoolState>,
    /// Process-wide `max_total_instances` safety valve (research.md R12),
    /// shared across every `InstancePool` in this process. One permit is
    /// held per currently-running instance (not per request), for that
    /// instance's whole lifetime.
    global_limit: Arc<Semaphore>,
}

/// Outcome of trying to become the single caller allowed to start a new
/// instance right now.
enum BeginStart {
    /// This caller won the single-flight race and should call
    /// `driver.spawn()` with `spec`; `permit` must be stored on the new
    /// `InstanceState` (or dropped, on spawn failure) to release it back to
    /// `global_limit`.
    Proceed {
        permit: OwnedSemaphorePermit,
        spec: InstanceSpec,
    },
    /// Someone else is already starting one; wait on this `Notify` (bounded,
    /// since `Notify::notify_waiters` can race a late subscriber — see
    /// `acquire`) then retry from the top.
    WaitForOther(Arc<Notify>),
    /// `max_instances` or the global limit is exhausted; apply `queue_policy`.
    AtCapacity,
    Draining,
}

impl InstancePool {
    pub fn new(
        function_name: String,
        driver: Arc<dyn Driver>,
        spec_template: InstanceSpec,
        config: PoolConfig,
        global_instance_limit: Arc<Semaphore>,
    ) -> Self {
        Self {
            function_name,
            driver_kind: driver.kind(),
            driver,
            config,
            global_limit: global_instance_limit,
            inner: Mutex::new(PoolState {
                spec_template,
                instances: HashMap::new(),
                starting: None,
                draining: false,
            }),
        }
    }

    /// Swaps the `InstanceSpec` template used for instances started from now
    /// on (version switch). Already-running instances keep running under the
    /// old template until stopped.
    pub async fn set_spec_template(&self, spec_template: InstanceSpec) {
        self.inner.lock().await.spec_template = spec_template;
    }

    /// Stops accepting new work: subsequent `acquire()` calls fail with
    /// `AcquireError::Draining`. Does not itself stop already-running
    /// instances; composing that with in-flight-request tracking is a
    /// Registry-level (T040) concern.
    pub async fn begin_drain(&self) {
        self.inner.lock().await.draining = true;
    }

    pub async fn is_draining(&self) -> bool {
        self.inner.lock().await.draining
    }

    /// Stops every currently-tracked instance (SIGTERM, up to `grace`, then
    /// SIGKILL — see `InstanceHandle::stop`), for process shutdown (T061).
    /// Does not set `draining`: the caller (process shutdown) has already
    /// stopped accepting new connections at the listener level, so refusing
    /// new `acquire()` calls here isn't needed and would only matter for a
    /// pool this call doesn't otherwise affect. Runs every instance's stop
    /// concurrently and waits for all of them, so the process doesn't exit
    /// mid-teardown.
    pub async fn stop_all(&self, grace: Duration) {
        let handles: Vec<InstanceHandle> = {
            let mut state = self.inner.lock().await;
            let handles = state
                .instances
                .drain()
                .filter_map(|(_, mut inst)| inst.handle.take())
                .collect();
            self.report_instance_count_gauge(0);
            handles
        };
        let mut tasks = Vec::with_capacity(handles.len());
        for handle in handles {
            tasks.push(tokio::spawn(async move {
                let _ = handle.stop(grace).await;
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Number of instances currently tracked (ready and running). Exposed as
    /// a plain `pub` method rather than `#[cfg(test)]` since it is also
    /// useful for an admin/metrics endpoint later.
    pub async fn instance_count(&self) -> usize {
        self.inner.lock().await.instances.len()
    }

    /// Acquires one in-flight-request slot against some running instance,
    /// starting a new instance if needed. Order, per plan.md: an existing
    /// ready instance with a free slot, then starting exactly one new
    /// instance (single-flight), then `queue_policy` once at capacity.
    pub async fn acquire(&self) -> Result<AcquiredInstance, AcquireError> {
        let wait_start = Instant::now();
        loop {
            if let Some(acquired) = self.try_acquire_existing().await {
                metrics::histogram!("cf_rs_queue_wait_seconds", "function" => self.function_name.clone())
                    .record(wait_start.elapsed().as_secs_f64());
                return Ok(acquired);
            }

            match self.begin_start().await {
                BeginStart::Proceed { permit, spec } => {
                    self.finish_start(permit, spec).await?;
                    continue;
                }
                BeginStart::WaitForOther(notify) => {
                    // `Notify::notify_waiters` only wakes tasks already
                    // polling `.notified()`; a caller that reads `starting`
                    // and calls `.notified()` just after the starter's
                    // `notify_waiters()` fired would otherwise wait forever.
                    // Bounding by `start_timeout` (the starter's own spawn
                    // deadline) turns a missed wakeup into "retry the loop a
                    // little late" instead of a hang.
                    let _ =
                        tokio::time::timeout(self.config.start_timeout, notify.notified()).await;
                    continue;
                }
                BeginStart::Draining => return Err(AcquireError::Draining),
                BeginStart::AtCapacity => match self.config.queue_policy {
                    QueuePolicy::Reject => return Err(AcquireError::Rejected),
                    QueuePolicy::Wait => {
                        let elapsed = wait_start.elapsed();
                        if elapsed >= self.config.queue_max_wait {
                            return Err(AcquireError::QueueTimeout(self.config.queue_max_wait));
                        }
                        let remaining = self.config.queue_max_wait - elapsed;
                        tokio::time::sleep(remaining.min(Duration::from_millis(20))).await;
                        continue;
                    }
                },
            }
        }
    }

    /// Starts new instances (via the same single-flight `begin_start`/
    /// `finish_start` primitives `acquire()` uses) until `instance_count()`
    /// reaches `target`, without claiming any concurrency slot on them — so
    /// they sit fully idle, ready to serve the first real request instantly.
    /// Used at startup restore (T060) to pre-warm `min_instances`, per
    /// FR-016. Stops early (returning the first error) if a spawn fails,
    /// rather than retrying forever against a persistently broken artifact;
    /// the caller decides whether that's fatal (restore treats it as a
    /// warning, not a startup failure).
    pub async fn warm_to(&self, target: u32) -> Result<(), DriverError> {
        loop {
            if self.instance_count().await as u32 >= target {
                return Ok(());
            }
            match self.begin_start().await {
                BeginStart::Proceed { permit, spec } => {
                    self.finish_start(permit, spec).await?;
                }
                BeginStart::WaitForOther(notify) => {
                    let _ =
                        tokio::time::timeout(self.config.start_timeout, notify.notified()).await;
                }
                BeginStart::AtCapacity | BeginStart::Draining => return Ok(()),
            }
        }
    }

    /// Reports that the instance at `addr` has become unusable (crashed, or
    /// the caller detected a connection failure). Idempotent — reporting an
    /// address that's already been removed, or was never tracked, is a
    /// no-op.
    pub async fn report_dead(&self, addr: SocketAddr) {
        let removed = {
            let mut state = self.inner.lock().await;
            let removed = state.instances.remove(&addr);
            self.report_instance_count_gauge(state.instances.len());
            removed
        };
        if let Some(mut inst) = removed {
            metrics::counter!("cf_rs_instance_crashes_total", "function" => self.function_name.clone())
                .increment(1);
            if let Some(handle) = inst.handle.take() {
                // Drive the handle to completion in the background so this
                // call doesn't block; we already know the instance is dead,
                // this just lets the driver's own bookkeeping (e.g. process
                // reaping) finish cleanly.
                tokio::spawn(async move {
                    let _ = handle.wait().await;
                });
            }
        }
        // `inst` (and its `_global_permit`), if any, has already dropped by
        // now, releasing the global slot.
    }

    /// Refreshes `cf_rs_instances{function,state="ready"}` (T082/US5) after a
    /// change to the tracked instance count. This pool doesn't distinguish a
    /// separate "starting" state from "ready" (an instance only enters
    /// `state.instances` once `driver.spawn()` has already confirmed
    /// readiness), so `"ready"` is the only state value this pool ever
    /// reports.
    fn report_instance_count_gauge(&self, count: usize) {
        metrics::gauge!(
            "cf_rs_instances",
            "function" => self.function_name.clone(),
            "state" => "ready",
        )
        .set(count as f64);
    }

    /// Background task: every 30s, stops instances idle past
    /// `idle_timeout`, without dropping the running count below
    /// `min_instances`. Returns the `JoinHandle` so callers (T040) manage
    /// its lifetime; not spawned automatically so the pool's own unit tests
    /// stay fast and deterministic.
    pub fn spawn_idle_reaper(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                self.reap_idle_once().await;
            }
        })
    }

    /// One idle-reaper sweep, factored out so tests can drive it directly
    /// instead of waiting on the real 30s `interval`.
    pub async fn reap_idle_once(&self) {
        let now = Instant::now();
        let to_stop: Vec<(SocketAddr, InstanceHandle)> = {
            let mut state = self.inner.lock().await;
            let running = state.instances.len() as u32;
            if running <= self.config.min_instances {
                return;
            }
            let mut removable = running - self.config.min_instances;

            let concurrency = self.config.concurrency as usize;
            let idle_timeout = self.config.idle_timeout;
            let mut candidates: Vec<SocketAddr> = state
                .instances
                .iter()
                .filter(|(_, inst)| {
                    // Fully idle only: no in-flight requests (all permits
                    // free) AND not used recently. A busy instance is never
                    // reaped even if it was first acquired long ago.
                    inst.semaphore.available_permits() == concurrency
                        && now.duration_since(inst.last_used) >= idle_timeout
                })
                .map(|(addr, _)| *addr)
                .collect();
            candidates.truncate(removable as usize);

            let mut stopped = Vec::with_capacity(candidates.len());
            for addr in candidates.drain(..) {
                if removable == 0 {
                    break;
                }
                if let Some(mut inst) = state.instances.remove(&addr) {
                    if let Some(handle) = inst.handle.take() {
                        stopped.push((addr, handle));
                    }
                    removable -= 1;
                }
            }
            self.report_instance_count_gauge(state.instances.len());
            stopped
        };

        for (_addr, handle) in to_stop {
            let grace = self.config.stop_grace;
            tokio::spawn(async move {
                let _ = handle.stop(grace).await;
            });
        }
    }

    /// Tries to claim a free concurrency slot on some already-running
    /// instance, without starting anything new. Also bumps `last_used` on
    /// success.
    async fn try_acquire_existing(&self) -> Option<AcquiredInstance> {
        let mut state = self.inner.lock().await;
        let addrs: Vec<SocketAddr> = state.instances.keys().copied().collect();
        for addr in addrs {
            let sem = match state.instances.get(&addr) {
                Some(inst) => Arc::clone(&inst.semaphore),
                None => continue,
            };
            // `try_acquire_owned` is synchronous (no await while holding the
            // pool lock).
            if let Ok(permit) = sem.try_acquire_owned() {
                if let Some(inst) = state.instances.get_mut(&addr) {
                    inst.last_used = Instant::now();
                }
                return Some(AcquiredInstance {
                    addr,
                    _permit: permit,
                });
            }
        }
        None
    }

    /// Single-flight gate for starting a new instance: only the first caller
    /// to find "no free slot, need to start one" actually spawns.
    async fn begin_start(&self) -> BeginStart {
        let mut state = self.inner.lock().await;
        if state.draining {
            return BeginStart::Draining;
        }
        if let Some(notify) = &state.starting {
            return BeginStart::WaitForOther(Arc::clone(notify));
        }
        if state.instances.len() as u32 >= self.config.max_instances {
            return BeginStart::AtCapacity;
        }
        let permit = match Arc::clone(&self.global_limit).try_acquire_owned() {
            Ok(permit) => permit,
            // Process-wide `max_total_instances` exhausted: same "at
            // capacity" condition as hitting this pool's own max_instances.
            Err(_) => return BeginStart::AtCapacity,
        };
        let notify = Arc::new(Notify::new());
        state.starting = Some(notify);
        BeginStart::Proceed {
            permit,
            spec: state.spec_template.clone(),
        }
    }

    /// Calls `driver.spawn()` (outside the pool lock — this can take up to
    /// `start_timeout`), then registers the new instance (or releases the
    /// global permit on failure) and wakes anyone waiting on the single-
    /// flight `Notify`.
    async fn finish_start(
        &self,
        permit: OwnedSemaphorePermit,
        spec: InstanceSpec,
    ) -> Result<(), DriverError> {
        let spawn_start = Instant::now();
        let result = self.driver.spawn(&spec).await;

        let mut state = self.inner.lock().await;
        let notify = state.starting.take();

        let outcome = match result {
            Ok(handle) => {
                metrics::counter!("cf_rs_instance_starts_total", "function" => self.function_name.clone(), "result" => "ok")
                    .increment(1);
                metrics::histogram!("cf_rs_cold_start_seconds", "function" => self.function_name.clone(), "driver" => self.driver_kind)
                    .record(spawn_start.elapsed().as_secs_f64());
                let addr = handle.addr;
                state.instances.insert(
                    addr,
                    InstanceState {
                        handle: Some(handle),
                        semaphore: Arc::new(Semaphore::new(self.config.concurrency as usize)),
                        last_used: Instant::now(),
                        _global_permit: permit,
                    },
                );
                self.report_instance_count_gauge(state.instances.len());
                Ok(())
            }
            Err(e) => {
                metrics::counter!("cf_rs_instance_starts_total", "function" => self.function_name.clone(), "result" => "fail")
                    .increment(1);
                // `permit` drops here (falls out of scope unstored),
                // releasing the global slot back for someone else.
                Err(e)
            }
        };
        drop(state);
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        outcome
    }
}
