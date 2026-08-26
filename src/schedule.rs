use std::fmt::{Display, Formatter};
use std::time::{Duration as StdDuration, Instant};

use chrono::{
    DateTime, Datelike, Days, Duration, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime,
    TimeZone, Utc,
};

use crate::CronSchedule;

const MAX_DUE_PER_TICK: usize = 1_000;
const MAX_CALENDAR_SEARCH: usize = 100_000;

/// Errors returned while constructing or updating schedules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleError {
    /// An interval must contain at least one non-zero component.
    ZeroInterval,
    /// The requested interval cannot be represented safely.
    IntervalOverflow,
    /// A calendar calculation produced a date outside chrono's supported range.
    CalendarOutOfRange,
    /// A newly-created one-shot schedule must be in the future.
    PastOneShot,
}

impl Display for ScheduleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroInterval => formatter.write_str("an interval must be greater than zero"),
            Self::IntervalOverflow => formatter.write_str("the interval is too large"),
            Self::CalendarOutOfRange => formatter.write_str("the calendar result is out of range"),
            Self::PastOneShot => formatter.write_str("a one-shot schedule must be in the future"),
        }
    }
}

impl std::error::Error for ScheduleError {}

/// A fixed monotonic duration or a calendar-aware interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Interval(IntervalKind);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntervalKind {
    Fixed(StdDuration),
    Calendar(CalendarInterval),
}

#[expect(
    clippy::missing_errors_doc,
    reason = "all constructors return ScheduleError for zero or overflow"
)]
impl Interval {
    /// Construct a checked fixed duration.
    pub fn fixed(duration: StdDuration) -> Result<Self, ScheduleError> {
        if duration.is_zero() {
            return Err(ScheduleError::ZeroInterval);
        }
        Ok(Self(IntervalKind::Fixed(duration)))
    }

    /// Construct a fixed interval from seconds.
    pub fn seconds(value: u64) -> Result<Self, ScheduleError> {
        Self::fixed(StdDuration::from_secs(value))
    }

    /// Construct a fixed interval from minutes.
    pub fn minutes(value: u64) -> Result<Self, ScheduleError> {
        Self::scaled_seconds(value, 60)
    }

    /// Construct a fixed interval from hours.
    pub fn hours(value: u64) -> Result<Self, ScheduleError> {
        Self::scaled_seconds(value, 60 * 60)
    }

    /// Construct a fixed interval from 24-hour days.
    pub fn days(value: u64) -> Result<Self, ScheduleError> {
        Self::scaled_seconds(value, 24 * 60 * 60)
    }

    fn scaled_seconds(value: u64, scale: u64) -> Result<Self, ScheduleError> {
        value
            .checked_mul(scale)
            .map(StdDuration::from_secs)
            .ok_or(ScheduleError::IntervalOverflow)
            .and_then(Self::fixed)
    }
}

impl From<CalendarInterval> for Interval {
    fn from(value: CalendarInterval) -> Self {
        Self(IntervalKind::Calendar(value))
    }
}

/// A combination of calendar units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalendarInterval {
    years: u32,
    months: u32,
    days: u32,
    hours: u32,
    minutes: u32,
    seconds: u32,
}

impl CalendarInterval {
    /// Start building a calendar interval.
    #[must_use]
    pub const fn builder() -> CalendarIntervalBuilder {
        CalendarIntervalBuilder::new()
    }

    fn is_zero(self) -> bool {
        self.years == 0
            && self.months == 0
            && self.days == 0
            && self.hours == 0
            && self.minutes == 0
            && self.seconds == 0
    }
}

/// Builder for [`CalendarInterval`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalendarIntervalBuilder {
    interval: CalendarInterval,
}

impl Default for CalendarIntervalBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[expect(
    clippy::missing_errors_doc,
    reason = "build returns ScheduleError when every component is zero"
)]
impl CalendarIntervalBuilder {
    /// Construct an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interval: CalendarInterval {
                years: 0,
                months: 0,
                days: 0,
                hours: 0,
                minutes: 0,
                seconds: 0,
            },
        }
    }

    /// Set the year component.
    #[must_use]
    pub const fn years(mut self, value: u32) -> Self {
        self.interval.years = value;
        self
    }

    /// Set the month component.
    #[must_use]
    pub const fn months(mut self, value: u32) -> Self {
        self.interval.months = value;
        self
    }

    /// Set the day component.
    #[must_use]
    pub const fn days(mut self, value: u32) -> Self {
        self.interval.days = value;
        self
    }

    /// Set the hour component.
    #[must_use]
    pub const fn hours(mut self, value: u32) -> Self {
        self.interval.hours = value;
        self
    }

    /// Set the minute component.
    #[must_use]
    pub const fn minutes(mut self, value: u32) -> Self {
        self.interval.minutes = value;
        self
    }

    /// Set the second component.
    #[must_use]
    pub const fn seconds(mut self, value: u32) -> Self {
        self.interval.seconds = value;
        self
    }

    /// Validate and finish the interval.
    pub fn build(self) -> Result<CalendarInterval, ScheduleError> {
        if self.interval.is_zero() {
            return Err(ScheduleError::ZeroInterval);
        }
        Ok(self.interval)
    }
}

/// The trigger used by a job.
#[derive(Clone, Debug)]
pub enum JobSchedule {
    /// A cron expression evaluated in the scheduler's configured time zone.
    Cron(Box<CronSchedule>),
    /// A single UTC instant.
    Once(DateTime<Utc>),
    /// Repeat after the previous invocation has completed.
    FixedDelay(Interval),
    /// Repeat relative to the original start anchor.
    FixedRate(Interval),
}

impl From<CronSchedule> for JobSchedule {
    fn from(value: CronSchedule) -> Self {
        Self::Cron(Box::new(value))
    }
}

impl JobSchedule {
    /// Wrap a cron expression without exposing the internal size optimization.
    #[must_use]
    pub fn cron(value: CronSchedule) -> Self {
        Self::from(value)
    }
}

/// Controls how overdue occurrences are handled.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MisfirePolicy {
    /// Coalesce all overdue occurrences into one invocation.
    #[default]
    Skip,
    /// Execute at most `max_runs` overdue occurrences before skipping the remainder.
    CatchUp { max_runs: usize },
}

/// Time zone used for cron and calendar schedules.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SchedulerTimeZone {
    /// Coordinated Universal Time.
    #[default]
    Utc,
    /// A fixed UTC offset.
    Fixed(FixedOffset),
    /// The operating system's local time zone, including DST rules.
    Local,
    /// An IANA time zone from `chrono-tz`.
    #[cfg(feature = "timezones")]
    Named(chrono_tz::Tz),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockSnapshot {
    pub(crate) wall: DateTime<Utc>,
    pub(crate) monotonic: Instant,
}

impl ClockSnapshot {
    pub(crate) fn now() -> Self {
        Self {
            wall: Utc::now(),
            monotonic: Instant::now(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ScheduleCursor {
    schedule: JobSchedule,
    initialized: bool,
    next_wall: Option<DateTime<Utc>>,
    next_monotonic: Option<Instant>,
    anchor_wall: Option<DateTime<Utc>>,
    anchor_monotonic: Option<Instant>,
    occurrence: u64,
    preferred_day: u32,
    completed: bool,
    cron_after_override: Option<DateTime<Utc>>,
}

impl ScheduleCursor {
    pub(crate) fn new(schedule: JobSchedule) -> Self {
        Self {
            schedule,
            initialized: false,
            next_wall: None,
            next_monotonic: None,
            anchor_wall: None,
            anchor_monotonic: None,
            occurrence: 1,
            preferred_day: 1,
            completed: false,
            cron_after_override: None,
        }
    }

    pub(crate) fn replace(&mut self, schedule: JobSchedule) {
        *self = Self::new(schedule);
    }

    pub(crate) fn set_cron_after(&mut self, after: Option<DateTime<Utc>>) {
        self.cron_after_override = after;
        self.initialized = false;
        self.next_wall = None;
    }

    pub(crate) fn resume(&mut self, now: ClockSnapshot, time_zone: SchedulerTimeZone) {
        if !matches!(self.schedule, JobSchedule::Once(_)) {
            self.initialized = false;
            self.next_wall = None;
            self.next_monotonic = None;
            self.anchor_wall = None;
            self.anchor_monotonic = None;
            self.occurrence = 1;
            self.cron_after_override = Some(now.wall);
            self.initialize(now, time_zone);
        }
    }

    pub(crate) fn due(
        &mut self,
        now: ClockSnapshot,
        time_zone: SchedulerTimeZone,
        missed_limit: usize,
    ) -> Vec<DateTime<Utc>> {
        let had_override = self.cron_after_override.is_some();
        if !self.initialized {
            self.initialize(now, time_zone);
            if !had_override && !matches!(self.schedule, JobSchedule::Once(_)) {
                return Vec::new();
            }
        }

        match &self.schedule {
            JobSchedule::Cron(_) => self.cron_due(now.wall, time_zone, missed_limit),
            JobSchedule::Once(_) => self.once_due(now.wall),
            JobSchedule::FixedDelay(_) => self.delay_due(now),
            JobSchedule::FixedRate(interval) => {
                let interval = *interval;
                self.rate_due(now, time_zone, interval, missed_limit)
            }
        }
    }

    pub(crate) fn completed_at(
        &mut self,
        now: ClockSnapshot,
        time_zone: SchedulerTimeZone,
    ) -> Result<(), ScheduleError> {
        if matches!(self.schedule, JobSchedule::Once(_)) {
            self.initialized = true;
            self.completed = true;
            self.next_wall = None;
            return Ok(());
        }
        let JobSchedule::FixedDelay(interval) = self.schedule else {
            return Ok(());
        };
        if !self.initialized {
            self.initialized = true;
            self.anchor_wall = Some(now.wall);
            self.anchor_monotonic = Some(now.monotonic);
            self.preferred_day = local_day(now.wall, time_zone);
        }

        match interval.0 {
            IntervalKind::Fixed(duration) => {
                self.next_monotonic = now.monotonic.checked_add(duration);
                if self.next_monotonic.is_none() {
                    return Err(ScheduleError::IntervalOverflow);
                }
            }
            IntervalKind::Calendar(calendar) => {
                self.next_wall = Some(add_calendar_interval(
                    now.wall,
                    time_zone,
                    calendar,
                    1,
                    self.preferred_day,
                )?);
            }
        }
        Ok(())
    }

    pub(crate) fn next_in(
        &self,
        now: ClockSnapshot,
        time_zone: SchedulerTimeZone,
    ) -> Option<StdDuration> {
        if !self.initialized {
            return match &self.schedule {
                JobSchedule::Cron(cron) => {
                    next_cron_after(cron, self.cron_after_override.unwrap_or(now.wall), time_zone)
                        .map(|next| (next - now.wall).to_std().unwrap_or(StdDuration::ZERO))
                }
                JobSchedule::Once(at) => {
                    Some((*at - now.wall).to_std().unwrap_or(StdDuration::ZERO))
                }
                JobSchedule::FixedDelay(Interval(IntervalKind::Fixed(duration)))
                | JobSchedule::FixedRate(Interval(IntervalKind::Fixed(duration))) => {
                    Some(*duration)
                }
                JobSchedule::FixedDelay(Interval(IntervalKind::Calendar(calendar)))
                | JobSchedule::FixedRate(Interval(IntervalKind::Calendar(calendar))) => {
                    add_calendar_interval(
                        now.wall,
                        time_zone,
                        *calendar,
                        1,
                        local_day(now.wall, time_zone),
                    )
                    .ok()
                    .map(|next| (next - now.wall).to_std().unwrap_or(StdDuration::ZERO))
                }
            };
        }

        match self.schedule {
            JobSchedule::FixedDelay(Interval(IntervalKind::Fixed(_)))
            | JobSchedule::FixedRate(Interval(IntervalKind::Fixed(_))) => self
                .next_monotonic
                .map(|next| next.saturating_duration_since(now.monotonic)),
            _ => self
                .next_wall
                .map(|next| (next - now.wall).to_std().unwrap_or(StdDuration::ZERO)),
        }
    }

    fn initialize(&mut self, now: ClockSnapshot, time_zone: SchedulerTimeZone) {
        self.initialized = true;
        self.completed = false;
        self.anchor_wall = Some(now.wall);
        self.anchor_monotonic = Some(now.monotonic);
        self.preferred_day = local_day(now.wall, time_zone);
        self.occurrence = 1;

        match &self.schedule {
            JobSchedule::Cron(cron) => {
                let after = self.cron_after_override.take().unwrap_or(now.wall);
                self.next_wall = next_cron_after(cron, after, time_zone);
            }
            JobSchedule::Once(at) => self.next_wall = Some(*at),
            JobSchedule::FixedDelay(interval) | JobSchedule::FixedRate(interval) => {
                match interval.0 {
                    IntervalKind::Fixed(duration) => {
                        self.next_monotonic = now.monotonic.checked_add(duration);
                    }
                    IntervalKind::Calendar(calendar) => {
                        self.next_wall = add_calendar_interval(
                            now.wall,
                            time_zone,
                            calendar,
                            1,
                            self.preferred_day,
                        )
                        .ok();
                    }
                }
            }
        }
    }

    fn cron_due(
        &mut self,
        now: DateTime<Utc>,
        time_zone: SchedulerTimeZone,
        missed_limit: usize,
    ) -> Vec<DateTime<Utc>> {
        let mut due = Vec::new();
        let effective_limit = effective_limit(missed_limit);

        while let Some(next) = self.next_wall {
            if next > now || due.len() >= effective_limit {
                break;
            }
            due.push(next);
            self.next_wall = match &self.schedule {
                JobSchedule::Cron(cron) => next_cron_after(cron, next, time_zone),
                _ => None,
            };
            if missed_limit == 1 {
                self.next_wall = match &self.schedule {
                    JobSchedule::Cron(cron) => next_cron_after(cron, now, time_zone),
                    _ => None,
                };
                break;
            }
        }

        if missed_limit != 0
            && due.len() == effective_limit
            && self.next_wall.is_some_and(|next| next <= now)
        {
            self.next_wall = match &self.schedule {
                JobSchedule::Cron(cron) => next_cron_after(cron, now, time_zone),
                _ => None,
            };
        }
        due
    }

    fn once_due(&mut self, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        if self.completed {
            return Vec::new();
        }
        let Some(at) = self.next_wall else {
            return Vec::new();
        };
        if at > now {
            return Vec::new();
        }
        self.completed = true;
        self.next_wall = None;
        vec![at]
    }

    fn delay_due(&mut self, now: ClockSnapshot) -> Vec<DateTime<Utc>> {
        let due = match self.schedule {
            JobSchedule::FixedDelay(Interval(IntervalKind::Fixed(_))) => self
                .next_monotonic
                .is_some_and(|next| next <= now.monotonic),
            JobSchedule::FixedDelay(Interval(IntervalKind::Calendar(_))) => {
                self.next_wall.is_some_and(|next| next <= now.wall)
            }
            _ => false,
        };
        if due {
            self.next_wall = None;
            self.next_monotonic = None;
            vec![now.wall]
        } else {
            Vec::new()
        }
    }

    fn rate_due(
        &mut self,
        now: ClockSnapshot,
        time_zone: SchedulerTimeZone,
        interval: Interval,
        missed_limit: usize,
    ) -> Vec<DateTime<Utc>> {
        match interval.0 {
            IntervalKind::Fixed(duration) => self.fixed_rate_due(now, duration, missed_limit),
            IntervalKind::Calendar(calendar) => {
                self.calendar_rate_due(now.wall, time_zone, calendar, missed_limit)
            }
        }
    }

    fn fixed_rate_due(
        &mut self,
        now: ClockSnapshot,
        interval: StdDuration,
        missed_limit: usize,
    ) -> Vec<DateTime<Utc>> {
        let Some(anchor) = self.anchor_monotonic else {
            return Vec::new();
        };
        let Some(next) = self.next_monotonic else {
            return Vec::new();
        };
        if next > now.monotonic {
            return Vec::new();
        }

        let interval_nanos = interval.as_nanos();
        let elapsed_nanos = now.monotonic.duration_since(anchor).as_nanos();
        let latest = u64::try_from(elapsed_nanos / interval_nanos).unwrap_or(u64::MAX);
        let available = latest.saturating_sub(self.occurrence).saturating_add(1);
        let run_count = if missed_limit == 0 {
            available.min(MAX_DUE_PER_TICK as u64)
        } else {
            available.min(missed_limit.max(1) as u64)
        };
        let due = vec![now.wall; usize::try_from(run_count).unwrap_or(MAX_DUE_PER_TICK)];

        if missed_limit == 0 && run_count < available {
            self.occurrence = self.occurrence.saturating_add(run_count);
        } else {
            self.occurrence = latest.saturating_add(1);
        }
        self.next_monotonic = multiply_duration(interval, self.occurrence)
            .and_then(|offset| anchor.checked_add(offset));
        due
    }

    fn calendar_rate_due(
        &mut self,
        now: DateTime<Utc>,
        time_zone: SchedulerTimeZone,
        interval: CalendarInterval,
        missed_limit: usize,
    ) -> Vec<DateTime<Utc>> {
        let Some(anchor) = self.anchor_wall else {
            return Vec::new();
        };
        let mut due = Vec::new();
        let effective_limit = effective_limit(missed_limit);
        let mut searched = 0;

        while self.next_wall.is_some_and(|next| next <= now) && searched < MAX_CALENDAR_SEARCH {
            if due.len() < effective_limit {
                due.push(self.next_wall.unwrap_or(now));
            }
            self.occurrence = self.occurrence.saturating_add(1);
            self.next_wall = add_calendar_interval(
                anchor,
                time_zone,
                interval,
                self.occurrence,
                self.preferred_day,
            )
            .ok();
            searched += 1;
            if missed_limit == 0 && due.len() == effective_limit {
                break;
            }
        }
        due
    }
}

fn effective_limit(missed_limit: usize) -> usize {
    if missed_limit == 0 {
        MAX_DUE_PER_TICK
    } else {
        missed_limit.clamp(1, MAX_DUE_PER_TICK)
    }
}

fn multiply_duration(duration: StdDuration, multiplier: u64) -> Option<StdDuration> {
    let nanos = duration.as_nanos().checked_mul(u128::from(multiplier))?;
    let seconds = u64::try_from(nanos / 1_000_000_000).ok()?;
    let subsecond_nanos = u32::try_from(nanos % 1_000_000_000).ok()?;
    Some(StdDuration::new(seconds, subsecond_nanos))
}

fn local_day(now: DateTime<Utc>, time_zone: SchedulerTimeZone) -> u32 {
    match time_zone {
        SchedulerTimeZone::Utc => now.day(),
        SchedulerTimeZone::Fixed(offset) => now.with_timezone(&offset).day(),
        SchedulerTimeZone::Local => now.with_timezone(&Local).day(),
        #[cfg(feature = "timezones")]
        SchedulerTimeZone::Named(time_zone) => now.with_timezone(&time_zone).day(),
    }
}

fn next_cron_after(
    cron: &CronSchedule,
    after: DateTime<Utc>,
    time_zone: SchedulerTimeZone,
) -> Option<DateTime<Utc>> {
    match time_zone {
        SchedulerTimeZone::Utc => next_cron_in_zone(cron, after),
        SchedulerTimeZone::Fixed(offset) => next_cron_in_zone(cron, after.with_timezone(&offset)),
        SchedulerTimeZone::Local => next_cron_in_zone(cron, after.with_timezone(&Local)),
        #[cfg(feature = "timezones")]
        SchedulerTimeZone::Named(time_zone) => {
            next_cron_in_zone(cron, after.with_timezone(&time_zone))
        }
    }
}

#[cfg(feature = "croner")]
fn next_cron_in_zone<Tz>(cron: &CronSchedule, after: DateTime<Tz>) -> Option<DateTime<Utc>>
where
    Tz: TimeZone + Copy,
    Tz::Offset: Display,
{
    cron.iter_after(after)
        .next()
        .map(|next| next.with_timezone(&Utc))
}

#[cfg(feature = "cron")]
#[expect(
    clippy::needless_pass_by_value,
    reason = "both cron backends share one adapter and Croner consumes this value"
)]
fn next_cron_in_zone<Tz>(cron: &CronSchedule, after: DateTime<Tz>) -> Option<DateTime<Utc>>
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    cron.after(&after)
        .next()
        .map(|next| next.with_timezone(&Utc))
}

fn add_calendar_interval(
    anchor: DateTime<Utc>,
    time_zone: SchedulerTimeZone,
    interval: CalendarInterval,
    occurrence: u64,
    preferred_day: u32,
) -> Result<DateTime<Utc>, ScheduleError> {
    match time_zone {
        SchedulerTimeZone::Utc => {
            add_calendar_in_zone(&anchor, &Utc, interval, occurrence, preferred_day)
        }
        SchedulerTimeZone::Fixed(offset) => add_calendar_in_zone(
            &anchor.with_timezone(&offset),
            &offset,
            interval,
            occurrence,
            preferred_day,
        ),
        SchedulerTimeZone::Local => add_calendar_in_zone(
            &anchor.with_timezone(&Local),
            &Local,
            interval,
            occurrence,
            preferred_day,
        ),
        #[cfg(feature = "timezones")]
        SchedulerTimeZone::Named(time_zone) => add_calendar_in_zone(
            &anchor.with_timezone(&time_zone),
            &time_zone,
            interval,
            occurrence,
            preferred_day,
        ),
    }
}

fn add_calendar_in_zone<Tz>(
    anchor: &DateTime<Tz>,
    time_zone: &Tz,
    interval: CalendarInterval,
    occurrence: u64,
    preferred_day: u32,
) -> Result<DateTime<Utc>, ScheduleError>
where
    Tz: TimeZone,
{
    let calendar_months = u64::from(interval.years)
        .checked_mul(12)
        .and_then(|years| years.checked_add(u64::from(interval.months)))
        .and_then(|months| months.checked_mul(occurrence))
        .ok_or(ScheduleError::IntervalOverflow)?;
    let base_month = i64::from(anchor.year())
        .checked_mul(12)
        .and_then(|year| year.checked_add(i64::from(anchor.month0())))
        .ok_or(ScheduleError::CalendarOutOfRange)?;
    let target_month = base_month
        .checked_add(i64::try_from(calendar_months).map_err(|_| ScheduleError::IntervalOverflow)?)
        .ok_or(ScheduleError::CalendarOutOfRange)?;
    let year = i32::try_from(target_month.div_euclid(12))
        .map_err(|_| ScheduleError::CalendarOutOfRange)?;
    let month = u32::try_from(target_month.rem_euclid(12) + 1)
        .map_err(|_| ScheduleError::CalendarOutOfRange)?;
    let day = preferred_day.min(days_in_month(year, month)?);
    let date =
        NaiveDate::from_ymd_opt(year, month, day).ok_or(ScheduleError::CalendarOutOfRange)?;
    let mut local = date.and_time(anchor.time());

    let days = u64::from(interval.days)
        .checked_mul(occurrence)
        .ok_or(ScheduleError::IntervalOverflow)?;
    local = local
        .checked_add_days(Days::new(days))
        .ok_or(ScheduleError::CalendarOutOfRange)?;

    let seconds = u64::from(interval.hours)
        .checked_mul(60 * 60)
        .and_then(|value| {
            u64::from(interval.minutes)
                .checked_mul(60)
                .and_then(|minutes| value.checked_add(minutes))
        })
        .and_then(|value| value.checked_add(u64::from(interval.seconds)))
        .and_then(|value| value.checked_mul(occurrence))
        .ok_or(ScheduleError::IntervalOverflow)?;
    local = local
        .checked_add_signed(Duration::seconds(
            i64::try_from(seconds).map_err(|_| ScheduleError::IntervalOverflow)?,
        ))
        .ok_or(ScheduleError::CalendarOutOfRange)?;

    resolve_local_datetime(time_zone, local)
        .map(|value| value.with_timezone(&Utc))
        .ok_or(ScheduleError::CalendarOutOfRange)
}

fn resolve_local_datetime<Tz>(time_zone: &Tz, local: NaiveDateTime) -> Option<DateTime<Tz>>
where
    Tz: TimeZone,
{
    match time_zone.from_local_datetime(&local) {
        LocalResult::Single(value) => Some(value),
        LocalResult::Ambiguous(first, second) => {
            if first.timestamp() <= second.timestamp() {
                Some(first)
            } else {
                Some(second)
            }
        }
        LocalResult::None => {
            for seconds in 1..=(24 * 60 * 60) {
                let candidate = local.checked_add_signed(Duration::seconds(seconds))?;
                match time_zone.from_local_datetime(&candidate) {
                    LocalResult::Single(value) => return Some(value),
                    LocalResult::Ambiguous(first, second) => {
                        return if first.timestamp() <= second.timestamp() {
                            Some(first)
                        } else {
                            Some(second)
                        };
                    }
                    LocalResult::None => {}
                }
            }
            None
        }
    }
}

fn days_in_month(year: i32, month: u32) -> Result<u32, ScheduleError> {
    let (next_year, next_month) = if month == 12 {
        (
            year.checked_add(1)
                .ok_or(ScheduleError::CalendarOutOfRange)?,
            1,
        )
    } else {
        (year, month + 1)
    };
    let first_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .ok_or(ScheduleError::CalendarOutOfRange)?;
    let last = first_next
        .pred_opt()
        .ok_or(ScheduleError::CalendarOutOfRange)?;
    Ok(last.day())
}

#[cfg(test)]
mod tests {
    #[cfg(any(feature = "croner", feature = "timezones"))]
    use chrono::Timelike;

    use super::*;

    #[test]
    fn interval_helpers_validate_zero_and_overflow() {
        assert_eq!(Interval::minutes(0), Err(ScheduleError::ZeroInterval));
        assert_eq!(Interval::hours(u64::MAX), Err(ScheduleError::IntervalOverflow));
    }

    #[test]
    fn calendar_interval_rejects_empty_builder() {
        assert_eq!(CalendarInterval::builder().build(), Err(ScheduleError::ZeroInterval));
    }

    #[test]
    fn calendar_month_end_clamps_and_restores_anchor() {
        let interval = CalendarInterval::builder().months(1).build().unwrap();
        let anchor = Utc.with_ymd_and_hms(2024, 1, 31, 10, 0, 0).unwrap();

        let february =
            add_calendar_interval(anchor, SchedulerTimeZone::Utc, interval, 1, 31).unwrap();
        let march = add_calendar_interval(anchor, SchedulerTimeZone::Utc, interval, 2, 31).unwrap();

        assert_eq!((february.month(), february.day()), (2, 29));
        assert_eq!((march.month(), march.day()), (3, 31));
    }

    #[test]
    fn one_shot_is_consumed_exactly_once() {
        let monotonic = Instant::now();
        let wall = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let at = wall + Duration::seconds(5);
        let mut cursor = ScheduleCursor::new(JobSchedule::Once(at));
        let initial = ClockSnapshot { wall, monotonic };
        let due = ClockSnapshot {
            wall: at,
            monotonic: monotonic + StdDuration::from_secs(5),
        };

        assert!(cursor.due(initial, SchedulerTimeZone::Utc, 1).is_empty());
        assert_eq!(cursor.due(due, SchedulerTimeZone::Utc, 1), vec![at]);
        assert!(cursor.due(due, SchedulerTimeZone::Utc, 1).is_empty());
    }

    #[test]
    fn overdue_one_shot_runs_on_the_first_tick() {
        let monotonic = Instant::now();
        let wall = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let at = wall + Duration::seconds(1);
        let mut cursor = ScheduleCursor::new(JobSchedule::Once(at));
        let overdue = ClockSnapshot {
            wall: wall + Duration::seconds(2),
            monotonic: monotonic + StdDuration::from_secs(2),
        };

        assert_eq!(cursor.due(overdue, SchedulerTimeZone::Utc, 1), vec![at]);
    }

    #[test]
    fn fixed_delay_restarts_after_completion() {
        let monotonic = Instant::now();
        let wall = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let mut cursor =
            ScheduleCursor::new(JobSchedule::FixedDelay(Interval::seconds(1).unwrap()));
        let initial = ClockSnapshot { wall, monotonic };
        let first_due = ClockSnapshot {
            wall: wall + Duration::seconds(1),
            monotonic: monotonic + StdDuration::from_secs(1),
        };
        let completed = ClockSnapshot {
            wall: wall + Duration::seconds(3),
            monotonic: monotonic + StdDuration::from_secs(3),
        };

        assert!(cursor.due(initial, SchedulerTimeZone::Utc, 1).is_empty());
        assert_eq!(cursor.due(first_due, SchedulerTimeZone::Utc, 1).len(), 1);
        cursor
            .completed_at(completed, SchedulerTimeZone::Utc)
            .unwrap();
        assert_eq!(
            cursor.next_in(completed, SchedulerTimeZone::Utc),
            Some(StdDuration::from_secs(1))
        );
    }

    #[test]
    fn fixed_rate_skips_old_occurrences_by_default() {
        let monotonic = Instant::now();
        let wall = Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let mut cursor = ScheduleCursor::new(JobSchedule::FixedRate(Interval::seconds(1).unwrap()));
        let initial = ClockSnapshot { wall, monotonic };
        let delayed = ClockSnapshot {
            wall: wall + Duration::seconds(5),
            monotonic: monotonic + StdDuration::from_secs(5),
        };

        assert!(cursor.due(initial, SchedulerTimeZone::Utc, 1).is_empty());
        assert_eq!(cursor.due(delayed, SchedulerTimeZone::Utc, 1).len(), 1);
        assert_eq!(
            cursor.next_in(delayed, SchedulerTimeZone::Utc),
            Some(StdDuration::from_secs(1))
        );
    }

    #[cfg(feature = "timezones")]
    #[test]
    fn calendar_interval_moves_dst_gap_to_first_valid_time() {
        let time_zone = chrono_tz::Europe::Berlin;
        let anchor = Utc.with_ymd_and_hms(2024, 3, 30, 1, 30, 0).unwrap();
        let interval = CalendarInterval::builder().days(1).build().unwrap();
        let next =
            add_calendar_interval(anchor, SchedulerTimeZone::Named(time_zone), interval, 1, 30)
                .unwrap()
                .with_timezone(&time_zone);

        assert_eq!((next.hour(), next.minute()), (3, 0));
    }

    #[cfg(feature = "croner")]
    #[test]
    fn croner_last_day_matches_leap_year() {
        let cron: CronSchedule = "0 0 23 L * *".parse().unwrap();
        let after = Utc.with_ymd_and_hms(2024, 2, 1, 0, 0, 0).unwrap();
        let next = next_cron_after(&cron, after, SchedulerTimeZone::Utc).unwrap();
        assert_eq!((next.month(), next.day(), next.hour()), (2, 29, 23));
    }
}
