# PCI passthrough with NUMA locality

This implementation extends the GPU-capable VFIO branch at
`5aa6bd2619d4769c824eb6d65a49bee476cb8a32`. It uses the branch's `vfio`
configuration API. It is a local implementation of HAVOCK-2250, building on
the October 21 topology and DMA-BUF prototype.

Start Firecracker with `--enable-pci --config-file <file>`. Add an explicit
`numa` object and a `pci_path` for each assigned function:

```json
{
  "machine-config": {
    "vcpu_count": 2,
    "mem_size_mib": 2048,
    "huge_pages": "Transparent"
  },
  "numa": {
    "nodes": [
      {"host_node": 0, "vcpus": [0], "host_cpus": [0], "memory_mib": 1024, "pci_root_slots": [13]},
      {"host_node": 1, "vcpus": [1], "host_cpus": [1], "memory_mib": 1024, "pci_root_slots": [14]}
    ],
    "distances": [[10, 21], [21, 10]]
  },
  "vfio": [
    {"id": "gpu0", "path": "/sys/bus/pci/devices/0000:19:00.0", "pci_path": [13, 0]},
    {"id": "gpu1", "path": "/sys/bus/pci/devices/0000:83:00.0", "pci_path": [14, 0]}
  ]
}
```

Merge this fragment with the VM's boot source, drives and other configuration.
Host CPU IDs, PCI addresses and distances must match the actual host. Devices
must already be allocated to the workload and bound to `vfio-pci`.

For jailed VMs, pass `--numa-node <host-id>` to the jailer for each configured
host node, alongside the existing `--vfio-device` arguments. Kata emits these
arguments automatically. The jailer copies only the requested CPU membership
and assigned devices' `numa_node` values; host sysfs remains outside the jail.

Guest domains are numbered by `nodes` order. The VMM emits ACPI SRAT/SLIT and
one ACPI PCI host bridge per domain, with `_PXM`, `_BBN` and disjoint `_CRS`
resources. Linux consequently assigns endpoint locality through its PCI root
bridge, including devices below multiple switch buses. Root-port `_PXM` alone
does not establish endpoint locality on x86 Linux.

`pci_path` lists bridge slots followed by an endpoint slot. For example,
`[13,0,1,0]` creates a root port, upstream port, downstream port and endpoint.
The VMM assigns buses deterministically by domain and lexicographic bridge
prefix. Each additional domain consumes a root bus, including empty domains.
CF8/CFC and ECAM accesses both decode secondary buses. The example above gives
roots 00 and 02, with endpoints 01:00.0 and 03:00.0 in guest segment 0000.

BAR placement reserves separate windows for sibling subtrees, respects 1-MiB
bridge window granularity, packs naturally aligned BARs, and allocates domains
consecutively. The x86 MMIO64 aperture spans 2 TiB starting at 2 TiB. Exhaustion
fails VM startup. Original host BDF numbering is not reproduced.

RAM ranges split at NUMA boundaries without counting PCI holes as RAM. Each
mapping receives `MPOL_BIND` before first touch and VFIO pinning. Each vCPU
thread pins to its corresponding host CPU before entering KVM. CPU locality,
effective affinity, device locality, RAM totals, and PCI root coverage are
validated. Host CPU pinning does not reserve those CPUs exclusively.

The static passthrough mode also retains IOMMUFD/VFIO DMA-BUF BAR export/import
for peer DMA, without disabling the host IOMMU or ACS. This requires compatible
host VFIO/IOMMUFD support and guest GPU/RDMA drivers. A successful VM boot alone
does not establish successful GPU peer DMA.

## Kata integration

The accompanying Kata checkout enables this configuration only with explicit
`enable_numa`, `numa_mapping` and `cold_plug_vfio = "switch-port"` settings.
It divides guest CPU/RAM evenly among configured domains, reads device ancestry
and locality from sysfs, and uses the host NUMA distance matrix. The guest agent
parses the selected `pciDDDD:BB` root when resolving device paths.

## Limits

- x86_64 cold boot; up to 64 vCPUs, 64 guest buses and host node IDs below 64.
- Guest PCI functions use function zero. Paths have 2–8 slots; root slot zero
  is reserved for the host bridge.
- No bridge bus renumbering, BAR relocation, device hotplug, memory hotplug,
  ballooning or NUMA snapshots. Unsupported combinations fail validation.
- Kata's existing PCI path prefix type limits root bus prefixes to 00–1f;
  an unrepresentable root fails configuration before starting the VM.
- Existing flat VFIO configurations can omit both `numa` and `pci_path`.

All build dependencies and tests run in Linux containers. The hardware-only
`numa::tests::test_numa_host_memory_binding` test explicitly verifies both
memory policy and resident pages on a two-node host.
