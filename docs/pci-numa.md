# PCI passthrough with NUMA locality

Start Firecracker with `--enable-pci --config-file <file>`. The caller owns all
topology policy. Firecracker consumes explicit guest PCI roots, bridges, port
types, bus registers, endpoint BDFs and root NUMA affinity. It never reads host
PCI ancestry or locality to choose any of these settings.

```json
{
  "machine-config": {
    "vcpu_count": 2,
    "mem_size_mib": 2048,
    "pci_mmio64": {"base_gib": 2048, "size_gib": 2048}
  },
  "numa": {
    "nodes": [
      {"host_node": 0, "vcpus": [0], "host_cpus": [0], "memory_mib": 1024},
      {"host_node": 1, "vcpus": [1], "host_cpus": [1], "memory_mib": 1024}
    ],
    "distances": [[10, 21], [21, 10]]
  },
  "pci-topology": {
    "roots": [
      {"bus": 0, "last_bus": 9, "numa_node": 0},
      {"bus": 20, "last_bus": 29, "numa_node": 1}
    ],
    "bridges": [
      {"pci_address": {"bus": 0, "device": 13, "function": 0}, "secondary_bus": 7, "subordinate_bus": 7, "type": "root-port"},
      {"pci_address": {"bus": 20, "device": 13, "function": 0}, "secondary_bus": 25, "subordinate_bus": 25, "type": "root-port"}
    ]
  },
  "device-passthrough": [
    {"id": "gpu0", "sbdf": "0000:a0:00.0", "peer_dma": true, "pci_address": {"bus": 7, "device": 0, "function": 0}},
    {"id": "gpu1", "sbdf": "0000:b0:00.0", "peer_dma": true, "pci_address": {"bus": 25, "device": 0, "function": 0}}
  ]
}
```

Merge this fragment with the VM's boot source, drives and other configuration.
The generator must supply actual host CPU IDs and distances and allocate devices
exclusively, already bound to `vfio-pci`. Guest bus numbers can differ from host
BDFs. In this example the guest endpoints are `0000:07:00.0` and `0000:19:00.0`.
Sparse bus ranges and multiple PCI roots with the same NUMA affinity are
allowed; reordering roots or changing affinity never renumbers any device.

`roots[].numa_node` is the guest domain index in `numa.nodes`, not `host_node`.
The VMM emits it directly as the ACPI PCI root's `_PXM`. When omitted, that root
has no `_PXM`. Root identity and `_BBN` come from its configured bus number.
[NUMA configuration](numa.md) and ACPI SRAT/SLIT describe the independently
configured CPU/memory domains. Linux derives subordinate endpoint locality
through its PCI root bridge; a root-port `_PXM` alone does not establish
endpoint locality on x86 Linux.

Every bridge explicitly supplies its guest address (including primary bus),
secondary bus, subordinate bus and PCIe port type. No missing bridge or endpoint
address is generated. Invalid addresses, duplicate BDFs, cycles, disconnected
buses, overlapping roots/windows, out-of-range domains and exhausted ECAM ranges
fail validation before VFIO devices are opened. Without `pci-topology`, upstream
flat placement and PCIe hotplug keep their existing behavior. An explicit guest
address requires a static graph.

The VMM still probes device BAR requirements and allocates MMIO resources within
the configured graph, including disjoint ACPI root windows and 1-MiB bridge
window granularity. This resource allocation does not choose PCI connections,
bus numbers, port types or locality. The caller also chooses
[`machine-config.pci_mmio64`](pci-mmio64.md) to fit the assigned BARs. The
example reserves a 2 TiB aperture starting at 2 TiB and requires at least 43
physical address bits, including the following pmem/hotplug pool. Omitting the
setting preserves the default 256 GiB aperture at 256 GiB; exhaustion fails
startup. CF8/CFC and ECAM both decode secondary buses.

For jailed VMs, pass `--numa-node <host-id>` for each configured host node. The
runtime supplies the assigned VFIO device nodes and the selected device's
`iommu_group` link as required by upstream passthrough. The NUMA option copies
only CPU-membership metadata for affinity validation; host PCI sysfs and
locality policy stay outside the VMM.

RAM ranges split at NUMA boundaries without counting PCI holes as RAM. Each
mapping receives `MPOL_BIND` before first touch and VFIO pinning. Each vCPU
thread pins to its corresponding host CPU before entering KVM. CPU locality,
effective affinity, RAM totals, and the explicit PCI graph are validated. Host
CPU pinning does not reserve those CPUs exclusively.

## Peer DMA and interrupts

Set `device-passthrough[].peer_dma: true` on endpoints whose large prefetchable
BARs must be visible to other assigned devices. This requires a static graph.
All devices in that graph then share rust-vmm's VFIO cdev/IOMMUFD backend; only
flagged endpoints export prefetchable 64-bit BARs of at least 1 GiB. Without the
flag, upstream's legacy VFIO container path is retained. The VMM does not infer
peer-DMA policy from host PCI ancestry or NUMA placement.

The runtime must expose `/dev/iommu`, the assigned `/dev/vfio/devices/vfioN`
character devices, and their selected sysfs `vfio-dev` entries in the jail.
These replace the group-device paths for this mode. Device ownership remains the
runtime's responsibility. Peer DMA requires compatible host DMA-BUF and IOMMUFD
support; startup fails if an explicitly requested export/import cannot be made.
BAR addresses stay fixed while peer DMA is configured. Memory decode disable
removes a device's imports, and re-enable recreates them; a remapping failure
stops the VM. All assigned DMA initiators stop before peer mappings are torn
down.

Devices exposing MSI-X keep upstream's interrupt model. Devices that expose only
MSI, including some PCIe switch control functions, use emulated MSI with bounded
multiple messages and per-vector masks. Legacy INTx is not supported. A boot
check alone does not validate GPU peer transfers.

## Runtime integration

A container runtime or another topology generator must produce the complete
configuration before launching Firecracker. It chooses device ownership, CPU and
memory placement, PCI roots, bridge types, bus ranges, endpoint addresses and
the MMIO64 aperture. Firecracker validates and materializes that configuration.

The [JSON fixture](fixtures/pci-topology.json) illustrates two guest NUMA nodes,
a PCIe switch hierarchy and three endpoints using synthetic host addresses.
Replace host device and CPU identifiers with resources allocated to the VM.

Static PCI topology cannot be combined with `pcie_hotplug_ports`. NUMA and
static PCI topology currently require x86_64 KVM and do not support snapshots;
NUMA also excludes ballooning, memory hotplug and memfd-backed memory.
