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

#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg_attr(not(windows), allow(dead_code))]
mod barrier_skip;
#[cfg_attr(not(windows), allow(dead_code))]
mod card_share;
#[cfg_attr(not(windows), allow(dead_code))]
mod cold_skip;
pub mod console;
#[cfg(windows)]
mod commitline;
#[cfg(windows)]
mod poolslack;
#[cfg(windows)]
mod staging_hold;
#[cfg(windows)]
mod idle_evict;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
#[cfg(windows)]
mod gpucost;
mod gpucost_slow;
#[cfg(feature = "mimalloc")]
pub mod heap;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod alloc_backoff;
#[cfg(windows)]
mod stack_commit;
#[cfg(windows)]
mod buffer_ahead;
#[cfg(windows)]
mod ambient;
#[cfg(windows)]
mod ambient_cover;
#[cfg(windows)]
mod ambient_miss;
#[cfg(windows)]
mod ambient_places;
#[cfg(windows)]
mod ambient_reflect;
#[cfg(windows)]
mod ambient_sky;
#[cfg_attr(not(windows), allow(dead_code))]
mod bake_ahead;
mod boot_prof;
#[cfg(windows)]
mod bake_check;
#[cfg(windows)]
mod bake_fresh;
#[cfg(windows)]
mod bake_name;
#[cfg(windows)]
mod bake_inputs;
#[cfg(windows)]
mod bake_publish;
#[cfg(windows)]
mod bake_replay;
#[cfg(windows)]
mod bake_spread;
#[cfg(windows)]
mod bind_hash;
#[cfg(windows)]
mod root_memo;
mod table_prof;
#[cfg(windows)]
mod bake_watch;
#[cfg(windows)]
mod bake_why;
#[cfg(windows)]
mod bake_zero;
#[cfg(windows)]
mod draw;
#[cfg(windows)]
mod dred;
#[cfg(windows)]
mod alloc_count;
#[cfg(windows)]
mod consumer_gate;
#[cfg(windows)]
mod hang_watch;
#[cfg(windows)]
mod raw_watch;
#[cfg_attr(not(windows), allow(dead_code))]
mod drop_why;
#[cfg_attr(not(windows), allow(dead_code))]
mod pieces_diag;
#[cfg_attr(not(windows), allow(dead_code))]
mod pieces_log;
#[cfg(windows)]
mod read_there;
#[cfg(windows)]
mod pieces_dump;
pub mod dxgi;
#[cfg(windows)]
pub mod external;
pub mod fence;
#[cfg(windows)]
mod flight_settle;
pub mod frame;
pub mod frametime;
pub mod hitch;
pub mod host_facts;
#[cfg(windows)]
pub mod card_awake;
#[cfg(windows)]
mod card_constants;
#[cfg(windows)]
mod joined_apart;
#[cfg(windows)]
mod port_order;
#[cfg(windows)]
mod heap_rounds;
#[cfg(windows)]
mod heap_warm;
#[cfg(windows)]
mod round_share;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod land_aside;
#[cfg(windows)]
mod list_reap;
#[cfg(windows)]
mod output_views;
pub mod menu;
#[cfg(windows)]
mod menu_shot;
pub mod modmenu;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod pacing;
#[cfg(windows)]
mod pixels;
#[cfg(windows)]
mod fetch_check;
#[cfg(windows)]
mod relay_runs;
#[cfg(windows)]
mod screenshot;
#[cfg(windows)]
mod snap;
#[cfg(windows)]
mod scratch_spare;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod launch;
mod load_rate;
#[cfg(windows)]
mod indirect_fit;
#[cfg(windows)]
mod cores;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
mod popwatch;
#[cfg(windows)]
mod present_audit;
#[cfg(windows)]
mod hold_frame;
mod flicker_catch;
#[cfg(windows)]
mod present_own;
#[cfg(windows)]
mod send_own;
mod raw_except;
#[cfg(windows)]
mod hdr_scrub;
#[cfg(windows)]
mod port;
#[cfg(windows)]
mod port_cbv;
pub mod list;
#[cfg(windows)]
pub mod native_rt;
#[cfg(windows)]
mod rt_bind;
#[cfg(windows)]
mod rt_diag;
#[cfg(windows)]
mod rt_hit_only;
pub mod object;
pub mod pipeline;
#[cfg(windows)]
mod primitive;
mod heap_pass;
pub mod query;
pub mod queue;
/// (mandy 6e80eb8648) `VICTORY_ALIAS_LATER=1`: page-mapping copies mapped behind the title's call.
pub mod alias_later;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod stretch;
#[cfg(windows)]
mod table_defer;
#[cfg(windows)]
mod table_null;
#[cfg(windows)]
mod readable_once;
#[cfg(windows)]
mod dlss;
#[cfg(windows)]
pub mod framegen;
#[cfg(windows)]
mod dlssg;
#[cfg(windows)]
mod streamline;
#[cfg(windows)]
mod ngx;
pub mod upscale;
pub mod upscale_live;
pub mod camera;
pub mod camera_diag;
pub mod camera_tag;
mod draw_dump;
#[cfg(windows)]
mod heap_find;
#[cfg(windows)]
pub mod camera_copy;
#[cfg(windows)]
pub mod camera_mv;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod shader_pack;
#[cfg(windows)]
mod shader_prefetch;
#[cfg(windows)]
mod wave_more;
mod zero_out;
mod zero_fill;
mod shadow_idle;
#[cfg(windows)]
mod face_watch;
#[cfg(windows)]
mod exposure_watch;
#[cfg(windows)]
mod target_reuse;
#[cfg(windows)]
mod skin_watch;
#[cfg(windows)]
mod tile_args_watch;
#[cfg(windows)]
mod material_demand;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
pub mod bvh_premap;
#[cfg(windows)]
mod bvh_declare;
#[cfg(windows)]
mod skin_fill;
#[cfg(windows)]
mod ramline;
#[cfg(windows)]
mod shadow;
#[cfg(windows)]
mod copy_profile;
#[cfg(all(windows, feature = "diagnostics"))]
mod trace_va;
#[cfg(windows)]
mod refresh_prof;
#[cfg(windows)]
mod stale_batch;
#[cfg(windows)]
mod sweep_every;
#[cfg(windows)]
mod card_prof;
#[cfg(windows)]
mod prep_cut;
#[cfg(windows)]
mod result_ring;
#[cfg(windows)]
mod read_back_pool;
#[cfg(windows)]
mod wide_copy;
#[cfg(windows)]
mod compare_apart;
#[cfg(windows)]
mod fresh_open;
#[cfg(windows)]
mod scratch_landing;
#[cfg(windows)]
mod submit_chunk;
#[cfg(windows)]
mod timeline;
/// (critpath #88) The title's thread placement calls; `VICTORY_OWN_CPUS`, `VICTORY_TITLE_BOOST`.
pub mod title_threads;
/// (critpath #88) `VICTORY_TITLE_WAITS=1`: the title's threads' time in our faults, and their die.
mod title_waits;
#[cfg(windows)]
mod merge_runs;
#[cfg(windows)]
mod windowup;
#[cfg(windows)]
mod tiles;
pub mod signature;
#[cfg(windows)]
mod tex_check;
mod tex_early;
#[cfg(windows)]
mod texback;
#[cfg(windows)]
mod texpool;
#[cfg(windows)]
mod texstore;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
pub mod titlemem;
#[cfg(windows)]
pub mod topology;
#[cfg(windows)]
pub mod priority;
#[cfg(windows)]
#[cfg(windows)]
mod defrag;
#[cfg(windows)]
mod loop_cap;
mod watch;
#[cfg_attr(not(windows), allow(dead_code))]
mod wc_skip;
#[cfg_attr(not(windows), allow(dead_code))]
mod default_back;

#[cfg(windows)]
pub use defrag::log_defrag_counters;
#[cfg(windows)]
pub use host::on_failure;
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_view};
#[cfg(windows)]
pub use wc_skip::title_memory_reserved;
#[cfg(windows)]
pub use shadow::LazyAccessHold;
#[cfg(windows)]
pub use watch::{
    Remapping, notify_view_writes, title_memory_reading, title_memory_reading_range, title_memory_remapped,
    title_pages_committed,
    title_memory_remapping, title_memory_remapping_range, title_memory_remapping_try, title_memory_written, writing as kernel_will_write,
    writing_hold as kernel_will_write_hold, written_through_view,
};
