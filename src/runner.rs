use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{
    CancellationToken, EventSink, Job, JobSchedule, JobScheduler, SchedulerError, SchedulerEvent,
    Uuid, emit,
};

const DEFAULT_CLOCK_RECHECK: Duration = Duration::from_secs(30);

enum JobSelector {
    Id(Uuid),
    Name(String),
}

enum Command {
    Add {
        name: Option<String>,
        job: Box<Job<'static>>,
        reply: Sender<Result<Uuid, SchedulerError>>,
    },
    Remove {
        selector: JobSelector,
        reply: Sender<Result<bool, SchedulerError>>,
    },
    Pause {
        selector: JobSelector,
        reply: Sender<Result<bool, SchedulerError>>,
    },
    Resume {
        selector: JobSelector,
        reply: Sender<Result<bool, SchedulerError>>,
    },
    Reschedule {
        selector: JobSelector,
        schedule: JobSchedule,
        reply: Sender<Result<(), SchedulerError>>,
    },
    RunNow {
        selector: JobSelector,
        reply: Sender<Result<(), SchedulerError>>,
    },
    Shutdown {
        cancel: bool,
    },
}

/// Cloneable control channel for a running [`ThreadRunner`].
#[derive(Clone)]
pub struct SchedulerHandle {
    sender: Sender<Command>,
}

#[expect(
    clippy::missing_errors_doc,
    reason = "control methods return the documented SchedulerError type"
)]
impl SchedulerHandle {
    /// Add an owned unnamed job and wake the runner immediately.
    pub fn add(&self, job: Job<'static>) -> Result<Uuid, SchedulerError> {
        self.add_inner(None, job)
    }

    /// Add an owned uniquely named job and wake the runner immediately.
    pub fn add_named(
        &self,
        name: impl Into<String>,
        job: Job<'static>,
    ) -> Result<Uuid, SchedulerError> {
        self.add_inner(Some(name.into()), job)
    }

    /// Remove a job by UUID.
    pub fn remove(&self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.boolean_request(|reply| Command::Remove {
            selector: JobSelector::Id(job_id),
            reply,
        })
    }

    /// Remove a job by name.
    pub fn remove_named(&self, name: impl Into<String>) -> Result<bool, SchedulerError> {
        let name = name.into();
        self.boolean_request(|reply| Command::Remove {
            selector: JobSelector::Name(name),
            reply,
        })
    }

    /// Pause a job by UUID.
    pub fn pause(&self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.boolean_request(|reply| Command::Pause {
            selector: JobSelector::Id(job_id),
            reply,
        })
    }

    /// Pause a job by name.
    pub fn pause_named(&self, name: impl Into<String>) -> Result<bool, SchedulerError> {
        let name = name.into();
        self.boolean_request(|reply| Command::Pause {
            selector: JobSelector::Name(name),
            reply,
        })
    }

    /// Resume a job by UUID.
    pub fn resume(&self, job_id: Uuid) -> Result<bool, SchedulerError> {
        self.boolean_request(|reply| Command::Resume {
            selector: JobSelector::Id(job_id),
            reply,
        })
    }

    /// Resume a job by name.
    pub fn resume_named(&self, name: impl Into<String>) -> Result<bool, SchedulerError> {
        let name = name.into();
        self.boolean_request(|reply| Command::Resume {
            selector: JobSelector::Name(name),
            reply,
        })
    }

    /// Replace a job's schedule by UUID.
    pub fn reschedule(&self, job_id: Uuid, schedule: JobSchedule) -> Result<(), SchedulerError> {
        self.unit_request(|reply| Command::Reschedule {
            selector: JobSelector::Id(job_id),
            schedule,
            reply,
        })
    }

    /// Replace a job's schedule by name.
    pub fn reschedule_named(
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
    }

    /// Invoke a job immediately by UUID without changing cron/fixed-rate anchors.
    pub fn run_now(&self, job_id: Uuid) -> Result<(), SchedulerError> {
        self.unit_request(|reply| Command::RunNow {
            selector: JobSelector::Id(job_id),
            reply,
        })
    }

    /// Invoke a job immediately by name.
    pub fn run_now_named(&self, name: impl Into<String>) -> Result<(), SchedulerError> {
        let name = name.into();
        self.unit_request(|reply| Command::RunNow {
            selector: JobSelector::Name(name),
            reply,
        })
    }

    fn add_inner(&self, name: Option<String>, job: Job<'static>) -> Result<Uuid, SchedulerError> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(Command::Add {
                name,
                job: Box::new(job),
                reply,
            })
            .map_err(|_| SchedulerError::RunnerStopped)?;
        response.recv().map_err(|_| SchedulerError::RunnerStopped)?
    }

    fn boolean_request(
        &self,
        command: impl FnOnce(Sender<Result<bool, SchedulerError>>) -> Command,
    ) -> Result<bool, SchedulerError> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(command(reply))
            .map_err(|_| SchedulerError::RunnerStopped)?;
        response.recv().map_err(|_| SchedulerError::RunnerStopped)?
    }

    fn unit_request(
        &self,
        command: impl FnOnce(Sender<Result<(), SchedulerError>>) -> Command,
    ) -> Result<(), SchedulerError> {
        let (reply, response) = mpsc::channel();
        self.sender
            .send(command(reply))
            .map_err(|_| SchedulerError::RunnerStopped)?;
        response.recv().map_err(|_| SchedulerError::RunnerStopped)?
    }
}

/// Event-driven background driver for synchronous jobs.
pub struct ThreadRunner {
    handle: SchedulerHandle,
    join_handle: Option<JoinHandle<()>>,
    cancellation: CancellationToken,
}

#[expect(
    clippy::missing_errors_doc,
    reason = "runner methods report thread and scheduler failures"
)]
impl ThreadRunner {
    /// Start a runner without an event sink.
    pub fn start(scheduler: JobScheduler<'static>) -> io::Result<Self> {
        Self::start_with_options(scheduler, DEFAULT_CLOCK_RECHECK, None)
    }

    /// Start a runner and forward events to the supplied non-blocking callback.
    pub fn start_with_event_sink<F>(scheduler: JobScheduler<'static>, sink: F) -> io::Result<Self>
    where
        F: Fn(SchedulerEvent) + Send + Sync + 'static,
    {
        Self::start_with_options(scheduler, DEFAULT_CLOCK_RECHECK, Some(std::sync::Arc::new(sink)))
    }

    /// Start a runner with an explicit wall-clock recheck interval.
    pub fn start_with_options(
        mut scheduler: JobScheduler<'static>,
        clock_recheck: Duration,
        sink: Option<EventSink>,
    ) -> io::Result<Self> {
        let clock_recheck = if clock_recheck.is_zero() {
            DEFAULT_CLOCK_RECHECK
        } else {
            clock_recheck
        };
        let cancellation = CancellationToken::default();
        scheduler.set_runner_cancellation(cancellation.clone());
        let (sender, receiver) = mpsc::channel();
        let handle = SchedulerHandle { sender };
        let join_handle = thread::Builder::new()
            .name(String::from("job-scheduler"))
            .spawn(move || run_loop(&mut scheduler, &receiver, clock_recheck, sink.as_ref()))?;
        Ok(Self {
            handle,
            join_handle: Some(join_handle),
            cancellation,
        })
    }

    /// Return a cloneable control handle.
    #[must_use]
    pub fn handle(&self) -> SchedulerHandle {
        self.handle.clone()
    }

    /// Stop scheduling new work and wait for the active callback to finish.
    pub fn shutdown(mut self) -> Result<(), SchedulerError> {
        self.stop(false)
    }

    /// Signal every cancellation token, stop new work, and wait for callbacks
    /// that do not cooperate with cancellation.
    pub fn cancel_and_shutdown(mut self) -> Result<(), SchedulerError> {
        self.cancellation.cancel();
        self.stop(true)
    }

    fn stop(&mut self, cancel: bool) -> Result<(), SchedulerError> {
        self.handle
            .sender
            .send(Command::Shutdown { cancel })
            .map_err(|_| SchedulerError::RunnerStopped)?;
        let Some(join_handle) = self.join_handle.take() else {
            return Ok(());
        };
        join_handle
            .join()
            .map_err(|_| SchedulerError::RunnerPanicked)
    }
}

impl Drop for ThreadRunner {
    fn drop(&mut self) {
        if self.join_handle.is_some() {
            let _result = self.handle.sender.send(Command::Shutdown { cancel: false });
        }
    }
}

fn run_loop(
    scheduler: &mut JobScheduler<'static>,
    receiver: &Receiver<Command>,
    clock_recheck: Duration,
    sink: Option<&EventSink>,
) {
    loop {
        scheduler.tick_catching(sink);
        let command = match scheduler.next_wake_in() {
            Some(next) => match receiver.recv_timeout(next.min(clock_recheck)) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match receiver.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            },
        };

        if command.is_some_and(|command| process_command(scheduler, command, sink)) {
            break;
        }
    }
    emit(sink, SchedulerEvent::RunnerStopped);
}

fn process_command(
    scheduler: &mut JobScheduler<'static>,
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
            let result = resolve_id(scheduler, selector).map(|job_id| {
                let event = scheduler.event_for(job_id).ok();
                let removed = scheduler.remove(job_id);
                match event {
                    Some(event) if removed => emit(sink, SchedulerEvent::JobRemoved(event)),
                    Some(_) | None => {}
                }
                removed
            });
            let _result = reply.send(result);
        }
        Command::Pause { selector, reply } => {
            let result = resolve_id(scheduler, selector).and_then(|job_id| {
                let changed = scheduler.pause(job_id)?;
                if changed {
                    emit(sink, SchedulerEvent::JobPaused(scheduler.event_for(job_id)?));
                }
                Ok(changed)
            });
            let _result = reply.send(result);
        }
        Command::Resume { selector, reply } => {
            let result = resolve_id(scheduler, selector).and_then(|job_id| {
                let changed = scheduler.resume(job_id)?;
                if changed {
                    emit(sink, SchedulerEvent::JobResumed(scheduler.event_for(job_id)?));
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
                scheduler.reschedule(job_id, schedule)?;
                emit(sink, SchedulerEvent::JobRescheduled(scheduler.event_for(job_id)?));
                Ok(())
            });
            let _result = reply.send(result);
        }
        Command::RunNow { selector, reply } => {
            let result = resolve_id(scheduler, selector)
                .and_then(|job_id| scheduler.run_now_catching(job_id, sink).map(|_event| ()));
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
    scheduler: &JobScheduler<'static>,
    selector: JobSelector,
) -> Result<Uuid, SchedulerError> {
    match selector {
        JobSelector::Id(job_id) => scheduler.event_for(job_id).map(|event| event.job_id),
        JobSelector::Name(name) => scheduler.id_by_name(&name),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{Interval, JobEvent};

    #[test]
    fn command_wakes_an_idle_runner() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let scheduler = JobScheduler::new();
        let runner = ThreadRunner::start_with_event_sink(scheduler, move |event| {
            captured.lock().unwrap().push(event);
        })
        .unwrap();
        let handle = runner.handle();
        let started = Instant::now();
        let id = handle
            .add_named("wake-test", Job::fixed_rate(Interval::hours(1).unwrap(), || {}))
            .unwrap();
        handle.run_now(id).unwrap();
        runner.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            SchedulerEvent::JobStarted(JobEvent { job_id, .. }) if *job_id == id
        )));
        assert!(events.contains(&SchedulerEvent::RunnerStopped));
    }

    #[test]
    fn panic_is_reported_without_stopping_runner() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let scheduler = JobScheduler::new();
        let runner = ThreadRunner::start_with_event_sink(scheduler, move |event| {
            captured.lock().unwrap().push(event);
        })
        .unwrap();
        let handle = runner.handle();
        let id = handle
            .add(Job::fixed_rate(Interval::hours(1).unwrap(), || {
                panic!("expected test panic");
            }))
            .unwrap();

        handle.run_now(id).unwrap();
        assert!(handle.pause(id).unwrap());
        runner.shutdown().unwrap();

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            SchedulerEvent::JobPanicked(JobEvent { job_id, .. }) if *job_id == id
        )));
    }

    #[test]
    fn cancel_and_shutdown_signals_a_running_callback_immediately() {
        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let callback_started = Arc::clone(&started);
        let callback_stopped = Arc::clone(&stopped);
        let job = Job::from_schedule_with_context(
            JobSchedule::FixedRate(Interval::hours(1).unwrap()),
            move |context| {
                callback_started.store(true, Ordering::Release);
                while !context.cancellation_token().is_cancelled() {
                    thread::sleep(Duration::from_millis(1));
                }
                callback_stopped.store(true, Ordering::Release);
            },
        );
        let mut scheduler = JobScheduler::new();
        let id = scheduler.add(job);
        let runner = ThreadRunner::start(scheduler).unwrap();
        let handle = runner.handle();
        let trigger = thread::spawn(move || handle.run_now(id).unwrap());
        let wait_started = Instant::now();
        while !started.load(Ordering::Acquire) {
            assert!(wait_started.elapsed() < Duration::from_secs(2));
            thread::yield_now();
        }

        runner.cancel_and_shutdown().unwrap();
        trigger.join().unwrap();

        assert!(stopped.load(Ordering::Acquire));
    }
}
