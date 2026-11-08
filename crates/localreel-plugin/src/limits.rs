//! Resource ceilings for one plugin process tree.
//!
//! These are not a substitute for OS network isolation; they stop a plugin from
//! consuming the host's memory or forking without bound. Windows applies both
//! limits to the Job Object; Unix applies the address-space limit with
//! `setrlimit(RLIMIT_AS)` before exec.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginLimits {
    pub max_memory_bytes: u64,
    pub max_processes: u32,
}

impl Default for PluginLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 512 * 1024 * 1024,
            max_processes: 16,
        }
    }
}

impl PluginLimits {
    pub const MIN_MEMORY_BYTES: u64 = 16 * 1024 * 1024;
    pub const MAX_MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;

    pub fn validate(&self) -> Result<(), String> {
        if !(Self::MIN_MEMORY_BYTES..=Self::MAX_MEMORY_BYTES).contains(&self.max_memory_bytes) {
            return Err(format!(
                "plugin max_memory_bytes must be {}..={}",
                Self::MIN_MEMORY_BYTES,
                Self::MAX_MEMORY_BYTES
            ));
        }
        if !(1..=256).contains(&self.max_processes) {
            return Err("plugin max_processes must be 1..=256".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_inside_the_supported_range() {
        PluginLimits::default().validate().unwrap();
        assert!(PluginLimits {
            max_memory_bytes: 1024,
            max_processes: 16,
        }
        .validate()
        .is_err());
        assert!(PluginLimits {
            max_memory_bytes: 256 * 1024 * 1024,
            max_processes: 0,
        }
        .validate()
        .is_err());
    }
}
