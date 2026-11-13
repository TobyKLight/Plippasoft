use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Manages a collection of threads.
///
/// A new thread is created every time all the existing threads are full.
/// Any idle thread will automatically die after a few seconds.
pub struct TaskPool {
    sharing: Arc<Sharing>,
    /// PLIPPA PATCH: ceiling on worker threads (`spawn` refuses beyond it, it never queues).
    max_threads: usize,
}

struct Sharing {
    // list of the tasks to be done by worker threads
    todo: Mutex<VecDeque<Box<dyn FnMut() + Send>>>,

    // condvar that will be notified whenever a task is added to `todo`
    condvar: Condvar,

    // number of total worker threads running
    active_tasks: AtomicUsize,

    // number of idle worker threads
    waiting_tasks: AtomicUsize,
}

/// Minimum number of active threads.
static MIN_THREADS: usize = 4;

struct Registration<'a> {
    nb: &'a AtomicUsize,
}

impl<'a> Registration<'a> {
    fn new(nb: &'a AtomicUsize) -> Registration<'a> {
        nb.fetch_add(1, Ordering::Release);
        Registration { nb }
    }
}

impl<'a> Drop for Registration<'a> {
    fn drop(&mut self) {
        self.nb.fetch_sub(1, Ordering::Release);
    }
}

impl TaskPool {
    pub fn new(max_threads: usize) -> TaskPool {
        let pool = TaskPool {
            max_threads: max_threads.max(MIN_THREADS),
            sharing: Arc::new(Sharing {
                todo: Mutex::new(VecDeque::new()),
                condvar: Condvar::new(),
                active_tasks: AtomicUsize::new(0),
                waiting_tasks: AtomicUsize::new(0),
            }),
        };

        for _ in 0..MIN_THREADS {
            pool.add_thread(None)
        }

        pool
    }

    /// Executes a function in a thread.
    /// If no thread is available, spawns a new one. PLIPPA PATCH: returns the task back when
    /// every one of the `max_threads` workers is busy, so the caller can refuse the connection
    /// instead of growing without bound.
    pub fn spawn(
        &self,
        code: Box<dyn FnMut() + Send>,
    ) -> Result<(), Box<dyn FnMut() + Send>> {
        let mut queue = self.sharing.todo.lock().unwrap();

        // PLIPPA PATCH (NB_VERIFY NB_10): upstream compares the idle-worker count with 0, but a
        // worker that has been notified only leaves that count once it gets the lock back, so
        // several connections accepted in a burst all saw "one worker is idle" and were queued
        // for the same idle worker. That worker then serves its keep-alive connection for as
        // long as the client keeps it open, and the queued connections were never served (a
        // browser loading the portal hung on 1-2 of its ~6 parallel requests). Spawn a thread
        // unless there is a spare idle worker for every task already queued plus this one.
        if self.sharing.waiting_tasks.load(Ordering::Acquire) <= queue.len() {
            if self.sharing.active_tasks.load(Ordering::Acquire) >= self.max_threads {
                return Err(code);
            }
            self.add_thread(Some(code));
        } else {
            queue.push_back(code);
            self.sharing.condvar.notify_one();
        }
        Ok(())
    }

    fn add_thread(&self, initial_fn: Option<Box<dyn FnMut() + Send>>) {
        let sharing = self.sharing.clone();

        thread::spawn(move || {
            let sharing = sharing;
            let _active_guard = Registration::new(&sharing.active_tasks);

            if let Some(mut f) = initial_fn {
                f();
            }

            loop {
                let mut task: Box<dyn FnMut() + Send> = {
                    let mut todo = sharing.todo.lock().unwrap();

                    let task;
                    loop {
                        if let Some(poped_task) = todo.pop_front() {
                            task = poped_task;
                            break;
                        }
                        let _waiting_guard = Registration::new(&sharing.waiting_tasks);

                        let received =
                            if sharing.active_tasks.load(Ordering::Acquire) <= MIN_THREADS {
                                todo = sharing.condvar.wait(todo).unwrap();
                                true
                            } else {
                                let (new_lock, waitres) = sharing
                                    .condvar
                                    .wait_timeout(todo, Duration::from_millis(5000))
                                    .unwrap();
                                todo = new_lock;
                                !waitres.timed_out()
                            };

                        if !received && todo.is_empty() {
                            return;
                        }
                    }

                    task
                };

                task();
            }
        });
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        self.sharing
            .active_tasks
            .store(999_999_999, Ordering::Release);
        self.sharing.condvar.notify_all();
    }
}
