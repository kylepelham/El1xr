//! Device-removed data in every build (`VICTORY_DRED=1`; off by default).
//!
//! Batch 2 lost the card twice at world entry (`DXGI_ERROR_DEVICE_HUNG`)
//! and the log only said that the relay's fence did not reach its value:
//! the device-removed data that names the command was only in diagnostics
//! builds. With the switch on, before the device is made, the runtime keeps
//! a breadcrumb per command and the page fault's address
//! (`Host::create`); every title draw and dispatch gets a marker command
//! before it and is noted per list (its kind and pipeline, [`op`]); host
//! objects are named. When the device is removed, [`report_once`] says the
//! reason, the faulting address and the allocations it lies in (alive and
//! freed), and for each list the card did not finish the command it stopped
//! at, the commands around it and which of our lists, pipelines and shader
//! hashes they are. Every line of it starts with `dred:`.
//!
//! The per-list notes also serve the hang watch (`crate::hang_watch`), which
//! keeps only the last few of them: what was sent ([`sent`]) and what each
//! list recorded last.
//!
//! The diagnostics build's own notes (`HostList::note_op`, with
//! `VICTORY_D3D12_DRED` or this switch) place their markers themselves; in
//! that build [`op`] places none, so the k-th marker stays the k-th note.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// `VICTORY_DRED=1`: device-removed data in release builds.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_DRED").is_ok_and(|v| v.trim() == "1"))
}

/// Whether title commands are noted per list: for this report or the hang
/// watch.
pub(crate) fn tracking() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on() || crate::hang_watch::seconds().is_some())
}

/// Whether [`op`] places a marker command before each title command (the
/// diagnostics build's notes place their own).
fn marks() -> bool {
    on() && !cfg!(feature = "diagnostics")
}

/// Turns breadcrumbs and page-fault data on; before the device is made.
pub(crate) fn enable() {
    let mut settings: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
    // SAFETY: a plain query of the runtime's settings interface.
    let got = unsafe { D3D12GetDebugInterface(&mut settings) };
    match (got, settings) {
        (Ok(()), Some(settings)) => {
            // SAFETY: plain setters, before the device is made.
            unsafe {
                settings.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                settings.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
            tracing::info!(
                markers = marks(),
                "dred: device-removed data on (breadcrumbs, page faults; a marker before each title command)"
            );
        }
        (got, _) => tracing::warn!(?got, "dred: device-removed data could not be switched on"),
    }
}

/// Title commands noted per list since its reset.
const RECENT: usize = 64;

/// What a list recorded since its reset: per title command its kind and
/// pipeline. With [`on`] every one (the k-th marker command is the k-th),
/// else the last [`RECENT`] or so.
pub(crate) struct Ops {
    id: u64,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The number of the first command kept (those before were let go).
    first: u64,
    /// Kind and index in `names`.
    ops: Vec<(&'static str, u32)>,
    names: Vec<String>,
    by_pipeline: HashMap<usize, u32>,
}

impl Ops {
    pub(crate) fn new() -> Arc<Ops> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Arc::new(Ops {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            inner: Mutex::default(),
        })
    }

    /// The list's number, as its name says (`victory list #N`).
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// The list is reset.
    pub(crate) fn reset(&self) {
        crate::raw_watch::reset(self.id);
        if !tracking() {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.first = 0;
        inner.ops.clear();
        inner.names.clear();
        inner.by_pipeline.clear();
    }

    fn push(&self, kind: &'static str, pipeline: usize, name: impl FnOnce() -> String) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        let index = match inner.by_pipeline.get(&pipeline) {
            Some(&i) => i,
            None => {
                let i = inner.names.len() as u32;
                inner.names.push(name());
                inner.by_pipeline.insert(pipeline, i);
                i
            }
        };
        if !on() && inner.ops.len() >= 2 * RECENT {
            inner.ops.drain(..RECENT);
            inner.first += RECENT as u64;
        }
        inner.ops.push((kind, index));
    }

    fn text(inner: &Inner, (kind, name): (&'static str, u32)) -> String {
        format!("{kind} {}", inner.names[name as usize])
    }

    /// Everything kept, for the report.
    pub(crate) fn view(&self) -> ListView {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        ListView {
            name: format!("victory list #{}", self.id),
            first: inner.first,
            ops: inner
                .ops
                .iter()
                .map(|&op| (Ops::text(&inner, op), None))
                .collect(),
            marked: marks(),
        }
    }

    /// For the hang watch, without waiting for the list's lock: the
    /// commands recorded since the reset and the texts of the last `n`.
    pub(crate) fn recent(&self, n: usize) -> Option<(u64, Vec<String>)> {
        let inner = self.inner.try_lock().ok()?;
        let total = inner.first + inner.ops.len() as u64;
        let from = inner.ops.len().saturating_sub(n);
        Some((
            total,
            inner.ops[from..]
                .iter()
                .map(|&op| Ops::text(&inner, op))
                .collect(),
        ))
    }
}

/// A title command about to be recorded on `list`: a marker command before
/// it (with [`marks`]) and a note of its kind and pipeline. `pipeline` is
/// the pipeline object; `name` its shaders, asked once per list and reset.
pub(crate) fn op(
    list: &crate::host::HostList,
    kind: &'static str,
    pipeline: usize,
    name: impl FnOnce() -> String,
) {
    if !tracking() {
        return;
    }
    if marks() {
        const MARK: [u16; 2] = [b'v' as u16, 0];
        // SAFETY: an open list; the text is copied when recorded.
        unsafe { list.list.SetMarker(0, Some(MARK.as_ptr().cast()), 4) };
    }
    list.ops.push(kind, pipeline, name);
}

/// What the report knows of one of our lists.
pub(crate) struct ListView {
    pub(crate) name: String,
    /// The number of `ops[0]` among the list's marker commands.
    pub(crate) first: u64,
    /// Per marker command, in order: what follows it, and the host GPU
    /// range a copy there reads (diagnostics notes).
    pub(crate) ops: Vec<(String, Option<(u64, u64)>)>,
    /// Whether the list's marker commands are these (else the texts are
    /// only the last commands recorded).
    pub(crate) marked: bool,
}

/// A submission sent to the card: its fence value, its number and lists.
pub(crate) struct Flight {
    pub(crate) fence: u64,
    pub(crate) submission: u64,
    pub(crate) sent: std::time::Instant,
    pub(crate) lists: Vec<(usize, Arc<Ops>)>,
}

/// The last submissions sent, newest last.
pub(crate) static FLIGHTS: Mutex<VecDeque<Flight>> = Mutex::new(VecDeque::new());
const FLIGHTS_KEPT: usize = 64;

/// A submission `submission` of `lists` went to the card, followed by the
/// signal `fence` of the host's fence.
pub(crate) fn sent(fence: u64, submission: u64, lists: &[&crate::host::HostList]) {
    if !tracking() {
        return;
    }
    crate::hang_watch::start();
    let flight = Flight {
        fence,
        submission,
        sent: std::time::Instant::now(),
        lists: lists
            .iter()
            .map(|l| (l.list.as_raw() as usize, l.ops.clone()))
            .collect(),
    };
    let mut flights = FLIGHTS.lock().unwrap_or_else(|e| e.into_inner());
    if flights.len() >= FLIGHTS_KEPT {
        flights.pop_front();
    }
    flights.push_back(flight);
}

/// One of our lists in the flights kept, by its host list.
fn in_flights(raw: usize) -> Option<(u64, ListView)> {
    let flights = FLIGHTS.lock().unwrap_or_else(|e| e.into_inner());
    flights.iter().rev().find_map(|f| {
        f.lists
            .iter()
            .find(|(r, _)| *r == raw)
            .map(|(_, ops)| (f.submission, ops.view()))
    })
}

/// A list's breadcrumbs, as the runtime gave them.
pub(crate) struct Node {
    pub(crate) list: usize,
    pub(crate) list_name: String,
    pub(crate) queue_name: String,
    pub(crate) done: u32,
    pub(crate) history: Vec<i32>,
}

/// The device-removed report, once (empty after that, or without the
/// data on): one line per finding, each starting with `dred:`, joined
/// with new lines and led by one. `find` names lists the caller knows
/// (a failing submission's own); the others are looked up among the
/// flights sent.
pub(crate) fn report_once(
    device: &ID3D12Device,
    find: &dyn Fn(usize) -> Option<ListView>,
) -> String {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !crate::host::dred_on() || SAID.swap(true, Ordering::AcqRel) {
        return String::new();
    }
    let mut lines = Vec::new();
    // SAFETY: a plain query.
    match unsafe { device.GetDeviceRemovedReason() } {
        Err(e) => lines.push(format!("dred: removal reason {:#010x}: {e}", e.code().0)),
        Ok(()) => lines.push("dred: the device reports no removal".to_owned()),
    }
    let Ok(dred) = device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
        lines.push("dred: no device-removed data interface".to_owned());
        return join(lines);
    };
    let fault_va = page_fault(&dred, &mut lines);
    let nodes = match breadcrumbs(&dred) {
        Ok(nodes) => nodes,
        Err(e) => {
            lines.push(format!("dred: no breadcrumbs ({e})"));
            return join(lines);
        }
    };
    let look = |raw: usize| {
        find(raw)
            .map(|v| (None, v))
            .or_else(|| in_flights(raw).map(|(s, v)| (Some(s), v)))
    };
    lines.extend(describe(&nodes, &look, fault_va));
    join(lines)
}

fn join(lines: Vec<String>) -> String {
    let mut text = String::new();
    for line in lines {
        text.push('\n');
        text.push_str(&line);
    }
    text
}

/// The page fault's address and the allocations at it, if one was noted.
fn page_fault(dred: &ID3D12DeviceRemovedExtendedData1, lines: &mut Vec<String>) -> Option<u64> {
    // SAFETY: the runtime's output, valid while the device lives; the
    // nodes are walked as linked.
    unsafe {
        let fault = match dred.GetPageFaultAllocationOutput1() {
            Ok(fault) => fault,
            Err(e) => {
                lines.push(format!("dred: no page-fault data ({e})"));
                return None;
            }
        };
        if fault.PageFaultVA == 0 {
            lines.push(
                "dred: no page fault (a hang or a fault the card did not tie to an address)"
                    .to_owned(),
            );
            return None;
        }
        let va = fault.PageFaultVA;
        lines.push(format!("dred: page fault at {va:#x}"));
        for (what, head) in [
            ("inside", fault.pHeadExistingAllocationNode),
            ("inside freed", fault.pHeadRecentFreedAllocationNode),
        ] {
            let mut names = Vec::new();
            let mut node = head;
            while let Some(n) = node.as_ref()
                && names.len() < 16
            {
                let name = if n.ObjectNameW.is_null() {
                    "unnamed".to_owned()
                } else {
                    n.ObjectNameW.to_string().unwrap_or_default()
                };
                names.push(format!("{name} ({})", allocation_kind(n.AllocationType.0)));
                node = n.pNext;
            }
            if !names.is_empty() {
                lines.push(format!("dred: {what} {names:?}"));
            }
        }
        let near = format!(
            "{}{}",
            crate::shadow::near(va),
            crate::descriptor::host::near_heaps(va)
        );
        if !near.is_empty() {
            lines.push(format!("dred: near it{near}"));
        }
        Some(va)
    }
}

/// Every list's breadcrumbs, copied out.
fn breadcrumbs(dred: &ID3D12DeviceRemovedExtendedData1) -> windows::core::Result<Vec<Node>> {
    let mut nodes = Vec::new();
    // SAFETY: the runtime's output, valid while the device lives; nodes are
    // walked as linked and histories read for their counts.
    unsafe {
        let crumbs = dred.GetAutoBreadcrumbsOutput()?;
        let mut node = crumbs.pHeadAutoBreadcrumbNode;
        while let Some(p) = node.as_ref() {
            let name = |w: windows::core::PCWSTR| {
                if w.is_null() {
                    String::new()
                } else {
                    w.to_string().unwrap_or_default()
                }
            };
            let history = if p.pCommandHistory.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize)
                    .iter()
                    .map(|op| op.0)
                    .collect()
            };
            nodes.push(Node {
                list: p.pCommandList.as_ref().map_or(0, |l| l.as_raw() as usize),
                list_name: name(p.pCommandListDebugNameW),
                queue_name: name(p.pCommandQueueDebugNameW),
                done: p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0),
                history,
            });
            node = p.pNext;
        }
    }
    Ok(nodes)
}

/// The lines for the lists' breadcrumbs: every list's progress, then for
/// each the card did not finish where it stopped and the commands that
/// may have been running there.
pub(crate) fn describe(
    nodes: &[Node],
    find: &dyn Fn(usize) -> Option<(Option<u64>, ListView)>,
    fault_va: Option<u64>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let label = |n: &Node, view: Option<&(Option<u64>, ListView)>| {
        let mut s = match (view, n.list_name.is_empty()) {
            (Some((_, v)), _) => v.name.clone(),
            (None, false) => n.list_name.clone(),
            (None, true) => format!("unnamed list {:#x}", n.list),
        };
        if let Some((Some(submission), _)) = view {
            s.push_str(&format!(" (submission {submission})"));
        }
        s
    };
    let progress: Vec<String> = nodes
        .iter()
        .map(|n| {
            format!(
                "{} {}/{}",
                label(n, find(n.list).as_ref()),
                n.done,
                n.history.len()
            )
        })
        .collect();
    lines.push(format!(
        "dred: {} lists with breadcrumbs (done/commands): {}",
        nodes.len(),
        progress.join(", ")
    ));
    for n in nodes {
        let count = n.history.len();
        if n.done as usize >= count {
            continue;
        }
        let view = find(n.list);
        let queue = if n.queue_name.is_empty() {
            String::new()
        } else {
            format!(" on queue {}", n.queue_name)
        };
        lines.push(format!(
            "dred: {}{queue} stopped at command {} of {count} ({})",
            label(n, view.as_ref()),
            n.done,
            op_name(n.history[n.done as usize])
        ));
        lines.extend(stopped(
            &n.history,
            n.done as usize,
            view.as_ref().map(|(_, v)| v),
            fault_va,
        ));
    }
    lines
}

/// Where a list stopped at command `done`: the last command it finished
/// and those between the barriers around `done` (work between barriers
/// overlaps on the card), each with what its marker says.
pub(crate) fn stopped(
    history: &[i32],
    done: usize,
    view: Option<&ListView>,
    fault_va: Option<u64>,
) -> Vec<String> {
    let marker = D3D12_AUTO_BREADCRUMB_OP_SETMARKER.0;
    let barrier = |op: &i32| {
        *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER.0
            || *op == D3D12_AUTO_BREADCRUMB_OP_BARRIER.0
    };
    // The number of each marker command among the list's markers.
    let mut numbers = vec![None; history.len()];
    let mut k = 0u64;
    for (i, op) in history.iter().enumerate() {
        if *op == marker {
            numbers[i] = Some(k);
            k += 1;
        }
    }
    let said = |i: usize| -> String {
        let Some(view) = view.filter(|v| v.marked) else {
            return String::new();
        };
        let Some(number) = i.checked_sub(1).and_then(|m| numbers[m]) else {
            return ": not a title command".to_owned();
        };
        match number
            .checked_sub(view.first)
            .and_then(|at| view.ops.get(at as usize))
        {
            Some((text, range)) => {
                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                format!(": {text}{}", place.unwrap_or_default())
            }
            None => format!(": title command {number}, not noted"),
        }
    };
    let mut lines = Vec::new();
    match history[..done].iter().rposition(|op| *op != marker) {
        Some(last) => lines.push(format!(
            "dred:   last finished: command {last} {}{}",
            op_name(history[last]),
            said(last)
        )),
        None => lines.push("dred:   it finished no command".to_owned()),
    }
    let from = history[..done]
        .iter()
        .rposition(barrier)
        .map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    lines.push(format!(
        "dred:   commands {from}..{to} lie between its barriers:"
    ));
    let mut shown = 0;
    // From just before the command the card had not finished: the first
    // twelve after the barrier named draws long done when a list of 79 mesh
    // draws stopped at its 45th (run 41).
    for i in from.max(done.saturating_sub(4))..to {
        if history[i] == marker {
            continue;
        }
        if shown == 12 {
            lines.push(format!("dred:     ... {} more", to - i));
            break;
        }
        shown += 1;
        let at = if i == done { " <- not finished" } else { "" };
        lines.push(format!(
            "dred:     command {i} {}{}{at}",
            op_name(history[i]),
            said(i)
        ));
    }
    if let Some(view) = view
        && !view.marked
    {
        let last: Vec<&str> = view
            .ops
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(|(t, _)| t.as_str())
            .collect();
        lines.push(format!(
            "dred:   its last title commands recorded (no markers): {last:?}"
        ));
    }
    lines
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads.
pub(crate) fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// A breadcrumb's command by name.
pub(crate) fn op_name(op: i32) -> String {
    const NAMES: &[(D3D12_AUTO_BREADCRUMB_OP, &str)] = &[
        (D3D12_AUTO_BREADCRUMB_OP_SETMARKER, "SetMarker"),
        (D3D12_AUTO_BREADCRUMB_OP_BEGINEVENT, "BeginEvent"),
        (D3D12_AUTO_BREADCRUMB_OP_ENDEVENT, "EndEvent"),
        (D3D12_AUTO_BREADCRUMB_OP_DRAWINSTANCED, "DrawInstanced"),
        (
            D3D12_AUTO_BREADCRUMB_OP_DRAWINDEXEDINSTANCED,
            "DrawIndexedInstanced",
        ),
        (D3D12_AUTO_BREADCRUMB_OP_EXECUTEINDIRECT, "ExecuteIndirect"),
        (D3D12_AUTO_BREADCRUMB_OP_DISPATCH, "Dispatch"),
        (
            D3D12_AUTO_BREADCRUMB_OP_COPYBUFFERREGION,
            "CopyBufferRegion",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_COPYTEXTUREREGION,
            "CopyTextureRegion",
        ),
        (D3D12_AUTO_BREADCRUMB_OP_COPYRESOURCE, "CopyResource"),
        (D3D12_AUTO_BREADCRUMB_OP_COPYTILES, "CopyTiles"),
        (
            D3D12_AUTO_BREADCRUMB_OP_RESOLVESUBRESOURCE,
            "ResolveSubresource",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_CLEARRENDERTARGETVIEW,
            "ClearRenderTargetView",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_CLEARUNORDEREDACCESSVIEW,
            "ClearUnorderedAccessView",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_CLEARDEPTHSTENCILVIEW,
            "ClearDepthStencilView",
        ),
        (D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER, "ResourceBarrier"),
        (D3D12_AUTO_BREADCRUMB_OP_EXECUTEBUNDLE, "ExecuteBundle"),
        (D3D12_AUTO_BREADCRUMB_OP_PRESENT, "Present"),
        (
            D3D12_AUTO_BREADCRUMB_OP_RESOLVEQUERYDATA,
            "ResolveQueryData",
        ),
        (D3D12_AUTO_BREADCRUMB_OP_BEGINSUBMISSION, "BeginSubmission"),
        (D3D12_AUTO_BREADCRUMB_OP_ENDSUBMISSION, "EndSubmission"),
        (
            D3D12_AUTO_BREADCRUMB_OP_ATOMICCOPYBUFFERUINT,
            "AtomicCopyBufferUINT",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_ATOMICCOPYBUFFERUINT64,
            "AtomicCopyBufferUINT64",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_RESOLVESUBRESOURCEREGION,
            "ResolveSubresourceRegion",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_WRITEBUFFERIMMEDIATE,
            "WriteBufferImmediate",
        ),
        (
            D3D12_AUTO_BREADCRUMB_OP_SETPIPELINESTATE1,
            "SetPipelineState1",
        ),
        (D3D12_AUTO_BREADCRUMB_OP_DISPATCHMESH, "DispatchMesh"),
        (D3D12_AUTO_BREADCRUMB_OP_BARRIER, "Barrier"),
        (
            D3D12_AUTO_BREADCRUMB_OP_BEGIN_COMMAND_LIST,
            "BeginCommandList",
        ),
    ];
    NAMES
        .iter()
        .find(|(o, _)| o.0 == op)
        .map_or_else(|| format!("op {op}"), |(_, n)| (*n).to_owned())
}

/// An allocation's kind by name, for the page-fault lines.
fn allocation_kind(kind: i32) -> String {
    const NAMES: &[(D3D12_DRED_ALLOCATION_TYPE, &str)] = &[
        (D3D12_DRED_ALLOCATION_TYPE_RESOURCE, "resource"),
        (D3D12_DRED_ALLOCATION_TYPE_HEAP, "heap"),
        (
            D3D12_DRED_ALLOCATION_TYPE_DESCRIPTOR_HEAP,
            "descriptor heap",
        ),
        (D3D12_DRED_ALLOCATION_TYPE_PIPELINE_STATE, "pipeline"),
        (D3D12_DRED_ALLOCATION_TYPE_COMMAND_LIST, "command list"),
        (
            D3D12_DRED_ALLOCATION_TYPE_COMMAND_ALLOCATOR,
            "command allocator",
        ),
        (D3D12_DRED_ALLOCATION_TYPE_QUERY_HEAP, "query heap"),
        (
            D3D12_DRED_ALLOCATION_TYPE_COMMAND_SIGNATURE,
            "command signature",
        ),
    ];
    NAMES
        .iter()
        .find(|(o, _)| o.0 == kind)
        .map_or_else(|| format!("type {kind}"), |(_, n)| (*n).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARK: i32 = 0;
    const DRAW: i32 = 4;
    const DISPATCH: i32 = 6;
    const BARRIER: i32 = 15;
    const COPY: i32 = 7;

    fn view(ops: &[&str], first: u64, marked: bool) -> ListView {
        ListView {
            name: "victory list #7".into(),
            first,
            ops: ops.iter().map(|s| ((*s).to_owned(), None)).collect(),
            marked,
        }
    }

    #[test]
    fn names_ops_and_unknowns() {
        assert_eq!(op_name(DRAW), "DrawIndexedInstanced");
        assert_eq!(op_name(DISPATCH), "Dispatch");
        assert_eq!(op_name(999), "op 999");
        assert_eq!(allocation_kind(34), "resource");
        assert_eq!(allocation_kind(5), "type 5");
    }

    #[test]
    fn pairs_markers_with_title_commands_where_the_list_stopped() {
        // barrier, [mark draw] [mark dispatch] copy [mark draw] barrier
        let history = [
            BARRIER, MARK, DRAW, MARK, DISPATCH, COPY, MARK, DRAW, BARRIER,
        ];
        let v = view(
            &[
                "indexed [vertex a, pixel b]",
                "dispatch [compute c]",
                "indexed [vertex d]",
            ],
            0,
            true,
        );
        // Finished up to the first draw (commands 0..=2); stopped at 3.
        let lines = stopped(&history, 3, Some(&v), None);
        assert_eq!(
            lines[0],
            "dred:   last finished: command 2 DrawIndexedInstanced: indexed [vertex a, pixel b]"
        );
        assert_eq!(lines[1], "dred:   commands 1..8 lie between its barriers:");
        assert!(lines.contains(&"dred:     command 4 Dispatch: dispatch [compute c]".to_owned()));
        assert!(
            lines.contains(&"dred:     command 5 CopyBufferRegion: not a title command".to_owned())
        );
        assert!(
            lines.contains(
                &"dred:     command 7 DrawIndexedInstanced: indexed [vertex d]".to_owned()
            )
        );
        // The marker it stopped at is flagged as not finished.
        let lines = stopped(&history, 4, Some(&v), None);
        assert!(lines.contains(
            &"dred:     command 4 Dispatch: dispatch [compute c] <- not finished".to_owned()
        ));
    }

    #[test]
    fn counts_markers_from_the_first_kept() {
        let history = [MARK, DRAW, MARK, DRAW, MARK, DISPATCH];
        // The first marker's note was let go: the kept ones start at 1.
        let v = view(&["second", "third"], 1, true);
        let lines = stopped(&history, 5, Some(&v), None);
        assert!(lines.contains(
            &"dred:     command 1 DrawIndexedInstanced: title command 0, not noted".to_owned()
        ));
        assert!(lines.contains(&"dred:     command 3 DrawIndexedInstanced: second".to_owned()));
        assert!(lines.contains(&"dred:     command 5 Dispatch: third <- not finished".to_owned()));
    }

    #[test]
    fn unmarked_lists_show_their_last_commands() {
        let history = [DRAW, DISPATCH];
        let v = view(&["a", "b"], 0, false);
        let lines = stopped(&history, 1, Some(&v), None);
        assert_eq!(
            lines[0],
            "dred:   last finished: command 0 DrawIndexedInstanced"
        );
        assert_eq!(
            lines.last().unwrap(),
            "dred:   its last title commands recorded (no markers): [\"a\", \"b\"]"
        );
    }

    #[test]
    fn places_the_fault_in_a_noted_range() {
        let history = [MARK, COPY];
        let mut v = view(&["copy"], 0, true);
        v.ops[0].1 = Some((0x1000, 0x100));
        let lines = stopped(&history, 1, Some(&v), Some(0x1010));
        assert!(lines.contains(&"dred:     command 1 CopyBufferRegion: copy (fault at source +0x10) <- not finished".to_owned()));
        assert_eq!(
            fault_in(0xf00, (0x1000, 0x100)),
            " (fault 0x100 below its source)"
        );
        assert_eq!(
            fault_in(0x1180, (0x1000, 0x100)),
            " (fault 0x80 past its source's end)"
        );
    }

    #[test]
    fn describes_every_list_and_the_unfinished_ones() {
        let nodes = [
            Node {
                list: 1,
                list_name: "victory list #3".into(),
                queue_name: String::new(),
                done: 2,
                history: vec![MARK, DRAW],
            },
            Node {
                list: 2,
                list_name: String::new(),
                queue_name: "direct".into(),
                done: 1,
                history: vec![MARK, DISPATCH, BARRIER],
            },
            Node {
                list: 3,
                list_name: String::new(),
                queue_name: String::new(),
                done: 0,
                history: vec![COPY],
            },
        ];
        let find = |raw: usize| {
            (raw == 2).then(|| {
                (
                    Some(412),
                    view(&["dispatch [compute 6779f6d8f122374e]"], 0, true),
                )
            })
        };
        let lines = describe(&nodes, &find, None);
        assert_eq!(
            lines[0],
            "dred: 3 lists with breadcrumbs (done/commands): victory list #3 2/2, victory list #7 (submission 412) 1/3, unnamed list 0x3 0/1"
        );
        assert_eq!(
            lines[1],
            "dred: victory list #7 (submission 412) on queue direct stopped at command 1 of 3 (Dispatch)"
        );
        assert!(lines.contains(&"dred:     command 1 Dispatch: dispatch [compute 6779f6d8f122374e] <- not finished".to_owned()));
        assert!(lines.iter().any(|l| l == "dred: unnamed list 0x3 stopped at command 0 of 1 (CopyBufferRegion)"));
        assert!(lines.contains(&"dred:   it finished no command".to_owned()));
    }

    #[test]
    fn keeps_the_last_commands_without_the_report() {
        let ops = Ops::new();
        for i in 0..(3 * RECENT) {
            ops.push("draw", i % 3, || format!("p{}", i % 3));
        }
        let (total, last) = ops.recent(2).unwrap();
        assert_eq!(last.len(), 2);
        assert!(total >= 2);
        let v = ops.view();
        assert_eq!(v.first + v.ops.len() as u64, total);
        assert_eq!(v.ops.last().unwrap().0, last[1]);
    }
}
