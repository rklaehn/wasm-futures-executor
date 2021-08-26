use futures::{Future, FutureExt};
use futures_task::{waker_ref, ArcWake, Context, FutureObj, Poll, Spawn, SpawnError};
use log::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use wasm_bindgen::{prelude::*, JsCast};
use web_sys::{
    Blob, BlobPropertyBag, DedicatedWorkerGlobalScope, Url, Worker, WorkerOptions, WorkerType,
};

use crate::unpark_mutex::UnparkMutex;

trait AssertSendSync: Send + Sync {}
impl AssertSendSync for ThreadPool {}

/// A general-purpose thread pool for scheduling tasks that poll futures to
/// completion.
///
/// The thread pool multiplexes any number of tasks onto a fixed number of
/// worker threads.
///
/// This type is a clonable handle to the threadpool itself.
/// Cloning it will only create a new reference, not a new threadpool.
///
/// The API follows [`futures_executor::ThreadPool`].
///
/// [`futures_executor::ThreadPool`]: https://docs.rs/futures-executor/0.3.16/futures_executor/struct.ThreadPool.html
pub struct ThreadPool {
    state: Arc<PoolState>,
}

impl Clone for ThreadPool {
    fn clone(&self) -> Self {
        self.state.cnt.fetch_add(1, Ordering::Relaxed);
        Self {
            state: self.state.clone(),
        }
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        if self.state.cnt.fetch_sub(1, Ordering::Relaxed) == 1 {
            for _ in 0..self.state.size {
                self.state.send(Message::Close);
            }
        }
    }
}

impl Spawn for ThreadPool {
    fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
        self.spawn_obj_ok(future);
        Ok(())
    }
}

/// Generates a DOMString containing an URL representing the code each worker is bootstrapped from.
/// This string can be used to as an argument to the [`Worker`] constructor.
fn worker_script() -> String {
    let path = js_sys::eval(r#"
// Taken from https://github.com/chemicstry/wasm_thread/blob/3c712fe91c8bc31cbdc8eeba7d151c4505d358e2/src/script_path.js
//
// Extracts current script file path from artificially generated stack trace
function script_path() {
    try {
        throw new Error();
    } catch (e) {
        let parts = e.stack.match(/\((\S+):\d+:\d+\)/);
        return parts[1];
    }
}

script_path()"#).unwrap().as_string().unwrap();
    let code = format!(
        r#"
// The first message initializes the wasm module with the passed
// shared memory.
self.onmessage = event => {{
    let [module, memory] = event.data;

    // Can't use relative imports here, as this would resolve to `blob:.../<crate_name>.js` [1] and
    // obviously that fails. So we have to construct the absolute file url ourselves. Furthermore, we
    // also need to figure out the name of the base js file, which usually is the crate name.
    //
    // [1] https://bugs.chromium.org/p/chromium/issues/detail?id=1161710
    let initialised = import('{}').then(async ({{ default: init, worker_entry_point }}) => {{
      await init(module, memory).catch(err => {{
        setTimeout(() => {{
          throw err;
        }});
        throw err;
      }});
      return worker_entry_point;
    }});

  // The second message passes shared state to the workers. There
  // shouldn't be any additional messages after that.
  self.onmessage = async event => {{
    let worker_entry_point = await initialised;
    worker_entry_point(event.data);

    // Terminate web worker
    close();
  }};
}};"#,
        path
    );
    let array = js_sys::Array::new();
    array.push(&code.into());

    let mut opts = BlobPropertyBag::new();
    opts.type_("text/javascript; charset=utf8");

    let blob = Blob::new_with_str_sequence_and_options(&array, &opts).unwrap();
    Url::create_object_url_with_blob(&blob).unwrap()
}

impl ThreadPool {
    /// Creates a new [`ThreadPool`] with the provided count of web workers.
    pub fn new(size: usize) -> Result<ThreadPool, JsValue> {
        let (tx, rx) = mpsc::channel();
        let pool = ThreadPool {
            state: Arc::new(PoolState {
                tx: Mutex::new(tx),
                rx: Mutex::new(rx),
                cnt: AtomicUsize::new(1),
                size,
            }),
        };
        let worker_script = worker_script();

        for idx in 0..size {
            let state = pool.state.clone();

            let mut opts = WorkerOptions::new();
            opts.type_(WorkerType::Module);
            opts.name(&*format!("Worker-{}", idx));
            let worker = Worker::new_with_options(&*worker_script, &opts)?;

            // With a worker spun up send it the module/memory so it can start
            // instantiating the wasm module. Later it might receive further
            // messages about code to run on the wasm module.
            let array = js_sys::Array::new();
            array.push(&wasm_bindgen::module());
            array.push(&wasm_bindgen::memory());
            worker.post_message(&array)?;
            let ptr = Arc::into_raw(state);
            worker.post_message(&JsValue::from(ptr as u32))?;
        }
        Ok(pool)
    }

    /// Creates a new [`ThreadPool`] with `Navigator.hardwareConcurrency` web workers.
    pub fn max_threads() -> Result<Self, JsValue> {
        #[wasm_bindgen]
        extern "C" {
            #[wasm_bindgen(js_namespace = navigator, js_name = hardwareConcurrency)]
            static HARDWARE_CONCURRENCY: usize;
        }
        let pool_size = std::cmp::min(*HARDWARE_CONCURRENCY, 1);
        Self::new(pool_size)
    }
    /// Spawns a future that will be run to completion.
    ///
    /// > **Note**: This method is similar to `Spawn::spawn_obj`, except that
    /// >           it is guaranteed to always succeed.
    pub fn spawn_obj_ok(&self, future: FutureObj<'static, ()>) {
        let task = Task {
            future,
            wake_handle: Arc::new(WakeHandle {
                exec: self.clone(),
                mutex: UnparkMutex::new(),
            }),
            exec: self.clone(),
        };
        self.state.send(Message::Run(task));
    }

    /// Spawns a task that polls the given future with output `()` to
    /// completion.
    ///
    /// ```
    /// use futures::executor::ThreadPool;
    ///
    /// let pool = ThreadPool::new().unwrap();
    ///
    /// let future = async { /* ... */ };
    /// pool.spawn_ok(future);
    /// ```
    ///
    /// > **Note**: This method is similar to `SpawnExt::spawn`, except that
    /// >           it is guaranteed to always succeed.
    pub fn spawn_ok<Fut>(&self, future: Fut)
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.spawn_obj_ok(FutureObj::new(Box::new(future)))
    }
}

enum Message {
    Run(Task),
    Close,
}

pub struct PoolState {
    tx: Mutex<mpsc::Sender<Message>>,
    rx: Mutex<mpsc::Receiver<Message>>,
    cnt: AtomicUsize,
    size: usize,
}

impl PoolState {
    fn send(&self, msg: Message) {
        self.tx.lock().send(msg).unwrap();
    }

    fn work(&self) {
        loop {
            let msg = self.rx.lock().recv().unwrap();
            match msg {
                Message::Run(task) => task.run(),
                Message::Close => break,
            }
        }
    }
}

/// A task responsible for polling a future to completion.
struct Task {
    future: FutureObj<'static, ()>,
    exec: ThreadPool,
    wake_handle: Arc<WakeHandle>,
}

impl Task {
    /// Actually run the task (invoking `poll` on the future) on the current
    /// thread.
    fn run(self) {
        let Self {
            mut future,
            wake_handle,
            mut exec,
        } = self;
        let waker = waker_ref(&wake_handle);
        let mut cx = Context::from_waker(&waker);

        // Safety: The ownership of this `Task` object is evidence that
        // we are in the `POLLING`/`REPOLL` state for the mutex.
        unsafe {
            wake_handle.mutex.start_poll();

            loop {
                let res = future.poll_unpin(&mut cx);
                match res {
                    Poll::Pending => {}
                    Poll::Ready(()) => return wake_handle.mutex.complete(),
                }
                let task = Self {
                    future,
                    wake_handle: wake_handle.clone(),
                    exec,
                };
                match wake_handle.mutex.wait(task) {
                    Ok(()) => return, // we've waited
                    Err(task) => {
                        // someone's notified us
                        future = task.future;
                        exec = task.exec;
                    }
                }
            }
        }
    }
}

impl ArcWake for WakeHandle {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        match arc_self.mutex.notify() {
            Ok(task) => arc_self.exec.state.send(Message::Run(task)),
            Err(()) => {}
        }
    }
}

struct WakeHandle {
    mutex: UnparkMutex<Task>,
    exec: ThreadPool,
}

/// Entry point invoked by the web worker. The passed pointer will be unconditionally interpreted
/// as an `Arc<PoolState`.
#[wasm_bindgen]
pub fn worker_entry_point(state_ptr: u32) {
    let state = unsafe { Arc::<PoolState>::from_raw(state_ptr as *const PoolState) };

    let global = js_sys::global().unchecked_into::<DedicatedWorkerGlobalScope>();
    debug!("{} spawned", global.name());
    state.work();
    debug!("{} yield", global.name());
}
