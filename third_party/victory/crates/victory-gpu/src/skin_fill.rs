//! `VICTORY_SKIN_FILL=1`: skin's own probe inputs get the ambient stand-in's light before
//! the frame's first skin lighting draw.
//!
//! Port of Linox-IDK/LNX-victory `0f9a071979` (GPL-3.0), adapted to our `crate::ambient`.
//! With the ray-tracing structure builder off and `VICTORY_BVH_NO_HIT=1` the stand-in raises
//! only the resolved diffuse light (`crate::ambient::LIGHT`). The skin lighting shaders
//! (`MATERIAL_SHEEN_ANISOTROPY_SSS` materials) also read the screen probes' radiance atlas
//! ([`ATLAS`], R32_UINT holding R9G9B9E5) and the probes' 13-layer array ([`LAYERS`]), which
//! only the traced passes write: skin is black while eyes, lip gloss and hair (which read the
//! resolved light) stay lit. Here both are set to one value before the first skin draw of a
//! frame that reads them: the room's slow mean the stand-in keeps (times its share), or its
//! constant (`VICTORY_AMBIENT=constant:<level>`). The atlas takes it everywhere; the layers
//! as `VICTORY_SKIN_LAYERS` says (`first`, the default: layer 0 alone, the others 0; `all`;
//! `off`: left).
//!
//! LNX's `VICTORY_SKIN_AT_DRAW=0` (fill at the next light resolve instead: skin stayed black)
//! and `VICTORY_SKIN_WIDE=1` (the player's `SKIN` materials too: see-through) are not ported.
//! Not in the shader or pipeline key: no translated shader or title pipeline changes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The radiance atlas skin reads: root parameter and dword in its table (an SRV).
pub(crate) const ATLAS: (u8, u32) = (0, 968);
/// The probes' layer array (or volume) skin reads beside it.
pub(crate) const LAYERS: (u8, u32) = (1, 80);

/// The programs whose pixel stages are skin's lighting.
const SKIN_PROGRAM: &[u8] = b"MATERIAL_SHEEN_ANISOTROPY_SSS";

/// Live: looked at by every draw's preparation (one cached load when off).
pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_SKIN_FILL", |v| u64::from(v == Some("1")));

pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// What the layers take: `Some(first_only)`, or `None` (left as they are).
fn parse_layers(set: &str) -> Option<bool> {
    match set.trim() {
        "off" => None,
        "all" => Some(false),
        _ => Some(true),
    }
}

/// `VICTORY_SKIN_LAYERS=first|all|off` (`first` when unset).
pub(crate) fn layers() -> Option<bool> {
    static SET: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *SET.get_or_init(|| parse_layers(&std::env::var("VICTORY_SKIN_LAYERS").unwrap_or_default()))
}

/// Whether a container is a skin lighting program (by its name).
pub(crate) fn skin_program(container: &[u8]) -> bool {
    container
        .windows(SKIN_PROGRAM.len())
        .any(|w| w == SKIN_PROGRAM)
}

/// The textures filled in a frame: each once.
struct Filled {
    frame: u64,
    done: Vec<usize>,
}

impl Filled {
    /// Whether `texture` is not filled yet in `frame` (it is now).
    fn first(&mut self, frame: u64, texture: usize) -> bool {
        if self.frame != frame {
            self.frame = frame;
            self.done.clear();
        }
        if self.done.contains(&texture) {
            return false;
        }
        self.done.push(texture);
        true
    }
}

static FILLED: Mutex<Filled> = Mutex::new(Filled {
    frame: u64::MAX,
    done: Vec::new(),
});

/// Whether atlas `texture` is not filled yet this frame (it counts as filled from now).
pub(crate) fn first_this_frame(texture: usize) -> bool {
    let frame = crate::queue::presented();
    FILLED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .first(frame, texture)
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    /// Skin lighting draws that read both inputs.
    Draws,
    /// Atlases filled.
    Atlas,
    /// Layer arrays or volumes filled.
    Layers,
    /// Textures that could not be filled (format, shape, no unordered access, no pass).
    Refused,
    /// Skin draws whose table had no atlas view.
    NoView,
    /// First skin draws of a frame before the stand-in had any light (or with it off).
    NotReady,
}
const N: usize = 6;
const NAMES: [&str; N] = [
    "draws",
    "atlas",
    "layers",
    "refused",
    "no_view",
    "not_ready",
];
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// The frame the counts are of.
static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

/// Counts `n` of `c` in this frame; the first count of a new frame logs the last one's.
pub(crate) fn add(c: C, n: u64) {
    let now = crate::queue::presented();
    let was = FRAME.load(Relaxed);
    if was != now && FRAME.compare_exchange(was, now, Relaxed, Relaxed).is_ok() && was != u64::MAX {
        let c: [u64; N] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed));
        tracing::info!(frame = was, counts = %line(&c), "skin fill frame");
    }
    COUNTS[c as usize].fetch_add(n, Relaxed);
}

fn line(c: &[u64; N]) -> String {
    NAMES
        .iter()
        .zip(c)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One source for both shapes: `VOLUME` defined for a 3D texture, else a 2D array (a plain
/// 2D texture is an array of one). `first_only`: slices past the first take 0.
const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u1), DescriptorTable(UAV(u0))"
#define CELLS (64 * 36)
cbuffer Fill : register(b0) { float level; float constant; uint first_only; uint unused; };
// The ambient stand-in's cells: the room's slow mean at CELLS.
RWStructuredBuffer<float4> cells : register(u1);
#ifdef VOLUME
RWTexture3D<uint> image : register(u0);
#else
RWTexture2DArray<uint> image : register(u0);
#endif
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float top = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(top, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (top * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void fill(uint3 at : SV_DispatchThreadID) {
    uint w, h, n;
    image.GetDimensions(w, h, n);
    if (at.x >= w || at.y >= h || at.z >= n) return;
    float3 light;
    if (constant > 0.0) {
        light = float3(1.0, 0.98, 0.94) * constant;
    } else {
        light = cells[CELLS].rgb;
        light = any(isnan(light)) || any(isinf(light)) ? 0.0 : max(light, 0.0) * level;
    }
    image[at] = (first_only == 0 || at.z == 0) ? pack(light) : 0u;
}
"#;

#[cfg(windows)]
pub(crate) use pass::{Light, fill};

#[cfg(windows)]
mod pass {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_UINT;
    use windows::core::s;

    use super::{C, SOURCE, add};
    use crate::host::{HostList, host};

    /// Views in our heap; one a fill, taken in turn (a slot comes round again only after
    /// this many fills, long after the list that used it ran).
    const VIEWS: u32 = 256;
    const UAV: u32 = D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0 as u32;
    const READ: u32 = (D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0
        | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0) as u32;

    /// What the fill takes from the ambient stand-in (`crate::ambient::skin_light`).
    pub(crate) struct Light<'a> {
        /// Its cells' buffer (the room's slow mean is in it).
        pub(crate) cells: &'a ID3D12Resource,
        pub(crate) level: f32,
        pub(crate) constant: f32,
    }

    struct Pass {
        root: ID3D12RootSignature,
        array: ID3D12PipelineState,
        volume: ID3D12PipelineState,
        views: ID3D12DescriptorHeap,
        step: u32,
        next: AtomicU32,
    }
    // SAFETY: D3D12 objects are free-threaded; the slot counter is atomic.
    unsafe impl Send for Pass {}
    // SAFETY: as above.
    unsafe impl Sync for Pass {}

    pub(super) fn code(volume: bool) -> windows::core::Result<Vec<u8>> {
        let define = if volume { "#define VOLUME\n" } else { "" };
        crate::host::compile(&format!("{define}{SOURCE}"), s!("fill"), s!("cs_5_1"))
    }

    pub(super) fn make(
        device: &ID3D12Device,
    ) -> windows::core::Result<(
        ID3D12RootSignature,
        ID3D12PipelineState,
        ID3D12PipelineState,
    )> {
        let array = code(false)?;
        let volume = code(true)?;
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &array)?;
            let pipeline = |code: &[u8]| {
                device.CreateComputePipelineState::<ID3D12PipelineState>(
                    &D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: code.as_ptr().cast(),
                            BytecodeLength: code.len(),
                        },
                        ..Default::default()
                    },
                )
            };
            let (array, volume) = (pipeline(&array)?, pipeline(&volume)?);
            Ok((root, array, volume))
        }
    }

    fn pass() -> Option<&'static Pass> {
        static PASS: OnceLock<Option<Pass>> = OnceLock::new();
        PASS.get_or_init(|| {
            let host = host()?;
            let made = || -> windows::core::Result<Pass> {
                let (root, array, volume) = make(&host.device)?;
                // SAFETY: plain creations on a live device.
                unsafe {
                    let views: ID3D12DescriptorHeap =
                        host.device
                            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                                NumDescriptors: VIEWS,
                                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                                NodeMask: 0,
                            })?;
                    let step = host
                        .device
                        .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
                    Ok(Pass {
                        root,
                        array,
                        volume,
                        views,
                        step,
                        next: AtomicU32::new(0),
                    })
                }
            };
            made()
                .inspect_err(|e| tracing::warn!(error = %e, "skin fill: no pass"))
                .ok()
        })
        .as_ref()
    }

    /// Why a texture is not filled, said once each.
    fn refused(texture: &D3D12_RESOURCE_DESC, why: &'static str) -> bool {
        add(C::Refused, 1);
        static SAID: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if !said.contains(&why) {
            said.push(why);
            tracing::warn!(
                why,
                format = texture.Format.0,
                dimension = texture.Dimension.0,
                width = texture.Width,
                height = texture.Height,
                depth = texture.DepthOrArraySize,
                flags = texture.Flags.0,
                "skin fill: a texture left as it is"
            );
        }
        false
    }

    /// Sets title texture `texture` (one a draw is about to read) to `light`, in `list`:
    /// to unordered access, filled, back to a shader read. `layers`: it is the probes'
    /// layer array or volume (more than one slice); `first_only`: slices past the first
    /// take 0. Whether it recorded anything (the list's compute root signature, pipeline
    /// and heaps are then ours).
    pub(crate) fn fill(
        list: &HostList,
        texture: crate::object::This,
        layers: bool,
        first_only: bool,
        light: &Light,
    ) -> bool {
        let Some(copy) = crate::resource::host_copy(texture) else {
            add(C::NoView, 1);
            return false;
        };
        // SAFETY: a live resource.
        let d = unsafe { copy.texture.GetDesc() };
        let volume = d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        if d.Format != DXGI_FORMAT_R32_UINT {
            return refused(&d, "not R32_UINT");
        }
        if !volume && d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || d.SampleDesc.Count != 1 {
            return refused(&d, "not a 2D or 3D texture");
        }
        if layers && d.DepthOrArraySize < 2 || !layers && (volume || d.DepthOrArraySize != 1) {
            return refused(&d, "another shape");
        }
        if d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return refused(&d, "no unordered access");
        }
        let (Some(host), Some(pass)) = (host(), pass()) else {
            return refused(&d, "no pass");
        };
        crate::list::ensure_held_state(list, &copy, UAV);
        list.keep(&copy.texture);
        list.keep(light.cells);
        let slot = pass.next.fetch_add(1, Relaxed) % VIEWS;
        let slices = u32::from(d.DepthOrArraySize);
        let view = if volume {
            D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE3D,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture3D: D3D12_TEX3D_UAV {
                        MipSlice: 0,
                        FirstWSlice: 0,
                        WSize: slices,
                    },
                },
            }
        } else {
            D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_UAV {
                        MipSlice: 0,
                        FirstArraySlice: 0,
                        ArraySize: slices,
                        PlaneSlice: 0,
                    },
                },
            }
        };
        let constants = [
            light.level.to_bits(),
            light.constant.to_bits(),
            u32::from(first_only && layers),
            0,
        ];
        let l = &list.list;
        // SAFETY: an open list; a live texture the list keeps, in the unordered-access state;
        // a view in a slot no list in flight reads; the cells' buffer the list keeps (always
        // used for unordered access, as the stand-in uses it).
        unsafe {
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: pass.views.GetCPUDescriptorHandleForHeapStart().ptr
                    + (slot * pass.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: pass.views.GetGPUDescriptorHandleForHeapStart().ptr
                    + u64::from(slot * pass.step),
            };
            host.device.CreateUnorderedAccessView(
                &copy.texture,
                None::<&ID3D12Resource>,
                Some(&view),
                cpu,
            );
            l.SetDescriptorHeaps(&[Some(pass.views.clone())]);
            l.SetComputeRootSignature(&pass.root);
            l.SetPipelineState(if volume { &pass.volume } else { &pass.array });
            l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            l.SetComputeRootUnorderedAccessView(1, light.cells.GetGPUVirtualAddress());
            l.SetComputeRootDescriptorTable(2, gpu);
            l.Dispatch((d.Width as u32).div_ceil(8), d.Height.div_ceil(8), slices);
            let barrier = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                ..Default::default()
            };
            l.ResourceBarrier(&[barrier]);
        }
        crate::list::ensure_held_state(list, &copy, READ);
        add(if layers { C::Layers } else { C::Atlas }, 1);
        static SAID: std::sync::Mutex<[bool; 2]> = std::sync::Mutex::new([false; 2]);
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if !said[usize::from(layers)] {
            said[usize::from(layers)] = true;
            tracing::info!(
                layers,
                volume,
                first_only,
                width = d.Width,
                height = d.Height,
                slices,
                "skin fill: skin's probe input gets the ambient light"
            );
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_set() {
        assert_eq!(
            on(),
            std::env::var_os("VICTORY_SKIN_FILL").is_some_and(|v| v == "1")
        );
    }

    #[test]
    fn layer_settings() {
        assert_eq!(parse_layers(""), Some(true));
        assert_eq!(parse_layers("first"), Some(true));
        assert_eq!(parse_layers(" all "), Some(false));
        assert_eq!(parse_layers("off"), None);
    }

    #[test]
    fn skin_programs_by_name() {
        assert!(skin_program(
            b"..PS_DeferredLighting_MATERIAL_SHEEN_ANISOTROPY_SSS_3c5d8c00.."
        ));
        assert!(!skin_program(
            b"..PS_DeferredLighting_MATERIAL_SHEEN_ANISOTROPY_3c5d8c00.."
        ));
        assert!(!skin_program(b"..PS_Character_SKIN_3c5d8c00.."));
        assert!(!skin_program(b""));
    }

    #[test]
    fn each_texture_once_a_frame() {
        let mut f = Filled {
            frame: u64::MAX,
            done: Vec::new(),
        };
        assert!(f.first(7, 0x10));
        assert!(!f.first(7, 0x10));
        assert!(f.first(7, 0x20));
        assert!(f.first(8, 0x10));
        assert!(!f.first(8, 0x10));
        assert_eq!(f.done, [0x10]);
    }

    #[test]
    fn the_line_names_every_count() {
        assert_eq!(
            line(&[3, 1, 1, 0, 2, 5]),
            "draws=3 atlas=1 layers=1 refused=0 no_view=2 not_ready=5"
        );
    }

    /// Both forms compile with the system's compiler and the host takes them.
    #[cfg(windows)]
    #[test]
    fn the_passes_build() {
        let (array, volume) = (
            pass::code(false).expect("array form"),
            pass::code(true).expect("volume form"),
        );
        assert_ne!(array, volume);
        let Some(host) = crate::host::host() else {
            return;
        };
        pass::make(&host.device).expect("the passes");
    }
}
