//! (racefix #57) Shared D3D12 debug layer + GPU-based validation hook for
//! WARP / D3D12 unit tests (`VICTORY_TEST_D3D12_DEBUG=1`; tests only, off
//! unless set).
//!
//! Run with `--test-threads=1`: WARP shares one software device per process,
//! so the layer must be enabled once before the first `D3D12CreateDevice`
//! call in the binary.

use std::sync::Once;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_MESSAGE, D3D12_MESSAGE_SEVERITY_WARNING, D3D12GetDebugInterface, ID3D12Debug,
    ID3D12Debug1, ID3D12Device, ID3D12InfoQueue,
};
use windows::core::Interface;

/// Whether `VICTORY_TEST_D3D12_DEBUG=1` is set.
pub(crate) fn wanted() -> bool {
    std::env::var_os("VICTORY_TEST_D3D12_DEBUG").is_some_and(|v| v == "1")
}

/// Enables the D3D12 debug layer and GPU-based validation once per process,
/// before any device is created, if `VICTORY_TEST_D3D12_DEBUG=1` is set.
pub(crate) fn enable() {
    if !wanted() {
        return;
    }
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut layer: Option<ID3D12Debug> = None;
        // SAFETY: a plain interface query before device creation.
        if unsafe { D3D12GetDebugInterface(&mut layer) }.is_ok()
            && let Some(layer) = layer
        {
            // SAFETY: before the device is made. GPU-based validation tracks
            // states across submissions (decay included), which the CPU layer
            // does not.
            unsafe {
                layer.EnableDebugLayer();
                if let Ok(gbv) = layer.cast::<ID3D12Debug1>() {
                    gbv.SetEnableGPUBasedValidation(true);
                }
            }
        }
    });
}

/// The info queue of `device`, if `VICTORY_TEST_D3D12_DEBUG=1` is set and the
/// debug layer is attached.
pub(crate) fn messages(device: &ID3D12Device) -> Option<ID3D12InfoQueue> {
    wanted().then(|| device.cast().ok()).flatten()
}

/// Moves every stored message of warning severity or worse into `out`
/// (printing each to stderr), and clears the queue.
pub(crate) fn drain(queue: Option<&ID3D12InfoQueue>, out: &mut Vec<String>) {
    let Some(queue) = queue else {
        return;
    };
    // SAFETY: each message is read into a buffer of the size the queue
    // reports, then the queue is emptied.
    unsafe {
        for i in 0..queue.GetNumStoredMessages() {
            let mut size = 0;
            if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                continue;
            }
            let mut buffer = vec![0u64; size.div_ceil(8)];
            let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
            if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                let m = &*message;
                if m.Severity.0 > D3D12_MESSAGE_SEVERITY_WARNING.0 {
                    continue;
                }
                let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                let text = String::from_utf8_lossy(text);
                let line = format!(
                    "severity {} id {}: {}",
                    m.Severity.0,
                    m.ID.0,
                    text.trim_end_matches('\0')
                );
                eprintln!("d3d12 debug layer: {line}");
                out.push(line);
            }
        }
        queue.ClearStoredMessages();
    }
}

/// Drains messages of warning severity or worse from `queue` and asserts none
/// were reported.
pub(crate) fn assert_clean_queue(queue: Option<&ID3D12InfoQueue>, suite: &str) {
    let mut out = Vec::new();
    drain(queue, &mut out);
    assert!(
        out.is_empty(),
        "D3D12 debug layer warnings/errors in {suite}: {out:#?}"
    );
}

/// Drains messages of warning severity or worse from `queue` and asserts none
/// were reported except those whose D3D12 message ID is in `ignored_ids`
/// (e.g., an intentional negative-test API refusal or test-setup perf hint).
pub(crate) fn assert_clean_queue_except(
    queue: Option<&ID3D12InfoQueue>,
    suite: &str,
    ignored_ids: &[i32],
) {
    let mut out = Vec::new();
    drain(queue, &mut out);
    out.retain(|line| {
        let id = line
            .split(" id ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.parse::<i32>().ok());
        !id.is_some_and(|i| ignored_ids.contains(&i))
    });
    assert!(
        out.is_empty(),
        "D3D12 debug layer warnings/errors in {suite}: {out:#?}"
    );
}

/// Drains messages of warning severity or worse from `device` and asserts none
/// were reported.
pub(crate) fn assert_clean(device: &ID3D12Device, suite: &str) {
    let queue = messages(device);
    assert_clean_queue(queue.as_ref(), suite);
}

/// Drains messages of warning severity or worse from `device` and asserts none
/// were reported except those in `ignored_ids`.
pub(crate) fn assert_clean_except(device: &ID3D12Device, suite: &str, ignored_ids: &[i32]) {
    let queue = messages(device);
    assert_clean_queue_except(queue.as_ref(), suite, ignored_ids);
}
