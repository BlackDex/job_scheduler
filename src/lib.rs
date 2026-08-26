//! A cross-platform, in-process cron and interval scheduler.
//!
//! The original polling API remains available through [`JobScheduler::tick`].
//! [`ThreadRunner`] adds an event-driven background thread, while the optional
//! `tokio` feature provides a native async runner.
//!
//! # Cron backends
//!
//! The default `cron` feature preserves compatibility with earlier releases.
//! Enable the mutually-exclusive `croner` feature for extended expressions,
//! including `L` for the last day of a month.
//!
//! ```toml
//! [dependencies]
//! job_scheduler_ng = { version = "2", default-features = false, features = ["croner"] }
//! ```

#[cfg(not(any(feature = "cron", feature = "croner")))]
compile_error!("either feature \"cron\" or feature \"croner\" must be enabled");

#[cfg(all(feature = "cron", feature = "croner"))]
compile_error!("feature \"cron\" and feature \"croner\" cannot be enabled at the same time");

use std::fmt::{Display, Formatter};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;

use chrono::{DateTime, FixedOffset, Utc};

#[cfg(feature = "croner")]
pub use croner::Cron;
#[cfg(feature = "croner")]
/// Cron expression type selected by the `croner` feature.
pub type CronSchedule = Cron;
#[cfg(feature = "croner")]
/// Legacy recommended polling duration in milliseconds.
pub const MIN_DURATION: u64 = 1_000;

#[cfg(feature = "cron")]
pub use cron::Schedule;
#[cfg(feature = "cron")]
/// Cron expression type selected by the default `cron` feature.
pub type CronSchedule = Schedule;
#[cfg(feature = "cron")]
/// Legacy recommended polling duration in milliseconds.
pub const MIN_DURATION: u64 = 500;

pub use uuid::Uuid;

mod runner;
mod schedule;
#[cfg(feature = "tokio")]
mod tokio_runner;

pub use runner::{SchedulerHandle, ThreadRunner};
pub use schedule::{
    CalendarInterval, CalendarIntervalBuilder, Interval, JobSchedule, MisfirePolicy, ScheduleError,
    SchedulerTimeZone,
};
#[cfg(feature = "tokio")]
pub use tokio_runner::{AsyncJob, AsyncJobScheduler, TokioRunner, TokioSchedulerHandle};

use schedule::{ClockSnapshot, ScheduleCursor};

/// A thread-safe cooperative cancellation flag.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    observed: Vec<Arc<AtomicBool>>,
}

impl CancellationToken {
    /// Signal cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Return whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .observed
                .iter()
                .any(|flag| flag.load(Ordering::Acquire))
    }

    fn linked_to(&self, other: &Self) -> Self {
        let mut observed = self.observed.clone();
        observed.push(Arc::clone(&other.cancelled));
        observed.extend(other.observed.iter().cloned());
        Self {
            cancelled: Arc::clone(&self.cancelled),
            observed,
        }
    }
}

/// Metadata made available to context-aware callbacks.
#[derive(Clone, Debug)]
pub struct JobContext {
    job_id: Uuid,
    name: Option<String>,
    scheduled_at: DateTime<Utc>,
    cancellation: CancellationToken,
}

impl JobContext {
    /// Stable UUID assigned to the job.
    #[must_use]
    pub const fn job_id(&self) -> Uuid {
        self.job_id
    }

    /// Optional human-readable name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// UTC instant represented by this invocation.
    #[must_use]
    pub const fn scheduled_at(&self) -> DateTime<Utc> {
        self.scheduled_at
    }

    /// Cooperative cancellation token shared with the runner.
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

/// Common metadata attached to scheduler events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobEvent {
    /// Stable UUID assigned to the job.
    pub job_id: Uuid,
    /// Optional unique job name.
    pub name: Option<String>,
    /// Scheduled UTC instant, when applicable.
    pub scheduled_at: Option<DateTime<Utc>>,
}

/// Notifications emitted by background runners.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerEvent {
    /// A callback is about to start.
    JobStarted(JobEvent),
    /// A callback returned normally.
    JobFinished(JobEvent),
    /// A callback panicked; the runner remains alive.
    JobPanicked(JobEvent),
    /// A job was paused.
    JobPaused(JobEvent),
    /// A job was resumed.
    JobResumed(JobEvent),
    /// A job was assigned a new schedule.
    JobRescheduled(JobEvent),
    /// A job was removed.
    JobRemoved(JobEvent),
    /// The runner stopped.
    RunnerStopped,
}

/// Thread-safe event callback used by both background runners.
pub type EventSink = Arc<dyn Fn(SchedulerEvent) + Send + Sync + 'static>;

/// Errors returned by scheduler control operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerError {
    /// A name became empty after trimming surrounding whitespace.
    EmptyName,
    /// A named job already exists in this scheduler.
    DuplicateName(String),
    /// The requested UUID or name does not exist.
    JobNotFound,
    /// The background runner is no longer accepting commands.
    RunnerStopped,
    /// The scheduler thread panicked while shutting down.
    RunnerPanicked,
    /// A schedule is invalid.
    Schedule(ScheduleError),
}

impl Display for SchedulerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("job names cannot be empty"),
            Self::DuplicateName(name) => write!(formatter, "a job named \"{name}\" already exists"),
            Self::JobNotFound => formatter.write_str("the requested job was not found"),
            Self::RunnerStopped => formatter.write_str("the scheduler runner has stopped"),
            Self::RunnerPanicked => formatter.write_str("the scheduler runner panicked"),
            Self::Schedule(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for SchedulerError {}

impl From<ScheduleError> for SchedulerError {
    fn from(value: ScheduleError) -> Self {
        Self::Schedule(value)
    }
}

enum JobCallback<'a> {
    Simple(Box<dyn FnMut() + Send + 'a>),
    Context(Box<dyn FnMut(&JobContext) + Send + 'a>),
}

impl JobCallback<'_> {
    fn call(&mut self, context: &JobContext) {
        match self {
            Self::Simple(callback) => callback(),
            Self::Context(callback) => callback(context),
        }
    }
}

/// A synchronous schedulable job.
#[allow(clippy::struct_field_names)]
pub struct Job<'a> {
    cursor: ScheduleCursor,
    callback: JobCallback<'a>,
    missed_limit: usize,
    job_id: Uuid,
    name: Option<String>,
    time_zone: SchedulerTimeZone,
    paused: bool,
    cancellation: CancellationToken,
    runner_cancellation: Option<CancellationToken>,
}

#[expect(
    clippy::missing_errors_doc,
    reason = "fallible methods return the documented scheduler error types"
)]
impl<'a> Job<'a> {
    /// Create a cron job while preserving the original crate API.
    pub fn new<T>(cron: CronSchedule, run: T) -> Self
    where
        T: FnMut() + Send + 'a,
    {
        Self::from_schedule(JobSchedule::from(cron), run)
    }

    /// Create a job from any supported schedule.
    pub fn from_schedule<T>(schedule: JobSchedule, run: T) -> Self
    where
        T: FnMut() + Send + 'a,
    {
        Self::from_callback(schedule, JobCallback::Simple(Box::new(run)))
    }

    /// Create a context-aware job that can observe cooperative cancellation.
    pub fn from_schedule_with_context<T>(schedule: JobSchedule, run: T) -> Self
    where
        T: FnMut(&JobContext) + Send + 'a,
    {
        Self::from_callback(schedule, JobCallback::Context(Box::new(run)))
    }

    /// Create a one-shot job at a future UTC instant.
    pub fn once<T>(at: DateTime<Utc>, run: T) -> Result<Self, ScheduleError>
    where
        T: FnMut() + Send + 'a,
    {
        validate_schedule(&JobSchedule::Once(at))?;
        Ok(Self::from_schedule(JobSchedule::Once(at), run))
    }

    /// Create a job whose delay starts after callback completion.
    pub fn fixed_delay<T>(interval: Interval, run: T) -> Self
    where
        T: FnMut() + Send + 'a,
    {
        Self::from_schedule(JobSchedule::FixedDelay(interval), run)
    }

    /// Create a job anchored to a fixed cadence.
    pub fn fixed_rate<T>(interval: Interval, run: T) -> Self
    where
        T: FnMut() + Send + 'a,
    {
        Self::from_schedule(JobSchedule::FixedRate(interval), run)
    }

    fn from_callback(schedule: JobSchedule, callback: JobCallback<'a>) -> Self {
        Self {
            cursor: ScheduleCursor::new(schedule),
            callback,
            missed_limit: 1,
            job_id: Uuid::new_v4(),
            name: None,
            time_zone: SchedulerTimeZone::Utc,
            paused: false,
            cancellation: CancellationToken::default(),
            runner_cancellation: None,
        }
    }

    /// Stable UUID assigned when the job is constructed.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.job_id
    }

    /// Optional scheduler-unique name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Set the limit for missed runs. Zero keeps the legacy unlimited behavior,
    /// processed in bounded batches.
    pub fn limit_missed_runs(&mut self, limit: usize) {
        self.missed_limit = limit;
    }

    /// Configure the safer misfire policy API.
    pub fn set_misfire_policy(&mut self, policy: MisfirePolicy) {
        self.missed_limit = match policy {
            MisfirePolicy::Skip => 1,
            MisfirePolicy::CatchUp { max_runs } => max_runs.max(1),
        };
    }

    /// Set the cron cursor used to replay or skip earlier occurrences.
    pub fn last_tick(&mut self, last_tick: Option<DateTime<FixedOffset>>) {
        self.cursor
            .set_cron_after(last_tick.map(|value| value.with_timezone(&Utc)));
    }

    fn set_name(&mut self, name: String) {
        self.name = Some(name);
    }

    fn event(&self, scheduled_at: Option<DateTime<Utc>>) -> JobEvent {
        JobEvent {
            job_id: self.job_id,
            name: self.name.clone(),
            scheduled_at,
        }
    }

    fn context(&self, scheduled_at: DateTime<Utc>) -> JobContext {
        let cancellation = self.runner_cancellation.as_ref().map_or_else(
            || self.cancellation.clone(),
            |runner| self.cancellation.linked_to(runner),
        );
        JobContext {
            job_id: self.job_id,
            name: self.name.clone(),
            scheduled_at,
            cancellation,
        }
    }

    fn tick(&mut self, catch_panics: bool, sink: Option<&EventSink>) {
        if self.paused {
            return;
        }
        let due = self
            .cursor
            .due(ClockSnapshot::now(), self.time_zone, self.missed_limit);
        for scheduled_at in due {
            self.execute(scheduled_at, catch_panics, sink);
        }
    }

    fn execute(
        &mut self,
        scheduled_at: DateTime<Utc>,
        catch_panics: bool,
        sink: Option<&EventSink>,
    ) {
        let event = self.event(Some(scheduled_at));
        emit(sink, SchedulerEvent::JobStarted(event.clone()));
        let context = self.context(scheduled_at);

        let succeeded = if catch_panics {
            catch_unwind(AssertUnwindSafe(|| self.callback.call(&context))).is_ok()
        } else {
            self.callback.call(&context);
            true
        };

        let _result = self
            .cursor
            .completed_at(ClockSnapshot::now(), self.time_zone);
        if succeeded {
            emit(sink, SchedulerEvent::JobFinished(event));
        } else {
            emit(sink, SchedulerEvent::JobPanicked(event));
        }
    }

    fn next_in(&self, now: ClockSnapshot) -> Option<StdDuration> {
        if self.paused {
            return None;
        }
        self.cursor.next_in(now, self.time_zone)
    }

    fn pause(&mut self) -> bool {
        if self.paused {
            return false;
        }
        self.paused = true;
        true
    }

    fn resume(&mut self) -> bool {
        if !self.paused {
            return false;
        }
        self.paused = false;
        self.cursor.resume(ClockSnapshot::now(), self.time_zone);
        true
    }

    fn reschedule(&mut self, schedule: JobSchedule) -> Result<(), ScheduleError> {
        validate_schedule(&schedule)?;
        self.cursor.replace(schedule);
        Ok(())
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

/// Collection and manual driver for synchronous jobs.
pub struct JobScheduler<'a> {
    jobs: Vec<Job<'a>>,
    time_zone: SchedulerTimeZone,
    runner_cancellation: Option<CancellationToken>,
}

impl Default for JobScheduler<'_> {
    fn default() -> Self {
        Self::new()
    }
}

#[expect(
    clippy::missing_errors_doc,
    reason = "control methods return the documented SchedulerError type"
)]
impl<'a> JobScheduler<'a> {
    /// Construct an empty UTC scheduler.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            jobs: Vec::new(),
            time_zone: SchedulerTimeZone::Utc,
            runner_cancellation: None,
        }
    }

    /// Preserve the original fixed-offset API.
    pub fn set_timezone(&mut self, time_zone: FixedOffset) {
        self.set_time_zone(SchedulerTimeZone::Fixed(time_zone));
    }

    /// Evaluate schedules in UTC.
    pub fn use_utc(&mut self) {
        self.set_time_zone(SchedulerTimeZone::Utc);
    }

    /// Evaluate schedules using the operating system's local time zone.
    pub fn use_system_timezone(&mut self) {
        self.set_time_zone(SchedulerTimeZone::Local);
    }

    /// Evaluate schedules in a named IANA time zone.
    #[cfg(feature = "timezones")]
    pub fn set_named_timezone(&mut self, time_zone: chrono_tz::Tz) {
        self.set_time_zone(SchedulerTimeZone::Named(time_zone));
    }

    /// Current scheduler time zone.
    #[must_use]
    pub const fn time_zone(&self) -> SchedulerTimeZone {
        self.time_zone
    }

    fn set_time_zone(&mut self, time_zone: SchedulerTimeZone) {
        self.time_zone = time_zone;
        let now = ClockSnapshot::now();
        for job in &mut self.jobs {
            job.time_zone = time_zone;
            job.cursor.resume(now, time_zone);
        }
    }

    /// Add an unnamed job and return its UUID.
    pub fn add(&mut self, mut job: Job<'a>) -> Uuid {
        let job_id = job.job_id;
        job.time_zone = self.time_zone;
        job.runner_cancellation
            .clone_from(&self.runner_cancellation);
        self.jobs.push(job);
        job_id
    }

    pub(crate) fn set_runner_cancellation(&mut self, cancellation: CancellationToken) {
        for job in &mut self.jobs {
            job.runner_cancellation = Some(cancellation.clone());
        }
        self.runner_cancellation = Some(cancellation);
    }

    /// Add a uniquely named job.
    pub fn add_named(
        &mut self,
        name: impl Into<String>,
        mut job: Job<'a>,
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
        job.set_name(name);
        Ok(self.add(job))
    }

    /// Remove a job by UUID.
    pub fn remove(&mut self, job_id: Uuid) -> bool {
        self.remove_index(self.index_by_id(job_id)).is_some()
    }

    /// Remove a job by name.
    pub fn remove_named(&mut self, name: &str) -> bool {
        self.remove_index(self.index_by_name(name)).is_some()
    }

    /// Pause a job by UUID.
    pub fn pause(&mut self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.job_mut(job_id).map(Job::pause)
    }

    /// Pause a job by name.
    pub fn pause_named(&mut self, name: &str) -> Result<bool, SchedulerError> {
        self.job_mut_named(name).map(Job::pause)
    }

    /// Resume a job by UUID. Time spent paused is intentionally skipped.
    pub fn resume(&mut self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.job_mut(job_id).map(Job::resume)
    }

    /// Resume a job by name.
    pub fn resume_named(&mut self, name: &str) -> Result<bool, SchedulerError> {
        self.job_mut_named(name).map(Job::resume)
    }

    /// Replace a job's schedule by UUID.
    pub fn reschedule(
        &mut self,
        job_id: Uuid,
        schedule: JobSchedule,
    ) -> Result<(), SchedulerError> {
        self.job_mut(job_id)?.reschedule(schedule)?;
        Ok(())
    }

    /// Replace a job's schedule by name.
    pub fn reschedule_named(
        &mut self,
        name: &str,
        schedule: JobSchedule,
    ) -> Result<(), SchedulerError> {
        self.job_mut_named(name)?.reschedule(schedule)?;
        Ok(())
    }

    /// Invoke a job immediately by UUID.
    pub fn run_now(&mut self, job_id: Uuid) -> Result<(), SchedulerError> {
        self.job_mut(job_id)?.execute(Utc::now(), false, None);
        Ok(())
    }

    /// Invoke a job immediately by name.
    pub fn run_now_named(&mut self, name: &str) -> Result<(), SchedulerError> {
        self.job_mut_named(name)?.execute(Utc::now(), false, None);
        Ok(())
    }

    /// Execute all due jobs on the caller's thread.
    pub fn tick(&mut self) {
        for job in &mut self.jobs {
            job.tick(false, None);
        }
    }

    /// Return the duration until the next job, falling back to [`MIN_DURATION`]
    /// when the scheduler is empty.
    #[must_use]
    pub fn time_till_next_job(&self) -> StdDuration {
        self.next_wake_in()
            .unwrap_or_else(|| StdDuration::from_millis(MIN_DURATION))
    }

    pub(crate) fn tick_catching(&mut self, sink: Option<&EventSink>) {
        for job in &mut self.jobs {
            job.tick(true, sink);
        }
    }

    pub(crate) fn run_now_catching(
        &mut self,
        job_id: Uuid,
        sink: Option<&EventSink>,
    ) -> Result<JobEvent, SchedulerError> {
        let job = self.job_mut(job_id)?;
        let event = job.event(Some(Utc::now()));
        job.execute(Utc::now(), true, sink);
        Ok(event)
    }

    pub(crate) fn next_wake_in(&self) -> Option<StdDuration> {
        let now = ClockSnapshot::now();
        self.jobs.iter().filter_map(|job| job.next_in(now)).min()
    }

    pub(crate) fn event_for(&self, job_id: Uuid) -> Result<JobEvent, SchedulerError> {
        self.jobs
            .iter()
            .find(|job| job.job_id == job_id)
            .map(|job| job.event(None))
            .ok_or(SchedulerError::JobNotFound)
    }

    pub(crate) fn id_by_name(&self, name: &str) -> Result<Uuid, SchedulerError> {
        self.jobs
            .iter()
            .find(|job| job.name.as_deref() == Some(name))
            .map(|job| job.job_id)
            .ok_or(SchedulerError::JobNotFound)
    }

    pub(crate) fn cancel_all(&self) {
        for job in &self.jobs {
            job.cancel();
        }
    }

    fn remove_index(&mut self, index: Option<usize>) -> Option<Job<'a>> {
        index.map(|index| self.jobs.remove(index))
    }

    fn index_by_id(&self, job_id: Uuid) -> Option<usize> {
        self.jobs.iter().position(|job| job.job_id == job_id)
    }

    fn index_by_name(&self, name: &str) -> Option<usize> {
        self.jobs
            .iter()
            .position(|job| job.name.as_deref() == Some(name))
    }

    fn job_mut(&mut self, job_id: Uuid) -> Result<&mut Job<'a>, SchedulerError> {
        self.jobs
            .iter_mut()
            .find(|job| job.job_id == job_id)
            .ok_or(SchedulerError::JobNotFound)
    }

    fn job_mut_named(&mut self, name: &str) -> Result<&mut Job<'a>, SchedulerError> {
        self.jobs
            .iter_mut()
            .find(|job| job.name.as_deref() == Some(name))
            .ok_or(SchedulerError::JobNotFound)
    }
}

fn validate_schedule(schedule: &JobSchedule) -> Result<(), ScheduleError> {
    match schedule {
        JobSchedule::Once(at) if *at <= Utc::now() => Err(ScheduleError::PastOneShot),
        _ => Ok(()),
    }
}

fn normalize_name(name: &str) -> Result<String, SchedulerError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(SchedulerError::EmptyName);
    }
    Ok(name)
}

pub(crate) fn emit(sink: Option<&EventSink>, event: SchedulerEvent) {
    if let Some(sink) = sink {
        let _result = catch_unwind(AssertUnwindSafe(|| sink(event)));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn one_shot_rejects_past_instant() {
        let result = Job::once(Utc::now() - chrono::Duration::seconds(1), || {});
        assert_eq!(result.err(), Some(ScheduleError::PastOneShot));
    }

    #[test]
    fn named_jobs_are_trimmed_and_unique() {
        let mut scheduler = JobScheduler::new();
        let first = Job::fixed_rate(Interval::seconds(5).unwrap(), || {});
        scheduler.add_named(" hourly ", first).unwrap();

        let duplicate = Job::fixed_rate(Interval::seconds(5).unwrap(), || {});
        assert_eq!(
            scheduler.add_named("hourly", duplicate),
            Err(SchedulerError::DuplicateName(String::from("hourly")))
        );
    }

    #[test]
    fn run_now_supports_name_and_uuid() {
        let count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&count);
        let mut scheduler = JobScheduler::new();
        let id = scheduler
            .add_named(
                "manual",
                Job::fixed_delay(Interval::hours(1).unwrap(), move || {
                    callback_count.fetch_add(1, Ordering::Relaxed);
                }),
            )
            .unwrap();

        scheduler.run_now(id).unwrap();
        scheduler.run_now_named("manual").unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pause_and_resume_are_idempotent() {
        let mut scheduler = JobScheduler::new();
        let id = scheduler.add(Job::fixed_rate(Interval::seconds(1).unwrap(), || {}));

        assert!(scheduler.pause(id).unwrap());
        assert!(!scheduler.pause(id).unwrap());
        assert!(scheduler.resume(id).unwrap());
        assert!(!scheduler.resume(id).unwrap());
    }

    #[test]
    fn cancellation_token_is_shared_with_context() {
        let observed = Arc::new(AtomicBool::new(false));
        let callback_observed = Arc::clone(&observed);
        let job = Job::from_schedule_with_context(
            JobSchedule::FixedRate(Interval::hours(1).unwrap()),
            move |context| {
                callback_observed
                    .store(context.cancellation_token().is_cancelled(), Ordering::Relaxed);
            },
        );
        let id = job.id();
        let mut scheduler = JobScheduler::new();
        scheduler.add(job);
        scheduler.cancel_all();
        scheduler.run_now(id).unwrap();

        assert!(observed.load(Ordering::Relaxed));
    }
}
