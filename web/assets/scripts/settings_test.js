// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { jest } from "@jest/globals";
import { newScanModal, newPtzField } from "./settings.js";

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

	test("hold sends move, heartbeats, and stop on release", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = mkFetch();
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) => b.dataset.tilt === "0.5",
		);
		up.dispatchEvent(new Event("pointerdown"));
		await flush();
		expect(calls.at(-1).body).toEqual({
			command: "move",
			tilt: 0.5,
			pan: 0,
		});

		const before = calls.length;
		jest.advanceTimersByTime(1600);
		await flush();
		expect(calls).toHaveLength(before + 1);
		expect(calls.at(-1).body.command).toBe("move");

		up.dispatchEvent(new Event("pointerup"));
		await flush();
		expect(calls.at(-1).body).toEqual({ command: "stop" });
		jest.useRealTimers();
	});

	test("heartbeat skipped while prior move in-flight", async () => {
		document.body.replaceChildren();
		jest.useFakeTimers();
		const calls = [];
		let resolveFirst;
		global.fetch = (url, opts) => {
			calls.push({ url, body: JSON.parse(opts.body) });
			if (resolveFirst === undefined) {
				return new Promise((resolve) => {
					resolveFirst = resolve;
				});
			}
			return Promise.resolve({ status: 200, json: () => Promise.resolve({}) });
		};
		const field = mkField();
		field.set({ enable: true, onvifAddr: "" });
		document.body.append(field.elems[2]);
		await flush();
		// getPresets call consumed the pending fetch.
		resolveFirst({ status: 200, json: () => Promise.resolve({}) });
		await flush();
		resolveFirst = undefined;

		const up = [...field.elems[2].querySelectorAll(".ptz-hold")].find(
			(b) => b.dataset.tilt === "0.5",
		);
		up.dispatchEvent(new Event("pointerdown"));
		await flush();
		const n = calls.length; // move in-flight, not resolved

		jest.advanceTimersByTime(1600);
		await flush();
		expect(calls).toHaveLength(n); // beat skipped

		resolveFirst({ status: 200, json: () => Promise.resolve({}) });
		await flush();
		jest.advanceTimersByTime(1600);
		await flush();
		expect(calls).toHaveLength(n + 1); // beats resume
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
			(b) => b.dataset.tilt === "0.5",
		);
		up.dispatchEvent(new Event("pointerdown"));
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
			(b) => b.dataset.tilt === "0.5",
		);
		up.dispatchEvent(new Event("pointerdown"));
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

		li.querySelector(".js-ptz-home").click();
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
		const calls = mkFetch({ unsupported: ["home", "getPresets"] });
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
		expect(home.textContent).toContain("unsupported");
		expect(li.querySelector(".js-ptz-presets").textContent).toContain(
			"unsupported",
		);
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
				(b) => /** @type {HTMLElement} */ (b).dataset.tilt === "0.5",
			)
		);
		expect(up.disabled).toBeFalsy();
		up.dispatchEvent(new Event("pointerdown"));
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
