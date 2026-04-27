#![no_std]

#[cfg(test)]
extern crate std;

pub mod arch_aarch64;
pub mod arch_x86_64;
pub mod feature_mask;
pub mod select;

pub use feature_mask::{Feature, FeatureMask};
pub use select::{CpuIdentity, HostInfo, TargetArch, TargetKind, VariantMeta, select_variant};
