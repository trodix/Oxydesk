//! CPU access to a linear DMA-BUF (fallback when no GPU import path is available).
use std::io::{Seek, SeekFrom};
use std::os::fd::{AsRawFd, BorrowedFd};

use rdsktp_core::error::EncodeError;
use rdsktp_core::frame::{DmaBuf, Frame};

// <linux/dma-buf.h>: struct dma_buf_sync { __u64 flags; }, DMA_BUF_IOCTL_SYNC = _IOW('b', 0, ...).
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const SYNC_READ: u64 = 1;
const SYNC_START: u64 = 0;
const SYNC_END: u64 = 4;

fn sync(fd: BorrowedFd<'_>, flags: u64) {
    // SAFETY: plain ioctl on a valid fd with a pointer to a live u64. Failure (unsupported) is harmless.
    unsafe {
        libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &flags as *const u64);
    }
}

/// Size of a DMA-BUF in bytes (`lseek(SEEK_END)`).
pub(crate) fn size(dma: &DmaBuf) -> Result<u64, EncodeError> {
    let mut f = std::fs::File::from(dma.fd.try_clone().map_err(|e| EncodeError::Backend(format!("dup dmabuf: {e}")))?);
    f.seek(SeekFrom::End(0)).map_err(|e| EncodeError::Backend(format!("dmabuf size: {e}")))
}

/// Calls `f` with the mapped pixels of `frame` (rows start at `stride`, from the buffer's offset).
pub(crate) fn with_pixels<R>(frame: &Frame, dma: &DmaBuf, f: impl FnOnce(&[u8]) -> R) -> Result<R, EncodeError> {
    if dma.modifier != 0 {
        return Err(EncodeError::Unsupported(format!("DMA-BUF with modifier {:#x} cannot be read on the CPU", dma.modifier)));
    }
    let len = size(dma)? as usize;
    let need = dma.offset as usize + frame.stride as usize * frame.height as usize;
    if len < need {
        return Err(EncodeError::Unsupported(format!("DMA-BUF of {len} bytes is shorter than {need}")));
    }
    // SAFETY: read-only shared mapping of `len` bytes of a valid DMA-BUF, unmapped before returning.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, dma.fd.as_raw_fd(), 0) };
    if ptr == libc::MAP_FAILED {
        return Err(EncodeError::Backend(format!("mmap dmabuf: {}", std::io::Error::last_os_error())));
    }
    sync(std::os::fd::AsFd::as_fd(&dma.fd), SYNC_START | SYNC_READ);
    // SAFETY: `ptr` maps `len` readable bytes; the producer may still write, which only tears pixels.
    let r = f(unsafe { std::slice::from_raw_parts((ptr as *const u8).add(dma.offset as usize), len - dma.offset as usize) });
    sync(std::os::fd::AsFd::as_fd(&dma.fd), SYNC_END | SYNC_READ);
    // SAFETY: matches the mmap above.
    unsafe { libc::munmap(ptr, len) };
    Ok(r)
}
