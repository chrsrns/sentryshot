// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { fetchPostJson, htmlToElem } from "../libs/common.js";

export const PTZ_HEARTBEAT_MS = 1500;

/**
 * Shared PTZ command channel: send + hold-to-move heartbeat + lifecycle
 * stops. Both the settings test panel and the /live overlay drive the
 * same POST /api/monitor/ptz route through this controller.
 *
 * @param {string} csrfToken
 * @param {() => string} getMonitorId
 * @param {HTMLElement} errorElem
 */
export function newPtzController(csrfToken, getMonitorId, errorElem) {
	/** @param {number} status @param {any} body */
	const showError = (status, body) => {
		const detail =
			body === undefined || body === null
				? ""
				: `: ${typeof body === "string" ? body : JSON.stringify(body)}`;
		errorElem.textContent = `${status}${detail}`;
	};

	/** @param {object} cmd */
	const send = async (cmd) => {
		const { status, body } = await fetchPostJson(
			`api/monitor/ptz?id=${getMonitorId()}`,
			cmd,
			csrfToken,
		).catch(() => ({ status: 0, body: undefined }));
		if (status === 0) {
			errorElem.textContent = "network error";
			return 0;
		}
		if (status !== 200) {
			showError(status, body);
		}
		return status;
	};

	/** @type {ReturnType<typeof setInterval>|undefined} */
	let heartbeat;
	let inFlight = false;
	let moving = false;

	/** @param {{pan?: number, tilt?: number, zoom?: number}} vel */
	const move = async (vel) => {
		if (inFlight) {
			return; // Skip beat while a prior move is still in-flight.
		}
		inFlight = true;
		try {
			await send(Object.assign({ command: "move" }, vel));
		} finally {
			inFlight = false;
		}
	};

	const stop = () => {
		send({ command: "stop" });
	};

	/** @param {{pan?: number, tilt?: number, zoom?: number}} vel */
	const holdStart = (vel) => {
		releaseHold(false); // clear any stale state without a stop.
		moving = true;
		move(vel);
		heartbeat = setInterval(() => {
			move(vel);
		}, PTZ_HEARTBEAT_MS);
	};

	/** @param {boolean} sendStop */
	const releaseHold = (sendStop) => {
		if (heartbeat !== undefined) {
			clearInterval(heartbeat);
			heartbeat = undefined;
		}
		if (moving) {
			moving = false;
			if (sendStop) {
				stop();
			}
		}
	};

	const onBlur = () => {
		releaseHold(true);
	};

	/**
	 * Query `capabilities` once. Returns the unsupported-command list —
	 * `[]` on any transport/camera fault (supported-by-default; the
	 * real op still errors honestly if it hits a gap).
	 */
	const capabilities = async () => {
		const { status, body } = await fetchPostJson(
			`api/monitor/ptz?id=${getMonitorId()}`,
			{ command: "capabilities" },
			csrfToken,
		).catch(() => ({ status: 0, body: undefined }));
		if (status !== 200 || body === undefined || body === null) {
			return [];
		}
		return body.unsupported || [];
	};

	/** @param {HTMLElement} presetsElem */
	const loadPresets = async (presetsElem) => {
		const { status, body } = await fetchPostJson(
			`api/monitor/ptz?id=${getMonitorId()}`,
			{ command: "getPresets" },
			csrfToken,
		).catch(() => ({ status: 0, body: undefined }));
		if (status !== 200) {
			showError(status, body);
			return;
		}
		const presets = body === undefined ? [] : body.presets || [];
		presetsElem.replaceChildren();
		for (const p of presets) {
			const row = htmlToElem(/* HTML */ `
				<div class="flex gap-1 items-center">
					<span class="js-ptz-preset-name"></span>
					<button type="button" class="js-ptz-recall px-2 rounded-lg bg-color2 hover:bg-color3">recall</button>
					<button type="button" class="js-ptz-delete px-2 rounded-lg bg-color2 hover:bg-color3">delete</button>
				</div>
			`);
			// textContent only — device-supplied strings.
			row.querySelector(".js-ptz-preset-name").textContent = p.name;
			// @ts-ignore
			row.querySelector(".js-ptz-recall").onclick = () => {
				send({ command: "gotoPreset", presetToken: p.token });
			};
			// @ts-ignore
			row.querySelector(".js-ptz-delete").onclick = async () => {
				const status2 = await send({
					command: "removePreset",
					presetToken: p.token,
				});
				if (status2 === 200) {
					await loadPresets(presetsElem);
				}
			};
			presetsElem.append(row);
		}
	};

	return {
		send,
		capabilities,
		/**
		 * Wire every `.ptz-hold` button under root: pointerdown starts a
		 * hold-to-move with the button's data-pan/tilt/zoom velocity,
		 * pointerup/leave/cancel ends it with a stop.
		 *
		 * @param {HTMLElement|Element} root
		 */
		bindHold(root) {
			for (const btn of /** @type {NodeListOf<HTMLElement>} */ (
				root.querySelectorAll(".ptz-hold")
			)) {
				const vel = {};
				for (const axis of ["pan", "tilt", "zoom"]) {
					if (btn.dataset[axis] !== undefined) {
						// @ts-ignore
						vel[axis] = Number(btn.dataset[axis]);
					}
				}
				btn.addEventListener("pointerdown", (e) => {
					e.preventDefault();
					holdStart(vel);
				});
				for (const ev of ["pointerup", "pointerleave", "pointercancel"]) {
					btn.addEventListener(ev, () => {
						releaseHold(true);
					});
				}
			}
		},
		/**
		 * Wire the preset-save button: validates name (non-empty, ≤64),
		 * sends setPreset, then runs `after` (typically a preset reload).
		 *
		 * @param {HTMLInputElement} nameInput
		 * @param {Element} saveBtn
		 * @param {() => Promise<void>} after
		 */
		bindPresetSave(nameInput, saveBtn, after) {
			// @ts-ignore
			saveBtn.onclick = async () => {
				const name = nameInput.value;
				if (name === "") {
					errorElem.textContent = "preset name required";
					return;
				}
				if (name.length > 64) {
					errorElem.textContent =
						"preset name must be 64 characters or less";
					return;
				}
				const status = await send({ command: "setPreset", presetName: name });
				if (status === 200) {
					nameInput.value = "";
					await after();
				}
			};
		},
		loadPresets,
		/** Cancel heartbeat + send stop if moving. */
		release() {
			releaseHold(true);
		},
		/** Start lifecycle stops (window blur). Call once when shown. */
		attach() {
			window.addEventListener("blur", onBlur);
		},
		/** Stop blur listener + release any hold. */
		detach() {
			releaseHold(true);
			window.removeEventListener("blur", onBlur);
		},
	};
}

/**
 * Disable + label controls for commands the camera reported
 * unsupported. Command → control map shared by the settings panel and
 * the live pad (same DOM classes).
 *
 * @param {Element|HTMLElement} root
 * @param {string[]} unsupported
 * @returns {boolean} `true` when `getPresets` is marked — callers skip
 * the preset list entirely.
 */
export function applyPtzCapabilities(root, unsupported) {
	/** @param {HTMLButtonElement|HTMLInputElement} el */
	const mark = (el) => {
		el.disabled = true;
		el.classList.add("opacity-50");
		el.title = "unsupported on this camera";
	};
	if (unsupported.includes("move") || unsupported.includes("stop")) {
		for (const btn of root.querySelectorAll(".ptz-hold")) {
			mark(/** @type {HTMLButtonElement} */ (btn));
		}
	}
	const home = /** @type {HTMLButtonElement|null} */ (root.querySelector(".js-ptz-home"));
	if (unsupported.includes("home") && home) {
		mark(home);
		home.textContent = "home (unsupported)";
	}
	if (
		["getPresets", "setPreset", "gotoPreset", "removePreset"].some((c) =>
			unsupported.includes(c),
		)
	) {
		for (const el of root.querySelectorAll(".js-ptz-name, .js-ptz-save")) {
			mark(/** @type {HTMLInputElement} */ (el));
		}
		const presets = /** @type {HTMLElement|null} */ (
			root.querySelector(".js-ptz-presets")
		);
		if (presets && unsupported.includes("getPresets")) {
			presets.textContent = "presets unsupported on this camera";
		}
	}
	return unsupported.includes("getPresets");
}

const iconPtzPath = "assets/icons/feather/move.svg";

/**
 * PTZ control for a /live tile. The overlay bar gets a feather-icon
 * `feed-btn` (same chrome as recordings/fullscreen); clicking it toggles
 * a compact pad anchored above the bar. The pad is a `.player-overlay`
 * sibling inside the feed, so once opened it follows the same
 * reveal/hide lifecycle as the bar itself.
 *
 * @param {string} csrfToken
 * @param {string} monitorId
 */
export function newLivePtz(csrfToken, monitorId) {
	const pad = /** @type {HTMLElement} */ (htmlToElem(/* HTML */ `
		<div
			class="js-ptz-live-pad player-overlay absolute flex flex-col gap-1 p-1 rounded-md bg-color1"
			style="z-index: 2; left: 50%; transform: translateX(-50%); bottom: 12%; border: none; font-size: 0.9em;"
			hidden
		>
			<div class="flex gap-1 items-center">
				<button type="button" class="ptz-hold px-2 rounded-lg bg-color2 hover:bg-color3" data-pan="0" data-tilt="0.5">↑</button>
				<button type="button" class="ptz-hold px-2 rounded-lg bg-color2 hover:bg-color3" data-pan="0" data-tilt="-0.5">↓</button>
				<button type="button" class="ptz-hold px-2 rounded-lg bg-color2 hover:bg-color3" data-pan="-0.5" data-tilt="0">←</button>
				<button type="button" class="ptz-hold px-2 rounded-lg bg-color2 hover:bg-color3" data-pan="0.5" data-tilt="0">→</button>
				<button type="button" class="ptz-hold px-2 rounded-lg bg-color2 hover:bg-color3" data-zoom="0.5">zoom+</button>
				<button type="button" class="ptz-hold px-2 rounded-lg bg-color2 hover:bg-color3" data-zoom="-0.5">zoom−</button>
				<button type="button" class="js-ptz-home px-2 rounded-lg bg-color2 hover:bg-color3">home</button>
			</div>
			<div class="js-ptz-presets flex flex-col gap-1"></div>
			<div class="flex gap-1 items-center">
				<input type="text" class="js-ptz-name px-1 bg-color1" placeholder="preset name" maxlength="64" />
				<button type="button" class="js-ptz-save px-2 rounded-lg bg-color2 hover:bg-color3">save</button>
			</div>
			<div class="js-ptz-error text-red"></div>
		</div>
	`));

	const errorElem = /** @type {HTMLElement} */ (pad.querySelector(".js-ptz-error"));
	const presetsElem = /** @type {HTMLElement} */ (pad.querySelector(".js-ptz-presets"));
	const nameInput = /** @type {HTMLInputElement} */ (pad.querySelector(".js-ptz-name"));

	const controller = newPtzController(csrfToken, () => monitorId, errorElem);
	controller.bindHold(pad);
	// @ts-ignore
	pad.querySelector(".js-ptz-home").onclick = () => {
		controller.send({ command: "home" });
	};
	controller.bindPresetSave(nameInput, /** @type {Element} */ (pad.querySelector(".js-ptz-save")), () =>
		controller.loadPresets(presetsElem),
	);

	/** @type {MutationObserver|undefined} */
	let observer;
	let presetsLoaded = false;

	const btn = /** @type {HTMLElement} */ (htmlToElem(/* HTML */ `
		<button type="button" class="js-ptz-live-btn feed-btn p-1 bg-transparent" title="PTZ">
			<img
				class="icon-filter"
				style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
				src="${iconPtzPath}"
			/>
		</button>
	`));

	return {
		elem: btn,
		/**
		 * @param {Element} $overlay
		 */
		init($overlay) {
			if ($overlay.parentElement) {
				$overlay.parentElement.append(pad);
			}
			controller.attach();
			btn.onclick = () => {
				pad.hidden = !pad.hidden;
				if (!pad.hidden && !presetsLoaded) {
					presetsLoaded = true;
					controller
						.capabilities()
						.then((unsupported) =>
							applyPtzCapabilities(pad, unsupported)
								? Promise.resolve()
								: controller.loadPresets(presetsElem),
						)
						.catch(() => {});
				}
			};
			// Unmount/navigation-away: cancel heartbeat + stop. The
			// camera-side Timeout still bounds a stop lost on unload.
			observer = new MutationObserver(() => {
				if (!pad.isConnected) {
					controller.detach();
					if (observer !== undefined) {
						observer.disconnect();
						observer = undefined;
					}
				}
			});
			observer.observe(document.body, { childList: true, subtree: true });
		},
	};
}
