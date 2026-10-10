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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    pub outputs: Vec<OutputInfo>,
}

impl Layout {
    /// `n` identical outputs laid out left to right, the first being primary.
    pub fn side_by_side(n: u32, width: u32, height: u32) -> Self {
        Self {
            outputs: (0..n)
                .map(|i| OutputInfo { id: OutputId(i), x: (i * width) as i32, y: 0, width, height, primary: i == 0 })
                .collect(),
        }
    }

    /// Bounding rectangle (width, height) from the origin (0, 0).
    pub fn desktop_size(&self) -> (u32, u32) {
        let right = self.outputs.iter().map(|o| o.x + o.width as i32).max().unwrap_or(0);
        let bottom = self.outputs.iter().map(|o| o.y + o.height as i32).max().unwrap_or(0);
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
        assert_eq!(CoordinateMapper::new(&l).locate(1920, 5), Some((OutputId(1), 0, 5)));
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
