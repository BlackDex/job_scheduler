use job_scheduler_ng::{Job, JobScheduler};
use std::time::Instant;

// If there are no jobs which should be run we could have run in a CPU Loop because we used `zero` sleep.
// This job is to test if we have a CPU loop or not.

fn main() {
    const WAIT_SECONDS: u64 = 40;

    let mut sched = JobScheduler::new();

    sched.add(Job::new("0/10 * * * * * 1999".parse().expect("Valid schedule"), || {
        log("I get executed every 10th second!");
    }));

    sched.add(Job::new("*/4 * * * * * 1999".parse().expect("Valid schedule"), || {
        log("I get executed every 4 seconds!");
    }));

    log(&format!("Run for about {WAIT_SECONDS} seconds!"));
    log("Starting loop");
    let start = Instant::now();
    loop {
        sched.tick();

        std::thread::sleep(sched.time_till_next_job());

        // Check if we have waited long enough
        if start.elapsed().as_secs() >= WAIT_SECONDS {
            break;
        }
    }
    log("Finished. Goodby!");
    std::process::exit(0);
}

fn log(msg: &str) {
    println!("{:?} - {:?} - {msg}", chrono::Utc::now(), std::thread::current().id());
}
