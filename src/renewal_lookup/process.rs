use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct Control {
    pub cancelled: Arc<AtomicBool>,
}

// Reads a pipe to the end, so a chatty helper never blocks on a full pipe, but
// keeps only the last part of the output for error messages.
fn capture(mut reader: impl Read) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buffer = [0; 4096];
    while let Ok(count) = reader.read(&mut buffer) {
        if count == 0 {
            break;
        }
        tail.extend_from_slice(&buffer[..count]);
        if tail.len() > 8192 {
            tail.drain(..tail.len() - 8192);
        }
    }
    tail
}

fn failure(result: &Output) -> String {
    let text = String::from_utf8_lossy(&result.stderr);
    let lines: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let cause = lines
        .iter()
        .rev()
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            (lower.contains("npm error") || lower.contains("npm err!"))
                && !lower.contains("log")
                && !lower.contains("command")
        })
        .copied()
        .or_else(|| lines.last().copied());
    match cause {
        Some(line) => {
            let mut message: String = line.trim().chars().take(240).collect();
            if line.trim().chars().count() > 240 || result.stderr.len() >= 8192 {
                message.push('…');
            }
            message
        }
        None => format!("Renewal helper failed ({}).", result.status),
    }
}

impl Control {
    pub fn output(&self, command: &mut Command, limit: Duration) -> Result<Output, String> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err("Lookup cancelled".into());
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Could not start renewal helper: {e}"))?;
        let tree = match ProcessTree::new(&child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout = thread::spawn(move || capture(stdout));
        let stderr = thread::spawn(move || capture(stderr));
        let deadline = Instant::now() + limit;
        let status = loop {
            if self.cancelled.load(Ordering::Relaxed) {
                break Err("Lookup cancelled".to_string());
            }
            if Instant::now() >= deadline {
                break Err("Renewal lookup timed out. Please retry.".to_string());
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(error) => break Err(error.to_string()),
            }
        };
        drop(tree); // Close any browser descendants and their inherited pipes.
        if status.is_err() {
            let _ = child.kill();
        }
        let _ = child.wait();
        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        let result = Output {
            status: status?,
            stdout,
            stderr,
        };
        if result.status.success() {
            Ok(result)
        } else {
            Err(failure(&result))
        }
    }
}

#[cfg(windows)]
struct ProcessTree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ProcessTree {
    fn new(child: &std::process::Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::*;
        // A job object that kills the helper and everything it starts when its
        // handle closes. Windows closes it even if the widget crashes.
        unsafe {
            let job = Self(CreateJobObjectW(std::ptr::null(), std::ptr::null()));
            if job.0.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job.0, child.as_raw_handle()) == 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(job)
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(unix)]
struct ProcessTree(u32);
#[cfg(unix)]
impl ProcessTree {
    fn new(child: &std::process::Child) -> Result<Self, String> {
        Ok(Self(child.id()))
    }
}
#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_is_bounded() {
        assert_eq!(
            capture(std::io::Cursor::new(vec![b'x'; 100_000])).len(),
            8192
        );
    }
    #[test]
    fn timeout_and_cancel_stop_descendants() {
        let node = crate::renewal_lookup::node_candidates()
            .into_iter()
            .find(|p| p.is_file())
            .expect("Node.js is required for helper tests");
        for cancel in [false, true] {
            let cancelled = Arc::new(AtomicBool::new(false));
            let control = Control {
                cancelled: cancelled.clone(),
            };
            let setter = thread::spawn(move || {
                thread::sleep(Duration::from_millis(700));
                if cancel {
                    cancelled.store(true, Ordering::Relaxed);
                }
            });
            // The grandchild keeps the pipes open, so this hangs unless cleanup
            // kills it too.
            let result = control.output(Command::new(&node).args(["-e", "require('child_process').spawn(process.execPath,['-e','setInterval(()=>{},1000)'],{stdio:'inherit'});setInterval(()=>{},1000)"]), Duration::from_secs(1));
            assert!(
                result
                    .unwrap_err()
                    .contains(if cancel { "cancelled" } else { "timed out" })
            );
            setter.join().unwrap();
        }
    }
    #[test]
    fn failures_without_stderr_have_a_message() {
        let node = crate::renewal_lookup::node_candidates()
            .into_iter()
            .find(|p| p.is_file())
            .unwrap();
        let control = Control {
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        assert!(
            control
                .output(
                    Command::new(node).args(["-e", "process.exit(7)"]),
                    Duration::from_secs(5)
                )
                .unwrap_err()
                .contains("failed")
        );
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    #[test]
    fn install_time_does_not_reduce_the_next_stage_budget() {
        let node = crate::renewal_lookup::node_candidates()
            .into_iter()
            .find(|p| p.is_file())
            .unwrap();
        let control = Control {
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        for _ in 0..2 {
            control
                .output(
                    Command::new(&node).args(["-e", "setTimeout(()=>process.exit(0),600)"]),
                    Duration::from_secs(1),
                )
                .unwrap();
        }
    }
    #[test]
    fn verbose_npm_failure_retains_the_cause() {
        let node = crate::renewal_lookup::node_candidates()
            .into_iter()
            .find(|p| p.is_file())
            .unwrap();
        let control = Control {
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let message = control.output(Command::new(&node).args(["-e", "console.error('npm warn '+ 'x'.repeat(12000));console.error('npm error code ECONNREFUSED');console.error('npm error A complete log can be found in a file');process.exitCode=1"]), Duration::from_secs(5)).unwrap_err();
        assert!(message.contains("ECONNREFUSED"), "{message}");
        assert!(message.len() < 260);
    }
}
