use super::*;
use std::sync::Arc;
use std::time::Duration;

/// Messages of failed jobs, all tests (each looks for its own tag).
static FAILS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A job that notes what it does in a shared queue, as the card's queue
/// would see it.
struct Fake {
    fence: u64,
    queue: Arc<Mutex<Vec<String>>>,
    sleep_ms: u64,
    outcome: Outcome,
    signalled: bool,
}

#[derive(Clone, Copy)]
enum Outcome {
    Sent,
    Error(&'static str),
    PanicBeforeSignal,
    PanicAfterSignal,
}

impl Fake {
    fn new(fence: u64, queue: &Arc<Mutex<Vec<String>>>) -> Fake {
        Fake {
            fence,
            queue: queue.clone(),
            sleep_ms: 0,
            outcome: Outcome::Sent,
            signalled: false,
        }
    }

    fn note(&self, what: String) {
        self.queue.lock().unwrap().push(what);
    }
}

impl Work for Fake {
    fn fence(&self) -> u64 {
        self.fence
    }

    fn run(&mut self) -> Result<(), String> {
        assert!(SENDING.with(Cell::get), "a job ran off the sending thread");
        std::thread::sleep(Duration::from_millis(self.sleep_ms));
        if let Outcome::PanicBeforeSignal = self.outcome {
            panic!("before the signal");
        }
        self.note(format!("lists {}", self.fence));
        self.note(format!("signal {}", self.fence));
        self.signalled = true;
        match self.outcome {
            Outcome::PanicAfterSignal => panic!("after the signal"),
            Outcome::Error(tag) => Err(format!("a submission sent ahead could not be followed by a signal ({tag})")),
            _ => Ok(()),
        }
    }

    fn rescue(&mut self) {
        if !self.signalled {
            self.note(format!("rescue signal {}", self.fence));
        }
    }

    fn failed(what: &str) {
        FAILS.lock().unwrap().push(what.to_string());
    }
}

fn sender() -> &'static Sender<Fake> {
    Box::leak(Box::new(Sender::new("victory-send-test")))
}

fn queue() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

#[test]
fn jobs_go_in_order_and_other_work_comes_after_them() {
    let s = sender();
    let q = queue();
    for fence in 1..=6 {
        let mut job = Fake::new(fence, &q);
        // Slow jobs: without the wait the caller's own work overtakes them.
        job.sleep_ms = 5;
        assert!(s.hand_over(job).is_ok());
    }
    s.wait_idle();
    q.lock().unwrap().push("other".to_string());
    let seen = q.lock().unwrap().clone();
    let mut want: Vec<String> = (1..=6)
        .flat_map(|f| [format!("lists {f}"), format!("signal {f}")])
        .collect();
    want.push("other".to_string());
    assert_eq!(seen, want);
    let st = s.take_stats();
    assert_eq!(st[JOBS], 6);
    assert_eq!(st[DISORDER], 0);
    assert!(st[WAITS] >= 1, "the wait found nothing outstanding");
}

#[test]
fn the_wait_returns_at_once_with_nothing_handed_over() {
    let s = sender();
    let began = Instant::now();
    s.wait_idle();
    assert!(began.elapsed() < Duration::from_secs(1));
    assert_eq!(s.take_stats()[WAITS], 0);
}

#[test]
fn a_panic_before_the_signal_is_signalled_booked_and_said() {
    let s = sender();
    let q = queue();
    let mut bad = Fake::new(1, &q);
    bad.outcome = Outcome::PanicBeforeSignal;
    s.hand_over(bad).ok().unwrap();
    s.hand_over(Fake::new(2, &q)).ok().unwrap();
    // Returns: the panicked job was booked as done, and the thread lives on.
    s.wait_idle();
    assert_eq!(
        *q.lock().unwrap(),
        ["rescue signal 1", "lists 2", "signal 2"]
    );
    assert!(FAILS.lock().unwrap().iter().any(|f| f.contains("panicked")));
    assert_eq!(s.take_stats()[FAILED], 1);
}

#[test]
fn a_panic_after_the_signal_does_not_signal_twice() {
    let s = sender();
    let q = queue();
    let mut bad = Fake::new(3, &q);
    bad.outcome = Outcome::PanicAfterSignal;
    s.hand_over(bad).ok().unwrap();
    s.wait_idle();
    assert_eq!(*q.lock().unwrap(), ["lists 3", "signal 3"]);
}

#[test]
fn a_failed_signal_ends_the_run_after_the_job_is_booked() {
    let s = sender();
    let q = queue();
    let mut bad = Fake::new(1, &q);
    bad.outcome = Outcome::Error("signal-refused-tag");
    s.hand_over(bad).ok().unwrap();
    s.hand_over(Fake::new(2, &q)).ok().unwrap();
    s.wait_idle();
    assert!(FAILS.lock().unwrap().iter().any(
        |f| f.contains("signal-refused-tag") && f.contains("could not be followed by a signal")
    ));
    assert_eq!(q.lock().unwrap().len(), 4);
}

#[test]
fn a_job_out_of_order_is_counted_and_still_sent() {
    let s = sender();
    let q = queue();
    s.hand_over(Fake::new(5, &q)).ok().unwrap();
    s.hand_over(Fake::new(4, &q)).ok().unwrap();
    s.wait_idle();
    assert_eq!(q.lock().unwrap().len(), 4);
    assert_eq!(s.take_stats()[DISORDER], 1);
}

#[test]
fn rising_fences_are_in_order_and_repeats_are_not() {
    let mut last = 0;
    assert!(in_order(&mut last, 1));
    assert!(in_order(&mut last, 3));
    assert!(!in_order(&mut last, 3));
    assert!(!in_order(&mut last, 2));
    assert!(in_order(&mut last, 4));
}

#[test]
fn many_waiters_from_other_threads_all_come_after() {
    let s = sender();
    let q = queue();
    for fence in 1..=3 {
        let mut job = Fake::new(fence, &q);
        job.sleep_ms = 10;
        s.hand_over(job).ok().unwrap();
    }
    let waiters: Vec<_> = (0..4)
        .map(|i| {
            let q = q.clone();
            std::thread::spawn(move || {
                s.wait_idle();
                q.lock().unwrap().push(format!("other {i}"));
            })
        })
        .collect();
    for w in waiters {
        w.join().unwrap();
    }
    let seen = q.lock().unwrap().clone();
    assert_eq!(
        seen[..6],
        [
            "lists 1", "signal 1", "lists 2", "signal 2", "lists 3", "signal 3"
        ]
    );
    assert_eq!(seen.len(), 10);
}

#[test]
fn the_switch_is_live_and_off_by_default() {
    if std::env::var_os("VICTORY_SEND_THREAD").is_some() {
        return;
    }
    assert!(!on(), "off by default");
    victory_core::settings::set("VICTORY_SEND_THREAD", "1").unwrap();
    let now = on();
    victory_core::settings::set("VICTORY_SEND_THREAD", "").unwrap();
    assert!(now, "a change takes effect while running");
    assert!(!on());
}

/// On a card (WARP): the job's lists and signal reach the queue from the
/// thread, the value completes only when the card gets there, and nothing of
/// the submitting thread waits for the card.
#[test]
fn a_job_signals_its_fence_from_the_thread_on_a_card() {
    use windows::Win32::Graphics::Direct3D12::{D3D12_FENCE_FLAG_NONE, ID3D12Fence};
    use windows::core::Interface;
    let Some(host) = Host::new_warp() else { return };
    let host: &'static Host = Box::leak(Box::new(host));
    let s: &'static Sender<Job> = Box::leak(Box::new(Sender::new("victory-send-test")));
    let Ok(list) = host.new_list() else { return };
    list.close();
    // SAFETY: a fence of our own.
    let Ok(gate) = (unsafe { host.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) }) else {
        return;
    };
    // SAFETY: the card runs what follows only once the gate opens.
    unsafe { host.queue.Wait(&gate, 1).unwrap() };
    let fence = 1u64 << 41;
    let job = Job { host, card: None, lists: vec![list.list.cast().ok()], apart: None, fence, signalled: false };
    assert!(s.hand_over(job).is_ok());
    s.wait_idle();
    // SAFETY: plain queries and our gate.
    unsafe {
        assert!(host.fence.GetCompletedValue() < fence, "completed before the card got there");
        gate.Signal(1).unwrap();
    }
    host.wait_fence(fence);
    // SAFETY: a plain query.
    assert!(unsafe { host.fence.GetCompletedValue() } >= fence);
}

/// (`VICTORY_CARD_RECORD_SEND`) A job records its card lists before it
/// sends; a recording that fails sends nothing, signals alone and gives its
/// failure back as it is.
#[test]
fn a_job_records_before_it_sends_and_a_failed_recording_only_signals() {
    let seen = std::cell::RefCell::new(Vec::new());
    let note = |what: &'static str| seen.borrow_mut().push(what);
    let ran = steps(
        || {
            note("record");
            Ok(())
        },
        || {
            note("lists, signal");
            Ok(())
        },
        || note("signal alone"),
    );
    assert_eq!(ran, Ok(()));
    assert_eq!(seen.take(), ["record", "lists, signal"]);
    let ran = steps(
        || {
            note("record");
            Err("could-not-record-tag".to_string())
        },
        || {
            note("lists, signal");
            Ok(())
        },
        || note("signal alone"),
    );
    assert_eq!(ran, Err("could-not-record-tag".to_string()));
    assert_eq!(seen.take(), ["record", "signal alone"]);
    let ran = steps(|| Ok(()), || Err("signal-refused".to_string()), || note("signal alone"));
    assert_eq!(ran, Err("signal-refused".to_string()));
    assert!(seen.take().is_empty(), "a failed send is not signalled again");
}

#[test]
fn the_card_record_switch_is_live_and_off_by_default() {
    use super::super::card_rec;
    if std::env::var_os("VICTORY_CARD_RECORD_SEND").is_some() {
        return;
    }
    assert!(!card_rec::on(), "off by default");
    victory_core::settings::set("VICTORY_CARD_RECORD_SEND", "1").unwrap();
    let now = card_rec::on();
    victory_core::settings::set("VICTORY_CARD_RECORD_SEND", "").unwrap();
    assert!(now, "a change takes effect while running");
    assert!(!card_rec::on());
}

/// On a card (WARP), `VICTORY_CARD_RECORD_SEND`: a sync list put among the
/// job's lists unrecorded is recorded by the thread before it sends, and the
/// card writes the same bytes as when the submitting thread records it first
/// (`record_here`): the bytes each upload copies, nothing else.
#[test]
fn a_card_list_recorded_on_the_thread_writes_the_same_bytes() {
    use super::super::card_rec::SyncRec;
    use windows::core::Interface;
    const BYTES: usize = 64 << 10;
    let Some(host) = Host::new_warp() else { return };
    let host: &'static Host = Box::leak(Box::new(host));
    let s: &'static Sender<Job> = Box::leak(Box::new(Sender::new("victory-send-test-rec")));
    let Ok(work) = host.make_card_work() else { return };
    let Some((src, src_data)) = host.shared_buffer(BYTES as u64) else { return };
    // SAFETY: the mapping of `src`, `BYTES` long; nothing on the card uses it yet.
    let src_bytes = unsafe { std::slice::from_raw_parts_mut(src_data, BYTES) };
    for (i, b) in src_bytes.iter_mut().enumerate() {
        *b = (i * 7 + 3) as u8;
    }
    let parts = [(0x100usize, 0x2300usize, 0x40u64), (0x8000, 0x8004, 0x10), (0xfff0, 0x10000, 0x0)];
    let mut want = vec![0u8; BYTES];
    for &(a, b, at) in &parts {
        want[a..b].copy_from_slice(&src_bytes[at as usize..at as usize + (b - a)]);
    }
    for (k, on_thread) in [false, true].into_iter().enumerate() {
        let Some((dst, dst_data)) = host.shared_buffer(BYTES as u64) else { return };
        // SAFETY: the mapping of `dst`, `BYTES` long; nothing on the card uses it yet.
        unsafe { std::ptr::write_bytes(dst_data, 0, BYTES) };
        let rec = SyncRec {
            items: Vec::new(),
            uploads: vec![(dst.clone(), parts.iter().map(|&(a, b, at)| (a, b, src.clone(), at)).collect())],
            patch: None,
            root: work.root.clone(),
            pso: work.pso.clone(),
            differs: work.differs.0.clone(),
            join: false,
            own: Vec::new(),
        };
        let Some(prelude) = host.make_prelude() else { return };
        let mut card = Some(Pending { sync: Some((rec, prelude.clone())), check: None });
        if !on_thread {
            record_here(&mut card);
        }
        assert_eq!(card.is_some(), on_thread);
        let fence = (1u64 << 42) + k as u64;
        let job = Job { host, card, lists: vec![prelude.1.cast().ok()], apart: None, fence, signalled: false };
        assert!(s.hand_over(job).is_ok());
        s.wait_idle();
        host.wait_fence(fence);
        // SAFETY: the card is done with `dst` (its fence completed).
        let got = unsafe { std::slice::from_raw_parts(dst_data, BYTES) };
        assert!(got == want.as_slice(), "recorded on the thread: {on_thread}");
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
