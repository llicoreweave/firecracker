// Copyright 2026 CoreWeave, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::static_topology::StaticTopology;
use crate::vfio::{BarRequirement, BarSlot, BarWindow};
use crate::vstate::resources::ResourceAllocator;
use std::collections::BTreeMap;

/// Choose the guest addresses of VFIO BARs, given as `(device, requirement)` pairs.
///
/// BARs are naturally aligned powers of two. Placing them largest first, each at the highest
/// suitable address of its window, packs them without fragmentation below the top of the window,
/// away from the virtio-pci BARs which are allocated from the bottom.
#[cfg(test)]
fn place_static_vfio_bars(
    allocator: &mut ResourceAllocator,
    requirements: Vec<(usize, BarRequirement)>,
    roots: &[u8],
) -> Result<Vec<(usize, BarSlot, u64)>, vm_allocator::Error> {
    let paths: Vec<_> = roots
        .iter()
        .enumerate()
        .map(|(i, root)| vec![*root, u8::try_from(i).unwrap()])
        .collect();
    place_grouped_vfio_bars(allocator, requirements, &paths, None)
}

struct SubtreeBars {
    size: u64,
    alignment: u64,
    entries: Vec<(usize, BarSlot, u64)>,
}

fn align_bar(value: u64, alignment: u64) -> Result<u64, vm_allocator::Error> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or(vm_allocator::Error::ResourceNotAvailable)
}

/// Plan bottom-up so sibling bridges never decode each other's BARs, including
/// the mandatory 1-MiB bridge window granularity. Only assigned endpoints enter
/// this tree; physical bridges and unassigned host functions are never opened.
fn subtree_bars(
    requirements: &[(usize, BarRequirement)],
    paths: &[Vec<u8>],
    depth: usize,
) -> Result<SubtreeBars, vm_allocator::Error> {
    let mut result = SubtreeBars {
        size: 0,
        alignment: 1 << 20,
        entries: Vec::new(),
    };
    if paths[requirements[0].0].len() == depth {
        let mut bars = requirements.to_vec();
        bars.sort_by_key(|(_, b)| std::cmp::Reverse(b.size));
        for (device, bar) in bars {
            result.alignment = result.alignment.max(bar.size);
            result.size = align_bar(result.size, bar.size)?;
            result.entries.push((device, bar.slot, result.size));
            result.size = result
                .size
                .checked_add(bar.size)
                .ok_or(vm_allocator::Error::ResourceNotAvailable)?;
        }
    } else {
        let mut children = BTreeMap::<u8, Vec<(usize, BarRequirement)>>::new();
        for &(device, bar) in requirements {
            children
                .entry(paths[device][depth])
                .or_default()
                .push((device, bar));
        }
        let mut layouts = children
            .values()
            .map(|bars| subtree_bars(bars, paths, depth + 1))
            .collect::<Result<Vec<_>, _>>()?;
        layouts.sort_by_key(|child| std::cmp::Reverse((child.alignment, child.size)));
        for child in layouts {
            result.alignment = result.alignment.max(child.alignment);
            let base = align_bar(result.size, child.alignment)?;
            result.size = base
                .checked_add(child.size)
                .ok_or(vm_allocator::Error::ResourceNotAvailable)?;
            result.entries.extend(
                child
                    .entries
                    .into_iter()
                    .map(|(dev, slot, offset)| (dev, slot, base + offset)),
            );
        }
    }
    result.size = align_bar(result.size, 1 << 20)?;
    Ok(result)
}

/// Reserve consecutive windows by configured root bus, even when root slots are
/// interleaved. Exact placement prevents filling a previous root's gaps.
pub(crate) fn place_grouped_vfio_bars(
    allocator: &mut ResourceAllocator,
    requirements: Vec<(usize, BarRequirement)>,
    paths: &[Vec<u8>],
    plan: Option<&StaticTopology>,
) -> Result<Vec<(usize, BarSlot, u64)>, vm_allocator::Error> {
    let mut groups = BTreeMap::<(u8, u8), Vec<(usize, BarRequirement)>>::new();
    for (device, bar) in requirements {
        let (bus, root) = if plan.is_some() {
            (paths[device][0], paths[device][1])
        } else {
            (0, paths[device][0])
        };
        groups.entry((bus, root)).or_default().push((device, bar));
    }
    let mut next = [None, None];
    let mut placed = Vec::new();
    for bars in groups.values() {
        for (index, kind) in [BarWindow::Mmio32, BarWindow::Mmio64]
            .into_iter()
            .enumerate()
        {
            let bars: Vec<_> = bars
                .iter()
                .filter(|(_, b)| b.window == kind)
                .copied()
                .collect();
            if bars.is_empty() {
                continue;
            }
            let layout = subtree_bars(&bars, paths, if plan.is_some() { 2 } else { 1 })?;
            let parent = match kind {
                BarWindow::Mmio32 => &mut allocator.mmio32_memory,
                BarWindow::Mmio64 => &mut allocator.mmio64_memory,
            };
            let policy = match next[index] {
                Some(cursor) => {
                    vm_allocator::AllocPolicy::ExactMatch(align_bar(cursor, layout.alignment)?)
                }
                None => vm_allocator::AllocPolicy::FirstMatch,
            };
            let range = parent.allocate(layout.size, layout.alignment, policy)?;
            next[index] = range.end().checked_add(1);
            placed.extend(
                layout
                    .entries
                    .into_iter()
                    .map(|(dev, slot, offset)| (dev, slot, range.start() + offset)),
            );
        }
    }
    Ok(placed)
}

/// ACPI root windows are disjoint and cover each bridge's 1-MiB decoding
/// granularity. Root zero owns the remaining aperture, including virtio BARs.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub(crate) fn pci_root_windows(
    plan: &StaticTopology,
    ranges: &[(Vec<u8>, u64, u64, bool)],
    root_bus: u8,
    prefetch: bool,
    aperture: (u64, u64),
) -> Vec<(u64, u64)> {
    let span = |root_bus: u8| {
        let selected: Vec<_> = ranges
            .iter()
            .filter(|(path, _, _, pref)| *pref == prefetch && path[0] == root_bus)
            .collect();
        Some((
            selected.iter().map(|r| r.1).min()? & !((1 << 20) - 1),
            selected.iter().map(|r| r.1 + r.2 - 1).max()? | ((1 << 20) - 1),
        ))
    };
    if root_bus != 0 {
        return span(root_bus).into_iter().collect();
    }
    let mut holes: Vec<_> = plan
        .config
        .roots
        .iter()
        .filter(|r| r.bus != 0)
        .filter_map(|r| span(r.bus))
        .collect();
    holes.sort_unstable();
    let mut result = Vec::new();
    let (mut next, end) = aperture;
    for (low, high) in holes {
        if next < low {
            result.push((next, low - 1));
        }
        next = high + 1;
    }
    if next <= end {
        result.push((next, end));
    }
    result
}

#[cfg(test)]
mod static_placement_tests {
    use super::*;

    #[test]
    fn static_placement_nested_siblings_and_interleaved_numa_roots() {
        use crate::vmm_config::pci_topology::*;
        let paths = vec![
            vec![0, 1, 0, 1, 0],
            vec![20, 2, 0],
            vec![10, 3, 0],
            vec![0, 1, 0, 2, 0],
            vec![0, 1, 0, 1, 1],
        ];
        let config = PciTopologyConfig {
            roots: vec![
                PciRootConfig {
                    bus: 0,
                    last_bus: 9,
                    numa_node: Some(2),
                },
                PciRootConfig {
                    bus: 20,
                    last_bus: 29,
                    numa_node: Some(0),
                },
                PciRootConfig {
                    bus: 10,
                    last_bus: 19,
                    numa_node: Some(0),
                },
            ],
            bridges: vec![
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 0,
                        device: 1,
                        function: 0,
                    },
                    secondary_bus: 1,
                    subordinate_bus: 4,
                    kind: PciBridgeKind::RootPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 1,
                        device: 0,
                        function: 0,
                    },
                    secondary_bus: 2,
                    subordinate_bus: 4,
                    kind: PciBridgeKind::UpstreamPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 2,
                        device: 1,
                        function: 0,
                    },
                    secondary_bus: 3,
                    subordinate_bus: 3,
                    kind: PciBridgeKind::DownstreamPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 2,
                        device: 2,
                        function: 0,
                    },
                    secondary_bus: 4,
                    subordinate_bus: 4,
                    kind: PciBridgeKind::DownstreamPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 20,
                        device: 2,
                        function: 0,
                    },
                    secondary_bus: 21,
                    subordinate_bus: 21,
                    kind: PciBridgeKind::RootPort,
                },
                PciBridgeConfig {
                    pci_address: PciAddress {
                        bus: 10,
                        device: 3,
                        function: 0,
                    },
                    secondary_bus: 11,
                    subordinate_bus: 11,
                    kind: PciBridgeKind::RootPort,
                },
            ],
        };
        let plan = StaticTopology::new(&config, &[], 3).unwrap();
        let requirements: Vec<_> = (0..paths.len())
            .map(|device| {
                (
                    device,
                    BarRequirement {
                        slot: BarSlot::Bar(0),
                        size: 4096,
                        window: BarWindow::Mmio64,
                    },
                )
            })
            .collect();
        let mut allocator = ResourceAllocator::new();
        let placed =
            place_grouped_vfio_bars(&mut allocator, requirements, &paths, Some(&plan)).unwrap();
        let address = |dev| placed.iter().find(|(d, _, _)| *d == dev).unwrap().2;
        // The neighboring downstream bridge must not decode either endpoint
        // on the first bridge's bus, even with interleaved configuration order.
        assert!(
            address(3) < address(0).min(address(4))
                || address(3) >= address(0).max(address(4)) + (1 << 20)
        );
        let ranges: Vec<_> = placed
            .iter()
            .map(|(dev, _, base)| (paths[*dev].clone(), *base, 4096, true))
            .collect();
        let windows: Vec<_> = [0, 10, 20]
            .into_iter()
            .map(|domain| {
                pci_root_windows(
                    &plan,
                    &ranges,
                    domain,
                    true,
                    (
                        allocator.mmio64_memory.base(),
                        allocator.mmio64_memory.end(),
                    ),
                )
            })
            .collect();
        for first in 0..3 {
            for second in first + 1..3 {
                for &(a, b) in &windows[first] {
                    for &(c, d) in &windows[second] {
                        assert!(b < c || d < a);
                    }
                }
            }
        }
        assert!(address(2) < address(1)); // Configured root bus order, independent of NUMA affinity.
    }

    #[test]
    fn static_placement_flat_multiple_roots_have_disjoint_bar_windows() {
        use crate::vmm_config::pci_topology::*;
        let config = PciTopologyConfig {
            roots: [0, 20, 40]
                .map(|bus| PciRootConfig {
                    bus,
                    last_bus: bus,
                    numa_node: None,
                })
                .to_vec(),
            bridges: vec![],
        };
        let plan = StaticTopology::new(&config, &[], 0).unwrap();
        let paths = vec![vec![0, 1], vec![20, 1], vec![40, 1]];
        let requirements: Vec<_> = [
            (0, 16 << 20),
            (0, 1 << 20),
            (1, 8 << 20),
            (1, 4 << 20),
            (2, 2 << 20),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (device, size))| {
            (
                device,
                BarRequirement {
                    slot: BarSlot::Bar(u8::try_from(i).unwrap()),
                    size,
                    window: BarWindow::Mmio64,
                },
            )
        })
        .collect();
        for (base, size) in [
            (crate::arch::MMIO64_MEM_START, crate::arch::MMIO64_MEM_SIZE),
            (16 << 30, 8 << 30),
            (2 << 40, 2 << 40),
        ] {
            let mut allocator = ResourceAllocator::with_mmio64_range(base, size).unwrap();
            let placed =
                place_grouped_vfio_bars(&mut allocator, requirements.clone(), &paths, Some(&plan))
                    .unwrap();
            let ranges: Vec<_> = placed
                .iter()
                .map(|(dev, slot, base)| {
                    (
                        paths[*dev].clone(),
                        *base,
                        requirements
                            .iter()
                            .find(|(d, b)| d == dev && b.slot == *slot)
                            .unwrap()
                            .1
                            .size,
                        true,
                    )
                })
                .collect();
            let windows: Vec<_> = [0, 20, 40]
                .map(|bus| {
                    pci_root_windows(
                        &plan,
                        &ranges,
                        bus,
                        true,
                        (
                            allocator.mmio64_memory.base(),
                            allocator.mmio64_memory.end(),
                        ),
                    )
                })
                .to_vec();
            for (path, base, size, _) in &ranges {
                let domain = match path[0] {
                    0 => 0,
                    20 => 1,
                    _ => 2,
                };
                assert!(
                    windows[domain]
                        .iter()
                        .any(|(low, high)| low <= base && base + size - 1 <= *high)
                );
            }
            assert!(
                ranges
                    .iter()
                    .all(|(_, addr, len, _)| { *addr >= base && *addr + *len <= base + size })
            );
            assert!(
                windows
                    .iter()
                    .flatten()
                    .all(|&(low, high)| { low >= base && high < base + size })
            );
            for first in 0..3 {
                for second in first + 1..3 {
                    for &(a, b) in &windows[first] {
                        for &(c, d) in &windows[second] {
                            assert!(b < c || d < a);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn static_placement_reports_exhausted_aperture() {
        let mut allocator = ResourceAllocator::new();
        allocator.mmio64_memory = vm_allocator::AddressAllocator::new(1 << 40, 1 << 20).unwrap();
        place_static_vfio_bars(
            &mut allocator,
            vec![(
                0,
                BarRequirement {
                    slot: BarSlot::Bar(0),
                    size: 2 << 20,
                    window: BarWindow::Mmio64,
                },
            )],
            &[1],
        )
        .unwrap_err();
    }
    #[test]
    fn static_placement_full_fabric_sibling_windows_do_not_overlap() {
        let mut allocator = ResourceAllocator::with_mmio64_range(2 << 40, 2 << 40).unwrap();
        allocator
            .mmio32_memory
            .allocate(13 * 4096, 4096, vm_allocator::AllocPolicy::FirstMatch)
            .unwrap();
        let mut requirements = Vec::new();
        let mut roots = Vec::new();
        for root in [13, 14, 15, 16, 18, 19, 20, 21] {
            let gpu = roots.len();
            roots.push(root);
            for (index, size) in [(0, 16 << 20), (2, 128 << 30), (4, 32 << 20)] {
                requirements.push((
                    gpu,
                    BarRequirement {
                        slot: BarSlot::Bar(index),
                        size,
                        window: BarWindow::Mmio64,
                    },
                ));
            }
            let nic = roots.len();
            roots.push(root);
            requirements.push((
                nic,
                BarRequirement {
                    slot: BarSlot::Bar(0),
                    size: 32 << 20,
                    window: BarWindow::Mmio64,
                },
            ));
        }
        for _ in 0..4 {
            let switch = roots.len();
            roots.push(17);
            requirements.push((
                switch,
                BarRequirement {
                    slot: BarSlot::Bar(0),
                    size: 32 << 20,
                    window: BarWindow::Mmio32,
                },
            ));
        }
        // Eight 128 GiB BARs cannot fit the default 256 GiB aperture.
        place_static_vfio_bars(&mut ResourceAllocator::new(), requirements.clone(), &roots)
            .unwrap_err();
        let placed = place_static_vfio_bars(&mut allocator, requirements.clone(), &roots).unwrap();
        assert_eq!(placed.len(), 36);
        for gpu in (0..16).step_by(2) {
            let span = |device| {
                let bars: Vec<_> = placed.iter().filter(|(d, _, _)| *d == device).collect();
                let begin = bars.iter().map(|(_, _, base)| *base).min().unwrap();
                let end = bars
                    .iter()
                    .map(|(_, slot, base)| {
                        base + requirements
                            .iter()
                            .find(|(d, r)| *d == device && r.slot == *slot)
                            .unwrap()
                            .1
                            .size
                            - 1
                    })
                    .max()
                    .unwrap();
                (begin, end)
            };
            let (gpu_start, gpu_end) = span(gpu);
            let (nic_start, nic_end) = span(gpu + 1);
            assert!(gpu_end < nic_start || nic_end < gpu_start);
        }
    }
}
