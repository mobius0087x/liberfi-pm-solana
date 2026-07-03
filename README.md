# Liberfi 长尾预测市场 · Solana/Anchor 版（Phase 1 对等实现）

与 EVM 版（`../contracts/`）功能对等：N 元 mint-then-swap 曲线 + xLP earmark（方案 A）+ 三终局结算 + 创建者押注定价 + per-creator cap。对应主报告 §3.11 / §3.3 / §3.5 / evidence/D §B2。

## 结构

| 文件 | 职责 |
|---|---|
| `programs/liberfi-pm/src/math.rs` | 纯数学模块（seeding / calc_buy / calc_sell / prices），**与 EVM 版逐位对齐**，`cargo test` 直接验证 |
| `programs/liberfi-pm/src/lib.rs` | Anchor 程序：initialize / fund_vault / create_market / buy / sell / set_outcome（mock 预言机）/ settle_resolved / settle_void / claim / claim_refund |

账户模型：Config + 全局签名 PDA("auth") 持有 `xlp_vault` 与各 `mvault`（SPL token 账户，对应 EVM 的 XlpVault/市场余额分账）；Market 定长数组（≤8 outcome）；UserPosition 内部账本；OracleResult PDA 存在即已判（Phase 2 换 Switchboard/Pyth）。

## 跨实现一致性（关键测试）

两条链锁定同一精确向量：足球 1X2（0.45/0.28/0.27，n=\$1,000，\$200 买平局，vig 2.5%）→ **sharesOut = 550,279,183**：
- Solidity：`test_BuyMatchesPythonVector`（assertEq 精确断言）
- Rust：`buy_matches_python_vector`（assert_eq! 同值）
- 定理 fuzz 双侧全过（Solidity 2,000 轮 / Rust 500 组随机序列）：任意路径 shortfall ≤ n_max

## 命令

```bash
export PATH="$HOME/.local/share/solana/install/active_release/bin:$HOME/.cargo/bin:$PATH"
cargo test -p liberfi-pm --lib     # 数学模块 6 测试（无需 solana 工具链）
anchor build --no-idl              # SBF 产物（磁盘紧张时跳过 IDL；需要 IDL 时去掉 --no-idl）
```

## Devnet 部署

```bash
solana config set --url devnet
solana airdrop 2                   # 限流时用 faucet.solana.com（GitHub 登录）
# 458K 程序 rent ≈3.2 SOL，建议备 ≥4 SOL
anchor keys sync                   # 真实 program id 写入 declare_id/Anchor.toml
anchor build --no-idl && anchor deploy --provider.cluster devnet
```

部署者钱包：`~/.config/solana/id.json`（地址见 `solana address`）。

## 与 EVM 版的差异点（审阅须知）

1. 结算拆成 `settle_resolved` / `settle_void` 两个指令（Solana 账户模型下比单入口干净）；
2. vig/residual/shortfall 资金流经 SPL transfer + PDA 签名（`auth` seeds）；
3. `emergencyVoid`/`sweepVoid` 未包含（testnet 精简，Phase 1.1 补）；
4. anchor-lang 0.31.1 在 anchor-cli 1.1.2 下构建有 deprecated 告警（不影响产物），升 1.x 时同步清理。

## Phase 2 待接

Switchboard/Pyth 结算适配、buyComplement、毕业/CLOB（Solana 侧可对接 OpenBook/Phoenix）、TS 集成测试（`tests/`）。

---

## On-chain (Devnet)

| | |
|---|---|
| Program | `5oBHzLgzE3X8QKQDspH2gCN34g1tbhnKs9dXxQ8KM8Sk` |
| Upgrade authority | `BuftLqdxDkVSconLCCKqyWQiZzATUf8zsS5v3QbaWxd` |
| IDL | `idl/liberfi_pm.json`（同步发布于链上 anchor IDL 账户 + program-metadata） |
| Test collateral (mUSDC, 6 dec) | `HCTgEu1guZo5NecM7VJ3qumDH7nUeDCzfVHTR4XHVpZp` |
| 实盘冒烟 | 8/8 断言（三终局全生命周期），见 `scripts/devnet-smoke.cjs` |

## Security

Phase 1 测试网原型，**未审计**，请勿用于主网/真实资金。安全问题请提 [GitHub Issues](https://github.com/mobius0087x/liberfi-pm-solana/issues)。程序内嵌 [security.txt](https://github.com/neodyme-labs/solana-security-txt)。

## Verified build

本机无 docker，可复现验证命令（任何人可执行）：

```bash
solana-verify build --library-name liberfi_pm
solana-verify verify-from-repo --url devnet --program-id 5oBHzLgzE3X8QKQDspH2gCN34g1tbhnKs9dXxQ8KM8Sk \
  https://github.com/mobius0087x/liberfi-pm-solana
```
