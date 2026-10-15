use kvm_bindings::kvm_cpuid_entry2;

const LEAF_VENDOR: u32 = 0x0;
const LEAF_FEATURES: u32 = 0x1;
const LEAF_CACHE_PARAMETERS: u32 = 0x4;
const LEAF_EXTENDED_TOPOLOGY: u32 = 0xb;
const LEAF_V2_EXTENDED_TOPOLOGY: u32 = 0x1f;
const LEAF_AMD_FEATURES: u32 = 0x8000_0001;
const LEAF_AMD_SIZE_IDENTIFIERS: u32 = 0x8000_0008;
const LEAF_AMD_EXTENDED_APIC_ID: u32 = 0x8000_001e;

const ECX_HYPERVISOR: u32 = 1 << 31;
const EDX_HTT: u32 = 1 << 28;
const AMD_ECX_CMP_LEGACY: u32 = 1 << 1;
const TOPOLOGY_LEVEL_SMT: u32 = 1;
const TOPOLOGY_LEVEL_CORE: u32 = 2;
const CACHE_LEVEL_SHARED_BY_ALL_CORES: u32 = 3;

#[derive(Clone, Copy, Debug)]
pub struct CpuTopology {
    pub apic_id: u8,
    pub cpu_count: u8,
}

impl CpuTopology {
    fn core_id_bits(self) -> u32 {
        u32::from(self.cpu_count.max(1))
            .next_power_of_two()
            .trailing_zeros()
    }
}

fn is_amd(entries: &[kvm_cpuid_entry2]) -> bool {
    entries
        .iter()
        .find(|e| e.function == LEAF_VENDOR)
        .is_some_and(|e| (e.ebx, e.edx, e.ecx) == (0x6874_7541, 0x6974_6e65, 0x444d_4163))
}

pub fn configure_cpuid(entries: &mut [kvm_cpuid_entry2], topology: CpuTopology) {
    let amd = is_amd(entries);
    let apic_id = u32::from(topology.apic_id);
    let count = u32::from(topology.cpu_count);
    for entry in entries.iter_mut() {
        match entry.function {
            LEAF_FEATURES => {
                entry.ebx = (entry.ebx & 0x0000_ffff) | (apic_id << 24) | (count << 16);
                entry.ecx |= ECX_HYPERVISOR;
                if count > 1 {
                    entry.edx |= EDX_HTT;
                } else {
                    entry.edx &= !EDX_HTT;
                }
            }
            LEAF_CACHE_PARAMETERS if !amd => {
                let level = (entry.eax >> 5) & 0x7;
                let sharing = if level >= CACHE_LEVEL_SHARED_BY_ALL_CORES {
                    count - 1
                } else {
                    0
                };
                entry.eax = (entry.eax & 0x3fff) | (sharing << 14) | ((count - 1) << 26);
            }
            LEAF_EXTENDED_TOPOLOGY | LEAF_V2_EXTENDED_TOPOLOGY => {
                configure_topology_leaf(entry, topology);
            }
            LEAF_AMD_FEATURES if amd => {
                if count > 1 {
                    entry.ecx |= AMD_ECX_CMP_LEGACY;
                } else {
                    entry.ecx &= !AMD_ECX_CMP_LEGACY;
                }
            }
            LEAF_AMD_SIZE_IDENTIFIERS if amd => {
                entry.ecx = (topology.core_id_bits() << 12) | (count - 1);
            }
            LEAF_AMD_EXTENDED_APIC_ID if amd => {
                entry.eax = apic_id;
                entry.ebx = apic_id;
                entry.ecx = 0;
            }
            _ => {}
        }
    }
}

fn configure_topology_leaf(entry: &mut kvm_cpuid_entry2, topology: CpuTopology) {
    let (eax, ebx, level_type) = match entry.index {
        0 => (0, 1, TOPOLOGY_LEVEL_SMT),
        1 => (
            topology.core_id_bits(),
            u32::from(topology.cpu_count),
            TOPOLOGY_LEVEL_CORE,
        ),
        _ => (0, 0, 0),
    };
    entry.eax = eax;
    entry.ebx = ebx;
    entry.ecx = (level_type << 8) | (entry.index & 0xff);
    entry.edx = u32::from(topology.apic_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(function: u32, index: u32) -> kvm_cpuid_entry2 {
        kvm_cpuid_entry2 {
            function,
            index,
            ..Default::default()
        }
    }

    fn intel_entries() -> Vec<kvm_cpuid_entry2> {
        let mut vendor = entry(0, 0);
        vendor.ebx = 0x756e_6547;
        vendor.edx = 0x4965_6e69;
        vendor.ecx = 0x6c65_746e;
        let mut features = entry(1, 0);
        features.ebx = 0xffff_0800;
        let mut l1 = entry(4, 0);
        l1.eax = (1 << 5) | 0x1;
        let mut l3 = entry(4, 3);
        l3.eax = (3 << 5) | 0x3;
        vec![
            vendor,
            features,
            l1,
            l3,
            entry(0xb, 0),
            entry(0xb, 1),
            entry(0xb, 2),
        ]
    }

    #[test]
    fn sets_apic_id_and_logical_count() {
        let mut entries = intel_entries();
        configure_cpuid(
            &mut entries,
            CpuTopology {
                apic_id: 3,
                cpu_count: 4,
            },
        );
        let features = entries[1];
        assert_eq!(features.ebx >> 24, 3);
        assert_eq!((features.ebx >> 16) & 0xff, 4);
        assert_eq!(features.ebx & 0xffff, 0x0800);
        assert_ne!(features.ecx & ECX_HYPERVISOR, 0);
        assert_ne!(features.edx & EDX_HTT, 0);
    }

    #[test]
    fn describes_cores_in_cache_and_topology_leaves() {
        let mut entries = intel_entries();
        configure_cpuid(
            &mut entries,
            CpuTopology {
                apic_id: 1,
                cpu_count: 3,
            },
        );
        assert_eq!(entries[2].eax >> 26, 2);
        assert_eq!((entries[2].eax >> 14) & 0xfff, 0);
        assert_eq!((entries[3].eax >> 14) & 0xfff, 2);
        let (smt, core, invalid) = (entries[4], entries[5], entries[6]);
        assert_eq!((smt.eax, smt.ebx, smt.ecx >> 8, smt.edx), (0, 1, 1, 1));
        assert_eq!((core.eax, core.ebx, core.ecx >> 8, core.edx), (2, 3, 2, 1));
        assert_eq!((invalid.ebx, invalid.ecx), (0, 2));
    }

    #[test]
    fn amd_core_count() {
        let mut vendor = entry(0, 0);
        vendor.ebx = 0x6874_7541;
        vendor.edx = 0x6974_6e65;
        vendor.ecx = 0x444d_4163;
        let mut entries = vec![vendor, entry(0x8000_0008, 0), entry(0x8000_001e, 0)];
        configure_cpuid(
            &mut entries,
            CpuTopology {
                apic_id: 5,
                cpu_count: 8,
            },
        );
        assert_eq!(entries[1].ecx, (3 << 12) | 7);
        assert_eq!(entries[2].eax, 5);
    }
}
