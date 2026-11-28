//! A DXIL vertex, pixel, compute or mesh shader under construction: the module,
//! the `dx.op` declarations it calls, its signatures and bound resources,
//! and the metadata and container that wrap it up (SM 6.6, validator 1.6).

use super::bitcode::{Attr, Md, Module, Ty, V};
use super::container::{self, Binding, Psv, SigElement};

/// Where the shader reads its user data: `b0` in this space.
pub const USER_DATA_SPACE: u32 = 0;
/// Where immediate samplers are bound: `s<n>` in this space.
pub const SAMPLER_SPACE: u32 = 0;
/// Where root CBVs and SRVs are bound: `b<root parameter index>` and
/// `t<root parameter index>` in this space.
pub const ROOT_CBV_SPACE: u32 = 1;

/// The root SRV `t<SLOT_STRIDES>` in [`ROOT_CBV_SPACE`] of a shader that
/// reads buffers through descriptors it picks from the view heap at an
/// index it computes (SM 6.6 dynamic resources): a raw buffer of
/// [`SLOT_STRIDES_COUNT`] dwords the host keeps, dword `i` the bytes per
/// element of the buffer view at heap slot `i` (0: none or not a buffer).
/// The console's descriptor holds that stride and its hardware multiplies
/// the element index by it; the program's code does not say it, and the
/// used-resource list only names what tables hold. A shader that has this
/// root SRV also tells the host that the buffer views of the whole heap
/// must be ready when it runs, not only those its tables name.
pub const SLOT_STRIDES: u8 = 255;
/// Dwords of the [`SLOT_STRIDES`] buffer: heap indices are clamped to it.
pub const SLOT_STRIDES_COUNT: u32 = 1 << 21;

/// Title memory by address (`resource::MEMORY`): console code reads and
/// writes through raw 40-bit addresses it takes from root constants,
/// descriptors and other memory (the ray-tracing structure builder and the
/// programs that walk its structures). The host keeps copies of the title
/// memory shaders use; a shader with such an access finds the copy by the
/// address:
///
/// - the root SRV `t<SLOT_MEMORY>` in [`ROOT_CBV_SPACE`] is a directory of
///   [`MEMORY_PAGES`] entries of two dwords, one per 64 KiB of title
///   address space: the number of the host copy holding that page (0: none
///   yet) and the page's byte offset in that copy;
/// - copy `n` is the raw buffer view at view-heap slot `base + n`, `base`
///   the user-data dword of kind [`USER_MEMORY_BASE`]; slot `base` itself
///   reads zeros and drops writes;
/// - the root UAV `u<SLOT_FAULTS>` takes the page numbers of accesses that
///   found no copy ([`MEMORY_FAULTS`] dwords, dword 0 a sink), which the
///   host reads afterwards to make those copies for the frames to come.
pub const SLOT_MEMORY: u8 = 254;
/// See [`SLOT_MEMORY`].
pub const SLOT_FAULTS: u8 = 253;
/// Root UAV `u<SLOT_MEET>`: three dwords the face-relocation compute adds to,
/// meetings, wait iterations and wait atomics. Ours, not the title's.
pub const SLOT_MEET: u8 = 252;
/// Root SRV `t<SLOT_TLAS>`: a top level built with the host's DXR
/// (`VICTORY_NATIVE_BVH`) for inline ray queries ([`super::ray_query`]).
pub const SLOT_TLAS: u8 = 251;
/// Root SRV `t<SLOT_TLAS_NEAR>`: a second host top level, for the glass
/// trace's near walk (the title's `t354`; [`SLOT_TLAS`] holds its `t355`).
pub const SLOT_TLAS_NEAR: u8 = 250;
/// (heapcap, `VICTORY_HEAP_CAP=3`) The root SRV `t<SLOT_REMAP>` in [`ROOT_CBV_SPACE`]: dword `k`
/// the host view slot of title view slot [`title_view_limit`]` + k` (that limit, a null slot,
/// for one the host has none for), [`title_remap_len`] of them and one more, the null slot.
/// The host binds the dwords from [`REMAP_BASE`] on of the [`SLOT_STRIDES`] buffer of the heap.
pub const SLOT_REMAP: u8 = 249;
/// (`VICTORY_HEAP_VIEW_ADDRESS=1`) The root SRV `t<SLOT_VIEW_ADDRESSES>` in [`ROOT_CBV_SPACE`] of
/// a shader that reads the address of a descriptor it loads from the view heap at an offset it
/// computes (`heap_address`): a raw buffer of [`VIEW_ADDRESSES_COUNT`] pairs of dwords the host
/// keeps, pair `i` the title address (low, high) of the buffer or ray-tracing structure view at
/// title slot `i` (0: none). Slots past the last read the last, never written.
pub const SLOT_VIEW_ADDRESSES: u8 = 248;
/// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) The root SRV `t<SLOT_BUFFER_RANGES>` in [`ROOT_CBV_SPACE`] of
/// a shader that reads buffers it picks from the view heap by their title address
/// (`lower::buffer_address`): [`BUFFER_RANGES_COUNT`] entries of four dwords the host keeps, entry
/// `i` the buffer view at title slot `i`: its first byte's address (low, high), its size in
/// bytes, its element stride; all 0 for a slot with no buffer view. Slots past the last read the
/// last, never written.
pub const SLOT_BUFFER_RANGES: u8 = 246;
/// Entries of the [`SLOT_BUFFER_RANGES`] buffer: every title slot (1,245,184 in the retail title).
pub const BUFFER_RANGES_COUNT: u32 = 1 << 21;
/// Bit 31 of dword 1 of a [`SLOT_BUFFER_RANGES`] entry: the host already placed the view
/// in a numbered copy of [`SLOT_MEMORY`], so dword 0 is the byte offset in that copy and
/// dword 1 without this bit is the copy number (skipping the page directory and fault buffer).
pub const BUFFER_RANGE_DIRECT: u32 = 1 << 31;
/// Pairs of the [`SLOT_VIEW_ADDRESSES`] buffer.
pub const VIEW_ADDRESSES_COUNT: u32 = 1 << 21;
/// (`VICTORY_LOOP_CAP_WATCH=1`) The root UAV `u<SLOT_LOOP_CAP>` in [`ROOT_CBV_SPACE`]: two
/// dwords a program, at byte `8 * id` its user-data dword of kind [`USER_LOOP_CAP_ID`] gives
/// (masked to [`LOOP_CAP_PROGRAMS`]): lanes that left the program at a loop's iteration limit (a
/// pixel discarded, a thread's later stores skipped), then lanes that left a loop at its limit
/// by the loop's own exit, keeping their values. Ours, not the title's.
pub const SLOT_LOOP_CAP: u8 = 247;
/// Programs the [`SLOT_LOOP_CAP`] buffer counts (id 0: the ones past the last).
pub const LOOP_CAP_PROGRAMS: u32 = 1 << 14;
/// (heapcap) Where the [`SLOT_REMAP`] dwords start in the [`SLOT_STRIDES`] buffer: past every
/// slot a capped host heap has.
pub const REMAP_BASE: u32 = 1 << 20;
/// (heapcap) Title view slots the [`SLOT_REMAP`] table covers from [`title_view_limit`] on.
pub const REMAP_LEN: u32 = SLOT_STRIDES_COUNT - REMAP_BASE - 1;
/// (heapcap, `VICTORY_HEAP_CAP=4`) Dwords of the whole-heap [`SLOT_REMAP`] table
/// ([`title_remap_full`]): dword `i` the host slot of title view slot `i`; indices at or past
/// the last read the last, a null slot.
pub const FULL_REMAP: u32 = 1 << 21;
/// (heapcap, `VICTORY_HEAP_CAP=5`) A whole-heap table entry with this bit set asks for a fault
/// note: the shader still reads the host slot in the other bits, and writes the title slot to
/// the fault buffer ([`title_fault_slot`]).
pub const FAULT_FLAG: u32 = 1 << 31;
/// (heapcap) Dwords of the fault buffer: title slot `k` is noted at dword `1 + k % (n - 1)`.
pub const FAULT_DWORDS: u32 = 1 << 17;
/// Entries of the [`SLOT_MEMORY`] directory: title addresses below 128 GiB.
pub const MEMORY_PAGES: u32 = 1 << 21;
/// Dwords of the [`SLOT_FAULTS`] buffer.
pub const MEMORY_FAULTS: u32 = 4096;
/// `RootType::Other` code of a user-data dword the host computes: the view
/// heap slot of the memory copies' views ([`SLOT_MEMORY`]).
pub const USER_MEMORY_BASE: u32 = 0x100;
/// `RootType::Other` code of a user-data dword the host computes: the low
/// 32 bits of the title address of the buffer view at dword `dword` of the
/// descriptor table of root parameter `slot`; [`USER_VIEW_ADDRESS`] + 1:
/// its bits from 32 up. (Console code reads the address out of a buffer
/// descriptor and uses it as a pointer.)
pub const USER_VIEW_ADDRESS: u32 = 0x101;
/// `RootType::Other` code of a user-data dword the host computes: the
/// bytes (0 to 15) by which the raw view the host binds for the root view
/// of root parameter `slot` begins before the title's address. A raw view
/// begins on 16 bytes of its host buffer; the console's root views begin
/// anywhere (the structure builder's counters sit at addresses ending in
/// 4 and c). The shader adds the dword to every byte offset through that
/// view. Asked for every root UAV, and for the root SRVs of programs with
/// a spilled user-data table.
pub const USER_VIEW_BIAS: u32 = 0x103;
/// `RootType::Other` code of a user-data dword the host sets: the number of
/// the first thread group in x. The console dispatches any number of groups
/// in one dimension; the host at most 65,535, so a larger dispatch runs in
/// parts, each with the groups before it here. Asked for by compute programs
/// that are told the x number alone (the one-dimensional ones).
pub const USER_GROUP_BASE: u32 = 0x104;
/// `RootType::Other` code of a user-data dword the host sets: the thread
/// count in x of a `DispatchX` that counts threads, `u32::MAX` otherwise.
/// Asked for by the structure builder's programs with
/// `VICTORY_DISPATCHX_MASK=1` (`lower::thread_limit`).
pub const USER_THREAD_LIMIT: u32 = 0x105;
/// `RootType::Other` code of a user-data dword the host sets: 1 when the
/// dispatch has the glass trace's host top levels bound
/// ([`SLOT_TLAS`], [`SLOT_TLAS_NEAR`]), else 0. Asked for by the spliced
/// glass trace (`VICTORY_NATIVE_GLASS=1`, `glass_splice::SELECTOR`).
pub const USER_GLASS_NATIVE: u32 = 0x106;
/// `RootType::Other` codes of the user-data dwords the host sets for a shadow trace that picks
/// its top level per wave (`shadow_patch`, `ttmp13`/`ttmp14` read on entry): the title address
/// >> 8 (low 32 bits) of its first and second candidate structure view, 0 where the view names
/// no top level the host built.
pub const USER_SHADOW_BASE_FIRST: u32 = 0x107;
pub const USER_SHADOW_BASE_SECOND: u32 = 0x108;
/// `RootType::Other` code of a user-data dword the host sets: the program's number in the
/// [`SLOT_LOOP_CAP`] buffer (`VICTORY_LOOP_CAP_WATCH=1`).
pub const USER_LOOP_CAP_ID: u32 = 0x109;

/// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) `RootType::Other` code of a user-data dword the host sets:
/// the title view slot of the first descriptor of the table of root parameter `slot` (whatever
/// slots the host binds the table at), for constant buffers read through the table by title
/// address ([`SLOT_BUFFER_RANGES`]); [`NO_TABLE_SLOT`] for none. Asked at dword 0. Asked at
/// [`TABLE_SLOT_KEPT`] too (the value unused) when the shader also reads a constant buffer of
/// that table through its descriptor: then the host keeps making the table's constant buffers.
pub const USER_TABLE_SLOT: u32 = 0x10a;
/// See [`USER_TABLE_SLOT`].
pub const TABLE_SLOT_KEPT: u32 = 1;
/// See [`USER_TABLE_SLOT`]: past every entry of the ranges table (which reads 0 there), and
/// far from wrapping when a descriptor's distance is added.
pub const NO_TABLE_SLOT: u32 = 1 << 30;

/// `ShaderFlags` bits (metadata) and their `SFI0` feature bits.
pub(crate) mod flags {
    pub const DOUBLES: u64 = 1 << 2;
    /// `[earlydepthstencil]`: depth and stencil are tested (and written)
    /// before the pixel program runs. No `SFI0` bit.
    pub const EARLY_DEPTH: u64 = 1 << 3;
    pub const RAW_BUFFERS: u64 = 1 << 4;
    pub const STENCIL_REF: u64 = 1 << 11;
    pub const TYPED_UAV_LOADS: u64 = 1 << 13;
    pub const ROVS: u64 = 1 << 18;
    pub const WAVE_OPS: u64 = 1 << 19;
    pub const BARYCENTRICS: u64 = 1 << 22;
    pub const INT64: u64 = 1 << 20;
    pub const ATOMIC_TYPED64: u64 = 1 << 27;
    pub const ATOMIC_HEAP64: u64 = 1 << 32;
    pub const RESOURCE_HEAP: u64 = 1 << 30;
    pub const SAMPLER_HEAP: u64 = 1 << 31;
    /// AllocateRayQuery (inline ray tracing, tier 1.1).
    pub const RAYTRACING_1_1: u64 = 1 << 25;
}

pub(crate) struct Types {
    pub void: Ty,
    pub i1: Ty,
    pub i32: Ty,
    pub f32: Ty,
    pub handle: Ty,
    pub props: Ty,
    pub bind: Ty,
    pub cbuf_ret: Ty,
    pub res_ret: Ty,
}

/// What a mesh shader declares beyond its thread-group size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mesh {
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// 1 lines, 2 triangles.
    pub topology: u8,
}

/// What a geometry shader declares: triangles in, triangle strips out on
/// stream 0, one instance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub max_vertices: u32,
}

/// The null slot every host view heap keeps at its end (0: no limit). Shaders translated while
/// it is set read that slot for any view index at or past it. A read past a heap's end is
/// undefined: a host that caps its heap (NVIDIA at 1 000 000 views; a retail game's heap is
/// larger) hung its GPU (GPU-based validation: "descriptor heap index out of bounds"), and AMD
/// RDNA3 took a page fault and removed the device.
static VIEW_LIMIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// See [`VIEW_LIMIT`]: `null_slot` is the last slot of the host's view heap, kept null.
pub fn set_view_heap_limit(null_slot: u32) {
    VIEW_LIMIT.store(null_slot, std::sync::atomic::Ordering::Relaxed);
}

/// See [`VIEW_LIMIT`]: the null slot shaders translated now clamp view indices to (0: none).
pub fn view_heap_limit() -> u32 {
    VIEW_LIMIT.load(std::sync::atomic::Ordering::Relaxed)
}

/// (heapcap) `VICTORY_HEAP_CAP`: the first slot past the title's own slots on a host view heap
/// capped below the title's size, kept null (0: none). The slots after it are the host's
/// per-draw tables; a view index the title computes itself (root type `ResourceHeap`) at or past
/// it would read one of those, so shaders translated while it is set read this slot instead.
/// Indices a table gives (the host made the table) are left alone. Set by the host with
/// [`set_view_heap_limit`]; part of the translation fingerprint when not 0.
static TITLE_VIEW_LIMIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// (heapcap, `VICTORY_HEAP_CAP=3`) Title view indices at or past [`TITLE_VIEW_LIMIT`] read the
/// host slot the [`SLOT_REMAP`] table gives, this many of them (0: none; they read the null slot).
static TITLE_REMAP_LEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
thread_local! {
    /// For a test's thread alone: [`title_view_limit`].
    pub(crate) static TITLE_VIEW_LIMIT_HERE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    /// For a test's thread alone: [`title_remap_len`].
    pub(crate) static TITLE_REMAP_LEN_HERE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// (heapcap, `VICTORY_HEAP_CAP=4`) Every view index the title computes reads the host slot the
/// whole-heap [`SLOT_REMAP`] table ([`FULL_REMAP`] dwords) gives: the host lays out its heap
/// as it likes (which title slots are its own, which are packed) without a new translation.
static TITLE_REMAP_FULL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    /// For a test's thread alone: [`title_remap_full`].
    pub(crate) static TITLE_REMAP_FULL_HERE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// (heapcap, `VICTORY_HEAP_CAP=5`) The host view slot of the fault buffer's raw UAV (0: no
/// faults): a whole-heap table entry with [`FAULT_FLAG`] makes the shader note its title slot
/// there. A note not asked for goes past the buffer's end, where the store is dropped: no
/// branch, one store issued per view index.
static TITLE_FAULT_SLOT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
thread_local! {
    /// For a test's thread alone: [`title_fault_slot`].
    pub(crate) static TITLE_FAULT_SLOT_HERE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// See [`TITLE_FAULT_SLOT`].
pub fn set_title_fault_slot(slot: u32) {
    TITLE_FAULT_SLOT.store(slot, std::sync::atomic::Ordering::Relaxed);
}

/// See [`TITLE_FAULT_SLOT`].
pub fn title_fault_slot() -> u32 {
    #[cfg(test)]
    if let Some(slot) = TITLE_FAULT_SLOT_HERE.get() {
        return slot;
    }
    TITLE_FAULT_SLOT.load(std::sync::atomic::Ordering::Relaxed)
}

/// See [`TITLE_REMAP_FULL`].
pub fn set_title_remap_full(on: bool) {
    TITLE_REMAP_FULL.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// See [`TITLE_REMAP_FULL`].
pub fn title_remap_full() -> bool {
    #[cfg(test)]
    if let Some(on) = TITLE_REMAP_FULL_HERE.get() {
        return on;
    }
    TITLE_REMAP_FULL.load(std::sync::atomic::Ordering::Relaxed)
}

/// See [`TITLE_REMAP_LEN`]: `len` title slots past the limit read the remap table (0: none).
pub fn set_title_remap_len(len: u32) {
    TITLE_REMAP_LEN.store(len.min(REMAP_LEN), std::sync::atomic::Ordering::Relaxed);
}

/// See [`TITLE_REMAP_LEN`].
pub fn title_remap_len() -> u32 {
    #[cfg(test)]
    if let Some(len) = TITLE_REMAP_LEN_HERE.get() {
        return len;
    }
    TITLE_REMAP_LEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// See [`TITLE_VIEW_LIMIT`]: `slot` is the null slot past the title's slots (0: none).
pub fn set_title_view_limit(slot: u32) {
    TITLE_VIEW_LIMIT.store(slot, std::sync::atomic::Ordering::Relaxed);
}

/// See [`TITLE_VIEW_LIMIT`].
pub fn title_view_limit() -> u32 {
    #[cfg(test)]
    if let Some(slot) = TITLE_VIEW_LIMIT_HERE.get() {
        return slot;
    }
    TITLE_VIEW_LIMIT.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    /// For a test's thread alone: [`constants_raw`].
    pub(crate) static CONSTANTS_RAW_HERE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Renderer port 5 (`VICTORY_PORT_CBV=1`; off by default): constant buffers are read as raw
/// bytes, a table's directly through its descriptor slot, a root one as a root SRV,
/// so the host can give the card the title's bytes where they are (16-byte aligned, which a
/// constant buffer view cannot take) instead of a copy made at every submission. The host's
/// views must follow: every constant buffer's slot a raw view of its bytes. Part of
/// the translation: the shader cache keys on the switch.
pub fn constants_raw() -> bool {
    #[cfg(test)]
    if let Some(on) = CONSTANTS_RAW_HERE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PORT_CBV").is_ok_and(|v| v.trim() == "1"))
}

pub(crate) struct Shader {
    pub m: Module,
    pub t: Types,
    pub pixel: bool,
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    /// A mesh shader's per-primitive outputs.
    pub prims: Vec<SigElement>,
    /// A mesh shader (`threads` is its thread-group size).
    pub mesh: Option<Mesh>,
    /// A geometry shader (the rectangle stage, `rect.rs`); `pixel` is false.
    pub geometry: Option<Geometry>,
    /// Dwords of user data the shader reads (0: no user-data buffer).
    pub user_data: u32,
    /// Immediate samplers, bound as `s0..` in [`SAMPLER_SPACE`].
    pub samplers: u32,
    /// Bit n: immediate sampler n is a comparison sampler.
    pub cmp_samplers: u64,
    /// Root CBVs read, by root parameter index, with their handles.
    pub root_cbvs: Vec<(u8, V)>,
    /// Root SRVs (raw buffers) read, likewise.
    pub root_srvs: Vec<(u8, V)>,
    /// Root UAVs (raw buffers) read or written, likewise.
    pub root_uavs: Vec<(u8, V)>,
    pub flags: u64,
    /// A pixel shader whose writes to views the console orders by primitive (POPS): its
    /// heap views written or read as UAVs are rasterizer-ordered views.
    pub ordered: bool,
    /// Heap handles made with a non-uniform index.
    pub nonuniform_handles: u32,
    /// A compute shader with this thread-group size.
    pub threads: Option<[u32; 3]>,
    /// The wave size a compute shader requires (`WaveSize`).
    pub wave_size: Option<u32>,
    /// Thread-group shared memory, in dwords.
    pub lds_dwords: u32,
    lds: Option<V>,
    main: V,
    pub entry: usize,
}

impl Shader {
    pub fn new(pixel: bool) -> Self {
        let mut m = Module::default();
        let void = m.void();
        let i1 = m.int(1);
        let i8 = m.int(8);
        let i32 = m.int(32);
        let f32 = m.float();
        let i8p = m.ptr(i8);
        let handle = m.named_struct("dx.types.Handle", vec![i8p]);
        let props = m.named_struct("dx.types.ResourceProperties", vec![i32, i32]);
        let bind = m.named_struct("dx.types.ResBind", vec![i32, i32, i32, i8]);
        let cbuf_ret = m.named_struct("dx.types.CBufRet.i32", vec![i32, i32, i32, i32]);
        let res_ret = m.named_struct("dx.types.ResRet.f32", vec![f32, f32, f32, f32, i32]);
        let main_ty = m.func_ty(void, vec![]);
        m.ptr(main_ty);
        let main = m.function("main", main_ty, None, true);
        let entry = m.new_block();
        Shader {
            m,
            t: Types {
                void,
                i1,
                i32,
                f32,
                handle,
                props,
                bind,
                cbuf_ret,
                res_ret,
            },
            pixel,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh: None,
            geometry: None,
            user_data: 0,
            samplers: 0,
            cmp_samplers: 0,
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            flags: 0,
            ordered: false,
            nonuniform_handles: 0,
            threads: None,
            wave_size: None,
            lds_dwords: 0,
            lds: None,
            main,
            entry,
        }
    }

    /// The thread-group shared memory: `[lds_dwords x i32]` in address
    /// space 3.
    pub fn lds(&mut self) -> V {
        if let Some(g) = self.lds {
            return g;
        }
        let arr = self.m.array(self.lds_dwords, self.t.i32);
        let g = self.m.global("lds", arr, 3);
        self.lds = Some(g);
        g
    }

    /// Calls `dx.op.<name>` (class and overload) with `opcode` and `args`.
    pub fn op(
        &mut self,
        block: usize,
        name: &str,
        attr: Attr,
        ret: Ty,
        opcode: u32,
        args: &[V],
    ) -> V {
        let mut params = vec![self.t.i32];
        params.extend(args.iter().map(|&a| self.m.type_of(a)));
        let fty = self.m.func_ty(ret, params);
        self.m.ptr(fty);
        let f = self
            .m
            .function(&format!("dx.op.{name}"), fty, Some(attr), false);
        let mut all = vec![self.m.i32(opcode)];
        all.extend(args);
        self.m.call(block, f, all)
    }

    /// The handle of the user-data constant buffer, made in the entry block.
    pub fn user_data_handle(&mut self) -> V {
        let bind = [
            self.m.i32(0),
            self.m.i32(0),
            self.m.i32(USER_DATA_SPACE),
            self.m.i8(2), // CBV
        ];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let block = self.entry;
        let (i, f) = (self.m.i32(0), self.m.bool(false));
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, i, f],
        );
        // The size is known in `finish`, which fixes this placeholder.
        self.annotate(block, h, 13, u32::MAX)
    }

    /// The handle of the root CBV of root parameter `slot`, made in the
    /// entry block.
    pub fn root_cbv_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_cbvs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(2)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 13, 65536);
        self.root_cbvs.push((slot, h));
        h
    }

    /// The handle of the root SRV (a raw buffer) of root parameter `slot`.
    pub fn root_srv_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_srvs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(0)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.root_srvs.push((slot, h));
        h
    }

    /// The handle of the root UAV (a raw buffer) of root parameter `slot`:
    /// `u<slot>` in [`ROOT_CBV_SPACE`].
    pub fn root_uav_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_uavs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(1)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11 | 0x1000, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.root_uavs.push((slot, h));
        h
    }

    /// The handle of immediate sampler `n` (a comparison sampler when its
    /// bit in `cmp_samplers` is set).
    pub fn sampler_handle(&mut self, block: usize, n: u32) -> V {
        let bind = [
            self.m.i32(n),
            self.m.i32(n),
            self.m.i32(SAMPLER_SPACE),
            self.m.i8(3), // sampler
        ];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let (i, f) = (self.m.i32(n), self.m.bool(false));
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, i, f],
        );
        let cmp = if self.cmp_samplers >> n & 1 != 0 {
            0x8000
        } else {
            0
        };
        self.annotate(block, h, 14 | cmp, 0)
    }

    pub fn annotate(&mut self, block: usize, h: V, props0: u32, props1: u32) -> V {
        let p = [self.m.i32(props0), self.m.i32(props1)];
        let p = self.m.aggregate(self.t.props, &p);
        self.op(
            block,
            "annotateHandle",
            Attr::ReadNone,
            self.t.handle,
            216,
            &[h, p],
        )
    }

    /// A view index the title computed itself (root type `ResourceHeap`),
    /// clamped to [`title_view_limit`] when it is set.
    pub(crate) fn title_view_index(&mut self, block: usize, index: V) -> V {
        let limit = title_view_limit();
        if limit == 0 {
            return index;
        }
        let last = self.m.i32(limit);
        let i32t = self.t.i32;
        let v = self.op(block, "binary.i32", Attr::ReadNone, i32t, 40, &[index, last]);
        self.m.tag(v, super::bitcode::Tag::Clamp);
        v
    }

    /// A descriptor heap handle, annotated with `props`; `nonuniform` when
    /// the index may differ between lanes (`NonUniformResourceIndex`).
    pub fn heap_handle(
        &mut self,
        block: usize,
        index: V,
        sampler: bool,
        nonuniform: bool,
        props: (u32, u32),
    ) -> V {
        // a view index past the host heap (`set_view_heap_limit`) reads its null slot instead
        let limit = VIEW_LIMIT.load(std::sync::atomic::Ordering::Relaxed);
        // (as `dx.op.binary` UMin: a compare and select emitted here made the bitcode invalid)
        let index = if !sampler && limit != 0 {
            let null = self.m.i32(limit);
            let i32t = self.t.i32;
            let v = self.op(block, "binary.i32", Attr::ReadNone, i32t, 40, &[index, null]);
            self.m.tag(v, super::bitcode::Tag::Clamp);
            v
        } else if sampler {
            // A sampler heap has at most 2,048 samplers on any host: an
            // index past that (a number read from memory the host has not
            // got yet) stays inside the largest heap there is.
            // shortcut: not the bound heap's own count, which a title
            // heap smaller than 2,048 would need.
            let last = self.m.i32(2047);
            let i32t = self.t.i32;
            let v = self.op(block, "binary.i32", Attr::ReadNone, i32t, 40, &[index, last]);
            self.m.tag(v, super::bitcode::Tag::Clamp);
            v
        } else {
            index
        };
        self.flags |= if sampler {
            flags::SAMPLER_HEAP
        } else {
            flags::RESOURCE_HEAP
        };
        // (bit 12 of the kind field: a UAV; bit 13: rasterizer-ordered)
        let props = if self.ordered && !sampler && props.0 & 0x1000 != 0 {
            self.flags |= flags::ROVS;
            (props.0 | 0x2000, props.1)
        } else {
            props
        };
        self.nonuniform_handles += u32::from(nonuniform);
        let (s, n) = (self.m.bool(sampler), self.m.bool(nonuniform));
        let h = self.op(
            block,
            "createHandleFromHeap",
            Attr::ReadNone,
            self.t.handle,
            218,
            &[index, s, n],
        );
        self.annotate(block, h, props.0, props.1)
    }

    /// Sets `EARLY_DEPTH` (`[earlydepthstencil]`) when the console's
    /// `DB_SHADER_CONTROL.DEPTH_BEFORE_SHADER` (bit 12) is set and the
    /// pixel program exports neither depth nor stencil ref (which DXIL
    /// forbids alongside early depth/stencil).
    pub fn set_early_depth(&mut self, db_shader_control: u32) {
        const Z_EXPORT: u32 = 1 << 0;
        const STENCIL_EXPORT: u32 = 1 << 1;
        const DEPTH_BEFORE_SHADER: u32 = 1 << 12;
        if self.pixel
            && db_shader_control & DEPTH_BEFORE_SHADER != 0
            && db_shader_control & (Z_EXPORT | STENCIL_EXPORT) == 0
            && !self.outputs.iter().any(|o| matches!(o.kind, 17..=20))
        {
            self.flags |= flags::EARLY_DEPTH;
        }
    }

    /// The finished container.
    pub fn finish(mut self) -> Vec<u8> {
        let ud_size = (self.user_data * 4).next_multiple_of(16);
        self.fix_user_data_size(ud_size);
        // The clean-up, before anything is declared about the code: a
        // feature whose last use it removed must not be declared (the
        // validator compares the flags with what the code uses).
        let passes = super::shader_opt() & super::bitcode::opt::MODULE;
        if passes != 0 {
            self.m.optimise(passes);
            let used = self.m.usage();
            for (flag, used) in [
                (flags::WAVE_OPS, used.wave_ops),
                (flags::INT64, used.int64),
                (flags::DOUBLES, used.doubles),
                (flags::TYPED_UAV_LOADS, used.typed_uav_loads),
                (flags::ROVS, used.rovs),
                (flags::RESOURCE_HEAP, used.heap_views),
                (flags::SAMPLER_HEAP, used.heap_samplers),
                (flags::ATOMIC_TYPED64 | flags::ATOMIC_HEAP64, used.atomic64),
            ] {
                if !used {
                    self.flags &= !flag;
                }
            }
        }
        let mut resources = None;
        let mut bindings = Vec::new();
        let i32c = |s: &mut Self, v: u32| {
            let c = s.m.i32(v);
            s.m.md_value(c)
        };
        if self.user_data > 0
            || self.samplers > 0
            || !self.root_cbvs.is_empty()
            || !self.root_srvs.is_empty()
            || !self.root_uavs.is_empty()
        {
            let empty = self.m.md_str("");
            let mut cbvs = None;
            let mut cbv_list = Vec::new();
            if self.user_data > 0 {
                let arr = self.m.array(self.user_data.next_multiple_of(4), self.t.i32);
                let ty = self.m.named_struct("UserData", vec![arr]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let ops = vec![
                    Some(i32c(&mut self, 0)),
                    Some(self.m.md_value(u)),
                    Some(empty),
                    Some(i32c(&mut self, USER_DATA_SPACE)),
                    Some(i32c(&mut self, 0)),
                    Some(i32c(&mut self, 1)),
                    Some(i32c(&mut self, ud_size)),
                    None,
                ];
                cbv_list.push(Some(self.m.md_node(ops)));
                bindings.push(Binding {
                    kind: 2,
                    space: USER_DATA_SPACE,
                    register: 0,
                    dxil_kind: 13,
                });
            }
            let blob = self.m.array(16384, self.t.i32);
            let ty = self.m.named_struct("RootCbv", vec![blob]);
            let p = self.m.ptr(ty);
            let u = self.m.undef(p);
            for (slot, _) in self.root_cbvs.clone() {
                let ops = vec![
                    Some(i32c(&mut self, cbv_list.len() as u32)),
                    Some(self.m.md_value(u)),
                    Some(empty),
                    Some(i32c(&mut self, ROOT_CBV_SPACE)),
                    Some(i32c(&mut self, u32::from(slot))),
                    Some(i32c(&mut self, 1)),
                    Some(i32c(&mut self, 65536)),
                    None,
                ];
                cbv_list.push(Some(self.m.md_node(ops)));
                bindings.push(Binding {
                    kind: 2,
                    space: ROOT_CBV_SPACE,
                    register: u32::from(slot),
                    dxil_kind: 13,
                });
            }
            if !cbv_list.is_empty() {
                cbvs = Some(self.m.md_node(cbv_list));
            }
            let mut srvs = None;
            if !self.root_srvs.is_empty() {
                let ty = self
                    .m
                    .named_struct("struct.ByteAddressBuffer", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let mut list = Vec::new();
                for (n, (slot, _)) in self.root_srvs.clone().into_iter().enumerate() {
                    let (var, shape, extra) = super::ray_query::srv_md(&mut self, slot, u);
                    let ops = vec![
                        Some(i32c(&mut self, n as u32)),
                        Some(self.m.md_value(var)),
                        Some(empty),
                        Some(i32c(&mut self, ROOT_CBV_SPACE)),
                        Some(i32c(&mut self, u32::from(slot))),
                        Some(i32c(&mut self, 1)),
                        Some(i32c(&mut self, shape)),
                        Some(i32c(&mut self, 0)),
                        extra,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 4,
                        space: ROOT_CBV_SPACE,
                        register: u32::from(slot),
                        dxil_kind: shape,
                    });
                }
                srvs = Some(self.m.md_node(list));
            }
            let mut uavs = None;
            if !self.root_uavs.is_empty() {
                let ty = self
                    .m
                    .named_struct("struct.RWByteAddressBuffer", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let no = self.m.bool(false);
                let no = self.m.md_value(no);
                let mut list = Vec::new();
                for (n, (slot, _)) in self.root_uavs.clone().into_iter().enumerate() {
                    // Id, variable, name, space, lower bound, range size,
                    // shape (11: raw buffer), globally coherent, counter,
                    // rasterizer ordered, no extra properties.
                    let ops = vec![
                        Some(i32c(&mut self, n as u32)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, ROOT_CBV_SPACE)),
                        Some(i32c(&mut self, u32::from(slot))),
                        Some(i32c(&mut self, 1)),
                        Some(i32c(&mut self, 11)),
                        Some(no),
                        Some(no),
                        Some(no),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 7,
                        space: ROOT_CBV_SPACE,
                        register: u32::from(slot),
                        dxil_kind: 11,
                    });
                }
                uavs = Some(self.m.md_node(list));
            }
            let mut samplers = None;
            if self.samplers > 0 {
                let ty = self.m.named_struct("struct.SamplerState", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let mut list = Vec::new();
                for n in 0..self.samplers {
                    let kind = (self.cmp_samplers >> n & 1) as u32;
                    let ops = vec![
                        Some(i32c(&mut self, n)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, SAMPLER_SPACE)),
                        Some(i32c(&mut self, n)),
                        Some(i32c(&mut self, 1)),
                        // Sampler kind: 1 comparison.
                        Some(i32c(&mut self, kind)),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 1,
                        space: SAMPLER_SPACE,
                        register: n,
                        dxil_kind: 14,
                    });
                }
                samplers = Some(self.m.md_node(list));
            }
            resources = Some(self.m.md_node(vec![srvs, uavs, cbvs, samplers]));
            // PSV0 lists them as the module does: CBVs, samplers, SRVs, UAVs.
            bindings.sort_by_key(|b| match b.kind {
                2 => 0,
                1 => 1,
                7 => 3,
                _ => 2,
            });
        }
        if let Some(r) = resources {
            self.m.named_md("dx.resources", vec![r]);
        }

        let graphics = self.threads.is_none() || self.mesh.is_some();
        if graphics {
            // shortcut: every output is declared to depend on every input; the
            // exact sets need a dependence pass. Drivers read this state.
            let scalars = |els: &[SigElement]| {
                els.iter()
                    .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
                    .max()
                    .unwrap_or(0)
            };
            let (ni, no) = (scalars(&self.inputs), scalars(&self.outputs));
            let mut view = vec![ni, no];
            // Mesh shaders have no inputs: then the primitive outputs' count.
            if self.mesh.is_some() {
                view.push(scalars(&self.prims));
            }
            for _ in 0..ni {
                for k in 0..no.div_ceil(32) {
                    let bits = (no - 32 * k).min(32);
                    view.push(if bits == 32 {
                        u32::MAX
                    } else {
                        (1 << bits) - 1
                    });
                }
            }
            // A geometry shader lists four streams: the other three have
            // no outputs.
            if self.geometry.is_some() {
                view.extend([0, 0, 0]);
            }
            let arr = self.m.array(view.len() as u32, self.t.i32);
            let elems: Vec<V> = view.iter().map(|&v| self.m.i32(v)).collect();
            let arr = self.m.aggregate(arr, &elems);
            let arr = self.m.md_value(arr);
            let view_node = self.m.md_node(vec![Some(arr)]);
            self.m.named_md("dx.viewIdState", vec![view_node]);
        }

        let sigs = if graphics {
            let ins = self.signature_md(self.inputs.clone());
            let outs = self.signature_md(self.outputs.clone());
            let prims = self.signature_md(self.prims.clone());
            Some(self.m.md_node(vec![ins, outs, prims]))
        } else {
            None
        };
        // A pixel shader with an SV_StencilRef output (as DXC collects it).
        if self.pixel && self.outputs.iter().any(|o| o.kind == 20) {
            self.flags |= flags::STENCIL_REF;
        }
        // Entry properties: shader flags (tag 0), geometry state (1),
        // thread-group size (4), wave size (11).
        let mut props = Vec::new();
        if self.flags != 0 {
            props.push(i32c(&mut self, 0));
            let i64t = self.m.int(64);
            let f = self.m.int_const(i64t, self.flags as i64);
            props.push(self.m.md_value(f));
        }
        if let Some(g) = self.geometry {
            // Input primitive (3: triangles), maximum vertices, stream
            // mask, output topology (5: triangle strips), instances.
            props.push(i32c(&mut self, 1));
            let state = [3, g.max_vertices, 1, 5, 1].map(|n| Some(i32c(&mut self, n)));
            props.push(self.m.md_node(state.to_vec()));
        }
        if let Some(threads) = self.threads {
            let t = threads.map(|n| Some(i32c(&mut self, n)));
            let t = self.m.md_node(t.to_vec());
            match self.mesh {
                // Mesh state (tag 9): threads, maximum vertices and
                // primitives, topology, payload bytes.
                Some(mesh) => {
                    props.push(i32c(&mut self, 9));
                    let state = [
                        mesh.max_vertices,
                        mesh.max_primitives,
                        mesh.topology.into(),
                        super::mesh_launch::mesh_payload_bytes(),
                    ]
                    .map(|n| Some(i32c(&mut self, n)));
                    let mut node = vec![Some(t)];
                    node.extend(state);
                    props.push(self.m.md_node(node));
                }
                None => {
                    props.push(i32c(&mut self, 4));
                    props.push(t);
                }
            }
        }
        if let Some(n) = self.wave_size {
            props.push(i32c(&mut self, 11));
            let n = Some(i32c(&mut self, n));
            props.push(self.m.md_node(vec![n]));
        }
        let props =
            (!props.is_empty()).then(|| self.m.md_node(props.into_iter().map(Some).collect()));
        let main = self.m.md_value(self.main);
        let name = self.m.md_str("main");
        let entry = self
            .m
            .md_node(vec![Some(main), Some(name), sigs, resources, props]);

        let ident = self.m.md_str("victory");
        let ident = self.m.md_node(vec![Some(ident)]);
        self.m.named_md("llvm.ident", vec![ident]);
        let one = i32c(&mut self, 1);
        let six = i32c(&mut self, 6);
        let version = self.m.md_node(vec![Some(one), Some(six)]);
        self.m.named_md("dx.version", vec![version]);
        self.m.named_md("dx.valver", vec![version]);
        // Program kinds: 0 pixel, 1 vertex, 2 geometry, 5 compute, 13 mesh.
        let (name, kind) = match (self.threads, self.pixel) {
            _ if self.mesh.is_some() => ("ms", 13u32),
            _ if self.geometry.is_some() => ("gs", 2),
            (Some(_), _) => ("cs", 5u32),
            (None, true) => ("ps", 0),
            (None, false) => ("vs", 1),
        };
        let name = self.m.md_str(name);
        let sm = self.m.md_node(vec![Some(name), Some(six), Some(six)]);
        self.m.named_md("dx.shaderModel", vec![sm]);
        self.m.named_md("dx.entryPoints", vec![entry]);

        let bitcode = self.m.write();
        super::bitcode::census::note(&self.m, &bitcode);
        let mut dxil = Vec::with_capacity(24 + bitcode.len());
        dxil.extend((kind << 16 | 0x66).to_le_bytes());
        dxil.extend((((24 + bitcode.len()) / 4) as u32).to_le_bytes());
        dxil.extend(b"DXIL");
        dxil.extend(0x106u32.to_le_bytes());
        dxil.extend(16u32.to_le_bytes());
        dxil.extend((bitcode.len() as u32).to_le_bytes());
        dxil.extend(&bitcode);

        let mut features = 0u64;
        if self.flags & flags::DOUBLES != 0 {
            features |= 0x1;
        }
        if self.flags & flags::STENCIL_REF != 0 {
            features |= 0x200;
        }
        if self.flags & flags::TYPED_UAV_LOADS != 0 {
            features |= 0x800;
        }
        if self.flags & flags::ROVS != 0 {
            features |= 0x1000;
        }
        if self.flags & flags::WAVE_OPS != 0 {
            features |= 0x4000;
        }
        if self.flags & flags::INT64 != 0 {
            features |= 0x8000;
        }
        if self.flags & flags::BARYCENTRICS != 0 {
            features |= 0x2_0000;
        }
        if self.flags & flags::RESOURCE_HEAP != 0 {
            features |= 0x200_0000;
        }
        if self.flags & flags::SAMPLER_HEAP != 0 {
            features |= 0x400_0000;
        }
        if self.flags & flags::ATOMIC_TYPED64 != 0 {
            features |= 0x40_0000;
        }
        if self.flags & flags::ATOMIC_HEAP64 != 0 {
            features |= 0x1000_0000;
        }
        if self.flags & flags::RAYTRACING_1_1 != 0 {
            features |= 0x10_0000;
        }
        let sfi0 = features.to_le_bytes();
        let isg1 = container::signature(&self.inputs, true);
        let osg1 = container::signature(&self.outputs, false);
        let flag = if self.pixel {
            self.outputs.iter().any(|o| (17..=19).contains(&o.kind))
        } else {
            self.outputs.iter().any(|o| o.kind == 3)
        };
        let psv0 = container::psv(&Psv {
            stage: kind as u8,
            flag: flag && self.threads.is_none(),
            inputs: &self.inputs,
            outputs: &self.outputs,
            prims: &self.prims,
            mesh: self.mesh.map(|m| (m, self.lds_dwords * 4)),
            geometry: self.geometry.map(|g| g.max_vertices),
            bindings: &bindings,
            threads: self.threads.unwrap_or_default(),
            wave_size: self.wave_size,
        });
        let hash = [0u8; 20];
        let psg1 = container::signature(&self.prims, false);
        let mut parts: Vec<(&[u8; 4], &[u8])> =
            vec![(b"SFI0", &sfi0), (b"ISG1", &isg1), (b"OSG1", &osg1)];
        if self.mesh.is_some() {
            parts.push((b"PSG1", &psg1));
        }
        parts.extend([(b"PSV0", &psv0[..]), (b"HASH", &hash), (b"DXIL", &dxil)]);
        container::build(&parts)
    }

    /// The user-data buffer's annotation carries its size, known only now.
    fn fix_user_data_size(&mut self, size: u32) {
        let old = [self.m.i32(13), self.m.i32(u32::MAX)];
        let old = self.m.aggregate(self.t.props, &old);
        let new = [self.m.i32(13), self.m.i32(size)];
        let new = self.m.aggregate(self.t.props, &new);
        self.m.replace_const(old, new);
    }

    fn signature_md(&mut self, elements: Vec<SigElement>) -> Option<Md> {
        if elements.is_empty() {
            return None;
        }
        let mut list = Vec::new();
        for (id, e) in elements.iter().enumerate() {
            let c32 = |s: &mut Self, v: u32| {
                let c = s.m.i32(v);
                Some(s.m.md_value(c))
            };
            let c8 = |s: &mut Self, v: u8| {
                let c = s.m.i8(v);
                Some(s.m.md_value(c))
            };
            let idx = c32(self, e.index);
            let indices = Some(self.m.md_node(vec![idx]));
            let usage = if e.usage != 0 {
                let (tag, mask) = (c32(self, 3), c32(self, u32::from(e.usage)));
                Some(self.m.md_node(vec![tag, mask]))
            } else {
                None
            };
            let ops = vec![
                c32(self, id as u32),
                Some(self.m.md_str(&e.name)),
                c8(self, e.comp),
                c8(self, e.kind),
                indices,
                c8(self, e.interp),
                c32(self, 1),
                c8(self, e.cols),
                c32(self, e.row.unwrap_or(u32::MAX)),
                c8(self, if e.row.is_some() { e.col } else { u8::MAX }),
                usage,
            ];
            list.push(Some(self.m.md_node(ops)));
        }
        Some(self.m.md_node(list))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(kind: u8) -> SigElement {
        SigElement {
            name: String::new(),
            index: 0,
            kind,
            comp: 9,
            interp: 0,
            row: None,
            col: 0,
            cols: 1,
            usage: 1,
            half: false,
        }
    }

    #[test]
    fn early_depth_follows_db_shader_control_and_forbids_depth_or_stencil_exports() {
        const DEPTH_BEFORE_SHADER: u32 = 1 << 12;
        const Z_EXPORT: u32 = 1 << 0;
        const STENCIL_EXPORT: u32 = 1 << 1;

        // Pixel shader with DEPTH_BEFORE_SHADER and no depth/stencil export sets EARLY_DEPTH.
        let mut ps = Shader::new(true);
        ps.set_early_depth(DEPTH_BEFORE_SHADER);
        assert_ne!(ps.flags & flags::EARLY_DEPTH, 0);

        // Without DEPTH_BEFORE_SHADER, EARLY_DEPTH stays clear.
        let mut plain = Shader::new(true);
        plain.set_early_depth(0);
        assert_eq!(plain.flags & flags::EARLY_DEPTH, 0);

        // Z_EXPORT or STENCIL_EXPORT in DB_SHADER_CONTROL suppresses EARLY_DEPTH.
        let mut z_bit = Shader::new(true);
        z_bit.set_early_depth(DEPTH_BEFORE_SHADER | Z_EXPORT);
        assert_eq!(z_bit.flags & flags::EARLY_DEPTH, 0);

        let mut stencil_bit = Shader::new(true);
        stencil_bit.set_early_depth(DEPTH_BEFORE_SHADER | STENCIL_EXPORT);
        assert_eq!(stencil_bit.flags & flags::EARLY_DEPTH, 0);

        // Declared SV_Depth* (17..=19) or SV_StencilRef (20) outputs suppress EARLY_DEPTH.
        for kind in 17..=20 {
            let mut out_ps = Shader::new(true);
            out_ps.outputs.push(sig(kind));
            out_ps.set_early_depth(DEPTH_BEFORE_SHADER);
            assert_eq!(out_ps.flags & flags::EARLY_DEPTH, 0, "output kind {kind}");
        }

        // Non-pixel shaders never set EARLY_DEPTH.
        let mut vs = Shader::new(false);
        vs.set_early_depth(DEPTH_BEFORE_SHADER);
        assert_eq!(vs.flags & flags::EARLY_DEPTH, 0);
    }
}
