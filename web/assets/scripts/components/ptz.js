// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { fetchPostJson, htmlToElem } from "../libs/common.js";

export const PTZ_HEARTBEAT_MS = 1500;

const UNSUPPORTED_TITLE = "unsupported on this camera";

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
			return status;
		}
		errorElem.textContent = "";
		return status;
	};

	/** @type {ReturnType<typeof setInterval>|undefined} */
	let heartbeat;
	let inFlight = false;
	let moving = false;
	let attached = false;
	/** Commands the camera reported unsupported (FE-known marks). */
	let marks = new Set();

	const stop = () => {
		if (marks.has("stop")) {
			return; // Known-unsupported — don't self-inflict a 502.
		}
		send({ command: "stop" });
	};

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

	/** @param {{pan?: number, tilt?: number, zoom?: number}} vel */
	const move = async (vel) => {
		if (inFlight) {
			return; // Skip beat while a prior move is still in-flight.
		}
		inFlight = true;
		try {
			const status = await send(Object.assign({ command: "move" }, vel));
			if (status !== 200) {
				releaseHold(true); // Faulted move — cancel + release semantics.
			}
		} finally {
			inFlight = false;
		}
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

	const onBlur = () => {
		releaseHold(true);
	};

	/**
	 * Query `capabilities`. Failure is distinct from a successful empty
	 * result — callers treat `ok: false` as "no result" and keep prior
	 * marks.
	 *
	 * @returns {Promise<{ok: boolean, unsupported: string[]}>}
	 */
	const capabilities = async () => {
		const { status, body } = await fetchPostJson(
			`api/monitor/ptz?id=${getMonitorId()}`,
			{ command: "capabilities" },
			csrfToken,
		).catch(() => ({ status: 0, body: undefined }));
		if (status !== 200 || body === undefined || body === null) {
			return { ok: false, unsupported: [...marks] };
		}
		marks = new Set(body.unsupported || []);
		return { ok: true, unsupported: [...marks] };
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
					<span class="js-ptz-wrap"><button type="button" class="js-ptz-recall px-2 rounded-lg bg-color2 hover:bg-color3">recall</button></span>
					<span class="js-ptz-wrap"><button type="button" class="js-ptz-delete px-2 rounded-lg bg-color2 hover:bg-color3">delete</button></span>
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
		// Rows created after the capability probe still get marked.
		applyPtzCapabilities(presetsElem, [...marks]);
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
		/** Send `stop` — skipped when the camera marked it unsupported. */
		stop,
		/** Cancel heartbeat + send stop if moving. */
		release() {
			releaseHold(true);
		},
		/** Start lifecycle stops (window blur). Idempotent. */
		attach() {
			if (!attached) {
				attached = true;
				window.addEventListener("blur", onBlur);
			}
		},
		/** Stop blur listener + release any hold. Idempotent. */
		detach() {
			releaseHold(true);
			if (attached) {
				attached = false;
				window.removeEventListener("blur", onBlur);
			}
		},
	};
}

/**
 * Apply capability marks to the controls under `root`. Per-command map:
 *
 * - `move`/`stop` → every `.ptz-hold` (a camera that can't stop can't
 *   safely move — `stop` marks cascade to all move-inducing controls)
 * - `stop` → `.js-ptz-stop`
 * - `home` → `.js-ptz-home`
 * - `getPresets` → `.js-ptz-presets-btn`
 * - `setPreset` → `.js-ptz-name`, `.js-ptz-save`
 * - `gotoPreset` → `.js-ptz-recall`
 * - `removePreset` → `.js-ptz-delete`
 *
 * Marks dim + strike the control via `.ptz-unsupported` and put the
 * tooltip on its `.js-ptz-wrap` (a disabled control swallows pointer
 * events — the title must live on the wrapper). Commands absent from
 * `unsupported` get unmarked, so a real 200 result shrinks the set.
 *
 * @param {Element|HTMLElement} root
 * @param {string[]} unsupported
 * @param {() => void} [onHoldMarked] called when a `.ptz-hold` was
 * marked — a held button loses pointer events mid-hold, so the caller
 * must release (cancel heartbeat + stop).
 * @returns {boolean} `true` when `getPresets` is marked — callers skip
 * the preset list entirely.
 */
export function applyPtzCapabilities(root, unsupported, onHoldMarked) {
	const marks = new Set(unsupported);
	const holdMarked = marks.has("move") || marks.has("stop");
	/** @param {Element} el @param {boolean} marked */
	const set = (el, marked) => {
		const wrap = el.closest(".js-ptz-wrap");
		if (marked) {
			if (wrap !== null) {
				wrap.setAttribute("title", UNSUPPORTED_TITLE);
			}
			// @ts-ignore
			el.disabled = true;
			el.classList.add("ptz-unsupported");
		} else {
			if (wrap !== null) {
				wrap.removeAttribute("title");
			}
			// @ts-ignore
			el.disabled = false;
			el.classList.remove("ptz-unsupported");
		}
	};
	/** @type {Array<[boolean, NodeListOf<Element>]>} */
	const map = [
		[holdMarked, root.querySelectorAll(".ptz-hold")],
		[marks.has("stop"), root.querySelectorAll(".js-ptz-stop")],
		[marks.has("home"), root.querySelectorAll(".js-ptz-home")],
		[marks.has("getPresets"), root.querySelectorAll(".js-ptz-presets-btn")],
		[marks.has("setPreset"), root.querySelectorAll(".js-ptz-name, .js-ptz-save")],
		[marks.has("gotoPreset"), root.querySelectorAll(".js-ptz-recall")],
		[marks.has("removePreset"), root.querySelectorAll(".js-ptz-delete")],
	];
	let holdApplied = false;
	for (const [marked, els] of map) {
		for (const el of els) {
			set(el, marked);
		}
		if (marked && els.length > 0 && els[0].classList.contains("ptz-hold")) {
			holdApplied = true;
		}
	}
	if (holdApplied && onHoldMarked !== undefined) {
		onHoldMarked();
	}
	return marks.has("getPresets");
}

const iconPtzPath = "assets/icons/feather/move.svg";

const glyphBtn = "ptz-glyph px-2 rounded-lg bg-color2 hover:bg-color3";

/**
 * PTZ control for a /live tile. The overlay bar gets a feather-icon
 * `feed-btn` (same chrome as recordings/fullscreen); clicking it toggles
 * a vertical rail docked mid-right inside the tile plus a preset
 * popover anchored to its left. DOM order is rail → popover → bar so
 * the forward-only `.player-overlay:hover ~ .player-overlay` reveal
 * keeps the bar lit while the rail or popover is hovered.
 *
 * @param {string} csrfToken
 * @param {string} monitorId
 */
export function newLivePtz(csrfToken, monitorId) {
	const rail = /** @type {HTMLElement} */ (htmlToElem(/* HTML */ `
		<div class="js-ptz-live-pad player-overlay ptz-rail" hidden>
			<div class="flex flex-col items-center gap-0.5">
				<span class="js-ptz-wrap"><button type="button" class="ptz-hold ${glyphBtn}" data-pan="0" data-tilt="0.5">↑</button></span>
				<div class="flex gap-0.5">
					<span class="js-ptz-wrap"><button type="button" class="ptz-hold ${glyphBtn}" data-pan="-0.5" data-tilt="0">←</button></span>
					<span class="js-ptz-wrap"><button type="button" class="js-ptz-stop ${glyphBtn}">■</button></span>
					<span class="js-ptz-wrap"><button type="button" class="ptz-hold ${glyphBtn}" data-pan="0.5" data-tilt="0">→</button></span>
				</div>
				<span class="js-ptz-wrap"><button type="button" class="ptz-hold ${glyphBtn}" data-pan="0" data-tilt="-0.5">↓</button></span>
			</div>
			<div class="ptz-rail-sep"></div>
			<span class="js-ptz-wrap"><button type="button" class="ptz-hold ${glyphBtn}" data-zoom="0.5">+</button></span>
			<span class="js-ptz-wrap"><button type="button" class="ptz-hold ${glyphBtn}" data-zoom="-0.5">−</button></span>
			<span class="js-ptz-wrap"><button type="button" class="js-ptz-home ${glyphBtn}">⌂</button></span>
			<span class="js-ptz-wrap"><button type="button" class="js-ptz-presets-btn ${glyphBtn}">★</button></span>
			<div class="js-ptz-error text-red"></div>
		</div>
	`));

	const popover = /** @type {HTMLElement} */ (htmlToElem(/* HTML */ `
		<div class="js-ptz-presets-pop player-overlay ptz-popover" hidden>
			<div class="js-ptz-presets flex flex-col gap-1"></div>
			<div class="flex gap-1 items-center mt-1">
				<span class="js-ptz-wrap"><input type="text" class="js-ptz-name px-1 bg-color1" placeholder="preset name" maxlength="64" /></span>
				<span class="js-ptz-wrap"><button type="button" class="js-ptz-save px-2 rounded-lg bg-color2 hover:bg-color3">save</button></span>
			</div>
		</div>
	`));

	const errorElem = /** @type {HTMLElement} */ (rail.querySelector(".js-ptz-error"));
	const presetsElem = /** @type {HTMLElement} */ (popover.querySelector(".js-ptz-presets"));
	const nameInput = /** @type {HTMLInputElement} */ (popover.querySelector(".js-ptz-name"));

	const controller = newPtzController(csrfToken, () => monitorId, errorElem);
	controller.bindHold(rail);
	// @ts-ignore
	rail.querySelector(".js-ptz-stop").onclick = () => {
		controller.stop();
	};
	// @ts-ignore
	rail.querySelector(".js-ptz-home").onclick = () => {
		controller.send({ command: "home" });
	};
	const presetsBtn = /** @type {HTMLElement} */ (rail.querySelector(".js-ptz-presets-btn"));
	controller.bindPresetSave(nameInput, /** @type {Element} */ (popover.querySelector(".js-ptz-save")), () =>
		controller.loadPresets(presetsElem),
	);

	/** @type {MutationObserver|undefined} */
	let observer;

	const refresh = () => {
		controller
			.capabilities()
			.then((res) => {
				if (!res.ok) {
					return res; // Failed query isn't a result — keep prior marks.
				}
				const skip = applyPtzCapabilities(rail, res.unsupported, () =>
					controller.release(),
				);
				applyPtzCapabilities(popover, res.unsupported);
				return skip ? res : controller.loadPresets(presetsElem);
			})
			.catch(() => {});
	};

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
			// rail → popover → bar: forward-sibling hover reveal.
			$overlay.before(rail, popover);
			controller.attach();
			presetsBtn.onclick = () => {
				popover.hidden = !popover.hidden;
			};
			btn.onclick = () => {
				rail.hidden = !rail.hidden;
				if (rail.hidden) {
					popover.hidden = true;
					controller.release();
					return;
				}
				refresh();
			};
			// Unmount/navigation-away: cancel heartbeat + stop. The
			// camera-side Timeout still bounds a stop lost on unload.
			observer = new MutationObserver(() => {
				if (!rail.isConnected) {
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
