//! H.264 Annex-B bitstream parsing helpers.

/// Detect whether an H.264 Annex-B bitstream contains a keyframe NAL unit.
///
/// Looks for NAL type 5 (IDR slice) or NAL type 7 (SPS, which always
/// precedes an IDR in the same AU).
pub fn is_keyframe_nal(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 <= data.len() {
        // Annex-B start code: 0x00 0x00 0x00 0x01 or 0x00 0x00 0x01
        if data[i] == 0 && data[i + 1] == 0 {
            let nal_byte_offset = if data[i + 2] == 0 && i + 4 < data.len() && data[i + 3] == 1 {
                i + 4
            } else if data[i + 2] == 1 {
                i + 3
            } else {
                i += 1;
                continue;
            };
            if nal_byte_offset < data.len() {
                let nal_type = data[nal_byte_offset] & 0x1F;
                if nal_type == 5 || nal_type == 7 {
                    return true;
                }
            }
            i = nal_byte_offset;
        } else {
            i += 1;
        }
    }
    false
}
