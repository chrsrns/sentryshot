// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { newScanModal } from "./settings.js";

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
					},
				],
			},
		],
		truncated: false,
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
			(b) => b.textContent === "scan",
		);
		scanBtn.click();
		// Flush the fetch + render microtasks.
		await new Promise((resolve) => {
			setTimeout(resolve, 0);
		});
	};

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
			(b) => b.textContent === "use",
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
			(b) => b.textContent === "use",
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
			(b) => b.textContent === "→sub",
		);
		toSub.click();

		const useBtn = [...modal.elem.querySelectorAll("button")].find(
			(b) => b.textContent === "use",
		);
		useBtn.click();

		expect(picked.sourcertsp.mainStream).toBe("rtsp://10.0.0.9/main");
		expect(picked.sourcertsp.subStream).toBe("rtsp://10.0.0.9/extra");
	});
});
