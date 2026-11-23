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
});
