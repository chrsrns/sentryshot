// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

/* eslint-disable require-await */

import { uidReset } from "../libs/common.js";
import { newSlowPollStream, newStreamerBtn } from "./streamer.js";
import { jest } from "@jest/globals";

describe("feed", () => {
	test("unsupported codec surfaces a fatal error without retrying", async () => {
		const fetches = [];
		global.fetch = (url) => {
			fetches.push(String(url));
			return Promise.resolve({
				status: 200,
				json: () =>
					Promise.resolve({
						startTimeNs: "0",
						codecs: "hvc1.1.6.L93.B0",
					}),
			});
		};
		const isTypeSupported = jest.fn(() => false);
		// @ts-ignore
		function FakeMediaSource() {}
		FakeMediaSource.isTypeSupported = (mime) => isTypeSupported(mime);
		window.MediaSource = FakeMediaSource;
		const consoleError = jest.spyOn(console, "error").mockImplementation(() => {});

		const feed = newSlowPollStream({ id: "a", name: "m" }, true, []);
		await new Promise((resolve) => { setTimeout(resolve, 10); });
		feed.destroy();

		// The fatal path surfaces the error via console.error; a returned
		// (retried) error never reaches it.
		expect(consoleError).toHaveBeenCalledWith(
			expect.objectContaining({
				message: expect.stringContaining("codec unsupported"),
			}),
		);
		consoleError.mockRestore();
		// @ts-ignore
		delete window.MediaSource;

		expect(isTypeSupported).toHaveBeenCalledWith(
			'video/mp4; codecs="hvc1.1.6.L93.B0"',
		);
		// Only the start-session fetch; play was never reached.
		expect(fetches.filter((u) => u.includes("start-session"))).toHaveLength(1);
		expect(fetches.filter((u) => u.includes("/play"))).toHaveLength(0);
		// The fatal gate surfaces an on-tile hint naming the codec family.
		const hint = feed.elem.querySelector(".js-media-unsupported");
		expect(hint && hint.textContent).toContain("hvc1");
	});

	test("play error status is not appended and restarts the session", async () => {
		const fetches = [];
		global.fetch = (url) => {
			fetches.push(String(url));
			if (String(url).includes("start-session")) {
				return Promise.resolve({
					status: 200,
					json: () =>
						Promise.resolve({
							startTimeNs: "0",
							codecs: "avc1.42E01E",
						}),
				});
			}
			// play -> error status; its body must never reach appendBuffer.
			return Promise.resolve({
				status: 410,
				arrayBuffer: () => Promise.resolve(new ArrayBuffer(4)),
				text: () => Promise.resolve("frames expired"),
			});
		};
		const appendBuffer = jest.fn();
		// @ts-ignore
		function FakeMediaSource() {}
		FakeMediaSource.isTypeSupported = () => true;
		FakeMediaSource.prototype.addEventListener = (type, fn) => {
			if (type === "sourceopen") queueMicrotask(fn);
		};
		FakeMediaSource.prototype.addSourceBuffer = () => ({
			buffered: { length: 0 },
			addEventListener() {},
			appendBuffer,
			remove() {},
		});
		window.MediaSource = FakeMediaSource;
		// @ts-ignore
		URL.createObjectURL = () => "blob:x";

		const feed = newSlowPollStream({ id: "a", name: "m" }, true, []);
		await new Promise((resolve) => { setTimeout(resolve, 30); });
		feed.destroy();
		// @ts-ignore
		delete window.MediaSource;
		// @ts-ignore
		delete URL.createObjectURL;

		expect(appendBuffer).not.toHaveBeenCalled();
		expect(fetches.filter((u) => u.includes("/play")).length).toBeGreaterThan(0);
	});

	test("prune keeps the playhead inside the buffered range", async () => {
		global.fetch = (url) => {
			if (String(url).includes("start-session")) {
				return Promise.resolve({
					status: 200,
					json: () =>
						Promise.resolve({
							startTimeNs: "0",
							codecs: "avc1.42E01E",
						}),
				});
			}
			return Promise.resolve({
				status: 200,
				arrayBuffer: () =>
					new Promise((resolve) => {
						setTimeout(() => resolve(new ArrayBuffer(4)), 0);
					}),
			});
		};
		// SourceBuffer with real MSE semantics: append/remove mutate the
		// buffered ranges asynchronously, on the updateend event.
		let ranges = [[0, 0]];
		const listeners = [];
		const fire = () =>
			queueMicrotask(() => {
				for (const f of listeners.splice(0)) f();
			});
		const sourceBuffer = {
			get buffered() {
				return {
					length: ranges.length,
					start: (i) => ranges[i][0],
					end: (i) => ranges[i][1],
				};
			},
			addEventListener(type, fn) {
				if (type === "updateend") listeners.push(fn);
			},
			appendBuffer() {
				const [start, end] = ranges.at(-1);
				ranges = [[start, end + 8]];
				fire();
			},
			remove(_start, end) {
				const bufferEnd = ranges.at(-1)[1];
				// The range mutates when the remove completes (updateend),
				// not when remove() is called.
				queueMicrotask(() => {
					ranges = [[end, bufferEnd]];
				});
				fire();
			},
		};
		// @ts-ignore
		function FakeMediaSource() {}
		FakeMediaSource.isTypeSupported = () => true;
		FakeMediaSource.prototype.addEventListener = (type, fn) => {
			if (type === "sourceopen") queueMicrotask(fn);
		};
		FakeMediaSource.prototype.addSourceBuffer = () => sourceBuffer;
		window.MediaSource = FakeMediaSource;
		// @ts-ignore
		URL.createObjectURL = () => "blob:x";

		const feed = newSlowPollStream({ id: "a", name: "m" }, true, []);
		const $video = feed.elem.querySelector("video");
		// Playhead lags behind a long catch-up backlog.
		$video.currentTime = 3;
		await new Promise((resolve) => { setTimeout(resolve, 100); });
		feed.destroy();
		// @ts-ignore
		delete window.MediaSource;
		// @ts-ignore
		delete URL.createObjectURL;

		// Buffer exceeded 15s so the front was pruned; the playhead must
		// have been moved into the remaining range, not left in the hole.
		expect(ranges[0][0]).toBeGreaterThan(0);
		expect($video.currentTime).toBeGreaterThanOrEqual(ranges[0][0]);
	});

	test("supported codec passes the gate", async () => {
		global.fetch = () =>
			Promise.resolve({
				status: 200,
				json: () =>
					Promise.resolve({
						startTimeNs: "0",
						codecs: "avc1.64001E",
					}),
			});
		const isTypeSupported = jest.fn(() => true);
		// @ts-ignore
		window.MediaSource = class {
			static isTypeSupported(mime) {
				return isTypeSupported(mime);
			}
			addEventListener() {}
		};
		// @ts-ignore
		URL.createObjectURL = () => "blob:x";

		const feed = newSlowPollStream({ id: "a", name: "m" }, true, []);
		await new Promise((resolve) => { setTimeout(resolve, 10); });
		feed.destroy();
		// @ts-ignore
		delete window.MediaSource;
		// @ts-ignore
		delete URL.createObjectURL;

		expect(isTypeSupported).toHaveBeenCalledWith('video/mp4; codecs="avc1.64001E"');
	});

	test("rendering", () => {
		uidReset();
		const monitor = { id: "a", name: "my-monitor" };
		const buttons = [newStreamerBtn.fullscreen()];
		const feed = newSlowPollStream(monitor, true, buttons);

		expect(feed.elem.outerHTML).toMatchInlineSnapshot(`
<div class="flex justify-center">
  <div class="relative flex justify-center items-center w-full"
       style="max-height: 100vh; align-self: center; --player-timeline-width: 90%; overflow: hidden;"
  >
    <input id="uid1"
           class="player-overlay-checkbox absolute"
           style="opacity: 0;"
           type="checkbox"
    >
    <label for="uid1"
           class="absolute w-full h-full"
           style="z-index: 1; opacity: 0.5;"
    >
    </label>
    <div class="player-overlay absolute flex bg-color1 text-color text-1.3 px-1"
         style="z-index: 2; left: 0; top: 0; border: none; border-bottom-right-radius: var(--radius-md);"
    >
      my-monitor
    </div>
    <div class="player-overlay absolute flex justify-center rounded-md bg-color1"
         style="z-index: 2; bottom: 0; margin-bottom: 5%; border: none;"
    >
      <button class="js-fullscreen-btn feed-btn p-1 bg-transparent">
        <img class="icon-filter"
             style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
             src="assets/icons/feather/maximize.svg"
        >
      </button>
    </div>
    <video class="w-full h-full"
           style="max-height: 100vh; object-fit: contain;"
           autoplay
           muted
           disablepictureinpicture
           playsinline
           type="video/mp4"
    >
    </video>
  </div>
</div>
`);
	});

	test("codec badge shows the sub-stream codec when preferring low res", () => {
		global.fetch = () => new Promise(() => {});
		const feed = newSlowPollStream(
			{
				id: "a",
				name: "m",
				hasSubStream: true,
				codec: "hvc1.1.6.L93.B0",
				subCodec: "avc1.42E01E",
			},
			true,
			[],
		);
		const badge = feed.elem.querySelector(".player-overlay-codec");
		expect(badge && badge.textContent).toBe("avc1");
		feed.destroy();
	});

	test("codec badge shows the main codec without preferLowRes", () => {
		global.fetch = () => new Promise(() => {});
		const feed = newSlowPollStream(
			{
				id: "a",
				name: "m",
				hasSubStream: true,
				codec: "hvc1.1.6.L93.B0",
				subCodec: "avc1.42E01E",
			},
			false,
			[],
		);
		const badge = feed.elem.querySelector(".player-overlay-codec");
		expect(badge && badge.textContent).toBe("hvc1");
		feed.destroy();
	});

	test("codec badge falls back to main codec and is absent without codec", () => {
		global.fetch = () => new Promise(() => {});
		// Sub stream preferred but has no codec: fall back to main.
		const feed = newSlowPollStream(
			{ id: "a", name: "m", hasSubStream: true, codec: "avc1.42E01E" },
			true,
			[],
		);
		const badge3 = feed.elem.querySelector(".player-overlay-codec");
		expect(badge3 && badge3.textContent).toBe("avc1");
		feed.destroy();

		// No codec at all: no badge.
		const feed2 = newSlowPollStream({ id: "a", name: "m" }, true, []);
		expect(feed2.elem.querySelector(".player-overlay-codec")).toBeNull();
		feed2.destroy();
	});
});

/*
describe("muteBtn", () => {
	test("rendering", async () => {
		const monitor = { audioEnabled: "true" };
		const actual = newFeedBtn.mute(monitor).html.replaceAll(/\s/g, "");
		const expected = `
			<button class="js-mute-btn feed-btn">
				<img
					class="feed-btn-img icon"
					src="assets/icons/feather/volume.svg"
				/>
			</button>`.replaceAll(/\s/g, "");
		expect(actual).toBe(expected);
	});
	test("logic", async () => {
		document.body.innerHTML = "<div><video></video</div>";
		const element = document.querySelector("div");
		const $video = element.querySelector("video");
		$video.muted = true;

		const monitor = { audioEnabled: "true" };
		const btn = newFeedBtn.mute(monitor);
		element.innerHTML += btn.html;
		btn.init(element, $video);
		const $btn = element.querySelector("button");
		const $img = element.querySelector("img");

		expect($img.src).toBe("http://localhost/assets/icons/feather/volume.svg");
		expect($video.muted).toBe(true);

		$btn.click();
		expect($img.src).toBe("http://localhost/assets/icons/feather/volume-x.svg");
		expect($video.muted).toBe(false);

		$btn.click();
		expect($img.src).toBe("http://localhost/assets/icons/feather/volume.svg");
		expect($video.muted).toBe(true);
	});
});
*/

test("recordingsBtn", async () => {
	expect(newStreamerBtn.recordings("b").elem.outerHTML).toMatchInlineSnapshot(`
<a href="http://test.com/recordings#monitors=b"
   class="feed-btn p-1 bg-transparent"
>
  <img class="icon-filter"
       style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
       src="assets/icons/feather/film.svg"
  >
</a>
`);
});

test("fullscreenBtn", () => {
	expect(newStreamerBtn.fullscreen().elem.outerHTML).toMatchInlineSnapshot(`
<button class="js-fullscreen-btn feed-btn p-1 bg-transparent">
  <img class="icon-filter"
       style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
       src="assets/icons/feather/maximize.svg"
  >
</button>
`);
});
