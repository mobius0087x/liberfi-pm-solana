//! Liberfi 长尾预测市场 · Solana/Anchor 版（Phase 1 对等实现）
//! =============================================================
//! 与 EVM 版（contracts/src/PredictionVamm.sol + XlpVault.sol）功能对等：
//!  - N 元 mint-then-swap 曲线（math.rs，与 §3.11 python 向量逐位对齐）
//!  - seeding R_i = n_max·p_min/p_i，earmark R = n_max（shortfall ≤ n 构造性执行）
//!  - 三终局：settle_resolved / settle_void（流拍全退含 vig）/ 毕业留 Phase 2
//!  - 创建者押注定价 + per-creator 并行上限
//! 账户模型（对应 evidence/D §B2 骨架）：
//!  - Config PDA + 全局签名 PDA("auth") 持有 xlp_vault 与各 market_vault（SPL token 账户）
//!  - Market PDA 定长数组存储（≤8 outcome）；UserPosition 内部账本；OracleResult = mock 预言机

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

pub mod math;
use math::{calc_buy, calc_sell, seed_reserves, MAX_OUTCOMES, PPM};

declare_id!("5oBHzLgzE3X8QKQDspH2gCN34g1tbhnKs9dXxQ8KM8Sk");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Liberfi Prediction vAMM (Phase 1, devnet)",
    project_url: "https://github.com/mobius0087x/liberfi-pm-solana",
    contacts: "link:https://github.com/mobius0087x/liberfi-pm-solana/issues",
    policy: "https://github.com/mobius0087x/liberfi-pm-solana#security",
    preferred_languages: "en,zh",
    source_code: "https://github.com/mobius0087x/liberfi-pm-solana"
}

#[program]
pub mod liberfi_pm {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        void_threshold: u64,
        min_creator_stake: u64,
        creator_cap: u8,
    ) -> Result<()> {
        let c = &mut ctx.accounts.config;
        c.authority = ctx.accounts.authority.key();
        c.usdc_mint = ctx.accounts.usdc_mint.key();
        c.auth_bump = ctx.bumps.auth;
        c.total_reserved = 0;
        c.void_threshold = void_threshold;
        c.min_creator_stake = min_creator_stake;
        c.creator_cap = creator_cap;
        c.market_count = 0;
        Ok(())
    }

    /// xLP 金库注资（协议自有资金；Phase 2 可份额化开放）
    pub fn fund_vault(ctx: Context<FundVault>, amount: u64) -> Result<()> {
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.funder_token.to_account_info(),
                    to: ctx.accounts.xlp_vault.to_account_info(),
                    authority: ctx.accounts.funder.to_account_info(),
                },
            ),
            amount,
        )
    }

    /// 建盘（§3.9 双轨：authority = A 类轨豁免押金/上限；其他地址 = B 类 staked 轨）
    #[allow(clippy::too_many_arguments)]
    pub fn create_market(
        ctx: Context<CreateMarket>,
        market_id: u64,
        question_id: [u8; 32],
        priors_ppm: Vec<u32>,
        n_max: u64,
        fee_bps: u16,
        end_ts: i64,
        creator_side: u8,
        creator_stake: u64,
    ) -> Result<()> {
        let cfg = &mut ctx.accounts.config;
        let n = priors_ppm.len();
        require!((2..=MAX_OUTCOMES).contains(&n), PmError::BadOutcomeCount);
        require!(fee_bps <= 1_000, PmError::FeeTooHigh);
        require!(end_ts > Clock::get()?.unix_timestamp, PmError::EndInPast);
        require!(n_max > 0, PmError::ZeroSeed);
        require!((creator_side as usize) < n, PmError::BadOutcome);
        require!(market_id == cfg.market_count + 1, PmError::BadMarketId);

        let is_admin = ctx.accounts.creator.key() == cfg.authority;
        if !is_admin {
            require!(
                ctx.accounts.creator_stats.active < cfg.creator_cap,
                PmError::CreatorCap
            );
            require!(creator_stake >= cfg.min_creator_stake, PmError::StakeTooLow);
        }

        let mut sum = 0u64;
        for &p in &priors_ppm {
            require!((20_000..=960_000).contains(&p), PmError::PriorOutOfRange);
            sum += p as u64;
        }
        require!(sum == PPM, PmError::PriorsMustSumToOne);

        // earmark R = n_max：组合预算 Σreserved ≤ 金库余额（§3.4-5 / §3.8-A）
        cfg.total_reserved += n_max;
        require!(
            cfg.total_reserved <= ctx.accounts.xlp_vault.amount,
            PmError::BudgetExhausted
        );
        cfg.market_count = market_id;

        let m = &mut ctx.accounts.market;
        m.id = market_id;
        m.question_id = question_id;
        m.creator = ctx.accounts.creator.key();
        m.end_ts = end_ts;
        m.fee_bps = fee_bps;
        m.n = n as u8;
        m.status = STATUS_OPEN;
        m.n_max = n_max;
        m.earmark_remaining = n_max;
        m.reserves = seed_reserves(&priors_ppm, n_max);

        ctx.accounts.creator_stats.active += 1;

        // 创建者押注定价：设价必须下首注（§3.4-7a）
        if creator_stake > 0 {
            do_buy(
                m,
                &mut ctx.accounts.creator_position,
                creator_side as usize,
                creator_stake,
                0,
                &ctx.accounts.creator_token,
                &ctx.accounts.market_vault,
                &ctx.accounts.creator,
                &ctx.accounts.token_program,
            )?;
        }
        Ok(())
    }

    /// 买入（先铸后换，math::calc_buy）
    pub fn buy(
        ctx: Context<Trade>,
        _market_id: u64,
        outcome: u8,
        usd_in: u64,
        min_shares_out: u64,
    ) -> Result<()> {
        let m = &mut ctx.accounts.market;
        require!(m.status == STATUS_OPEN, PmError::NotTrading);
        require!(Clock::get()?.unix_timestamp < m.end_ts, PmError::NotTrading);
        require!((outcome as usize) < m.n as usize, PmError::BadOutcome);
        do_buy(
            m,
            &mut ctx.accounts.position,
            outcome as usize,
            usd_in,
            min_shares_out,
            &ctx.accounts.user_token,
            &ctx.accounts.market_vault,
            &ctx.accounts.user,
            &ctx.accounts.token_program,
        )
    }

    /// 卖出（取回 cash_out 现金；真金守卫 + 协议侧有利舍入）
    pub fn sell(
        ctx: Context<Trade>,
        _market_id: u64,
        outcome: u8,
        cash_out: u64,
        max_shares_in: u64,
    ) -> Result<()> {
        let m = &mut ctx.accounts.market;
        require!(m.status == STATUS_OPEN, PmError::NotTrading);
        require!(Clock::get()?.unix_timestamp < m.end_ts, PmError::NotTrading);
        let i = outcome as usize;
        require!(i < m.n as usize, PmError::BadOutcome);
        require!(cash_out > 0 && cash_out <= m.real_usdc, PmError::CashGuard);
        let n = m.n as usize;
        for j in 0..n {
            if j != i {
                require!(cash_out < m.reserves[j], PmError::CashGuard);
            }
        }
        let (z, x) = calc_sell(&m.reserves[..n], n, i, cash_out);
        require!(z <= max_shares_in, PmError::Slippage);
        let pos = &mut ctx.accounts.position;
        require!(pos.shares[i] >= z, PmError::InsufficientShares);

        pos.shares[i] -= z;
        pos.net_paid -= cash_out as i64;
        m.q_out[i] -= z;
        for j in 0..n {
            if j != i {
                m.reserves[j] -= cash_out;
            }
        }
        m.reserves[i] = x;
        m.real_usdc -= cash_out;

        transfer_from_vault(
            &ctx.accounts.market_vault,
            &ctx.accounts.user_token,
            &ctx.accounts.auth,
            &ctx.accounts.token_program,
            ctx.accounts.config.auth_bump,
            cash_out,
        )
    }

    /// mock 预言机：authority 设结果（Phase 2 换 Switchboard/Pyth/UMA 形状适配器）
    pub fn set_outcome(ctx: Context<SetOutcome>, _question_id: [u8; 32], winner: u8) -> Result<()> {
        ctx.accounts.oracle_result.winner = winner;
        Ok(())
    }

    /// 结算（oracle 已出结果）：vig→金库、residual→金库 或 shortfall←金库（≤ earmark）
    pub fn settle_resolved(ctx: Context<SettleResolved>, _market_id: u64) -> Result<()> {
        let m = &mut ctx.accounts.market;
        require!(m.status == STATUS_OPEN, PmError::NotOpen);
        let w = ctx.accounts.oracle_result.winner;
        require!((w as usize) < m.n as usize, PmError::BadOutcome);
        m.winner = w;
        m.status = STATUS_RESOLVED;

        let cfg = &mut ctx.accounts.config;
        let bump = cfg.auth_bump;

        // vig 归金库（xLP 承担尾部风险的保费）
        if m.vig_acc > 0 {
            transfer_from_vault(
                &ctx.accounts.market_vault,
                &ctx.accounts.xlp_vault,
                &ctx.accounts.auth,
                &ctx.accounts.token_program,
                bump,
                m.vig_acc,
            )?;
            m.vig_acc = 0;
        }

        let owed = m.q_out[w as usize];
        if m.real_usdc >= owed {
            let residual = m.real_usdc - owed; // 庄赢盘 → 归金库
            if residual > 0 {
                transfer_from_vault(
                    &ctx.accounts.market_vault,
                    &ctx.accounts.xlp_vault,
                    &ctx.accounts.auth,
                    &ctx.accounts.token_program,
                    bump,
                    residual,
                )?;
            }
        } else {
            // 定理：缺口 ≤ n_max，由划拨额内支付（超出即 revert —— 构造性保护）
            let shortfall = owed - m.real_usdc;
            require!(shortfall <= m.earmark_remaining, PmError::TheoremViolation);
            m.earmark_remaining -= shortfall;
            cfg.total_reserved -= shortfall;
            transfer_from_vault(
                &ctx.accounts.xlp_vault,
                &ctx.accounts.market_vault,
                &ctx.accounts.auth,
                &ctx.accounts.token_program,
                bump,
                shortfall,
            )?;
        }
        m.real_usdc = owed;

        // 释放剩余 earmark
        cfg.total_reserved -= m.earmark_remaining;
        m.earmark_remaining = 0;
        ctx.accounts.creator_stats.active -= 1;
        Ok(())
    }

    /// 流拍（到期无结果且成交 < 流拍线）：全退含 vig
    pub fn settle_void(ctx: Context<SettleVoid>, _market_id: u64) -> Result<()> {
        let m = &mut ctx.accounts.market;
        require!(m.status == STATUS_OPEN, PmError::NotOpen);
        require!(
            Clock::get()?.unix_timestamp >= m.end_ts,
            PmError::AwaitingOracle
        );
        require!(
            m.volume < ctx.accounts.config.void_threshold,
            PmError::AwaitingOracle
        );
        m.status = STATUS_VOID;
        ctx.accounts.creator_stats.active -= 1;
        Ok(())
    }

    /// 赢方兑付：每份 1 USDC
    pub fn claim(ctx: Context<ClaimCtx>, _market_id: u64) -> Result<()> {
        let m = &mut ctx.accounts.market;
        require!(m.status == STATUS_RESOLVED, PmError::NotResolved);
        let pos = &mut ctx.accounts.position;
        let amount = pos.shares[m.winner as usize];
        require!(amount > 0, PmError::Nothing);
        pos.shares[m.winner as usize] = 0;
        m.real_usdc -= amount;
        transfer_from_vault(
            &ctx.accounts.market_vault,
            &ctx.accounts.user_token,
            &ctx.accounts.auth,
            &ctx.accounts.token_program,
            ctx.accounts.config.auth_bump,
            amount,
        )
    }

    /// 流拍退款：净入金全退（含 vig）；极端卖出获利路径缺口由 earmark 兜底（≤ n）
    pub fn claim_refund(ctx: Context<RefundCtx>, _market_id: u64) -> Result<()> {
        let m = &mut ctx.accounts.market;
        require!(m.status == STATUS_VOID, PmError::NotVoid);
        let pos = &mut ctx.accounts.position;
        require!(pos.net_paid > 0, PmError::Nothing);
        let refund = pos.net_paid as u64;
        pos.net_paid = 0;

        let cfg = &mut ctx.accounts.config;
        let avail = m.real_usdc + m.vig_acc;
        if refund > avail {
            let pull = refund - avail;
            require!(pull <= m.earmark_remaining, PmError::TheoremViolation);
            m.earmark_remaining -= pull;
            cfg.total_reserved -= pull;
            transfer_from_vault(
                &ctx.accounts.xlp_vault,
                &ctx.accounts.market_vault,
                &ctx.accounts.auth,
                &ctx.accounts.token_program,
                cfg.auth_bump,
                pull,
            )?;
            m.real_usdc = 0;
            m.vig_acc = 0;
        } else {
            let from_vig = refund.min(m.vig_acc);
            m.vig_acc -= from_vig;
            m.real_usdc -= refund - from_vig;
        }
        transfer_from_vault(
            &ctx.accounts.market_vault,
            &ctx.accounts.user_token,
            &ctx.accounts.auth,
            &ctx.accounts.token_program,
            ctx.accounts.config.auth_bump,
            refund,
        )
    }
}

// ---------------- 内部逻辑 ----------------

const STATUS_OPEN: u8 = 1;
const STATUS_RESOLVED: u8 = 2;
const STATUS_VOID: u8 = 3;

#[allow(clippy::too_many_arguments)]
fn do_buy<'info>(
    m: &mut Account<'info, Market>,
    pos: &mut Account<'info, UserPosition>,
    outcome: usize,
    usd_in: u64,
    min_shares_out: u64,
    user_token: &Account<'info, TokenAccount>,
    market_vault: &Account<'info, TokenAccount>,
    user: &Signer<'info>,
    token_program: &Program<'info, Token>,
) -> Result<()> {
    require!(usd_in > 0, PmError::Zero);
    let fee = (usd_in as u128 * m.fee_bps as u128 / 10_000) as u64;
    let a = usd_in - fee;
    let n = m.n as usize;

    let (shares, new_ri) = calc_buy(&m.reserves[..n], n, outcome, a);
    require!(shares >= min_shares_out && shares > 0, PmError::Slippage);

    token::transfer(
        CpiContext::new(
            token_program.to_account_info(),
            Transfer {
                from: user_token.to_account_info(),
                to: market_vault.to_account_info(),
                authority: user.to_account_info(),
            },
        ),
        usd_in,
    )?;

    for j in 0..n {
        if j != outcome {
            m.reserves[j] += a;
        }
    }
    m.reserves[outcome] = new_ri;
    m.real_usdc += a;
    m.vig_acc += fee;
    m.volume += usd_in;
    m.q_out[outcome] += shares;
    pos.shares[outcome] += shares;
    pos.net_paid += usd_in as i64;
    Ok(())
}

fn transfer_from_vault<'info>(
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    auth: &UncheckedAccount<'info>,
    token_program: &Program<'info, Token>,
    bump: u8,
    amount: u64,
) -> Result<()> {
    let seeds: &[&[u8]] = &[b"auth", &[bump]];
    token::transfer(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            Transfer {
                from: from.to_account_info(),
                to: to.to_account_info(),
                authority: auth.to_account_info(),
            },
            &[seeds],
        ),
        amount,
    )
}

// ---------------- 账户 ----------------

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub authority: Pubkey,
    pub usdc_mint: Pubkey,
    pub auth_bump: u8,
    pub total_reserved: u64,
    pub void_threshold: u64,
    pub min_creator_stake: u64,
    pub creator_cap: u8,
    pub market_count: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Market {
    pub id: u64,
    pub question_id: [u8; 32],
    pub creator: Pubkey,
    pub end_ts: i64,
    pub fee_bps: u16,
    pub n: u8,
    pub winner: u8,
    pub status: u8,
    pub n_max: u64,
    pub real_usdc: u64,
    pub vig_acc: u64,
    pub volume: u64,
    pub earmark_remaining: u64,
    pub reserves: [u64; 8],
    pub q_out: [u64; 8],
}

#[account]
#[derive(InitSpace)]
pub struct UserPosition {
    pub shares: [u64; 8],
    pub net_paid: i64,
}

#[account]
#[derive(InitSpace)]
pub struct CreatorStats {
    pub active: u8,
}

/// mock 预言机结果（存在即已判）
#[account]
#[derive(InitSpace)]
pub struct OracleResult {
    pub winner: u8,
}

// ---------------- Contexts ----------------

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = authority, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: 全局签名 PDA，仅作 token authority
    #[account(seeds = [b"auth"], bump)]
    pub auth: UncheckedAccount<'info>,
    pub usdc_mint: Account<'info, Mint>,
    #[account(init, payer = authority, seeds = [b"xlp_vault"], bump,
        token::mint = usdc_mint, token::authority = auth)]
    pub xlp_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FundVault<'info> {
    #[account(mut, seeds = [b"xlp_vault"], bump)]
    pub xlp_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub funder_token: Account<'info, TokenAccount>,
    pub funder: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct CreateMarket<'info> {
    #[account(mut, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: 签名 PDA
    #[account(seeds = [b"auth"], bump = config.auth_bump)]
    pub auth: UncheckedAccount<'info>,
    #[account(init, payer = creator, space = 8 + Market::INIT_SPACE,
        seeds = [b"market", market_id.to_le_bytes().as_ref()], bump)]
    pub market: Account<'info, Market>,
    #[account(init, payer = creator, seeds = [b"mvault", market_id.to_le_bytes().as_ref()], bump,
        token::mint = usdc_mint, token::authority = auth)]
    pub market_vault: Account<'info, TokenAccount>,
    #[account(seeds = [b"xlp_vault"], bump)]
    pub xlp_vault: Account<'info, TokenAccount>,
    #[account(init_if_needed, payer = creator, space = 8 + CreatorStats::INIT_SPACE,
        seeds = [b"creator", creator.key().as_ref()], bump)]
    pub creator_stats: Account<'info, CreatorStats>,
    #[account(init_if_needed, payer = creator, space = 8 + UserPosition::INIT_SPACE,
        seeds = [b"pos", market_id.to_le_bytes().as_ref(), creator.key().as_ref()], bump)]
    pub creator_position: Account<'info, UserPosition>,
    #[account(mut, token::mint = usdc_mint)]
    pub creator_token: Account<'info, TokenAccount>,
    pub usdc_mint: Account<'info, Mint>,
    #[account(mut)]
    pub creator: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct Trade<'info> {
    #[account(seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: 签名 PDA
    #[account(seeds = [b"auth"], bump = config.auth_bump)]
    pub auth: UncheckedAccount<'info>,
    #[account(mut, seeds = [b"market", market_id.to_le_bytes().as_ref()], bump)]
    pub market: Account<'info, Market>,
    #[account(mut, seeds = [b"mvault", market_id.to_le_bytes().as_ref()], bump)]
    pub market_vault: Account<'info, TokenAccount>,
    #[account(init_if_needed, payer = user, space = 8 + UserPosition::INIT_SPACE,
        seeds = [b"pos", market_id.to_le_bytes().as_ref(), user.key().as_ref()], bump)]
    pub position: Account<'info, UserPosition>,
    #[account(mut)]
    pub user_token: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(question_id: [u8; 32])]
pub struct SetOutcome<'info> {
    #[account(seeds = [b"config"], bump, has_one = authority)]
    pub config: Account<'info, Config>,
    #[account(init_if_needed, payer = authority, space = 8 + OracleResult::INIT_SPACE,
        seeds = [b"oracle", question_id.as_ref()], bump)]
    pub oracle_result: Account<'info, OracleResult>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct SettleResolved<'info> {
    #[account(mut, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: 签名 PDA
    #[account(seeds = [b"auth"], bump = config.auth_bump)]
    pub auth: UncheckedAccount<'info>,
    #[account(mut, seeds = [b"market", market_id.to_le_bytes().as_ref()], bump)]
    pub market: Account<'info, Market>,
    #[account(mut, seeds = [b"mvault", market_id.to_le_bytes().as_ref()], bump)]
    pub market_vault: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"xlp_vault"], bump)]
    pub xlp_vault: Account<'info, TokenAccount>,
    #[account(seeds = [b"oracle", market.question_id.as_ref()], bump)]
    pub oracle_result: Account<'info, OracleResult>,
    #[account(mut, seeds = [b"creator", market.creator.as_ref()], bump)]
    pub creator_stats: Account<'info, CreatorStats>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct SettleVoid<'info> {
    #[account(seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"market", market_id.to_le_bytes().as_ref()], bump)]
    pub market: Account<'info, Market>,
    #[account(mut, seeds = [b"creator", market.creator.as_ref()], bump)]
    pub creator_stats: Account<'info, CreatorStats>,
}

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct ClaimCtx<'info> {
    #[account(seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: 签名 PDA
    #[account(seeds = [b"auth"], bump = config.auth_bump)]
    pub auth: UncheckedAccount<'info>,
    #[account(mut, seeds = [b"market", market_id.to_le_bytes().as_ref()], bump)]
    pub market: Account<'info, Market>,
    #[account(mut, seeds = [b"mvault", market_id.to_le_bytes().as_ref()], bump)]
    pub market_vault: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"pos", market_id.to_le_bytes().as_ref(), user.key().as_ref()], bump)]
    pub position: Account<'info, UserPosition>,
    #[account(mut)]
    pub user_token: Account<'info, TokenAccount>,
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct RefundCtx<'info> {
    #[account(mut, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: 签名 PDA
    #[account(seeds = [b"auth"], bump = config.auth_bump)]
    pub auth: UncheckedAccount<'info>,
    #[account(mut, seeds = [b"market", market_id.to_le_bytes().as_ref()], bump)]
    pub market: Account<'info, Market>,
    #[account(mut, seeds = [b"mvault", market_id.to_le_bytes().as_ref()], bump)]
    pub market_vault: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"xlp_vault"], bump)]
    pub xlp_vault: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"pos", market_id.to_le_bytes().as_ref(), user.key().as_ref()], bump)]
    pub position: Account<'info, UserPosition>,
    #[account(mut)]
    pub user_token: Account<'info, TokenAccount>,
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

// ---------------- 错误 ----------------

#[error_code]
pub enum PmError {
    #[msg("outcome count must be 2..8")]
    BadOutcomeCount,
    #[msg("fee too high")]
    FeeTooHigh,
    #[msg("end date in past")]
    EndInPast,
    #[msg("zero seed")]
    ZeroSeed,
    #[msg("bad outcome index")]
    BadOutcome,
    #[msg("market id must be sequential")]
    BadMarketId,
    #[msg("creator cap reached")]
    CreatorCap,
    #[msg("creator stake too low")]
    StakeTooLow,
    #[msg("prior out of [2%,96%]")]
    PriorOutOfRange,
    #[msg("priors must sum to 1e6")]
    PriorsMustSumToOne,
    #[msg("xLP budget exhausted")]
    BudgetExhausted,
    #[msg("not trading")]
    NotTrading,
    #[msg("zero amount")]
    Zero,
    #[msg("slippage")]
    Slippage,
    #[msg("cash guard")]
    CashGuard,
    #[msg("insufficient shares")]
    InsufficientShares,
    #[msg("market not open")]
    NotOpen,
    #[msg("awaiting oracle")]
    AwaitingOracle,
    #[msg("not resolved")]
    NotResolved,
    #[msg("not void")]
    NotVoid,
    #[msg("nothing to claim")]
    Nothing,
    #[msg("THEOREM VIOLATION: shortfall exceeds earmark")]
    TheoremViolation,
}
