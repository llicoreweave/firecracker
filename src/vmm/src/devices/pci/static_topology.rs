// SPDX-License-Identifier: Apache-2.0
//! Fixed, cold-boot PCIe topology. No hotplug or bus renumbering.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Barrier, Mutex};

use crate::pci::bus::{PciBusError, PciBuses};
use crate::pci::{PciDevice, PciSBDF};
use crate::vmm_config::pci_topology::PciTopologyConfig;

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
        // The static topology preassigns and preserves bus numbers and resource windows.
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

/// Explicit, validated bus graph. Paths are derived only for BAR containment;
/// no host topology, NUMA placement, bus numbers or bridge types are inferred.
#[derive(Debug)]
pub struct StaticTopology {
    /// Caller-supplied roots and bridges.
    pub config: PciTopologyConfig,
    /// Configured secondary bus numbers indexed by bridge path.
    pub buses: BTreeMap<Vec<u8>, u8>,
    /// Explicit endpoint paths indexed by device identifier.
    pub paths: BTreeMap<String, Vec<u8>>,
}

impl StaticTopology {
    /// Validate the complete graph without reading host PCI topology.
    pub fn new(
        config: &PciTopologyConfig,
        devices: &[crate::vmm_config::vfio::VfioDeviceConfig],
        numa_nodes: usize,
    ) -> Result<Self, String> {
        let invalid = || "invalid explicit PCI topology".to_string();
        let mut roots = BTreeSet::new();
        let mut covered = BTreeSet::new();
        for root in &config.roots {
            if root.bus > root.last_bus
                || root.last_bus >= 64
                || !roots.insert(root.bus)
                || root.numa_node.is_some_and(|n| n as usize >= numa_nodes)
                || (root.bus..=root.last_bus).any(|b| !covered.insert(b))
            {
                return Err(invalid());
            }
        }
        if !roots.contains(&0) {
            return Err("explicit PCI topology requires root bus zero".into());
        }
        let mut addresses = BTreeSet::new();
        let mut parents = BTreeMap::new();
        for bridge in &config.bridges {
            bridge.pci_address.validate()?;
            if bridge.secondary_bus == bridge.pci_address.bus
                || bridge.secondary_bus > bridge.subordinate_bus
                || bridge.subordinate_bus >= 64
                || roots.contains(&bridge.secondary_bus)
                || !addresses.insert(bridge.pci_address)
                || parents.insert(bridge.secondary_bus, bridge).is_some()
            {
                return Err(invalid());
            }
        }
        // Walking configured parent edges detects cycles and disconnected buses.
        let bus_path = |mut bus: u8| -> Result<Vec<u8>, String> {
            let mut slots = Vec::new();
            let mut seen = BTreeSet::new();
            while !roots.contains(&bus) {
                if !seen.insert(bus) || seen.len() > 7 {
                    return Err(invalid());
                }
                let bridge = parents.get(&bus).ok_or_else(invalid)?;
                slots.push(bridge.pci_address.device);
                bus = bridge.pci_address.bus;
            }
            slots.push(bus);
            slots.reverse();
            Ok(slots)
        };
        let mut buses = BTreeMap::new();
        for bridge in &config.bridges {
            let path = bus_path(bridge.secondary_bus)?;
            let root = config.roots.iter().find(|r| r.bus == path[0]).unwrap();
            if bridge.secondary_bus < root.bus || bridge.subordinate_bus > root.last_bus {
                return Err(invalid());
            }
            // Forwarding windows must contain every child and must not overlap siblings.
            if let Some(parent) = parents.get(&bridge.pci_address.bus)
                && (bridge.secondary_bus < parent.secondary_bus
                    || bridge.subordinate_bus > parent.subordinate_bus)
            {
                return Err(invalid());
            }
            for sibling in &config.bridges {
                if sibling.pci_address.bus == bridge.pci_address.bus
                    && sibling.pci_address != bridge.pci_address
                    && bridge.secondary_bus <= sibling.subordinate_bus
                    && sibling.secondary_bus <= bridge.subordinate_bus
                {
                    return Err(invalid());
                }
            }
            buses.insert(path, bridge.secondary_bus);
        }
        let mut paths = BTreeMap::new();
        for dev in devices {
            let address = dev
                .pci_address
                .ok_or("every VFIO device requires pci_address")?;
            address.validate()?;
            if !addresses.insert(address) {
                return Err("duplicate PCI address".into());
            }
            let mut path = bus_path(address.bus)?;
            path.push(address.device);
            if dev.id.is_empty() || paths.insert(dev.id.clone(), path).is_some() {
                return Err("duplicate or empty VFIO id".into());
            }
        }
        Ok(Self {
            config: config.clone(),
            buses,
            paths,
        })
    }

    /// Resolve a previously validated path to its configured endpoint address.
    pub fn address(&self, path: &[u8]) -> PciSBDF {
        let bus = if path.len() == 2 {
            path[0]
        } else {
            self.buses[&path[..path.len() - 1]]
        };
        PciSBDF::new(0, bus, *path.last().unwrap(), 0)
    }

    /// Create exactly the configured buses and bridges with allocated BAR windows.
    pub fn install(
        &self,
        buses: &PciBuses,
        ranges: &[(Vec<u8>, u64, u64, bool)],
    ) -> Result<(), PciBusError> {
        for bus in self
            .config
            .roots
            .iter()
            .flat_map(|r| [r.bus, r.last_bus])
            .chain(self.config.bridges.iter().map(|b| b.secondary_bus))
        {
            buses.add_bus(bus)?;
        }
        for bridge in &self.config.bridges {
            let path = self
                .buses
                .iter()
                .find(|(_, bus)| **bus == bridge.secondary_bus)
                .unwrap()
                .0;
            let window = |prefetch| {
                let selected: Vec<_> = ranges
                    .iter()
                    .filter(|(leaf, _, _, pref)| leaf.starts_with(path) && *pref == prefetch)
                    .collect();
                Some((
                    selected.iter().map(|r| r.1).min()?,
                    selected.iter().map(|r| r.1 + r.2 - 1).max()?,
                ))
            };
            let device = Arc::new(Mutex::new(StaticBridge::new(
                bridge.pci_address.bus,
                bridge.secondary_bus,
                bridge.subordinate_bus,
                bridge.kind as u8,
                window(false),
                window(true),
            )));
            buses
                .get(bridge.pci_address.bus)
                .ok_or(PciBusError::InvalidBusNumber(bridge.pci_address.bus))?
                .lock()
                .unwrap()
                .add_device(bridge.pci_address.device, device)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmm_config::pci_topology::*;
    use crate::vmm_config::vfio::VfioDeviceConfig;

    fn fixture() -> (PciTopologyConfig, Vec<VfioDeviceConfig>) {
        let config = serde_json::from_value(serde_json::json!({
            "roots":[{"bus":40,"last_bus":50,"numa_node":0},{"bus":0,"last_bus":12,"numa_node":1},{"bus":20,"last_bus":29,"numa_node":0}],
            "bridges":[
                {"pci_address":{"bus":7,"device":0,"function":0},"secondary_bus":9,"subordinate_bus":9,"type":"upstream-port"},
                {"pci_address":{"bus":0,"device":13,"function":0},"secondary_bus":7,"subordinate_bus":9,"type":"root-port"},
                {"pci_address":{"bus":20,"device":13,"function":0},"secondary_bus":25,"subordinate_bus":25,"type":"root-port"}
            ]
        })).unwrap();
        let devices = vec![VfioDeviceConfig {
            id: "gpu".into(),
            path: "/unavailable/host/topology".into(),
            pci_address: Some(PciAddress {
                bus: 9,
                device: 3,
                function: 0,
            }),
        }];
        (config, devices)
    }

    #[test]
    fn static_topology_preserves_sparse_explicit_graph_independent_of_numa() {
        let (mut config, devices) = fixture();
        let first = StaticTopology::new(&config, &devices, 2).unwrap();
        let root = Arc::new(PciBuses::new(0));
        first.install(&root, &[]).unwrap();

        let port = root
            .get(0)
            .unwrap()
            .lock()
            .unwrap()
            .get_device(13)
            .unwrap()
            .lock()
            .unwrap()
            .read_config_register(6);
        assert_eq!(port, 0x00090700);
        let bus = root.get(7).unwrap().lock().unwrap();
        let upstream = bus.get_device(0).unwrap();
        assert_eq!(upstream.lock().unwrap().read_config_register(6), 0x00090907);
        assert_eq!(
            (upstream.lock().unwrap().read_config_register(0x40 / 4) >> 20) & 15,
            5
        );
        assert!(root.get(1).is_none());
        assert_eq!(first.address(&first.paths["gpu"]), PciSBDF::new(0, 9, 3, 0));
        drop(bus);
        for r in &mut config.roots {
            r.numa_node = Some(0);
        }
        config.roots.reverse();
        config.bridges.reverse();
        let second = StaticTopology::new(&config, &devices, 1).unwrap();
        assert_eq!(first.buses, second.buses);
        assert_eq!(first.paths, second.paths);
    }

    #[test]
    fn static_topology_rejects_invalid_graphs_and_missing_addresses() {
        let (config, devices) = fixture();
        let bad: [fn(&mut PciTopologyConfig); 12] = [
            |c| c.roots[0].bus = 10, // overlapping roots
            |c| c.roots[1].bus = 1,  // missing root zero
            |c| c.roots[0].numa_node = Some(2),
            |c| c.roots[0].last_bus = 64,
            |c| c.bridges[0].pci_address.bus = 8, // disconnected
            |c| c.bridges[0].secondary_bus = 7,   // duplicate secondary bus
            |c| c.bridges[0].subordinate_bus = 10, // escapes parent
            |c| c.bridges[1].pci_address.bus = 9, // cycle
            |c| c.bridges[2].secondary_bus = 40,  // collides with root
            |c| c.bridges[0].pci_address.function = 1,
            |c| c.bridges[1].pci_address.device = 0,
            |c| c.bridges.push(c.bridges[1].clone()),
        ];
        for change in bad {
            let mut c = config.clone();
            change(&mut c);
            StaticTopology::new(&c, &devices, 2).unwrap_err();
        }
        for address in [
            None,
            Some(PciAddress {
                bus: 8,
                device: 0,
                function: 0,
            }),
            Some(PciAddress {
                bus: 7,
                device: 0,
                function: 0,
            }),
            Some(PciAddress {
                bus: 9,
                device: 32,
                function: 0,
            }),
        ] {
            let mut d = devices.clone();
            d[0].pci_address = address;
            StaticTopology::new(&config, &d, 2).unwrap_err();
        }
        let mut duplicate = devices.clone();
        duplicate.push(devices[0].clone());
        StaticTopology::new(&config, &duplicate, 2).unwrap_err();
        serde_json::from_str::<VfioDeviceConfig>(r#"{"id":"gpu","path":"/host","pci_path":[1,0]}"#)
            .unwrap_err();
    }

    #[test]
    fn static_topology_ecam_io_and_fixed_bridge_registers() {
        use crate::pci::bus::{PciConfigIo, PciConfigMmio};
        use crate::vstate::bus::BusDevice;
        let (config, devices) = fixture();
        let plan = StaticTopology::new(&config, &devices, 2).unwrap();
        let root = Arc::new(PciBuses::new(0));
        plan.install(&root, &[]).unwrap();
        let mut ecam = PciConfigMmio::new(root.clone());
        let mut io = PciConfigIo::new(root.clone());
        let mut data = [0; 4];
        ecam.read(0, 7 << 20, &mut data);
        assert_eq!(data, [0x0f, 0x1d, 1, 2]);
        io.write(0, 0, &(0x80000000u32 | (7 << 16)).to_le_bytes());
        assert_eq!(io.config_space_read(), u32::from_le_bytes(data));
        ecam.read(0, (7 << 20) | (1 << 12), &mut data);
        assert_eq!(data, [0xff; 4]);
        ecam.write(0, (7 << 20) | 0x18, &[0; 4]);
        ecam.read(0, (7 << 20) | 0x18, &mut data);
        assert_eq!(data, [7, 9, 9, 0]);
    }

    #[test]
    fn static_topology_materializes_json_configuration() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Contract {
            #[serde(rename = "pci-topology")]
            topology: PciTopologyConfig,
            vfio: Vec<VfioDeviceConfig>,
            numa: crate::numa::NumaConfig,
        }
        let data = include_str!("../../../../../docs/fixtures/pci-topology.json");
        let contract: Contract = serde_json::from_str(data).unwrap();
        contract.numa.validate(3, 10).unwrap();
        let plan = StaticTopology::new(
            &contract.topology,
            &contract.vfio,
            contract.numa.nodes.len(),
        )
        .unwrap();
        let root = Arc::new(PciBuses::new(0));
        plan.install(&root, &[]).unwrap();
        assert_eq!(
            plan.config
                .roots
                .iter()
                .map(|r| (r.bus, r.last_bus, r.numa_node))
                .collect::<Vec<_>>(),
            vec![(0, 4, Some(0)), (5, 8, Some(1))]
        );
        for (i, device) in contract.vfio.iter().enumerate() {
            let address = plan.address(&plan.paths[&device.id]);
            assert_eq!(address.bus(), [3, 4, 8][i]);
            assert_eq!(address.device(), 0);
        }
        let bus = root.get(5).unwrap().lock().unwrap();
        let bridge = bus.get_device(14).unwrap();
        assert_eq!(bridge.lock().unwrap().read_config_register(6), 0x00080605);
    }
}
