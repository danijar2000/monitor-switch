// Renders docs/demo/demo.html (drawn over desk.jpg) into docs/demo.mp4 and docs/demo.gif.
//
//   node docs/demo/render.mjs
//
// Needs Google Chrome and ffmpeg. Chrome runs headless and is asked over
// its debugging port to draw the animation frame by frame, so the result
// does not depend on how fast this machine is.
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const docs = resolve(here, "..");
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const FPS = 30;
const PORT = 9333;

const work = mkdtempSync(join(tmpdir(), "monitor-switch-demo-"));
const chrome = spawn(CHROME, [
  "--headless=new", `--remote-debugging-port=${PORT}`, `--user-data-dir=${join(work, "profile")}`,
  "--window-size=1280,720", "--hide-scrollbars", "--force-device-scale-factor=1", "about:blank",
], { stdio: "ignore" });

async function target() {
  for (let tries = 0; tries < 50; tries++) {
    try {
      const pages = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
      const page = pages.find(entry => entry.type === "page");
      if (page) return page.webSocketDebuggerUrl;
    } catch {}
    await new Promise(done => setTimeout(done, 200));
  }
  throw new Error("Chrome did not start");
}

try {
  const socket = new WebSocket(await target());
  await new Promise(done => socket.addEventListener("open", done));
  let next = 0;
  const waiting = new Map();
  socket.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    waiting.get(message.id)?.(message);
    waiting.delete(message.id);
  });
  const send = (method, params = {}) => new Promise((done, fail) => {
    const id = ++next;
    waiting.set(id, message => (message.error ? fail(new Error(message.error.message)) : done(message.result)));
    socket.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async expression =>
    (await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true })).result.value;

  await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 720, deviceScaleFactor: 1, mobile: false });
  await send("Page.navigate", { url: `file://${join(here, "demo.html")}#still` });
  while ((await evaluate("typeof window.render")) !== "function") await new Promise(done => setTimeout(done, 100));
  await evaluate("document.fonts.ready.then(() => 1)");
  await evaluate("window.READY.then(() => 1)");

  const length = await evaluate("window.LENGTH");
  const frames = Math.round(length * FPS);
  for (let frame = 0; frame < frames; frame++) {
    await evaluate(`window.render(${frame / FPS})`);
    const shot = await send("Page.captureScreenshot", { format: "png" });
    writeFileSync(join(work, `${String(frame).padStart(5, "0")}.png`), Buffer.from(shot.data, "base64"));
    if (frame % 150 === 0) console.log(`frame ${frame} of ${frames}`);
  }
  socket.close();

  const ffmpeg = args => execFileSync("ffmpeg", ["-hide_banner", "-loglevel", "error", "-y", ...args], { stdio: "inherit" });
  const input = ["-framerate", String(FPS), "-i", join(work, "%05d.png")];
  ffmpeg([...input, "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "20", "-movflags", "+faststart", join(docs, "demo.mp4")]);
  // A GIF plays inline in the README: smaller, slower frames, its own palette.
  const filter = "fps=15,scale=800:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle";
  ffmpeg([...input, "-filter_complex", filter, join(docs, "demo.gif")]);
  console.log("wrote docs/demo.mp4 and docs/demo.gif");
} finally {
  chrome.kill();
  rmSync(work, { recursive: true, force: true });
}
