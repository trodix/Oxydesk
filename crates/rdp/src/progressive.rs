//! RemoteFX Progressive encoder ([MS-RDPEGFX] 2.2.4.2, [MS-RDPRFX]).
//!
//! The desktop is cut into 64x64 tiles. A tile that changed is sent at the quality of the classic RemoteFX (`TILE_FIRST`);
//! once it stays unchanged, upgrade passes (`TILE_UPGRADE`) refine it until it is exact. A still desktop therefore ends
//! with no quantization grain, and the client never receives the same tile twice.
//!
//! Everything here is a pure function of the pixels: no socket, no EGFX channel. The wire format follows the reference
//! decoder of FreeRDP (`libfreerdp/codec/progressive.c`), which is what the tests mirror:
//! - the reduce-extrapolate DWT (region flag `0x01`), the only layout the upgrade passes of FreeRDP and Windows know;
//! - one SRL and one raw bit stream per component for a whole upgrade pass, shared by all the sub-bands;
//! - the quantization of the first pass is `(base - 1) + progressive` bits per sub-band; a coefficient is truncated
//!   toward zero, except in `LL3` where it is floored, so that the refinement of `LL3` is a positive raw value.

use anyhow::{anyhow, ensure, Result};
use ironrdp_graphics::color_conversion::to_64x64_ycbcr_tile;
use ironrdp_graphics::dwt_extrapolate::{self, BandInfo};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::{rlgr, subband_reconstruction};
use ironrdp_pdu::codecs::rfx::progressive::{
    encode_progressive_stream, ComponentCodecQuant, ProgressiveBlock, ProgressiveCodecQuant,
    ProgressiveContextPdu, ProgressiveFrameBeginPdu, ProgressiveFrameEndPdu, ProgressiveRegion,
    ProgressiveSyncPdu, ProgressiveTile, TileFirst, TileUpgrade,
};
use ironrdp_pdu::codecs::rfx::{EntropyAlgorithm, RfxRectangle};

const TILE: usize = 64;
const COEFFS: usize = TILE * TILE;
const BANDS: usize = 10;
/// Index of `LL3` in the sub-band order `HL1, LH1, HH1, HL2, LH2, HH2, HL3, LH3, HH3, LL3`.
const LL3: usize = 9;

/// Base quantization of every sub-band: a shift of `6 - 1` bits, which drops the 5 fractional bits of the 11.5 fixed
/// point coefficients and keeps whole pixel values. Decoders refuse a smaller value (FreeRDP: `cQuant < 6`), so the
/// last level cannot be finer than that.
const BASE_QUANT: u8 = 6;

/// Extra bits dropped per sub-band at each quality level. The first level is the quantization of the classic
/// RemoteFX (`Quant::default()`), the last one drops nothing more than the base.
const LEVELS: [[u8; BANDS]; 3] = [
    [2, 2, 3, 1, 1, 2, 0, 0, 0, 0],
    [1, 1, 1, 0, 0, 1, 0, 0, 0, 0],
    [0; BANDS],
];

/// Output room of one entropy-coded component.
const COMPONENT_BYTES: usize = 16 * 1024;

/// Tiles refined per call: the work is spread over the calls instead of delaying the changed tiles.
const UPGRADES_PER_CALL: usize = 24;

type Plane = [i16; COEFFS];

/// Bits dropped from a sub-band at a quality level.
fn shift(level: usize, band: usize) -> u32 {
    u32::from(BASE_QUANT - 1) + u32::from(LEVELS[level][band])
}

fn component_quant(level: usize) -> ComponentCodecQuant {
    let l = &LEVELS[level];
    ComponentCodecQuant {
        hl1: l[0],
        lh1: l[1],
        hh1: l[2],
        hl2: l[3],
        lh2: l[4],
        hh2: l[5],
        hl3: l[6],
        lh3: l[7],
        hh3: l[8],
        ll3: l[9],
    }
}

fn base_quant() -> ComponentCodecQuant {
    ComponentCodecQuant {
        ll3: BASE_QUANT,
        hl3: BASE_QUANT,
        lh3: BASE_QUANT,
        hh3: BASE_QUANT,
        hl2: BASE_QUANT,
        lh2: BASE_QUANT,
        hh2: BASE_QUANT,
        hl1: BASE_QUANT,
        lh1: BASE_QUANT,
        hh1: BASE_QUANT,
    }
}

fn progressive_quants() -> Vec<ProgressiveCodecQuant> {
    const QUALITY: [u8; LEVELS.len()] = [0x28, 0x80, 0xFF];
    (0..LEVELS.len())
        .map(|l| ProgressiveCodecQuant {
            quality: QUALITY[l],
            y_quant: component_quant(l),
            cb_quant: component_quant(l),
            cr_quant: component_quant(l),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Bit streams
// ---------------------------------------------------------------------------------------------------------------

/// Most significant bit first, padded with zeros to a whole byte.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    used: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> i) & 1 == 1 {
                *self.bytes.last_mut().expect("pushed above") |= 0x80 >> (self.used % 8);
            }
            self.used += 1;
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// The SRL coding of the values of an upgrade pass that were zero before it: runs of zeros with an adaptive length
/// (`kp`, `k = kp / 8`), then a sign and a unary magnitude. The state lives across the sub-bands of a component, as
/// in the decoder.
struct SrlWriter {
    bits: BitWriter,
    kp: u32,
    zeros: u32,
}

impl SrlWriter {
    fn new() -> Self {
        Self {
            bits: BitWriter::default(),
            kp: 8,
            zeros: 0,
        }
    }

    /// `value` has a magnitude below `1 << num_bits`.
    fn push(&mut self, value: i32, num_bits: u32) {
        if value == 0 {
            self.zeros += 1;
            return;
        }
        loop {
            let k = self.kp / 8;
            if self.zeros < (1 << k) {
                break;
            }
            self.bits.put(0, 1);
            self.zeros -= 1 << k;
            self.kp = (self.kp + 4).min(80);
        }
        let k = self.kp / 8;
        self.bits.put(1, 1);
        self.bits.put(self.zeros, k);
        self.zeros = 0;
        self.bits.put(u32::from(value < 0), 1);
        self.kp = self.kp.saturating_sub(6);
        if num_bits > 1 {
            let max = (1u32 << num_bits) - 1;
            let magnitude = value.unsigned_abs().min(max);
            for _ in 1..magnitude {
                self.bits.put(0, 1);
            }
            if magnitude < max {
                self.bits.put(1, 1);
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        // Zeros left at the end: a run marker covers them, the decoder stops reading when its tile is complete.
        if self.zeros > 0 {
            self.bits.put(0, 1);
        }
        self.bits.finish()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Tiles
// ---------------------------------------------------------------------------------------------------------------

/// What the encoder keeps of a tile to refine it: the exact coefficients and the sign of what the client holds.
struct Refinement {
    coeffs: [Plane; 3],
    sign: [[i8; COEFFS]; 3],
}

struct FirstPass {
    refinement: Box<Refinement>,
    data: [Vec<u8>; 3],
}

struct UpgradePass {
    srl: [Vec<u8>; 3],
    raw: [Vec<u8>; 3],
}

fn layout() -> [BandInfo; BANDS] {
    dwt_extrapolate::band_layout()
}

fn sign_of(v: i16) -> i8 {
    v.signum() as i8
}

/// The coefficients of a 64x64 tile, in 11.5 fixed point, after the three DWT levels.
fn transform(bgra: &[u8]) -> Result<[Plane; 3]> {
    let (mut y, mut cb, mut cr) = ([0i16; COEFFS], [0i16; COEFFS], [0i16; COEFFS]);
    to_64x64_ycbcr_tile(
        bgra,
        64,
        64,
        (TILE * 4) as u32,
        PixelFormat::BgrA32,
        &mut y,
        &mut cb,
        &mut cr,
    )
    .map_err(|e| anyhow!("colour conversion: {e}"))?;
    let mut temp = [0i16; COEFFS];
    for plane in [&mut y, &mut cb, &mut cr] {
        dwt_extrapolate::encode(plane, &mut temp);
    }
    Ok([y, cb, cr])
}

/// The value the client holds for `coeff` once the bits below `level` are dropped.
fn quantize(coeff: i16, shift: u32, floor: bool) -> i16 {
    if floor {
        coeff >> shift
    } else {
        let magnitude = i32::from(coeff).unsigned_abs() >> shift;
        // At most 15 bits: the cast cannot overflow.
        (magnitude as i16) * coeff.signum()
    }
}

fn first_pass(bgra: &[u8]) -> Result<FirstPass> {
    let coeffs = transform(bgra)?;
    let bands = layout();
    let mut sign = [[0i8; COEFFS]; 3];
    let mut data: [Vec<u8>; 3] = Default::default();
    for c in 0..3 {
        let mut q = coeffs[c];
        for (b, band) in bands.iter().enumerate() {
            let s = shift(0, b);
            let range = band.offset..band.offset + band.count();
            for (v, g) in q[range.clone()].iter_mut().zip(&mut sign[c][range]) {
                *v = quantize(*v, s, b == LL3);
                if b != LL3 {
                    *g = sign_of(*v);
                }
            }
        }
        subband_reconstruction::encode(&mut q[bands[LL3].offset..]);
        let mut out = vec![0u8; COMPONENT_BYTES];
        let len = rlgr::encode(EntropyAlgorithm::Rlgr1, &q, &mut out)
            .map_err(|e| anyhow!("RLGR: {e:?}"))?;
        out.truncate(len);
        data[c] = out;
    }
    Ok(FirstPass {
        refinement: Box::new(Refinement { coeffs, sign }),
        data,
    })
}

/// Refines a component from `level - 1` to `level`.
fn upgrade_component(coeffs: &Plane, sign: &mut [i8; COEFFS], level: usize) -> (Vec<u8>, Vec<u8>) {
    let bands = layout();
    let mut srl = SrlWriter::new();
    let mut raw = BitWriter::default();
    for (b, band) in bands.iter().enumerate() {
        let (from, to) = (shift(level - 1, b), shift(level, b));
        let num_bits = from - to;
        if num_bits == 0 {
            continue;
        }
        for i in band.offset..band.offset + band.count() {
            let c = coeffs[i];
            if b == LL3 {
                // Floored in every pass: what is left is positive.
                let held = i32::from(c >> from) << num_bits;
                raw.put((i32::from(c >> to) - held) as u32, num_bits);
            } else if sign[i] != 0 {
                let magnitude = i32::from(c).unsigned_abs();
                let held = (magnitude >> from) << num_bits;
                raw.put((magnitude >> to) - held, num_bits);
            } else {
                let value = i32::from(c.signum()) * (i32::from(c).unsigned_abs() >> to) as i32;
                srl.push(value, num_bits);
                sign[i] = value.signum() as i8;
            }
        }
    }
    (srl.finish(), raw.finish())
}

fn upgrade_pass(refinement: &mut Refinement, level: usize) -> UpgradePass {
    let mut pass = UpgradePass {
        srl: Default::default(),
        raw: Default::default(),
    };
    for c in 0..3 {
        let (srl, raw) = upgrade_component(&refinement.coeffs[c], &mut refinement.sign[c], level);
        pass.srl[c] = srl;
        pass.raw[c] = raw;
    }
    pass
}

// ---------------------------------------------------------------------------------------------------------------
// Frame encoder
// ---------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Tile {
    /// Passes the client has received (`0`: nothing yet).
    passes: usize,
    /// Present while the tile can still be refined.
    refinement: Option<Box<Refinement>>,
}

enum Work {
    First {
        pixels: Box<[u8; COEFFS * 4]>,
    },
    Upgrade {
        refinement: Box<Refinement>,
        level: usize,
    },
}

enum Done {
    First(FirstPass),
    Upgrade(Box<Refinement>, UpgradePass, usize),
}

struct Job {
    index: usize,
    work: Option<Work>,
    done: Option<Result<Done>>,
}

impl Job {
    fn run(&mut self) {
        self.done = Some(match self.work.take().expect("run once") {
            Work::First { pixels } => first_pass(&pixels[..]).map(Done::First),
            Work::Upgrade {
                mut refinement,
                level,
            } => {
                let pass = upgrade_pass(&mut refinement, level);
                Ok(Done::Upgrade(refinement, pass, level))
            }
        });
    }
}

/// One progressive bitmap stream, ready for `WireToSurface2`.
pub struct Update {
    pub data: Vec<u8>,
    /// Tiles in it that are new (first pass) or refined (upgrade pass).
    pub first: usize,
    pub upgraded: usize,
    /// Tiles that can still be refined: the caller keeps calling `encode` while it is true.
    pub pending: bool,
}

/// The encoder of one surface.
pub struct Encoder {
    width: usize,
    height: usize,
    wide: usize,
    high: usize,
    /// The pixels the client was given: only tiles that differ are sent. Empty: nothing was sent yet.
    shown: Vec<u8>,
    tiles: Vec<Tile>,
    scan: usize,
    frame: u32,
}

impl Encoder {
    pub fn new(width: usize, height: usize) -> Self {
        let (wide, high) = (width.div_ceil(TILE), height.div_ceil(TILE));
        Self {
            width,
            height,
            wide,
            high,
            shown: Vec::new(),
            tiles: (0..wide * high).map(|_| Tile::default()).collect(),
            scan: 0,
            frame: 0,
        }
    }

    /// The client lost what it held (new surface or connection): everything is sent again.
    pub fn reset(&mut self) {
        self.shown.clear();
        self.tiles.iter_mut().for_each(|t| *t = Tile::default());
        self.scan = 0;
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Pixels of a tile, the missing ones at the edges repeating the last row and column.
    fn tile_pixels(&self, bgra: &[u8], tx: usize, ty: usize) -> Box<[u8; COEFFS * 4]> {
        let mut out = Box::new([0u8; COEFFS * 4]);
        for row in 0..TILE {
            let y = (ty * TILE + row).min(self.height - 1);
            for col in 0..TILE {
                let x = (tx * TILE + col).min(self.width - 1);
                let src = (y * self.width + x) * 4;
                let dst = (row * TILE + col) * 4;
                out[dst..dst + 4].copy_from_slice(&bgra[src..src + 4]);
            }
        }
        out
    }

    fn tile_rows(&self, tx: usize, ty: usize) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
        let x0 = tx * TILE;
        let x1 = (x0 + TILE).min(self.width);
        let y0 = ty * TILE;
        let y1 = (y0 + TILE).min(self.height);
        (y0..y1).map(move |y| (y * self.width + x0) * 4..(y * self.width + x1) * 4)
    }

    fn changed(&self, bgra: &[u8], tx: usize, ty: usize) -> bool {
        self.shown.is_empty()
            || self
                .tile_rows(tx, ty)
                .any(|r| bgra[r.clone()] != self.shown[r])
    }

    /// Encodes what changed since the last call, plus a few refinements. `None`: nothing to send.
    pub fn encode(&mut self, bgra: &[u8]) -> Result<Option<Update>> {
        ensure!(
            bgra.len() == self.width * self.height * 4,
            "frame of {} bytes for a {}x{} surface",
            bgra.len(),
            self.width,
            self.height
        );
        let mut jobs = Vec::new();
        let mut is_changed = vec![false; self.tiles.len()];
        for ty in 0..self.high {
            for tx in 0..self.wide {
                let index = ty * self.wide + tx;
                if self.changed(bgra, tx, ty) {
                    is_changed[index] = true;
                    jobs.push(Job {
                        index,
                        work: Some(Work::First {
                            pixels: self.tile_pixels(bgra, tx, ty),
                        }),
                        done: None,
                    });
                }
            }
        }
        // Tiles that kept still get closer to the exact picture, starting where the last call stopped.
        let total = self.tiles.len();
        let (start, mut upgrades) = (self.scan, 0);
        for step in 0..total {
            if upgrades == UPGRADES_PER_CALL {
                break;
            }
            let index = (start + step) % total;
            if is_changed[index] {
                continue;
            }
            let tile = &mut self.tiles[index];
            if let Some(refinement) = tile.refinement.take() {
                jobs.push(Job {
                    index,
                    work: Some(Work::Upgrade {
                        refinement,
                        level: tile.passes,
                    }),
                    done: None,
                });
                upgrades += 1;
                self.scan = (index + 1) % total;
            }
        }
        if jobs.is_empty() {
            return Ok(None);
        }
        // The pixels are remembered before the work: a failure resets the encoder, see `reset`.
        if self.shown.is_empty() {
            self.shown = vec![0; bgra.len()];
        }
        for job in &jobs {
            if matches!(job.work, Some(Work::First { .. })) {
                let (tx, ty) = (job.index % self.wide, job.index / self.wide);
                for r in self.tile_rows(tx, ty).collect::<Vec<_>>() {
                    self.shown[r.clone()].copy_from_slice(&bgra[r]);
                }
            }
        }
        run(&mut jobs);

        let mut first = Vec::new();
        let mut upgraded = Vec::new();
        for job in jobs {
            match job.done.expect("run set it")? {
                Done::First(pass) => {
                    let tile = &mut self.tiles[job.index];
                    tile.passes = 1;
                    tile.refinement = Some(pass.refinement);
                    first.push((job.index, pass.data));
                }
                Done::Upgrade(refinement, pass, level) => {
                    let tile = &mut self.tiles[job.index];
                    tile.passes = level + 1;
                    tile.refinement = (tile.passes < LEVELS.len()).then_some(refinement);
                    upgraded.push((job.index, level, pass));
                }
            }
        }
        let update = self.stream(&first, &upgraded)?;
        Ok(Some(update))
    }

    fn stream(
        &mut self,
        first: &[(usize, [Vec<u8>; 3])],
        upgraded: &[(usize, usize, UpgradePass)],
    ) -> Result<Update> {
        let coord = |index: usize| ((index % self.wide) as u16, (index / self.wide) as u16);
        let mut tiles = Vec::with_capacity(first.len() + upgraded.len());
        for (index, data) in first {
            let (x_idx, y_idx) = coord(*index);
            tiles.push(ProgressiveTile::First(TileFirst {
                quant_idx_y: 0,
                quant_idx_cb: 0,
                quant_idx_cr: 0,
                x_idx,
                y_idx,
                flags: 0,
                quality: 0,
                y_data: &data[0],
                cb_data: &data[1],
                cr_data: &data[2],
                tail_data: &[],
            }));
        }
        for (index, quality, pass) in upgraded {
            let (x_idx, y_idx) = coord(*index);
            tiles.push(ProgressiveTile::Upgrade(TileUpgrade {
                quant_idx_y: 0,
                quant_idx_cb: 0,
                quant_idx_cr: 0,
                x_idx,
                y_idx,
                quality: *quality as u8,
                y_srl_data: &pass.srl[0],
                y_raw_data: &pass.raw[0],
                cb_srl_data: &pass.srl[1],
                cb_raw_data: &pass.raw[1],
                cr_srl_data: &pass.srl[2],
                cr_raw_data: &pass.raw[2],
            }));
        }
        let rects = self.rects(
            first
                .iter()
                .map(|t| t.0)
                .chain(upgraded.iter().map(|t| t.0)),
        );
        self.frame = self.frame.wrapping_add(1);
        let blocks = [
            ProgressiveBlock::Sync(ProgressiveSyncPdu),
            ProgressiveBlock::Context(ProgressiveContextPdu {
                context_id: 0,
                tile_size: TILE as u16,
                // RFX_SUBBAND_DIFFING: the LL3 coefficients are delta coded.
                flags: 0x01,
            }),
            ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                frame_index: self.frame,
                region_count: 1,
            }),
            ProgressiveBlock::Region(ProgressiveRegion {
                tile_size: TILE as u8,
                rects,
                quant_vals: vec![base_quant()],
                quant_prog_vals: progressive_quants(),
                // RFX_DWT_REDUCE_EXTRAPOLATE
                flags: 0x01,
                tiles,
            }),
            ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
        ];
        let data =
            encode_progressive_stream(&blocks).map_err(|e| anyhow!("progressive stream: {e}"))?;
        Ok(Update {
            data,
            first: first.len(),
            upgraded: upgraded.len(),
            pending: self.tiles.iter().any(|t| t.refinement.is_some()),
        })
    }

    /// The area the tiles cover: one rectangle per run of neighbours on a row, cut at the surface edge.
    fn rects(&self, indexes: impl Iterator<Item = usize>) -> Vec<RfxRectangle> {
        let mut sorted: Vec<usize> = indexes.collect();
        sorted.sort_unstable();
        let mut rects: Vec<(usize, usize, usize)> = Vec::new(); // row, first column, last column
        for index in sorted {
            let (col, row) = (index % self.wide, index / self.wide);
            match rects.last_mut() {
                Some((r, _, last)) if *r == row && *last + 1 == col => *last = col,
                _ => rects.push((row, col, col)),
            }
        }
        rects
            .into_iter()
            .map(|(row, first, last)| {
                let (x, y) = (first * TILE, row * TILE);
                RfxRectangle {
                    x: x as u16,
                    y: y as u16,
                    width: ((last + 1) * TILE).min(self.width).saturating_sub(x) as u16,
                    height: ((row + 1) * TILE).min(self.height).saturating_sub(y) as u16,
                }
            })
            .collect()
    }
}

/// Runs the jobs on all the cores when there are enough of them to pay for the threads.
fn run(jobs: &mut [Job]) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    if jobs.len() < 8 || threads == 1 {
        jobs.iter_mut().for_each(Job::run);
        return;
    }
    let chunk = jobs.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for part in jobs.chunks_mut(chunk) {
            scope.spawn(|| part.iter_mut().for_each(Job::run));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_graphics::color_conversion::{ycbcr_to_rgba, YCbCrBuffer};
    use ironrdp_graphics::rlgr;
    use ironrdp_pdu::codecs::rfx::progressive::decode_progressive_stream;
    use ironrdp_pdu::codecs::rfx::EntropyAlgorithm;

    // The reference decoder below is a port of the one of FreeRDP (`progressive_rfx_srl_read`,
    // `progressive_rfx_upgrade_block`, `progressive_decompress_tile_first`): what an encoder must satisfy.

    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> Bits<'a> {
        fn new(data: &'a [u8]) -> Self {
            Self { data, pos: 0 }
        }
        fn bit(&mut self) -> u32 {
            let byte = self.data.get(self.pos / 8).copied().unwrap_or(0);
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            u32::from(bit)
        }
        fn bits(&mut self, count: u32) -> u32 {
            (0..count).fold(0, |acc, _| (acc << 1) | self.bit())
        }
    }

    struct SrlReader<'a> {
        bits: Bits<'a>,
        kp: u32,
        mode: bool,
        nz: u32,
    }

    impl<'a> SrlReader<'a> {
        fn new(data: &'a [u8]) -> Self {
            Self {
                bits: Bits::new(data),
                kp: 8,
                mode: false,
                nz: 0,
            }
        }

        fn read(&mut self, num_bits: u32) -> i32 {
            if self.nz > 0 {
                self.nz -= 1;
                return 0;
            }
            let k = self.kp / 8;
            if !self.mode {
                if self.bits.bit() == 0 {
                    self.nz = 1 << k;
                    self.kp = (self.kp + 4).min(80);
                    self.nz -= 1;
                    return 0;
                }
                self.nz = 0;
                self.mode = true;
                if k > 0 {
                    self.nz = self.bits.bits(k);
                }
                if self.nz > 0 {
                    self.nz -= 1;
                    return 0;
                }
            }
            self.mode = false;
            let negative = self.bits.bit() == 1;
            self.kp = self.kp.saturating_sub(6);
            if num_bits == 1 {
                return if negative { -1 } else { 1 };
            }
            let max = (1u32 << num_bits) - 1;
            let mut magnitude = 1;
            while magnitude < max {
                if self.bits.bit() == 1 {
                    break;
                }
                magnitude += 1;
            }
            if negative {
                -(magnitude as i32)
            } else {
                magnitude as i32
            }
        }
    }

    /// What a client holds of a tile.
    struct Held {
        current: [Plane; 3],
        sign: [[i16; COEFFS]; 3],
        prog: ComponentCodecQuant,
    }

    fn bit_pos(base: &ComponentCodecQuant, prog: &ComponentCodecQuant, band: usize) -> u32 {
        u32::from(base.for_band(band)) + u32::from(prog.for_band(band))
    }

    fn held_first(tile: &TileFirst, region: &ProgressiveRegion) -> Held {
        let base = &region.quant_vals[usize::from(tile.quant_idx_y)];
        let prog = region.quant_prog_vals[usize::from(tile.quality)].y_quant;
        let bands = layout();
        let mut held = Held {
            current: [[0; COEFFS]; 3],
            sign: [[0; COEFFS]; 3],
            prog,
        };
        for (c, data) in [tile.y_data, tile.cb_data, tile.cr_data]
            .into_iter()
            .enumerate()
        {
            let mut buf = [0i16; COEFFS];
            rlgr::decode(EntropyAlgorithm::Rlgr1, data, &mut buf).expect("RLGR");
            held.sign[c] = buf;
            subband_reconstruction::decode(&mut buf[bands[LL3].offset..]);
            for (b, band) in bands.iter().enumerate() {
                let s = bit_pos(base, &prog, b) - 1;
                for v in &mut buf[band.offset..band.offset + band.count()] {
                    *v = (i32::from(*v) << s) as i16;
                }
            }
            held.current[c] = buf;
        }
        held
    }

    fn held_upgrade(held: &mut Held, tile: &TileUpgrade, region: &ProgressiveRegion) {
        let base = &region.quant_vals[usize::from(tile.quant_idx_y)];
        let prog = region.quant_prog_vals[usize::from(tile.quality)].y_quant;
        let bands = layout();
        let streams = [
            (tile.y_srl_data, tile.y_raw_data),
            (tile.cb_srl_data, tile.cb_raw_data),
            (tile.cr_srl_data, tile.cr_raw_data),
        ];
        for (c, (srl, raw)) in streams.into_iter().enumerate() {
            let mut srl = SrlReader::new(srl);
            let mut raw = Bits::new(raw);
            for (b, band) in bands.iter().enumerate() {
                let num_bits = bit_pos(base, &held.prog, b) - bit_pos(base, &prog, b);
                if num_bits < 1 {
                    continue;
                }
                let s = bit_pos(base, &prog, b) - 1;
                for i in band.offset..band.offset + band.count() {
                    let input = if b == LL3 || held.sign[c][i] > 0 {
                        raw.bits(num_bits) as i32
                    } else if held.sign[c][i] < 0 {
                        -(raw.bits(num_bits) as i32)
                    } else {
                        let v = srl.read(num_bits);
                        held.sign[c][i] = v as i16;
                        v
                    };
                    held.current[c][i] = (i32::from(held.current[c][i]) + (input << s)) as i16;
                }
            }
        }
        held.prog = prog;
    }

    fn rgba_of(held: &Held) -> Vec<u8> {
        let mut planes = held.current;
        let mut temp = [0i16; COEFFS];
        for p in &mut planes {
            dwt_extrapolate::decode(p, &mut temp);
        }
        let mut out = vec![0u8; COEFFS * 4];
        ycbcr_to_rgba(
            YCbCrBuffer {
                y: &planes[0],
                cb: &planes[1],
                cr: &planes[2],
            },
            &mut out,
        )
        .expect("colour conversion");
        out
    }

    /// A client: applies the streams it is given and tells what it shows.
    struct Client {
        width: usize,
        height: usize,
        wide: usize,
        held: std::collections::HashMap<(u16, u16), Held>,
        screen: Vec<u8>,
    }

    impl Client {
        fn new(width: usize, height: usize) -> Self {
            Self {
                width,
                height,
                wide: width.div_ceil(TILE),
                held: Default::default(),
                screen: vec![0; width * height * 4],
            }
        }

        fn apply(&mut self, data: &[u8]) {
            let blocks = decode_progressive_stream(data).expect("a valid progressive stream");
            assert!(matches!(blocks[0], ProgressiveBlock::Sync(_)));
            assert!(matches!(blocks[1], ProgressiveBlock::Context(_)));
            assert!(matches!(blocks.last(), Some(ProgressiveBlock::FrameEnd(_))));
            for block in &blocks {
                let ProgressiveBlock::Region(region) = block else {
                    continue;
                };
                assert!(region.uses_reduce_extrapolate());
                // FreeRDP refuses the region otherwise.
                for quant in &region.quant_vals {
                    assert!((0..10).all(|b| (6..=15).contains(&quant.for_band(b))));
                }
                for tile in &region.tiles {
                    let key = (tile.x_idx(), tile.y_idx());
                    match tile {
                        ProgressiveTile::First(t) => {
                            self.held.insert(key, held_first(t, region));
                        }
                        ProgressiveTile::Upgrade(t) => {
                            let held = self
                                .held
                                .get_mut(&key)
                                .expect("an upgrade follows a first pass");
                            held_upgrade(held, t, region);
                        }
                        ProgressiveTile::Simple(_) => panic!("no simple tile expected"),
                    }
                    self.show(key);
                }
            }
        }

        fn show(&mut self, (tx, ty): (u16, u16)) {
            let rgba = rgba_of(&self.held[&(tx, ty)]);
            for row in 0..TILE {
                let y = usize::from(ty) * TILE + row;
                for col in 0..TILE {
                    let x = usize::from(tx) * TILE + col;
                    if x < self.width && y < self.height {
                        let (s, d) = ((row * TILE + col) * 4, (y * self.width + x) * 4);
                        // BGRA on the screen, RGBA from the decoder.
                        self.screen[d..d + 4].copy_from_slice(&[
                            rgba[s + 2],
                            rgba[s + 1],
                            rgba[s],
                            255,
                        ]);
                    }
                }
            }
            let _ = self.wide;
        }

        fn worst_error(&self, source: &[u8]) -> i32 {
            self.screen
                .chunks(4)
                .zip(source.chunks(4))
                .flat_map(|(a, b)| (0..3).map(move |i| (i32::from(a[i]) - i32::from(b[i])).abs()))
                .max()
                .unwrap_or(0)
        }
    }

    fn picture(width: usize, height: usize, seed: u32) -> Vec<u8> {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let mut out = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let checker = if ((x / 7) + (y / 5)) % 2 == 0 {
                    230
                } else {
                    20
                };
                let noise = if x > width / 2 && y > height / 3 {
                    (next() % 64) as i32
                } else {
                    0
                };
                let b = (x * 255 / width) as i32;
                let g = (y * 255 / height) as i32;
                let r = (checker + noise).min(255);
                out.extend_from_slice(&[b as u8, g as u8, r as u8, 255]);
            }
        }
        out
    }

    fn settle(encoder: &mut Encoder, client: &mut Client, picture: &[u8]) -> Vec<Update> {
        let mut updates = Vec::new();
        for _ in 0..40 {
            let Some(update) = encoder.encode(picture).expect("encoding") else {
                break;
            };
            client.apply(&update.data);
            updates.push(update);
        }
        updates
    }

    #[test]
    fn a_picture_that_stays_still_ends_exact() {
        let (w, h) = (200, 150);
        let source = picture(w, h, 7);
        let (mut encoder, mut client) = (Encoder::new(w, h), Client::new(w, h));
        let first = encoder.encode(&source).unwrap().expect("the first picture");
        assert_eq!(first.first, 4 * 3);
        assert_eq!(first.upgraded, 0);
        assert!(first.pending);
        client.apply(&first.data);
        let coarse = client.worst_error(&source);
        assert!(
            coarse > 4 && coarse < 90,
            "the first pass is lossy but recognisable: {coarse}"
        );
        let rest = settle(&mut encoder, &mut client, &source);
        assert!(!rest.is_empty());
        assert!(!rest.last().unwrap().pending);
        // Whole pixel precision on the coefficients of a picture made of hard edges and noise.
        assert!(
            client.worst_error(&source) <= 16,
            "{}",
            client.worst_error(&source)
        );
    }

    #[test]
    fn each_pass_gets_closer_to_the_picture() {
        let (w, h) = (128, 64);
        let source = picture(w, h, 3);
        let (mut encoder, mut client) = (Encoder::new(w, h), Client::new(w, h));
        let mut errors = Vec::new();
        for _ in 0..LEVELS.len() {
            let update = encoder.encode(&source).unwrap().expect("a pass");
            client.apply(&update.data);
            errors.push(client.worst_error(&source));
        }
        assert!(errors.windows(2).all(|e| e[1] <= e[0]), "{errors:?}");
        assert!(errors[0] > errors[LEVELS.len() - 1], "{errors:?}");
    }

    #[test]
    fn a_picture_already_sent_is_not_sent_again() {
        let (w, h) = (96, 96);
        let source = picture(w, h, 5);
        let (mut encoder, mut client) = (Encoder::new(w, h), Client::new(w, h));
        settle(&mut encoder, &mut client, &source);
        assert!(encoder.encode(&source).unwrap().is_none());
    }

    #[test]
    fn only_the_tiles_that_changed_are_sent() {
        let (w, h) = (200, 150);
        let mut source = picture(w, h, 9);
        let (mut encoder, mut client) = (Encoder::new(w, h), Client::new(w, h));
        settle(&mut encoder, &mut client, &source);
        // One pixel in the tile at column 3, row 2.
        let at = (140 * w + 190) * 4;
        source[at] ^= 0xFF;
        let update = encoder.encode(&source).unwrap().expect("the change");
        assert_eq!((update.first, update.upgraded), (1, 0));
        client.apply(&update.data);
        settle(&mut encoder, &mut client, &source);
        assert!(client.worst_error(&source) <= 16);
    }

    #[test]
    fn a_tile_that_moves_restarts_from_the_first_pass() {
        let (w, h) = (64, 64);
        let (a, b) = (picture(w, h, 1), picture(w, h, 2));
        let (mut encoder, mut client) = (Encoder::new(w, h), Client::new(w, h));
        let one = encoder.encode(&a).unwrap().unwrap();
        client.apply(&one.data);
        // Changed in the middle of its refinement.
        let two = encoder.encode(&b).unwrap().unwrap();
        assert_eq!((two.first, two.upgraded), (1, 0));
        client.apply(&two.data);
        settle(&mut encoder, &mut client, &b);
        assert!(client.worst_error(&b) <= 16);
    }

    #[test]
    fn a_reset_sends_everything_again() {
        let (w, h) = (130, 70);
        let source = picture(w, h, 4);
        let (mut encoder, mut client) = (Encoder::new(w, h), Client::new(w, h));
        settle(&mut encoder, &mut client, &source);
        encoder.reset();
        let update = encoder.encode(&source).unwrap().expect("everything again");
        assert_eq!(update.first, 3 * 2);
    }

    #[test]
    fn the_rectangles_cover_the_tiles_and_stay_in_the_surface() {
        let (w, h) = (200, 150);
        let source = picture(w, h, 6);
        let mut encoder = Encoder::new(w, h);
        let update = encoder.encode(&source).unwrap().unwrap();
        let blocks = decode_progressive_stream(&update.data).unwrap();
        let region = blocks
            .iter()
            .find_map(|b| match b {
                ProgressiveBlock::Region(r) => Some(r),
                _ => None,
            })
            .unwrap();
        // One rectangle per row of tiles, cut at the edges of the surface.
        assert_eq!(region.rects.len(), 3);
        assert_eq!(
            (
                region.rects[0].x,
                region.rects[0].width,
                region.rects[0].height
            ),
            (0, 200, 64)
        );
        assert_eq!((region.rects[2].y, region.rects[2].height), (128, 22));
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_refused() {
        let mut encoder = Encoder::new(64, 64);
        assert!(encoder.encode(&[0; 10]).is_err());
    }

    #[test]
    fn the_srl_coding_is_read_back_by_the_reference_decoder() {
        let mut state = 0x1234_5678u32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for round in 0..200 {
            let num_bits = 1 + round % 4;
            let max = (1i32 << num_bits) - 1;
            // Mostly zeros, as in an upgrade pass.
            let density = 1 + next() % 30;
            let values: Vec<i32> = (0..300 + (next() % 200) as usize)
                .map(|_| {
                    if next() % density == 0 {
                        let magnitude = 1 + (next() as i32).rem_euclid(max);
                        if next() % 2 == 0 {
                            magnitude
                        } else {
                            -magnitude
                        }
                    } else {
                        0
                    }
                })
                .collect();
            let mut writer = SrlWriter::new();
            for &v in &values {
                writer.push(v, num_bits);
            }
            let bytes = writer.finish();
            let mut reader = SrlReader::new(&bytes);
            let read: Vec<i32> = values.iter().map(|_| reader.read(num_bits)).collect();
            assert!(read == values, "round {round}, {num_bits} bits");
        }
    }

    #[test]
    fn the_patched_rlgr_encoder_is_read_back_by_the_decoder() {
        let mut state = 0x9E37_79B9u32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for round in 0..200 {
            let density = 1 + next() % 12;
            let spread = 1 + (next() % 40) as i32;
            let values: Vec<i16> = (0..COEFFS)
                .map(|_| {
                    if next() % density == 0 {
                        ((next() as i32).rem_euclid(2 * spread + 1) - spread) as i16
                    } else {
                        0
                    }
                })
                .collect();
            let mut bytes = vec![0u8; COMPONENT_BYTES];
            let len =
                rlgr::encode(EntropyAlgorithm::Rlgr1, &values, &mut bytes).expect("encodable");
            bytes.truncate(len);
            let mut back = [0i16; COEFFS];
            rlgr::decode(EntropyAlgorithm::Rlgr1, &bytes, &mut back).expect("decodable");
            assert!(back[..] == values[..], "round {round}");
        }
    }

    #[test]
    fn the_bits_go_most_significant_first() {
        let mut w = BitWriter::default();
        w.put(0b101, 3);
        w.put(0b1, 1);
        w.put(0xFF, 8);
        assert_eq!(w.finish(), vec![0b1011_1111, 0b1111_0000]);
    }
}
