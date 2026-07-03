#!/usr/bin/env node
/**
 * Liberfi PM · devnet 实盘冒烟 v2（纯 HTTP 轮询确认版）
 * 网络约束：本机必须走 VeeeVPN 代理（HTTPS_PROXY），wss 不可用 →
 *  - undici ProxyAgent 接管全部 fetch（HTTP RPC 可用）
 *  - 弃用 web3.js/anchor 的 WS 确认与 SPL sendAndConfirm，全部手工拼指令 + getSignatureStatuses 轮询
 * 场景（同 v1）：mUSDC 测试币 → initialize+注资 → M1 精确向量+定理+resolved →
 *              M2 B类押注建盘+庄赢 → M3 流拍全退。归档 ../2026-07-03/devnet-smoke-results.json
 */
const undici = require("undici");
{
  const proxy = process.env.HTTPS_PROXY || process.env.https_proxy;
  if (proxy) { undici.setGlobalDispatcher(new undici.ProxyAgent(proxy)); console.log("[proxy]", proxy); }
}
// 代理链路抖动免疫：所有 RPC 走 5 次退避重试的 fetch
const sleep0 = (ms) => new Promise((r) => setTimeout(r, ms));
async function retryFetch(url, opts) {
  let last;
  for (let i = 0; i < 5; i++) {
    try { return await undici.fetch(url, opts); } catch (e) { last = e; await sleep0(800 * (i + 1)); }
  }
  throw last;
}
const anchor = require("@coral-xyz/anchor");
const {
  Connection, Keypair, PublicKey, SystemProgram, LAMPORTS_PER_SOL, Transaction,
} = require("@solana/web3.js");
const spl = require("@solana/spl-token");
const fs = require("fs");
const os = require("os");
const path = require("path");

const RPC = "https://api.devnet.solana.com";
const IDL = JSON.parse(fs.readFileSync(path.join(__dirname, "../target/idl/liberfi_pm.json")));
const OUT = path.join(__dirname, "../../2026-07-03/devnet-smoke-results.json");

const archive = { startedAt: new Date().toISOString(), cluster: "devnet", programId: IDL.address, steps: [], assertions: [] };
const log = (step, data) => { console.log(`[${step}]`, JSON.stringify(data)); archive.steps.push({ step, ...data }); };
const assert = (name, cond, detail) => {
  archive.assertions.push({ name, pass: !!cond, detail });
  if (!cond) throw new Error(`ASSERT FAIL: ${name} — ${JSON.stringify(detail)}`);
  console.log(`  ✓ ${name}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  const deployer = Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(os.homedir() + "/.config/solana/id.json")))
  );
  const connection = new Connection(RPC, { commitment: "confirmed", fetch: retryFetch });
  const wallet = new anchor.Wallet(deployer);
  const provider = new anchor.AnchorProvider(connection, wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);
  const program = new anchor.Program(IDL, provider);
  const pid = program.programId;
  const BN = anchor.BN;

  // ---- 纯 HTTP 发送 + 轮询确认（绕开 wss） ----
  async function sendIxs(label, ixs, signers = []) {
    const tx = new Transaction().add(...ixs);
    tx.feePayer = deployer.publicKey;
    tx.recentBlockhash = (await connection.getLatestBlockhash("confirmed")).blockhash;
    tx.sign(deployer, ...signers);
    const sig = await connection.sendRawTransaction(tx.serialize(), { skipPreflight: false });
    process.stdout.write(`  [${label}] ${sig.slice(0, 16)}… `);
    for (let i = 0; i < 90; i++) {
      await sleep(2000);
      const st = (await connection.getSignatureStatuses([sig])).value[0];
      if (st && (st.confirmationStatus === "confirmed" || st.confirmationStatus === "finalized")) {
        if (st.err) throw new Error(`${label} tx failed on-chain: ${JSON.stringify(st.err)} ${sig}`);
        console.log("✓");
        return sig;
      }
    }
    throw new Error(`${label} poll timeout: ${sig}`);
  }
  const sendIx = async (label, builder, signers = []) => sendIxs(label, [await builder.instruction()], signers);

  // ---- PDA helpers ----
  const idBuf = (id) => new BN(id).toArrayLike(Buffer, "le", 8);
  const [config] = PublicKey.findProgramAddressSync([Buffer.from("config")], pid);
  const [auth] = PublicKey.findProgramAddressSync([Buffer.from("auth")], pid);
  const [xlpVault] = PublicKey.findProgramAddressSync([Buffer.from("xlp_vault")], pid);
  const marketPda = (id) => PublicKey.findProgramAddressSync([Buffer.from("market"), idBuf(id)], pid)[0];
  const mvaultPda = (id) => PublicKey.findProgramAddressSync([Buffer.from("mvault"), idBuf(id)], pid)[0];
  const posPda = (id, user) => PublicKey.findProgramAddressSync([Buffer.from("pos"), idBuf(id), user.toBuffer()], pid)[0];
  const creatorPda = (user) => PublicKey.findProgramAddressSync([Buffer.from("creator"), user.toBuffer()], pid)[0];
  const oraclePda = (qidArr) => PublicKey.findProgramAddressSync([Buffer.from("oracle"), Buffer.from(qidArr)], pid)[0];
  const qid = (label) => { const b = Buffer.alloc(32); b.write(label); return Array.from(b); };

  // ---- 幂等续接 ----
  let mint = null, needInit = true, baseId = 0;
  if (await connection.getAccountInfo(config)) {
    const cfg0 = await program.account.config.fetch(config);
    mint = cfg0.usdcMint; needInit = false; baseId = Number(cfg0.marketCount);
    console.log("[resume] config exists → reuse mint", mint.toBase58(), "marketCount", baseId);
  }
  const id1 = baseId + 1, id2 = baseId + 2, id3 = baseId + 3;

  // ---- 0) 测试代币 mUSDC + 角色分发（全部手工指令） ----
  const alice = Keypair.generate();
  if (!mint) {
    const mintKp = Keypair.generate();
    const rentMin = await spl.getMinimumBalanceForRentExemptMint(connection);
    await sendIxs("create_mint+fund_alice", [
      SystemProgram.transfer({ fromPubkey: deployer.publicKey, toPubkey: alice.publicKey, lamports: 0.1 * LAMPORTS_PER_SOL }),
      SystemProgram.createAccount({
        fromPubkey: deployer.publicKey, newAccountPubkey: mintKp.publicKey,
        space: spl.MINT_SIZE, lamports: rentMin, programId: spl.TOKEN_PROGRAM_ID,
      }),
      spl.createInitializeMint2Instruction(mintKp.publicKey, 6, deployer.publicKey, null),
    ], [mintKp]);
    mint = mintKp.publicKey;
  } else {
    await sendIxs("fund_alice_sol", [
      SystemProgram.transfer({ fromPubkey: deployer.publicKey, toPubkey: alice.publicKey, lamports: 0.1 * LAMPORTS_PER_SOL }),
    ]);
  }
  const deployerAta = spl.getAssociatedTokenAddressSync(mint, deployer.publicKey);
  const aliceAta = spl.getAssociatedTokenAddressSync(mint, alice.publicKey);
  await sendIxs("atas_and_mint", [
    spl.createAssociatedTokenAccountIdempotentInstruction(deployer.publicKey, deployerAta, deployer.publicKey, mint),
    spl.createAssociatedTokenAccountIdempotentInstruction(deployer.publicKey, aliceAta, alice.publicKey, mint),
    spl.createMintToInstruction(mint, deployerAta, deployer.publicKey, 100_000_000_000n),
    spl.createMintToInstruction(mint, aliceAta, deployer.publicKey, 20_000_000_000n),
  ]);
  log("mint", { mUSDC: mint.toBase58(), alice: alice.publicKey.toBase58() });

  const bal = async (ata) => BigInt((await connection.getTokenAccountBalance(ata)).value.amount);
  const vaultBal = async () => BigInt((await connection.getTokenAccountBalance(xlpVault)).value.amount);

  // ---- 1) initialize + fund_vault ----
  if (needInit) {
    const sig = await sendIx("initialize", program.methods
      .initialize(new BN(50_000_000), new BN(10_000_000), 5)
      .accounts({
        config, auth, usdcMint: mint, xlpVault,
        authority: deployer.publicKey,
        tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
      }));
    log("initialize", { sig });
  }
  {
    const sig = await sendIx("fund_vault", program.methods
      .fundVault(new BN(5_000_000_000))
      .accounts({ xlpVault, funderToken: deployerAta, funder: deployer.publicKey, tokenProgram: spl.TOKEN_PROGRAM_ID }));
    log("fund_vault_$5k", { sig, vaultBalance: (await vaultBal()).toString() });
  }

  // ---- 2) M1: A 类足球 1X2 ----
  const Q1 = qid(`devnet-football-${id1}`);
  let sig = await sendIx("create_M1", program.methods
    .createMarket(new BN(id1), Q1, [450_000, 280_000, 270_000], new BN(1_000_000_000), 250,
      new BN(Math.floor(Date.now() / 1000) + 3600), 0, new BN(0))
    .accounts({
      config, auth, market: marketPda(id1), marketVault: mvaultPda(id1), xlpVault,
      creatorStats: creatorPda(deployer.publicKey), creatorPosition: posPda(id1, deployer.publicKey),
      creatorToken: deployerAta, usdcMint: mint, creator: deployer.publicKey,
      tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    }));
  let m1 = await program.account.market.fetch(marketPda(id1));
  log("create_M1_football", { sig, id: id1, reserves: m1.reserves.slice(0, 3).map(String) });
  assert("M1 seeding = (600e6, 964285714, 1000e6)",
    m1.reserves[0].eq(new BN(600_000_000)) && m1.reserves[1].eq(new BN(964_285_714)) && m1.reserves[2].eq(new BN(1_000_000_000)),
    m1.reserves.slice(0, 3).map(String));

  sig = await sendIx("M1_buy200", program.methods
    .buy(new BN(id1), 1, new BN(200_000_000), new BN(0))
    .accounts({
      config, auth, market: marketPda(id1), marketVault: mvaultPda(id1),
      position: posPda(id1, alice.publicKey), userToken: aliceAta, user: alice.publicKey,
      tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    }), [alice]);
  let pos1 = await program.account.userPosition.fetch(posPda(id1, alice.publicKey));
  log("M1_buy_$200_draw", { sig, shares: pos1.shares[1].toString() });
  assert("跨实现精确向量: shares == 550,279,183（devnet == Solidity == Rust）",
    pos1.shares[1].eq(new BN(550_279_183)), pos1.shares[1].toString());

  sig = await sendIx("M1_buy10k", program.methods
    .buy(new BN(id1), 1, new BN(10_000_000_000), new BN(0))
    .accounts({
      config, auth, market: marketPda(id1), marketVault: mvaultPda(id1),
      position: posPda(id1, alice.publicKey), userToken: aliceAta, user: alice.publicKey,
      tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    }), [alice]);
  m1 = await program.account.market.fetch(marketPda(id1));
  const owedD = BigInt(m1.qOut[1].toString());
  const realU = BigInt(m1.realUsdc.toString());
  const shortfall = owedD > realU ? owedD - realU : 0n;
  log("M1_onesided_$10k", { sig, qOutD: owedD.toString(), realUsdc: realU.toString(), shortfallIfDWins: shortfall.toString() });
  assert("定理: 单边 $10k 后 shortfall ≤ n=1000e6", shortfall <= 1_000_000_000n, shortfall.toString());
  assert("定理下界核对（~959e6 区间）", shortfall >= 900_000_000n, shortfall.toString());

  const vaultBefore = await vaultBal();
  await sendIx("M1_set_outcome", program.methods.setOutcome(Q1, 1).accounts({
    config, oracleResult: oraclePda(Q1), authority: deployer.publicKey, systemProgram: SystemProgram.programId,
  }));
  sig = await sendIx("M1_settle", program.methods.settleResolved(new BN(id1)).accounts({
    config, auth, market: marketPda(id1), marketVault: mvaultPda(id1), xlpVault,
    oracleResult: oraclePda(Q1), creatorStats: creatorPda(deployer.publicKey), tokenProgram: spl.TOKEN_PROGRAM_ID,
  }));
  const vaultAfter = await vaultBal();
  const vig1 = 200_000_000n * 250n / 10_000n + 10_000_000_000n * 250n / 10_000n;
  log("M1_settle_resolved", { sig, vaultDelta: (vaultAfter - vaultBefore).toString(), expected: (vig1 - shortfall).toString() });
  assert("M1 金库账实: Δvault == vig − shortfall", vaultAfter - vaultBefore === vig1 - shortfall,
    { delta: (vaultAfter - vaultBefore).toString(), vig: vig1.toString(), shortfall: shortfall.toString() });

  const aliceBefore = await bal(aliceAta);
  sig = await sendIx("M1_claim", program.methods.claim(new BN(id1)).accounts({
    config, auth, market: marketPda(id1), marketVault: mvaultPda(id1),
    position: posPda(id1, alice.publicKey), userToken: aliceAta, user: alice.publicKey,
    tokenProgram: spl.TOKEN_PROGRAM_ID,
  }), [alice]);
  const aliceAfter = await bal(aliceAta);
  log("M1_claim", { sig, received: (aliceAfter - aliceBefore).toString() });
  assert("M1 claim 全额兑付（每份 $1）", aliceAfter - aliceBefore === owedD, (aliceAfter - aliceBefore).toString());

  // ---- 3) M2: B 类创建者押注定价 + 庄赢 residual ----
  const Q2 = qid(`devnet-btrack-${id2}`);
  sig = await sendIx("create_M2", program.methods
    .createMarket(new BN(id2), Q2, [500_000, 500_000], new BN(250_000_000), 250,
      new BN(Math.floor(Date.now() / 1000) + 3600), 0, new BN(10_000_000))
    .accounts({
      config, auth, market: marketPda(id2), marketVault: mvaultPda(id2), xlpVault,
      creatorStats: creatorPda(alice.publicKey), creatorPosition: posPda(id2, alice.publicKey),
      creatorToken: aliceAta, usdcMint: mint, creator: alice.publicKey,
      tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    }), [alice]);
  log("create_M2_btrack_stake$10", { sig, id: id2 });

  await sendIx("M2_noise", program.methods.buy(new BN(id2), 0, new BN(500_000_000), new BN(0)).accounts({
    config, auth, market: marketPda(id2), marketVault: mvaultPda(id2),
    position: posPda(id2, deployer.publicKey), userToken: deployerAta, user: deployer.publicKey,
    tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }));
  const v2Before = await vaultBal();
  await sendIx("M2_set_outcome", program.methods.setOutcome(Q2, 1).accounts({
    config, oracleResult: oraclePda(Q2), authority: deployer.publicKey, systemProgram: SystemProgram.programId,
  }));
  sig = await sendIx("M2_settle", program.methods.settleResolved(new BN(id2)).accounts({
    config, auth, market: marketPda(id2), marketVault: mvaultPda(id2), xlpVault,
    oracleResult: oraclePda(Q2), creatorStats: creatorPda(alice.publicKey), tokenProgram: spl.TOKEN_PROGRAM_ID,
  }));
  const v2After = await vaultBal();
  log("M2_settle_crowd_wrong", { sig, vaultDelta: (v2After - v2Before).toString() });
  assert("M2 庄赢: residual+vig 归金库（>0）", v2After - v2Before > 0n, (v2After - v2Before).toString());

  // ---- 4) M3: 流拍全退 ----
  const Q3 = qid(`devnet-void-${id3}`);
  const endTs3 = Math.floor(Date.now() / 1000) + 45;
  sig = await sendIx("create_M3", program.methods
    .createMarket(new BN(id3), Q3, [500_000, 500_000], new BN(250_000_000), 250, new BN(endTs3), 0, new BN(0))
    .accounts({
      config, auth, market: marketPda(id3), marketVault: mvaultPda(id3), xlpVault,
      creatorStats: creatorPda(deployer.publicKey), creatorPosition: posPda(id3, deployer.publicKey),
      creatorToken: deployerAta, usdcMint: mint, creator: deployer.publicKey,
      tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    }));
  await sendIx("M3_trickle", program.methods.buy(new BN(id3), 0, new BN(20_000_000), new BN(0)).accounts({
    config, auth, market: marketPda(id3), marketVault: mvaultPda(id3),
    position: posPda(id3, alice.publicKey), userToken: aliceAta, user: alice.publicKey,
    tokenProgram: spl.TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  }), [alice]);
  log("create_M3_void_track", { sig, id: id3, endTs: endTs3, waiting: "~50s" });
  await sleep(52_000);

  sig = await sendIx("M3_settle_void", program.methods.settleVoid(new BN(id3)).accounts({
    config, market: marketPda(id3), creatorStats: creatorPda(deployer.publicKey),
  }));
  const a3Before = await bal(aliceAta);
  const refundSig = await sendIx("M3_refund", program.methods.claimRefund(new BN(id3)).accounts({
    config, auth, market: marketPda(id3), marketVault: mvaultPda(id3), xlpVault,
    position: posPda(id3, alice.publicKey), userToken: aliceAta, user: alice.publicKey,
    tokenProgram: spl.TOKEN_PROGRAM_ID,
  }), [alice]);
  const a3After = await bal(aliceAta);
  log("M3_void_and_refund", { settleSig: sig, refundSig, refunded: (a3After - a3Before).toString() });
  assert("M3 流拍全退含 vig（$20 → $20）", a3After - a3Before === 20_000_000n, (a3After - a3Before).toString());

  // ---- 收尾归档 ----
  const cfg = await program.account.config.fetch(config);
  archive.finishedAt = new Date().toISOString();
  archive.finalState = {
    xlpVaultBalance: (await vaultBal()).toString(),
    totalReserved: cfg.totalReserved.toString(),
    marketCount: cfg.marketCount.toString(),
    mUSDC: mint.toBase58(),
  };
  archive.summary = `${archive.assertions.length} assertions, all pass=${archive.assertions.every((a) => a.pass)}`;
  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, JSON.stringify(archive, null, 1));
  console.log(`\n==== DEVNET SMOKE COMPLETE ====\n${archive.summary}\narchived → ${OUT}`);
})().catch((e) => {
  archive.error = String(e.message || e);
  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, JSON.stringify(archive, null, 1));
  console.error("SMOKE FAILED:", e.message || e);
  process.exit(1);
});
