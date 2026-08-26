use std::sync::mpsc;

use job_scheduler_ng::{CalendarInterval, Job, JobSchedule, SchedulerEvent, ThreadRunner};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut scheduler = job_scheduler_ng::JobScheduler::new();
    scheduler.use_system_timezone();

    let month_end = "0 0 23 L * *".parse()?;
    scheduler.add_named("month-end", Job::new(month_end, || println!("Last day of the month")))?;

    let calendar_interval = CalendarInterval::builder().days(1).hours(2).build()?;
    scheduler.add_named(
        "daily-refresh",
        Job::fixed_delay(calendar_interval.into(), || println!("Refresh")),
    )?;

    let (event_sender, event_receiver) = mpsc::channel();
    let runner = ThreadRunner::start_with_event_sink(scheduler, move |event| {
        let _result = event_sender.send(event);
    })?;
    runner.handle().run_now_named("daily-refresh")?;
    let hourly: job_scheduler_ng::CronSchedule = "0 0 * * * *".parse()?;
    runner
        .handle()
        .reschedule_named("daily-refresh", JobSchedule::from(hourly))?;
    runner.shutdown()?;

    for event in event_receiver.try_iter() {
        if matches!(event, SchedulerEvent::JobFinished(_)) {
            println!("Job finished");
        }
    }
    Ok(())
}
