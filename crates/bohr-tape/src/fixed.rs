//! Fixed-point evaluation of the exponential inverse CDF.
//!
//! `f64::ln` is libm-backed and may differ between platforms and libc versions, and a delay that
//! differs in one low bit is a cross-machine reproduction failure. So the mapping from a uniform
//! draw to an exponentially distributed value is integer arithmetic over a frozen table, and the
//! table is as much a part of the tape ABI as the draw widths are: the committed golden vectors in
//! `testdata/tape/exponential.golden` pin it, interpolation error and all. Normative text:
//! `docs/API.md` §2.4. Regenerate both artifacts with
//! `scripts/experiments/exponential-table.py`.

/// `ln(1 + k/256) · 2^32` for `k` in `0..=256`, rounded to nearest.
///
/// The last entry is `ln 2 · 2^32`, which the evaluation below also uses as the per-octave step.
pub(crate) const LN_TABLE_Q32: [u64; 257] = [
    0, 16744533, 33424039, 50039020, 66589974, 83077393, 99501762, 115863562, 132163268, 148401349,
    164578269, 180694488, 196750459, 212746631, 228683449, 244561349, 260380768, 276142134,
    291845871, 307492399, 323082134, 338615487, 354092863, 369514665, 384881291, 400193133,
    415450582, 430654022, 445803834, 460900396, 475944079, 490935254, 505874286, 520761536,
    535597362, 550382118, 565116154, 579799817, 594433450, 609017394, 623551984, 638037554,
    652474432, 666862946, 681203418, 695496167, 709741511, 723939763, 738091233, 752196229,
    766255054, 780268011, 794235396, 808157507, 822034634, 835867069, 849655098, 863399005,
    877099072, 890755577, 904368797, 917939005, 931466472, 944951467, 958394255, 971795100,
    985154263, 998472001, 1011748572, 1024984229, 1038179224, 1051333805, 1064448219, 1077522711,
    1090557523, 1103552895, 1116509066, 1129426270, 1142304743, 1155144714, 1167946415, 1180710071,
    1193435910, 1206124153, 1218775023, 1231388739, 1243965519, 1256505579, 1269009132, 1281476389,
    1293907562, 1306302859, 1318662486, 1330986647, 1343275546, 1355529384, 1367748360, 1379932673,
    1392082518, 1404198090, 1416279581, 1428327183, 1440341085, 1452321476, 1464268541, 1476182467,
    1488063435, 1499911628, 1511727226, 1523510409, 1535261353, 1546980234, 1558667227, 1570322505,
    1581946239, 1593538601, 1605099758, 1616629879, 1628129128, 1639597673, 1651035675, 1662443297,
    1673820701, 1685168045, 1696485489, 1707773188, 1719031300, 1730259979, 1741459379, 1752629651,
    1763770948, 1774883418, 1785967210, 1797022473, 1808049353, 1819047995, 1830018543, 1840961141,
    1851875930, 1862763052, 1873622647, 1884454852, 1895259807, 1906037648, 1916788510, 1927512529,
    1938209838, 1948880570, 1959524856, 1970142828, 1980734614, 1991300345, 2001840147, 2012354148,
    2022842474, 2033305250, 2043742599, 2054154646, 2064541513, 2074903321, 2085240191, 2095552242,
    2105839594, 2116102364, 2126340670, 2136554628, 2146744353, 2156909961, 2167051565, 2177169278,
    2187263213, 2197333481, 2207380193, 2217403458, 2227403387, 2237380086, 2247333665, 2257264230,
    2267171887, 2277056741, 2286918897, 2296758460, 2306575533, 2316370217, 2326142616, 2335892829,
    2345620959, 2355327104, 2365011363, 2374673836, 2384314620, 2393933811, 2403531508, 2413107804,
    2422662797, 2432196579, 2441709246, 2451200890, 2460671605, 2470121482, 2479550612, 2488959088,
    2498346998, 2507714433, 2517061482, 2526388233, 2535694775, 2544981195, 2554247578, 2563494013,
    2572720585, 2581927378, 2591114477, 2600281967, 2609429930, 2618558451, 2627667611, 2636757492,
    2645828176, 2654879744, 2663912276, 2672925851, 2681920551, 2690896452, 2699853634, 2708792175,
    2717712152, 2726613642, 2735496721, 2744361466, 2753207951, 2762036253, 2770846446, 2779638603,
    2788412798, 2797169106, 2805907598, 2814628347, 2823331424, 2832016902, 2840684851, 2849335342,
    2857968445, 2866584230, 2875182766, 2883764122, 2892328366, 2900875568, 2909405794, 2917919111,
    2926415587, 2934895289, 2943358281, 2951804631, 2960234402, 2968647661, 2977044472,
];

/// `⌊mean · (−ln(1 − u/2^32))⌋` for `u` in `0..2^32`, saturating at `u64::MAX`.
///
/// The survival value `v = 2^32 − u` is normalised to a mantissa in `[1, 2)` and an octave count;
/// the mantissa's logarithm is linearly interpolated from [`LN_TABLE_Q32`] on the top eight
/// fraction bits. `u == 0` maps to zero: the smallest uniform draw is the shortest delay.
pub(crate) fn exp_inverse_cdf(mean: u64, u: u64) -> u64 {
    debug_assert!(u < 1 << 32, "the uniform input is a 32-bit draw");
    if u == 0 || mean == 0 {
        return 0;
    }
    let v = (1u64 << 32) - u;
    let bitlen = 64 - u64::from(u64::leading_zeros(v));
    let m32 = v << (32 - bitlen);
    let frac31 = m32 - (1 << 31);
    let k = (frac31 >> 23) as usize;
    let rem = frac31 & ((1 << 23) - 1);
    let ln_m = LN_TABLE_Q32[k] + (((LN_TABLE_Q32[k + 1] - LN_TABLE_Q32[k]) * rem) >> 23);
    let neg_ln = (33 - bitlen) * LN_TABLE_Q32[256] - ln_m;
    u64::try_from((u128::from(mean) * u128::from(neg_ln)) >> 32).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_strictly_increasing_up_to_ln_two() {
        assert!(LN_TABLE_Q32.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(LN_TABLE_Q32[0], 0);
        assert_eq!(LN_TABLE_Q32[256], 2_977_044_472, "the last entry is ln 2 in Q32");
    }

    #[test]
    fn the_mapping_is_monotone_in_the_uniform_input() {
        let mut last = 0;
        for u in (0..1u64 << 32).step_by(65_537) {
            let value = exp_inverse_cdf(1_000_000, u);
            assert!(value >= last, "u = {u} mapped below its predecessor");
            last = value;
        }
    }

    #[test]
    fn the_median_lands_at_mean_times_ln_two() {
        // At u = 2^31 the survival is exactly one half, so the value is mean·ln 2 with no
        // interpolation involved — an anchor the whole table hangs from.
        let median = exp_inverse_cdf(1 << 32, 1 << 31);
        assert_eq!(median, 2_977_044_472);
    }

    /// The committed vectors are derived independently by
    /// `scripts/experiments/exponential-table.py`; a mismatch here is a tape ABI break, not a
    /// tuning opportunity.
    #[test]
    fn golden_vectors_match_the_frozen_evaluation() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/tape/exponential.golden");
        let text = std::fs::read_to_string(path).expect("golden vectors are committed");
        let mut checked = 0;
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            let mut parts = line.split_ascii_whitespace();
            let mean: u64 = parts.next().unwrap().parse().unwrap();
            let u: u64 = parts.next().unwrap().parse().unwrap();
            let expected: u64 = parts.next().unwrap().parse().unwrap();
            assert_eq!(
                exp_inverse_cdf(mean, u),
                expected,
                "vector (mean {mean}, u {u}) diverged from the frozen table"
            );
            checked += 1;
        }
        assert!(checked >= 70, "the golden file lost vectors: {checked} left");
    }

    #[test]
    fn extremes_are_finite_and_ordered() {
        assert_eq!(exp_inverse_cdf(1_000, 0), 0);
        let deepest = exp_inverse_cdf(1_000, (1 << 32) - 1);
        // 32 octaves of ln 2: the tail is capped by the draw's resolution, about 22.2 means.
        assert_eq!(deepest, 22_180);
        assert_eq!(exp_inverse_cdf(u64::MAX, (1 << 32) - 1), u64::MAX, "saturates, never wraps");
    }
}
