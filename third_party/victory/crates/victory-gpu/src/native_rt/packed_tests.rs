use super::*;

/// Title memory for a test: `bytes` at `base`; reads outside it fail.
pub(crate) struct Memory {
    pub(crate) base: u64,
    pub(crate) bytes: Vec<u8>,
}

impl Memory {
    pub(crate) fn reader(&self) -> impl FnMut(u64, &mut [u8]) -> bool + '_ {
        move |at, out| {
            let Some(from) = at.checked_sub(self.base) else {
                return false;
            };
            let from = from as usize;
            match self.bytes.get(from..from + out.len()) {
                Some(src) => {
                    out.copy_from_slice(src);
                    true
                }
                None => false,
            }
        }
    }
}

/// `indices` (whole triangles) in the meshlet form, as the title's files
/// hold it: meshlets of at most 128 table entries, `first` added to every
/// meshlet's first entry (to reach entries past 65,535).
pub(crate) fn encode_meshlets(indices: &[u16], first: u32) -> Vec<u8> {
    let mut table: Vec<u16> = vec![0; first as usize];
    let mut meshlets: Vec<u32> = Vec::new();
    let mut records = Vec::new();
    let mut local: Vec<u16> = Vec::new();
    for tri in indices.chunks_exact(3) {
        let fits = tri.iter().filter(|v| !local.contains(v)).count() + local.len() <= 128;
        if meshlets.is_empty() || !fits {
            meshlets.push(table.len() as u32);
            local.clear();
        }
        let mut rec = (meshlets.len() as u32 - 1) << 21;
        for (k, v) in tri.iter().enumerate() {
            let at = local.iter().position(|l| l == v).unwrap_or_else(|| {
                local.push(*v);
                table.push(*v);
                local.len() - 1
            });
            rec |= (at as u32) << (7 * k);
        }
        records.push(rec);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(meshlets.len() as u32).to_le_bytes());
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for m in &meshlets {
        // The first entry's bit 16 in bit 14 of the first u32.
        out.extend_from_slice(&((m >> 16) << 14).to_le_bytes());
        out.extend_from_slice(&(m & 0xffff).to_le_bytes());
        out.extend_from_slice(&[0; 8]);
    }
    for r in &records {
        out.extend_from_slice(&r.to_le_bytes());
    }
    for v in &table {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Quantized positions with their 128-byte header first: (x 24 bits,
/// y 24 bits, z 16 bits) each, `stride` bytes apart.
pub(crate) fn encode_positions(
    q: &[[u32; 3]],
    offset: [f32; 3],
    scale: [f32; 3],
    axis: u32,
    stride: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; HEADER as usize];
    let o = OFFSET_AT as usize;
    for c in 0..3 {
        out[o + 4 * c..o + 4 * c + 4].copy_from_slice(&offset[c].to_le_bytes());
        let s = SCALE_AT as usize + 4 * c;
        out[s..s + 4].copy_from_slice(&scale[c].to_le_bytes());
    }
    out[o + 12..o + 16].copy_from_slice(&axis.to_le_bytes());
    for p in q {
        let d0 = (p[0] << 8) | (p[1] & 0xff);
        let d1 = (p[1] >> 8) | (p[2] << 16);
        let mut v = vec![0u8; stride];
        v[..4].copy_from_slice(&d0.to_le_bytes());
        v[4..8].copy_from_slice(&d1.to_le_bytes());
        out.extend_from_slice(&v);
    }
    out
}

/// A strip of `n` quads: 16-bit indices over `2 * n + 2` vertices.
fn strip(n: u16) -> Vec<u16> {
    (0..n)
        .flat_map(|i| {
            let a = 2 * i;
            [a, a + 1, a + 2, a + 1, a + 3, a + 2]
        })
        .collect()
}

#[test]
fn meshlet_indices_come_back_as_written() {
    // 300 quads: several meshlets of at most 128 entries each.
    let want = strip(300);
    let mem = Memory {
        base: 0xc_5613_fc00,
        bytes: encode_meshlets(&want, 0),
    };
    let tris = (want.len() / 3) as u32;
    let got = indices(mem.base, tris, 602, false, &mut mem.reader()).unwrap();
    assert_eq!(got, want);
    // Fewer triangles than records: the first ones.
    let got = indices(mem.base, 5, 602, false, &mut mem.reader()).unwrap();
    assert_eq!(got, want[..15]);
}

#[test]
fn a_meshlet_entry_past_65535_takes_bit_14_of_the_first_word() {
    let want = strip(4);
    let mem = Memory {
        base: 0x1000,
        bytes: encode_meshlets(&want, 70_000),
    };
    assert_eq!(
        indices(mem.base, 8, 10, false, &mut mem.reader()).unwrap(),
        want
    );
}

#[test]
fn stride_bit_15_swaps_first_and_third() {
    assert_eq!(stride(0x8008), 8);
    assert!(flips(0x8008));
    assert!(!flips(12));
    let want = strip(2);
    let mem = Memory {
        base: 0x2000,
        bytes: encode_meshlets(&want, 0),
    };
    let got = indices(mem.base, 4, 6, true, &mut mem.reader()).unwrap();
    let flipped: Vec<u16> = want
        .chunks_exact(3)
        .flat_map(|t| [t[2], t[1], t[0]])
        .collect();
    assert_eq!(got, flipped);
}

#[test]
fn bad_meshlet_buffers_are_refused() {
    let want = strip(3);
    let bytes = encode_meshlets(&want, 0);
    let mem = Memory {
        base: 0x4000,
        bytes,
    };
    // More triangles than records.
    assert_eq!(
        indices(mem.base, 7, 8, false, &mut mem.reader()),
        Err(Refused::Header)
    );
    // An index at the vertex count.
    assert_eq!(
        indices(mem.base, 6, 7, false, &mut mem.reader()),
        Err(Refused::Index)
    );
    // Not readable.
    assert_eq!(
        indices(0x10, 6, 8, false, &mut mem.reader()),
        Err(Refused::Unreadable)
    );
    // A triangle naming a meshlet past the count.
    let mut bytes = mem.bytes.clone();
    let rec_at = 12 + 16;
    bytes[rec_at + 3] |= 0x20;
    let bad = Memory {
        base: 0x4000,
        bytes,
    };
    assert_eq!(
        indices(bad.base, 6, 8, false, &mut bad.reader()),
        Err(Refused::Meshlet)
    );
}

#[test]
fn quantized_positions_decode_as_the_builder_does() {
    let q = [
        [0, 0, 0],
        [0xff_ffff, 0xab_cdef, 0xffff],
        [0x12_3456, 0x65_4321, 0x7fff],
    ];
    let offset = [-100.0, 20.0, 3.5];
    let scale = [0.001, 0.002, 0.25];
    let want =
        |p: [u32; 3]| -> [f32; 3] { std::array::from_fn(|c| offset[c] + scale[c] * p[c] as f32) };
    for (axis, order) in [
        (0u32, [2usize, 1, 0]),
        (1, [1, 2, 0]),
        (2, [0, 1, 2]),
        (7, [0, 1, 2]),
    ] {
        for stride in [8usize, 12] {
            let bytes = encode_positions(&q, offset, scale, axis, stride);
            let mem = Memory {
                base: 0xc_5613_f800,
                bytes,
            };
            let vb = mem.base + HEADER;
            let got = positions(vb, 3, stride as u64, &mut mem.reader()).unwrap();
            for (g, p) in got.iter().zip(q) {
                let w = want(p);
                assert_eq!(*g, order.map(|c| w[c]), "axis {axis} stride {stride}");
            }
        }
    }
}

#[test]
fn bad_positions_are_refused() {
    let q = [[1, 2, 3]];
    let mem = Memory {
        base: 0x8000,
        bytes: encode_positions(&q, [f32::NAN, 0.0, 0.0], [1.0; 3], 2, 8),
    };
    let vb = mem.base + HEADER;
    assert_eq!(
        positions(vb, 1, 8, &mut mem.reader()),
        Err(Refused::Position)
    );
    // A stride under the 8 bytes of a position, and reads past the end.
    assert_eq!(positions(vb, 1, 4, &mut mem.reader()), Err(Refused::Header));
    assert_eq!(
        positions(vb, 2, 8, &mut mem.reader()),
        Err(Refused::Unreadable)
    );
    assert_eq!(positions(vb, 0, 8, &mut mem.reader()), Ok(vec![]));
}

/// On the card (`cargo test -p victory-gpu packed_blas_on_the_card --
/// --ignored`, under the `gpu` lock; not the game): a grid in the title's
/// two forms, decoded, builds a bottom level from `R16_UINT` indices that
/// matches one built from the plain grid with `R32_UINT` indices.
#[test]
#[ignore = "on the card, under the gpu lock"]
fn packed_blas_on_the_card() {
    use super::super::refit_bench::{hardware, upload};
    use super::super::{buffer, uav_barrier};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    let Some(device) = hardware() else {
        panic!("no hardware device");
    };
    // A 24 x 24 grid of quads; heights in the 16-bit axis, placed as y.
    let side = 24u32;
    let row = side + 1;
    let q: Vec<[u32; 3]> = (0..row * row)
        .map(|k| [(k % row) * 40_000, (k / row) * 40_000, (k * 977) % 0xffff])
        .collect();
    let (offset, scale, axis) = ([-50.0, 2.0, 10.0], [0.0001, 0.0001, 0.0005], 1);
    let plain: Vec<[f32; 3]> = q
        .iter()
        .map(|p| {
            let p: [f32; 3] = std::array::from_fn(|c| offset[c] + scale[c] * p[c] as f32);
            [p[1], p[2], p[0]]
        })
        .collect();
    let idx: Vec<u16> = (0..side)
        .flat_map(|y| (0..side).map(move |x| y * row + x))
        .flat_map(|a| [a, a + 1, a + row, a + 1, a + row + 1, a + row].map(|v| v as u16))
        .collect();
    let mem_i = Memory {
        base: 0xc_5613_fc00,
        bytes: encode_meshlets(&idx, 0),
    };
    let mem_v = Memory {
        base: 0xc_5600_0000,
        bytes: encode_positions(&q, offset, scale, axis, 8),
    };
    let tris = (idx.len() / 3) as u32;
    let got_i = indices(mem_i.base, tris, row * row, false, &mut mem_i.reader()).unwrap();
    let got_v = positions(mem_v.base + HEADER, row * row, 8, &mut mem_v.reader()).unwrap();
    assert_eq!(got_i, idx);
    assert_eq!(got_v, plain);
    let floats = |v: &[[f32; 3]]| -> Vec<u8> { v.iter().flatten().flat_map(|c| c.to_le_bytes()).collect() };
    let ib16 = upload(&device, &got_i.iter().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>());
    let ib32 = upload(&device, &idx.iter().flat_map(|i| u32::from(*i).to_le_bytes()).collect::<Vec<u8>>());
    let vb_decoded = upload(&device, &floats(&got_v));
    let vb_plain = upload(&device, &floats(&plain));
    // SAFETY: a test harness of plain D3D12 calls on objects made here.
    unsafe {
        let geometry = |ib: &ID3D12Resource, format: u32, vb: &ID3D12Resource| D3D12_RAYTRACING_GEOMETRY_DESC {
            Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
            Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE,
            Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                    Transform3x4: 0,
                    IndexFormat: DXGI_FORMAT(format as i32),
                    VertexFormat: DXGI_FORMAT(R32G32B32_FLOAT as i32),
                    IndexCount: idx.len() as u32,
                    VertexCount: row * row,
                    IndexBuffer: ib.GetGPUVirtualAddress(),
                    VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                        StartAddress: vb.GetGPUVirtualAddress(),
                        StrideInBytes: POSITION_STRIDE,
                    },
                },
            },
        };
        let geometries = [
            geometry(&ib16, R16_UINT, &vb_decoded),
            geometry(&ib32, 42, &vb_plain),
        ];
        let inputs = |g: &D3D12_RAYTRACING_GEOMETRY_DESC| D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
            // PREFER_FAST_TRACE | ALLOW_COMPACTION.
            Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(6),
            NumDescs: 1,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { pGeometryDescs: g },
        };
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        device.GetRaytracingAccelerationStructurePrebuildInfo(&inputs(&geometries[0]), &mut info);
        assert!(info.ResultDataMaxSizeInBytes > 0);
        let state = D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE;
        let results: Vec<_> = (0..2)
            .map(|_| buffer(&device, info.ResultDataMaxSizeInBytes, state).unwrap())
            .collect();
        let scratch: Vec<_> = (0..2)
            .map(|_| buffer(&device, info.ScratchDataSizeInBytes, D3D12_RESOURCE_STATE_UNORDERED_ACCESS).unwrap())
            .collect();
        let sizes = buffer(&device, 256, D3D12_RESOURCE_STATE_UNORDERED_ACCESS).unwrap();
        let readback = {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut out)
                .unwrap();
            out.unwrap()
        };
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList4 = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        for k in 0..2 {
            let desc = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                DestAccelerationStructureData: results[k].GetGPUVirtualAddress(),
                Inputs: inputs(&geometries[k]),
                SourceAccelerationStructureData: 0,
                ScratchAccelerationStructureData: scratch[k].GetGPUVirtualAddress(),
            };
            let post = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC {
                DestBuffer: sizes.GetGPUVirtualAddress() + 8 * k as u64,
                InfoType: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE,
            };
            list.BuildRaytracingAccelerationStructure(&desc, Some(&[post]));
        }
        list.ResourceBarrier(&[
            uav_barrier(&sizes),
            crate::host::transition(
                &sizes,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            ),
        ]);
        list.CopyBufferRegion(&readback, 0, &sizes, 0, 16);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        fence.SetEventOnCompletion(1, event).unwrap();
        WaitForSingleObject(event, INFINITE);
        assert!(device.GetDeviceRemovedReason().is_ok(), "the device was removed");
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut data)).unwrap();
        let got = std::slice::from_raw_parts(data.cast::<u64>(), 2).to_vec();
        readback.Unmap(0, None);
        println!("packed blas on the card: compacted sizes r16 decoded {} r32 plain {}", got[0], got[1]);
        assert!(got[0] > 0);
        assert_eq!(got[0], got[1]);
    }
}
