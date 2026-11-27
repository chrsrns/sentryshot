// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { uidReset } from "./libs/common.js";
import { newViewer, resBtn } from "./live.js";

class mockHls {
	constructor() {}
	init() {}
	destroy() {}
	static isSupported() {
		return true;
	}
}
mockHls.Events = {
	MEDIA_ATTACHED() {},
};

describe("resBtn", () => {
	const mockContent = {
		setPreferLowRes() {},
		reset() {},
	};
	test("ok", () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");

		element.replaceChildren(resBtn(mockContent));

		const $btn = document.querySelector("button");
		expect($btn.textContent).toBe("HD");

		// @ts-ignore
		$btn.click();
		expect($btn.textContent).toBe("SD");
		expect(localStorage.getItem("preferLowRes")).toBe("true");

		// @ts-ignore
		$btn.click();
		expect($btn.textContent).toBe("HD");
		expect(localStorage.getItem("preferLowRes")).toBe("false");

		// @ts-ignore
		$btn.click();
		expect($btn.textContent).toBe("SD");
		expect(localStorage.getItem("preferLowRes")).toBe("true");
	});
	test("contentCalled", () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");

		let preferLowCalled, resetCalled;
		const content = {
			setPreferLowRes() {
				preferLowCalled = true;
			},
			reset() {
				resetCalled = true;
			},
		};

		element.replaceChildren(resBtn(content));

		// @ts-ignore
		document.querySelector("button").click();
		expect(preferLowCalled).toBe(true);
		expect(resetCalled).toBe(true);
	});
});

test("fullscreen", () => {
	uidReset();
	document.body.innerHTML = `<div></div>`;
	const element = document.querySelector("div");
	// @ts-ignore
	const viewer = newViewer(element, [{ enable: true }, { enable: true }], "hls");
	viewer.reset();

	expect(element.innerHTML).toMatchInlineSnapshot(`
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
      <a href="http://test.com/recordings#monitors=undefined"
         class="feed-btn p-1 bg-transparent"
      >
        <img class="icon-filter"
             style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
             src="assets/icons/feather/film.svg"
        >
      </a>
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
<div class="flex justify-center">
  <div class="relative flex justify-center items-center w-full"
       style="max-height: 100vh; align-self: center; --player-timeline-width: 90%;"
  >
    <input id="uid2"
           class="js-checkbox player-overlay-checkbox absolute"
           style="opacity: 0;"
           type="checkbox"
    >
    <label for="uid2"
           class="absolute w-full h-full"
           style="z-index: 1; opacity: 0.5;"
    >
    </label>
    <div class="player-overlay absolute flex justify-center rounded-md bg-color1"
         style="z-index: 2; bottom: 0; margin-bottom: 5%; border: none;"
    >
      <a href="http://test.com/recordings#monitors=undefined"
         class="feed-btn p-1 bg-transparent"
      >
        <img class="icon-filter"
             style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
             src="assets/icons/feather/film.svg"
        >
      </a>
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
	/** @param {number} i */
	const isFullscreen = (i) => {
		return element.children[i].classList.contains("grid-fullscreen");
	};

	expect(isFullscreen(0)).toBe(false);
	expect(isFullscreen(1)).toBe(false);
	// @ts-ignore
	element.querySelector(".js-fullscreen-btn").click();
	expect(isFullscreen(0)).toBe(true);
	expect(isFullscreen(1)).toBe(false);
});

describe("live ptz overlay", () => {
	const mkMonitors = () => [
		{ enable: true, id: "m1", name: "cam1", hasSubStream: false },
		{ enable: true, id: "m2", name: "cam2", hasSubStream: false },
	];
	const mkPtz = (isAdmin) => ({
		isAdmin,
		csrfToken: "tok",
		configs: {
			m1: { ptz: { enable: true, onvifAddr: "" } },
			m2: { ptz: { enable: false, onvifAddr: "" } },
		},
	});

	test("pad rendered for admin on ptz-enabled monitors only", () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");
		global.fetch = () => Promise.reject(new Error("no network"));
		// @ts-ignore
		newViewer(element, mkMonitors(), "hls", mkPtz(true)).reset();

		expect(document.querySelectorAll(".js-ptz-live-pad")).toHaveLength(1);
		expect(document.querySelectorAll(".js-ptz-live-btn")).toHaveLength(1);
	});

	test("no pad for non-admin or disabled monitors", () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");
		global.fetch = () => Promise.reject(new Error("no network"));
		// @ts-ignore
		newViewer(element, mkMonitors(), "hls", mkPtz(false)).reset();

		expect(document.querySelectorAll(".js-ptz-live-pad")).toHaveLength(0);
	});

	test("rail markup + DOM order rail → popover → bar", () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");
		global.fetch = () => Promise.reject(new Error("no network"));
		// @ts-ignore
		newViewer(element, mkMonitors(), "hls", mkPtz(true)).reset();

		const rail = /** @type {HTMLElement} */ (
			document.querySelector(".js-ptz-live-pad")
		);
		expect(rail.classList.contains("ptz-rail")).toBe(true);
		// Cross cluster + divider + zoom + home + presets toggle.
		expect(rail.querySelectorAll(".ptz-hold")).toHaveLength(6);
		expect(rail.querySelector(".js-ptz-stop").textContent).toBe("■");
		expect(rail.querySelector(".js-ptz-home").textContent).toBe("⌂");
		expect(rail.querySelector(".js-ptz-presets-btn").textContent).toBe("★");
		expect(rail.querySelector(".ptz-rail-sep")).not.toBeNull();

		// DOM order rail → popover → bar keeps the bar lit while the
		// rail/popover is hovered (forward-only sibling selectors).
		const popover = /** @type {HTMLElement} */ (
			document.querySelector(".js-ptz-presets-pop")
		);
		expect(popover.classList.contains("ptz-popover")).toBe(true);
		expect(rail.nextElementSibling).toBe(popover);
		expect(
			/** @type {Element} */ (popover.nextElementSibling).classList.contains(
				"player-overlay",
			),
		).toBe(true);
	});

	test("presets ★ toggles popover; rail collapse closes it", () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");
		global.fetch = () => Promise.reject(new Error("no network"));
		// @ts-ignore
		newViewer(element, mkMonitors(), "hls", mkPtz(true)).reset();

		const rail = /** @type {HTMLElement} */ (
			document.querySelector(".js-ptz-live-pad")
		);
		const popover = /** @type {HTMLElement} */ (
			document.querySelector(".js-ptz-presets-pop")
		);
		const btn = /** @type {HTMLElement} */ (
			document.querySelector(".js-ptz-live-btn")
		);
		const presetsBtn = /** @type {HTMLElement} */ (
			rail.querySelector(".js-ptz-presets-btn")
		);

		btn.click(); // open rail
		expect(rail.hidden).toBe(false);
		presetsBtn.click();
		expect(popover.hidden).toBe(false);
		presetsBtn.click();
		expect(popover.hidden).toBe(true);
		presetsBtn.click();
		expect(popover.hidden).toBe(false);
		btn.click(); // collapse rail → popover closes too
		expect(rail.hidden).toBe(true);
		expect(popover.hidden).toBe(true);
	});

	test("rail collapse while holding sends stop", async () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");
		const calls = [];
		global.fetch = /** @type {any} */ ((url, opts) => {
			calls.push({
				url,
				body: JSON.parse(/** @type {string} */ (opts.body)),
			});
			return Promise.resolve({
				status: 200,
				json: () => Promise.resolve({ presets: [] }),
			});
		});
		// @ts-ignore
		newViewer(element, mkMonitors(), "hls", mkPtz(true)).reset();
		for (let i = 0; i < 5; i++) {
			await Promise.resolve();
		}

		/** @type {HTMLElement} */ (
			document.querySelector(".js-ptz-live-btn")
		).click(); // open rail
		const rail = document.querySelector(".js-ptz-live-pad");
		const up = rail.querySelector('[data-tilt="0.5"]');
		up.dispatchEvent(Object.assign(new window.Event("pointerdown"), { button: 0, pointerId: 1 }));
		for (let i = 0; i < 5; i++) {
			await Promise.resolve();
		}
		expect(calls.some((c) => c.body.command === "move")).toBe(true);

		/** @type {HTMLElement} */ (
			document.querySelector(".js-ptz-live-btn")
		).click(); // collapse
		for (let i = 0; i < 5; i++) {
			await Promise.resolve();
		}
		expect(calls.some((c) => c.body.command === "stop")).toBe(true);
	});

	test("hold sends move and release sends stop via the shared route", async () => {
		uidReset();
		document.body.innerHTML = `<div></div>`;
		const element = document.querySelector("div");
		const calls = [];
		global.fetch = /** @type {any} */ ((url, opts) => {
			calls.push({ url, body: JSON.parse(opts.body) });
			return Promise.resolve({
				status: 200,
				json: () => Promise.resolve({ presets: [] }),
			});
		});
		// @ts-ignore
		newViewer(element, mkMonitors(), "hls", mkPtz(true)).reset();
		for (let i = 0; i < 5; i++) {
			await Promise.resolve();
		}

		const pad = document.querySelector(".js-ptz-live-pad");
		const btn = pad.querySelector('[data-tilt="0.5"]');
		btn.dispatchEvent(Object.assign(new window.Event("pointerdown"), { button: 0, pointerId: 1 }));
		for (let i = 0; i < 5; i++) {
			await Promise.resolve();
		}
		expect(
			calls.some(
				(c) =>
					c.url.includes("api/monitor/ptz?id=m1") &&
					c.body.command === "move" &&
					c.body.tilt === 0.25 &&
					c.body.timeout === "PT0.5S",
			),
		).toBe(true);
		btn.dispatchEvent(Object.assign(new window.Event("pointerup"), { button: 0, pointerId: 1 }));
		for (let i = 0; i < 5; i++) {
			await Promise.resolve();
		}
		expect(calls.some((c) => c.body.command === "stop")).toBe(true);
	});
});
