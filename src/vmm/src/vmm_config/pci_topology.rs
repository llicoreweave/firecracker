// SPDX-License-Identifier: Apache-2.0
//! Caller-owned cold-boot PCI layout. All bus numbering and locality are explicit.
use serde::{Deserialize, Serialize};

/// Segment-zero, function-zero guest PCI address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PciAddress {
    /// Guest bus number.
    pub bus: u8,
    /// Guest slot number.
    pub device: u8,
    /// Currently only function zero is supported.
    pub function: u8,
}
impl PciAddress {
    /// Check the emulated ECAM aperture and function limits.
    pub fn validate(&self) -> Result<(), String> {
        if self.bus >= 64
            || self.device >= 32
            || self.function != 0
            || (self.bus == 0 && self.device == 0)
        {
            return Err("invalid guest PCI address".into());
        }
        Ok(())
    }
}

/// An ACPI PCI host bridge, independent of the order of NUMA domains.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PciRootConfig {
    /// First bus in this host bridge's range.
    pub bus: u8,
    /// Inclusive last bus.
    pub last_bus: u8,
    /// Guest proximity domain for _PXM. Omitted when no affinity is requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
}

/// PCI Express port type, supplied by the topology generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum PciBridgeKind {
    /// Root port.
    RootPort = 4,
    /// Switch upstream port.
    UpstreamPort = 5,
    /// Switch downstream port.
    DownstreamPort = 6,
}

/// One bridge and its fixed bus forwarding registers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PciBridgeConfig {
    /// Address of this bridge, including its primary bus.
    pub pci_address: PciAddress,
    /// Secondary bus register.
    pub secondary_bus: u8,
    /// Inclusive subordinate bus register.
    pub subordinate_bus: u8,
    /// PCIe port type.
    #[serde(rename = "type")]
    pub kind: PciBridgeKind,
}

/// Complete graph of configured roots and bridges; VFIO endpoints carry addresses separately.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PciTopologyConfig {
    /// ACPI roots, including root bus zero.
    pub roots: Vec<PciRootConfig>,
    /// Explicit bridge connections. Input order is immaterial.
    pub bridges: Vec<PciBridgeConfig>,
}
