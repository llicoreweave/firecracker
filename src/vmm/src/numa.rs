// SPDX-License-Identifier: Apache-2.0
//! Explicit cold-boot NUMA topology and host placement.
use std::collections::BTreeSet;

use acpi_tables::Sdt;
use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress, GuestMemory};

use crate::vstate::memory::{GuestRegionMmap, MemoryError};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// Explicit guest NUMA layout and host placement.
pub struct NumaConfig {
    /// Ordered proximity domains, starting at zero.
    pub nodes: Vec<NumaNode>,
    /// Row-major SLIT distances, including diagonal entries.
    pub distances: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
/// A single guest proximity domain.
pub struct NumaNode {
    /// Physical host NUMA node (this prototype supports IDs below 64).
    pub host_node: u32,
    /// Guest APIC/vCPU IDs.
    pub vcpus: Vec<u8>,
    /// One physical host CPU for each corresponding vCPU.
    pub host_cpus: Vec<u32>,
    /// Guest RAM assigned to this domain, in MiB.
    pub memory_mib: usize,
    /// Root-bus slots whose PCI subtrees belong to this domain.
    pub pci_root_slots: Vec<u8>,
}

impl NumaConfig {
    /// Validate before allocating memory or launching any vCPU.
    pub fn validate(&self, cpus: u8, memory_mib: usize) -> Result<(), String> {
        let n = self.nodes.len();
        if n == 0 || n > 64 || self.distances.len() != n || memory_mib > (usize::MAX >> 20) {
            return Err("NUMA requires 1..=64 nodes and a square distance matrix".into());
        }
        let mut seen = BTreeSet::new();
        let mut hosts = BTreeSet::new();
        let mut slots = BTreeSet::new();
        let mut memory = 0usize;
        for (id, node) in self.nodes.iter().enumerate() {
            if node.host_node >= 64
                || node.vcpus.is_empty()
                || node.vcpus.len() != node.host_cpus.len()
                || node.memory_mib == 0
                || node.memory_mib % 2 != 0
                || node.memory_mib > (usize::MAX >> 20)
                || self.distances[id].len() != n
                || self.distances[id][id] != 10
            {
                return Err(format!("invalid NUMA node {id}"));
            }
            memory = memory
                .checked_add(node.memory_mib)
                .ok_or("NUMA memory overflow")?;
            for (&cpu, &host) in node.vcpus.iter().zip(&node.host_cpus) {
                if cpu >= cpus
                    || host >= libc::CPU_SETSIZE as u32
                    || !seen.insert(cpu)
                    || !hosts.insert(host)
                {
                    return Err("NUMA CPUs must be distinct and cover the configured vCPUs".into());
                }
            }
            for &slot in &node.pci_root_slots {
                if slot == 0 || slot > 31 || !slots.insert(slot) {
                    return Err("invalid or duplicate NUMA PCI root slot".into());
                }
            }
            if self.distances[id]
                .iter()
                .enumerate()
                .any(|(j, &d)| j != id && d <= 10)
            {
                return Err("remote NUMA distance must exceed local distance".into());
            }
        }
        if seen.len() != cpus as usize || memory != memory_mib {
            return Err("NUMA CPU/memory totals do not match machine configuration".into());
        }
        Ok(())
    }

    /// Split RAM ranges at NUMA boundaries without counting the PCI hole as RAM.
    pub fn regions(
        &self,
        ranges: &[(GuestAddress, usize)],
    ) -> Result<Vec<(GuestAddress, usize, usize)>, String> {
        let expected = self
            .nodes
            .iter()
            .try_fold(0usize, |total, node| {
                node.memory_mib
                    .checked_mul(1 << 20)
                    .and_then(|size| total.checked_add(size))
            })
            .ok_or("NUMA memory overflow")?;
        let actual = ranges
            .iter()
            .try_fold(0usize, |total, &(base, length)| {
                base.0.checked_add(length as u64)?;
                total.checked_add(length)
            })
            .ok_or("guest RAM range overflow")?;
        if self.nodes.is_empty()
            || self.nodes.iter().any(|n| n.memory_mib == 0)
            || actual != expected
        {
            return Err("guest RAM ranges do not match NUMA memory".into());
        }
        let mut out = Vec::new();
        let mut node = 0;
        let mut remaining = self.nodes[0].memory_mib << 20;
        for &(base, length) in ranges {
            let mut offset = 0;
            while offset < length {
                let size = remaining.min(length - offset);
                out.push((GuestAddress(base.0 + offset as u64), size, node));
                offset += size;
                remaining -= size;
                if remaining == 0 && node + 1 < self.nodes.len() {
                    node += 1;
                    remaining = self.nodes[node].memory_mib << 20;
                }
            }
        }
        Ok(out)
    }

    /// Install strict host memory policies before kernel/device DMA first-touch.
    pub fn bind_memory(
        &self,
        regions: &[GuestRegionMmap],
        layout: &[(GuestAddress, usize, usize)],
    ) -> Result<(), MemoryError> {
        if regions.len() != layout.len() {
            return Err(MemoryError::Numa(std::io::Error::other(
                "NUMA region count mismatch",
            )));
        }
        for (region, &(_, length, node)) in regions.iter().zip(layout) {
            let mask = 1u64 << self.nodes[node].host_node;
            // SAFETY: this live mmap is page aligned; mbind changes policy, not contents.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_mbind,
                    region.as_ptr(),
                    length,
                    2, /* MPOL_BIND */
                    &mask as *const u64,
                    64usize,
                    0usize,
                )
            };
            if rc != 0 {
                return Err(MemoryError::Numa(std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }

    /// Validate against the effective host cpuset. Never widen cgroup CPU access.
    pub fn validate_host_cpus(&self) -> std::io::Result<()> {
        // SAFETY: zero is a valid initial cpu_set_t and its size is supplied correctly.
        let mut allowed: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: allowed is a writable cpu_set_t and its exact size is supplied.
        if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&allowed), &mut allowed) } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        for host in self.nodes.iter().flat_map(|n| &n.host_cpus) {
            // SAFETY: host CPU IDs have been checked against CPU_SETSIZE.
            if !unsafe { libc::CPU_ISSET(*host as usize, &allowed) } {
                return Err(std::io::Error::other(format!(
                    "NUMA host CPU {host} outside effective cpuset"
                )));
            }
        }
        for node in &self.nodes {
            let directory = format!("/sys/devices/system/node/node{}", node.host_node);
            for cpu in &node.host_cpus {
                if !std::path::Path::new(&directory)
                    .join(format!("cpu{cpu}"))
                    .exists()
                {
                    return Err(std::io::Error::other(format!(
                        "host CPU {cpu} does not belong to NUMA node {}",
                        node.host_node
                    )));
                }
            }
        }
        Ok(())
    }

    /// Assigned host CPU for a guest vCPU.
    pub fn host_cpu(&self, vcpu: u8) -> Option<u32> {
        self.nodes.iter().find_map(|n| {
            n.vcpus
                .iter()
                .position(|&v| v == vcpu)
                .map(|i| n.host_cpus[i])
        })
    }

    /// ACPI 6.x CPU/memory affinity and locality tables.
    pub fn tables(&self, layout: &[(GuestAddress, usize, usize)]) -> (NumaTable, NumaTable) {
        let mut srat = Vec::new();
        srat.extend(1u32.to_le_bytes());
        srat.extend(0u64.to_le_bytes());
        for (domain, node) in self.nodes.iter().enumerate() {
            for &cpu in &node.vcpus {
                srat.extend([0, 16, u8::try_from(domain).unwrap(), cpu]);
                srat.extend(1u32.to_le_bytes());
                srat.extend([0, 0, 0, 0]);
                srat.extend(0u32.to_le_bytes());
            }
        }
        for &(base, length, domain) in layout {
            srat.extend([1, 40]);
            srat.extend(u32::try_from(domain).unwrap().to_le_bytes());
            srat.extend(0u16.to_le_bytes());
            srat.extend(base.0.to_le_bytes());
            srat.extend((length as u64).to_le_bytes());
            srat.extend(0u32.to_le_bytes());
            srat.extend(1u32.to_le_bytes());
            srat.extend(0u64.to_le_bytes());
        }
        let mut slit = (self.nodes.len() as u64).to_le_bytes().to_vec();
        slit.extend(self.distances.iter().flatten());
        (
            NumaTable::new(b"SRAT", 3, srat),
            NumaTable::new(b"SLIT", 1, slit),
        )
    }
}

/// Byte-encoded ACPI table with a standard checksummed 36-byte header.
#[derive(Debug)]
pub struct NumaTable(Vec<u8>);
impl NumaTable {
    fn new(signature: &[u8; 4], revision: u8, body: Vec<u8>) -> Self {
        let mut bytes = Vec::new();
        bytes.extend(signature);
        bytes.extend(u32::try_from(36 + body.len()).unwrap().to_le_bytes());
        bytes.extend([revision, 0]);
        bytes.extend(b"FIRECKFCVMNUMA");
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(b"FCAT");
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(body);
        bytes[9] = 0u8.wrapping_sub(bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
        Self(bytes)
    }
}
impl Sdt for NumaTable {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn write_to_guest<M: GuestMemory>(
        &mut self,
        mem: &M,
        address: GuestAddress,
    ) -> acpi_tables::Result<()> {
        mem.write_slice(&self.0, address)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> NumaConfig {
        NumaConfig {
            nodes: (0..2)
                .map(|id| NumaNode {
                    host_node: id,
                    vcpus: (id * 32..(id + 1) * 32)
                        .map(|c| u8::try_from(c).unwrap())
                        .collect(),
                    host_cpus: (0..32).map(|c| c * 2 + id).collect(),
                    memory_mib: 524288,
                    pci_root_slots: if id == 0 {
                        vec![13, 14, 15, 16]
                    } else {
                        vec![17, 18, 19, 20, 21]
                    },
                })
                .collect(),
            distances: vec![vec![10, 21], vec![21, 10]],
        }
    }
    #[test]
    fn test_numa_layout_and_acpi() {
        let config = config();
        config.validate(64, 1048576).unwrap();
        let ranges = crate::arch::arch_memory_regions(1usize << 40);
        let layout = config.regions(&ranges).unwrap();
        assert_eq!(layout.len(), 3);
        for id in 0..2 {
            assert_eq!(
                layout
                    .iter()
                    .filter(|r| r.2 == id)
                    .map(|r| r.1)
                    .sum::<usize>(),
                1usize << 39
            );
        }
        let (srat, slit) = config.tables(&layout);
        assert_eq!(srat.len(), 48 + 64 * 16 + 3 * 40);
        assert_eq!(slit.len(), 48);
        for table in [srat, slit] {
            assert_eq!(table.0.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
            assert_eq!(
                u32::from_le_bytes(table.0[4..8].try_into().unwrap()) as usize,
                table.len()
            );
        }
        assert_eq!(config.host_cpu(31), Some(62));
        assert_eq!(config.host_cpu(32), Some(1));
    }
    #[test]
    fn test_numa_reject_invalid_layout() {
        let mut c = config();
        c.nodes[1].vcpus[0] = 0;
        c.validate(64, 1048576).unwrap_err();
        c = config();
        c.nodes[1].memory_mib -= 2;
        c.validate(64, 1048576).unwrap_err();
        c = config();
        c.nodes[1].pci_root_slots.push(13);
        c.validate(64, 1048576).unwrap_err();
    }

    #[test]
    fn test_numa_arbitrary_node_count_and_sparse_host_ids() {
        let config = NumaConfig {
            nodes: (0..3)
                .map(|id| NumaNode {
                    host_node: id * 2,
                    vcpus: vec![u8::try_from(id).unwrap()],
                    host_cpus: vec![id],
                    memory_mib: 2,
                    pci_root_slots: vec![],
                })
                .collect(),
            distances: vec![vec![10, 21, 31], vec![21, 10, 21], vec![31, 21, 10]],
        };
        config.validate(3, 6).unwrap();
        let layout = config.regions(&[(GuestAddress(0), 6 << 20)]).unwrap();
        assert_eq!(
            layout,
            vec![
                (GuestAddress(0), 2 << 20, 0),
                (GuestAddress(2 << 20), 2 << 20, 1),
                (GuestAddress(4 << 20), 2 << 20, 2)
            ]
        );
        let (_, slit) = config.tables(&layout);
        assert_eq!(&slit.0[44..], &[10, 21, 31, 21, 10, 21, 31, 21, 10]);
    }

    #[test]
    fn test_numa_regions_reject_mismatched_and_overflowing_ram() {
        let c = config();
        c.regions(&[(GuestAddress(0), 1 << 40 | 1)]).unwrap_err();
        c.regions(&[(GuestAddress(0), 1 << 39)]).unwrap_err();
        c.regions(&[(GuestAddress(u64::MAX), 1 << 40)]).unwrap_err();
        let mut empty = c;
        empty.nodes.clear();
        empty.regions(&[]).unwrap_err();
        empty.validate(0, 0).unwrap_err();
    }

    #[test]
    fn test_numa_rejects_invalid_host_cpus_distances_and_memory() {
        for invalid in 0..5 {
            let mut c = config();
            match invalid {
                0 => c.nodes[1].host_cpus[0] = c.nodes[0].host_cpus[0],
                1 => c.nodes[1].host_node = 64,
                2 => c.distances[0][1] = 10,
                3 => c.distances[0].clear(),
                _ => c.nodes[0].memory_mib = usize::MAX - 1,
            }
            c.validate(64, 1048576).unwrap_err();
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn test_numa_resources_revalidate_preboot_updates() {
        use crate::resources::VmResources;
        let mut resources = VmResources {
            balloon: crate::vmm_config::balloon::BalloonBuilder::new(),
            pci_enabled: true,
            ..Default::default()
        };
        resources.machine_config.vcpu_count = 64;
        resources.machine_config.mem_size_mib = 1048576;
        resources.numa = Some(config());
        resources.validate_numa().unwrap();
        resources.machine_config.mem_size_mib += 2;
        resources.validate_numa().unwrap_err();
        resources.machine_config.mem_size_mib -= 2;
        resources.pci_enabled = false;
        resources.validate_numa().unwrap_err();
        resources.pci_enabled = true;
        resources
            .vfio
            .configs
            .push(crate::vmm_config::vfio::VfioDeviceConfig::default());
        resources.validate_numa().unwrap_err();
    }

    /// Opt-in hardware check, run inside a container on the two-node lli host.
    #[test]
    #[ignore = "requires a two-node NUMA host; run explicitly in the node container"]
    fn test_numa_host_memory_binding() {
        use vm_memory::GuestMemoryRegion;
        let c = NumaConfig {
            nodes: (0..2)
                .map(|id| NumaNode {
                    host_node: id,
                    vcpus: vec![u8::try_from(id).unwrap()],
                    host_cpus: vec![id],
                    memory_mib: 2,
                    pci_root_slots: vec![],
                })
                .collect(),
            distances: vec![vec![10, 21], vec![21, 10]],
        };
        c.validate(2, 4).unwrap();
        c.validate_host_cpus().unwrap();
        let layout = c.regions(&[(GuestAddress(0), 4 << 20)]).unwrap();
        let ranges: Vec<_> = layout.iter().map(|r| (r.0, r.1)).collect();
        let memory = crate::vstate::memory::anonymous(
            &ranges,
            false,
            crate::vmm_config::machine_config::HugePageConfig::None,
        )
        .unwrap();
        c.bind_memory(&memory, &layout).unwrap();
        for region in &memory {
            // SAFETY: each region owns a writable mapping of exactly len bytes.
            unsafe {
                std::ptr::write_bytes(region.as_ptr(), 1, usize::try_from(region.len()).unwrap())
            };
        }
        let maps = std::fs::read_to_string("/proc/self/numa_maps").unwrap();
        for (domain, region) in memory.iter().enumerate() {
            let address = format!("{:x} ", region.as_ptr() as usize);
            let line = maps
                .lines()
                .find(|line| line.starts_with(&address))
                .unwrap();
            assert!(line.contains(&format!("bind:{domain}")), "{line}");
            assert!(line.contains(&format!("N{domain}=512")), "{line}");
            eprintln!("{line}");
        }
    }
}
