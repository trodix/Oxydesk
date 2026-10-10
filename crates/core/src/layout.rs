#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    pub id: OutputId,
    /// Offsets éventuellement négatifs dans l'espace virtuel.
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

/// Convertit des coordonnées souris de l'espace virtuel RDP vers une sortie.
pub struct CoordinateMapper<'a> {
    layout: &'a Layout,
}

impl<'a> CoordinateMapper<'a> {
    pub fn new(layout: &'a Layout) -> Self {
        Self { layout }
    }

    /// Retourne la sortie contenant le point et les coordonnées locales.
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
