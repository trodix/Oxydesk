//! Minimal H.264 SPS / PPS / slice header writer (CAVLC, progressive, one reference frame, POC type 2).
//!
//! Hardware encoders (VA-API on AMD) expect the application to supply these headers as "packed
//! headers" and only produce the slice data.
#![cfg_attr(not(feature = "vaapi"), allow(dead_code))]

struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self { bytes: Vec::new(), cur: 0, nbits: 0 }
    }

    fn bit(&mut self, b: bool) {
        self.cur = (self.cur << 1) | u8::from(b);
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    fn bits(&mut self, value: u32, n: u8) {
        for i in (0..n).rev() {
            self.bit((value >> i) & 1 == 1);
        }
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros() as u8;
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
    }

    fn se(&mut self, v: i32) {
        self.ue(if v > 0 { (2 * v - 1) as u32 } else { (-2 * v) as u32 });
    }

    /// Number of bits written so far.
    fn len_bits(&self) -> usize {
        self.bytes.len() * 8 + usize::from(self.nbits)
    }

    /// `rbsp_trailing_bits()`: a 1 then zeros up to the byte boundary.
    fn trailing(&mut self) {
        self.bit(true);
        while self.nbits != 0 {
            self.bit(false);
        }
    }

    /// Bytes written, the last partial byte zero-padded.
    fn into_bytes(mut self) -> Vec<u8> {
        if self.nbits != 0 {
            self.cur <<= 8 - self.nbits;
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

/// Inserts emulation prevention bytes (`00 00 0x` with x <= 3 becomes `00 00 03 0x`).
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

fn nal(ref_idc: u8, nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = START_CODE.to_vec();
    out.push((ref_idc << 5) | nal_type);
    out.extend(escape(rbsp));
    out
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamParams {
    /// 66 (constrained baseline), 77 (main) or 100 (high).
    pub profile_idc: u8,
    pub level_idc: u8,
    pub mb_width: u32,
    pub mb_height: u32,
    /// Cropping in luma samples (even numbers), right and bottom.
    pub crop_right: u32,
    pub crop_bottom: u32,
    pub log2_max_frame_num: u32,
}

/// Annex-B SPS followed by PPS (start codes included, emulation prevention applied).
pub(crate) fn sps_pps(p: &StreamParams) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(u32::from(p.profile_idc), 8);
    // constraint_set1_flag for constrained baseline; reserved_zero_2bits.
    w.bits(if p.profile_idc == 66 { 0x40 } else { 0 }, 8);
    w.bits(u32::from(p.level_idc), 8);
    w.ue(0); // seq_parameter_set_id
    if p.profile_idc >= 100 {
        w.ue(1); // chroma_format_idc 4:2:0
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.bit(false); // qpprime_y_zero_transform_bypass_flag
        w.bit(false); // seq_scaling_matrix_present_flag
    }
    w.ue(p.log2_max_frame_num - 4);
    w.ue(2); // pic_order_cnt_type
    w.ue(1); // max_num_ref_frames
    w.bit(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(p.mb_width - 1);
    w.ue(p.mb_height - 1);
    w.bit(true); // frame_mbs_only_flag
    w.bit(true); // direct_8x8_inference_flag
    let crop = p.crop_right != 0 || p.crop_bottom != 0;
    w.bit(crop);
    if crop {
        w.ue(0);
        w.ue(p.crop_right / 2);
        w.ue(0);
        w.ue(p.crop_bottom / 2);
    }
    w.bit(false); // vui_parameters_present_flag
    w.trailing();
    let mut out = nal(3, 7, &w.into_bytes());

    let mut w = BitWriter::new();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.bit(false); // entropy_coding_mode_flag: CAVLC
    w.bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.bit(false); // weighted_pred_flag
    w.bits(0, 2); // weighted_bipred_idc
    w.se(0); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.bit(true); // deblocking_filter_control_present_flag
    w.bit(false); // constrained_intra_pred_flag
    w.bit(false); // redundant_pic_cnt_present_flag
    w.trailing();
    out.extend(nal(3, 8, &w.into_bytes()));
    out
}

/// Annex-B slice header of a single-slice frame: returns the bytes (start code and NAL header
/// included, last byte zero-padded) and the exact length in bits.
pub(crate) fn slice_header(p: &StreamParams, idr: bool, frame_num: u32, idr_pic_id: u32) -> (Vec<u8>, usize) {
    let mut w = BitWriter::new();
    w.ue(0); // first_mb_in_slice
    w.ue(if idr { 7 } else { 5 }); // slice_type: I / P, all slices of the picture
    w.ue(0); // pic_parameter_set_id
    w.bits(frame_num, p.log2_max_frame_num as u8);
    if idr {
        w.ue(idr_pic_id);
    }
    if !idr {
        w.bit(false); // num_ref_idx_active_override_flag
        w.bit(false); // ref_pic_list_modification_flag_l0
    }
    // dec_ref_pic_marking (nal_ref_idc != 0)
    if idr {
        w.bit(false); // no_output_of_prior_pics_flag
        w.bit(false); // long_term_reference_flag
    } else {
        w.bit(false); // adaptive_ref_pic_marking_mode_flag
    }
    w.se(0); // slice_qp_delta
    w.ue(0); // disable_deblocking_filter_idc
    w.se(0); // slice_alpha_c0_offset_div2
    w.se(0); // slice_beta_offset_div2
    let header_bits = w.len_bits();
    let rbsp = w.into_bytes();

    let mut out = START_CODE.to_vec();
    out.push(if idr { (3 << 5) | 5 } else { (2 << 5) | 1 });
    let escaped = escape(&rbsp);
    // Bits = prefix + escaped payload, minus the zero padding of the last byte.
    let pad = rbsp.len() * 8 - header_bits;
    let bits = (out.len() + escaped.len()) * 8 - pad;
    out.extend(escaped);
    (out, bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes() {
        let mut w = BitWriter::new();
        w.ue(0); // 1
        w.ue(1); // 010
        w.ue(2); // 011
        w.ue(3); // 00100
        w.se(-1); // ue(2) = 011
        w.trailing();
        // 1 010 011 00100 011 + trailing 1 -> 1010 0110 0100 0111 = a6 47
        assert_eq!(w.into_bytes(), vec![0b1010_0110, 0b0100_0111]);
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(escape(&[0, 0, 1, 0, 0, 0, 5, 0, 0, 4]), vec![0, 0, 3, 1, 0, 0, 3, 0, 5, 0, 0, 4]);
    }

    #[test]
    fn sps_has_the_expected_shape() {
        let p = StreamParams { profile_idc: 66, level_idc: 42, mb_width: 120, mb_height: 68, crop_right: 0, crop_bottom: 8, log2_max_frame_num: 8 };
        let b = sps_pps(&p);
        assert_eq!(&b[..5], &[0, 0, 0, 1, 0x67]);
        assert_eq!(&b[5..8], &[66, 0x40, 42]);
        assert!(b.windows(5).any(|w| w == [0, 0, 0, 1, 0x68]));
    }

    #[test]
    fn slice_header_bit_length_matches() {
        let p = StreamParams { profile_idc: 66, level_idc: 42, mb_width: 120, mb_height: 68, crop_right: 0, crop_bottom: 8, log2_max_frame_num: 8 };
        let (bytes, bits) = slice_header(&p, true, 0, 1);
        assert_eq!(bytes[4], 0x65);
        assert!(bits > 40 && bits <= bytes.len() * 8 && bits > (bytes.len() - 1) * 8 - 8);
        let (bytes, _) = slice_header(&p, false, 5, 0);
        assert_eq!(bytes[4], 0x41);
    }
}
