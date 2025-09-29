use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::hypervisor::InterruptHandle;

/// TODO
pub trait Observer {
    /// Call before starting guest execution. Must be called on the thread that will be running the guest.
    /// # NOTE
    /// A sandbox must not be moved accross threads while being observed.
    fn start_timeout(&self, interrupt_handle: &Arc<dyn InterruptHandle>) -> Duration;

    /// Call after guest call completes. Must be called on the thread that was running the guest.
    fn stop_timeout(&self, interrupt_handle: &Arc<dyn InterruptHandle>);
}

/// TODO
pub struct CpuTimeObserver {
    shared_state: Arc<ObserverState>,
    _thread_handle: JoinHandle<()>,
}

struct ObserverState {
    should_stop: AtomicBool,
    active_monitors: Mutex<HashMap<usize, MonitoringInfo>>, // Arc pointer address as key (as usize)
    condvar: Condvar,
}

struct MonitoringInfo {
    start_cpu_time: Duration,
    thread_id: libc::pthread_t,
    interrupt_handle: Arc<dyn InterruptHandle>,
}

impl CpuTimeObserver {
    /// Creates a new CPU time observer
    ///
    /// # Arguments
    /// * `timeout` - Maximum CPU time allowed before interrupting execution
    /// * `check_interval` - How often to check for timeouts (resolution of timeout detection)
    ///   - Smaller values = more precise timeout detection but higher CPU usage
    ///   - Larger values = less precise but more CPU efficient
    ///   - Recommended: 1ms for general use, 100μs for high precision, 10ms for low overhead
    pub fn new(timeout: Duration, check_interval: Duration) -> Self {
        let shared_state = Arc::new(ObserverState {
            should_stop: AtomicBool::new(false),
            active_monitors: Mutex::new(HashMap::new()),
            condvar: Condvar::new(),
        });

        let state_clone = shared_state.clone();

        let thread_handle = thread::spawn(move || {
            observer_thread_main(state_clone, timeout, check_interval);
        });

        Self {
            shared_state,
            _thread_handle: thread_handle,
        }
    }
}

impl Observer for CpuTimeObserver {
    fn start_timeout(&self, interrupt_handle: &Arc<dyn InterruptHandle>) -> Duration {
        let thread_id = unsafe { libc::pthread_self() };
        let start_cpu_time = get_thread_cpu_time(thread_id).unwrap();

        let monitoring_info = MonitoringInfo {
            start_cpu_time,
            thread_id,
            interrupt_handle: interrupt_handle.clone(),
        };

        // Use pointer address as key. This is safe because the Arc is also part of the value, ensuring it lives as long as needed, guaranteeing uniqueness in hashmap
        let key = Arc::as_ptr(&interrupt_handle) as *const () as usize;

        self.shared_state
            .active_monitors
            .lock()
            .unwrap() // Replaces any existing entry for this handle
            .insert(key, monitoring_info);
        self.shared_state.condvar.notify_one();
        start_cpu_time
    }

    fn stop_timeout(&self, interrupt_handle: &Arc<dyn InterruptHandle>) {
        let key = Arc::as_ptr(interrupt_handle) as *const () as usize;
        self.shared_state
            .active_monitors
            .lock()
            .unwrap()
            .remove(&key);
    }
}

impl Drop for CpuTimeObserver {
    fn drop(&mut self) {
        // Signal thread to shut down
        self.shared_state.should_stop.store(true, Ordering::Relaxed);
        self.shared_state.condvar.notify_one();

        // Thread handle will be joined automatically when dropped
    }
}

fn observer_thread_main(state: Arc<ObserverState>, timeout: Duration, check_interval: Duration) {
    loop {
        // Check if we should stop
        if state.should_stop.load(Ordering::Relaxed) {
            break;
        }

        // Wait for monitors to exist
        {
            let mut monitors = state.active_monitors.lock().unwrap();
            while monitors.is_empty() && !state.should_stop.load(Ordering::Relaxed) {
                monitors = state.condvar.wait(monitors).unwrap();
            }
        }

        // Check all monitors for timeouts in a single lock acquisition
        {
            let mut monitors = state.active_monitors.lock().unwrap();

            monitors.retain(|_key, info| {
                let current_time = get_thread_cpu_time(info.thread_id).unwrap();
                let elapsed = current_time - info.start_cpu_time;
                if elapsed >= timeout {
                    info.interrupt_handle.kill();
                    false // Remove this monitor
                } else {
                    true // Keep this monitor
                }
            });
        }

        // Sleep for the check interval
        thread::sleep(check_interval);
    }
}

// CPU time measurement of given thread (as a duration since epoch)
fn get_thread_cpu_time(thread_id: libc::pthread_t) -> Result<Duration, Box<dyn std::error::Error>> {
    // Convert pthread_t to clockid_t for the specific thread
    let mut clock_id: libc::clockid_t = 0;
    let result = unsafe { libc::pthread_getcpuclockid(thread_id, &mut clock_id) };

    if result != 0 {
        return Err(
            "pthread_getcpuclockid is not supported by system or thread does not exist.".into(),
        );
    }

    let mut timespec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    let result = unsafe { libc::clock_gettime(clock_id, &mut timespec) };

    if result == 0 {
        Ok(Duration::new(
            timespec.tv_sec as u64,
            timespec.tv_nsec as u32,
        ))
    } else {
        Err("Failed to get thread CPU time".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxConfiguration;
    use crate::sandbox::uninitialized::GuestBinary;
    use crate::{Result, UninitializedSandbox};
    use hyperlight_testing::simple_guest_as_string;
    use std::time::Duration;

    #[test]
    fn test_cpu_time_observer_timeout() {
        const TIMEOUT_MS: u64 = 100;

        let guest_binary = simple_guest_as_string().unwrap();
        let config = SandboxConfiguration::default();
        let uninitialized =
            UninitializedSandbox::new(GuestBinary::FilePath(guest_binary), Some(config)).unwrap();
        let mut sandbox = uninitialized.evolve().unwrap();
        let interrupt_handle = sandbox.interrupt_handle();

        // Create observer with a short timeout
        let observer =
            CpuTimeObserver::new(Duration::from_millis(TIMEOUT_MS), Duration::from_millis(1));

        // Get current thread ID for CPU time measurement
        let thread_id = unsafe { libc::pthread_self() };

        // Measure CPU time before the call
        let cpu_time_before = observer.start_timeout(&interrupt_handle);

        // Call the Spin function which should never return, using the observer
        let result: Result<()> = sandbox.call("Spin", ());
        observer.stop_timeout(&interrupt_handle);

        // Measure CPU time after the call
        let cpu_time_after = get_thread_cpu_time(thread_id).unwrap();
        let elapsed_cpu_time = cpu_time_after - cpu_time_before;

        println!(
            "CPU time elapsed: {:?} (expected ~{}ms)",
            elapsed_cpu_time, TIMEOUT_MS
        );

        // Check that we get the expected error type (execution canceled by host)
        let error = result.unwrap_err();
        assert!(matches!(
            error,
            crate::HyperlightError::ExecutionCanceledByHost()
        ));
    }

    #[test]
    fn test_cpu_time_observer_normal_completion() {
        let guest_binary = simple_guest_as_string().unwrap();
        let config = SandboxConfiguration::default();
        let uninitialized =
            UninitializedSandbox::new(GuestBinary::FilePath(guest_binary), Some(config)).unwrap();
        let mut sandbox = uninitialized.evolve().unwrap();
        let interrupt_handle = sandbox.interrupt_handle();

        let observer = CpuTimeObserver::new(Duration::from_secs(1), Duration::from_millis(1));

        // Start monitoring
        observer.start_timeout(&interrupt_handle);

        // Call a function that should complete quickly
        sandbox.call::<String>("Echo", "hello".to_string()).unwrap();

        // Stop monitoring
        observer.stop_timeout(&interrupt_handle);

        // next call should NOT be interrupted by the observer. That would be a bug. But we need someway to cancel
        // it still, so we spawn a new manual observer thread that will interrupt after 3 second, and make sure cancellation is done
        // after 3 seconds instead of the original observer's 1 second
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            interrupt_handle.kill();
        });
        let now = std::time::Instant::now();
        let res = sandbox.call::<()>("Spin", ()).unwrap_err();
        assert!(matches!(
            res,
            crate::HyperlightError::ExecutionCanceledByHost()
        ));
        let elapsed = now.elapsed();
        assert!(elapsed >= Duration::from_secs(3));
    }

    #[test]
    fn test_cpu_time_observer_parallel_sandboxes() {
        // number of parallel sandboxes to run
        const NUM_SANDBOXES: usize = 8;
        // time allowed for a sandbox before it should be interrupted
        const TIMEOUT_MS: u64 = 20;
        // how often to check for timeouts in the observer thread
        const CHECK_INTERVAL_MS: u64 = 1;
        // delay between interrupt attempts in the sandbox
        const INTERRUPT_RETRY_DELAY_MICROSECONDS: u64 = 500;

        let guest_binary = simple_guest_as_string().unwrap();
        let mut config = SandboxConfiguration::default();
        config.set_interrupt_retry_delay(Duration::from_micros(INTERRUPT_RETRY_DELAY_MICROSECONDS));

        let observer = Arc::new(CpuTimeObserver::new(
            Duration::from_millis(TIMEOUT_MS),
            Duration::from_millis(CHECK_INTERVAL_MS),
        ));
        let barrier = Arc::new(std::sync::Barrier::new(NUM_SANDBOXES));

        let handles: Vec<_> = (0..NUM_SANDBOXES)
            .map(|_| {
                let guest_binary = guest_binary.clone();
                let config = config.clone();
                let observer = observer.clone();
                let barrier = barrier.clone();

                std::thread::spawn(move || {
                    let uninitialized = UninitializedSandbox::new(
                        GuestBinary::FilePath(guest_binary),
                        Some(config),
                    )
                    .unwrap();
                    let mut sandbox = uninitialized.evolve().unwrap();
                    let interrupt_handle = sandbox.interrupt_handle();

                    // Get current thread ID for CPU time measurement
                    let thread_id = unsafe { libc::pthread_self() };

                    barrier.wait();

                    // Measure CPU time before the call
                    let cpu_time_before = observer.start_timeout(&interrupt_handle);

                    let result = sandbox.call::<()>("Spin", ()).unwrap_err();

                    // Measure CPU time after the call
                    let cpu_time_after = get_thread_cpu_time(thread_id).unwrap();
                    let elapsed_cpu_time = cpu_time_after - cpu_time_before;

                    (result, elapsed_cpu_time)
                })
            })
            .collect();

        // Collect results from all threads
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(results.len(), NUM_SANDBOXES);

        // Print timing results after all threads have completed
        for (i, (_, elapsed_time)) in results.iter().enumerate() {
            println!(
                "Thread {}: CPU time elapsed: {:?} (expected ~{}ms)",
                i, elapsed_time, TIMEOUT_MS
            );
        }

        // All should have been interrupted with ExecutionCanceledByHost
        for (result, _) in &results {
            assert!(
                matches!(result, crate::HyperlightError::ExecutionCanceledByHost()),
                "Expected ExecutionCanceledByHost error, got: {:?}",
                result
            );
        }
    }

    #[test]
    fn test_cpu_time_observer_high_precision() {
        let guest_binary = simple_guest_as_string().unwrap();
        let config = SandboxConfiguration::default();
        let uninitialized =
            UninitializedSandbox::new(GuestBinary::FilePath(guest_binary), Some(config)).unwrap();
        let mut sandbox = uninitialized.evolve().unwrap();
        let interrupt_handle = sandbox.interrupt_handle();

        // Create observer with high precision
        let observer = CpuTimeObserver::new(Duration::from_millis(50), Duration::from_micros(100));

        let thread_id = unsafe { libc::pthread_self() };
        let cpu_time_before = observer.start_timeout(&interrupt_handle);

        let result: Result<()> = sandbox.call("Spin", ());

        let cpu_time_after = get_thread_cpu_time(thread_id).unwrap();
        let elapsed_cpu_time = cpu_time_after - cpu_time_before;

        println!(
            "High precision - CPU time elapsed: {:?} (expected ~50ms)",
            elapsed_cpu_time
        );

        let error = result.unwrap_err();
        assert!(matches!(
            error,
            crate::HyperlightError::ExecutionCanceledByHost()
        ));
    }
}
