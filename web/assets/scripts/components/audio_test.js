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

	test("talk capture sends only full 160-sample frames", async () => {
		// Context running at a foreign rate (48 kHz — a browser may
		// ignore the requested sampleRate) decimates to sub-frame
		// chunks; the carry must hold them to 160 boundaries.
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
		const c = newAudioController("csrf", () => "m1", err);
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
		expect(typeof proc.onaudioprocess).toBe("function");

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
