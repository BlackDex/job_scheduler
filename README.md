# JobScheduler
[![](https://docs.rs/job_scheduler_ng/badge.svg)](https://docs.rs/job_scheduler_ng) [![](https://img.shields.io/crates/v/job_scheduler_ng.svg)](https://crates.io/crates/job_scheduler_ng) [![](https://deps.rs/repo/github/BlackDex/job_scheduler/status.svg)](https://deps.rs/repo/github/BlackDex/job_scheduler)

A simple cron-like job scheduling library for Rust.

Forked from https://github.com/lholden/job_scheduler, thanks @lholden!
This is a fork which i try to maintain and maybe even improve where needed.


## Updates

**2026-07-26 (v2.5.0):**
 - Set MSRV to v1.87.0 which supports `cast_signed()`
 - Some refactoring to keep the different cron features more DRY.
 - Fixed a possible issue when all jobs are in the past. This could cause a CPU loop.
 - Prevent a single task from starving others if there is a large list of ticks in the backlog.
   Large backlogs are run in batches of `1_000` jobs per tick.

**2025-08-13 (v2.4.0):**
 - Should be backwards compatible with previous versions
 - Added croner as an alternative cron parser (thanks to @DenuxPlays)
 - Added pre-commit and GitHub workflow checks

**2025-08-13 (v2.3.0):**
 - Should be backwards compatible with previous versions
 - Set MSRV to v1.83.0 which supports const `Option::<T>`
 - Updated dependencies
 - Fixed all tests, they were all ignored before
 - Fixed a possible panic
 - Used `pedantic` clippy and fixed those items
 - Added some extra lints and adjusted where needed
 - Only include files needed in the crate package

**2025-04-13 (v2.2.0):**
 - Should be backwards compatible with previous versions
 - Set MSRV to v1.65.0 to match the lowest dependency
 - Updated dependencies to the lowest possible version
 - Added an option to use a different timezone, default is still UTC
 - Added an example test for the timezone functionality

**2024-12-15 (v2.1.0 / unreleased):**
 - Updated dependencies
 - Added and fixed some clippy lints

**2024-04-24 (v2.0.5):**
 - Set MSRV to v1.61.0 to match chrono's v0.4.34 MSRV
 - Updated dev dependency of tokio to v1.37.0 or higher
 - Several clippy check added
 - Fixed all clippy reported items
 - Set JobScheduler::new() as `const fn`
 - Updated examples to use a `log` function and always print the current thread id
 - Added a very simple hash to better differentiate the tokio 5th second example

**2023-02-01 (v2.0.4):**
 - Validated uuid v1.3.0 works
 - Used miri to check examples
   Added an `std::process::exit(0);` to produce a clean exit.
 - Set MSRV to v1.56.1
 - Updated dev dependency of tokio to v1.25.0 or higher

**2022-12-10 (v2.0.3):**
 - Don't require Sync trait for job function (PR [#1](https://github.com/BlackDex/job_scheduler/pull/1) - Thanks @mikel1703)
 - Added two other examples. One using threading, and one also using MPSC.
 - Added some clippy checks
 - Fixed some spelling errors

**2022-10-09 (v2.0.2):**
 - Updated cron to v0.12.0
 - Set chrono v0.4.20 as minimum version to mitigate a know CVE.


## Usage

Please see the [Documentation](https://docs.rs/job_scheduler_ng/) for more details.

Be sure to add the job_scheduler_ng crate to your `Cargo.toml`:

```toml
[dependencies]
job_scheduler_ng = "2"
```

Creating a Cron for a job is done using the `FromStr` impl for the
`Schedule` or `Cron` structs depending on which cron parser feature you use.
By default, the `Schedule` struct from the [cron](https://github.com/zslayton/cron) library is used,
but the `Cron` struct from [croner](https://github.com/Hexagon/croner-rust) is also supported as an alternative.

To use croner just change your `Cargo.toml` to the following:
```toml
[dependencies]
job_scheduler_ng = { version = "2", default-features = false, features = ["croner"] }
```

Croner also enables extended expressions such as `L` for the last day of a
month. For example, `0 0 23 L * *` runs at 23:00 on every month's last day.

Optional features remain opt-in:

| Feature | Purpose |
| --- | --- |
| `croner` | Extended cron syntax including `L`, `W`, and `#` |
| `timezones` | Named IANA time zones through `chrono-tz` |
| `tokio` | Native async jobs and an event-driven Tokio runner |

Tokio is not downloaded or compiled for synchronous-only users:

```toml
[dependencies]
# Thread/manual scheduler, no Tokio dependency
job_scheduler_ng = { version = "2", default-features = false, features = ["croner", "timezones"] }

# Native async scheduler
job_scheduler_ng = { version = "2", default-features = false, features = ["croner", "timezones", "tokio"] }
```

Besides cron, jobs can run once, use a fixed rate, or wait until the previous
run finishes. Calendar intervals support years through seconds:

```rust
use job_scheduler_ng::{CalendarInterval, Job, JobScheduler, ThreadRunner};

let interval = CalendarInterval::builder()
    .days(1)
    .hours(2)
    .build()?;
let mut scheduler = JobScheduler::new();
scheduler.add_named(
    "refresh",
    Job::fixed_delay(interval.into(), || println!("refresh")),
)?;

let runner = ThreadRunner::start(scheduler)?;
runner.handle().run_now_named("refresh")?;
runner.shutdown()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`ThreadRunner` waits on its control channel until the next deadline, so adding,
pausing, resuming, rescheduling, or removing a job wakes it immediately. The
manual `tick()` API remains available for event loops that want to own the
scheduler thread.

The scheduling format is as follows:

```text
sec   min   hour   day of month   month   day of week   year
*     *     *      *              *       *             *
```

Time is specified for `UTC` and not your local timezone. Note that the year may
be omitted.

Comma separated values such as `5,8,10` represent more than one time value. So
for example, a schedule of `0 2,14,26 * * * *` would execute on the 2nd, 14th,
and 26th minute of every hour.

Ranges can be specified with a dash. A schedule of `0 0 * 5-10 * *` would
execute once per hour but only on day 5 through 10 of the month.

Day of the week can be specified as an abbreviation or the full name. A
schedule of `0 0 6 * * Sun,Sat` would execute at 6am on Sunday and Saturday.

A simple usage example:

```rust
extern crate job_scheduler_ng;
use job_scheduler_ng::{JobScheduler, Job, MIN_DURATION};
use std::time::Duration;

fn main() {
    let mut sched = JobScheduler::new();

    sched.add(Job::new("1/10 * * * * *".parse().unwrap(), || {
        println!("I get executed every 10 seconds!");
    }));

    loop {
        sched.tick();

        std::thread::sleep(Duration::from_millis(MIN_DURATION));
    }
}
```

Setting a custom timezone other than the default UTC.
Any `Tz::Offset` provided by chrono will work.

```rust
use chrono::Local;
use job_scheduler_ng::{JobScheduler, Job, MIN_DURATION};
use core::time::Duration;

fn main() {
    let mut sched = JobScheduler::new();
    let local_tz = chrono::Local::now();
    sched.set_timezone(*local_tz.offset());

    sched.add(Job::new("0 5 13 * * *".parse().unwrap(), || {
        println!("I get executed every day 13:05 local time!");
    }));

    loop {
        sched.tick();
        std::thread::sleep(Duration::from_millis(MIN_DURATION));
    }
}
```

## Similar Libraries

* [cron](https://github.com/zslayton/cron) the cron expression parser we use.
* [croner](https://github.com/Hexagon/croner-rust) the other cron expression parser you can use.
* [schedule-rs](https://github.com/mehcode/schedule-rs) is a similar rust library that implements it's own cron expression parser.

## License

JobScheduler is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

## Contributing

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

Please see the [CONTRIBUTING](CONTRIBUTING.md) file for more information.
