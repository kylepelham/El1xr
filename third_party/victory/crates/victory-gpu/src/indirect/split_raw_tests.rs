//! `VICTORY_SPLIT_RAW` (El1xr): a dispatch of a program with a group base
//! run from the title's own record, with no clamp and no parts, runs the
//! groups the clamped parts run, with the same group numbers.

use super::*;

/// Each group adds one to its execution's first word and keeps the largest
/// group number (the base added, as a translated program does) in the next.
const COUNTING: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), UAV(u0)"
cbuffer Execution : register(b0) { uint execution; uint base; };
RWByteAddressBuffer groups : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 group : SV_GroupID, uint index : SV_GroupIndex) {
    if (index != 0) return;
    uint before;
    groups.InterlockedAdd(execution * 8, 1, before);
    groups.InterlockedMax(execution * 8 + 4, group.x + base, before);
}
"#;

fn dispatch_signature(device: &ID3D12Device) -> ID3D12CommandSignature {
    let argument = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: 12,
        NumArgumentDescs: 1,
        pArgumentDescs: &argument,
        NodeMask: 0,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: one argument, no root arguments.
    unsafe { device.CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut signature) }
        .unwrap();
    signature.unwrap()
}

/// Groups in x of the records, and what each runs: (groups, largest group
/// number) clamped in parts, then raw.
fn run(xs: &[u32]) -> Option<Vec<([u32; 2], [u32; 2])>> {
    let host = host()?;
    let counting = make_from(&host.device, COUNTING, s!("count")).unwrap();
    let signature = dispatch_signature(&host.device);
    let (title, data) = host.shared_buffer(xs.len() as u64 * 12).unwrap();
    let (counts, counted) = host.shared_buffer(xs.len() as u64 * 16).unwrap();
    let list = host.new_list().unwrap();
    // SAFETY: our mapped buffers, written before the list runs and read
    // after it has; an open list, our pipelines and live buffers.
    unsafe {
        std::ptr::write_bytes(counted, 0, xs.len() * 16);
        for (i, &x) in xs.iter().enumerate() {
            data.cast::<[u32; 3]>().add(i).write_volatile([x, 1, 1]);
        }
        for (i, _) in xs.iter().enumerate() {
            let at = i as u64 * 12;
            let parts = bound_split(&list, (&title, at)).unwrap();
            let l = &list.list;
            l.SetComputeRootSignature(&counting.root);
            l.SetPipelineState(&counting.pso);
            l.SetComputeRootUnorderedAccessView(1, counts.GetGPUVirtualAddress());
            l.SetComputeRoot32BitConstant(0, 2 * i as u32, 0);
            execute_parts(l, (0, 1), &signature, (&parts, 0));
            l.ResourceBarrier(&[uav_barrier()]);
            l.SetComputeRoot32BitConstants(0, 2, [2 * i as u32 + 1, 0].as_ptr().cast(), 0);
            l.ExecuteIndirect(&signature, 1, &title, at, None, 0);
            l.ResourceBarrier(&[uav_barrier()]);
        }
        list.close();
        host.execute(&[&list]);
        let words = counted.cast::<u32>();
        let got = (0..xs.len())
            .map(|i| {
                let w = |k: usize| words.add(i * 4 + k).read_volatile();
                ([w(0), w(1)], [w(2), w(3)])
            })
            .collect();
        list.reset();
        Some(got)
    }
}

/// Up to the most a dispatch runs in a dimension, the raw record runs what
/// the clamped parts run: the same groups, the same largest number.
#[test]
fn a_raw_record_runs_what_its_clamped_parts_run() {
    let xs = [1, 2, 63, 64, 1000, 43_584, 65_535];
    let Some(got) = run(&xs) else {
        return; // no host GPU here
    };
    for (&x, (parts, raw)) in xs.iter().zip(got) {
        assert_eq!(parts, [x, x - 1], "x {x}: clamped in parts");
        assert_eq!(raw, parts, "x {x}: raw");
    }
}

/// Past 65,535 groups in x, what the host card does with the record raw
/// (`VICTORY_SPLIT_RAW=1` keeps these right only on a card that runs them
/// all). Asserted only with `VICTORY_TEST_SPLIT_RAW_WIDE=1`; otherwise printed.
#[test]
fn a_raw_record_past_one_dimension() {
    let xs = [65_536, 81_064, 92_288, 115_200, 131_070];
    let Some(got) = run(&xs) else {
        return; // no host GPU here
    };
    let strict = std::env::var_os("VICTORY_TEST_SPLIT_RAW_WIDE").is_some_and(|v| v == "1");
    eprintln!("adapter: {:?}", crate::host::adapter_summary().map(|a| a.0));
    for (&x, (parts, raw)) in xs.iter().zip(got) {
        eprintln!("x {x}: parts ran {parts:?}, raw ran {raw:?}");
        assert_eq!(parts, [x, x - 1], "x {x}: clamped in parts");
        if strict {
            assert_eq!(raw, parts, "x {x}: raw");
        }
    }
}
