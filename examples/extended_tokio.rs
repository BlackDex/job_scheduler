use job_scheduler_ng::{AsyncJob, AsyncJobScheduler, Interval, TokioRunner};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut scheduler = AsyncJobScheduler::new();
    scheduler.use_system_timezone();
    scheduler.add_named(
        "async-refresh",
        AsyncJob::fixed_rate(Interval::minutes(5)?, async || {
            println!("Async refresh");
        }),
    )?;

    let runner = TokioRunner::start(scheduler);
    let handle = runner.handle();
    handle.run_now_named("async-refresh").await?;
    runner.shutdown().await?;
    Ok(())
}
