// 携程酒店评论爬虫（正式版）。需先运行 ./trip hotel login 保存登录态。
// 用法：node crawl.mjs <hotelId|URL> [--out DIR] [--dislike 15] [--good 10] [--images 5]
//
// 安全间隔（防风控，勿调小）：
// - 页面滚动每轮 900ms（懒加载逐页请求）
// - 阶段切换（detail→列表、好评→差评）强制 2~3s
// - 图片下载每张 400ms
// - 一家酒店只爬一次；同一酒店重复爬无意义且增加风控暴露
// 输出：JSON（stdout）+ 图片落盘 out 目录。全程只读、慢速滚动。
// 流程：detail 页抓元数据（名称/评分/开业/亮点标签）→ 评论列表页（头部统计+好评关键词）
//       → 点「差评」chip 滚动收集（图优先/字数次之排序取 topN）→ 回「全部」收好评（按字数 topN）
//       → 图片下载。
import { chromium } from "playwright";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const STATE_FILE = path.join(HERE, ".ctrip-state.json");
const UA =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

// ---- 参数 ----
const argv = process.argv.slice(2);
const argOf = (flag, def) => {
  const i = argv.indexOf(flag);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : def;
};
const rawId = argv[0];
if (!rawId) {
  console.error("用法: node crawl.mjs <hotelId|URL> [--out DIR] [--dislike 15] [--good 10] [--images 30]");
  process.exit(1);
}
const hotelId = (rawId.match(/hotelId=(\d+)/) || rawId.match(/hotels\/(\d+)\.html/) || [null, rawId])[1];
const OUT_DIR = argOf("--out", path.join(HERE, "out", hotelId));
const DISLIKE_N = parseInt(argOf("--dislike", "15"), 10);
const GOOD_N = parseInt(argOf("--good", "10"), 10);
const IMAGE_N = parseInt(argOf("--images", "5"), 10);
const iso = (d) => d.toISOString().slice(0, 10);
const CHECKIN = argOf("--checkin") || iso(new Date(Date.now() + 7 * 86400000));
const CHECKOUT = argOf("--checkout") || iso(new Date(Date.now() + 8 * 86400000));

if (!fs.existsSync(STATE_FILE)) {
  console.error("缺登录态文件 .ctrip-state.json，请先运行 ./trip hotel login");
  process.exit(1);
}
fs.mkdirSync(OUT_DIR, { recursive: true });
const IMG_DIR = path.join(OUT_DIR, "images");
fs.mkdirSync(IMG_DIR, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const browser = await chromium.launch({ headless: true });
const ctx = await browser.newContext({
  userAgent: UA,
  viewport: { width: 390, height: 844 },
  isMobile: true,
  hasTouch: true,
  locale: "zh-CN",
  storageState: STATE_FILE,
});
const page = await ctx.newPage();
page.setDefaultTimeout(30000);

const result = { hotelId, name: null, rating: null, subRatings: {}, reviewCount: null, opened: null, tags: [], keywordTags: [], aiSummary: null, dislikeCount: null, dislikes: [], good: [], imageDir: IMG_DIR, images: [] };

try {
  // ===== ① detail 页：元数据 =====
  await page.goto(
    `https://m.ctrip.com/webapp/hotel/hoteldetail/${hotelId}.html?checkin=${CHECKIN}&checkout=${CHECKOUT}`,
    { waitUntil: "domcontentloaded", timeout: 60000 },
  );
  await page.waitForTimeout(5000);
  const meta = await page.evaluate(() => document.title);
  result.name = meta.match(/携程酒店-(.+?)(预订|价格|电话)/)?.[1] || null;
  const t0 = await page.evaluate(() => document.body.innerText);
  result.rating = t0.match(/([0-5]\.\d)\s*\n\s*(很好|棒|不错|一般)/)?.[1] || null;
  result.reviewCount = t0.match(/([\d.]+万?)\s*条/)?.[1] || null;
  result.opened = t0.match(/(20\d{2}年(?:开业|装修|重装)[^,\n]{0,10})/)?.[1] || null;
  // 亮点标签：detail 页头部「家庭房 | 拍照出片 | 设计师酒店」这类 2~6 字短标签
  const TAG_NOISE = /^(地图|设施|政策|点评|亮点|房型|周边|位置|相册|封面|精选|预订|规则|图片加载中|查看更多|很好|明天|今天|酒店|开业|装修)$/;
  result.tags = [
    ...new Set(
      [...t0.slice(0, 4000).matchAll(/\n([\u4e00-\u9fa5]{2,6})\n/g)].map((m) => m[1]).filter((w) => !TAG_NOISE.test(w)),
    ),
  ].slice(0, 10);
  // ===== ①b 房型价格：点「房型」tab，收集 name+起价（带日期的实时价）=====
  await page.evaluate(() => {
    for (const el of document.querySelectorAll("a,button,div,span,li")) {
      if ((el.innerText || "").trim() === "房型" && el.getBoundingClientRect().height > 0) {
        el.scrollIntoView({ block: "center" });
        el.click();
        break;
      }
    }
  });
  await sleep(4000);
  for (let i = 0; i < 3; i++) {
    await page.evaluate(() => {
      let best = null;
      for (const el of document.querySelectorAll("*")) {
        if (el.scrollHeight > el.clientHeight + 100 && el.clientHeight > 300 && (!best || el.scrollHeight > best.scrollHeight)) best = el;
      }
      if (best) best.scrollTop = best.scrollHeight;
      window.scrollTo(0, document.body.scrollHeight);
    });
    await sleep(800);
  }
  result.rooms = await page.evaluate(() => {
    const isRoomName = (l) =>
      /房/.test(l) &&
      /[\u4e00-\u9fa5]{3,}/.test(l) &&
      !l.includes("¥") &&
      l.length <= 50 &&
      !/升级|优惠|连住|仅剩|取消|在线付|早餐|销量|评论/.test(l);
    const cards = [];
    for (const el of document.querySelectorAll("div")) {
      const t = el.innerText || "";
      if (!t.includes("¥") || t.length < 20 || t.length > 500) continue;
      const nameLine = t.split("\n").find(isRoomName);
      if (!nameLine) continue;
      // 最内层：子 div 不再同时满足「有 ¥ + 有房型名」
      let deepest = true;
      for (const c of el.querySelectorAll("div")) {
        const ct = c.innerText || "";
        if (ct.includes("¥") && ct.split("\n").find(isRoomName)) { deepest = false; break; }
      }
      if (!deepest) continue;
      const prices = [...t.matchAll(/¥\s*\n?\s*(\d+)/g)].map((m) => parseInt(m[1], 10));
      if (prices.length === 0) continue;
      cards.push({ name: nameLine.trim().slice(0, 40), price: Math.min(...prices) });
    }
    return cards;
  });
  // 同名房型去重留最低价，截 6 条
  const roomMap = new Map();
  for (const r of result.rooms || []) if (!roomMap.has(r.name) || roomMap.get(r.name).price > r.price) roomMap.set(r.name, r.price);
  result.rooms = [...roomMap.entries()].map(([name, price]) => ({ name, price })).slice(0, 6);
  await sleep(2000);

  // ===== ② 评论列表页：头部统计 =====
  await page.goto(`https://m.ctrip.com/h5/xHtlCommentList/list?id=${hotelId}&biz=false`, {
    waitUntil: "domcontentloaded",
    timeout: 60000,
    referer: `https://m.ctrip.com/webapp/hotel/hoteldetail/${hotelId}.html`,
  });
  await page.waitForTimeout(9000);
  const head = await page.evaluate(() => document.body.innerText.slice(0, 3500));
  if (/登录即可查看|立即登录/.test(head)) {
    console.error("登录态已失效：评论页要求登录。请运行 ./trip hotel login 重新扫码。");
    process.exit(2);
  }
  // 布局是「分数在上、标签在下」：匹配 (分数)(标签) 而非 (标签)(分数)
  for (const [k, key] of [["卫\\s*生", "卫生"], ["环\\s*境", "环境"], ["服\\s*务", "服务"], ["设\\s*施", "设施"]]) {
    const m = head.match(new RegExp(`([0-5]\\.\\d)\\s*\\n\\s*${k}`));
    if (m) result.subRatings[key] = parseFloat(m[1]);
  }
  const dis = head.match(/差评\s*\n?\s*(\d+)/);
  result.dislikeCount = dis ? parseInt(dis[1], 10) : null;
  // 好评关键词（带计数）：「前台热情1718」——全头扫描，计数 3 位以上过滤噪声
  result.keywordTags = [
    ...new Map(
      [...head.matchAll(/([\u4e00-\u9fa5]{2,8})(\d{3,5})/g)]
        .filter((m) => !/^(连续|位住客|与您|相似|由|发布|入住|评分|图片|北京|上海|地铁|酒店|设施|营业)/.test(m[1]))
        .map((m) => [m[1], parseInt(m[2], 10)]),
    ).entries(),
  ]
    .map(([word, count]) => ({ word, count }))
    .sort((a, b) => b.count - a.count)
    .slice(0, 10);
  const ai = head.match(/连续\d+位住客好评[^\n]*/);
  result.aiSummary = ai ? ai[0] : null;

  // ===== 滚动工具：找到内层滚动容器并拨到底 =====
  const scrollBottom = () =>
    page.evaluate(() => {
      let best = null;
      for (const el of document.querySelectorAll("*")) {
        if (el.scrollHeight > el.clientHeight + 100 && el.clientHeight > 300 && (!best || el.scrollHeight > best.scrollHeight)) best = el;
      }
      if (best) best.scrollTop = best.scrollHeight;
      window.scrollTo(0, document.body.scrollHeight);
    });

  // 卡片收集：含「入住」+分数的最内层 div；正文是卡片外的独立叶子节点（房型名竖排会打散 innerText，
  // 所以用 TreeWalker 按文档顺序把「≥15 个汉字的叶子节点」归属到它前面最近的卡片）
  const collectCards = () =>
    page.evaluate(() => {
      const cardEls = [];
      for (const el of document.querySelectorAll("div")) {
        const t = (el.innerText || "").trim();
        if (!/\d{4}年\d{1,2}月入住/.test(t) || !/[0-5]\.\d/.test(t) || t.length < 30 || t.length > 1200) continue;
        let deepest = true;
        for (const c of el.querySelectorAll("div")) {
          const ct = c.innerText || "";
          if (/\d{4}年\d{1,2}月入住/.test(ct) && /[0-5]\.\d/.test(ct)) {
            deepest = false;
            break;
          }
        }
        if (deepest) cardEls.push(el);
      }
      const cards = cardEls.map((el) => {
        const t = (el.innerText || "").trim();
        const score = parseFloat(t.match(/([0-5]\.\d)\s*\n?\s*分/)?.[1] ?? "NaN");
        return {
          score: Number.isNaN(score) ? null : score,
          date: t.match(/(20\d{2}年\d{1,2}月)入住/)?.[1] || null,
          province: t.match(/发布于([\u4e00-\u9fa5]{2,6})/)?.[1] || null,
          level: t.match(/(钻石|黑钻|铂金|黄金|白银|青铜)贵宾/)?.[0] || null,
          text: null,
          hotelReply: null,
          images: [...el.querySelectorAll("img")].map((im) => im.src).filter((s) => /dimg.*\.(jpg|jpeg|webp|png)/i.test(s) && !/icon|logo/i.test(s)),
        };
      });
      // 正文归属：文档顺序走叶子节点；「位于 card[i] 与 card[i+1] 之间」的长中文叶子 → card[i]。
      // 住客原文与「酒店回复」区分（回复以「酒店回复/尊敬的」开头）；各取最长。
      const aiRe = /用户普遍认为|连续\d+位住客|《.*》/;
      const isReply = (s) => /^(酒店回复|尊敬的|您好)/.test(s);
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_ELEMENT);
      let ci = -1;
      const cands = cardEls.map(() => []);
      while (walker.nextNode()) {
        const el = walker.currentNode;
        while (ci + 1 < cardEls.length && cardEls[ci + 1].compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING) ci++;
        if (ci < 0 || ci >= cards.length) continue;
        if (!(cardEls[ci].compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING)) continue; // 必须在卡片之后
        if (el.children.length > 0) continue;
        const tc = (el.textContent || "").trim();
        if (/[\u4e00-\u9fa5]{15,}/.test(tc) && tc.length < 900 && !aiRe.test(tc)) {
          cands[ci].push(tc);
        }
      }
      cardEls.forEach((_, i) => {
        const guest = cands[i].filter((s) => !isReply(s)).sort((a, b) => b.length - a.length)[0];
        const reply = cands[i].filter(isReply).sort((a, b) => b.length - a.length)[0];
        cards[i].text = guest || null;
        cards[i].hotelReply = reply || null;
      });
      return cards;
    });

  const dedupe = (arr) => {
    if (!Array.isArray(arr)) return [];
    const seen = new Set();
    return arr.filter((c) => {
      const k = (c.text || "") + "|" + (c.date || "") + "|" + c.score;
      if (seen.has(k)) return false;
      seen.add(k);
      return true;
    });
  };

  // ===== ③ 好评：默认「全部」tab 下先收（按字数 topN）=====
  const goodsPool = [];
  let last = 0;
  for (let i = 0; i < 18; i++) {
    await scrollBottom();
    await sleep(900);
    for (const c of dedupe(await collectCards())) {
      if (c.score !== null && c.score >= 4.0 && (c.text || "").length > 30) goodsPool.push(c);
    }
    const uniq = dedupe(goodsPool);
    if (i % 5 === 4) console.error(`好评滚动#${i + 1}: 累计 ${uniq.length}`);
    if (uniq.length >= GOOD_N + 8) break;
    if (i > 8 && uniq.length === last) break;
    last = uniq.length;
  }
  result.good = dedupe(goodsPool)
    .sort((a, b) => (b.text || "").length - (a.text || "").length)
    .slice(0, GOOD_N)
    .map(({ raw, ...rest }) => rest);
  await sleep(2000);

  // ===== ④ 差评：点 chip → 滚动收集（图优先/字数次之 topN）=====
  const chipClicked = await page.evaluate(() => {
    for (const el of document.querySelectorAll("div,span,li")) {
      const t = (el.innerText || "").trim();
      if (/^差评(\d+)?$/.test(t) && el.getBoundingClientRect().height > 0) {
        el.scrollIntoView({ block: "center" });
        el.click();
        return true;
      }
    }
    return false;
  });
  console.error("差评 chip 点击:", chipClicked);
  await sleep(3000);
  const disPool = [];
  let stale = 0;
  for (let i = 0; i < 35; i++) {
    await scrollBottom();
    await sleep(900);
    for (const c of dedupe(await collectCards())) {
      if (c.score !== null && c.score <= 3.5) disPool.push(c);
    }
    const uniq = dedupe(disPool);
    if (i % 5 === 4) console.error(`差评滚动#${i + 1}: 累计 ${uniq.length}`);
    if (i > 12 && uniq.length === stale) break;
    stale = uniq.length;
    if (uniq.length >= DISLIKE_N + 10) break;
  }
  result.dislikes = dedupe(disPool)
    .sort((a, b) => b.images.length - a.images.length || (b.text || "").length - (a.text || "").length)
    .slice(0, DISLIKE_N)
    .map(({ raw, ...rest }) => rest);
  await sleep(2000);

  // ===== ⑤ 图片下载（选中卡片去重，上限 IMAGE_N）=====
  const urls = [...new Set([...result.dislikes.flatMap((d) => d.images), ...result.good.flatMap((g) => g.images)])].slice(0, IMAGE_N);
  let i = 0;
  for (const u of urls) {
    i++;
    try {
      const bigger = u.replace(/_W_\d+_\d+/, "_W_1600_0"); // 缩略图换大图（CDN 参数惯例）
      const resp = await fetch(bigger, {
        headers: { Referer: "https://m.ctrip.com/" },
        signal: AbortSignal.timeout(15000),
      });
      if (!resp.ok) throw new Error(String(resp.status));
      const buf = Buffer.from(await resp.arrayBuffer());
      const ext = /png/i.test(u) ? "png" : /webp/i.test(u) ? "webp" : "jpg";
      const file = path.join(IMG_DIR, `${String(i).padStart(2, "0")}.${ext}`);
      fs.writeFileSync(file, buf);
      result.images.push(file);
      await sleep(400); // 礼貌间隔
    } catch (e) {
      console.error(`图片 ${i} 下载失败: ${e.message}`);
    }
  }

  result.dislikes = result.dislikes.map((d) => ({ ...d, images: undefined }));
  result.good = result.good.map((g) => ({ ...g, images: undefined }));
  console.log(JSON.stringify(result, null, 1));
} catch (e) {
  console.error("ERROR:", e.message);
  console.log(JSON.stringify(result, null, 1));
  process.exitCode = 1;
} finally {
  await browser.close();
}
