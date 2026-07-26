//! # `JobScheduler`
//!
//! A simple cron-like job scheduling library for Rust.
//!
//! ## Usage
//!
//! Be sure to add the `job_scheduler_ng` crate to your `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! job_scheduler_ng = "2"
//! ```
//!
//! Creating a schedule for a job is done using the `FromStr` impl for the
//! `Schedule` type of the [cron](https://github.com/zslayton/cron) library.
//! Or
//! `Cron` type of the [croner](https://github.com/hexagon/croner-rust) library.
//!
//! See the documentation of the feature you use on which scheduling format is supported.
//!
//! The scheduling format is as follows:
//!
//! ```text
//! sec   min   hour   day of month   month   day of week   year
//! *     *     *      *              *       *             *
//! ```
//!
//! Note that the year may be omitted.
//!
//! Comma separated values such as `5,8,10` represent more than one time
//! value. So for example, a schedule of `0 2,14,26 * * * *` would execute
//! on the 2nd, 14th, and 26th minute of every hour.
//!
//! Ranges can be specified with a dash. A schedule of `0 0 * 5-10 * *`
//! would execute once per hour but only on day 5 through 10 of the month.
//!
//!
//! ## Features
//!
//! - `cron` (default): Uses the `Schedule` struct from the [cron](https://github.com/zslayton/cron) library.
//! - `croner`: Uses the `Cron` struct from the [croner](https://github.com/hexagon/croner-rust) library.
//!
//! Only one of these features can be enabled at the same time.
//! To use croner, disable the default features:
//!
//! ```toml
//! [dependencies]
//! job_scheduler_ng = { version = "2", default-features = false, features = ["croner"] }
//! ```
//!
//!
//! ## Example
//!
//! A simple usage example:
//!
//! ```no_run
//! use job_scheduler_ng::{JobScheduler, Job, MIN_DURATION};
//! use core::time::Duration;
//!
//! let mut sched = JobScheduler::new();
//!
//! sched.add(Job::new("0/10 * * * * *".parse().expect("Valid schedule"), || {
//!     println!("I get executed every 10th second!");
//! }));
//!
//! sched.add(Job::new("*/4 * * * * *".parse().expect("Valid schedule"), || {
//!     println!("I get executed every 4 seconds!");
//! }));
//!
//! loop {
//!     sched.tick();
//!     std::thread::sleep(Duration::from_millis(MIN_DURATION));
//! }
//! ```
//!
//! Setting a custom timezone other than the default UTC
//! Any `Tz::Offset` provided by chrono will work.
//!
//! ```no_run
//! use chrono::Local;
//! use job_scheduler_ng::{JobScheduler, Job, MIN_DURATION};
//! use core::time::Duration;
//!
//! let mut sched = JobScheduler::new();
//! let local_tz = Local::now();
//! sched.set_timezone(*local_tz.offset());
//!
//! sched.add(Job::new("0 5 13 * * *".parse().expect("Valid schedule"), || {
//!     println!("I get executed every day 13:05 local time!");
//! }));
//!
//! loop {
//!     sched.tick();
//!     std::thread::sleep(Duration::from_millis(MIN_DURATION));
//! }
//! ```
//!
//!
//! ## How jobs are executed
//!
//! Jobs are executed one after the other, on the thread which calls `tick()`, in the order they were added.
//! A long running job delays all jobs after it. Spawn a thread or use a channel if this is an issue,
//! see the `simple_job_thread` and `simple_job_mpsc` examples.
//!
//! The first call to `tick()` only stores the current time and does not run any jobs.
//! Every scheduled run is executed at most once. If a job panics, that run is not retried.
//! If the system clock is set backwards, jobs could run twice once the clock catches up again.
//!

#[cfg(not(any(feature = "cron", feature = "croner")))]
compile_error!("either feature \"cron\" or feature \"croner\" must be enabled");

#[cfg(all(feature = "cron", feature = "croner"))]
compile_error!("feature \"cron\" and feature \"croner\" cannot be enabled at the same time");

use chrono::{DateTime, Duration, FixedOffset, Utc};
pub use uuid::Uuid;

// Feature-gated imports and type aliases
#[cfg(feature = "croner")]
pub use croner::Cron;
#[cfg(feature = "croner")]
type CronSchedule = Cron;
#[cfg(feature = "croner")]
/// Recommended minimum sleep between `tick()` calls, in milliseconds.
/// `500` for the `cron` backend, `1_000` for `croner`.
pub const MIN_DURATION: u64 = 1_000;

#[cfg(feature = "cron")]
pub use cron::Schedule;
#[cfg(feature = "cron")]
type CronSchedule = Schedule;
#[cfg(feature = "cron")]
/// Recommended minimum sleep between `tick()` calls, in milliseconds.
/// `500` for the `cron` backend, `1_000` for `croner`.
pub const MIN_DURATION: u64 = 500;

const MAX_MISSED_PER_TICK: usize = 1_000;

#[cfg(feature = "croner")]
#[inline]
fn item_after(
    cron: &CronSchedule,
    after: DateTime<FixedOffset>,
) -> impl Iterator<Item = DateTime<FixedOffset>> + '_ {
    cron.iter_after(after)
}

#[cfg(feature = "cron")]
#[inline]
fn item_after(
    cron: &CronSchedule,
    after: DateTime<FixedOffset>,
) -> impl Iterator<Item = DateTime<FixedOffset>> + '_ {
    cron.after(&after)
}

/// A schedulable `Job`.
pub struct Job<'a> {
    cron: CronSchedule,
    run: Box<dyn FnMut() + Send + 'a>,
    last_tick: Option<DateTime<FixedOffset>>,
    limit_missed_runs: usize,
    #[allow(clippy::struct_field_names)]
    job_id: Uuid,
    tz: FixedOffset,
}

impl<'a> Job<'a> {
    /// Create a new job.
    ///
    /// Either with `cron`
    /// ```rust
    /// # #[cfg(feature = "cron")] {
    /// use job_scheduler_ng::{Schedule, Job};
    /// // Run at second 0 of the 15th minute of the 6th, 8th, and 10th hour
    /// // of any day in March and June that is a Friday of the year 2025.
    /// let s: Schedule = "0 15 6,8,10 * Mar,Jun Fri 2025".parse().expect("Valid schedule");
    /// Job::new(s, || println!("I have a complex schedule...") );
    /// # }
    /// ```
    ///
    /// Or `croner`
    /// ```rust
    /// # #[cfg(feature = "croner")] {
    /// use job_scheduler_ng::{Cron, Job};
    /// // Run at second 0 of the 20th minute of the 5th, 7th, and 9th hour,
    /// // only on the Last day of the month (Which is what the `L` does)
    /// // of any day in March and June that is a Friday of the year 2025.
    /// let s: Cron = "0 20 5,7,9 L Mar,Jun Fri 2025".parse().expect("Valid schedule");
    /// Job::new(s, || println!("I have a complex schedule...") );
    /// # }
    /// ```
    #[inline]
    #[expect(clippy::missing_panics_doc, reason = "infallible")]
    pub fn new<T>(cron: CronSchedule, run: T) -> Self
    where
        T: FnMut() + Send + 'a,
    {
        Self {
            cron,
            run: Box::new(run),
            last_tick: None,
            limit_missed_runs: 1,
            job_id: Uuid::new_v4(),
            tz: FixedOffset::east_opt(0).expect("Valid FixedOffset for UTC"),
        }
    }

    fn tick(&mut self) {
        let now = Utc::now().with_timezone(&self.tz);
        let Some(last_tick) = self.last_tick else {
            self.last_tick = Some(now);
            return;
        };

        let limit = if self.limit_missed_runs == 0 {
            MAX_MISSED_PER_TICK
        } else {
            self.limit_missed_runs.min(MAX_MISSED_PER_TICK)
        };

        let mut executed = 0;
        for event in item_after(&self.cron, last_tick).take(limit) {
            if event > now {
                break;
            }
            // Set the last_tick to this event before running it.
            // If the callback panics the last_tick would be lost, and would re-fire/replay
            self.last_tick = Some(event);
            (self.run)();
            executed += 1;
        }

        // Move last_tick to now.
        // One exception: in unlimited mode, hitting MAX_MISSED_PER_TICK means there may be missed runs left.
        // Keep last_tick at the last executed event so the next tick continues where this one stopped.
        if self.limit_missed_runs != 0 || executed < limit {
            self.last_tick = Some(now);
        }
    }

    /// Set the limit for missed runs in the case of delayed ticks. Setting to 0 means unlimited.
    ///
    /// At most `1_000` missed runs are executed per `tick()` call, so one job cannot
    /// starve the others. With a limit of 0 the remainder carries over to the next
    /// tick until caught up; with an explicit limit, runs beyond the limit are skipped.
    ///
    /// ```rust
    /// use job_scheduler_ng::Job;
    /// let mut job = Job::new("0/1 * * * * *".parse().expect("Valid schedule"), || {
    ///     println!("I get executed every 1 seconds!");
    /// });
    /// job.limit_missed_runs(99);
    /// ```
    #[inline]
    pub fn limit_missed_runs(&mut self, limit: usize) {
        self.limit_missed_runs = limit;
    }

    /// Set last tick to force re-running of missed runs.
    ///
    /// Be careful setting this far into the past in combination with `limit_missed_runs(0)`,
    /// as every missed run since then will be executed (in batches of `1_000` per tick).
    ///
    /// ```rust
    /// use job_scheduler_ng::Job;
    /// use chrono::{Local, Utc};
    /// let mut job = Job::new("0/1 * * * * *".parse().expect("Valid schedule"), || {
    ///     println!("I get executed every 1 seconds!");
    /// });
    /// let local_tz = Local::now();
    /// job.last_tick(Some(Utc::now().with_timezone(local_tz.offset())));
    /// ```
    #[inline]
    pub fn last_tick(&mut self, last_tick: Option<DateTime<FixedOffset>>) {
        self.last_tick = last_tick;
    }
}

/// The `JobScheduler` contains and executes the scheduled jobs.
pub struct JobScheduler<'a> {
    jobs: Vec<Job<'a>>,
    tz: FixedOffset,
}

impl Default for JobScheduler<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> JobScheduler<'a> {
    /// Create a new `JobScheduler`.
    #[inline]
    #[must_use]
    #[expect(clippy::missing_panics_doc, reason = "infallible")]
    pub const fn new() -> Self {
        Self {
            jobs: Vec::new(),
            tz: FixedOffset::east_opt(0).expect("Valid FixedOffset for UTC"),
        }
    }

    /// Set a custom timezone other than the default UTC.
    ///
    /// ```rust
    /// use job_scheduler_ng::{JobScheduler, Job};
    /// use chrono::Local;
    /// let mut sched = JobScheduler::new();
    /// let local_tz = Local::now();
    /// sched.set_timezone(*local_tz.offset());
    /// sched.add(Job::new("0 5 13 * * *".parse().expect("Valid schedule"), || {
    ///     println!("I get executed every day 13:05 local time!");
    /// }));
    /// ```
    pub fn set_timezone(&mut self, tz: FixedOffset) {
        self.tz = tz;
    }

    /// Add a job to the `JobScheduler`
    /// Returns a UUID which can be used with `remove()` to remove this specific job.
    ///
    /// ```rust
    /// use job_scheduler_ng::{JobScheduler, Job};
    /// let mut sched = JobScheduler::new();
    /// sched.add(Job::new("1/10 * * * * *".parse().expect("Valid schedule"), || {
    ///     println!("I get executed every 10 seconds!");
    /// }));
    /// ```
    #[inline]
    pub fn add(&mut self, job: Job<'a>) -> Uuid {
        let job_id = job.job_id;

        let mut job = job;
        job.tz = self.tz;
        self.jobs.push(job);

        job_id
    }

    /// Remove a job from the `JobScheduler` using the UUID returned by `add()`.
    /// Will return a boolean true if the UUID is found and removed, false otherwise.
    ///
    /// ```rust
    /// use job_scheduler_ng::{JobScheduler, Job};
    /// let mut sched = JobScheduler::new();
    /// let job_id = sched.add(Job::new("1/10 * * * * *".parse().expect("Valid schedule"), || {
    ///     println!("I get executed every 10 seconds!");
    /// }));
    /// sched.remove(job_id);
    /// ```
    #[inline]
    pub fn remove(&mut self, job_id: Uuid) -> bool {
        let mut found_index = None;
        for (i, job) in self.jobs.iter().enumerate() {
            if job.job_id == job_id {
                found_index = Some(i);
                break;
            }
        }

        if let Some(index) = found_index {
            self.jobs.remove(index);
        }

        found_index.is_some()
    }

    /// The `tick` method increments time for the `JobScheduler` and executes any pending jobs.
    /// It is recommended to use `MIN_DURATION` for sleep as milliseconds between invocations of this method.
    ///
    /// ```no_run
    /// use job_scheduler_ng::{JobScheduler, MIN_DURATION};
    /// use core::time::Duration;
    /// let mut sched = JobScheduler::new();
    /// loop {
    ///     sched.tick();
    ///     std::thread::sleep(Duration::from_millis(MIN_DURATION));
    /// }
    /// ```
    #[inline]
    pub fn tick(&mut self) {
        for job in &mut self.jobs {
            job.tick();
        }
    }

    #[inline]
    fn get_next_job_durations(
        &'a self,
        now: &'a DateTime<FixedOffset>,
    ) -> impl Iterator<Item = Duration> + 'a {
        self.jobs
            .iter()
            .filter_map(move |job| item_after(&job.cron, *now).next().map(|event| event - now))
    }

    /// The `time_till_next_job` method returns the duration till the next job is supposed to run.
    /// This can be used to sleep until then without waking up at a fixed interval.
    /// If no jobs are available, or all are in the past, it will fallback to `MIN_DURATION`
    ///
    /// ```no_run
    /// use job_scheduler_ng::JobScheduler;
    /// let mut sched = JobScheduler::new();
    /// loop {
    ///     sched.tick();
    ///     std::thread::sleep(sched.time_till_next_job());
    /// }
    /// ```
    #[inline]
    #[must_use]
    pub fn time_till_next_job(&self) -> core::time::Duration {
        if self.jobs.is_empty() {
            return core::time::Duration::from_millis(MIN_DURATION);
        }
        let tz = self.tz;
        let now = Utc::now().with_timezone(&tz);

        let duration = self
            .get_next_job_durations(&now)
            .min()
            .unwrap_or_else(|| Duration::milliseconds(MIN_DURATION.cast_signed()));

        duration
            .to_std()
            .unwrap_or(core::time::Duration::from_millis(MIN_DURATION))
    }
}
