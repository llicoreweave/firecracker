// Copyright 2026 CoreWeave, Inc.
// SPDX-License-Identifier: Apache-2.0

//! MSI capability emulation using upstream VFIO eventfds and KVM MSI routing.

use super::*;
use crate::pci::msix::MsixTableEntry;
use crate::vstate::interrupts::MsixVectorGroup;

const ENABLE: u16 = 1;
const MME: u16 = 0x70;
const ADDRESS_64: u16 = 0x80;
const PER_VECTOR_MASK: u16 = 0x100;

#[derive(Clone, Debug)]
struct MsiCapability {
    offset: usize,
    bytes: Vec<u8>,
    max_vectors: usize,
}

impl MsiCapability {
    fn find(config: &[u32; PCI_CONFIG_SPACE_REGS as usize], irq_count: u32) -> Option<Self> {
        let bytes = config.as_bytes();
        let mut next = usize::from(bytes[0x34]);
        let mut visited = [false; 256];
        while (0x40..256).contains(&next) && next.is_multiple_of(4) && !visited[next] {
            visited[next] = true;
            if bytes[next] == 5 {
                let mut control = u16::from_le_bytes(bytes[next + 2..next + 4].try_into().unwrap());
                let max_shift = ((control >> 1) & 7)
                    .min(5)
                    .min(u16::try_from(irq_count.checked_ilog2()?).ok()?);
                let max_vectors = 1usize << max_shift;
                control = (control & !(ENABLE | MME | 0xe)) | (max_shift << 1);
                let data_offset = if control & ADDRESS_64 != 0 { 12 } else { 8 };
                let length = if control & PER_VECTOR_MASK != 0 {
                    data_offset + 12
                } else {
                    data_offset + 4
                };
                if next + length > 256 {
                    return None;
                }
                let mut cap = vec![0; length];
                cap[..2].copy_from_slice(&bytes[next..next + 2]);
                cap[2..4].copy_from_slice(&control.to_le_bytes());
                return Some(Self {
                    offset: next,
                    bytes: cap,
                    max_vectors,
                });
            }
            next = usize::from(bytes[next + 1]);
        }
        None
    }

    fn control(&self) -> u16 {
        u16::from_le_bytes(self.bytes[2..4].try_into().unwrap())
    }
    fn data_offset(&self) -> usize {
        if self.control() & ADDRESS_64 != 0 {
            12
        } else {
            8
        }
    }
    fn contains(&self, offset: usize) -> bool {
        (self.offset..self.offset + self.bytes.len()).contains(&offset)
    }
    fn active_vectors(&self) -> usize {
        if self.control() & ENABLE == 0 {
            0
        } else {
            (1usize << ((self.control() & MME) >> 4)).min(self.max_vectors)
        }
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) {
        let start = offset - self.offset;
        let old = self.control();
        let data_offset = self.data_offset();
        let mask_offset = data_offset + 4;
        for (index, &byte) in bytes.iter().enumerate() {
            let index = start + index;
            if index >= self.bytes.len() {
                break;
            }
            if (2..4).contains(&index)
                || (4..data_offset + 2).contains(&index)
                || (old & PER_VECTOR_MASK != 0 && (mask_offset..mask_offset + 4).contains(&index))
            {
                self.bytes[index] = byte;
            }
        }
        let mut control = (old & !(ENABLE | MME)) | (self.control() & (ENABLE | MME));
        let shift = ((control & MME) >> 4).min(u16::try_from(self.max_vectors.ilog2()).unwrap());
        control = (control & !MME) | (shift << 4);
        self.bytes[2..4].copy_from_slice(&control.to_le_bytes());
        self.bytes[4] &= !3;
    }

    fn entries(&self) -> Vec<MsixTableEntry> {
        let data = self.data_offset();
        let count = self.active_vectors();
        let address = u32::from_le_bytes(self.bytes[4..8].try_into().unwrap());
        let high = if self.control() & ADDRESS_64 != 0 {
            u32::from_le_bytes(self.bytes[8..12].try_into().unwrap())
        } else {
            0
        };
        let message = u32::from(u16::from_le_bytes(
            self.bytes[data..data + 2].try_into().unwrap(),
        ));
        let mask = if self.control() & PER_VECTOR_MASK != 0 {
            u32::from_le_bytes(self.bytes[data + 4..data + 8].try_into().unwrap())
        } else {
            0
        };
        (0..self.max_vectors)
            .map(|index| MsixTableEntry {
                msg_addr_lo: address,
                msg_addr_hi: high,
                msg_data: (message & !u32::try_from(count.saturating_sub(1)).unwrap())
                    | u32::try_from(index).unwrap(),
                vector_ctl: u32::from(index >= count || mask & (1 << index) != 0),
            })
            .collect()
    }
}

pub(super) struct MsiState {
    capability: MsiCapability,
    vectors: Arc<MsixVectorGroup>,
    sbdf: PciSBDF,
    enabled_count: usize,
}

impl MsiState {
    pub(super) fn new(
        config: &[u32; PCI_CONFIG_SPACE_REGS as usize],
        device: &InternalVfioDevice,
        vm: &Arc<KvmVm>,
        sbdf: PciSBDF,
    ) -> Result<Self, VfioError> {
        let count = device
            .get_irq_info(VFIO_PCI_MSI_IRQ_INDEX)
            .map_or(0, |info| info.count);
        let capability = MsiCapability::find(config, count).ok_or(VfioError::NoSupportedIrq)?;
        let vectors = Arc::new(KvmVm::create_msix_group(
            vm.clone(),
            u16::try_from(capability.max_vectors).unwrap(),
        )?);
        Ok(Self {
            capability,
            vectors,
            sbdf,
            enabled_count: 0,
        })
    }

    pub(super) fn contains(&self, register: u16) -> bool {
        self.capability.contains(usize::from(register) * 4)
    }

    pub(super) fn read(&self, register: u16) -> u32 {
        let offset = usize::from(register) * 4 - self.capability.offset;
        let pending_offset = self.capability.data_offset() + 8;
        if self.capability.control() & PER_VECTOR_MASK != 0 && offset == pending_offset {
            return self
                .vectors
                .vectors
                .iter()
                .enumerate()
                .fold(0, |bits, (index, vector)| {
                    let mut event = libc::pollfd {
                        fd: vector.event_fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: one initialized pollfd is live; a zero timeout never blocks.
                    let ready = unsafe { libc::poll(&mut event, 1, 0) } > 0
                        && event.revents & libc::POLLIN != 0;
                    bits | if ready { 1 << index } else { 0 }
                });
        }
        u32::from_le_bytes(
            self.capability.bytes[offset..offset + 4]
                .try_into()
                .unwrap(),
        )
    }

    pub(super) fn write(
        &mut self,
        device: &InternalVfioDevice,
        register: u16,
        offset: u8,
        bytes: &[u8],
    ) {
        self.capability
            .write(usize::from(register) * 4 + usize::from(offset), bytes);
        if let Err(error) = self.apply(device) {
            error!("Could not configure VFIO MSI routing: {error}");
            // Stop physical delivery as well as irqfds if reconfiguration failed.
            if let Err(error) = device.disable_msi() {
                error!("Could not disable VFIO MSI: {error}");
            }
            self.enabled_count = 0;
            self.capability.bytes[2] &= !1;
            if let Err(error) = self.vectors.disable() {
                error!("Could not disable KVM MSI vectors: {error}");
            }
        }
    }

    fn apply(&mut self, device: &InternalVfioDevice) -> Result<(), VfioError> {
        let count = self.capability.active_vectors();
        if self.enabled_count != count && self.enabled_count != 0 {
            device.disable_msi()?;
            self.enabled_count = 0;
        }
        self.vectors
            .update_batched(&self.capability.entries(), self.sbdf)?;
        if count != 0 && self.enabled_count != count {
            device.enable_msi(
                self.vectors.vectors[..count]
                    .iter()
                    .map(|v| &v.event_fd)
                    .collect(),
            )?;
            self.enabled_count = count;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(control: u16) -> [u32; PCI_CONFIG_SPACE_REGS as usize] {
        let mut words = [0; PCI_CONFIG_SPACE_REGS as usize];
        words[0x34 / 4] = 0x40;
        words[0x40 / 4] = (u32::from(control) << 16) | 5;
        words
    }

    #[test]
    fn msi_capability_bounds_and_read_only_fields() {
        let mut cap = MsiCapability::find(&config(ADDRESS_64 | PER_VECTOR_MASK | 0xe), 4).unwrap();
        assert_eq!(cap.max_vectors, 4);
        assert_eq!(cap.active_vectors(), 0);
        cap.write(0x40, &[0xff; 4]);
        assert_eq!(cap.bytes[0], 5);
        assert_eq!(cap.control() & 0xe, 4);
        assert_eq!(cap.active_vectors(), 4);
        assert_eq!(
            cap.control() & (ADDRESS_64 | PER_VECTOR_MASK),
            ADDRESS_64 | PER_VECTOR_MASK
        );
        assert!(MsiCapability::find(&config(0), 0).is_none());
        let mut invalid = config(0);
        invalid[0x34 / 4] = 0xfc;
        invalid[0xfc / 4] = (u32::from(ADDRESS_64) << 16) | 5;
        assert!(MsiCapability::find(&invalid, 1).is_none());
        invalid[0xfc / 4] = 0xfc01;
        assert!(MsiCapability::find(&invalid, 1).is_none());
    }

    #[test]
    fn msi_messages_masks_and_disable() {
        for wide in [0, ADDRESS_64] {
            let mut cap = MsiCapability::find(&config(wide | PER_VECTOR_MASK | 4), 4).unwrap();
            cap.write(0x44, &0xfee00003u32.to_le_bytes());
            if wide != 0 {
                cap.write(0x48, &2u32.to_le_bytes());
            }
            let offset = cap.data_offset();
            cap.write(0x40 + offset, &0x43u16.to_le_bytes());
            cap.write(0x40 + offset + 4, &2u32.to_le_bytes());
            cap.write(0x42, &0x21u16.to_le_bytes());
            let entries = cap.entries();
            assert_eq!(
                entries.iter().map(|e| e.msg_data).collect::<Vec<_>>(),
                vec![0x40, 0x41, 0x42, 0x43]
            );
            assert_eq!(
                entries.iter().map(|e| e.masked()).collect::<Vec<_>>(),
                vec![false, true, false, false]
            );
            assert_eq!(entries[0].msg_addr_lo, 0xfee00000);
            assert_eq!(entries[0].msg_addr_hi, if wide == 0 { 0 } else { 2 });
            cap.write(0x42, &0u16.to_le_bytes());
            assert!(cap.entries().iter().all(MsixTableEntry::masked));
        }
    }
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn msi_routes_masking_and_pending_bits_with_kvm() {
        use std::sync::atomic::Ordering;
        let vm = crate::vstate::vm::tests::setup_vm();
        vm.setup_irqchip().unwrap();
        let vm = Arc::new(vm);
        let mut capability = MsiCapability::find(&config(PER_VECTOR_MASK | 4), 4).unwrap();
        capability.write(0x44, &0xfee00000u32.to_le_bytes());
        capability.write(0x48, &0x40u16.to_le_bytes());
        capability.write(0x4c, &2u32.to_le_bytes());
        capability.write(0x42, &0x21u16.to_le_bytes());
        let state = MsiState {
            capability,
            vectors: Arc::new(KvmVm::create_msix_group(vm, 4).unwrap()),
            sbdf: PciSBDF::new(0, 25, 0, 0),
            enabled_count: 0,
        };
        state
            .vectors
            .update_batched(&state.capability.entries(), state.sbdf)
            .unwrap();
        assert!(state.vectors.vectors[0].enabled.load(Ordering::Acquire));
        assert!(!state.vectors.vectors[1].enabled.load(Ordering::Acquire));
        state.vectors.vectors[1].event_fd.write(1).unwrap();
        assert_eq!(state.read(0x50 / 4), 2);
        state.vectors.disable().unwrap();
    }
}
