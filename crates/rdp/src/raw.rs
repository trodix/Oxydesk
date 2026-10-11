//! Uncompressed pixels for the clients that cannot decode H.264: EGFX without AVC (`AVC_DISABLED`) and
//! clients without EGFX at all (legacy bitmap updates). Last resort, far heavier than H.264.

pub(crate) use rdsktp_core::pixels::to_bgra;
