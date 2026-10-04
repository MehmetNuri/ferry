//! The transfer queue. Every file is one job; jobs started together form a
//! batch with a single completion callback. The queue lives on the GTK main
//! thread, the work of each job runs on the Tokio runtime.
use crate::i18n::trf;
use gtk::glib;
use gtk::glib::prelude::*;
use gtk::glib::subclass::prelude::*;
use gtk::gio;
use gtk::gio::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::runtime::bg;
use crate::s3::{CANCELLED, Progress, Res};

pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";
pub const CANCELED: &str = "cancelled";
/// Stopped by the user and kept in its place; resuming continues where it stopped.
pub const PAUSED: &str = "paused";

/// Finished jobs kept in the list; older ones are dropped.
const HISTORY: usize = 2000;

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::TransferItem)]
    pub struct TransferItem {
        #[property(get, set)]
        id: Cell<u64>,
        /// "upload", "download", "copy", "move", "sync", "backup".
        #[property(get, set)]
        kind: RefCell<String>,
        #[property(get, set)]
        name: RefCell<String>,
        /// Where the file goes, shown under the name.
        #[property(get, set)]
        detail: RefCell<String>,
        #[property(get, set)]
        state: RefCell<String>,
        #[property(get, set)]
        done: Cell<u64>,
        #[property(get, set)]
        total: Cell<u64>,
        /// Bytes per second while running.
        #[property(get, set)]
        speed: Cell<u64>,
        #[property(get, set)]
        error: RefCell<String>,
        /// Queue position; lower runs first.
        #[property(get, set)]
        seq: Cell<f64>,
        /// Monotonic time the job finished, for ordering the history.
        #[property(get, set)]
        finished: Cell<i64>,
        /// A local file or folder the job wrote, for "Open Containing Folder".
        #[property(get, set)]
        local: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TransferItem {
        const NAME: &'static str = "FerryTransferItem";
        type Type = super::TransferItem;
    }

    #[glib::derived_properties]
    impl ObjectImpl for TransferItem {}
}

glib::wrapper! {
    pub struct TransferItem(ObjectSubclass<imp::TransferItem>);
}

impl TransferItem {
    pub fn is_active(&self) -> bool {
        matches!(self.state().as_str(), QUEUED | RUNNING | PAUSED)
    }
    pub fn is_failed(&self) -> bool {
        matches!(self.state().as_str(), FAILED | CANCELED)
    }
}

pub type Work = Rc<dyn Fn(Progress) -> Pin<Box<dyn Future<Output = Res<()>> + Send>>>;

/// Creates the work of a job from a closure returning a future.
pub fn work<F, Fut>(f: F) -> Work
where
    F: Fn(Progress) -> Fut + 'static,
    Fut: Future<Output = Res<()>> + Send + 'static,
{
    Rc::new(move |progress| Box::pin(f(progress)))
}

struct Job {
    item: TransferItem,
    work: Work,
    progress: Progress,
    batch: Option<u64>,
    /// Quiet jobs never trigger notifications (mounts, external editors).
    quiet: bool,
    sample: (u64, Instant),
    /// How to recreate the job after a restart; jobs without one are not kept.
    spec: Option<serde_json::Value>,
    /// Set when a running job is stopped to be paused rather than cancelled.
    pausing: bool,
    /// Automatic retries after connection problems, and when the next one may start.
    attempts: u32,
    not_before: Option<Instant>,
}

/// The outcome of a batch: finished, failed and cancelled jobs, and the last error.
#[derive(Clone, Default, Debug)]
pub struct Outcome {
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub error: String,
}

struct Batch {
    pending: usize,
    outcome: Outcome,
    on_done: Option<Box<dyn FnOnce(Outcome)>>,
}

/// Counts and rates for the summary line, the progress pie and the background status.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Summary {
    pub running: usize,
    pub queued: usize,
    pub failed: usize,
    pub done: usize,
    pub speed: u64,
    /// Bytes of running and queued jobs, done and total.
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub paused: bool,
    /// Jobs paused one by one.
    pub held: usize,
}

impl Summary {
    pub fn active(&self) -> usize {
        self.running + self.queued
    }
    pub fn fraction(&self) -> f64 {
        if self.bytes_total > 0 { (self.bytes_done as f64 / self.bytes_total as f64).min(1.0) } else { 0.0 }
    }
    pub fn seconds_left(&self) -> Option<u64> {
        (self.speed > 0 && self.bytes_total > self.bytes_done).then(|| (self.bytes_total - self.bytes_done) / self.speed)
    }
}

#[derive(Default)]
struct Inner {
    store: Option<gio::ListStore>,
    jobs: RefCell<HashMap<u64, Job>>,
    batches: RefCell<HashMap<u64, Batch>>,
    next_id: Cell<u64>,
    next_seq: Cell<f64>,
    running: Cell<usize>,
    paused: Cell<bool>,
    limit: Cell<usize>,
    listeners: RefCell<Vec<Rc<dyn Fn(&Summary)>>>,
    finished_listeners: RefCell<Vec<Rc<dyn Fn(&TransferItem, bool)>>>,
    last_summary: RefCell<Summary>,
    save_pending: Cell<bool>,
    ticking: Cell<bool>,
    /// When the current run of work began (monotonic microseconds).
    session: Cell<i64>,
}

#[derive(Clone)]
pub struct Queue(Rc<Inner>);

impl Queue {
    pub fn new(limit: usize) -> Self {
        let inner = Inner { store: Some(gio::ListStore::new::<TransferItem>()), ..Default::default() };
        inner.limit.set(limit.max(1));
        inner.next_seq.set(1.0);
        Queue(Rc::new(inner))
    }

    /// Speeds and byte counts are sampled a few times a second while something runs;
    /// an idle queue has no timer, so the application sleeps.
    fn ensure_ticking(&self) {
        if self.0.ticking.replace(true) { return; }
        let weak = Rc::downgrade(&self.0);
        glib::timeout_add_local(std::time::Duration::from_millis(400), move || {
            let Some(inner) = weak.upgrade() else { return glib::ControlFlow::Break };
            let queue = Queue(inner);
            queue.tick();
            if queue.0.running.get() == 0 {
                queue.0.ticking.set(false);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    pub fn store(&self) -> gio::ListStore {
        self.0.store.clone().unwrap()
    }

    /// Called whenever the summary changes.
    pub fn connect_changed(&self, f: impl Fn(&Summary) + 'static) {
        self.0.listeners.borrow_mut().push(Rc::new(f));
    }

    /// Called when a job ends; the flag tells whether it was quiet.
    pub fn connect_finished(&self, f: impl Fn(&TransferItem, bool) + 'static) {
        self.0.finished_listeners.borrow_mut().push(Rc::new(f));
    }

    /// Starts a batch; `on_done` runs once every job of it has ended.
    pub fn batch(&self, on_done: impl FnOnce(Outcome) + 'static) -> u64 {
        let id = self.next_id();
        self.0.batches.borrow_mut().insert(id, Batch { pending: 0, outcome: Outcome::default(), on_done: Some(Box::new(on_done)) });
        id
    }

    /// Closes a batch that received no jobs, or lets it finish once its jobs end.
    pub fn seal(&self, batch: u64) {
        self.finish_batch_if_done(batch);
    }

    fn next_id(&self) -> u64 {
        let id = self.0.next_id.get() + 1;
        self.0.next_id.set(id);
        id
    }

    pub fn add(&self, batch: Option<u64>, kind: &str, name: &str, detail: &str, total: u64, local: Option<&str>, work: Work) -> TransferItem {
        self.add_job(batch, kind, name, detail, total, local, false, work)
    }

    pub fn add_quiet(&self, kind: &str, name: &str, detail: &str, total: u64, work: Work) -> TransferItem {
        self.add_job(None, kind, name, detail, total, None, true, work)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_job(&self, batch: Option<u64>, kind: &str, name: &str, detail: &str, total: u64, local: Option<&str>, quiet: bool, work: Work) -> TransferItem {
        self.begin_session_if_idle();
        let id = self.next_id();
        let seq = self.0.next_seq.get();
        self.0.next_seq.set(seq + 1.0);
        let item: TransferItem = glib::Object::builder()
            .property("id", id).property("kind", kind).property("name", name).property("detail", detail)
            .property("state", QUEUED).property("total", total).property("seq", seq).property("local", local.unwrap_or_default())
            .build();
        if let Some(b) = batch && let Some(batch) = self.0.batches.borrow_mut().get_mut(&b) {
            batch.pending += 1;
        }
        self.0.jobs.borrow_mut().insert(id, Job { item: item.clone(), work, progress: Progress::default(), batch, quiet, sample: (0, Instant::now()), spec: None, pausing: false, attempts: 0, not_before: None });
        self.store().append(&item);
        self.trim_history();
        self.pump();
        self.notify();
        item
    }

    fn trim_history(&self) {
        let store = self.store();
        if store.n_items() as usize <= HISTORY + 200 {
            return;
        }
        let mut finished: Vec<TransferItem> = (0..store.n_items()).filter_map(|i| store.item(i).and_downcast::<TransferItem>()).filter(|t| !t.is_active()).collect();
        finished.sort_by_key(|t| t.finished());
        let drop = finished.len().saturating_sub(HISTORY);
        for item in finished.into_iter().take(drop) {
            self.forget(&item);
        }
    }

    fn forget(&self, item: &TransferItem) {
        self.0.jobs.borrow_mut().remove(&item.id());
        let store = self.store();
        if let Some(position) = store.find(item) {
            store.remove(position);
        }
    }

    /// Starts queued jobs, lowest position first, while slots are free.
    fn pump(&self) {
        while !self.0.paused.get() && self.0.running.get() < self.0.limit.get() {
            let now = Instant::now();
            let next = self.0.jobs.borrow().values()
                .filter(|j| j.item.state() == QUEUED && j.not_before.is_none_or(|t| t <= now))
                .min_by(|a, b| a.item.seq().total_cmp(&b.item.seq()))
                .map(|j| j.item.id());
            let Some(id) = next else { break };
            self.start(id);
        }
    }

    fn start(&self, id: u64) {
        let (future, item) = {
            let mut jobs = self.0.jobs.borrow_mut();
            let Some(job) = jobs.get_mut(&id) else { return };
            job.progress = Progress::default();
            job.pausing = false;
            job.not_before = None;
            job.sample = (0, Instant::now());
            job.item.set_state(RUNNING);
            job.item.set_done(0);
            job.item.set_speed(0);
            job.item.set_error("");
            ((job.work)(job.progress.clone()), job.item.clone())
        };
        self.0.running.set(self.0.running.get() + 1);
        self.ensure_ticking();
        let queue = self.clone();
        glib::spawn_future_local(async move {
            let result = bg(future).await;
            queue.end(&item, result);
        });
    }

    fn end(&self, item: &TransferItem, result: Res<()>) {
        self.0.running.set(self.0.running.get().saturating_sub(1));
        let (batch, quiet, done, pausing) = {
            let jobs = self.0.jobs.borrow();
            match jobs.get(&item.id()) {
                Some(job) => (job.batch, job.quiet, job.progress.done.load(Ordering::Relaxed), job.pausing),
                None => (None, true, item.done(), false),
            }
        };
        // A job removed while running ends silently.
        if !self.0.jobs.borrow().contains_key(&item.id()) {
            self.pump();
            self.notify();
            return;
        }
        // A paused job waits in its place; its batch is not finished yet.
        if pausing && result.as_ref().err().is_some_and(|e| e == CANCELLED) {
            item.set_done(done);
            item.set_speed(0);
            item.set_state(PAUSED);
            self.pump();
            self.notify();
            return;
        }
        // A connection problem is tried again by itself a few times, waiting longer each
        // time; uploads and downloads continue where they stopped.
        if let Err(error) = &result && error != CANCELLED && transient(error) {
            let delay = {
                let mut jobs = self.0.jobs.borrow_mut();
                jobs.get_mut(&item.id()).filter(|j| j.attempts < RETRIES).map(|job| {
                    job.attempts += 1;
                    let delay = if cfg!(test) { std::time::Duration::from_millis(30) } else { std::time::Duration::from_secs(5 * 3u64.pow(job.attempts - 1)) };
                    job.not_before = Some(Instant::now() + delay);
                    delay
                })
            };
            if let Some(delay) = delay {
                item.set_speed(0);
                item.set_done(done);
                item.set_error(trf("Connection problem; trying again in {n} s", &[("n", &delay.as_secs().to_string())]));
                item.set_state(QUEUED);
                let weak = Rc::downgrade(&self.0);
                glib::timeout_add_local_once(delay, move || if let Some(inner) = weak.upgrade() { Queue(inner).pump(); });
                self.pump();
                self.notify();
                return;
            }
        }
        let state = match &result {
            Ok(()) => DONE,
            Err(e) if e == CANCELLED => CANCELED,
            Err(_) => FAILED,
        };
        item.set_done(if state == DONE { item.total().max(done) } else { done });
        if state == DONE && item.total() == 0 {
            item.set_total(done);
        }
        item.set_speed(0);
        item.set_error(result.as_ref().err().filter(|e| *e != CANCELLED).cloned().unwrap_or_default());
        item.set_finished(glib::monotonic_time());
        item.set_state(state);
        if let Some(b) = batch {
            self.account(b, state, item.error());
        }
        for listener in self.0.finished_listeners.borrow().clone() {
            listener(item, quiet);
        }
        self.pump();
        self.notify();
    }

    fn account(&self, batch: u64, state: &str, error: String) {
        if let Some(b) = self.0.batches.borrow_mut().get_mut(&batch) {
            b.pending = b.pending.saturating_sub(1);
            match state {
                DONE => b.outcome.done += 1,
                CANCELED => b.outcome.cancelled += 1,
                _ => { b.outcome.failed += 1; b.outcome.error = error; }
            }
        }
        self.finish_batch_if_done(batch);
    }

    fn finish_batch_if_done(&self, batch: u64) {
        let finished = {
            let mut batches = self.0.batches.borrow_mut();
            match batches.get(&batch) {
                Some(b) if b.pending == 0 => batches.remove(&batch),
                _ => None,
            }
        };
        if let Some(mut b) = finished && let Some(callback) = b.on_done.take() {
            callback(b.outcome);
        }
    }

    fn tick(&self) {
        let now = Instant::now();
        for job in self.0.jobs.borrow_mut().values_mut() {
            if job.item.state() != RUNNING {
                continue;
            }
            let done = job.progress.done.load(Ordering::Relaxed);
            let elapsed = now.duration_since(job.sample.1).as_secs_f64();
            if elapsed >= 0.35 {
                let rate = (done.saturating_sub(job.sample.0)) as f64 / elapsed;
                // Smoothed so the figure does not jump with every chunk.
                let smoothed = if job.item.speed() == 0 { rate } else { job.item.speed() as f64 * 0.6 + rate * 0.4 };
                job.item.set_speed(smoothed as u64);
                job.sample = (done, now);
            }
            if done != job.item.done() {
                job.item.set_done(done);
            }
        }
        self.notify();
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary { paused: self.0.paused.get(), ..Default::default() };
        for job in self.0.jobs.borrow().values() {
            let item = &job.item;
            match item.state().as_str() {
                RUNNING => { summary.running += 1; summary.speed += item.speed(); summary.bytes_done += item.done(); summary.bytes_total += item.total().max(item.done()); }
                QUEUED => { summary.queued += 1; summary.bytes_total += item.total(); }
                PAUSED => summary.held += 1,
                DONE => {
                    summary.done += 1;
                    // Work finished since the queue last started counts, so progress reaches 100 %.
                    if item.finished() >= self.0.session.get() {
                        let size = item.total().max(item.done());
                        summary.bytes_done += size;
                        summary.bytes_total += size;
                    }
                }
                _ => summary.failed += 1,
            }
        }
        summary
    }

    /// Progress is measured from the moment an idle queue gets work again.
    fn begin_session_if_idle(&self) {
        let busy = self.0.jobs.borrow().values().any(|j| matches!(j.item.state().as_str(), QUEUED | RUNNING));
        if !busy { self.0.session.set(glib::monotonic_time()); }
    }

    pub fn set_spec(&self, item: &TransferItem, spec: serde_json::Value) {
        if let Some(job) = self.0.jobs.borrow_mut().get_mut(&item.id()) {
            job.spec = Some(spec);
        }
        self.schedule_save();
    }

    pub fn spec_of(&self, item: &TransferItem) -> Option<serde_json::Value> {
        self.0.jobs.borrow().get(&item.id()).and_then(|j| j.spec.clone())
    }

    /// Writes unfinished jobs that can be recreated, at most once a second.
    fn schedule_save(&self) {
        if self.0.save_pending.replace(true) { return; }
        let weak = Rc::downgrade(&self.0);
        glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
            if let Some(inner) = weak.upgrade() { inner.save_pending.set(false); Queue(inner).save(); }
        });
    }

    pub fn save(&self) {
        let mut jobs: Vec<(f64, serde_json::Value)> = self.0.jobs.borrow().values()
            .filter(|j| matches!(j.item.state().as_str(), QUEUED | RUNNING | PAUSED | FAILED))
            .filter_map(|j| j.spec.clone().map(|mut s| {
                if let Some(map) = s.as_object_mut() { map.insert("paused".into(), serde_json::Value::Bool(j.item.state() == PAUSED)); }
                (j.item.seq(), s)
            }))
            .collect();
        jobs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let list: Vec<serde_json::Value> = jobs.into_iter().map(|(_, s)| s).collect();
        let path = crate::profile::config_dir().join("queue.json");
        if list.is_empty() {
            let _ = std::fs::remove_file(path);
        } else if let Ok(data) = serde_json::to_vec_pretty(&list) {
            let _ = crate::profile::write_private(&path, &data);
        }
    }

    /// Jobs saved by an earlier session.
    pub fn saved() -> Vec<serde_json::Value> {
        std::fs::read(crate::profile::config_dir().join("queue.json")).ok()
            .and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
    }

    fn notify(&self) {
        self.schedule_save();
        let summary = self.summary();
        if *self.0.last_summary.borrow() == summary {
            return;
        }
        self.0.last_summary.replace(summary.clone());
        for listener in self.0.listeners.borrow().clone() {
            listener(&summary);
        }
    }

    // ----- Controls -----

    pub fn paused(&self) -> bool {
        self.0.paused.get()
    }

    /// Paused: running jobs finish, no new ones start.
    pub fn set_paused(&self, paused: bool) {
        self.0.paused.set(paused);
        self.pump();
        self.notify();
    }

    pub fn set_limit(&self, limit: usize) {
        self.0.limit.set(limit.clamp(1, 16));
        self.pump();
    }

    pub fn set_bandwidth(&self, bytes_per_second: u64) {
        crate::s3::BANDWIDTH.store(bytes_per_second, Ordering::Relaxed);
    }

    pub fn cancel(&self, item: &TransferItem) {
        match item.state().as_str() {
            RUNNING => {
                if let Some(job) = self.0.jobs.borrow().get(&item.id()) {
                    job.progress.cancel.store(true, Ordering::Relaxed);
                }
            }
            QUEUED | PAUSED => {
                item.set_state(CANCELED);
                item.set_finished(glib::monotonic_time());
                let batch = self.0.jobs.borrow().get(&item.id()).and_then(|j| j.batch);
                if let Some(b) = batch { self.account(b, CANCELED, String::new()); }
                self.notify();
            }
            _ => {}
        }
    }

    /// Stops a job without giving it up: a queued one stays in its place, a running one
    /// stops at its next chunk. Resumed uploads and downloads continue where they stopped.
    pub fn pause(&self, item: &TransferItem) {
        match item.state().as_str() {
            QUEUED => {
                item.set_state(PAUSED);
                self.notify();
            }
            RUNNING => {
                if let Some(job) = self.0.jobs.borrow_mut().get_mut(&item.id()) {
                    job.pausing = true;
                    job.progress.cancel.store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }

    pub fn resume(&self, item: &TransferItem) {
        if item.state() != PAUSED { return; }
        self.begin_session_if_idle();
        item.set_state(QUEUED);
        self.pump();
        self.notify();
    }

    pub fn retry(&self, item: &TransferItem) {
        if !item.is_failed() {
            return;
        }
        self.begin_session_if_idle();
        // A retried job joins the end of the queue; its batch waits for it again if still open.
        let batch = self.0.jobs.borrow().get(&item.id()).and_then(|j| j.batch);
        if let Some(b) = batch && let Some(batch) = self.0.batches.borrow_mut().get_mut(&b) {
            batch.pending += 1;
            if item.state() == FAILED { batch.outcome.failed = batch.outcome.failed.saturating_sub(1); } else { batch.outcome.cancelled = batch.outcome.cancelled.saturating_sub(1); }
        }
        let seq = self.0.next_seq.get();
        self.0.next_seq.set(seq + 1.0);
        item.set_seq(seq);
        item.set_error("");
        item.set_done(0);
        if let Some(job) = self.0.jobs.borrow_mut().get_mut(&item.id()) {
            job.attempts = 0;
            job.not_before = None;
        }
        item.set_state(QUEUED);
        self.pump();
        self.notify();
    }

    pub fn remove(&self, item: &TransferItem) {
        if matches!(item.state().as_str(), RUNNING | QUEUED | PAUSED) {
            self.cancel(item);
        }
        discard_partial(item);
        self.forget(item);
        self.notify();
    }

    fn queued(&self) -> Vec<TransferItem> {
        let mut list: Vec<TransferItem> = self.0.jobs.borrow().values().map(|j| j.item.clone()).filter(|i| i.state() == QUEUED).collect();
        list.sort_by(|a, b| a.seq().total_cmp(&b.seq()));
        list
    }

    /// Moves a queued job to the front of the queue.
    pub fn move_to_front(&self, item: &TransferItem) {
        if item.state() != QUEUED { return; }
        let first = self.queued().first().map(|i| i.seq()).unwrap_or(1.0);
        item.set_seq(first - 1.0);
    }

    pub fn move_to_end(&self, item: &TransferItem) {
        if item.state() != QUEUED { return; }
        let seq = self.0.next_seq.get();
        self.0.next_seq.set(seq + 1.0);
        item.set_seq(seq);
    }

    /// Moves a queued job one place up (-1) or down (+1).
    pub fn shift(&self, item: &TransferItem, direction: i32) {
        let list = self.queued();
        let Some(index) = list.iter().position(|i| i == item) else { return };
        let target = index as i64 + direction as i64;
        if target < 0 || target as usize >= list.len() { return; }
        let other = &list[target as usize];
        let (a, b) = (item.seq(), other.seq());
        item.set_seq(b);
        other.set_seq(a);
    }

    /// Places a queued job right before another queued job (drag and drop).
    pub fn place_before(&self, item: &TransferItem, before: &TransferItem) {
        if item == before || item.state() != QUEUED || before.state() != QUEUED { return; }
        let others: Vec<TransferItem> = self.queued().into_iter().filter(|i| i != item).collect();
        let Some(index) = others.iter().position(|i| i == before) else { return };
        let previous = if index == 0 { before.seq() - 1.0 } else { others[index - 1].seq() };
        item.set_seq((previous + before.seq()) / 2.0);
    }

    pub fn retry_failed(&self) -> usize {
        let failed: Vec<TransferItem> = self.0.jobs.borrow().values().map(|j| j.item.clone()).filter(|i| i.is_failed()).collect();
        for item in &failed { self.retry(item); }
        failed.len()
    }

    pub fn cancel_queued(&self) -> usize {
        let queued = self.queued();
        for item in &queued { self.cancel(item); }
        queued.len()
    }

    pub fn cancel_all(&self) {
        self.cancel_queued();
        let running: Vec<TransferItem> = self.0.jobs.borrow().values().map(|j| j.item.clone()).filter(|i| i.state() == RUNNING).collect();
        for item in &running { self.cancel(item); }
    }

    /// Pauses every running and queued job; they keep their places and their progress.
    pub fn pause_all(&self) {
        let items: Vec<TransferItem> = self.0.jobs.borrow().values().map(|j| j.item.clone()).filter(|i| matches!(i.state().as_str(), QUEUED | RUNNING)).collect();
        for item in &items { self.pause(item); }
    }

    pub fn resume_all(&self) {
        self.begin_session_if_idle();
        let items: Vec<TransferItem> = self.0.jobs.borrow().values().map(|j| j.item.clone()).filter(|i| i.state() == PAUSED).collect();
        for item in &items { item.set_state(QUEUED); }
        self.pump();
        self.notify();
    }

    pub fn clear_finished(&self) {
        let finished: Vec<TransferItem> = self.0.jobs.borrow().values().map(|j| j.item.clone()).filter(|i| i.state() == DONE).collect();
        for item in &finished { self.forget(item); }
        self.notify();
    }

    pub fn clear_inactive(&self) {
        let finished: Vec<TransferItem> = self.0.jobs.borrow().values().map(|j| j.item.clone()).filter(|i| !i.is_active()).collect();
        for item in &finished {
            discard_partial(item);
            self.forget(item);
        }
        self.notify();
    }
}

const RETRIES: u32 = 3;

/// Errors worth trying again: the connection, timeouts, or a busy service.
fn transient(error: &str) -> bool {
    let error = error.to_lowercase();
    ["dispatch failure", "timeout", "timed out", "connection", "connect", "io error", "network", "broken pipe", "reset by peer",
     "slowdown", "slow down", "internalerror", "serviceunavailable", "service unavailable", "503", "502", "504", "dns"]
        .iter().any(|needle| error.contains(needle))
}

/// A download given up keeps no half-written file behind; a retry would have continued it.
fn discard_partial(item: &TransferItem) {
    if item.kind() != "download" || item.state() == DONE || item.local().is_empty() {
        return;
    }
    let local = item.local();
    // A running transfer stops at its next chunk; remove the file once it has.
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
        let _ = std::fs::remove_file(format!("{local}.part"));
        let _ = std::fs::remove_file(format!("{local}.part.etag"));
        let _ = std::fs::remove_file(format!("{local}.part.json"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    /// Runs the GLib main loop until the condition holds or the time runs out.
    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let context = glib::MainContext::default();
        let start = Instant::now();
        while !condition() {
            if start.elapsed().as_secs() > 10 { return false; }
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        true
    }

    #[test]
    fn queue_behaviour() {
        let queue = Queue::new(2);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let job = |name: &'static str, fail_once: Option<Arc<AtomicBool>>| {
            let (running, peak, order) = (running.clone(), peak.clone(), order.clone());
            work(move |progress| {
                let (running, peak, order, fail_once) = (running.clone(), peak.clone(), order.clone(), fail_once.clone());
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    order.lock().unwrap().push(name);
                    for _ in 0..10 {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        if progress.cancel.load(Ordering::Relaxed) { running.fetch_sub(1, Ordering::SeqCst); return Err(CANCELLED.to_string()); }
                        progress.done.fetch_add(10, Ordering::Relaxed);
                    }
                    running.fetch_sub(1, Ordering::SeqCst);
                    if let Some(flag) = fail_once && !flag.swap(true, Ordering::SeqCst) { return Err("boom".into()); }
                    Ok(())
                }
            })
        };
        let outcome = Rc::new(RefCell::new(None));
        let o = outcome.clone();
        let batch = queue.batch(move |result| { o.replace(Some(result)); });
        queue.set_paused(true);
        let items: Vec<TransferItem> = ["a", "b", "c", "d"].iter().map(|n| queue.add(Some(batch), "upload", n, "", 100, None, job(n, None))).collect();
        let flaky = Arc::new(AtomicBool::new(false));
        let e = queue.add(Some(batch), "upload", "e", "", 100, None, job("e", Some(flaky)));
        let cancelled = queue.add(Some(batch), "upload", "f", "", 100, None, job("f", None));
        queue.seal(batch);
        // Nothing runs while paused; "d" goes to the front, "f" is cancelled before it starts.
        assert_eq!(queue.summary().queued, 6);
        queue.move_to_front(&items[3]);
        queue.cancel(&cancelled);
        queue.set_paused(false);
        assert!(wait_for(|| outcome.borrow().is_some()), "batch did not finish");
        let result = outcome.borrow().clone().unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 2, "concurrency limit");
        assert!(order.lock().unwrap()[..2].contains(&"d"), "move to front");
        assert_eq!((result.done, result.failed, result.cancelled), (4, 1, 1));
        assert_eq!(e.state(), FAILED);
        assert_eq!(e.error(), "boom");
        assert_eq!(items[0].done(), 100);
        // Finished work keeps counting until the queue is idle, so progress ends at 100 %.
        assert!((queue.summary().fraction() - 1.0).abs() < 1e-9, "progress ended at {}", queue.summary().fraction());
        // A retried job runs again and succeeds the second time.
        queue.retry(&e);
        assert!(wait_for(|| e.state() == DONE), "retry did not finish");
        assert_eq!(queue.summary().failed, 1, "only the cancelled job is left as failed");
        queue.clear_inactive();
        assert_eq!(queue.store().n_items(), 0);
        // A paused job, running or queued, keeps its batch open until it is resumed.
        let outcome = Rc::new(RefCell::new(None));
        let o = outcome.clone();
        let batch = queue.batch(move |result| { o.replace(Some(result)); });
        let held = queue.add(Some(batch), "upload", "g", "", 100, None, job("g", None));
        let waiting = queue.add(Some(batch), "upload", "h", "", 100, None, job("h", None));
        queue.seal(batch);
        assert!(wait_for(|| held.state() == RUNNING));
        queue.pause(&held);
        assert!(wait_for(|| held.state() == PAUSED), "running job did not pause");
        assert!(wait_for(|| waiting.state() == DONE));
        assert!(outcome.borrow().is_none(), "batch finished with a paused job");
        assert_eq!(queue.summary().held, 1);
        queue.resume(&held);
        assert!(wait_for(|| outcome.borrow().is_some()), "resumed job did not finish");
        assert_eq!(outcome.borrow().as_ref().unwrap().done, 2);
        // A connection problem is retried by itself; the batch only sees the success.
        let outcome = Rc::new(RefCell::new(None));
        let o = outcome.clone();
        let batch = queue.batch(move |result| { o.replace(Some(result)); });
        let tries = Arc::new(AtomicUsize::new(0));
        let t = tries.clone();
        queue.add(Some(batch), "upload", "net", "", 10, None, work(move |_| {
            let t = t.clone();
            async move { if t.fetch_add(1, Ordering::SeqCst) < 2 { Err("dispatch failure: connection reset".to_string()) } else { Ok(()) } }
        }));
        queue.seal(batch);
        assert!(wait_for(|| outcome.borrow().is_some()), "retried job did not finish");
        let result = outcome.borrow().clone().unwrap();
        assert_eq!((result.done, result.failed, tries.load(Ordering::SeqCst)), (1, 0, 3));
    }
}
