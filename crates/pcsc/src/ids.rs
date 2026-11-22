//! Handles minted for the clients: `hContext` (non-zero `u32`) and `hCard` (positive `i32`).
//!
//! pcsc-lite hands out random handles so that a client cannot guess the handle of another one; the same here.

/// A small generator (splitmix64) seeded from the kernel.
pub(crate) struct IdMint {
    state: u64,
}

fn seed() -> u64 {
    let mut buf = [0u8; 8];
    // SAFETY: `buf` is valid for 8 bytes.
    let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
    if n == buf.len() as isize {
        return u64::from_ne_bytes(buf);
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    t ^ (u64::from(std::process::id()) << 32)
}

impl IdMint {
    pub fn new() -> Self {
        Self { state: seed() }
    }

    #[cfg(test)]
    pub fn with_seed(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A context handle: not 0, and not one `taken` says is in use.
    pub fn context(&mut self, taken: impl Fn(u32) -> bool) -> u32 {
        loop {
            let v = self.next() as u32;
            if v != 0 && !taken(v) {
                return v;
            }
        }
    }

    /// A card handle: positive, and not one `taken` says is in use.
    pub fn card(&mut self, taken: impl Fn(i32) -> bool) -> i32 {
        loop {
            let v = (self.next() as u32 & 0x7FFF_FFFF) as i32;
            if v != 0 && !taken(v) {
                return v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_valid_and_avoid_the_taken_ones() {
        let mut m = IdMint::with_seed(1);
        let first = IdMint::with_seed(1).next() as u32;
        let c = m.context(|v| v == first);
        assert!(c != 0 && c != first);
        for _ in 0..1000 {
            let h = m.card(|_| false);
            assert!(h > 0);
        }
        let h = m.card(|v| v < 0x7000_0000);
        assert!(h >= 0x7000_0000);
    }

    #[test]
    fn two_mints_do_not_agree() {
        let a = IdMint::new().next();
        let b = IdMint::new().next();
        assert_ne!(a, b);
    }

    #[test]
    fn zero_is_never_a_context() {
        // A generator that would return 0 first: the loop skips it.
        let mut m = IdMint::with_seed(0);
        let mut zeros = 0;
        for _ in 0..10_000 {
            if m.context(|_| false) == 0 {
                zeros += 1;
            }
        }
        assert_eq!(zeros, 0);
    }
}
