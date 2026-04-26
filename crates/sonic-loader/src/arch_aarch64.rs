use crate::feature_mask::{Feature, FeatureMask};

pub const HWCAP_ASIMD: usize = 1 << 1;
pub const HWCAP_AES: usize = 1 << 3;
pub const HWCAP_PMULL: usize = 1 << 4;
pub const HWCAP_SHA1: usize = 1 << 5;
pub const HWCAP_SHA2: usize = 1 << 6;
pub const HWCAP_CRC32: usize = 1 << 7;
pub const HWCAP_ATOMICS: usize = 1 << 8;
pub const HWCAP_FPHP: usize = 1 << 9;
pub const HWCAP_ASIMDHP: usize = 1 << 10;
pub const HWCAP_CPUID: usize = 1 << 11;
pub const HWCAP_ASIMDRDM: usize = 1 << 12;
pub const HWCAP_JSCVT: usize = 1 << 13;
pub const HWCAP_FCMA: usize = 1 << 14;
pub const HWCAP_LRCPC: usize = 1 << 15;
pub const HWCAP_DCPOP: usize = 1 << 16;
pub const HWCAP_SHA3: usize = 1 << 17;
pub const HWCAP_SM3: usize = 1 << 18;
pub const HWCAP_SM4: usize = 1 << 19;
pub const HWCAP_ASIMDDP: usize = 1 << 20;
pub const HWCAP_SHA512: usize = 1 << 21;
pub const HWCAP_SVE: usize = 1 << 22;
pub const HWCAP_ASIMDFHM: usize = 1 << 23;
pub const HWCAP_DIT: usize = 1 << 24;
pub const HWCAP_USCAT: usize = 1 << 25;
pub const HWCAP_ILRCPC: usize = 1 << 26;
pub const HWCAP_FLAGM: usize = 1 << 27;
pub const HWCAP_SSBS: usize = 1 << 28;
pub const HWCAP_SB: usize = 1 << 29;
pub const HWCAP_PACA: usize = 1 << 30;
pub const HWCAP_PACG: usize = 1 << 31;

pub const HWCAP2_DCPODP: usize = 1 << 0;
pub const HWCAP2_SVE2: usize = 1 << 1;
pub const HWCAP2_SVEAES: usize = 1 << 2;
pub const HWCAP2_SVEPMULL: usize = 1 << 3;
pub const HWCAP2_SVEBITPERM: usize = 1 << 4;
pub const HWCAP2_SVESHA3: usize = 1 << 5;
pub const HWCAP2_SVESM4: usize = 1 << 6;
pub const HWCAP2_FLAGM2: usize = 1 << 7;
pub const HWCAP2_FRINT: usize = 1 << 8;
pub const HWCAP2_SVEI8MM: usize = 1 << 9;
pub const HWCAP2_SVEF32MM: usize = 1 << 10;
pub const HWCAP2_SVEF64MM: usize = 1 << 11;
pub const HWCAP2_SVEBF16: usize = 1 << 12;
pub const HWCAP2_I8MM: usize = 1 << 13;
pub const HWCAP2_BF16: usize = 1 << 14;
pub const HWCAP2_DGH: usize = 1 << 15;
pub const HWCAP2_RNG: usize = 1 << 16;
pub const HWCAP2_BTI: usize = 1 << 17;
pub const HWCAP2_MTE: usize = 1 << 18;
pub const HWCAP2_ECV: usize = 1 << 19;
pub const HWCAP2_AFP: usize = 1 << 20;
pub const HWCAP2_RPRES: usize = 1 << 21;
pub const HWCAP2_MTE3: usize = 1 << 22;
pub const HWCAP2_SME: usize = 1 << 23;

pub fn detect_aarch64_features_from_hwcap(hwcap: usize, hwcap2: usize, _hwcap3: usize) -> FeatureMask {
    let mut out = FeatureMask::EMPTY;
    if has(hwcap, HWCAP_ASIMD) { out.insert(Feature::Asimd); }
    if has(hwcap, HWCAP_AES) { out.insert(Feature::Aes); }
    if has(hwcap, HWCAP_CRC32) { out.insert(Feature::Crc); }
    if has(hwcap, HWCAP_ATOMICS) { out.insert(Feature::Lse); }
    if has(hwcap, HWCAP_FPHP) || has(hwcap, HWCAP_ASIMDHP) { out.insert(Feature::Fp16); }
    if has(hwcap, HWCAP_ASIMDRDM) { out.insert(Feature::Rdm); }
    if has(hwcap, HWCAP_JSCVT) { out.insert(Feature::Jsconv); }
    if has(hwcap, HWCAP_FCMA) { out.insert(Feature::Fcma); }
    if has(hwcap, HWCAP_LRCPC) { out.insert(Feature::Rcpc); }
    if has(hwcap, HWCAP_DCPOP) { out.insert(Feature::Dpb); }
    if has(hwcap, HWCAP_SHA2) { out.insert(Feature::Sha2); }
    if has(hwcap, HWCAP_SHA3) { out.insert(Feature::Sha3); }
    if has(hwcap, HWCAP_SHA512) { out.insert(Feature::Sha512); }
    if has(hwcap, HWCAP_SM3) { out.insert(Feature::Sm3); }
    if has(hwcap, HWCAP_SM4) { out.insert(Feature::Sm4); }
    if has(hwcap, HWCAP_ASIMDDP) { out.insert(Feature::Dotprod); }
    if has(hwcap, HWCAP_SVE) { out.insert(Feature::Sve); }
    if has(hwcap, HWCAP_ASIMDFHM) { out.insert(Feature::Fhm); }
    if has(hwcap, HWCAP_DIT) { out.insert(Feature::Dit); }
    if has(hwcap, HWCAP_FLAGM) { out.insert(Feature::Flagm); }
    if has(hwcap, HWCAP_SSBS) { out.insert(Feature::Ssbs); }
    if has(hwcap, HWCAP_SB) { out.insert(Feature::Sb); }
    if has(hwcap, HWCAP_PACA) { out.insert(Feature::Paca); }
    if has(hwcap, HWCAP_PACG) { out.insert(Feature::Pacg); }
    if has(hwcap2, HWCAP2_DCPODP) { out.insert(Feature::Dpb2); }
    if has(hwcap2, HWCAP2_SVE2) { out.insert(Feature::Sve2); }
    if has(hwcap2, HWCAP2_FRINT) { out.insert(Feature::Frintts); }
    if has(hwcap2, HWCAP2_I8MM) { out.insert(Feature::I8mm); }
    if has(hwcap2, HWCAP2_BF16) { out.insert(Feature::Bf16); }
    if has(hwcap2, HWCAP2_RNG) { out.insert(Feature::Rand); }
    if has(hwcap2, HWCAP2_BTI) { out.insert(Feature::Bti); }
    if has(hwcap2, HWCAP2_MTE) { out.insert(Feature::Mte); }
    out
}

const fn has(value: usize, bit: usize) -> bool {
    (value & bit) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aarch64_neon_maps_to_asimd_hwcap() {
        assert!(detect_aarch64_features_from_hwcap(HWCAP_ASIMD, 0, 0).contains(Feature::Asimd));
    }

    #[test]
    fn aarch64_lse_crc_dotprod_fp16_sve_sve2_map_correctly() {
        let mask = detect_aarch64_features_from_hwcap(
            HWCAP_ATOMICS | HWCAP_CRC32 | HWCAP_ASIMDDP | HWCAP_FPHP | HWCAP_SVE,
            HWCAP2_SVE2,
            0,
        );
        assert!(mask.contains(Feature::Lse));
        assert!(mask.contains(Feature::Crc));
        assert!(mask.contains(Feature::Dotprod));
        assert!(mask.contains(Feature::Fp16));
        assert!(mask.contains(Feature::Sve));
        assert!(mask.contains(Feature::Sve2));
    }

    #[test]
    fn aarch64_unknown_feature_mapping_is_not_silently_accepted() {
        let mask = detect_aarch64_features_from_hwcap(0, 0, usize::MAX);
        assert_eq!(mask, FeatureMask::EMPTY);
    }
}
