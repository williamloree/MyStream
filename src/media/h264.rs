//! Minimal Annex-B helpers.

/// Iterates NAL unit types found in an Annex-B byte stream.
pub fn nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                i += 3;
                return data.get(i).map(|b| b & 0x1f);
            }
            i += 1;
        }
        None
    })
}

pub fn contains_sps(data: &[u8]) -> bool {
    nal_types(data).any(|t| t == 7)
}

pub fn is_idr(data: &[u8]) -> bool {
    nal_types(data).any(|t| t == 5)
}
