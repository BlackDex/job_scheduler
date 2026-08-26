use std::collections::VecDeque;
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::schedule::{ClockSnapshot, ScheduleCursor};
use crate::{
    CancellationToken, CronSchedule, EventSink, JobContext, JobEvent, JobSchedule, MisfirePolicy,
    ScheduleError, SchedulerError, SchedulerEvent, SchedulerTimeZone, Uuid, emit, normalize_name,
    validate_schedule,
};

const DEFAULT_CLOCK_RECHECK: Duration = Duration::from_secs(30);

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

enum AsyncCallback {
    Simple(Box<dyn FnMut() -> BoxFuture + Send + 'static>),
    Context(Box<dyn FnMut(JobContext) -> BoxFuture + Send + 'static>),
}

impl AsyncCallback {
    fn call(&mut self, context: JobContext) -> BoxFuture {
        match self {
            Self::Simple(callback) => callback(),
            Self::Context(callback) => callback(context),
        }
    }
}

/// A Tokio-native asynchronous job.
pub struct AsyncJob {
    cursor: ScheduleCursor,
    callback: AsyncCallback,
    missed_limit: usize,
    job_id: Uuid,
    name: Option<String>,
    time_zone: SchedulerTimeZone,
    paused: bool,
    active: bool,
    generation: u64,
    pending: VecDeque<DateTimeUtc>,
    cancellation: CancellationToken,
}

type DateTimeUtc = chrono::DateTime<chrono::Utc>;

#[expect(
    clippy::missing_errors_doc,
    reason = "fallible constructors return the documented ScheduleError type"
)]
impl AsyncJob {
    /// Create an async cron job.
    pub fn new<F, Fut>(cron: CronSchedule, run: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::from_schedule(JobSchedule::from(cron), run)
    }

    /// Create an async job from any schedule.
    pub fn from_schedule<F, Fut>(schedule: JobSchedule, mut run: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::from_callback(schedule, AsyncCallback::Simple(Box::new(move || Box::pin(run()))))
    }

    /// Create an async job receiving [`JobContext`].
    pub fn from_schedule_with_context<F, Fut>(schedule: JobSchedule, mut run: F) -> Self
    where
        F: FnMut(JobContext) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::from_callback(
            schedule,
            AsyncCallback::Context(Box::new(move |context| Box::pin(run(context)))),
        )
    }

    /// Create a future one-shot async job.
    pub fn once<F, Fut>(at: DateTimeUtc, run: F) -> Result<Self, ScheduleError>
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        validate_schedule(&JobSchedule::Once(at))?;
        Ok(Self::from_schedule(JobSchedule::Once(at), run))
    }

    /// Create an async fixed-delay job.
    pub fn fixed_delay<F, Fut>(interval: crate::Interval, run: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::from_schedule(JobSchedule::FixedDelay(interval), run)
    }

    /// Create an async fixed-rate job.
    pub fn fixed_rate<F, Fut>(interval: crate::Interval, run: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::from_schedule(JobSchedule::FixedRate(interval), run)
    }

    fn from_callback(schedule: JobSchedule, callback: AsyncCallback) -> Self {
        Self {
            cursor: ScheduleCursor::new(schedule),
            callback,
            missed_limit: 1,
            job_id: Uuid::new_v4(),
            name: None,
            time_zone: SchedulerTimeZone::Utc,
            paused: false,
            active: false,
            generation: 0,
            pending: VecDeque::new(),
            cancellation: CancellationToken::default(),
        }
    }

    /// Stable UUID assigned to this job.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.job_id
    }

    /// Optional scheduler-unique name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Configure overdue occurrence handling.
    pub fn set_misfire_policy(&mut self, policy: MisfirePolicy) {
        self.missed_limit = match policy {
            MisfirePolicy::Skip => 1,
            MisfirePolicy::CatchUp { max_runs } => max_runs.max(1),
        };
    }

    fn event(&self, scheduled_at: Option<DateTimeUtc>) -> JobEvent {
        JobEvent {
            job_id: self.job_id,
            name: self.name.clone(),
            scheduled_at,
        }
    }

    fn context(&self, scheduled_at: DateTimeUtc) -> JobContext {
        JobContext {
            job_id: self.job_id,
            name: self.name.clone(),
            scheduled_at,
            cancellation: self.cancellation.clone(),
        }
    }

    fn queue_due(&mut self, now: ClockSnapshot) {
        if self.paused || self.active {
            return;
        }
        self.pending
            .extend(self.cursor.due(now, self.time_zone, self.missed_limit));
    }

    fn take_invocation(&mut self) -> Option<(JobEvent, u64, BoxFuture)> {
        if self.paused || self.active {
            return None;
        }
        let scheduled_at = self.pending.pop_front()?;
        let event = self.event(Some(scheduled_at));
        let context = self.context(scheduled_at);
        let future = self.callback.call(context);
        self.active = true;
        Some((event, self.generation, future))
    }

    fn completed(&mut self, generation: u64) {
        self.active = false;
        if self.generation == generation {
            let _result = self
                .cursor
                .completed_at(ClockSnapshot::now(), self.time_zone);
        }
    }

    fn next_in(&self, now: ClockSnapshot) -> Option<Duration> {
        if self.paused || self.active {
            return None;
        }
        if !self.pending.is_empty() {
            return Some(Duration::ZERO);
        }
        self.cursor.next_in(now, self.time_zone)
    }
}

/// Collection used by [`TokioRunner`].
pub struct AsyncJobScheduler {
    jobs: Vec<AsyncJob>,
    time_zone: SchedulerTimeZone,
}

impl Default for AsyncJobScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[expect(
    clippy::missing_errors_doc,
    reason = "control methods return the documented SchedulerError type"
)]
impl AsyncJobScheduler {
    /// Construct an empty UTC async scheduler.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            jobs: Vec::new(),
            time_zone: SchedulerTimeZone::Utc,
        }
    }

    /// Use the operating system's local time zone.
    pub fn use_system_timezone(&mut self) {
        self.set_time_zone(SchedulerTimeZone::Local);
    }

    /// Use UTC.
    pub fn use_utc(&mut self) {
        self.set_time_zone(SchedulerTimeZone::Utc);
    }

    /// Use a fixed UTC offset.
    pub fn set_timezone(&mut self, time_zone: chrono::FixedOffset) {
        self.set_time_zone(SchedulerTimeZone::Fixed(time_zone));
    }

    /// Use a named IANA time zone.
    #[cfg(feature = "timezones")]
    pub fn set_named_timezone(&mut self, time_zone: chrono_tz::Tz) {
        self.set_time_zone(SchedulerTimeZone::Named(time_zone));
    }

    /// Add an unnamed async job.
    pub fn add(&mut self, mut job: AsyncJob) -> Uuid {
        let job_id = job.job_id;
        job.time_zone = self.time_zone;
        self.jobs.push(job);
        job_id
    }

    /// Add a uniquely named async job.
    pub fn add_named(
        &mut self,
        name: impl Into<String>,
        mut job: AsyncJob,
    ) -> Result<Uuid, SchedulerError> {
        let name = name.into();
        let name = normalize_name(&name)?;
        if self
            .jobs
            .iter()
            .any(|existing| existing.name.as_deref() == Some(&name))
        {
            return Err(SchedulerError::DuplicateName(name));
        }
        job.name = Some(name);
        Ok(self.add(job))
    }

    fn set_time_zone(&mut self, time_zone: SchedulerTimeZone) {
        self.time_zone = time_zone;
        let now = ClockSnapshot::now();
        for job in &mut self.jobs {
            job.time_zone = time_zone;
            job.generation = job.generation.wrapping_add(1);
            job.cursor.resume(now, time_zone);
        }
    }

    fn id_by_name(&self, name: &str) -> Result<Uuid, SchedulerError> {
        self.jobs
            .iter()
            .find(|job| job.name.as_deref() == Some(name))
            .map(|job| job.job_id)
            .ok_or(SchedulerError::JobNotFound)
    }

    fn event_for(&self, job_id: Uuid) -> Result<JobEvent, SchedulerError> {
        self.jobs
            .iter()
            .find(|job| job.job_id == job_id)
            .map(|job| job.event(None))
            .ok_or(SchedulerError::JobNotFound)
    }

    fn job_mut(&mut self, job_id: Uuid) -> Result<&mut AsyncJob, SchedulerError> {
        self.jobs
            .iter_mut()
            .find(|job| job.job_id == job_id)
            .ok_or(SchedulerError::JobNotFound)
    }

    fn remove(&mut self, job_id: Uuid) -> Result<JobEvent, SchedulerError> {
        let index = self
            .jobs
            .iter()
            .position(|job| job.job_id == job_id)
            .ok_or(SchedulerError::JobNotFound)?;
        self.jobs[index].cancellation.cancel();
        let event = self.jobs[index].event(None);
        self.jobs.remove(index);
        Ok(event)
    }

    fn pause(&mut self, job_id: Uuid) -> Result<(bool, JobEvent), SchedulerError> {
        let job = self.job_mut(job_id)?;
        let changed = !job.paused;
        job.paused = true;
        Ok((changed, job.event(None)))
    }

    fn resume(&mut self, job_id: Uuid) -> Result<(bool, JobEvent), SchedulerError> {
        let job = self.job_mut(job_id)?;
        let changed = job.paused;
        if changed {
            job.paused = false;
            job.pending.clear();
            job.generation = job.generation.wrapping_add(1);
            job.cursor.resume(ClockSnapshot::now(), job.time_zone);
        }
        Ok((changed, job.event(None)))
    }

    fn reschedule(
        &mut self,
        job_id: Uuid,
        schedule: JobSchedule,
    ) -> Result<JobEvent, SchedulerError> {
        validate_schedule(&schedule)?;
        let job = self.job_mut(job_id)?;
        job.pending.clear();
        job.generation = job.generation.wrapping_add(1);
        job.cursor.replace(schedule);
        Ok(job.event(None))
    }

    fn run_now(&mut self, job_id: Uuid) -> Result<(), SchedulerError> {
        self.job_mut(job_id)?.pending.push_back(chrono::Utc::now());
        Ok(())
    }

    fn queue_due(&mut self) {
        let now = ClockSnapshot::now();
        for job in &mut self.jobs {
            job.queue_due(now);
        }
    }

    fn take_invocations(&mut self) -> Vec<(Uuid, JobEvent, u64, BoxFuture)> {
        self.jobs
            .iter_mut()
            .filter_map(|job| {
                job.take_invocation()
                    .map(|(event, generation, future)| (job.job_id, event, generation, future))
            })
            .collect()
    }

    fn complete(&mut self, job_id: Uuid, generation: u64) {
        if let Ok(job) = self.job_mut(job_id) {
            job.completed(generation);
        }
    }

    fn next_wake_in(&self) -> Option<Duration> {
        let now = ClockSnapshot::now();
        self.jobs.iter().filter_map(|job| job.next_in(now)).min()
    }

    fn cancel_all(&self) {
        for job in &self.jobs {
            job.cancellation.cancel();
        }
    }
}

enum JobSelector {
    Id(Uuid),
    Name(String),
}

enum Command {
    Add {
        name: Option<String>,
        job: Box<AsyncJob>,
        reply: oneshot::Sender<Result<Uuid, SchedulerError>>,
    },
    Remove {
        selector: JobSelector,
        reply: oneshot::Sender<Result<bool, SchedulerError>>,
    },
    Pause {
        selector: JobSelector,
        reply: oneshot::Sender<Result<bool, SchedulerError>>,
    },
    Resume {
        selector: JobSelector,
        reply: oneshot::Sender<Result<bool, SchedulerError>>,
    },
    Reschedule {
        selector: JobSelector,
        schedule: JobSchedule,
        reply: oneshot::Sender<Result<(), SchedulerError>>,
    },
    RunNow {
        selector: JobSelector,
        reply: oneshot::Sender<Result<(), SchedulerError>>,
    },
    Shutdown {
        cancel: bool,
    },
}

/// Cloneable asynchronous control handle.
#[derive(Clone)]
pub struct TokioSchedulerHandle {
    sender: mpsc::UnboundedSender<Command>,
}

#[expect(
    clippy::missing_errors_doc,
    reason = "control methods return the documented SchedulerError type"
)]
impl TokioSchedulerHandle {
    /// Add an unnamed job.
    pub async fn add(&self, job: AsyncJob) -> Result<Uuid, SchedulerError> {
        self.add_inner(None, job).await
    }

    /// Add a uniquely named job.
    pub async fn add_named(
        &self,
        name: impl Into<String>,
        job: AsyncJob,
    ) -> Result<Uuid, SchedulerError> {
        self.add_inner(Some(name.into()), job).await
    }

    /// Remove a job by UUID.
    pub async fn remove(&self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.boolean_request(|reply| Command::Remove {
            selector: JobSelector::Id(job_id),
            reply,
        })
        .await
    }

    /// Remove a job by name.
    pub async fn remove_named(&self, name: impl Into<String>) -> Result<bool, SchedulerError> {
        let name = name.into();
        self.boolean_request(|reply| Command::Remove {
            selector: JobSelector::Name(name),
            reply,
        })
        .await
    }

    /// Pause a job by UUID.
    pub async fn pause(&self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.boolean_request(|reply| Command::Pause {
            selector: JobSelector::Id(job_id),
            reply,
        })
        .await
    }

    /// Pause a job by name.
    pub async fn pause_named(&self, name: impl Into<String>) -> Result<bool, SchedulerError> {
        let name = name.into();
        self.boolean_request(|reply| Command::Pause {
            selector: JobSelector::Name(name),
            reply,
        })
        .await
    }

    /// Resume a job by UUID.
    pub async fn resume(&self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.boolean_request(|reply| Command::Resume {
            selector: JobSelector::Id(job_id),
            reply,
        })
        .await
    }

    /// Resume a job by name.
    pub async fn resume_named(&self, name: impl Into<String>) -> Result<bool, SchedulerError> {
        let name = name.into();
        self.boolean_request(|reply| Command::Resume {
            selector: JobSelector::Name(name),
            reply,
        })
        .await
    }

    /// Replace a schedule by UUID.
    pub async fn reschedule(
        &self,
        job_id: Uuid,
        schedule: JobSchedule,
    ) -> Result<(), SchedulerError> {
        self.unit_request(|reply| Command::Reschedule {
            selector: JobSelector::Id(job_id),
            schedule,
            reply,
        })
        .await
    }

    /// Replace a schedule by name.
    pub async fn reschedule_named(
        &self,
        name: impl Into<String>,
        schedule: JobSchedule,
    ) -> Result<(), SchedulerError> {
        let name = name.into();
        self.unit_request(|reply| Command::Reschedule {
            selector: JobSelector::Name(name),
            schedule,
            reply,
        })
        .await
    }

    /// Queue an immediate invocation by UUID.
    pub async fn run_now(&self, job_id: Uuid) -> Result<(), SchedulerError> {
        self.unit_request(|reply| Command::RunNow {
            selector: JobSelector::Id(job_id),
            reply,
        })
        .await
    }

    /// Queue an immediate invocation by name.
    pub async fn run_now_named(&self, name: impl Into<String>) -> Result<(), SchedulerError> {
        let name = name.into();
        self.unit_request(|reply| Command::RunNow {
            selector: JobSelector::Name(name),
            reply,
        })
        .await
    }

    async fn add_inner(&self, name: Option<String>, job: AsyncJob) -> Result<Uuid, SchedulerError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(Command::Add {
                name,
                job: Box::new(job),
                reply,
            })
            .map_err(|_| SchedulerError::RunnerStopped)?;
        response.await.map_err(|_| SchedulerError::RunnerStopped)?
    }

    async fn boolean_request(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<bool, SchedulerError>>) -> Command,
    ) -> Result<bool, SchedulerError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(command(reply))
            .map_err(|_| SchedulerError::RunnerStopped)?;
        response.await.map_err(|_| SchedulerError::RunnerStopped)?
    }

    async fn unit_request(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<(), SchedulerError>>) -> Command,
    ) -> Result<(), SchedulerError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(command(reply))
            .map_err(|_| SchedulerError::RunnerStopped)?;
        response.await.map_err(|_| SchedulerError::RunnerStopped)?
    }
}

/// Tokio-native event-driven scheduler runner.
pub struct TokioRunner {
    handle: TokioSchedulerHandle,
    join_handle: Option<JoinHandle<()>>,
}

#[expect(
    clippy::missing_errors_doc,
    reason = "runner methods return the documented SchedulerError type"
)]
impl TokioRunner {
    /// Start a Tokio runner without an event sink.
    #[must_use]
    pub fn start(scheduler: AsyncJobScheduler) -> Self {
        Self::start_with_options(scheduler, DEFAULT_CLOCK_RECHECK, None)
    }

    /// Start a Tokio runner with an event sink.
    pub fn start_with_event_sink<F>(scheduler: AsyncJobScheduler, sink: F) -> Self
    where
        F: Fn(SchedulerEvent) + Send + Sync + 'static,
    {
        Self::start_with_options(scheduler, DEFAULT_CLOCK_RECHECK, Some(Arc::new(sink)))
    }

    /// Start a runner with an explicit wall-clock recheck interval.
    #[must_use]
    pub fn start_with_options(
        scheduler: AsyncJobScheduler,
        clock_recheck: Duration,
        sink: Option<EventSink>,
    ) -> Self {
        let clock_recheck = if clock_recheck.is_zero() {
            DEFAULT_CLOCK_RECHECK
        } else {
            clock_recheck
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let handle = TokioSchedulerHandle { sender };
        let join_handle = tokio::spawn(run_loop(scheduler, receiver, clock_recheck, sink));
        Self {
            handle,
            join_handle: Some(join_handle),
        }
    }

    /// Return a cloneable async control handle.
    #[must_use]
    pub fn handle(&self) -> TokioSchedulerHandle {
        self.handle.clone()
    }

    /// Stop new work and await active jobs.
    pub async fn shutdown(mut self) -> Result<(), SchedulerError> {
        self.stop(false).await
    }

    /// Signal cancellation tokens, stop new work, and await active jobs.
    pub async fn cancel_and_shutdown(mut self) -> Result<(), SchedulerError> {
        self.stop(true).await
    }

    async fn stop(&mut self, cancel: bool) -> Result<(), SchedulerError> {
        self.handle
            .sender
            .send(Command::Shutdown { cancel })
            .map_err(|_| SchedulerError::RunnerStopped)?;
        let Some(join_handle) = self.join_handle.take() else {
            return Ok(());
        };
        join_handle
            .await
            .map_err(|_| SchedulerError::RunnerPanicked)
    }
}

impl Drop for TokioRunner {
    fn drop(&mut self) {
        if self.join_handle.is_some() {
            let _result = self.handle.sender.send(Command::Shutdown { cancel: false });
        }
    }
}

async fn run_loop(
    mut scheduler: AsyncJobScheduler,
    mut receiver: mpsc::UnboundedReceiver<Command>,
    clock_recheck: Duration,
    sink: Option<EventSink>,
) {
    let mut running = JoinSet::new();
    loop {
        scheduler.queue_due();
        for (job_id, event, generation, future) in scheduler.take_invocations() {
            emit(sink.as_ref(), SchedulerEvent::JobStarted(event.clone()));
            running.spawn(async move {
                let succeeded = std::panic::AssertUnwindSafe(future)
                    .catch_unwind()
                    .await
                    .is_ok();
                (job_id, event, generation, succeeded)
            });
        }

        let next = scheduler
            .next_wake_in()
            .map(|duration| duration.min(clock_recheck));
        let timer = async move {
            if let Some(duration) = next {
                tokio::time::sleep(duration).await;
            } else {
                pending::<()>().await;
            }
        };
        tokio::pin!(timer);

        tokio::select! {
            biased;
            command = receiver.recv() => {
                let Some(command) = command else {
                    break;
                };
                if process_command(&mut scheduler, command, sink.as_ref()) {
                    break;
                }
            }
            completed = running.join_next(), if !running.is_empty() => {
                if let Some(Ok((job_id, event, generation, succeeded))) = completed {
                    scheduler.complete(job_id, generation);
                    if succeeded {
                        emit(sink.as_ref(), SchedulerEvent::JobFinished(event));
                    } else {
                        emit(sink.as_ref(), SchedulerEvent::JobPanicked(event));
                    }
                }
            }
            () = &mut timer => {}
        }
    }

    while let Some(completed) = running.join_next().await {
        if let Ok((job_id, event, generation, succeeded)) = completed {
            scheduler.complete(job_id, generation);
            if succeeded {
                emit(sink.as_ref(), SchedulerEvent::JobFinished(event));
            } else {
                emit(sink.as_ref(), SchedulerEvent::JobPanicked(event));
            }
        }
    }
    emit(sink.as_ref(), SchedulerEvent::RunnerStopped);
}

fn process_command(
    scheduler: &mut AsyncJobScheduler,
    command: Command,
    sink: Option<&EventSink>,
) -> bool {
    match command {
        Command::Add { name, job, reply } => {
            let result = if let Some(name) = name {
                scheduler.add_named(name, *job)
            } else {
                Ok(scheduler.add(*job))
            };
            let _result = reply.send(result);
        }
        Command::Remove { selector, reply } => {
            let result = resolve_id(scheduler, selector).and_then(|job_id| {
                let event = scheduler.remove(job_id)?;
                emit(sink, SchedulerEvent::JobRemoved(event));
                Ok(true)
            });
            let _result = reply.send(result);
        }
        Command::Pause { selector, reply } => {
            let result = resolve_id(scheduler, selector).and_then(|job_id| {
                let (changed, event) = scheduler.pause(job_id)?;
                if changed {
                    emit(sink, SchedulerEvent::JobPaused(event));
                }
                Ok(changed)
            });
            let _result = reply.send(result);
        }
        Command::Resume { selector, reply } => {
            let result = resolve_id(scheduler, selector).and_then(|job_id| {
                let (changed, event) = scheduler.resume(job_id)?;
                if changed {
                    emit(sink, SchedulerEvent::JobResumed(event));
                }
                Ok(changed)
            });
            let _result = reply.send(result);
        }
        Command::Reschedule {
            selector,
            schedule,
            reply,
        } => {
            let result = resolve_id(scheduler, selector).and_then(|job_id| {
                let event = scheduler.reschedule(job_id, schedule)?;
                emit(sink, SchedulerEvent::JobRescheduled(event));
                Ok(())
            });
            let _result = reply.send(result);
        }
        Command::RunNow { selector, reply } => {
            let result =
                resolve_id(scheduler, selector).and_then(|job_id| scheduler.run_now(job_id));
            let _result = reply.send(result);
        }
        Command::Shutdown { cancel } => {
            if cancel {
                scheduler.cancel_all();
            }
            return true;
        }
    }
    false
}

fn resolve_id(
    scheduler: &AsyncJobScheduler,
    selector: JobSelector,
) -> Result<Uuid, SchedulerError> {
    match selector {
        JobSelector::Id(job_id) => scheduler.event_for(job_id).map(|event| event.job_id),
        JobSelector::Name(name) => scheduler.id_by_name(&name),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::{Interval, JobSchedule};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn different_jobs_run_concurrently() {
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let completed = Arc::new(AtomicUsize::new(0));
        let scheduler = AsyncJobScheduler::new();
        let runner = TokioRunner::start(scheduler);
        let handle = runner.handle();
        let mut ids = Vec::new();
        for _index in 0..2 {
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let completed = Arc::clone(&completed);
            ids.push(
                handle
                    .add(AsyncJob::fixed_rate(Interval::hours(1).unwrap(), move || {
                        let started = Arc::clone(&started);
                        let release = Arc::clone(&release);
                        let completed = Arc::clone(&completed);
                        async move {
                            started.fetch_add(1, Ordering::SeqCst);
                            release.notified().await;
                            completed.fetch_add(1, Ordering::SeqCst);
                        }
                    }))
                    .await
                    .unwrap(),
            );
        }
        for id in ids {
            handle.run_now(id).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 2);
        release.notify_waiters();
        runner.cancel_and_shutdown().await.unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_same_job_never_overlaps_itself() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum_active = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let scheduler = AsyncJobScheduler::new();
        let runner = TokioRunner::start(scheduler);
        let handle = runner.handle();
        let id = handle
            .add(AsyncJob::fixed_rate(Interval::hours(1).unwrap(), {
                let active = Arc::clone(&active);
                let maximum_active = Arc::clone(&maximum_active);
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move || {
                    let active = Arc::clone(&active);
                    let maximum_active = Arc::clone(&maximum_active);
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum_active.fetch_max(current, Ordering::SeqCst);
                        started.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            }))
            .await
            .unwrap();

        handle.run_now(id).await.unwrap();
        handle.run_now(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) < 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(maximum_active.load(Ordering::SeqCst), 1);
        release.notify_one();
        runner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn active_completion_does_not_consume_a_rescheduled_one_shot() {
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let scheduler = AsyncJobScheduler::new();
        let runner = TokioRunner::start(scheduler);
        let handle = runner.handle();
        let id = handle
            .add(AsyncJob::fixed_rate(Interval::hours(1).unwrap(), {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move || {
                    let invocation = started.fetch_add(1, Ordering::SeqCst);
                    let release = Arc::clone(&release);
                    async move {
                        if invocation == 0 {
                            release.notified().await;
                        }
                    }
                }
            }))
            .await
            .unwrap();
        handle.run_now(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) < 1 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        handle
            .reschedule(
                id,
                JobSchedule::Once(chrono::Utc::now() + chrono::Duration::milliseconds(50)),
            )
            .await
            .unwrap();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while started.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        runner.shutdown().await.unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn async_named_lifecycle_works() {
        let scheduler = AsyncJobScheduler::new();
        let runner = TokioRunner::start(scheduler);
        let handle = runner.handle();
        let id = handle
            .add_named("async-job", AsyncJob::fixed_rate(Interval::hours(1).unwrap(), async || {}))
            .await
            .unwrap();

        assert!(handle.pause_named("async-job").await.unwrap());
        assert!(handle.resume(id).await.unwrap());
        handle.run_now_named("async-job").await.unwrap();
        assert!(handle.remove(id).await.unwrap());
        runner.shutdown().await.unwrap();
    }
}
