//! N 元曲线纯数学模块（与 EVM PredictionVamm.sol / 主报告 §3.11 逐位对齐）
//! 全部 u128 中间量 + 协议侧有利舍入（ceil）；无 anchor 依赖，可 `cargo test` 直接验证。

pub const MAX_OUTCOMES: usize = 8;
pub const PPM: u64 = 1_000_000;

/// seeding：R_i = n_max · p_min / p_i（大侧 = n_max ⇒ earmark R = n_max，§3.11-②）
pub fn seed_reserves(priors_ppm: &[u32], n_max: u64) -> [u64; MAX_OUTCOMES] {
    let min_p = *priors_ppm.iter().min().unwrap() as u128;
    let mut r = [0u64; MAX_OUTCOMES];
    for (i, &p) in priors_ppm.iter().enumerate() {
        r[i] = ((n_max as u128 * min_p) / p as u128) as u64;
    }
    r
}

#[inline]
fn ceil_div(x: u128, y: u128) -> u128 {
    (x + y - 1) / y
}

/// 买入（mint-then-swap，§3.11-③）：入金 a（已扣 vig）买 outcome i
/// 返回 (shares_out, new_r_i)；调用方随后执行 R_j += a (j≠i)、R_i = new_r_i
pub fn calc_buy(reserves: &[u64], n: usize, outcome: usize, a: u64) -> (u64, u64) {
    let mut new_ri = reserves[outcome] as u128;
    for (j, &rj) in reserves.iter().enumerate().take(n) {
        if j == outcome {
            continue;
        }
        new_ri = ceil_div(new_ri * rj as u128, rj as u128 + a as u128);
    }
    let shares = a as u128 + reserves[outcome] as u128 - new_ri;
    (shares as u64, new_ri as u64)
}

/// 卖出（§3.11-④）：取回现金 c，解 (R_i+z−c)·∏_{j≠i}(R_j−c) = K
/// 返回 (shares_in z, new_r_i = R_i+z−c)；调用方需先校验 c < R_j (∀j≠i) 且 c ≤ real_usdc
pub fn calc_sell(reserves: &[u64], n: usize, outcome: usize, c: u64) -> (u64, u64) {
    let mut x = reserves[outcome] as u128;
    for (j, &rj) in reserves.iter().enumerate().take(n) {
        if j == outcome {
            continue;
        }
        x = ceil_div(x * rj as u128, rj as u128 - c as u128);
    }
    let z = c as u128 + x - reserves[outcome] as u128;
    (z as u64, x as u64)
}

/// 价格（ppm）：p_i = (1/R_i)/Σ(1/R_k)
pub fn prices_ppm(reserves: &[u64], n: usize) -> [u64; MAX_OUTCOMES] {
    let mut w = [0u128; MAX_OUTCOMES];
    let mut sum = 0u128;
    for i in 0..n {
        w[i] = u128::pow(10, 30) / reserves[i] as u128;
        sum += w[i];
    }
    let mut p = [0u64; MAX_OUTCOMES];
    for i in 0..n {
        p[i] = ((w[i] * PPM as u128) / sum) as u64;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    // §3.11 足球 1X2 实算向量（与 python / Solidity 测试逐位一致）
    const PRIORS: [u32; 3] = [450_000, 280_000, 270_000];
    const N_MAX: u64 = 1_000_000_000; // $1,000（6 位小数）

    #[test]
    fn seed_matches_python_vector() {
        let r = seed_reserves(&PRIORS, N_MAX);
        assert_eq!(r[0], 600_000_000);
        assert_eq!(r[1], 964_285_714);
        assert_eq!(r[2], 1_000_000_000);
        let p = prices_ppm(&r, 3);
        assert!((p[0] as i64 - 450_000).abs() <= 2);
        assert!((p[1] as i64 - 280_000).abs() <= 2);
        assert!((p[2] as i64 - 270_000).abs() <= 2);
    }

    #[test]
    fn buy_matches_python_vector() {
        // $200 买平局，fee 2.5% → a = 195e6 → 550,279,184 份（与 Solidity 测试同值）
        let mut r = seed_reserves(&PRIORS, N_MAX);
        let a = 195_000_000u64;
        let (shares, new_ri) = calc_buy(&r, 3, 1, a);
        assert_eq!(shares, 550_279_183);
        r[0] += a;
        r[2] += a;
        r[1] = new_ri;
        let p = prices_ppm(&r, 3);
        assert!((p[1] as i64 - 439_430).abs() <= 600, "p_D≈0.4394, got {}", p[1]);
        assert!((p[0] + p[1] + p[2]).abs_diff(PPM) <= 3);
    }

    #[test]
    fn theorem_one_sided_10k() {
        // 单边 $10k 砸平局：shortfall = Q_win − S ≤ n_max（§3.3 定理，python 值 ≈959e6）
        let mut r = seed_reserves(&PRIORS, N_MAX);
        let a = 9_750_000_000u64; // $10k 扣 2.5% vig
        let (shares, new_ri) = calc_buy(&r, 3, 1, a);
        r[0] += a;
        r[2] += a;
        r[1] = new_ri;
        let shortfall = shares - a; // Q_win − S
        assert!(shortfall <= N_MAX, "THEOREM VIOLATION: {shortfall}");
        assert!(shortfall >= 900_000_000, "python vector ~959e6, got {shortfall}");
    }

    #[test]
    fn sell_roundtrip_protocol_favorable() {
        let mut r = seed_reserves(&PRIORS, N_MAX);
        let a = 97_500_000u64; // $100 扣 vig
        let (got, new_ri) = calc_buy(&r, 3, 0, a);
        r[1] += a;
        r[2] += a;
        r[0] = new_ri;
        // 卖回 $50 现金所需份额 ≤ 持仓（协议侧有利舍入）
        let (need, _) = calc_sell(&r, 3, 0, 50_000_000);
        assert!(need <= got, "need {need} > got {got}");
    }

    #[test]
    fn fuzz_theorem_random_sequences() {
        // 轻量 fuzz：随机 2-4 元、随机先验、随机买卖序列 → 任意赢家 shortfall ≤ n_max + 尘埃
        let mut rng: u64 = 0x5eed_2026_0703;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for _case in 0..500 {
            let n = 2 + (next() % 3) as usize;
            // 生成合法先验
            let mut priors = vec![0u32; n];
            let mut rem = 1_000_000u64;
            for i in 0..n - 1 {
                let slots = (n - 1 - i) as u64;
                let lo = if rem > 960_000 * slots { rem - 960_000 * slots } else { 20_000 };
                let hi = (rem - 20_000 * slots).min(960_000);
                let pick = lo + next() % (hi - lo + 1);
                priors[i] = pick as u32;
                rem -= pick;
            }
            priors[n - 1] = rem as u32;

            let n_max = 250_000_000u64; // B 类档 $250
            let mut r = seed_reserves(&priors, n_max);
            let mut real: u128 = 0;
            let mut q_out = [0u128; MAX_OUTCOMES];

            for _op in 0..10 {
                let side = (next() % n as u64) as usize;
                if next() % 4 == 0 && real > 2_000_000 {
                    // 卖：取回 ≤ 池内真金 1/3
                    let c = (real / 3) as u64;
                    if (0..n).filter(|&j| j != side).all(|j| r[j] > c) && q_out[side] > 0 {
                        let (z, x) = calc_sell(&r[..n], n, side, c);
                        if (z as u128) <= q_out[side] {
                            for j in 0..n {
                                if j != side {
                                    r[j] -= c;
                                }
                            }
                            r[side] = x;
                            real -= c as u128;
                            q_out[side] -= z as u128;
                        }
                    }
                } else {
                    let a = 1_000_000 + (next() % 3_000) * 1_000_000; // $1..$3000
                    let (s, x) = calc_buy(&r[..n], n, side, a);
                    for j in 0..n {
                        if j != side {
                            r[j] += a;
                        }
                    }
                    r[side] = x;
                    real += a as u128;
                    q_out[side] += s as u128;
                }
            }
            for w in 0..n {
                let shortfall = q_out[w].saturating_sub(real);
                assert!(
                    shortfall <= n_max as u128 + 10,
                    "THEOREM VIOLATION case={_case} w={w} shortfall={shortfall}"
                );
            }
        }
    }
}
