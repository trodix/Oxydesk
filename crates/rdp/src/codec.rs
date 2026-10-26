//! How the desktop is sent to the client.

use std::str::FromStr;

/// Picture codec of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// Bitmap updates encoded by IronRDP (RemoteFX, or the plain bitmap): every pixel keeps its own colour, at the
    /// price of a higher bitrate. No EGFX channel is offered.
    Rfx,
    /// H.264 over EGFX (4:2:0: thin coloured text loses its colour and sharpness), with a bitrate regulated from the
    /// client's acknowledgements.
    H264,
    /// RemoteFX Progressive over EGFX: tiles with no chroma subsampling, sent coarse first then refined to the exact
    /// picture while the desktop stays still. The default.
    #[default]
    Rfxp,
}

impl FromStr for Codec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "rfx" => Ok(Self::Rfx),
            "h264" => Ok(Self::H264),
            "rfxp" => Ok(Self::Rfxp),
            other => Err(format!("unknown codec: {other} (rfx|h264|rfxp)")),
        }
    }
}

impl Codec {
    /// The codec named by `OXYDESK_CODEC`; the default one when it is unset or not understood.
    pub fn from_env() -> Self {
        Self::from_setting(std::env::var("OXYDESK_CODEC").ok().as_deref())
    }

    /// Whether the desktop agent should send uncompressed pixels from the start of a connection: the bitmap and
    /// Progressive paths encode them here, so an H.264 stream from the agent would only be thrown away.
    pub fn wants_raw_pixels(self) -> bool {
        self != Self::H264
    }

    fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::parse) {
            None => Self::default(),
            Some(Ok(codec)) => codec,
            Some(Err(e)) => {
                tracing::warn!("OXYDESK_CODEC ignored: {e}");
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_named_by_the_setting() {
        assert_eq!(Codec::from_setting(None), Codec::Rfxp);
        assert_eq!(Codec::from_setting(Some("rfx")), Codec::Rfx);
        assert_eq!(Codec::from_setting(Some("h264")), Codec::H264);
        assert_eq!(Codec::from_setting(Some("rfxp")), Codec::Rfxp);
        assert!(Codec::Rfx.wants_raw_pixels() && Codec::Rfxp.wants_raw_pixels());
        assert!(!Codec::H264.wants_raw_pixels());
    }

    #[test]
    fn an_unknown_name_keeps_the_default() {
        assert_eq!(Codec::from_setting(Some("av1")), Codec::Rfxp);
        assert_eq!(Codec::from_setting(Some("")), Codec::Rfxp);
    }
}
