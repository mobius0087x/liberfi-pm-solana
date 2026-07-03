// Node --require 预加载：让内置 fetch/undici 走 HTTPS_PROXY（官方 npx 工具默认不认代理）
try {
  const u = require("undici");
  const p = process.env.HTTPS_PROXY || process.env.https_proxy;
  if (p) { u.setGlobalDispatcher(new u.ProxyAgent(p)); }
} catch (_) {}
