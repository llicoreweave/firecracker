// SPDX-License-Identifier: Apache-2.0
//! Host-side peer BAR export/import. Guest CUDA DMA-BUFs are separate objects.
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use super::sys::{Container, VfioSysError};

// Linux include/uapi/linux/{vfio,iommufd}.h: all requests use _IO(';', nr).
const VFIO_DEVICE_FEATURE: libc::c_ulong = 0x3b75;
const IOMMU_VFIO_IOAS: libc::c_ulong = 0x3b88;
const IOMMU_IOAS_MAP_FILE: libc::c_ulong = 0x3b8f;
const IOMMU_IOAS_UNMAP: libc::c_ulong = 0x3b86;

#[repr(C)]
#[derive(Default)]
struct CompatIoas {
    size: u32,
    id: u32,
    op: u32,
    reserved: u32,
}
#[repr(C)]
#[derive(Default)]
struct Export {
    argsz: u32,
    feature: u32,
    region: u32,
    open_flags: u32,
    flags: u32,
    ranges: u32,
    offset: u64,
    length: u64,
}
#[repr(C)]
#[derive(Default)]
struct MapFile {
    size: u32,
    flags: u32,
    id: u32,
    fd: i32,
    start: u64,
    length: u64,
    iova: u64,
}
#[repr(C)]
struct Unmap {
    size: u32,
    id: u32,
    iova: u64,
    length: u64,
}

fn ioctl<T>(
    fd: RawFd,
    request: libc::c_ulong,
    args: &mut T,
    name: &'static str,
) -> Result<i32, VfioSysError> {
    // SAFETY: callers supply the matching initialized repr(C) UAPI argument, live for this call.
    let result = unsafe { libc::ioctl(fd, request as _, args as *mut T) };
    if result < 0 {
        Err(VfioSysError::Ioctl(
            name,
            vmm_sys_util::errno::Error::last(),
        ))
    } else {
        Ok(result)
    }
}

/// Owns a BAR import and its IOAS reference until DMA has been stopped.
#[derive(Debug)]
pub struct PeerDmaMapping {
    ioas: OwnedFd,
    _buffer: OwnedFd,
    id: u32,
    iova: u64,
    length: u64,
}

impl PeerDmaMapping {
    pub fn new(
        container: &Container,
        device: RawFd,
        region: u32,
        offset: u64,
        length: u64,
        iova: u64,
    ) -> Result<Self, VfioSysError> {
        let mut compat = CompatIoas {
            size: u32::try_from(size_of::<CompatIoas>()).unwrap(),
            ..Default::default()
        };
        ioctl(
            container.as_raw_fd(),
            IOMMU_VFIO_IOAS,
            &mut compat,
            "IOMMU_VFIO_IOAS_GET",
        )?;
        let mut export = Export {
            argsz: u32::try_from(size_of::<Export>()).unwrap(),
            feature: (1 << 16) | 11,
            region,
            open_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
            ranges: 1,
            offset,
            length,
            ..Default::default()
        };
        let fd = ioctl(
            device,
            VFIO_DEVICE_FEATURE,
            &mut export,
            "VFIO_DEVICE_FEATURE_DMA_BUF",
        )?;
        // SAFETY: the successful GET operation returns a new owned DMA-BUF descriptor.
        let buffer = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: container is live; F_DUPFD_CLOEXEC returns a new descriptor owned here.
        let duplicate = unsafe { libc::fcntl(container.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(VfioSysError::Ioctl(
                "duplicate IOAS",
                vmm_sys_util::errno::Error::last(),
            ));
        }
        // SAFETY: duplicate is a newly owned descriptor.
        let ioas = unsafe { OwnedFd::from_raw_fd(duplicate) };
        let mut map = MapFile {
            size: u32::try_from(size_of::<MapFile>()).unwrap(),
            flags: 7,
            id: compat.id,
            fd: buffer.as_raw_fd(),
            start: 0,
            length,
            iova,
        };
        ioctl(
            ioas.as_raw_fd(),
            IOMMU_IOAS_MAP_FILE,
            &mut map,
            "IOMMU_IOAS_MAP_FILE peer BAR",
        )?;
        crate::logger::info!(
            "peer-dma: BAR{region} offset={offset:#x} length={length:#x} IOVA={iova:#x} IOAS={} DMA-BUF imported",
            compat.id
        );
        Ok(Self {
            ioas,
            _buffer: buffer,
            id: compat.id,
            iova,
            length,
        })
    }
}

impl Drop for PeerDmaMapping {
    fn drop(&mut self) {
        let mut unmap = Unmap {
            size: u32::try_from(size_of::<Unmap>()).unwrap(),
            id: self.id,
            iova: self.iova,
            length: self.length,
        };
        if let Err(error) = ioctl(
            self.ioas.as_raw_fd(),
            IOMMU_IOAS_UNMAP,
            &mut unmap,
            "IOMMU_IOAS_UNMAP peer BAR",
        ) {
            crate::logger::error!("Failed to remove peer-DMA mapping: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peer_dma_uapi_layout() {
        assert_eq!(size_of::<CompatIoas>(), 16);
        assert_eq!(size_of::<Export>(), 40);
        assert_eq!(size_of::<MapFile>(), 40);
        assert_eq!(size_of::<Unmap>(), 24);
    }
}
