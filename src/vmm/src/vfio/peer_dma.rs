// Copyright 2026 CoreWeave, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Host-side peer BAR export/import. Guest CUDA DMA-BUFs are separate objects.
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use iommufd_bindings::iommufd::iommu_ioas_alloc;
use iommufd_ioctls::IommuFd;
use vfio_ioctls::VfioIommufd;

use super::{KvmVm, VfioError};

// Linux include/uapi/linux/{vfio,iommufd}.h: all requests use _IO(';', nr).
const VFIO_DEVICE_FEATURE: libc::c_ulong = 0x3b75;
const IOMMU_IOAS_MAP_FILE: libc::c_ulong = 0x3b8f;
const IOMMU_IOAS_UNMAP: libc::c_ulong = 0x3b86;

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
) -> Result<i32, std::io::Error> {
    // SAFETY: callers supply the matching initialized repr(C) UAPI argument, live for this call.
    let result = unsafe { libc::ioctl(fd, request as _, args as *mut T) };
    if result < 0 {
        Err(std::io::Error::new(
            std::io::Error::last_os_error().kind(),
            format!("{name}: {}", std::io::Error::last_os_error()),
        ))
    } else {
        Ok(result)
    }
}

/// One explicitly selected IOMMUFD address space shared by the VM's devices.
/// The upstream cdev backend owns device attach/detach and KVM VFIO tracking.
pub(crate) struct PeerContext {
    pub(crate) ops: Arc<VfioIommufd>,
    // Drop the backend before destroying its externally allocated IOAS.
    owner: IoasOwner,
}

impl std::fmt::Debug for PeerContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerContext")
            .field("ioas", &self.owner.id)
            .finish()
    }
}

struct IoasOwner {
    fd: Arc<IommuFd>,
    id: u32,
}
impl Drop for IoasOwner {
    fn drop(&mut self) {
        if let Err(error) = self.fd.destroy_iommu_object(self.id) {
            crate::logger::error!("Could not destroy peer IOAS: {error}");
        }
    }
}

impl PeerContext {
    pub(crate) fn new(vm: &KvmVm) -> Result<Arc<Self>, VfioError> {
        let fd = Arc::new(IommuFd::new()?);
        let mut alloc = iommu_ioas_alloc {
            size: u32::try_from(size_of::<iommu_ioas_alloc>()).unwrap(),
            flags: 0,
            out_ioas_id: 0,
        };
        fd.alloc_iommu_ioas(&mut alloc)?;
        let owner = IoasOwner {
            fd: fd.clone(),
            id: alloc.out_ioas_id,
        };
        let device = Arc::new(super::vfio_create_kvm_device(vm)?);
        let ops = Arc::new(VfioIommufd::new(fd, Some(owner.id), Some(device))?);
        Ok(Arc::new(Self { ops, owner }))
    }
    pub(crate) fn map_guest_memory(
        &self,
        memory: &super::GuestMemoryMmap,
    ) -> Result<(), VfioError> {
        super::vfio_dma_map_guest_memory_with_ops(self.ops.as_ref(), memory)
    }
}

/// Owns a BAR import and its IOAS reference until DMA has been stopped.
#[derive(Debug)]
pub(crate) struct PeerDmaMapping {
    context: Arc<PeerContext>,
    _buffer: OwnedFd,
    id: u32,
    iova: u64,
    length: u64,
}

impl PeerDmaMapping {
    /// Export a device BAR and import it into this VM's IOMMU address space.
    pub(crate) fn new(
        context: &Arc<PeerContext>,
        device: RawFd,
        region: u32,
        offset: u64,
        length: u64,
        iova: u64,
    ) -> Result<Self, std::io::Error> {
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
        let mut map = MapFile {
            size: u32::try_from(size_of::<MapFile>()).unwrap(),
            flags: 7,
            id: context.owner.id,
            fd: buffer.as_raw_fd(),
            start: 0,
            length,
            iova,
        };
        ioctl(
            context.owner.fd.as_raw_fd(),
            IOMMU_IOAS_MAP_FILE,
            &mut map,
            "IOMMU_IOAS_MAP_FILE peer BAR",
        )?;
        crate::logger::info!(
            "peer-dma: BAR{region} offset={offset:#x} length={length:#x} IOVA={iova:#x} IOAS={} \
             DMA-BUF imported",
            context.owner.id
        );
        Ok(Self {
            context: context.clone(),
            _buffer: buffer,
            id: context.owner.id,
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
            self.context.owner.fd.as_raw_fd(),
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
        assert_eq!(size_of::<Export>(), 40);
        assert_eq!(size_of::<MapFile>(), 40);
        assert_eq!(size_of::<Unmap>(), 24);
    }
    #[test]
    #[ignore = "requires KVM, /dev/iommu, and permission to lock memory"]
    fn peer_dma_iommufd_guest_memory() {
        let vm = crate::vstate::vm::tests::setup_vm();
        let context = PeerContext::new(&vm).unwrap();
        let memory = crate::test_utils::single_region_mem(4 << 20);
        context.map_guest_memory(&memory).unwrap();
        context.ops.vfio_dma_unmap(0, 4 << 20).unwrap();
    }
}
