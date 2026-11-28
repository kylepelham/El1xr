//! Two geometry forms the title's own builder reads that DXR has no format
//! for, decoded on the CPU from title memory into plain 16-bit indices and
//! R32G32B32 positions before a host build.
//!
//! The title ships a build of `XboxBvhBuilder.dll` of its own (its shaders'
//! debug names are under `rage\3rdparty\Microsoft\XboxBvhBuilder`). Its
//! `LoadTriangles` and `ReloadTriangles` shaders (DXIL in the DLL), and the
//! C++ that feeds them, give these two forms:
//!
//! - Index format 53 (`R16_TYPELESS`; flag bit 18 beside bit 16 for
//!   `R16_UINT` and 17 for `R32_UINT`): meshlet indices. At the index
//!   buffer: a u32 whose low 17 bits count the meshlets, a u32 counting the
//!   triangle records, 4 bytes more, then 16 bytes per meshlet, then a u32
//!   per triangle, then a u16 table. A triangle's u32 holds its meshlet
//!   (bits 21-31) and three 7-bit offsets (bits 0-6, 7-13, 14-20) into the
//!   table from its meshlet's first entry: the low 16 bits of the meshlet's
//!   second u32, with bit 14 of its first as bit 16. The table holds the
//!   vertex indices.
//! - Vertex format 16 (`R32G32_FLOAT`), the only position format the builder
//!   gives code 0: a quantized position in 8 bytes, with a 128-byte header
//!   before the vertex buffer (the builder binds `vertex buffer - 0x80`).
//!   X is the high 24 bits of the first u32; Y its low 8 bits under the low
//!   16 bits of the second; Z the high 16 bits of the second. A position is
//!   `offset + scale * q` (offset at header byte 96, scale at 112, three
//!   floats each); the u32 at 108 then places the axes: 0 gives (z, y, x),
//!   1 gives (y, z, x), others (x, y, z).
//!
//! Bit 15 of the stride is not stride: the builder masks it off, and with
//! meshlet indices it swaps each triangle's first and third index.

/// `DXGI_FORMAT_R16_TYPELESS`: meshlet indices.
pub(crate) const MESHLET_INDICES: u32 = 53;
/// `DXGI_FORMAT_R32G32_FLOAT`: quantized positions.
pub(crate) const QUANTIZED_POSITIONS: u32 = 16;

/// What DXR is given instead: `R16_UINT` indices, `R32G32B32_FLOAT`
/// positions 12 bytes apart.
pub(crate) const R16_UINT: u32 = 57;
pub(crate) const R32G32B32_FLOAT: u32 = 6;
pub(crate) const POSITION_STRIDE: u64 = 12;

const STRIDE_BITS: u64 = 0x7fff;
const FLIP_BIT: u64 = 0x8000;

/// The header before quantized positions, and where its parts are.
const HEADER: u64 = 0x80;
const OFFSET_AT: u64 = 96;
const SCALE_AT: u64 = 112;

/// The meshlet form's fixed parts.
const MESHLETS_AT: u64 = 12;
const MESHLET_SIZE: u64 = 16;
const MESHLET_BITS: u32 = 0x1_ffff;

/// No decode reads more than this from title memory.
const MOST_BYTES: u64 = 64 << 20;

/// The stride the builder uses (bit 15 off).
pub(crate) fn stride(stride: u64) -> u64 {
    stride & STRIDE_BITS
}

/// Whether the builder swaps each meshlet triangle's first and third index.
pub(crate) fn flips(stride: u64) -> bool {
    stride & FLIP_BIT != 0
}

/// Why a decode was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// A page of it is not readable.
    Unreadable,
    /// A header that cannot be right (counts, sizes).
    Header,
    /// A triangle names a meshlet past the count.
    Meshlet,
    /// An index at or past the vertex count.
    Index,
    /// A position that is not a finite number.
    Position,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or_default())
}

fn f32_at(bytes: &[u8], at: usize) -> f32 {
    f32::from_bits(u32_at(bytes, at))
}

fn read_vec(
    read: &mut impl FnMut(u64, &mut [u8]) -> bool,
    at: u64,
    len: u64,
) -> Result<Vec<u8>, Refused> {
    if len > MOST_BYTES {
        return Err(Refused::Header);
    }
    let mut out = vec![0u8; len as usize];
    if len != 0 && !read(at, &mut out) {
        return Err(Refused::Unreadable);
    }
    Ok(out)
}

/// The vertex indices of `triangles` meshlet triangles at title address
/// `index_buffer`, each below `vertex_count`, read with `read` (title
/// address, bytes; false if unreadable). `flip`: [`flips`].
pub(crate) fn indices(
    index_buffer: u64,
    triangles: u32,
    vertex_count: u32,
    flip: bool,
    read: &mut impl FnMut(u64, &mut [u8]) -> bool,
) -> Result<Vec<u16>, Refused> {
    // The builder reads from the address with its low two bits off.
    let base = index_buffer & !3;
    let head = read_vec(read, base, 8)?;
    let meshlets = u32_at(&head, 0) & MESHLET_BITS;
    let records = u32_at(&head, 4);
    if triangles > records {
        return Err(Refused::Header);
    }
    let records_at = MESHLETS_AT + MESHLET_SIZE * u64::from(meshlets);
    let table_at = records_at + 4 * u64::from(records);
    let meshlet_bytes = read_vec(read, base + MESHLETS_AT, MESHLET_SIZE * u64::from(meshlets))?;
    let record_bytes = read_vec(read, base + records_at, 4 * u64::from(triangles))?;
    // Each triangle's three table entries, then the table up to the last.
    let mut entries = Vec::with_capacity(3 * triangles as usize);
    for t in 0..triangles as usize {
        let rec = u32_at(&record_bytes, 4 * t);
        let m = rec >> 21;
        if m >= meshlets {
            return Err(Refused::Meshlet);
        }
        let at = (MESHLET_SIZE * u64::from(m)) as usize;
        let (a, b) = (u32_at(&meshlet_bytes, at), u32_at(&meshlet_bytes, at + 4));
        let first = (b & 0xffff) | ((a << 2) & 0x1_0000);
        let local = [rec & 0x7f, (rec >> 7) & 0x7f, (rec >> 14) & 0x7f];
        let k = if flip { [2, 1, 0] } else { [0, 1, 2] };
        entries.extend(k.map(|k| first + local[k]));
    }
    let last = entries
        .iter()
        .copied()
        .max()
        .map_or(0, |e| u64::from(e) + 1);
    let table = read_vec(read, base + table_at, 2 * last)?;
    entries
        .into_iter()
        .map(|e| {
            let at = 2 * e as usize;
            let v = u16::from_le_bytes([table[at], table[at + 1]]);
            if u32::from(v) < vertex_count {
                Ok(v)
            } else {
                Err(Refused::Index)
            }
        })
        .collect()
}

/// `count` quantized positions `stride` bytes apart ([`stride`]) at title
/// address `vertex_buffer`, with the header before them, read with `read`.
pub(crate) fn positions(
    vertex_buffer: u64,
    count: u32,
    stride: u64,
    read: &mut impl FnMut(u64, &mut [u8]) -> bool,
) -> Result<Vec<[f32; 3]>, Refused> {
    if stride < 8 || vertex_buffer < HEADER {
        return Err(Refused::Header);
    }
    let head = read_vec(
        read,
        vertex_buffer - HEADER + OFFSET_AT,
        SCALE_AT + 12 - OFFSET_AT,
    )?;
    let offset = [f32_at(&head, 0), f32_at(&head, 4), f32_at(&head, 8)];
    let axis = u32_at(&head, 12);
    let at = (SCALE_AT - OFFSET_AT) as usize;
    let scale = [
        f32_at(&head, at),
        f32_at(&head, at + 4),
        f32_at(&head, at + 8),
    ];
    let len = if count == 0 {
        0
    } else {
        stride * u64::from(count - 1) + 8
    };
    let data = read_vec(read, vertex_buffer, len)?;
    (0..count as usize)
        .map(|i| {
            let at = i * stride as usize;
            let (d0, d1) = (u32_at(&data, at), u32_at(&data, at + 4));
            let q = [d0 >> 8, ((d1 & 0xffff) << 8) | (d0 & 0xff), d1 >> 16];
            // As the shader: a multiply, then an add (not fused).
            let p: [f32; 3] = std::array::from_fn(|c| offset[c] + scale[c] * q[c] as f32);
            let out = match axis {
                0 => [p[2], p[1], p[0]],
                1 => [p[1], p[2], p[0]],
                _ => p,
            };
            if out.iter().all(|c| c.is_finite()) {
                Ok(out)
            } else {
                Err(Refused::Position)
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "packed_tests.rs"]
pub(super) mod tests;
