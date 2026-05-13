//! DECODE_ERROR message (msg_type = 0x04): client → server per-tile
//! decode-failure report on the reliable bidi FEEDBACK stream.
//!
//! Wire format (5 bytes):
//!   [0]  msg_type     = 0x04
//!   [1]  codec        : u8     Codec enum discriminant (PalRle = 2 mostly)
//!   [2]  tile_x       : u8
//!   [3]  tile_y       : u8
//!   [4]  error_code   : u8     1..=7 per the taxonomy below
//!
//! Error code taxonomy:
//!   1 = payload too short (CPU pre-validation)
//!   2 = count out of range (not 1..16) (CPU pre-validation)
//!   3 = thin payload references uncached palette_id (CPU pre-validation)
//!   4 = bundled payload truncated mid-palette (CPU pre-validation)
//!   5 = palette index ≥ count (GPU shader)
//!   6 = RLE expansion overshot 1024 pixels (GPU shader)
//!   7 = RLE expansion undershot 1024 pixels (GPU shader)

pub const DECODE_ERROR_MSG_TYPE: u8 = 0x04;
pub const DECODE_ERROR_SIZE: usize = 5;

pub const ERR_PAYLOAD_TOO_SHORT: u8 = 1;
pub const ERR_COUNT_OUT_OF_RANGE: u8 = 2;
pub const ERR_THIN_UNCACHED_PALETTE: u8 = 3;
pub const ERR_BUNDLED_TRUNCATED: u8 = 4;
pub const ERR_INDEX_OOB: u8 = 5;
pub const ERR_RLE_OVERSHOOT: u8 = 6;
pub const ERR_RLE_UNDERSHOOT: u8 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeErrorMsg {
    pub codec: u8,
    pub tile_x: u8,
    pub tile_y: u8,
    pub error_code: u8,
}

impl DecodeErrorMsg {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < DECODE_ERROR_SIZE {
            return None;
        }
        if data[0] != DECODE_ERROR_MSG_TYPE {
            return None;
        }
        Some(Self {
            codec: data[1],
            tile_x: data[2],
            tile_y: data[3],
            error_code: data[4],
        })
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(DECODE_ERROR_MSG_TYPE);
        buf.push(self.codec);
        buf.push(self.tile_x);
        buf.push(self.tile_y);
        buf.push(self.error_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_basic() {
        let m = DecodeErrorMsg::decode(&[DECODE_ERROR_MSG_TYPE, 2, 7, 13, ERR_THIN_UNCACHED_PALETTE]).unwrap();
        assert_eq!(m.codec, 2);
        assert_eq!(m.tile_x, 7);
        assert_eq!(m.tile_y, 13);
        assert_eq!(m.error_code, ERR_THIN_UNCACHED_PALETTE);
    }

    #[test]
    fn decode_too_short() {
        assert!(DecodeErrorMsg::decode(&[DECODE_ERROR_MSG_TYPE, 2, 7, 13]).is_none());
        assert!(DecodeErrorMsg::decode(&[]).is_none());
    }

    #[test]
    fn decode_wrong_msg_type() {
        assert!(DecodeErrorMsg::decode(&[0x05, 2, 7, 13, 3]).is_none());
    }

    #[test]
    fn roundtrip() {
        let m = DecodeErrorMsg { codec: 2, tile_x: 30, tile_y: 60, error_code: ERR_INDEX_OOB };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(buf, vec![DECODE_ERROR_MSG_TYPE, 2, 30, 60, ERR_INDEX_OOB]);
        assert_eq!(DecodeErrorMsg::decode(&buf), Some(m));
    }
}
