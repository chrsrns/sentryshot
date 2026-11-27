// SPDX-License-Identifier: GPL-2.0-or-later

/* eslint-disable require-await */

import { uidReset } from "../libs/common.js";
import { newFeed } from "./feed.js";
import { newStreamerBtn } from "./streamer.js";
import { jest } from "@jest/globals";

class MockHls {
	static isSupported() {
		return true;
	}
	static instances = [];
	constructor() {
		MockHls.instances.push(this);
		this.onError = undefined;
		this.destroyed = false;
	}
	init($video, url) {
		this.initUrl = url;
	}
	destroy() {
		this.destroyed = true;
	}
}

const playlistWithCodec = (codec) => `#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000,CODECS="${codec}"
stream.m3u8
`;

const flush = () => new Promise((resolve) => setTimeout(resolve, 10));

describe("feed", () => {
	test("unsupported playlist codec alerts and skips hls", async () => {
		MockHls.instances = [];
		global.fetch = () =>
			Promise.resolve({
				text: () => Promise.resolve(playlistWithCodec("hvc1.1.6.L93.B0")),
			});
		// @ts-ignore
		window.MediaSource = { isTypeSupported: () => false };
		global.alert = jest.fn();

		const feed = newFeed(MockHls, { id: "a" }, false, []);
		await flush();
		feed.destroy();
		// @ts-ignore
		delete window.MediaSource;

		expect(global.alert).toHaveBeenCalledWith(
			expect.stringContaining("hvc1.1.6.L93.B0"),
		);
		expect(MockHls.instances.length).toBe(0);
	});

	test("supported playlist codec starts hls", async () => {
		MockHls.instances = [];
		global.fetch = () =>
			Promise.resolve({
				text: () => Promise.resolve(playlistWithCodec("avc1.64001E")),
			});
		// @ts-ignore
		window.MediaSource = { isTypeSupported: () => true };
		global.alert = jest.fn();

		const feed = newFeed(MockHls, { id: "a" }, false, []);
		await flush();
		// @ts-ignore
		delete window.MediaSource;

		expect(global.alert).not.toHaveBeenCalled();
		expect(MockHls.instances.length).toBe(1);
		expect(MockHls.instances[0].initUrl).toBe("hls/a/index.m3u8");
		feed.destroy();
		expect(MockHls.instances[0].destroyed).toBe(true);
	});

	test("playlist fetch failure falls through to hls", async () => {
		MockHls.instances = [];
		global.fetch = () => Promise.reject(new Error("no network"));

		newFeed(MockHls, { id: "a" }, false, []);
		await flush();

		expect(MockHls.instances.length).toBe(1);
	});

	test("playlist without CODECS attribute proceeds", async () => {
		MockHls.instances = [];
		global.fetch = () =>
			Promise.resolve({
				text: () => Promise.resolve("#EXTM3U\nstream.m3u8\n"),
			});

		newFeed(MockHls, { id: "a" }, false, []);
		await flush();

		expect(MockHls.instances.length).toBe(1);
	});

	test("rendering", () => {
		uidReset();
		const monitor = { id: "a" };
		const buttons = [newStreamerBtn.fullscreen()];
		const feed = newFeed(undefined, monitor, true, buttons);

		expect(feed.elem.outerHTML).toMatchInlineSnapshot(`
<div class="flex justify-center">
  <div class="relative flex justify-center items-center w-full"
       style="max-height: 100vh; align-self: center; --player-timeline-width: 90%;"
  >
    <input id="uid1"
           class="js-checkbox player-overlay-checkbox absolute"
           style="opacity: 0;"
           type="checkbox"
    >
    <label for="uid1"
           class="absolute w-full h-full"
           style="z-index: 1; opacity: 0.5;"
    >
    </label>
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
           muted
           disablepictureinpicture
           playsinline
    >
    </video>
  </div>
</div>
`);
	});
});
