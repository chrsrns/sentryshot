// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { jest } from "@jest/globals";
import { newScanModal, newPtzField, newAudioField } from "./settings.js";

describe("newScanModal", () => {
	test("open attaches modal to the document", () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");

		modal.open();

		expect(document.body.contains(modal.elem)).toBe(true);
		expect(modal.elem.classList.contains("modal-open")).toBe(true);
	});

	const scanBody = (videoCodec) => ({
		cameras: [
			{
				ip: "10.0.0.9",
				name: "Cam",
				xaddrs: [],
				ptz: false,
				backchannel: false,
				needsAuth: false,
				streams: [
					{
						role: "main",
						rtspUrl: "rtsp://10.0.0.9/main",
						videoCodec,
						live: true,
					},
				],
			},
		],
		truncated: false,
	});

	test("renders labeled credential fields", () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");

		const text = modal.elem.textContent;
		expect(text).toContain("Camera IP");
		expect(text).toContain("Username");
		expect(text).toContain("Password");
	});

	/** Runs a scan inside an open modal with a canned response. */
	const runScan = async (modal, body) => {
		global.fetch = () =>
			Promise.resolve({
				status: 200,
				json: () => Promise.resolve(body),
			});
		modal.open();
		const scanBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Scan",
		);
		scanBtn.click();
		// Flush the fetch + render microtasks.
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
	};

	test("masks embedded password in rendered stream urls", async () => {
		document.body.replaceChildren();
		global.confirm = () => true;
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		const body = scanBody("h264");
		body.cameras[0].streams[0].rtspUrl = "rtsp://admin:secret@10.0.0.9/main";
		await runScan(modal, body);

		expect(modal.elem.textContent).toContain("admin:•••@10.0.0.9");
		expect(modal.elem.textContent).not.toContain("secret");

		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();
		// The real (unmasked) URL is what gets applied.
		expect(picked.sourcertsp.mainStream).toBe(
			"rtsp://admin:secret@10.0.0.9/main",
		);
	});

	/** Sets a labeled text field inside the modal. */
	const setModalField = (modal, label, value) => {
		const l = [...modal.elem.querySelectorAll("label")].find(
			(x) => x.textContent === label,
		);
		const input = modal.elem.querySelector(`#${l.htmlFor}`);
		input.value = value;
	};

	test("Use injects modal creds into creds-free picked urls", async () => {
		document.body.replaceChildren();
		global.confirm = () => true;
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		await runScan(modal, scanBody("h264"));

		setModalField(modal, "Username", "admin");
		setModalField(modal, "Password", "s@cret");

		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();

		expect(picked.sourcertsp.mainStream).toBe(
			"rtsp://admin:s%40cret@10.0.0.9/main",
		);
	});

	test("Use leaves backend-embedded creds urls untouched", async () => {
		document.body.replaceChildren();
		global.confirm = () => true;
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		const body = scanBody("h264");
		body.cameras[0].streams[0].rtspUrl = "rtsp://cam:cam@10.0.0.9/main";
		await runScan(modal, body);

		setModalField(modal, "Username", "admin");
		setModalField(modal, "Password", "secret");

		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();

		expect(picked.sourcertsp.mainStream).toBe("rtsp://cam:cam@10.0.0.9/main");
	});

	test("Use passes urls through verbatim when no creds supplied", async () => {
		document.body.replaceChildren();
		global.confirm = () => true;
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		await runScan(modal, scanBody("h264"));

		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();

		expect(picked.sourcertsp.mainStream).toBe("rtsp://10.0.0.9/main");
	});

	test("marks dead streams in the list", async () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");

		const body = scanBody("h264");
		body.cameras[0].streams[0].live = false;
		await runScan(modal, body);

		expect(modal.elem.textContent).toContain("dead");
	});

	test("picking a dead stream requires confirmation", async () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		const body = scanBody("h264");
		body.cameras[0].streams[0].live = false;
		await runScan(modal, body);

		global.confirm = () => false;
		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();
		expect(picked).toBeUndefined();

		global.confirm = () => true;
		useBtn.click();
		expect(picked.sourcertsp.mainStream).toBe("rtsp://10.0.0.9/main");
	});

	test("sub-stream select drives slot assignment", async () => {
		document.body.replaceChildren();
		global.confirm = () => true;
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		const body = scanBody("h264");
		body.cameras[0].streams.push({
			role: "extra",
			rtspUrl: "rtsp://10.0.0.9/extra",
			videoCodec: "h264",
		});
		await runScan(modal, body);

		const labels = [...modal.elem.querySelectorAll("label")];
		const subLabel = labels.find((l) => l.textContent === "Sub stream");
		const subSelect = modal.elem.querySelector(`#${subLabel.htmlFor}`);
		const extraOption = [...subSelect.options].find((o) =>
			o.textContent.includes("rtsp://10.0.0.9/extra"),
		);
		subSelect.value = extraOption.value;
		subSelect.dispatchEvent(new Event("change"));

		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();

		expect(picked.sourcertsp.mainStream).toBe("rtsp://10.0.0.9/main");
		expect(picked.sourcertsp.subStream).toBe("rtsp://10.0.0.9/extra");
	});

	test("marks non-h264 streams unsupported", async () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");

		await runScan(modal, scanBody("h265"));

		expect(modal.elem.textContent).toContain("h265 — unsupported");
	});

	test("h264 streams render without unsupported mark", async () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");

		await runScan(modal, scanBody("h264"));

		expect(modal.elem.textContent).toContain("rtsp://10.0.0.9/main");
		expect(modal.elem.textContent).not.toContain("unsupported");
	});

	test("pick warns when a chosen stream is unsupported", async () => {
		document.body.replaceChildren();
		let confirmCalls = 0;
		global.confirm = () => {
			confirmCalls++;
			return false;
		};
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		await runScan(modal, scanBody("h265"));
		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();

		expect(confirmCalls).toBe(1);
		expect(picked).toBeUndefined();
	});

	test("pick applies without warning on h264", async () => {
		document.body.replaceChildren();
		let confirmCalls = 0;
		global.confirm = () => {
			confirmCalls++;
			return true;
		};
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		await runScan(modal, scanBody("h264"));
		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();

		expect(confirmCalls).toBe(0);
		expect(picked.sourcertsp.mainStream).toBe("rtsp://10.0.0.9/main");
	});

	test("extra stream can be assigned to a slot before pick", async () => {
		document.body.replaceChildren();
		global.confirm = () => true;
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};

		const body = scanBody("h264");
		body.cameras[0].streams.push({
			role: "extra",
			rtspUrl: "rtsp://10.0.0.9/extra",
			videoCodec: "h264",
		});
		await runScan(modal, body);

		const extraText = [...modal.elem.querySelectorAll("span")].find((sp) =>
			sp.textContent.includes("rtsp://10.0.0.9/extra"),
		);
		const extraRow = extraText.parentElement;
		const toSub = [...extraRow.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "→sub",
		);
		toSub.click();

		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();

		expect(picked.sourcertsp.mainStream).toBe("rtsp://10.0.0.9/main");
		expect(picked.sourcertsp.subStream).toBe("rtsp://10.0.0.9/extra");
	});
});

describe("newPtzField", () => {
	test("round-trips enable and onvifAddr", () => {
		const field = newPtzField("tok", () => "m1");

		field.set({ enable: true, onvifAddr: "http://cam/onvif/device_service" });
		expect(field.value()).toEqual({
			enable: true,
			onvifAddr: "http://cam/onvif/device_service",
		});

		field.set(undefined);
		expect(field.value()).toEqual({ enable: false, onvifAddr: "" });
	});

	test("validates onvifAddr", () => {
		const field = newPtzField("tok", () => "m1");

		for (const bad of [
			"https://cam",
			"http://u:p@cam",
			"http://cam/x?y=1",
			"http://cam/x#f",
			"rtsp://cam",
			"not a url",
		]) {
			field.set({ enable: true, onvifAddr: bad });
			expect(field.validate()).toBeDefined();
		}

		for (const good of ["", "http://cam", "http://cam:8080/onvif/device_service"]) {
			field.set({ enable: true, onvifAddr: good });
			expect(field.validate()).toBeUndefined();
		}
	});
});

describe("newPtzField panel", () => {
	const mkField = () => newPtzField("tok", () => "m1");

	/** Fetch spy; returns a canned 200 JSON. */
	const mkFetch = (json = {}) => {
		const calls = [];
		global.fetch = (url, opts) => {
			calls.push({ url, body: JSON.parse(opts.body) });
			return Promise.resolve({ status: 200, json: () => Promise.resolve(json) });
		};
		return calls;
	};

	/** Flush microtasks a few times (fetch chains). */
	const flush = async () => {
		for (let i = 0; i < 12; i++) {
			await Promise.resolve();
		}
	};

	/** Pointer event with a primary button + pointer id. */
	const ptr = (type, props = {}) => {
		const ev = /** @type {any} */ (Object.assign(new Event(type), props));
		if (ev.button === undefined) {
			ev.button = 0;
		}
		if (ev.pointerId === undefined) {
			ev.pointerId = 1;
		}
		return ev;
	};

	test("panel hidden until a persisted enabled config is set", () => {
		document.body.replaceChildren();
		const field = mkField();
		const li = field.elems.find((e) => e.classList.contains("ptz-panel"));

		expect(li.hidden).toBe(true);
		field.set({ enable: false, onvifAddr: "" });
		expect(li.hidden).toBe(true);
		field.set({ enable: true, onvifAddr: "" });
		expect(li.hidden).toBe(false);
		field.set(undefined);
		expect(li.hidden).toBe(true);
	});

	test("hold sends pulse, upgrade move, heartbeats, and stop on release", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = mkFetch();
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) =>
				/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
				/** @type {HTMLElement} */ (b).dataset.pan === "0",
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		// Every press starts as a tap-pulse: sign·0.25 per axis, PT0.5S.
		expect(calls.at(-1).body).toEqual({
			command: "move",
			tilt: 0.25,
			pan: 0,
			timeout: "PT0.5S",
		});

		// Still held at 300ms → upgrade to full velocity + heartbeat.
		jest.advanceTimersByTime(300);
		await flush();
		expect(calls.at(-1).body).toEqual({
			command: "move",
			tilt: 0.5,
			pan: 0,
			timeout: "PT2S",
		});

		const before = calls.length;
		jest.advanceTimersByTime(1600);
		await flush();
		expect(calls).toHaveLength(before + 1);
		expect(calls.at(-1).body).toEqual({
			command: "move",
			tilt: 0.5,
			pan: 0,
			timeout: "PT2S",
		});

		up.dispatchEvent(ptr("pointerup"));
		await flush();
		expect(calls.at(-1).body).toEqual({ command: "stop" });
		jest.useRealTimers();
	});

	test("tap releases before upgrade — stop, then no ghost move", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = mkFetch();
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) =>
				/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
				/** @type {HTMLElement} */ (b).dataset.pan === "0",
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		expect(calls.at(-1).body.timeout).toBe("PT0.5S");

		up.dispatchEvent(ptr("pointerup"));
		await flush();
		expect(calls.at(-1).body).toEqual({ command: "stop" });

		// The cleared upgrade timer must not fire a full-velocity move
		// after the stop.
		const n = calls.length;
		jest.advanceTimersByTime(2000);
		await flush();
		expect(calls).toHaveLength(n);
		jest.useRealTimers();
	});

	test("heartbeat beat dropped while commands queued or in-flight", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = [];
		/** @type {undefined | ((v: any) => void)} */
		let release;
		global.fetch = /** @type {any} */ ((url, opts) => {
			calls.push({ url, body: JSON.parse(/** @type {string} */ (opts.body)) });
			if (release === undefined) {
				return new Promise((resolve) => {
					release = resolve;
				});
			}
			return Promise.resolve({ status: 200, json: () => Promise.resolve({}) });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();
		// getPresets consumed the pending fetch slot.
		/** @type {any} */ (release)({ status: 200, json: () => Promise.resolve({}) });
		await flush();
		release = undefined;

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) =>
				/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
				/** @type {HTMLElement} */ (b).dataset.pan === "0",
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		const n = calls.length; // Pulse in-flight, not resolved.

		// Upgrade enqueues unconditionally; the beat is droppable while
		// anything is queued or awaiting response.
		jest.advanceTimersByTime(2000);
		await flush();
		expect(calls).toHaveLength(n);

		/** @type {any} */ (release)({ status: 200, json: () => Promise.resolve({}) });
		await flush();
		release = undefined;
		jest.advanceTimersByTime(1600);
		await flush();
		expect(
			calls.filter((c) => c.body.command === "move").length,
		).toBeGreaterThan(1); // Beats resume after the queue drains.
		jest.useRealTimers();
	});

	test("release stop dispatches after its move settles — no overtake", async () => {
		document.body.replaceChildren();
		// The previous test's field unmounts here — let its MutationObserver
		// detach (a stray stop fetch) run before the mock installs.
		await flush();
		jest.useFakeTimers();
		const order = [];
		const resolvers = [];
		global.fetch = /** @type {any} */ ((url, opts) => {
			const body = JSON.parse(/** @type {string} */ (opts.body));
			return new Promise((resolve) => {
				resolvers.push(() => {
					order.push(body.command);
					resolve({ status: 200, json: () => Promise.resolve({}) });
				});
			});
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) =>
				/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
				/** @type {HTMLElement} */ (b).dataset.pan === "0",
		);
		up.dispatchEvent(ptr("pointerdown"));
		jest.advanceTimersByTime(300);
		up.dispatchEvent(ptr("pointerup"));

		// Only the pulse has been dispatched to fetch so far — upgrade
		// and stop wait behind it in the queue. Draining fetches in
		// dispatch order surfaces move, move, stop.
		for (let i = 0; i < 10 && resolvers.length > 0; i++) {
			for (const r of resolvers.splice(0)) {
				r();
			}
			await flush();
		}
		expect(order.filter((c) => c === "move" || c === "stop")).toEqual([
			"move",
			"move",
			"stop",
		]);
		jest.useRealTimers();
	});

	test("auxiliary press does not actuate; aux release does not cancel", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = mkFetch();
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) =>
				/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
				/** @type {HTMLElement} */ (b).dataset.pan === "0",
		);
		// Right-button press → nothing sent.
		up.dispatchEvent(ptr("pointerdown", { button: 2 }));
		await flush();
		expect(calls.some((c) => c.body.command === "move")).toBe(false);

		// Primary press starts the hold; a different pointer's release
		// must not cancel it.
		up.dispatchEvent(ptr("pointerdown", { pointerId: 1 }));
		await flush();
		jest.advanceTimersByTime(300);
		await flush();
		up.dispatchEvent(ptr("pointerup", { pointerId: 9 }));
		await flush();
		expect(calls.some((c) => c.body.command === "stop")).toBe(false);

		up.dispatchEvent(ptr("pointerup", { pointerId: 1 }));
		await flush();
		expect(calls.at(-1).body).toEqual({ command: "stop" });
		jest.useRealTimers();
	});

	test("window blur sends stop", async () => {
		document.body.replaceChildren();
		const calls = mkFetch();
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) =>
				/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
				/** @type {HTMLElement} */ (b).dataset.pan === "0",
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		window.dispatchEvent(new Event("blur"));
		await flush();
		expect(calls.at(-1).body).toEqual({ command: "stop" });
	});

	test("panel unmount sends stop", async () => {
		document.body.replaceChildren();
		const calls = mkFetch();
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		const up = [...li.querySelectorAll(".ptz-hold")].find(
			(b) =>
				/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
				/** @type {HTMLElement} */ (b).dataset.pan === "0",
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		li.remove();
		await flush();
		await new Promise((resolve) => {
				setTimeout(resolve, 0);
			}); // MutationObserver
		await flush();
		expect(calls.some((c) => c.body.command === "stop")).toBe(true);
	});

	test("home and preset list/recall/delete/save", async () => {
		document.body.replaceChildren();
		const calls = mkFetch({
			presets: [
				{ token: "1", name: "Gate" },
				{ token: "2", name: "<img onerror=x>" },
			],
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();
		expect(calls[0].body).toEqual({ command: "capabilities" });
		expect(calls[1].body).toEqual({ command: "getPresets" });

		// Device names render as text only.
		const names = [...li.querySelectorAll(".js-ptz-preset-name")].map(
			(e) => e.textContent,
		);
		expect(names).toEqual(["Gate", "<img onerror=x>"]);
		expect(li.querySelector(".js-ptz-presets img")).toBeNull();

		/** @type {HTMLElement} */ (
			li.querySelector(".js-ptz-home")
		).click();
		await flush();
		expect(calls.at(-1).body).toEqual({ command: "home" });

		li.querySelectorAll(".js-ptz-recall")[0].click();
		await flush();
		expect(calls.at(-1).body).toEqual({
			command: "gotoPreset",
			presetToken: "1",
		});

		li.querySelectorAll(".js-ptz-delete")[0].click();
		await flush();
		await flush();
		expect(
			calls.some(
				(c) =>
					c.body.command === "removePreset" && c.body.presetToken === "1",
			),
		).toBe(true);

		li.querySelector(".js-ptz-name").value = "My spot";
		li.querySelector(".js-ptz-save").click();
		await flush();
		await flush();
		expect(
			calls.some(
				(c) =>
					c.body.command === "setPreset" && c.body.presetName === "My spot",
			),
		).toBe(true);
	});

	test("preset save rejects empty and over-64-char names", async () => {
		document.body.replaceChildren();
		const calls = mkFetch({ presets: [] });
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		li.querySelector(".js-ptz-save").click();
		await flush();
		expect(li.querySelector(".js-ptz-error").textContent).toContain(
			"required",
		);

		li.querySelector(".js-ptz-name").value = "x".repeat(65);
		li.querySelector(".js-ptz-save").click();
		await flush();
		expect(li.querySelector(".js-ptz-error").textContent).toContain("64");
		expect(calls.some((c) => c.body.command === "setPreset")).toBe(false);
	});

	test("unsupported marks disable + label, presets skipped", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = mkFetch({
			unsupported: ["home", "getPresets", "setPreset"],
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		expect(calls[0].body).toEqual({ command: "capabilities" });
		// `getPresets` marked → preset list never requested.
		expect(calls.some((c) => c.body.command === "getPresets")).toBe(false);

		const home = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-home")
		);
		expect(home.disabled).toBe(true);
		expect(home.classList.contains("ptz-unsupported")).toBe(true);
		expect(home.textContent).toBe("⌂"); // Glyph preserved — no label rewrite.
		expect(home.closest(".js-ptz-wrap").getAttribute("title")).toBe(
			"unsupported on this camera",
		);
		// `getPresets` marked → the ★ toggle itself is struck; nothing
		// renders in place of the preset UI.
		const presetsBtn = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-presets-btn")
		);
		expect(presetsBtn.disabled).toBe(true);
		expect(presetsBtn.classList.contains("ptz-unsupported")).toBe(true);
		expect(li.querySelector(".js-ptz-presets").textContent).toBe("");
		const nameInput = /** @type {HTMLInputElement} */ (
			li.querySelector(".js-ptz-name")
		);
		const saveBtn = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-save")
		);
		expect(nameInput.disabled).toBe(true);
		expect(saveBtn.disabled).toBe(true);

		// Movement controls untouched — still usable.
		const up = /** @type {HTMLButtonElement} */ (
			[...li.querySelectorAll(".ptz-hold")].find(
				(b) =>
					/** @type {HTMLElement} */ (b).dataset.tilt === "0.5" &&
					/** @type {HTMLElement} */ (b).dataset.pan === "0",
			)
		);
		expect(up.disabled).toBeFalsy();
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		expect(calls.at(-1).body.command).toBe("move");
		jest.useRealTimers();
	});

	test("supported camera leaves controls enabled", async () => {
		document.body.replaceChildren();
		const calls = mkFetch({ unsupported: [] });
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		const homeBtn = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-home")
		);
		const saveBtn2 = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-save")
		);
		expect(homeBtn.disabled).toBeFalsy();
		expect(saveBtn2.disabled).toBeFalsy();
		expect(calls.some((c) => c.body.command === "getPresets")).toBe(true);
	});

	test("diagonal hold sends combined pan+tilt, centre stop is tap-only", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = mkFetch();
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		const diag = /** @type {HTMLElement} */ (
			li.querySelector('[data-pan="-0.5"][data-tilt="0.5"]')
		);
		expect(diag.textContent).toBe("↖");
		diag.dispatchEvent(ptr("pointerdown"));
		await flush();
		// Pulse carries both axes at sign·0.25.
		expect(calls.at(-1).body).toEqual({
			command: "move",
			pan: -0.25,
			tilt: 0.25,
			timeout: "PT0.5S",
		});
		jest.advanceTimersByTime(300);
		await flush();
		// Upgrade carries the full combined velocity in one move.
		expect(calls.at(-1).body).toEqual({
			command: "move",
			pan: -0.5,
			tilt: 0.5,
			timeout: "PT2S",
		});
		diag.dispatchEvent(ptr("pointerup"));
		await flush();

		const n = calls.length;
		/** @type {HTMLElement} */ (
			li.querySelector(".js-ptz-stop")
		).click();
		await flush();
		expect(calls).toHaveLength(n + 1); // Tap only — no move precedes it.
		expect(calls.at(-1).body).toEqual({ command: "stop" });
		jest.useRealTimers();
	});

	test("capability shrink restores controls", async () => {
		document.body.replaceChildren();
		const marks = { list: ["home"] };
		global.fetch = /** @type {any} */ ((_url, opts) => {
			const cmd = JSON.parse(/** @type {string} */ (opts.body)).command;
			const json = cmd === "capabilities" ? { unsupported: marks.list } : {};
			return Promise.resolve({ status: 200, json: () => Promise.resolve(json) });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();
		const home = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-home")
		);
		expect(home.disabled).toBe(true);

		marks.list = [];
		field.set(undefined);
		field.set({ enable: true, onvifAddr: "" });
		await flush();
		expect(home.disabled).toBe(false);
		expect(home.classList.contains("ptz-unsupported")).toBe(false);
		expect(home.closest(".js-ptz-wrap").getAttribute("title")).toBeNull();
	});

	test("mid-hold mark cancels the heartbeat and sends stop", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const caps = { list: /** @type {string[]} */ ([]) };
		const calls = [];
		global.fetch = /** @type {any} */ ((_url, opts) => {
			const body = JSON.parse(/** @type {string} */ (opts.body));
			calls.push({ url: _url, body });
			const json =
				body.command === "capabilities" ? { unsupported: caps.list } : {};
			return Promise.resolve({ status: 200, json: () => Promise.resolve(json) });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		const up = /** @type {HTMLElement} */ (
			li.querySelector('[data-tilt="0.5"][data-pan="0"]')
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		expect(calls.at(-1).body.command).toBe("move");

		// Marks arrive mid-hold → held control released: heartbeat dead, stop sent.
		caps.list = ["move"];
		field.set({ enable: true, onvifAddr: "" });
		await flush();
		expect(calls.some((c) => c.body.command === "stop")).toBe(true);
		const moves = calls.filter((c) => c.body.command === "move").length;
		jest.advanceTimersByTime(3200);
		await flush();
		expect(calls.filter((c) => c.body.command === "move")).toHaveLength(moves);
		jest.useRealTimers();
	});

	test("stop mark skips the release stop and cascades to holds", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const caps = { list: /** @type {string[]} */ ([]) };
		const calls = [];
		global.fetch = /** @type {any} */ ((_url, opts) => {
			const body = JSON.parse(/** @type {string} */ (opts.body));
			calls.push({ url: _url, body });
			const json =
				body.command === "capabilities" ? { unsupported: caps.list } : {};
			return Promise.resolve({ status: 200, json: () => Promise.resolve(json) });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		const up = /** @type {HTMLElement} */ (
			li.querySelector('[data-tilt="0.5"][data-pan="0"]')
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();

		// `stop` marks mid-hold → cascade strikes every hold control and the
		// release must NOT send a stop the camera can't honour.
		caps.list = ["stop"];
		field.set({ enable: true, onvifAddr: "" });
		await flush();
		expect(up.classList.contains("ptz-unsupported")).toBe(true);
		expect(calls.some((c) => c.body.command === "stop")).toBe(false);
		const moves = calls.filter((c) => c.body.command === "move").length;
		jest.advanceTimersByTime(3200);
		await flush();
		expect(calls.filter((c) => c.body.command === "move")).toHaveLength(moves);
		jest.useRealTimers();
	});

	test("faulted move cancels the heartbeat", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = [];
		global.fetch = /** @type {any} */ ((_url, opts) => {
			const body = JSON.parse(/** @type {string} */ (opts.body));
			calls.push({ url: _url, body });
			const status = body.command === "move" ? 502 : 200;
			return Promise.resolve({ status, json: () => Promise.resolve({}) });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		const up = /** @type {HTMLElement} */ (
			li.querySelector('[data-tilt="0.5"][data-pan="0"]')
		);
		up.dispatchEvent(ptr("pointerdown"));
		await flush();
		const moves = calls.filter((c) => c.body.command === "move").length;
		expect(moves).toBe(1);
		jest.advanceTimersByTime(3200);
		await flush();
		expect(calls.filter((c) => c.body.command === "move")).toHaveLength(moves);
		jest.useRealTimers();
	});

	test("capability marks apply to asynchronously created preset rows", async () => {
		document.body.replaceChildren();
		global.fetch = /** @type {any} */ ((_url, opts) => {
			const body = JSON.parse(opts.body);
			const json =
				body.command === "capabilities"
					? { unsupported: ["gotoPreset"] }
					: (body.command === "getPresets"
						? { presets: [{ name: "p1", token: "t1" }] }
						: {});
			return Promise.resolve({ status: 200, json: () => Promise.resolve(json) });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		const recall = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-recall")
		);
		const del = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-delete")
		);
		expect(recall.disabled).toBe(true);
		expect(recall.classList.contains("ptz-unsupported")).toBe(true);
		expect(recall.closest(".js-ptz-wrap").getAttribute("title")).toBe(
			"unsupported on this camera",
		);
		expect(del.disabled).toBe(false);
	});

	test("successful command clears a prior error", async () => {
		document.body.replaceChildren();
		let fail = true;
		global.fetch = /** @type {any} */ ((_url, opts) => {
			const body = JSON.parse(opts.body);
			const status = body.command === "home" && fail ? 502 : 200;
			return Promise.resolve({ status, json: () => Promise.resolve("x") });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();

		/** @type {HTMLElement} */ (
			li.querySelector(".js-ptz-home")
		).click();
		await flush();
		expect(li.querySelector(".js-ptz-error").textContent).toContain("502");
		fail = false;
		/** @type {HTMLElement} */ (
			li.querySelector(".js-ptz-home")
		).click();
		await flush();
		expect(li.querySelector(".js-ptz-error").textContent).toBe("");
	});

	test("failed capabilities query keeps prior marks", async () => {
		document.body.replaceChildren();
		const caps = { fail: false };
		global.fetch = /** @type {any} */ ((_url, opts) => {
			const body = JSON.parse(opts.body);
			if (body.command === "capabilities" && caps.fail) {
				return Promise.resolve({
					status: 500,
					json: () => Promise.resolve("boom"),
				});
			}
			const json =
				body.command === "capabilities" ? { unsupported: ["home"] } : {};
			return Promise.resolve({ status: 200, json: () => Promise.resolve(json) });
		});
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		const li = field.elems[2];
		document.body.append(li);
		await flush();
		const home = /** @type {HTMLButtonElement} */ (
			li.querySelector(".js-ptz-home")
		);
		expect(home.disabled).toBe(true);

		// A failed probe is no result — the mark stays instead of clearing.
		caps.fail = true;
		field.set(undefined);
		field.set({ enable: true, onvifAddr: "" });
		await flush();
		expect(home.disabled).toBe(true);
		expect(home.classList.contains("ptz-unsupported")).toBe(true);
	});

	test("preset list refetches on every open", async () => {
		document.body.replaceChildren();
		const calls = mkFetch({ presets: [] });
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();
		expect(
			calls.filter((c) => c.body.command === "getPresets"),
		).toHaveLength(1);

		field.set(undefined);
		field.set({ enable: true, onvifAddr: "" });
		await flush();
		expect(
			calls.filter((c) => c.body.command === "getPresets"),
		).toHaveLength(2);
	});

	test("show() is idempotent — one blur listener", async () => {
		document.body.replaceChildren();
		mkFetch();
		const spy = jest.spyOn(window, "addEventListener");
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();
		expect(
			spy.mock.calls.filter((c) => c[0] === "blur"),
		).toHaveLength(1);
		spy.mockRestore();
	});

	test("value() round-trips unsupported marks", () => {
		const field = mkField();
		field.set({
			enable: true,
			onvifAddr: "",
			unsupported: ["home", "getPresets"],
		});
		expect(field.value()).toEqual({
			enable: true,
			onvifAddr: "",
			unsupported: ["home", "getPresets"],
		});
		// Cleared state drops the key entirely.
		field.set(undefined);
		expect(field.value()).toEqual({ enable: false, onvifAddr: "" });
	});
});

describe("scan pick ptz prefill", () => {
	const runScan = async (modal, body) => {
		global.fetch = () =>
			Promise.resolve({
				status: 200,
				json: () => Promise.resolve(body),
			});
		modal.open();
		const scanBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Scan",
		);
		scanBtn.click();
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
	};

	test("ptz camera prefills enable and first http xaddr", async () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};
		await runScan(modal, {
			cameras: [
				{
					ip: "10.0.0.9",
					name: "Cam",
					xaddrs: ["https://cam/onvif", "http://cam/onvif/device_service"],
					ptz: true,
					backchannel: false,
					needsAuth: false,
					streams: [
						{ role: "main", rtspUrl: "rtsp://10.0.0.9/main", videoCodec: "h264" },
					],
				},
			],
			truncated: false,
		});
		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();
		expect(picked.ptz).toEqual({
			enable: true,
			onvifAddr: "http://cam/onvif/device_service",
		});
	});

	test("ptz camera without http xaddr leaves addr empty", async () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};
		await runScan(modal, {
			cameras: [
				{
					ip: "10.0.0.9",
					name: "Cam",
					xaddrs: ["https://cam/onvif"],
					ptz: true,
					backchannel: false,
					needsAuth: false,
					streams: [
						{ role: "main", rtspUrl: "rtsp://10.0.0.9/main", videoCodec: "h264" },
					],
				},
			],
			truncated: false,
		});
		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();
		expect(picked.ptz).toEqual({ enable: true, onvifAddr: "" });
	});

	test("non-ptz camera adds no ptz prefill", async () => {
		document.body.replaceChildren();
		const modal = newScanModal("token");
		let picked;
		modal.onPick = (p) => {
			picked = p;
		};
		await runScan(modal, {
			cameras: [
				{
					ip: "10.0.0.9",
					name: "Cam",
					xaddrs: ["http://cam/onvif"],
					ptz: false,
					backchannel: false,
					needsAuth: false,
					streams: [
						{ role: "main", rtspUrl: "rtsp://10.0.0.9/main", videoCodec: "h264" },
					],
				},
			],
			truncated: false,
		});
		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent.trim() === "Use",
		);
		useBtn.click();
		expect(picked.ptz).toBeUndefined();
	});
});

describe("newAudioField", () => {
	const getId = () => "cam1";
	const panelOf = (field) =>
		field.elems.find((e) => e.classList.contains("audio-panel"));

	test("panel hidden when monitor is not persisted", () => {
		// Scan prefill on a monitor with no saved config.
		const field = newAudioField("token", getId, {});
		field.set({ enable: true });
		expect(panelOf(field).hasAttribute("hidden")).toBe(true);
	});

	test("panel shows for a persisted monitor saved with enable", () => {
		const field = newAudioField("token", getId, {
			cam1: { audio: { enable: true } },
		});
		field.set({ enable: true });
		expect(panelOf(field).hasAttribute("hidden")).toBe(false);
	});

	test("panel hidden when saved config has enable false", () => {
		const field = newAudioField("token", getId, {
			cam1: { audio: { enable: false } },
		});
		field.set({ enable: true });
		expect(panelOf(field).hasAttribute("hidden")).toBe(true);
	});
});
