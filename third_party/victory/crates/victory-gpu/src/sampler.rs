//! Samplers as the host describes them, from the console's sampler
//! description (`CreateSamplerX`), which names the hardware's sampler
//! fields. (Immediate samplers in shader code come decoded with the
//! translation.)

use windows::Win32::Graphics::Direct3D12::*;

/// The fields both forms share.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fields {
    /// Point 0, bilinear 1, anisotropic point 2, anisotropic bilinear 3.
    mag: u32,
    min: u32,
    /// None 0, point 1, linear 2.
    mip: u32,
    /// Plain 0, minimum 1, maximum 2.
    mode: u32,
    /// Host address modes.
    address: [u32; 3],
    anisotropy: u32,
    /// Host comparison function.
    compare: u32,
    min_lod: f32,
    max_lod: f32,
    bias: f32,
    border: [f32; 4],
}

impl Fields {
    /// `aniso_point_mip`: the host filters anisotropically with point mips
    /// ([`aniso_point_mip`]).
    fn host(&self, aniso_point_mip: bool) -> D3D12_SAMPLER_DESC {
        let linear = |f: u32| u32::from(f == 1 || f == 3);
        let aniso = self.mag >= 2 || self.min >= 2;
        // The console compares when the instruction says so, with the
        // sampler's function; the host needs a comparison sampler for that.
        // A sampler given a real test (less ... greater-or-equal) is one;
        // never, always and none are plain samplers.
        const LESS: u32 = 2;
        const GREATER_EQUAL: u32 = 7;
        let compares = (LESS..=GREATER_EQUAL).contains(&self.compare);
        let reduction = match self.mode {
            1 => 2,
            2 => 3,
            _ => u32::from(compares),
        };
        let filter = if aniso {
            // Anisotropic with point mips where the host has it, as for
            // static samplers (`victory_shader::emit::StaticSampler`);
            // else with linear mips, the nearest.
            0x54 | u32::from(self.mip == 2 || !aniso_point_mip)
        } else {
            linear(self.min) << 4 | linear(self.mag) << 2 | u32::from(self.mip == 2)
        };
        let desc = D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER((reduction << 7 | filter) as i32),
            AddressU: D3D12_TEXTURE_ADDRESS_MODE(self.address[0] as i32),
            AddressV: D3D12_TEXTURE_ADDRESS_MODE(self.address[1] as i32),
            AddressW: D3D12_TEXTURE_ADDRESS_MODE(self.address[2] as i32),
            MipLODBias: self.bias,
            MaxAnisotropy: if aniso {
                self.anisotropy.clamp(1, 16)
            } else {
                1
            },
            ComparisonFunc: D3D12_COMPARISON_FUNC(self.compare.max(1) as i32),
            BorderColor: self.border,
            MinLOD: self.min_lod,
            // Without a mip filter the console samples mip 0 only.
            MaxLOD: if self.mip == 0 {
                self.min_lod
            } else {
                self.max_lod
            },
        };
        if clamp_on() { clamped(desc) } else { desc }
    }
}

/// Whether console samplers get their values clamped into the host's ranges
/// (`VICTORY_SAMPLER_CLAMP=1`; off by default, read once).
fn clamp_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_SAMPLER_CLAMP")
            .is_ok_and(|v| v.trim() == "1");
        if on {
            tracing::info!(
                "console samplers clamped into the host's ranges (VICTORY_SAMPLER_CLAMP=1; VItoriaBr #129)"
            );
        }
        on
    })
}

/// Console samplers [`clamped`] changed, since the start.
pub(crate) static SAMPLERS_CLAMPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `desc` with its values in the host's ranges, which the title's words can
/// leave: NaN or infinite LOD bounds, a maximum LOD below the minimum, a bias
/// past 16 levels, an address mode the host has no name for, a comparison
/// function on a sampler that does not compare. (Ported from
/// uiliannn/VItoriaBr #129, wilu222.) A sampler without a mip filter keeps
/// its maximum at its minimum (see [`Fields::host`]).
fn clamped(desc: D3D12_SAMPLER_DESC) -> D3D12_SAMPLER_DESC {
    let lod = |value: f32, fallback: f32| {
        if value.is_nan() {
            fallback
        } else {
            value.clamp(-D3D12_FLOAT32_MAX, D3D12_FLOAT32_MAX)
        }
    };
    let address = |mode: D3D12_TEXTURE_ADDRESS_MODE| match mode.0 {
        1..=5 => mode,
        _ => D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
    };
    let min_lod = lod(desc.MinLOD, 0.0);
    let out = D3D12_SAMPLER_DESC {
        AddressU: address(desc.AddressU),
        AddressV: address(desc.AddressV),
        AddressW: address(desc.AddressW),
        MipLODBias: if desc.MipLODBias.is_nan() {
            0.0
        } else {
            desc.MipLODBias
                .clamp(D3D12_MIP_LOD_BIAS_MIN, D3D12_MIP_LOD_BIAS_MAX)
        },
        // Only a comparison sampler (reduction 1) reads the function.
        ComparisonFunc: if (desc.Filter.0 >> 7) & 3 == 1 {
            desc.ComparisonFunc
        } else {
            D3D12_COMPARISON_FUNC_NEVER
        },
        MinLOD: min_lod,
        MaxLOD: lod(desc.MaxLOD, min_lod).max(min_lod),
        ..desc
    };
    if bits(&out) != bits(&desc) {
        let n = SAMPLERS_CLAMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 8 || n.is_multiple_of(1000) {
            tracing::info!(
                clamped = n,
                from = ?bits(&desc),
                to = ?bits(&out),
                "sampler clamp: a console sampler had values outside the host's ranges (VICTORY_SAMPLER_CLAMP)"
            );
        }
    }
    out
}

/// The fields of a sampler description as bits (NaN equal to itself).
fn bits(d: &D3D12_SAMPLER_DESC) -> [u32; 13] {
    [
        d.Filter.0 as u32,
        d.AddressU.0 as u32,
        d.AddressV.0 as u32,
        d.AddressW.0 as u32,
        d.MipLODBias.to_bits(),
        d.MaxAnisotropy,
        d.ComparisonFunc.0 as u32,
        d.BorderColor[0].to_bits(),
        d.BorderColor[1].to_bits(),
        d.BorderColor[2].to_bits(),
        d.BorderColor[3].to_bits(),
        d.MinLOD.to_bits(),
        d.MaxLOD.to_bits(),
    ]
}

/// Whether the host GPU filters anisotropically with point mips
/// (`D3D12_OPTIONS19`, optional): asked once. Older runtimes do not know
/// the query, which means no.
pub(crate) fn aniso_point_mip(device: &ID3D12Device) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        // SAFETY: the feature query writes the structure of the given size.
        unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS19,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }
        .is_ok()
            && options.AnisoFilterWithPointMipSupported.as_bool()
    })
}

/// The console's sampler description (27 words): flags, magnification,
/// minification, mip and depth filters, filter mode, three address modes,
/// minimum and maximum LOD, LOD bias, secondary bias, anisotropy, its bias
/// and threshold, comparison, border color, then tuning words.
/// `aniso_point_mip`: as [`aniso_point_mip`] says.
pub(crate) fn console(words: &[u32; 27], aniso_point_mip: bool) -> D3D12_SAMPLER_DESC {
    let f = |i: usize| f32::from_bits(words[i]);
    Fields {
        mag: words[1],
        min: words[2],
        mip: words[3],
        mode: words[5],
        address: [words[6], words[7], words[8]],
        min_lod: f(9),
        max_lod: f(10),
        bias: f(11),
        anisotropy: words[13],
        compare: words[16],
        border: [f(17), f(18), f(19), f(20)],
    }
    .host(aniso_point_mip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_descriptions_decode() {
        let mut w = [0u32; 27];
        w[1] = 1; // bilinear magnification
        w[2] = 0; // point minification
        w[3] = 2; // linear mips
        w[6..9].copy_from_slice(&[3, 3, 3]);
        w[10] = 1000.0f32.to_bits();
        let d = console(&w, true);
        assert_eq!(d.Filter.0, 0x5, "min point, mag and mip linear");
        assert_eq!(d.AddressW, D3D12_TEXTURE_ADDRESS_MODE_CLAMP);
        assert_eq!(d.MaxLOD, 1000.0);
        // A less-or-equal test makes a comparison sampler; never does not.
        w[16] = 4;
        assert_eq!(console(&w, true).Filter.0, 0x85);
        assert_eq!(
            console(&w, true).ComparisonFunc,
            D3D12_COMPARISON_FUNC_LESS_EQUAL
        );
        w[16] = 1;
        assert_eq!(console(&w, true).Filter.0, 0x5);
    }

    /// Anisotropic samplers keep their mip filter: point mips where the
    /// host has anisotropic point-mip filtering (as static samplers do),
    /// linear mips where it has not; comparison and min/max reduction
    /// stay.
    #[test]
    fn anisotropic_samplers_keep_point_mips_where_the_host_can() {
        let mut w = [0u32; 27];
        w[1] = 3; // anisotropic bilinear magnification
        w[2] = 3; // and minification
        w[13] = 8;
        for (mip, supported, filter) in [
            (1, true, 0x54),
            (1, false, 0x55),
            (2, true, 0x55),
            (2, false, 0x55),
            (0, true, 0x54),
        ] {
            w[3] = mip;
            let d = console(&w, supported);
            assert_eq!(
                d.Filter.0, filter,
                "mip filter {mip}, point-mip support {supported}"
            );
            assert_eq!(d.MaxAnisotropy, 8);
        }
        w[3] = 1;
        w[16] = 4; // less or equal: a comparison sampler
        assert_eq!(console(&w, true).Filter.0, 0xd4);
        assert_eq!(console(&w, false).Filter.0, 0xd5);
        w[16] = 0;
        w[5] = 2; // maximum reduction
        assert_eq!(console(&w, true).Filter.0, 0x1d4);
        assert_eq!(console(&w, false).Filter.0, 0x1d5);
        w[5] = 1; // minimum reduction
        assert_eq!(console(&w, true).Filter.0, 0x154);
        assert_eq!(console(&w, false).Filter.0, 0x155);
    }

    /// The host takes the anisotropic samplers it says it can filter: a
    /// point-mip one where it has the feature, made in a sampler heap of
    /// ours, leaves the device as it was. It says which it made (a run
    /// without a device says it was skipped).
    #[test]
    fn the_host_takes_its_anisotropic_samplers() {
        let Some(host) = crate::host::host() else {
            eprintln!("skipped: no host GPU here");
            return;
        };
        let supported = aniso_point_mip(&host.device);
        let mut w = [0u32; 27];
        (w[1], w[2], w[3], w[13]) = (3, 3, 1, 16);
        w[6..9].copy_from_slice(&[1, 1, 1]);
        let desc = console(&w, supported);
        assert_eq!(desc.Filter.0, if supported { 0x54 } else { 0x55 });
        let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
            NumDescriptors: 1,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        };
        // SAFETY: plain creation; the slot is the heap's first.
        unsafe {
            let heap: ID3D12DescriptorHeap = host.device.CreateDescriptorHeap(&heap_desc).unwrap();
            host.device
                .CreateSampler(&desc, heap.GetCPUDescriptorHandleForHeapStart());
            assert!(
                host.device.GetDeviceRemovedReason().is_ok(),
                "point-mip support {supported}"
            );
        }
        eprintln!("made an anisotropic sampler, filter {:#x}", desc.Filter.0);
    }

    /// The plain description, then clamped: LOD bounds finite and ordered,
    /// NaN falling back to 0 (minimum) or the minimum (maximum).
    #[test]
    fn clamped_lod_bounds_are_finite_and_ordered() {
        let mut w = [0u32; 27];
        w[3] = 2; // linear mips
        w[9] = 4.0f32.to_bits();
        w[10] = 2.0f32.to_bits();
        let d = console(&w, true);
        assert_eq!((d.MinLOD, d.MaxLOD), (4.0, 2.0), "unclamped: as the title wrote it");
        let c = clamped(d);
        assert_eq!((c.MinLOD, c.MaxLOD), (4.0, 4.0));
        w[9] = f32::NAN.to_bits();
        w[10] = f32::NAN.to_bits();
        let d = console(&w, true);
        assert!(d.MinLOD.is_nan() && d.MaxLOD.is_nan());
        let c = clamped(d);
        assert_eq!((c.MinLOD, c.MaxLOD), (0.0, 0.0));
        w[9] = f32::NEG_INFINITY.to_bits();
        w[10] = f32::INFINITY.to_bits();
        let c = clamped(console(&w, true));
        assert_eq!((c.MinLOD, c.MaxLOD), (-D3D12_FLOAT32_MAX, D3D12_FLOAT32_MAX));
        // Without a mip filter both are the (clamped) minimum.
        w[3] = 0;
        w[9] = 3.0f32.to_bits();
        w[10] = 8.0f32.to_bits();
        let c = clamped(console(&w, true));
        assert_eq!((c.MinLOD, c.MaxLOD), (3.0, 3.0));
        w[9] = f32::NAN.to_bits();
        let c = clamped(console(&w, true));
        assert_eq!((c.MinLOD, c.MaxLOD), (0.0, 0.0));
    }

    /// Bias, address modes and the comparison function land in the host's
    /// ranges; a sampler already inside them is left as it was.
    #[test]
    fn clamped_bias_address_and_comparison_stay_in_host_ranges() {
        let mut w = [0u32; 27];
        w[6..9].copy_from_slice(&[3, 3, 3]);
        let d = console(&w, true);
        assert_eq!(bits(&clamped(d)), bits(&d), "already in range: unchanged");
        w[11] = 100.0f32.to_bits();
        assert_eq!(console(&w, true).MipLODBias, 100.0);
        assert_eq!(clamped(console(&w, true)).MipLODBias, D3D12_MIP_LOD_BIAS_MAX);
        w[11] = (-100.0f32).to_bits();
        assert_eq!(clamped(console(&w, true)).MipLODBias, D3D12_MIP_LOD_BIAS_MIN);
        w[11] = f32::NAN.to_bits();
        assert_eq!(clamped(console(&w, true)).MipLODBias, 0.0);
        // Address modes 1..=5 pass; others become clamp.
        w[6..9].copy_from_slice(&[0, 5, 9]);
        let c = clamped(console(&w, true));
        assert_eq!(c.AddressU, D3D12_TEXTURE_ADDRESS_MODE_CLAMP);
        assert_eq!(c.AddressV, D3D12_TEXTURE_ADDRESS_MODE_MIRROR_ONCE);
        assert_eq!(c.AddressW, D3D12_TEXTURE_ADDRESS_MODE_CLAMP);
        // A minimum or maximum reduction, or a plain sampler with a host
        // value past the functions, takes no comparison function.
        w[5] = 1;
        w[16] = 4;
        assert_eq!(clamped(console(&w, true)).ComparisonFunc, D3D12_COMPARISON_FUNC_NEVER);
        w[5] = 0;
        w[16] = 99;
        assert_eq!(console(&w, true).ComparisonFunc.0, 99);
        assert_eq!(clamped(console(&w, true)).ComparisonFunc, D3D12_COMPARISON_FUNC_NEVER);
        // A comparison sampler keeps its function.
        w[16] = 4;
        assert_eq!(clamped(console(&w, true)).ComparisonFunc, D3D12_COMPARISON_FUNC_LESS_EQUAL);
    }
}
