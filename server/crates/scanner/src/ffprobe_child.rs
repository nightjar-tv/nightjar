//! Shared ownership of one ffprobe child and its two output readers.

use std::collections::VecDeque;
use std::fmt;
use std::io::Read;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The terminal class is kept distinct so callers cannot turn a lifecycle
/// failure into successful metadata or a partial keyframe map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildFailureKind {
    Spawn,
    Startup,
    Read,
    OutputBudget,
    ProcessExit,
    ReaderPanic,
    Cancelled,
    Deadline,
    Wait,
    Cleanup,
}

#[derive(Debug, Clone)]
pub(crate) struct ChildFailure {
    pub(crate) kind: ChildFailureKind,
    detail: String,
}

impl ChildFailure {
    pub(crate) fn new(kind: ChildFailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.detail
    }

    fn include(mut self, other: ChildFailure) -> Self {
        self.detail.push_str("; ");
        self.detail.push_str(other.message());
        self
    }
}

impl fmt::Display for ChildFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl From<std::io::Error> for ChildFailure {
    fn from(error: std::io::Error) -> Self {
        Self::new(ChildFailureKind::Read, error.to_string())
    }
}

#[derive(Debug)]
pub(crate) struct CompletedChild<Stdout, Stderr> {
    pub(crate) status: Result<ExitStatus, ChildFailure>,
    pub(crate) stdout: Stdout,
    pub(crate) stderr: Stderr,
}

/// Last diagnostic bytes retained while the stderr pipe continues draining.
/// The byte limit is an approved operational policy, not a delivery limit.
pub(crate) struct StderrTail {
    bytes: VecDeque<u8>,
}

impl StderrTail {
    pub(crate) fn read<R: Read>(mut reader: R, limit: usize) -> Result<Self, ChildFailure> {
        let mut bytes = VecDeque::with_capacity(limit);
        let mut chunk = [0_u8; 4096];
        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                return Ok(Self { bytes });
            }
            for byte in &chunk[..read] {
                if bytes.len() == limit {
                    bytes.pop_front();
                }
                bytes.push_back(*byte);
            }
        }
    }

    pub(crate) fn display(&self) -> String {
        if self.bytes.is_empty() {
            return "(no stderr)".into();
        }
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        let start = bytes
            .iter()
            .position(|byte| byte & 0b1100_0000 != 0b1000_0000)
            .unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }

    #[cfg(test)]
    pub(crate) fn retained_len(&self) -> usize {
        self.bytes.len()
    }

    #[cfg(test)]
    pub(crate) fn retained_capacity(&self) -> usize {
        self.bytes.capacity()
    }
}

/// Supervise both output readers until the child has exited and both readers
/// have joined successfully. The durations are operational refusal policies,
/// not measured universal maxima.
pub(crate) fn supervise<Stdout, Stderr, ReadStdout, ReadStderr>(
    mut child: Child,
    deadline: Duration,
    poll_interval: Duration,
    should_cancel: Option<&dyn Fn() -> bool>,
    read_stdout: ReadStdout,
    read_stderr: ReadStderr,
) -> Result<CompletedChild<Stdout, Stderr>, ChildFailure>
where
    Stdout: Send + 'static,
    Stderr: Send + 'static,
    ReadStdout: FnOnce(ChildStdout) -> Result<Stdout, ChildFailure> + Send + 'static,
    ReadStderr: FnOnce(ChildStderr) -> Result<Stderr, ChildFailure> + Send + 'static,
{
    let started = Instant::now();
    let (events_tx, events_rx) = mpsc::channel();
    let stdout = match start_reader(
        "ffprobe-stdout",
        child.stdout.take(),
        events_tx.clone(),
        read_stdout,
    ) {
        Ok(reader) => reader,
        Err(error) => return Err(error.include(clean_up(&mut child))),
    };
    let stderr = match start_reader(
        "ffprobe-stderr",
        child.stderr.take(),
        events_tx.clone(),
        read_stderr,
    ) {
        Ok(reader) => reader,
        Err(error) => {
            let cleanup = clean_up(&mut child);
            return Err(include_join(error.include(cleanup), stdout, "stdout"));
        }
    };
    drop(events_tx);

    let mut finished_readers = 0usize;
    let mut readers_disconnected = false;
    let mut child_status = None;
    let status = loop {
        if should_cancel.is_some_and(|cancel| cancel()) {
            let error = ChildFailure::new(
                ChildFailureKind::Cancelled,
                "ffprobe cancelled (library unreachable)",
            );
            return Err(finish_after_failure(error, &mut child, stdout, stderr));
        }
        if started.elapsed() >= deadline {
            let error = ChildFailure::new(ChildFailureKind::Deadline, "ffprobe deadline exceeded");
            return Err(finish_after_failure(error, &mut child, stdout, stderr));
        }
        if child_status.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => child_status = Some(status),
                Ok(None) => {}
                Err(error) => {
                    let error =
                        ChildFailure::new(ChildFailureKind::Wait, format!("wait ffprobe: {error}"));
                    return Err(finish_after_failure(error, &mut child, stdout, stderr));
                }
            }
        }

        // EOF and child exit do not imply reader completion. Keep observing
        // cancellation and the deadline until both threads can be joined
        // without waiting for unfinished output processing.
        if let Some(status) = child_status
            && finished_readers == 2
            && stdout.is_finished()
            && stderr.is_finished()
        {
            break status;
        }

        let remaining = deadline.saturating_sub(started.elapsed());
        let completion_pending = finished_readers == 2 && child_status.is_some();
        let wait = if completion_pending {
            Duration::from_millis(1).min(remaining)
        } else {
            poll_interval.min(remaining)
        };
        if readers_disconnected {
            thread::park_timeout(Duration::from_millis(1).min(remaining));
            continue;
        }
        match events_rx.recv_timeout(wait) {
            Ok(ReaderEvent::Finished) => finished_readers += 1,
            Ok(ReaderEvent::Failed(kind, detail)) => {
                let error = ChildFailure::new(kind, detail);
                return Err(finish_after_failure(error, &mut child, stdout, stderr));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if finished_readers < 2 {
                    let error = ChildFailure::new(
                        ChildFailureKind::ReaderPanic,
                        "ffprobe reader ended without reporting completion",
                    );
                    return Err(finish_after_failure(error, &mut child, stdout, stderr));
                }
                if let Some(status) = child_status {
                    break status;
                }
                readers_disconnected = true;
            }
        }
    };

    let stdout = join_reader(stdout, "stdout");
    let stderr = join_reader(stderr, "stderr");
    let status = if status.success() {
        Ok(status)
    } else {
        let code = status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".into());
        Err(ChildFailure::new(
            ChildFailureKind::ProcessExit,
            format!("ffprobe failed (exit {code})"),
        ))
    };
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => Ok(CompletedChild {
            status,
            stdout,
            stderr,
        }),
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(other)) => Err(error.include(other)),
    }
}

pub(crate) fn spawn(command: &mut Command) -> Result<Child, ChildFailure> {
    command.spawn().map_err(|error| {
        let detail = if error.kind() == std::io::ErrorKind::NotFound {
            "spawn ffprobe: not found on PATH".into()
        } else {
            format!("spawn ffprobe: {error}")
        };
        ChildFailure::new(ChildFailureKind::Spawn, detail)
    })
}

fn start_reader<Value, Pipe, Reader>(
    name: &str,
    pipe: Option<Pipe>,
    events: mpsc::Sender<ReaderEvent>,
    read: Reader,
) -> Result<JoinHandle<Result<Value, ChildFailure>>, ChildFailure>
where
    Value: Send + 'static,
    Pipe: Send + 'static,
    Reader: FnOnce(Pipe) -> Result<Value, ChildFailure> + Send + 'static,
{
    let pipe = pipe.ok_or_else(|| {
        ChildFailure::new(
            ChildFailureKind::Startup,
            format!("{name}: child pipe missing"),
        )
    })?;
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read(pipe))) {
                Ok(result) => {
                    let event = match &result {
                        Ok(_) => ReaderEvent::Finished,
                        Err(error) => ReaderEvent::Failed(error.kind, error.detail.clone()),
                    };
                    let _ = events.send(event);
                    result
                }
                Err(panic) => {
                    let _ = events.send(ReaderEvent::Failed(
                        ChildFailureKind::ReaderPanic,
                        "ffprobe reader panicked".into(),
                    ));
                    std::panic::resume_unwind(panic)
                }
            }
        })
        .map_err(|error| {
            ChildFailure::new(
                ChildFailureKind::Startup,
                format!("spawn {name} reader: {error}"),
            )
        })
}

enum ReaderEvent {
    Finished,
    Failed(ChildFailureKind, String),
}

fn finish_after_failure<Stdout, Stderr>(
    error: ChildFailure,
    child: &mut Child,
    stdout: JoinHandle<Result<Stdout, ChildFailure>>,
    stderr: JoinHandle<Result<Stderr, ChildFailure>>,
) -> ChildFailure
where
    Stdout: Send + 'static,
    Stderr: Send + 'static,
{
    let error = error.include(clean_up(child));
    let error = include_join(error, stdout, "stdout");
    include_join(error, stderr, "stderr")
}

fn include_join<Value>(
    failure: ChildFailure,
    reader: JoinHandle<Result<Value, ChildFailure>>,
    name: &str,
) -> ChildFailure {
    match join_reader(reader, name) {
        Ok(_) => failure,
        Err(error) => failure.include(error),
    }
}

fn clean_up(child: &mut Child) -> ChildFailure {
    let mut details = Vec::new();
    if let Err(error) = child.kill() {
        details.push(format!("kill ffprobe: {error}"));
    }
    if let Err(error) = child.wait() {
        details.push(format!("reap ffprobe: {error}"));
    }
    if details.is_empty() {
        ChildFailure::new(ChildFailureKind::Cleanup, "child killed and reaped")
    } else {
        ChildFailure::new(ChildFailureKind::Cleanup, details.join(", "))
    }
}

fn join_reader<Value>(
    reader: JoinHandle<Result<Value, ChildFailure>>,
    name: &str,
) -> Result<Value, ChildFailure> {
    match reader.join() {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(ChildFailure::new(
            error.kind,
            format!("ffprobe {name} reader: {error}"),
        )),
        Err(_) => Err(ChildFailure::new(
            ChildFailureKind::ReaderPanic,
            format!("ffprobe {name} reader panicked"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Write;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn process_exit_is_explicitly_classified() {
        let completed = supervise(
            child("printf diagnostic >&2; exit 7"),
            Duration::from_secs(2),
            Duration::from_millis(1),
            None,
            drained,
            drained,
        )
        .unwrap();
        let failure = completed.status.unwrap_err();
        assert_eq!(failure.kind, ChildFailureKind::ProcessExit);
        assert!(failure.message().contains("exit 7"));
        assert_eq!(completed.stderr, b"diagnostic");
    }

    #[test]
    fn output_budget_classification_survives_reader_cleanup() {
        let failure = supervise(
            child("exec sleep 30"),
            Duration::from_secs(2),
            Duration::from_millis(1),
            None,
            |_| -> Result<Vec<u8>, ChildFailure> {
                Err(ChildFailure::new(
                    ChildFailureKind::OutputBudget,
                    "injected output policy refusal",
                ))
            },
            drained,
        )
        .unwrap_err();
        assert_eq!(failure.kind, ChildFailureKind::OutputBudget);
    }

    #[test]
    fn stderr_budget_triplet_and_utf8_boundaries() {
        for delivered in [3, 4, 5] {
            let mut input = Cursor::new(vec![b'x'; delivered]);
            let tail = StderrTail::read(&mut input, 4).unwrap();
            assert_eq!(input.position() as usize, delivered);
            assert_eq!(tail.retained_len(), delivered.min(4));
            assert_eq!(tail.retained_capacity(), 4);
            assert_eq!(tail.display(), "x".repeat(delivered.min(4)));
            eprintln!(
                "stderr delivered={delivered} retained={} capacity={}",
                tail.retained_len(),
                tail.retained_capacity()
            );
        }
        for (input, limit, expected) in [
            ("a€z".as_bytes(), 4, "€z"),
            ("a€z".as_bytes(), 3, "z"),
            (&[b'a', 0xff, b'z'][..], 2, "\u{fffd}z"),
            (&[b'a', 0xe2, 0x82][..], 2, "\u{fffd}"),
        ] {
            let tail = StderrTail::read(Cursor::new(input), limit).unwrap();
            assert_eq!(tail.display(), expected);
            assert!(tail.retained_len() <= limit);
            assert!(tail.retained_capacity() <= limit);
        }
    }

    fn delayed_readers_after_exit(cancel: bool) {
        let mut child = child("printf out; printf err >&2");
        assert!(child.wait().unwrap().success());
        assert!(child.try_wait().unwrap().is_some());
        let finished = Arc::new(AtomicUsize::new(0));
        let stdout_finished = Arc::clone(&finished);
        let stderr_finished = Arc::clone(&finished);
        let (stdout_release, stdout_wait) = mpsc::channel();
        let (stderr_release, stderr_wait) = mpsc::channel();
        let observations = AtomicUsize::new(0);
        let started = Instant::now();
        let trigger = Duration::from_millis(40);
        let should_cancel = || {
            observations.fetch_add(1, Ordering::SeqCst);
            if cancel && started.elapsed() >= trigger {
                stdout_release.send(()).ok();
                stderr_release.send(()).ok();
                cancel
            } else {
                false
            }
        };
        let failure = supervise(
            child,
            if cancel {
                Duration::from_secs(2)
            } else {
                trigger
            },
            Duration::from_millis(1),
            Some(&should_cancel),
            move |pipe| {
                let bytes = drained(pipe)?;
                assert_eq!(bytes, b"out");
                // The fallback releases the old implementation's blocking
                // join so the regression fails instead of hanging.
                stdout_wait.recv_timeout(Duration::from_secs(1)).ok();
                stdout_finished.fetch_add(1, Ordering::SeqCst);
                Ok(bytes)
            },
            move |pipe| {
                let bytes = drained(pipe)?;
                assert_eq!(bytes, b"err");
                stderr_wait.recv_timeout(Duration::from_secs(1)).ok();
                stderr_finished.fetch_add(1, Ordering::SeqCst);
                Ok(bytes)
            },
        )
        .expect_err("child exit must not disable cancellation or deadline");
        assert_eq!(
            failure.kind,
            if cancel {
                ChildFailureKind::Cancelled
            } else {
                ChildFailureKind::Deadline
            }
        );
        assert!(observations.load(Ordering::SeqCst) > 1);
        assert_eq!(finished.load(Ordering::SeqCst), 2);
        eprintln!(
            "post-exit kind={:?} observations={} readers_finished={} elapsed={:?}",
            failure.kind,
            observations.load(Ordering::SeqCst),
            finished.load(Ordering::SeqCst),
            started.elapsed()
        );
    }

    #[test]
    fn cancellation_remains_active_after_child_exit_with_delayed_readers() {
        delayed_readers_after_exit(true);
    }

    #[test]
    fn deadline_remains_active_after_child_exit_with_delayed_readers() {
        delayed_readers_after_exit(false);
    }

    #[test]
    fn stderr_read_failure_finishes_both_readers() {
        let finished = Arc::new(AtomicUsize::new(0));
        let stdout_finished = Arc::clone(&finished);
        let stderr_finished = Arc::clone(&finished);
        let failure = supervise(
            child("exec sleep 30"),
            Duration::from_secs(2),
            Duration::from_millis(1),
            None,
            move |pipe| {
                let result = drained(pipe);
                stdout_finished.fetch_add(1, Ordering::SeqCst);
                result
            },
            move |_| -> Result<Vec<u8>, ChildFailure> {
                stderr_finished.fetch_add(1, Ordering::SeqCst);
                Err(std::io::Error::other("injected stderr read error").into())
            },
        )
        .unwrap_err();
        assert_eq!(failure.kind, ChildFailureKind::Read);
        assert_eq!(finished.load(Ordering::SeqCst), 2);
    }

    // Invoked in a directly spawned test executable, with no subprocesses.
    #[test]
    #[ignore = "child entrypoint invoked by simultaneous_pipe_pressure"]
    fn pressure_child() {
        let barrier = Arc::new(Barrier::new(2));
        let stderr_barrier = Arc::clone(&barrier);
        let stderr = thread::spawn(move || {
            let mut output = std::io::stderr().lock();
            stderr_barrier.wait();
            for _ in 0..256 {
                output.write_all(&[0xa6; 4096]).unwrap();
            }
            output.flush().unwrap();
        });
        {
            let mut output = std::io::stdout().lock();
            barrier.wait();
            for _ in 0..256 {
                output.write_all(&[0xa5; 4096]).unwrap();
            }
            output.flush().unwrap();
        }
        stderr.join().unwrap();
        std::process::exit(0);
    }

    struct CountingReader<R> {
        reader: R,
        bytes: usize,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let count = self.reader.read(output)?;
            self.bytes += count;
            Ok(count)
        }
    }

    #[test]
    fn simultaneous_pipe_pressure() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "ffprobe_child::tests::pressure_child",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "positive liveness control"
        );
        let pid = child.id();
        let completed = supervise(
            child,
            Duration::from_secs(10),
            Duration::from_millis(1),
            None,
            |mut pipe| {
                let mut count = 0;
                let mut chunk = [0_u8; 4096];
                loop {
                    let read = pipe.read(&mut chunk)?;
                    if read == 0 {
                        return Ok(count);
                    }
                    count += chunk[..read].iter().filter(|byte| **byte == 0xa5).count();
                }
            },
            |pipe| {
                let mut reader = CountingReader {
                    reader: pipe,
                    bytes: 0,
                };
                let tail = StderrTail::read(&mut reader, 4096)?;
                Ok((reader.bytes, tail))
            },
        )
        .unwrap();
        assert!(completed.status.unwrap().success());
        assert_eq!(completed.stdout, 1024 * 1024);
        let (stderr_bytes, tail) = completed.stderr;
        assert_eq!(stderr_bytes, 1024 * 1024);
        assert_eq!(tail.retained_len(), 4096);
        assert_eq!(tail.retained_capacity(), 4096);
        assert!(tail.bytes.iter().all(|byte| *byte == 0xa6));
        let survivor = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .output()
            .expect("observe child after owner completion");
        assert!(!survivor.status.success());
        eprintln!(
            "pressure pid={pid} stdout_bytes={} stderr_bytes={stderr_bytes} stderr_capacity={} survivor={}",
            completed.stdout,
            tail.retained_capacity(),
            survivor.status.success()
        );
    }

    fn drained<R: Read>(mut reader: R) -> Result<Vec<u8>, ChildFailure> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn child(script: &str) -> Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn direct test child")
    }

    #[test]
    fn keeps_a_live_child_under_supervision_after_both_pipes_close() {
        let child = child("printf out; printf err >&2; exec 1>&- 2>&-; exec sleep 30");
        let pid = child.id();
        let result = supervise(
            child,
            Duration::from_millis(40),
            Duration::from_millis(1),
            None,
            drained,
            drained,
        )
        .expect_err("a live child after EOF must hit its deadline");
        assert_eq!(result.kind, ChildFailureKind::Deadline, "{result:?}");
        assert!(result.message().contains("child killed and reaped"));
        let survivor = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(!survivor, "child {pid} survived the owner");
    }

    #[test]
    fn disconnected_readers_do_not_add_a_poll_interval_after_child_exit() {
        let started = Instant::now();
        let completed = supervise(
            child("exec 1>&- 2>&-; exec sleep 0.01"),
            Duration::from_secs(2),
            Duration::from_secs(5),
            None,
            drained,
            drained,
        )
        .expect("disconnected readers must still reap a child that exits");
        assert!(completed.status.unwrap().success());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "disconnected-reader supervision took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn joins_both_readers_and_keeps_known_pipe_bytes() {
        let done = supervise(
            child("printf stdout; printf stderr >&2"),
            Duration::from_secs(1),
            Duration::from_millis(1),
            None,
            drained,
            drained,
        )
        .expect("successful child and readers");
        assert!(done.status.unwrap().success());
        assert_eq!(done.stdout, b"stdout");
        assert_eq!(done.stderr, b"stderr");
    }

    #[test]
    fn cancellation_after_early_eof_reaps_the_child() {
        let error = supervise(
            child("exec 1>&- 2>&-; exec sleep 30"),
            Duration::from_secs(30),
            Duration::from_millis(1),
            Some(&|| true),
            drained,
            drained,
        )
        .expect_err("cancellation wins after EOF");
        assert_eq!(error.kind, ChildFailureKind::Cancelled);
        assert!(error.message().contains("child killed and reaped"));
    }

    #[test]
    fn cancellation_wins_after_eof_and_child_exit_rendezvous() {
        let child = child("printf out; printf err >&2; exit 0");
        let pid = child.id();
        let eof = Arc::new(AtomicUsize::new(0));
        let stdout_eof = Arc::clone(&eof);
        let stderr_eof = Arc::clone(&eof);
        let cancel = || eof.load(Ordering::Acquire) == 2;
        let error = supervise(
            child,
            Duration::from_secs(2),
            Duration::from_millis(50),
            Some(&cancel),
            move |pipe| {
                let bytes = drained(pipe)?;
                stdout_eof.fetch_add(1, Ordering::Release);
                Ok(bytes)
            },
            move |pipe| {
                let bytes = drained(pipe)?;
                stderr_eof.fetch_add(1, Ordering::Release);
                Ok(bytes)
            },
        )
        .expect_err("cancellation must be observed after the EOF/exit rendezvous");
        assert_eq!(error.kind, ChildFailureKind::Cancelled);
        assert!(error.message().contains("child killed and reaped"));
        assert!(
            !Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("probe child liveness")
                .success()
        );
    }

    #[test]
    fn finished_events_precede_reader_thread_return() {
        let ready = Arc::new(AtomicUsize::new(0));
        let returned = Arc::new(AtomicUsize::new(0));
        let stdout_ready = Arc::clone(&ready);
        let stderr_ready = Arc::clone(&ready);
        let stdout_returned = Arc::clone(&returned);
        let stderr_returned = Arc::clone(&returned);
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let stdout_release = Arc::clone(&release_rx);
        let stderr_release = Arc::clone(&release_rx);
        let release = thread::spawn(move || {
            while ready.load(Ordering::Acquire) != 2 {
                thread::yield_now();
            }
            release_tx.send(()).unwrap();
            release_tx.send(()).unwrap();
        });
        let completed = supervise(
            child("printf out; printf err >&2"),
            Duration::from_secs(2),
            Duration::from_millis(50),
            None,
            move |pipe| {
                let bytes = drained(pipe)?;
                stdout_ready.fetch_add(1, Ordering::Release);
                stdout_release.lock().unwrap().recv().unwrap();
                stdout_returned.fetch_add(1, Ordering::Release);
                Ok(bytes)
            },
            move |pipe| {
                let bytes = drained(pipe)?;
                stderr_ready.fetch_add(1, Ordering::Release);
                stderr_release.lock().unwrap().recv().unwrap();
                stderr_returned.fetch_add(1, Ordering::Release);
                Ok(bytes)
            },
        )
        .expect("owner must converge after both reader threads return");
        release.join().unwrap();
        assert!(completed.status.unwrap().success());
        assert_eq!(returned.load(Ordering::Acquire), 2);
    }

    #[test]
    fn cancellation_before_eof_reaps_the_child() {
        let error = supervise(
            child("exec sleep 30"),
            Duration::from_secs(30),
            Duration::from_millis(1),
            Some(&|| true),
            drained,
            drained,
        )
        .expect_err("cancellation wins while pipes remain open");
        assert_eq!(error.kind, ChildFailureKind::Cancelled);
        assert!(error.message().contains("child killed and reaped"));
    }

    #[test]
    fn reader_errors_and_panics_kill_and_join_the_other_reader() {
        let read_error = supervise(
            child("exec sleep 30"),
            Duration::from_secs(30),
            Duration::from_millis(1),
            None,
            |_| -> Result<Vec<u8>, ChildFailure> {
                Err(ChildFailure::new(
                    ChildFailureKind::Read,
                    "injected stdout read error",
                ))
            },
            drained,
        )
        .expect_err("reader failure is terminal");
        assert_eq!(read_error.kind, ChildFailureKind::Read);
        assert!(read_error.message().contains("child killed and reaped"));

        let panic = supervise(
            child("exec sleep 30"),
            Duration::from_secs(30),
            Duration::from_millis(1),
            None,
            |_| -> Result<Vec<u8>, ChildFailure> { panic!("injected stdout panic") },
            drained,
        )
        .expect_err("reader panic is terminal");
        assert_eq!(panic.kind, ChildFailureKind::ReaderPanic);
        assert!(panic.message().contains("child killed and reaped"));
    }

    #[test]
    fn partial_reader_start_failure_reaps_the_child() {
        let child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn direct test child");
        let error = supervise(
            child,
            Duration::from_secs(1),
            Duration::from_millis(1),
            None,
            drained,
            drained,
        )
        .expect_err("missing stderr pipe is a startup failure");
        assert_eq!(error.kind, ChildFailureKind::Startup);
        assert!(error.message().contains("child killed and reaped"));
    }

    #[test]
    fn spawn_failure_is_distinct() {
        let error = spawn(&mut Command::new("/definitely/not/a/nightjar-ffprobe"))
            .expect_err("nonexistent executable cannot spawn");
        assert_eq!(error.kind, ChildFailureKind::Spawn);
    }

    #[test]
    fn stderr_tail_discards_excess_while_continuing_to_drain() {
        let tail = StderrTail::read(Cursor::new(vec![b'x'; 4_097]), 4_096)
            .expect("tail reader drains all bytes");
        assert_eq!(tail.retained_len(), 4_096);
        assert!(tail.retained_capacity() <= 4_096);
        assert_eq!(tail.display().len(), 4_096);
    }
}
