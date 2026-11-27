//! The upscaler's output by a plain bilinear stretch of its colour input
//! (from `CuteBooty/victory-8gb` `over.rs`, via `Peppacodes/playablegames-peppa`
//! `b3c3766546`), used by `VICTORY_UPSCALE=stretch` and as the fallback when
//! `VICTORY_UPSCALE=dlss` cannot run NGX.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

/// A bilinear, clamping sampler at `s0` for the stages `visibility` names.
fn bilinear(visibility: D3D12_SHADER_VISIBILITY) -> D3D12_STATIC_SAMPLER_DESC {
    D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_LINEAR_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        MaxLOD: f32::MAX,
        ShaderRegister: 0,
        RegisterSpace: 0,
        ShaderVisibility: visibility,
        ..Default::default()
    }
}

/// A root signature of `parameters` and one bilinear sampler, serialized by
/// the runtime and not taken from a shader: the system compiler under Wine
/// compiles a `[RootSignature]` attribute without writing the signature
/// into the shader, and `CreateRootSignature` then fails on it (#188).
fn root_signature(
    device: &ID3D12Device,
    parameters: &[D3D12_ROOT_PARAMETER],
    sampler: D3D12_SHADER_VISIBILITY,
    flags: D3D12_ROOT_SIGNATURE_FLAGS,
) -> windows::core::Result<ID3D12RootSignature> {
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let samplers = [bilinear(sampler)];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: 1,
        pStaticSamplers: samplers.as_ptr(),
        Flags: flags,
    };
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: the description points at live slices; the blob is the
    // runtime's own, read while it lives.
    unsafe {
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        crate::pipeline::samplers::layer(device, &samplers);
        device.CreateRootSignature(0, bytes)
    }
}

/// One descriptor range of one view at register 0, at `offset` in its table.
fn range(kind: D3D12_DESCRIPTOR_RANGE_TYPE, offset: u32) -> D3D12_DESCRIPTOR_RANGE {
    D3D12_DESCRIPTOR_RANGE {
        RangeType: kind,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: offset,
    }
}

/// A root parameter that is a table of `ranges`, for the stages `visibility`
/// names.
fn table(
    ranges: &[D3D12_DESCRIPTOR_RANGE],
    visibility: D3D12_SHADER_VISIBILITY,
) -> D3D12_ROOT_PARAMETER {
    D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                NumDescriptorRanges: ranges.len() as u32,
                pDescriptorRanges: ranges.as_ptr(),
            },
        },
        ShaderVisibility: visibility,
    }
}

// ---------------------------------------------------------------------------
// The upscaler's output, by a plain stretch
// ---------------------------------------------------------------------------

/// Stands in for the console's FidelityFX upscaler: each output texel is the
/// bilinear sample of the rendered part of the colour input.
///
/// shortcut: no temporal accumulation, sharpening or use of the depth and
/// motion inputs: an image, not the upscaler's quality.
const STRETCH: &str = r#"
cbuffer Sizes : register(b0) { float2 scale; float2 texel; };
Texture2D<float4> source : register(t0);
RWTexture2D<float4> target : register(u0);
SamplerState bilinear : register(s0);
[numthreads(8, 8, 1)]
void stretch(uint3 id : SV_DispatchThreadID) {
    // Groups are 8 by 8: the last ones reach past a size that is not a
    // multiple of 8.
    uint width, height;
    target.GetDimensions(width, height);
    if (id.x >= width || id.y >= height) return;
    float2 at = (float2(id.xy) + 0.5) * texel * scale;
    // The rendered size can be the top left of a larger texture: stay half a
    // texel inside it, or the filter takes in what is beside it.
    uint sw, sh;
    source.GetDimensions(sw, sh);
    at = min(at, scale - 0.5 / float2(sw, sh));
    target[id.xy] = source.SampleLevel(bilinear, at, 0);
}
"#;

struct Stretch {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: u32,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Stretch {}

static STRETCHER: Mutex<Option<Stretch>> = Mutex::new(None);

/// Views of a stretch are kept this many stretches (two views each).
///
/// shortcut: a fixed count. The title dispatches the upscaler once a frame
/// and a list runs within the frames the title keeps in flight; 64 frames
/// is far more than it keeps.
const STRETCH_RING: u32 = 64;

fn make_stretch(device: &ID3D12Device) -> windows::core::Result<Stretch> {
    use windows::core::s;
    let cs = crate::host::compile(STRETCH, s!("stretch"), s!("cs_5_1"))?;
    let views = [
        range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0),
        range(D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1),
    ];
    let sizes = D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Constants: D3D12_ROOT_CONSTANTS {
                ShaderRegister: 0,
                RegisterSpace: 0,
                Num32BitValues: 4,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let root = root_signature(
        device,
        &[sizes, table(&views, D3D12_SHADER_VISIBILITY_ALL)],
        D3D12_SHADER_VISIBILITY_ALL,
        D3D12_ROOT_SIGNATURE_FLAG_NONE,
    )?;
    // SAFETY: the description points at live data and never releases its
    // pointer.
    unsafe {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: cs.as_ptr().cast(),
                BytecodeLength: cs.len(),
            },
            ..Default::default()
        };
        let pipeline: ID3D12PipelineState = device.CreateComputePipelineState(&desc)?;
        let views: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: STRETCH_RING * 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
        Ok(Stretch {
            root,
            pipeline,
            step: device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            views,
            next: 0,
        })
    }
}

/// A typeless colour format as the typed one shaders read and write.
fn typed(format: DXGI_FORMAT) -> DXGI_FORMAT {
    DXGI_FORMAT(match format.0 {
        1 => 2,
        9 => 10,
        23 => 24,
        27 => 28,
        f => f,
    })
}

/// Computes `[scale_x, scale_y, texel_x, texel_y]` root constants for the
/// stretch compute shader.
fn stretch_constants(
    from_width: u32,
    from_height: u32,
    to_width: u32,
    to_height: u32,
    render: [u32; 2],
) -> [f32; 4] {
    let rendered = [
        if render[0] == 0 {
            from_width
        } else {
            render[0].min(from_width)
        },
        if render[1] == 0 {
            from_height
        } else {
            render[1].min(from_height)
        },
    ];
    [
        rendered[0] as f32 / from_width as f32,
        rendered[1] as f32 / from_height as f32,
        1.0 / to_width as f32,
        1.0 / to_height as f32,
    ]
}

/// Records into `list` the stretch of the top-left `render` texels of
/// `source` (readable by compute shaders) over the whole of `target` (in
/// the unordered-access state). False if nothing was recorded.
///
/// The pass sets the list's descriptor heap, compute root signature and
/// pipeline and puts none of them back: the caller replays the title's.
pub(crate) fn record_stretch(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    target: &ID3D12Resource,
    source: &ID3D12Resource,
    render: [u32; 2],
) -> bool {
    // SAFETY: live resources.
    let (to, from) = unsafe { (target.GetDesc(), source.GetDesc()) };
    if to.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0
        || to.Width == 0
        || to.Height == 0
        || from.Width == 0
        || from.Height == 0
    {
        return false;
    }
    let mut made = STRETCHER.lock().unwrap_or_else(|e| e.into_inner());
    if made.is_none() {
        // Tried again at the next dispatch if it fails: a failure need not last.
        *made = match make_stretch(device) {
            Ok(p) => {
                tracing::info!(
                    "the upscaler's output is made by a plain stretch of its colour input"
                );
                Some(p)
            }
            Err(e) => {
                static TOLD: std::sync::Once = std::sync::Once::new();
                TOLD.call_once(|| {
                    tracing::warn!(error = %e, "no pass to stretch the upscaler's input: its output stays as it is");
                });
                None
            }
        };
    }
    let Some(p) = made.as_mut() else {
        return false;
    };
    let slot = (p.next % STRETCH_RING) * 2;
    p.next = p.next.wrapping_add(1);
    let (width, height) = (to.Width as u32, to.Height);
    let sizes = stretch_constants(from.Width as u32, from.Height, width, height, render);
    let read = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: typed(from.Format),
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    let written = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: typed(to.Format),
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    // SAFETY: an open list; slots of our heap; live resources and objects
    // (the pass's own live as long as the process).
    unsafe {
        let cpu = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetCPUDescriptorHandleForHeapStart().ptr + ((slot + i) * p.step) as usize,
        };
        let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: p.views.GetGPUDescriptorHandleForHeapStart().ptr + u64::from(slot * p.step),
        };
        device.CreateShaderResourceView(source, Some(&read), cpu(0));
        device.CreateUnorderedAccessView(target, None, Some(&written), cpu(1));
        list.SetDescriptorHeaps(&[Some(p.views.clone())]);
        list.SetComputeRootSignature(&p.root);
        list.SetPipelineState(&p.pipeline);
        list.SetComputeRoot32BitConstants(0, 4, sizes.as_ptr().cast(), 0);
        list.SetComputeRootDescriptorTable(1, gpu);
        list.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_maps_typeless_colour_formats() {
        assert_eq!(typed(DXGI_FORMAT(1)).0, 2);
        assert_eq!(typed(DXGI_FORMAT(9)).0, 10);
        assert_eq!(typed(DXGI_FORMAT(23)).0, 24);
        assert_eq!(typed(DXGI_FORMAT(27)).0, 28);
        assert_eq!(typed(DXGI_FORMAT(10)).0, 10);
        assert_eq!(typed(DXGI_FORMAT(26)).0, 26);
    }

    #[test]
    fn stretch_constants_handle_zero_dynamic_and_clamped_render_sizes() {
        // Zero render size uses full source texture.
        assert_eq!(
            stretch_constants(1920, 1080, 3840, 2160, [0, 0]),
            [1.0, 1.0, 1.0 / 3840.0, 1.0 / 2160.0]
        );
        // Dynamic subrect inside source texture scales UV range.
        assert_eq!(
            stretch_constants(2560, 1440, 3840, 2160, [1280, 720]),
            [0.5, 0.5, 1.0 / 3840.0, 1.0 / 2160.0]
        );
        // Oversized render dims clamp to source texture dims.
        assert_eq!(
            stretch_constants(1280, 720, 3840, 2160, [4096, 4096]),
            [1.0, 1.0, 1.0 / 3840.0, 1.0 / 2160.0]
        );
    }

    #[test]
    fn stretch_shader_compiles_with_fxc() {
        use windows::core::s;
        let bytecode = crate::host::compile(STRETCH, s!("stretch"), s!("cs_5_1"))
            .expect("STRETCH compute shader must compile cleanly");
        assert!(!bytecode.is_empty());
    }
}
