// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { uidReset, htmlToElem } from "../libs/common.js";
import { applyAudioMarks, newAudioController, newLiveAudio } from "./audio.js";

/** Mock WebSocket capturing sent frames. */
class MockWebSocket {
	constructor(url) {
		this.url = url;
		this.readyState = MockWebSocket.CONNECTING;
		this.sent = [];
		this.binaryType = "";
		MockWebSocket.instances.push(this);
	}
	open() {
		this.readyState = MockWebSocket.OPEN;
		if (this.onopen) {
			this.onopen();
		}
	}
	send(data) {
		this.sent.push(data);
	}
	close() {
		this.readyState = 3;
		if (this.onclose) {
			this.onclose();
		}
	}
	/** Simulate a server ctrl frame. */
	receiveText(obj) {
		if (this.onmessage) {
			this.onmessage({ data: JSON.stringify(obj) });
		}
	}
	addEventListener() {}
}
MockWebSocket.OPEN = 1;
MockWebSocket.CONNECTING = 0;
MockWebSocket.instances = [];
MockWebSocket.prototype.CONNECTING = 0;
MockWebSocket.prototype.OPEN = 1;

const mkErrorElem = () => /** @type {HTMLElement} */ (htmlToElem(`<div></div>`));

const mkRoot = () =>
	/** @type {HTMLElement} */ (
		htmlToElem(/* HTML */ `
			<div>
				<span class="js-audio-wrap"
					><button type="button" class="js-audio-listen"></button
				></span>
				<span class="js-audio-wrap js-audio-talk-wrap"
					><button type="button" class="js-audio-talk"></button
				></span>
			</div>
		`)
	);

const mkFetchToken = () => {
	// @ts-ignore
	global.fetch = () =>
		Promise.resolve({
			ok: true,
			status: 200,
			json: () => Promise.resolve({ token: "tok123" }),
		});
};

beforeEach(() => {
	uidReset();
	MockWebSocket.instances = [];
	localStorage.clear();
	// @ts-ignore
	global.WebSocket = MockWebSocket;
});

describe("audio marks", () => {
	test("marked controls disabled + titled on wrapper", () => {
		const root = mkRoot();
		applyAudioMarks(root, ["talk"]);
		const talk = /** @type {HTMLButtonElement} */ (
			root.querySelector(".js-audio-talk")
		);
		const listen = /** @type {HTMLButtonElement} */ (
			root.querySelector(".js-audio-listen")
		);
		expect(talk.disabled).toBe(true);
		expect(listen.disabled).toBe(false);
		const wrap = /** @type {Element} */ (root.querySelector(".js-audio-talk-wrap"));
		expect(wrap.getAttribute("title")).toBe("unsupported on this camera");
	});
	test("unmarked restore clears title", () => {
		const root = mkRoot();
		applyAudioMarks(root, ["talk"]);
		applyAudioMarks(root, []);
		const talk = /** @type {HTMLButtonElement} */ (
			root.querySelector(".js-audio-talk")
		);
		expect(talk.disabled).toBe(false);
		expect(
			root.querySelector(".js-audio-talk-wrap").getAttribute("title"),
		).toBeNull();
	});
});

describe("audio controller", () => {
	test("listen toggle sends listen then listen-stop", async () => {
		mkFetchToken();
		const err = mkErrorElem();
		const c = newAudioController("csrf", () => "m1", err);
		const root = mkRoot();
		c.bindListen(root);
		c.bindTalk(root);

		const t = c.listenToggle();
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
		const ws = MockWebSocket.instances[0];
		ws.open();
		await t;
		expect(ws.url).toContain("/api/monitor/audio?token=tok123");
		expect(JSON.parse(ws.sent.at(-1))).toEqual({ type: "listen" });

		await c.listenToggle();
		expect(JSON.parse(ws.sent.at(-1))).toEqual({ type: "listen-stop" });
	});

	test("caps ctrl marks unsupported directions", async () => {
		mkFetchToken();
		const err = mkErrorElem();
		const c = newAudioController("csrf", () => "m1", err);
		const root = mkRoot();
		c.bindListen(root);
		c.bindTalk(root);

		const t = c.listenToggle();
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
		const ws = MockWebSocket.instances[0];
		ws.open();
		await t;
		ws.receiveText({
			type: "caps",
			listen: true,
			talk: false,
			unsupported: ["talk"],
		});
		const talk = /** @type {HTMLButtonElement} */ (
			root.querySelector(".js-audio-talk")
		);
		expect(talk.disabled).toBe(true);
	});

	test("busy marks talk, caps talkFree unmarks", async () => {
		mkFetchToken();
		const err = mkErrorElem();
		const c = newAudioController("csrf", () => "m1", err);
		const root = mkRoot();
		c.bindListen(root);
		c.bindTalk(root);
		const t = c.listenToggle();
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
		const ws = MockWebSocket.instances[0];
		ws.open();
		await t;
		ws.receiveText({ type: "busy", command: "talk" });
		const talk = /** @type {HTMLButtonElement} */ (
			root.querySelector(".js-audio-talk")
		);
		expect(talk.disabled).toBe(true);
		expect(
			/** @type {Element} */ (
				root.querySelector(".js-audio-talk-wrap")
			).getAttribute("title"),
		).toBe("another session is talking");

		ws.receiveText({ type: "caps", talkFree: true, unsupported: [] });
		expect(talk.disabled).toBe(false);
	});

	test("busy-unmark requires talkFree — plain caps does not unmark", async () => {
		mkFetchToken();
		const err = mkErrorElem();
		const c = newAudioController("csrf", () => "m1", err);
		const root = mkRoot();
		c.bindListen(root);
		c.bindTalk(root);
		const t = c.listenToggle();
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
		const ws = MockWebSocket.instances[0];
		ws.open();
		await t;
		ws.receiveText({ type: "busy", command: "talk" });
		ws.receiveText({ type: "caps", talkFree: false, unsupported: [] });
		expect(
			/** @type {HTMLButtonElement} */ (root.querySelector(".js-audio-talk"))
				.disabled,
		).toBe(true);
	});

	test("error ctrl surfaces message", async () => {
		mkFetchToken();
		const err = mkErrorElem();
		const c = newAudioController("csrf", () => "m1", err);
		const root = mkRoot();
		c.bindListen(root);
		c.bindTalk(root);
		const t = c.listenToggle();
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
		const ws = MockWebSocket.instances[0];
		ws.open();
		await t;
		ws.receiveText({ type: "error", message: "session overrun" });
		expect(err.textContent).toBe("session overrun");
	});

	/** Mock the capture chain (48 kHz context — a browser may ignore
	 * the requested rate), open the talk hold, return the harness. */
	const mkCapture = async (monitorId) => {
		const proc = { connect() {}, onaudioprocess: undefined };
		class MockAudioContext {
			constructor() {
				this.sampleRate = 48000;
				this.destination = {};
			}
			createMediaStreamSource() {
				return { connect() {} };
			}
			createScriptProcessor() {
				return proc;
			}
			close() {
				return Promise.resolve();
			}
		}
		// @ts-ignore
		global.AudioContext = MockAudioContext;
		Object.defineProperty(global.navigator, "mediaDevices", {
			value: {
				getUserMedia: () => Promise.resolve({ getTracks: () => [] }),
			},
			configurable: true,
		});
		mkFetchToken();
		const err = mkErrorElem();
		const c = newAudioController("csrf", () => monitorId, err);
		const root = mkRoot();
		c.bindListen(root);
		c.bindTalk(root);

		const talk = /** @type {HTMLElement} */ (root.querySelector(".js-audio-talk"));
		talk.dispatchEvent(
			Object.assign(new Event("pointerdown"), { button: 0, pointerId: 1 }),
		);
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
		const ws = MockWebSocket.instances[0];
		ws.open();
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
		if (typeof proc.onaudioprocess !== "function") {
			throw new TypeError("onaudioprocess never wired");
		}
		return { proc, ws, controller: c };
	};

	test("talk capture sends only full 160-sample frames", async () => {
		const { proc, ws } = await mkCapture("m1");

		const binarySent = () => ws.sent.filter((f) => typeof f !== "string");
		/** @param {number} n */
		const fire = (n) =>
			proc.onaudioprocess({
				inputBuffer: {
					getChannelData: () => new Float32Array(n).fill(0.5),
				},
			});
		fire(512); // 85 decimated samples — held, nothing sent.
		expect(binarySent()).toHaveLength(0);
		fire(512); // 170 total → one 160 frame + 10 carried.
		expect(binarySent()).toHaveLength(1);
		for (let i = 0; i < 5; i++) {
			fire(512);
		}
		for (const f of binarySent()) {
			expect(f.byteLength).toBe(320);
		}

		delete global.AudioContext;
	});

	test("setGain scales sent frames live mid-capture + stores", async () => {
		const { proc, ws, controller } = await mkCapture("m9");
		const binarySent = () => ws.sent.filter((f) => typeof f !== "string");
		/** @param {number} n */
		const fire = (n) =>
			proc.onaudioprocess({
				inputBuffer: {
					getChannelData: () => new Float32Array(n).fill(0.25),
				},
			});
		fire(512);
		fire(512); // ≥170 decimated samples → first frame out
		const f1 = /** @type {ArrayBuffer} */ (binarySent().at(-1));
		const amp1 = new Int16Array(f1)[0];

		controller.setGain(2);
		// The ≤159-sample carry is pre-gain — fire past one frame so
		// the last frame is entirely post-change.
		for (let i = 0; i < 4; i++) {
			fire(512);
		}
		const f2 = /** @type {ArrayBuffer} */ (binarySent().at(-1));
		const amp2 = new Int16Array(f2)[0];

		expect(amp2).toBeGreaterThan(amp1 * 1.8);
		expect(localStorage.getItem("audioGain-m9")).toBe("2");
		localStorage.clear();
		delete global.AudioContext;
	});

	test("stored gain applies at capture start", async () => {
		localStorage.setItem("audioGain-m9", "2");
		const { proc, ws } = await mkCapture("m9");
		const binarySent = () => ws.sent.filter((f) => typeof f !== "string");
		/** @param {number} n */
		const fire = (n) =>
			proc.onaudioprocess({
				inputBuffer: {
					getChannelData: () => new Float32Array(n).fill(0.25),
				},
			});
		fire(512);
		fire(512);
		const f = /** @type {ArrayBuffer} */ (binarySent().at(-1));
		// 0.25 × 2 → ≈0.5 → ~16383 vs the 8191 of gain 1.
		expect(new Int16Array(f)[0]).toBeGreaterThan(15000);
		localStorage.clear();
		delete global.AudioContext;
	});

	test("storage edges — NaN default, range clamp, per-monitor key", () => {
		let id = "a";
		const c = newAudioController("csrf", () => id, mkErrorElem());
		expect(c.refreshGain()).toBe(1); // empty storage
		localStorage.setItem("audioGain-a", "abc");
		expect(c.refreshGain()).toBe(1); // NaN
		localStorage.setItem("audioGain-a", "99");
		expect(c.refreshGain()).toBe(4); // clamp high
		localStorage.setItem("audioGain-a", "0.01");
		expect(c.refreshGain()).toBe(0.25); // clamp low
		c.setGain(2);
		id = "b";
		expect(c.refreshGain()).toBe(1); // b unset — no bleed
		id = "a";
		expect(c.refreshGain()).toBe(2); // a's pref restored
		localStorage.clear();
	});

	test("session mint failure surfaces status", async () => {
		// @ts-ignore
		global.fetch = () =>
			Promise.resolve({ ok: false, status: 409, json: () => Promise.resolve({}) });
		const err = mkErrorElem();
		const c = newAudioController("csrf", () => "m1", err);
		await c.listenToggle();
		expect(err.textContent).toBe("409");
	});
});

describe("live audio cluster", () => {
	test("renders listen + talk buttons", () => {
		const { elem } = newLiveAudio("csrf", "m1");
		expect(elem.querySelector(".js-audio-listen")).not.toBeNull();
		expect(elem.querySelector(".js-audio-talk")).not.toBeNull();
	});
	test("buttons wear feed-btn chrome + wraps carry hint titles", () => {
		Object.defineProperty(window, "isSecureContext", {
			value: true,
			configurable: true,
		});
		const { elem } = newLiveAudio("csrf", "m1");
		const listen = /** @type {HTMLElement} */ (
			elem.querySelector(".js-audio-listen")
		);
		const talk = /** @type {HTMLElement} */ (elem.querySelector(".js-audio-talk"));
		expect(listen.classList.contains("feed-btn")).toBe(true);
		expect(talk.classList.contains("feed-btn")).toBe(true);
		expect(elem.querySelectorAll("img.icon-filter")).toHaveLength(2);
		const listenWrap = /** @type {HTMLElement} */ (
			elem.querySelector(".js-audio-wrap")
		);
		const talkWrap = /** @type {HTMLElement} */ (
			elem.querySelector(".js-audio-talk-wrap")
		);
		expect(talkWrap.getAttribute("title")).toBe("hold to talk");
		expect(listenWrap.getAttribute("title")).toBe("listen");
	});
	test("marked title clears back to the data-hint", () => {
		const { elem, controller } = newLiveAudio("csrf", "m1");
		const wrap = /** @type {HTMLElement} */ (
			elem.querySelector(".js-audio-talk-wrap")
		);
		controller.onCaps({ unsupported: ["talk"] });
		expect(wrap.getAttribute("title")).toBe("unsupported on this camera");
		controller.onCaps({ unsupported: [] });
		expect(wrap.getAttribute("title")).toBe("hold to talk");
	});
});
