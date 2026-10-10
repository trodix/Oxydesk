#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    pub id: OutputId,
    /// Possibly negative offsets in the virtual space.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Most outputs a layout may carry.
pub const MAX_OUTPUTS: usize = 16;

/// A monitor as announced by the client (Display Control): position in the client's space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorSpec {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub outputs: Vec<OutputInfo>,
}

impl Layout {
    /// `n` identical outputs laid out left to right, the first being primary.
    pub fn side_by_side(n: u32, width: u32, height: u32) -> Self {
        Self {
            outputs: (0..n)
                .map(|i| OutputInfo {
                    id: OutputId(i),
                    x: (i * width) as i32,
                    y: 0,
                    width,
                    height,
                    primary: i == 0,
                })
                .collect(),
        }
    }

    /// Layout from the client's monitors: at most `MAX_OUTPUTS`, even dimensions, translated so that
    /// the bounding rectangle starts at (0, 0) (RDP requirement). The primary monitor is output 0
    /// (the first one when none is flagged); the others keep their relative order (left to right,
    /// then top to bottom). `None` without any usable monitor.
    pub fn from_monitors(monitors: &[MonitorSpec]) -> Option<Self> {
        let mut m: Vec<MonitorSpec> = monitors
            .iter()
            .take(MAX_OUTPUTS)
            .map(|m| MonitorSpec {
                width: m.width & !1,
                height: m.height & !1,
                ..*m
            })
            .filter(|m| m.width > 0 && m.height > 0)
            .collect();
        if m.is_empty() {
            return None;
        }
        let primary = m.iter().position(|m| m.primary).unwrap_or(0);
        let first = m.remove(primary);
        m.sort_by_key(|m| (m.x, m.y));
        m.insert(
            0,
            MonitorSpec {
                primary: true,
                ..first
            },
        );
        let (ox, oy) = (
            m.iter().map(|m| m.x).min().unwrap_or(0),
            m.iter().map(|m| m.y).min().unwrap_or(0),
        );
        Some(Self {
            outputs: m
                .iter()
                .enumerate()
                .map(|(i, m)| OutputInfo {
                    id: OutputId(i as u32),
                    x: m.x - ox,
                    y: m.y - oy,
                    width: m.width,
                    height: m.height,
                    primary: i == 0,
                })
                .collect(),
        })
    }

    /// Bounding rectangle (width, height) from the origin (0, 0).
    pub fn desktop_size(&self) -> (u32, u32) {
        let right = self
            .outputs
            .iter()
            .map(|o| o.x + o.width as i32)
            .max()
            .unwrap_or(0);
        let bottom = self
            .outputs
            .iter()
            .map(|o| o.y + o.height as i32)
            .max()
            .unwrap_or(0);
        (right.max(0) as u32, bottom.max(0) as u32)
    }
}

/// Converts mouse coordinates from the RDP virtual space to an output.
pub struct CoordinateMapper<'a> {
    layout: &'a Layout,
}

impl<'a> CoordinateMapper<'a> {
    pub fn new(layout: &'a Layout) -> Self {
        Self { layout }
    }

    /// Returns the output containing the point and the local coordinates.
    pub fn locate(&self, x: i32, y: i32) -> Option<(OutputId, u32, u32)> {
        self.layout.outputs.iter().find_map(|o| {
            let inside =
                x >= o.x && y >= o.y && (x - o.x) < o.width as i32 && (y - o.y) < o.height as i32;
            inside.then(|| (o.id, (x - o.x) as u32, (y - o.y) as u32))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(id: u32, x: i32, w: u32) -> OutputInfo {
        OutputInfo {
            id: OutputId(id),
            x,
            y: 0,
            width: w,
            height: 100,
            primary: id == 0,
        }
    }

    #[test]
    fn side_by_side_layout_and_bounds() {
        let l = Layout::side_by_side(3, 1920, 1080);
        assert_eq!(l.desktop_size(), (5760, 1080));
        assert_eq!(l.outputs[2].x, 3840);
        assert!(l.outputs[0].primary && !l.outputs[1].primary);
        assert_eq!(
            CoordinateMapper::new(&l).locate(1920, 5),
            Some((OutputId(1), 0, 5))
        );
    }

    fn mon(x: i32, y: i32, w: u32, h: u32, primary: bool) -> MonitorSpec {
        MonitorSpec {
            x,
            y,
            width: w,
            height: h,
            primary,
        }
    }

    #[test]
    fn from_monitors_normalises_to_origin_and_even_sizes() {
        let l = Layout::from_monitors(&[
            mon(-1920, 100, 1921, 1081, false),
            mon(0, 0, 1280, 720, true),
        ])
        .unwrap();
        assert_eq!(l.outputs[0].id, OutputId(0));
        assert!(l.outputs[0].primary && !l.outputs[1].primary);
        // Primary first; the other monitor was left of it and 100 px lower.
        assert_eq!((l.outputs[0].x, l.outputs[0].y), (1920, 0));
        assert_eq!((l.outputs[1].x, l.outputs[1].y), (0, 100));
        assert_eq!((l.outputs[1].width, l.outputs[1].height), (1920, 1080));
        assert_eq!(l.desktop_size(), (3200, 1180));
    }

    #[test]
    fn from_monitors_limits_and_rejects_empty() {
        assert!(Layout::from_monitors(&[]).is_none());
        assert!(Layout::from_monitors(&[mon(0, 0, 0, 10, true)]).is_none());
        let many: Vec<_> = (0..20).map(|i| mon(i * 100, 0, 100, 100, i == 0)).collect();
        assert_eq!(
            Layout::from_monitors(&many).unwrap().outputs.len(),
            MAX_OUTPUTS
        );
        let stacked =
            Layout::from_monitors(&[mon(0, 0, 100, 100, true), mon(0, 100, 100, 100, false)])
                .unwrap();
        assert_eq!(stacked.desktop_size(), (100, 200));
    }

    #[test]
    fn locates_with_negative_offsets() {
        let layout = Layout {
            outputs: vec![out(1, -200, 200), out(0, 0, 300)],
        };
        let m = CoordinateMapper::new(&layout);
        assert_eq!(m.locate(-1, 5), Some((OutputId(1), 199, 5)));
        assert_eq!(m.locate(0, 5), Some((OutputId(0), 0, 5)));
        assert_eq!(m.locate(300, 5), None);
    }
}
