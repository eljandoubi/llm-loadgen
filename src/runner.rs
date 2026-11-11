//! The two ways to generate load.
//!
//! **Open loop** (the point of this crate): requests are meant to start at times fixed before the
//! run, whatever the server does. A request that has to wait for a free connection is not
//! forgiven: its latency is measured from the moment it was meant to start. If the server stalls
//! for a second, every request that should have started during that second shows it.
//!
//! **Closed loop** (for comparison): a fixed number of clients each send a request, wait for the
//! answer, then send the next. When the server stalls, the clients stall with it and stop
//! sending, so the requests that would have suffered are never made. That is coordinated
//! omission, and it makes a server look better exactly when it is worse.

use crate::arrival::{Process, Schedule};
use crate::client::{self, RequestSpec};
use crate::report::{Report, Stats};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Open,
    Closed,
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub mode: Mode,
    /// Open loop only.
    pub process: Process,
    /// Requests per second, open loop only.
    pub rate: f64,
    /// Length of the measured window.
    pub duration: Duration,
    /// Load that is sent but not measured, before the window.
    pub warmup: Duration,
    /// Open loop: most requests in flight at once. Closed loop: the number of clients.
    pub connections: usize,
    /// Open loop: how many requests may be due and waiting for a connection before further ones
    /// are dropped (and counted).
    pub max_queue: usize,
    pub seed: u64,
    pub request: RequestSpec,
}

impl RunConfig {
    fn check(&self) -> Result<(), String> {
        if self.connections == 0 {
            return Err("--connections must be at least 1".to_string());
        }
        if self.duration.is_zero() {
            return Err("--duration must be longer than zero".to_string());
        }
        if self.mode == Mode::Open && self.max_queue == 0 {
            return Err("--max-queue must be at least 1".to_string());
        }
        Ok(())
    }

    fn title(&self) -> String {
        let window = format!(
            "{:.1} s measured after {:.1} s of warm-up, {} connections",
            self.duration.as_secs_f64(),
            self.warmup.as_secs_f64(),
            self.connections
        );
        match self.mode {
            Mode::Open => {
                let process = match self.process {
                    Process::Poisson => "poisson",
                    Process::Constant => "constant",
                };
                format!(
                    "llm-loadgen  open loop, {process} {:.1} req/s, {window}",
                    self.rate
                )
            }
            Mode::Closed => format!("llm-loadgen  closed loop, {window}"),
        }
    }
}

/// What a run produced, before it becomes a report.
struct Measured {
    stats: Stats,
    offered: u64,
    /// When the measured window began.
    from: Instant,
    /// When the last measured request finished, if any did.
    last_done: Option<Instant>,
}

pub fn run(cfg: &RunConfig) -> Result<Report, String> {
    cfg.check()?;
    let measured = match cfg.mode {
        Mode::Open => run_open(cfg)?,
        Mode::Closed => run_closed(cfg),
    };
    let drained = measured.last_done.map_or(Duration::ZERO, |t| {
        t.saturating_duration_since(measured.from)
    });
    Ok(Report {
        title: cfg.title(),
        target_rate: (cfg.mode == Mode::Open).then_some(cfg.rate),
        offered: measured.offered,
        window: cfg.duration,
        // a server that is behind keeps working after the window: throughput is judged over
        // the whole time it took to answer what was asked in the window
        span: drained.max(cfg.duration),
        stats: measured.stats,
    })
}

fn sleep_until(moment: Instant) {
    let wait = moment.saturating_duration_since(Instant::now());
    if !wait.is_zero() {
        thread::sleep(wait);
    }
}

struct Job {
    intended: Instant,
    measured: bool,
}

/// Requests that are due and waiting for a connection.
struct Queue {
    jobs: Mutex<(VecDeque<Job>, bool)>,
    ready: Condvar,
}

impl Queue {
    /// `false` if the queue is full and the job was dropped.
    fn push(&self, job: Job, limit: usize) -> bool {
        let mut guard = self.jobs.lock().unwrap();
        if guard.0.len() >= limit {
            return false;
        }
        guard.0.push_back(job);
        self.ready.notify_one();
        true
    }

    fn close(&self) {
        self.jobs.lock().unwrap().1 = true;
        self.ready.notify_all();
    }

    /// The next job, waiting for one; `None` once the queue is closed and empty.
    fn pop(&self) -> Option<Job> {
        let mut guard = self.jobs.lock().unwrap();
        loop {
            if let Some(job) = guard.0.pop_front() {
                return Some(job);
            }
            if guard.1 {
                return None;
            }
            guard = self.ready.wait(guard).unwrap();
        }
    }
}

fn run_open(cfg: &RunConfig) -> Result<Measured, String> {
    let schedule = Schedule::new(cfg.process, cfg.rate, cfg.seed)?;
    let queue = Arc::new(Queue {
        jobs: Mutex::new((VecDeque::new(), false)),
        ready: Condvar::new(),
    });
    let workers: Vec<_> = (0..cfg.connections)
        .map(|_| {
            let queue = Arc::clone(&queue);
            let spec = cfg.request.clone();
            thread::spawn(move || {
                let mut stats = Stats::default();
                let mut last_done = None;
                while let Some(job) = queue.pop() {
                    let sample = client::run_one(&spec, job.intended);
                    if job.measured {
                        stats.record(&sample);
                        last_done = Some(Instant::now());
                    }
                }
                (stats, last_done)
            })
        })
        .collect();

    let origin = Instant::now() + Duration::from_millis(20); // time for the workers to start
    let end = cfg.warmup + cfg.duration;
    let (mut offered, mut dropped) = (0u64, 0u64);
    for offset in schedule {
        if offset >= end {
            break;
        }
        let intended = origin + offset;
        sleep_until(intended);
        let measured = offset >= cfg.warmup;
        offered += u64::from(measured);
        if !queue.push(Job { intended, measured }, cfg.max_queue) && measured {
            dropped += 1;
        }
    }
    queue.close();

    let mut stats = Stats::default();
    let mut last_done: Option<Instant> = None;
    for worker in workers {
        let (s, done) = worker
            .join()
            .map_err(|_| "a worker thread panicked".to_string())?;
        stats.merge(&s);
        last_done = last_done.max(done);
    }
    stats.dropped = dropped;
    Ok(Measured {
        stats,
        offered,
        from: origin + cfg.warmup,
        last_done,
    })
}

fn run_closed(cfg: &RunConfig) -> Measured {
    let origin = Instant::now();
    let end = origin + cfg.warmup + cfg.duration;
    let from = origin + cfg.warmup;
    let clients: Vec<_> = (0..cfg.connections)
        .map(|_| {
            let spec = cfg.request.clone();
            thread::spawn(move || {
                let mut stats = Stats::default();
                let (mut offered, mut last_done) = (0u64, None);
                while Instant::now() < end {
                    let intended = Instant::now(); // the client only "intends" what it is about to send
                    let sample = client::run_one(&spec, intended);
                    if intended >= from {
                        offered += 1;
                        stats.record(&sample);
                        last_done = Some(Instant::now());
                    }
                }
                (stats, offered, last_done)
            })
        })
        .collect();
    let mut stats = Stats::default();
    let (mut offered, mut last_done): (u64, Option<Instant>) = (0, None);
    for client in clients {
        let (s, n, done) = client.join().unwrap_or_default();
        stats.merge(&s);
        offered += n;
        last_done = last_done.max(done);
    }
    Measured {
        stats,
        offered,
        from,
        last_done,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::parse_url;
    use crate::mock::{MockConfig, Server, Stall};

    fn config(server: &Server, mode: Mode) -> RunConfig {
        RunConfig {
            mode,
            process: Process::Constant,
            rate: 20.0,
            duration: Duration::from_secs(1),
            warmup: Duration::ZERO,
            connections: 16,
            max_queue: 1000,
            seed: 1,
            request: RequestSpec {
                url: parse_url(&server.url()).unwrap(),
                model: "m".to_string(),
                prompt: "hello".to_string(),
                max_tokens: 4,
                timeout: Duration::from_secs(10),
                api_key: None,
            },
        }
    }

    fn mock(cfg: MockConfig) -> Server {
        Server::start("127.0.0.1:0", cfg).unwrap()
    }

    /// Time to first token 20 ms, 4 tokens 5 ms apart: a request takes about 35 ms.
    fn quick() -> MockConfig {
        MockConfig {
            ttft: Duration::from_millis(20),
            itl: Duration::from_millis(5),
            slots: 64,
            ..MockConfig::default()
        }
    }

    fn p(h: &crate::hist::Histogram, q: f64) -> f64 {
        h.value_at_quantile(q) as f64 / 1000.0
    }

    #[test]
    fn a_constant_open_loop_run_offers_exactly_rate_times_duration() {
        let server = mock(quick());
        let mut cfg = config(&server, Mode::Open);
        cfg.rate = 10.0;
        let report = run(&cfg).unwrap();
        assert_eq!(report.offered, 10);
        assert_eq!(
            (report.stats.ok, report.stats.failed(), report.stats.dropped),
            (10, 0, 0)
        );
        assert_eq!(report.stats.tokens, 40);
        assert_eq!(report.target_rate, Some(10.0));
        let ttft = p(&report.stats.ttft, 0.5);
        assert!((20.0..600.0).contains(&ttft), "ttft p50 {ttft} ms");
    }

    #[test]
    fn the_warm_up_is_sent_but_not_measured() {
        let server = mock(quick());
        let mut cfg = config(&server, Mode::Open);
        cfg.rate = 10.0;
        cfg.warmup = Duration::from_millis(500);
        let report = run(&cfg).unwrap();
        assert_eq!(report.offered, 10);
        assert_eq!(report.stats.ok, 10);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            server.served(),
            15,
            "five warm-up requests and ten measured ones"
        );
    }

    #[test]
    fn a_poisson_run_offers_about_rate_times_duration() {
        let server = mock(quick());
        let mut cfg = config(&server, Mode::Open);
        cfg.process = Process::Poisson;
        cfg.rate = 100.0;
        cfg.duration = Duration::from_secs(2);
        let report = run(&cfg).unwrap();
        assert!((140..=260).contains(&report.offered), "{}", report.offered);
        assert_eq!(
            report.stats.ok + report.stats.failed() + report.stats.dropped,
            report.offered
        );
    }

    #[test]
    fn a_closed_loop_client_sends_the_next_request_when_the_last_one_is_done() {
        let server = mock(MockConfig {
            ttft: Duration::from_millis(100),
            itl: Duration::ZERO,
            ..quick()
        });
        let mut cfg = config(&server, Mode::Closed);
        cfg.connections = 2;
        let report = run(&cfg).unwrap();
        // 2 clients, about 100 ms per request, 1 s: about 20, never more than 22
        assert!((8..=22).contains(&report.offered), "{}", report.offered);
        assert_eq!(report.stats.ok, report.offered);
        assert_eq!(report.target_rate, None);
        assert!(report.title.contains("closed loop"));
    }

    #[test]
    fn a_stalled_server_cannot_hide_behind_an_open_loop() {
        // Fast requests (about 10 ms), but the server freezes for a second 0.3 s into the run.
        let stall = Stall {
            after: Duration::from_millis(300),
            length: Duration::from_millis(1000),
        };
        let cfg = MockConfig {
            stall: Some(stall),
            ttft: Duration::from_millis(10),
            itl: Duration::ZERO,
            ..quick()
        };
        let open_server = mock(cfg.clone());
        let mut open = config(&open_server, Mode::Open);
        open.rate = 50.0;
        open.duration = Duration::from_secs(2);
        open.connections = 200;
        let open = run(&open).unwrap();

        let closed_server = mock(cfg);
        let mut closed = config(&closed_server, Mode::Closed);
        closed.duration = Duration::from_secs(2);
        closed.connections = 4;
        let closed = run(&closed).unwrap();

        let (open_p90, closed_p90) = (p(&open.stats.e2e, 0.9), p(&closed.stats.e2e, 0.9));
        // half of the open-loop requests were meant to start during the freeze, so their
        // latency is the rest of the freeze: the top decile sits well above 500 ms
        assert!(open_p90 > 500.0, "open loop p90 {open_p90} ms");
        // the closed loop only has 4 requests caught by the freeze, and says it is fast
        assert!(closed_p90 < 250.0, "closed loop p90 {closed_p90} ms");
        assert!(open_p90 > 2.0 * closed_p90);
        assert_eq!(
            open.stats.ok, open.offered,
            "nothing failed, it was only slow"
        );
        // and the closed loop silently sent far fewer requests than the 100 that were due
        assert!(closed.offered > 4 * 10);
    }

    #[test]
    fn an_overloaded_server_shows_its_queue_in_an_open_loop() {
        // 2 slots of about 100 ms: it can serve 20 requests per second. We offer 40.
        let server = mock(MockConfig {
            slots: 2,
            ttft: Duration::from_millis(100),
            itl: Duration::ZERO,
            ..quick()
        });
        let mut cfg = config(&server, Mode::Open);
        cfg.rate = 40.0;
        cfg.duration = Duration::from_secs(2);
        cfg.connections = 200;
        let open = run(&cfg).unwrap();

        let mut cfg = config(&server, Mode::Closed);
        cfg.duration = Duration::from_secs(2);
        cfg.connections = 2;
        let closed = run(&cfg).unwrap();

        let (open_p99, closed_p99) = (p(&open.stats.e2e, 0.99), p(&closed.stats.e2e, 0.99));
        assert!(
            open_p99 > 500.0,
            "open loop p99 {open_p99} ms: the backlog grows all run long"
        );
        assert!(
            closed_p99 < 600.0,
            "closed loop p99 {closed_p99} ms: two clients never queue"
        );
        assert!(
            open.rate() < 25.0,
            "the server cannot do better than its 20 req/s: {}",
            open.rate()
        );
    }

    #[test]
    fn requests_beyond_the_queue_limit_are_dropped_and_counted() {
        let server = mock(MockConfig {
            ttft: Duration::from_millis(200),
            itl: Duration::ZERO,
            ..quick()
        });
        let mut cfg = config(&server, Mode::Open);
        cfg.rate = 50.0;
        cfg.connections = 1;
        cfg.max_queue = 2;
        let report = run(&cfg).unwrap();
        assert_eq!(report.offered, 50);
        assert!(
            report.stats.dropped >= 30,
            "dropped {}",
            report.stats.dropped
        );
        assert_eq!(
            report.stats.ok + report.stats.failed() + report.stats.dropped,
            report.offered
        );
        assert!(
            report.stats.lag.max() >= 100_000,
            "queued requests start late and the lag says so"
        );
        let text = report.text();
        assert!(
            text.contains("were dropped") && text.contains("started late"),
            "{text}"
        );
    }

    #[test]
    fn errors_are_counted_and_do_not_end_the_run() {
        let server = mock(MockConfig {
            error_status: Some(503),
            ..quick()
        });
        let mut cfg = config(&server, Mode::Open);
        cfg.rate = 10.0;
        let report = run(&cfg).unwrap();
        assert_eq!(report.stats.ok, 0);
        assert_eq!(report.stats.errors["status 503"], 10);
        assert!(report.stats.e2e.is_empty());
    }

    #[test]
    fn a_dead_server_is_reported_not_hidden() {
        let mut server = mock(quick());
        let mut cfg = config(&server, Mode::Open);
        server.stop();
        drop(server);
        cfg.rate = 10.0;
        let report = run(&cfg).unwrap();
        assert_eq!(report.stats.ok, 0);
        assert_eq!(report.stats.failed(), 10);
        assert!(report
            .stats
            .errors
            .keys()
            .all(|k| k.starts_with("failed: connect")));
    }

    #[test]
    fn bad_settings_are_refused_before_anything_is_sent() {
        let server = mock(quick());
        let base = config(&server, Mode::Open);
        let mut c = base.clone();
        c.rate = 0.0;
        assert!(run(&c).unwrap_err().contains("positive"));
        let mut c = base.clone();
        c.connections = 0;
        assert!(run(&c).unwrap_err().contains("--connections"));
        let mut c = base.clone();
        c.duration = Duration::ZERO;
        assert!(run(&c).unwrap_err().contains("--duration"));
        let mut c = base;
        c.max_queue = 0;
        assert!(run(&c).unwrap_err().contains("--max-queue"));
        assert_eq!(server.served(), 0);
    }

    #[test]
    fn the_title_says_what_was_run() {
        let server = mock(quick());
        let mut cfg = config(&server, Mode::Open);
        cfg.process = Process::Poisson;
        cfg.warmup = Duration::from_secs(2);
        let title = cfg.title();
        assert!(title.contains("open loop, poisson 20.0 req/s"), "{title}");
        assert!(
            title.contains("1.0 s measured after 2.0 s of warm-up, 16 connections"),
            "{title}"
        );
    }
}
