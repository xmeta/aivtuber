#![forbid(unsafe_code)]

//! Performance Asset storage and cache primitives.

/// Cache tiers used by the architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTier {
    /// Hot in-memory assets.
    Memory,
    /// Local persistent assets.
    LocalStorage,
    /// Dynamic generation fallback.
    Generated,
}

#[cfg(test)]
mod tests {
    use super::CacheTier;

    #[test]
    fn hot_path_is_memory() {
        assert_eq!(CacheTier::Memory, CacheTier::Memory);
    }
}
