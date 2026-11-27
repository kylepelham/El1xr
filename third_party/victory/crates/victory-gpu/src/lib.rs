//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(windows)]
mod alloc_backoff;
#[cfg(windows)]
mod ambient;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg_attr(not(windows), allow(dead_code))]
mod cold_skip;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
#[cfg(windows)]
mod cores;
#[cfg(windows)]
#[cfg(windows)]
mod defrag;
pub mod descriptor;
pub mod device;
#[cfg(all(windows, feature = "dlss"))]
mod dlss;
#[cfg(windows)]
mod draw;
pub mod dxgi;
#[cfg(windows)]
pub mod external;
pub mod fence;
pub mod frame;
pub mod frametime;
#[cfg(all(windows, feature = "diagnostics"))]
mod gpucost;
#[cfg(feature = "mimalloc")]
pub mod heap;
mod heap_pass;
#[cfg(windows)]
mod heap_rounds;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
mod land_aside;
pub mod list;
#[cfg(windows)]
mod memory;
pub mod object;
#[cfg(windows)]
mod output_views;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod pacing;
pub mod pipeline;
#[cfg(windows)]
mod pixels;
#[cfg(windows)]
mod port;
#[cfg(windows)]
mod port_cbv;
#[cfg(windows)]
mod primitive;
pub mod query;
pub mod queue;
#[cfg(windows)]
mod ramline;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
mod scratch_spare;
#[cfg(windows)]
mod screenshot;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod shadow;
pub mod signature;
#[cfg(windows)]
mod stretch;
#[cfg(windows)]
mod submit_chunk;
#[cfg(windows)]
mod texback;
#[cfg(windows)]
mod texstore;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
mod tiles;
#[cfg(windows)]
pub mod titlemem;
pub mod upscale;
mod watch;
#[cfg_attr(not(windows), allow(dead_code))]
mod wc_skip;
#[cfg(windows)]
mod windowup;

#[cfg(windows)]
pub use defrag::log_defrag_counters;
#[cfg(windows)]
pub use host::on_failure;
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_view};
#[cfg(windows)]
pub use shadow::LazyAccessHold;
#[cfg(windows)]
pub use watch::{
    Remapping, notify_view_writes, title_memory_reading, title_memory_reading_range,
    title_memory_remapped, title_memory_remapping, title_memory_remapping_range,
    title_memory_written, writing as kernel_will_write, writing_hold as kernel_will_write_hold,
    written_through_view,
};
#[cfg(windows)]
pub use wc_skip::title_memory_reserved;
