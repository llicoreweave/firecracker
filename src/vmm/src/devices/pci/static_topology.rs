// SPDX-License-Identifier: Apache-2.0
//! Experimental fixed, cold-boot PCIe topology. No hotplug or bus renumbering.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Barrier, Mutex};

use crate::pci::bus::{PciBus, PciBusError};
use crate::pci::{PciDevice, PciSBDF};

/// Fixed bridge configuration. Unknown/unimplemented capabilities are not exposed.
#[derive(Debug)]
pub struct StaticBridge {
    bytes: [u8; 256],
}

impl StaticBridge {
    fn new(
        primary: u8,
        secondary: u8,
        subordinate: u8,
        kind: u8,
        memory: Option<(u64, u64)>,
        prefetch: Option<(u64, u64)>,
    ) -> Self {
        let mut bytes = [0u8; 256];
        bytes[0..4].copy_from_slice(&[0x0f, 0x1d, 0x01, 0x02]);
        bytes[4] = 0x06; // Memory decoding and bus mastering.
        bytes[6] = 0x10; // Capabilities list.
        bytes[8] = 1;
        bytes[0x0a] = 4;
        bytes[0x0b] = 6; // PCI-to-PCI bridge.
        bytes[0x0e] = 1;
        bytes[0x18] = primary;
        bytes[0x19] = secondary;
        bytes[0x1a] = subordinate;
        bytes[0x1c] = 0xf0; // Disabled I/O window.
        bytes[0x20..0x24].copy_from_slice(&0x0000fff0u32.to_le_bytes());
        bytes[0x24..0x28].copy_from_slice(&0x0001fff1u32.to_le_bytes());
        if let Some((start, end)) = memory {
            let base = u16::try_from((start >> 16) & 0xfff0).unwrap();
            let limit = u16::try_from((end >> 16) & 0xfff0).unwrap();
            bytes[0x20..0x22].copy_from_slice(&base.to_le_bytes());
            bytes[0x22..0x24].copy_from_slice(&limit.to_le_bytes());
        }
        if let Some((start, end)) = prefetch {
            let base = (u16::try_from((start >> 16) & 0xfff0).unwrap()) | 1;
            let limit = (u16::try_from((end >> 16) & 0xfff0).unwrap()) | 1;
            bytes[0x24..0x26].copy_from_slice(&base.to_le_bytes());
            bytes[0x26..0x28].copy_from_slice(&limit.to_le_bytes());
            bytes[0x28..0x2c].copy_from_slice(&((start >> 32) as u32).to_le_bytes());
            bytes[0x2c..0x30].copy_from_slice(&((end >> 32) as u32).to_le_bytes());
        }
        bytes[0x34] = 0x40;
        bytes[0x40] = 0x10; // PCI Express capability.
        bytes[0x42..0x44].copy_from_slice(&(2u16 | (u16::from(kind) << 4)).to_le_bytes());
        // Synthetic links, not a promise of measured link bandwidth. No hotplug/ACS/ATS.
        bytes[0x4c..0x50].copy_from_slice(&0x00100105u32.to_le_bytes()); // 32 GT/s, x16, DLL active reporting.
        bytes[0x52..0x54].copy_from_slice(&0x2105u16.to_le_bytes());
        Self { bytes }
    }
}

impl PciDevice for StaticBridge {
    fn read_config_register(&mut self, reg: u16) -> u32 {
        self.bytes
            .get(usize::from(reg) * 4..usize::from(reg) * 4 + 4)
            .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
            .unwrap_or(u32::MAX)
    }
    fn write_config_register(&mut self, reg: u16, offset: u8, data: &[u8]) -> Option<Arc<Barrier>> {
        // This prototype preassigns and preserves bus numbers and resource windows.
        // Command and PCIe Device Control remain writable for driver initialization.
        for (i, value) in data.iter().enumerate() {
            let byte = usize::from(reg) * 4 + usize::from(offset) + i;
            if matches!(byte, 4 | 5 | 0x0c | 0x0d | 0x48 | 0x49) {
                self.bytes[byte] = *value;
            }
        }
        None
    }
}

/// A deterministic path-to-bus plan, validated before assigning any device.
#[derive(Debug)]
pub struct StaticTopology {
    pub buses: BTreeMap<Vec<u8>, u8>,
    pub root_buses: BTreeMap<u8, u8>,
    /// (root bus, last child bus, NUMA domain).
    pub domains: Vec<(u8, u8, usize)>,
}

impl StaticTopology {
    pub fn new(paths: &[Vec<u8>]) -> Result<Self, String> {
        let mut leaves = BTreeSet::new();
        let mut parents = BTreeSet::new();
        for path in paths {
            if path.len() < 2
                || path.len() > 8
                || path[0] == 0
                || path.iter().any(|slot| *slot >= 32)
            {
                return Err("invalid static PCI path".into());
            }
            if !leaves.insert(path.clone()) {
                return Err("duplicate static PCI path".into());
            }
            for count in 1..path.len() {
                parents.insert(path[..count].to_vec());
            }
        }
        if parents.iter().any(|path| leaves.contains(path)) || parents.len() >= 64 {
            return Err("PCI endpoint used as bridge, or exhausted 64-bus budget".into());
        }
        let buses = parents
            .into_iter()
            .enumerate()
            .map(|(i, p)| (p, u8::try_from(i + 1).unwrap()))
            .collect();
        Ok(Self {
            buses,
            root_buses: BTreeMap::new(),
            domains: Vec::new(),
        })
    }

    /// Keep each NUMA hierarchy under its own ACPI PCI host bridge. On x86,
    /// Linux derives subordinate-device NUMA nodes from that root's sysdata.
    pub fn apply_numa(&mut self, numa: &crate::numa::NumaConfig) -> Result<(), String> {
        if numa.nodes.is_empty() || self.buses.len() + numa.nodes.len() > 64 {
            return Err("NUMA PCI topology exhausts 64 buses".into());
        }
        let mut roots = BTreeSet::new();
        for slot in numa.nodes.iter().flat_map(|node| &node.pci_root_slots) {
            if *slot == 0 || *slot >= 32 || !roots.insert(*slot) {
                return Err("invalid or duplicate NUMA PCI root slot".into());
            }
        }
        if self.buses.keys().any(|path| !roots.contains(&path[0])) {
            return Err("NUMA mapping does not cover every PCI root slot".into());
        }
        self.root_buses.clear();
        self.domains.clear();
        let mut next = 0usize;
        for (domain, node) in numa.nodes.iter().enumerate() {
            if domain != 0 {
                next += 1;
            }
            let root_bus = u8::try_from(next).unwrap();
            for &slot in &node.pci_root_slots {
                self.root_buses.insert(slot, root_bus);
            }
            for (path, bus) in &mut self.buses {
                if node.pci_root_slots.contains(&path[0]) {
                    next += 1;
                    if next >= 64 {
                        return Err("NUMA PCI topology exhausts 64 buses".into());
                    }
                    *bus = u8::try_from(next).unwrap();
                }
            }
            self.domains
                .push((root_bus, u8::try_from(next).unwrap(), domain));
        }
        if self
            .buses
            .keys()
            .any(|p| !self.root_buses.contains_key(&p[0]))
        {
            return Err("NUMA mapping does not cover every PCI root slot".into());
        }
        Ok(())
    }

    pub fn address(&self, path: &[u8]) -> PciSBDF {
        PciSBDF::new(
            0,
            self.buses[&path[..path.len() - 1]],
            *path.last().unwrap(),
            0,
        )
    }

    pub fn install(
        &self,
        root: &Arc<Mutex<PciBus>>,
        ranges: &[(Vec<u8>, u64, u64, bool)],
    ) -> Result<(), PciBusError> {
        let mut root = root.lock().unwrap();
        for &(bus, _, _) in &self.domains {
            if bus != 0 {
                root.secondary
                    .insert(bus, Arc::new(Mutex::new(PciBus::default())));
            }
        }
        for bus in self.buses.values() {
            root.secondary
                .insert(*bus, Arc::new(Mutex::new(PciBus::default())));
        }
        for (path, secondary) in &self.buses {
            let primary = if path.len() == 1 {
                self.root_buses.get(&path[0]).copied().unwrap_or(0)
            } else {
                self.buses[&path[..path.len() - 1]]
            };
            let subordinate = self
                .buses
                .iter()
                .filter(|(child, _)| child.starts_with(path))
                .map(|(_, bus)| *bus)
                .max()
                .unwrap();
            let window = |prefetch| {
                let selected: Vec<_> = ranges
                    .iter()
                    .filter(|(leaf, _, _, pref)| leaf.starts_with(path) && *pref == prefetch)
                    .collect();
                Some((
                    selected.iter().map(|(_, start, _, _)| *start).min()?,
                    selected
                        .iter()
                        .map(|(_, start, len, _)| start + len - 1)
                        .max()?,
                ))
            };
            let kind = match path.len() {
                1 => 4,
                n if n % 2 == 0 => 5,
                _ => 6,
            };
            let bridge = Arc::new(Mutex::new(StaticBridge::new(
                primary,
                *secondary,
                subordinate,
                kind,
                window(false),
                window(true),
            )));
            if primary == 0 {
                root.add_device(*path.last().unwrap(), bridge)?;
            } else {
                root.secondary[&primary]
                    .lock()
                    .unwrap()
                    .add_device(*path.last().unwrap(), bridge)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_topology_counts_empty_numa_roots_in_bus_budget() {
        use crate::numa::{NumaConfig, NumaNode};
        let mut plan = StaticTopology::new(&[vec![1, 0]]).unwrap();
        let mut numa = NumaConfig {
            nodes: (0..64)
                .map(|id| NumaNode {
                    host_node: id,
                    vcpus: vec![u8::try_from(id).unwrap()],
                    host_cpus: vec![id],
                    memory_mib: 2,
                    pci_root_slots: if id == 0 { vec![1] } else { vec![] },
                })
                .collect(),
            distances: vec![],
        };
        plan.apply_numa(&numa).unwrap_err();
        assert!(plan.domains.is_empty());
        numa.nodes.pop();
        plan.apply_numa(&numa).unwrap();
        assert_eq!(plan.domains.last(), Some(&(63, 63, 62)));
        plan.apply_numa(&numa).unwrap();
        assert_eq!(plan.domains.len(), 63);
    }

    #[test]
    fn static_topology_configuration_access_reaches_secondary_buses() {
        use crate::pci::bus::{PciConfigIo, PciConfigMmio};
        use crate::vstate::bus::BusDevice;
        let plan = StaticTopology::new(&[vec![1, 2, 0]]).unwrap();
        let root = Arc::new(Mutex::new(PciBus::default()));
        plan.install(&root, &[]).unwrap();
        let mut ecam = PciConfigMmio::new(root.clone());
        let mut io = PciConfigIo::new(root);
        let address = (1 << 20) | (2 << 15);
        let mut data = [0; 4];
        ecam.read(0, address, &mut data);
        assert_eq!(data, [0x0f, 0x1d, 1, 2]);
        io.write(
            0,
            0,
            &(0x8000_0000u32 | (1 << 16) | (2 << 11)).to_le_bytes(),
        );
        assert_eq!(io.config_space_read(), u32::from_le_bytes(data));
        ecam.read(0, address | (1 << 12), &mut data);
        assert_eq!(data, [0xff; 4]);
        ecam.read(0, 63 << 20, &mut data);
        assert_eq!(data, [0xff; 4]);
    }
    #[test]
    fn static_topology_numa_has_separate_root_buses() {
        use crate::numa::{NumaConfig, NumaNode};
        let mut paths = Vec::new();
        for root in [13, 14, 15, 16, 18, 19, 20, 21] {
            paths.push(vec![root, 0, 1, 0]);
            paths.push(vec![root, 0, 2, 0]);
        }
        for slot in 0..4 {
            paths.push(vec![17, 0, slot, 0]);
        }
        let numa = NumaConfig {
            nodes: (0..2)
                .map(|id| NumaNode {
                    host_node: id,
                    vcpus: vec![u8::try_from(id).unwrap()],
                    host_cpus: vec![id],
                    memory_mib: 1024,
                    pci_root_slots: if id == 0 {
                        vec![13, 14, 15, 16]
                    } else {
                        vec![17, 18, 19, 20, 21]
                    },
                })
                .collect(),
            distances: vec![vec![10, 21], vec![21, 10]],
        };
        let mut plan = StaticTopology::new(&paths).unwrap();
        plan.apply_numa(&numa).unwrap();
        assert_eq!(plan.domains, vec![(0, 16, 0), (17, 39, 1)]);
        assert_eq!(plan.address(&[18, 0, 1, 0]).bus(), 26);
        let root = Arc::new(Mutex::new(PciBus::default()));
        plan.install(&root, &[]).unwrap();
        let bus = root.lock().unwrap();
        assert!(bus.device_on_bus(0, 17).is_none());
        let rp = bus.device_on_bus(17, 17).unwrap();
        let buses = rp.lock().unwrap().read_config_register(6);
        assert_eq!(buses & 0xff, 17);
        assert_eq!((buses >> 8) & 0xff, 18);
    }
    #[test]
    fn static_topology_pairs_share_upstream_switch() {
        let paths = vec![vec![16, 0, 1, 0], vec![16, 0, 2, 0], vec![17, 0, 1, 0]];
        let plan = StaticTopology::new(&paths).unwrap();
        assert_eq!(plan.buses.len(), 7);
        assert_ne!(plan.address(&paths[0]).bus(), plan.address(&paths[1]).bus());
        let root = Arc::new(Mutex::new(PciBus::default()));
        plan.install(&root, &[(paths[0].clone(), 2 << 40, 128 << 30, true)])
            .unwrap();
        let upstream = root
            .lock()
            .unwrap()
            .device_on_bus(plan.buses[&vec![16]], 0)
            .unwrap();
        assert_eq!(
            (upstream.lock().unwrap().read_config_register(0x42 / 4) >> 20) & 15,
            5
        );
        assert!(root.lock().unwrap().device_on_bus(63, 0).is_none());
    }
    #[test]
    fn static_topology_rejects_invalid_and_conflicting_paths() {
        for paths in [
            vec![vec![32, 0]],
            vec![vec![1]],
            vec![vec![1, 0], vec![1, 0]],
            vec![vec![1, 0], vec![1, 0, 0]],
        ] {
            StaticTopology::new(&paths).unwrap_err();
        }
    }

    #[test]
    fn static_topology_full_h100_fabric_fits_ecam() {
        let mut paths = Vec::new();
        for root in [13, 14, 15, 16, 18, 19, 20, 21] {
            paths.push(vec![root, 0, 1, 0]);
            paths.push(vec![root, 0, 2, 0]);
        }
        for slot in 0..4 {
            paths.push(vec![17, 0, slot, 0]);
        }
        let plan = StaticTopology::new(&paths).unwrap();
        assert_eq!(plan.buses.len(), 38);
        assert_eq!(plan.buses.values().copied().max(), Some(38));
        let root = Arc::new(Mutex::new(PciBus::default()));
        plan.install(&root, &[]).unwrap();
        for path in paths {
            let address = plan.address(&path);
            assert!(address.bus() < 64);
            assert_eq!(address.function(), 0);
        }
    }
}
