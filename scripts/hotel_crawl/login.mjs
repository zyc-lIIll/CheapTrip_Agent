// 携程登录态持久化：开有头浏览器人工登录（扫码/短信均可），完成后保存 storageState。
// 运行：./trip hotel login   （登录成功后写 scripts/hotel_crawl/.ctrip-state.json，已 gitignore）
import { chromium } from "playwright";
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const STATE_FILE = path.join(HERE, ".ctrip-state.json");
const UA =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

if (fs.existsSync(STATE_FILE)) {
  console.log("已存在登录态文件（ .ctrip-state.json ），如需重登请先删除它");
  process.exit(0);
}

console.log("打开有头浏览器，请在窗口里完成登录（扫码/短信都行）……");
const browser = await chromium.launch({ headless: false });
const ctx = await browser.newContext({
  userAgent: UA,
  viewport: { width: 390, height: 844 },
  isMobile: true,
  hasTouch: true,
  locale: "zh-CN",
  timezoneId: "Asia/Shanghai",
});
const page = await ctx.newPage();
page.setDefaultTimeout(30000);

// 进一个免登录可见、带「立即登录」入口的页面
await page.goto("https://m.ctrip.com/webapp/hotel/hoteldetail/1286148.html", {
  waitUntil: "domcontentloaded",
  timeout: 60000,
});
await page.waitForTimeout(4000);

// 尝试点「立即登录」；失败也不管（用户可在窗口里自己点）
await page
  .getByText(/立即登录|登录/)
  .first()
  .click({ timeout: 6000, force: true })
  .catch(() => console.log("自动点登录没成功，请在窗口里手动点"));

// 轮询登录完成：离开登录页且页面不再出现「立即登录」
const deadline = Date.now() + 5 * 60 * 1000;
let ok = false;
while (Date.now() < deadline) {
  await page.waitForTimeout(3000);
  const state = await page
    .evaluate(() => ({
      url: location.href,
      hasLoginBtn: /立即登录/.test(document.body.innerText),
    }))
    .catch(() => ({ url: "", hasLoginBtn: true }));
  const onLogin = /passport|login/i.test(state.url);
  if (!onLogin && !state.hasLoginBtn) {
    ok = true;
    break;
  }
  process.stdout.write(".");
}
console.log("");

if (!ok) {
  console.log("5 分钟内未检测到登录成功，退出（可重跑本脚本）");
  await browser.close();
  process.exit(1);
}

// 回详情页确认登录态有效（不再显示登录按钮）
await page.goto("https://m.ctrip.com/webapp/hotel/hoteldetail/1286148.html", {
  waitUntil: "domcontentloaded",
  timeout: 60000,
});
await page.waitForTimeout(4000);
const verify = await page.evaluate(() => document.body.innerText);
if (/立即登录/.test(verify)) {
  console.log("登录态校验失败：详情页仍显示登录按钮。可重跑本脚本。");
  await browser.close();
  process.exit(1);
}

await ctx.storageState({ path: STATE_FILE });
console.log(`登录态已保存: ${STATE_FILE}`);
console.log("登录态已就绪；接下来可运行 ./trip hotel crawl <酒店ID或URL> [参数]");
await browser.close();
