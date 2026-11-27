//! (bootfast) A graphics state as bytes, for the pipeline recipes
//! (`super::super::recipe`): what the title's stream gave, after its
//! formats were made the host's ([`GraphicsState::read`]), so the state
//! read back is the one the run made its pipeline with, and its
//! [`GraphicsState::describe`] (part of the pipeline's name) the same.

use std::ffi::CString;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::GraphicsState;

/// Bumped when the layout below changes: older recipes are then skipped.
const VERSION: u32 = 1;

fn pod<T: Copy>(out: &mut Vec<u8>, value: &T) {
    // SAFETY: `T` is a plain `repr(C)` D3D12 or DXGI description: its bytes
    // are its value.
    let bytes =
        unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    out.extend_from_slice(bytes);
}

fn u32le(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn pod<T: Copy>(&mut self) -> Option<T> {
        let bytes = self.take(size_of::<T>())?;
        // SAFETY: `size_of::<T>()` bytes written by [`pod`] for the same
        // plain description type (the sizes are checked first).
        Some(unsafe { bytes.as_ptr().cast::<T>().read_unaligned() })
    }
}

/// The sizes of the descriptions copied whole: a recipe written by a build
/// whose types differ is not read.
fn sizes() -> [u32; 5] {
    [
        size_of::<D3D12_BLEND_DESC>() as u32,
        size_of::<D3D12_RASTERIZER_DESC>() as u32,
        size_of::<D3D12_DEPTH_STENCIL_DESC>() as u32,
        size_of::<D3D12_RT_FORMAT_ARRAY>() as u32,
        size_of::<DXGI_SAMPLE_DESC>() as u32,
    ]
}

impl GraphicsState {
    /// The state as bytes ([`GraphicsState::decode`] reads them back).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        u32le(&mut out, VERSION);
        for s in sizes() {
            u32le(&mut out, s);
        }
        pod(&mut out, &self.blend);
        u32le(&mut out, self.sample_mask);
        pod(&mut out, &self.rasterizer);
        pod(&mut out, &self.depth);
        u32le(&mut out, u32::from(self.depth_bounds));
        u32le(&mut out, self.strip_cut.0 as u32);
        u32le(&mut out, self.topology.0 as u32);
        pod(&mut out, &self.targets);
        u32le(&mut out, self.depth_format.0 as u32);
        pod(&mut out, &self.samples);
        u32le(&mut out, self.inputs.len() as u32);
        for (e, name) in self.inputs.iter().zip(&self.names) {
            let name = name.as_bytes();
            u32le(&mut out, name.len() as u32);
            out.extend_from_slice(name);
            u32le(&mut out, e.SemanticIndex);
            u32le(&mut out, e.Format.0 as u32);
            u32le(&mut out, e.InputSlot);
            u32le(&mut out, e.AlignedByteOffset);
            u32le(&mut out, e.InputSlotClass.0 as u32);
            u32le(&mut out, e.InstanceDataStepRate);
        }
        out
    }

    /// The state [`GraphicsState::encode`] wrote (`None`: another layout,
    /// or cut short).
    pub(crate) fn decode(bytes: &[u8]) -> Option<GraphicsState> {
        let mut r = Reader(bytes);
        if r.u32()? != VERSION {
            return None;
        }
        for s in sizes() {
            if r.u32()? != s {
                return None;
            }
        }
        let mut g = GraphicsState {
            blend: r.pod()?,
            sample_mask: r.u32()?,
            rasterizer: r.pod()?,
            depth: r.pod()?,
            depth_bounds: r.u32()? != 0,
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(r.u32()? as i32),
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE(r.u32()? as i32),
            targets: r.pod()?,
            depth_format: DXGI_FORMAT(r.u32()? as i32),
            samples: r.pod()?,
            ..GraphicsState::default()
        };
        let count = r.u32()? as usize;
        if count > 64 {
            return None;
        }
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            let len = r.u32()? as usize;
            let name = CString::new(r.take(len)?).ok()?;
            let e = (r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u32()?, r.u32()?);
            fields.push((name, e));
        }
        if !r.0.is_empty() {
            return None;
        }
        g.names = fields.iter().map(|(n, _)| n.clone()).collect();
        g.inputs = fields
            .iter()
            .zip(&g.names)
            .map(|((_, e), name)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                SemanticIndex: e.0,
                Format: DXGI_FORMAT(e.1 as i32),
                InputSlot: e.2,
                AlignedByteOffset: e.3,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION(e.4 as i32),
                InstanceDataStepRate: e.5,
            })
            .collect();
        Some(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphicsState {
        let mut g = GraphicsState::default();
        g.blend.RenderTarget[1].BlendEnable = true.into();
        g.blend.RenderTarget[1].SrcBlend = D3D12_BLEND_SRC_ALPHA;
        g.sample_mask = 0xf0f0;
        g.rasterizer.CullMode = D3D12_CULL_MODE_NONE;
        g.rasterizer.DepthBias = -7;
        g.depth.StencilEnable = true.into();
        g.depth_bounds = true;
        g.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        g.targets.NumRenderTargets = 2;
        g.targets.RTFormats[0] = DXGI_FORMAT_R16G16B16A16_FLOAT;
        g.targets.RTFormats[1] = DXGI_FORMAT_R8G8B8A8_UNORM;
        g.depth_format = DXGI_FORMAT_D32_FLOAT;
        g.names = vec![
            CString::new("POSITION").unwrap(),
            CString::new("TEXCOORD").unwrap(),
        ];
        g.inputs = vec![
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(g.names[0].as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(g.names[1].as_ptr().cast()),
                SemanticIndex: 3,
                Format: DXGI_FORMAT_R16G16_FLOAT,
                InputSlot: 2,
                AlignedByteOffset: D3D12_APPEND_ALIGNED_ELEMENT,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                InstanceDataStepRate: 1,
            },
        ];
        g
    }

    /// Read back, a state describes itself as before: the pipeline made
    /// from it gets the same name in the library.
    #[test]
    fn a_state_read_back_describes_itself_the_same() {
        let g = sample();
        let bytes = g.encode();
        let back = GraphicsState::decode(&bytes).expect("read back");
        assert_eq!(back.describe(), g.describe());
        assert_eq!(back.encode(), bytes);
        let plain = GraphicsState::default();
        assert_eq!(
            GraphicsState::decode(&plain.encode()).unwrap().describe(),
            plain.describe()
        );
    }

    #[test]
    fn a_cut_or_foreign_state_is_not_read() {
        let bytes = sample().encode();
        assert!(GraphicsState::decode(&bytes[..bytes.len() - 1]).is_none());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(GraphicsState::decode(&longer).is_none());
        let mut other = bytes;
        other[0] = 9;
        assert!(GraphicsState::decode(&other).is_none());
        assert!(GraphicsState::decode(&[]).is_none());
    }
}
