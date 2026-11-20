//! Shutdown follows process ancestry, not process groups: terminals, Claude
//! and Bash jobs can all lead separate sessions inside one workset.
use std::collections::HashMap;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitId, WaitIdOptions, WaitOptions, pidfd_open, pidfd_send_signal,
    wait, waitid,
};

/// A process's children may have been forked by any of its threads. Tasks
/// that disappear mid-read are covered by rescanning during shutdown.
pub(crate) fn children(pid: Pid) -> impl Iterator<Item = Pid> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|thread| std::fs::read_to_string(thread.path().join("children")).ok())
        .flat_map(|children| {
            children
                .split_whitespace()
                .filter_map(|pid| Pid::from_raw(pid.parse().ok()?))
                .collect::<Vec<_>>()
        })
}

fn running(process: BorrowedFd<'_>) -> bool {
    // A pidfd becomes readable when that process exits, even before reap.
    let mut fds = [PollFd::from_borrowed_fd(process, PollFlags::IN)];
    loop {
        match poll(&mut fds, Some(&Timespec::default())) {
            Err(rustix::io::Errno::INTR) => {}
            result => return matches!(result, Ok(0)),
        }
    }
}

/// A snapshot retained while a rejected worker has a chance to clean up.
/// Even if children are reparented during the grace, their pidfds still name
/// the same processes, never somebody else who reused a numeric pid.
#[derive(Default)]
pub(crate) struct Descendants(HashMap<Pid, OwnedFd>);

impl Descendants {
    pub(crate) fn scan(&mut self, root: Pid, worker: BorrowedFd<'_>, freeze: bool) {
        let mut parents = self.0.keys().copied().collect::<Vec<_>>();
        parents.push(root);
        while let Some(pid) = parents.pop() {
            let parent = if pid == root {
                worker
            } else {
                self.0[&pid].as_fd()
            };
            if !running(parent) {
                continue;
            }
            if freeze {
                let _ = pidfd_send_signal(parent, Signal::STOP);
            }
            let children = children(pid)
                .filter_map(|child| {
                    if self.0.get(&child).is_some_and(|fd| running(fd.as_fd())) {
                        return None;
                    }
                    let fd = pidfd_open(child, PidfdFlags::empty()).ok()?;
                    // Recheck ancestry after opening the pidfd: /proc may
                    // have named a process that went away before we opened it.
                    let status = std::fs::read_to_string(format!("/proc/{child}/status")).ok()?;
                    let ppid = status
                        .lines()
                        .find_map(|line| line.strip_prefix("PPid:"))?
                        .trim()
                        .parse::<i32>()
                        .ok()?;
                    (ppid == pid.as_raw_nonzero().get()).then_some((child, fd))
                })
                .collect::<Vec<_>>();
            if running(parent) {
                for (child, fd) in children {
                    self.0.insert(child, fd);
                    parents.push(child);
                }
            }
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.0.values().any(|fd| running(fd.as_fd()))
    }

    pub(crate) fn kill(&self) {
        for fd in self.0.values() {
            let _ = pidfd_send_signal(fd, Signal::KILL);
        }
    }
}

/// Called after stopping the executor, so no task can rely on receiving its
/// own child's status. Killing a parent brings the next layer to our
/// subreaper; repeats cover jobs that started their own session too.
pub(crate) fn terminate_children() {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        loop {
            match wait(WaitOptions::NOHANG) {
                Ok(Some(_)) | Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::CHILD) => return,
                _ => break,
            }
        }
        for pid in children(rustix::process::getpid()) {
            let Ok(child) = pidfd_open(pid, PidfdFlags::empty()) else {
                continue;
            };
            // Between reading /proc and opening the pidfd a child may
            // have been reaped. Never signal a reused PID.
            if matches!(
                waitid(
                    WaitId::PidFd(child.as_fd()),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT
                ),
                Ok(None)
            ) {
                let _ = pidfd_send_signal(&child, Signal::KILL);
            }
        }
        if Instant::now() >= deadline {
            eprintln!("rho-agent-worker: children did not exit before shutdown deadline");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
