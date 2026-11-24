// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import { fetchPostJson, htmlToElem } from "../libs/common.js";

const UNSUPPORTED_TITLE = "unsupported on this camera";

/** Clear a wrap's title back to its `data-hint` (idle hint), or
 * remove it when no hint exists. */
const clearWrapTitle = (wrap) => {
	const { hint } = wrap.dataset;
	if (hint === undefined) {
		wrap.removeAttribute("title");
	} else {
		wrap.setAttribute("title", hint);
	}
};

/**
 * WS URL for the audio channel — `ws:`/`wss:` follows
 * `location.protocol` so a TLS front-end still works.
 *
 * @param {string} token
 */
function audioWsUrl(token) {
	const scheme = location.protocol === "https:" ? "wss:" : "ws:";
	return `${scheme}//${location.host}/api/monitor/audio?token=${token}`;
}

/**
 * Shared two-way audio channel: token mint → WS upgrade → AudioCtl
 * text JSON + binary PCM Int16. Both the settings test row and the
 * /live tile cluster drive the same routes.
 *
 * @param {string} csrfToken
 * @param {() => string} getMonitorId
 * @param {HTMLElement} errorElem
 */
export function newAudioController(csrfToken, getMonitorId, errorElem) {
	/** @type {WebSocket|undefined} */
	let ws;
	let listening = false;
	let talking = false;
	/** Directions the camera disproved — FE-known marks. */
	let marks = new Set();
	/** Another consumer holds the talk lock — cleared only by a
	 * `caps` carrying `talkFree:true`. */
	let talkBusy = false;
	let attached = false;
	/** The pointer that initiated the active talk hold. */
	let talkPointerId;

	/** Mic gain ∈ [0.25, 4] — loaded from localStorage at use time
	 * (the monitor id resolves lazily — ⊥ cache the key). */
	let gainValue = 1;
	const gainKey = () => `audioGain-${getMonitorId()}`;
	const loadGain = () => {
		const v = localStorage.getItem(gainKey());
		if (v === null) {
			return 1;
		}
		const n = Number(v);
		return Number.isFinite(n) ? Math.min(4, Math.max(0.25, n)) : 1;
	};
	/** Re-read the stored gain for the current monitor — call on
	 * capture start and panel show. Returns the live value. */
	const refreshGain = () => {
		gainValue = loadGain();
		return gainValue;
	};
	/** @param {number} v */
	const setGain = (v) => {
		gainValue = Math.min(4, Math.max(0.25, v));
		localStorage.setItem(gainKey(), String(gainValue));
	};

	// --- audio plumbing -------------------------------------------------

	/** @type {AudioContext|undefined} */
	let playCtx;
	/** @type {AudioContext|undefined} */
	let captureCtx;
	/** @type {MediaStream|undefined} */
	let micStream;

	/** PCM playback — decoded Int16 frames become Float32 buffers. */
	const ensurePlayback = () => {
		if (playCtx === undefined) {
			playCtx = new AudioContext({ sampleRate: 8000 });
		}
		return playCtx;
	};

	/** @param {ArrayBuffer} buf */
	const playPcm = (buf) => {
		const ctx = ensurePlayback();
		const int16 = new Int16Array(buf);
		const audio = ctx.createBuffer(1, int16.length, 8000);
		const channel = audio.getChannelData(0);
		for (const [i, element] of int16.entries()) {
			channel[i] = element / 32768;
		}
		const src = ctx.createBufferSource();
		src.buffer = audio;
		src.connect(ctx.destination);
		src.start();
	};

	/** Mic capture → 8 kHz Int16 → binary frames. */
	const startCapture = async () => {
		refreshGain();
		micStream = await navigator.mediaDevices.getUserMedia({
			audio: {
				channelCount: 1,
				sampleRate: 8000,
				echoCancellation: true,
				noiseSuppression: true,
				autoGainControl: true,
			},
		});
		captureCtx = new AudioContext({ sampleRate: 8000 });
		const source = captureCtx.createMediaStreamSource(micStream);
		const rate = captureCtx.sampleRate;
		const decimation = Math.round(rate / 8000);
		// ScriptProcessor is deprecated but universally available —
		// AudioWorklet needs a separate asset the daemon would have to
		// serve; the 8 kHz decimation is cheap either way. The small
		// buffer keeps bursts short (~64 ms @ 8 kHz).
		const node = captureCtx.createScriptProcessor(512, 1, 1);
		// Wire frames are always a full 20 ms (160 samples @ 8 kHz) —
		// the decimated remainder carries between callbacks so a
		// foreign-rate context can't emit sub-frame parts.
		let pending = new Int16Array(0);
		node.onaudioprocess = (ev) => {
			if (ws === undefined || ws.readyState !== WebSocket.OPEN) {
				return;
			}
			const input = ev.inputBuffer.getChannelData(0);
			const merged = new Int16Array(
				pending.length + Math.floor(input.length / decimation),
			);
			merged.set(pending);
			for (let i = pending.length; i < merged.length; i++) {
				const s = Math.max(
					-1,
					Math.min(1, input[(i - pending.length) * decimation] * gainValue),
				);
				merged[i] = s < 0 ? s * 32768 : s * 32767;
			}
			let off = 0;
			for (; off + 160 <= merged.length; off += 160) {
				ws.send(merged.buffer.slice(off * 2, (off + 160) * 2));
			}
			pending = merged.slice(off);
		};
		source.connect(node);
		node.connect(captureCtx.destination);
	};

	const stopCapture = () => {
		if (micStream !== undefined) {
			for (const track of micStream.getTracks()) {
				track.stop();
			}
			micStream = undefined;
		}
		if (captureCtx !== undefined) {
			captureCtx.close().catch(() => {});
			captureCtx = undefined;
		}
	};

	// --- wire -----------------------------------------------------------

	/** @param {string} type */
	const sendCtrl = (type) => {
		if (ws !== undefined && ws.readyState === WebSocket.OPEN) {
			ws.send(JSON.stringify({ type }));
		}
	};

	/** @param {MessageEvent} ev */
	const onMessage = (ev) => {
		if (typeof ev.data !== "string") {
			// Binary = decoded camera→client PCM.
			if (listening && ev.data instanceof ArrayBuffer) {
				playPcm(ev.data);
			}
			return;
		}
		/** @type {{type?: string, listen?: boolean, talk?: boolean, talkFree?: boolean, unsupported?: string[], command?: string, message?: string}} */
		const ctrl = JSON.parse(ev.data);
		switch (ctrl.type) {
			case "caps": {
				marks = new Set(ctrl.unsupported || []);
				if (ctrl.talkFree === true) {
					talkBusy = false;
				}
				applyAudioCaps();
				break;
			}
			case "busy": {
				busyTalk();
				break;
			}
			case "error": {
				errorElem.textContent = ctrl.message || "audio error";
				break;
			}
			default: {
				break;
			} // unknown ctrl — forward-compat ignore
		}
	};

	// --- controls -------------------------------------------------------

	/** @returns {number} */
	const getGain = () => gainValue;

	/** @type {HTMLButtonElement|undefined} */
	let listenBtn;
	/** @type {HTMLButtonElement|undefined} */
	let talkBtn;
	/** @type {HTMLElement|undefined} */
	let talkWrap;

	const applyAudioCaps = () => {
		if (listenBtn !== undefined) {
			const marked = marks.has("listen");
			listenBtn.disabled = marked;
			listenBtn.classList.toggle("ptz-unsupported", marked);
			const wrap = listenBtn.closest(".js-audio-wrap");
			if (wrap !== null) {
				if (marked) {
					wrap.setAttribute("title", UNSUPPORTED_TITLE);
				} else {
					clearWrapTitle(wrap);
				}
			}
		}
		if (talkBtn !== undefined && talkWrap !== undefined) {
			const marked = marks.has("talk");
			const held = marked || talkBusy;
			talkBtn.disabled = held;
			talkBtn.classList.toggle("ptz-unsupported", held);
			if (marked) {
				talkWrap.setAttribute("title", UNSUPPORTED_TITLE);
			} else if (talkBusy) {
				talkWrap.setAttribute("title", "another session is talking");
			} else {
				clearWrapTitle(talkWrap);
			}
		}
	};

	/** Another consumer holds the talk lock — marked until a `caps`
	 * ctrl carrying `talkFree:true` clears it. */
	const busyTalk = () => {
		talkBusy = true;
		applyAudioCaps();
	};

	const ensureWs = async () => {
		if (ws !== undefined && ws.readyState === WebSocket.OPEN) {
			return ws;
		}
		const { status, body } = await fetchPostJson(
			`api/monitor/audio-session?id=${getMonitorId()}`,
			{},
			csrfToken,
		).catch(() => ({ status: 0, body: undefined }));
		if (status !== 200 || body === undefined || body === null) {
			errorElem.textContent = status === 0 ? "network error" : `${status}`;
			return;
		}
		ws = new WebSocket(audioWsUrl(body.token));
		ws.binaryType = "arraybuffer";
		ws.onmessage = onMessage;
		ws.onerror = () => {
			errorElem.textContent = "audio socket error";
		};
		ws.onclose = () => {
			listening = false;
			talking = false;
			stopCapture();
			updateButtons();
		};
		await new Promise((resolve) => {
			if (ws === undefined) {
				resolve();
				return;
			}
			ws.onopen = resolve;
			ws.addEventListener("error", resolve, { once: true });
		});
		return ws;
	};

	const updateButtons = () => {
		if (listenBtn !== undefined) {
			listenBtn.classList.toggle("audio-active", listening);
		}
		if (talkBtn !== undefined) {
			talkBtn.classList.toggle("audio-active", talking);
		}
	};

	const listenToggle = async () => {
		if (listening) {
			sendCtrl("listen-stop");
			listening = false;
			updateButtons();
			return;
		}
		if ((await ensureWs()) === undefined) {
			return;
		}
		sendCtrl("listen");
		listening = true;
		updateButtons();
	};

	const talkStart = async (pointerId) => {
		if (talking) {
			return;
		}
		if ((await ensureWs()) === undefined) {
			return;
		}
		sendCtrl("talk-start");
		talking = true;
		talkPointerId = pointerId;
		try {
			await startCapture();
		} catch (error) {
			void error;
			errorElem.textContent = "microphone unavailable";
			sendCtrl("talk-end");
			talking = false;
			talkPointerId = undefined;
		}
		updateButtons();
	};

	/** @param {number} [pointerId] */
	const talkEnd = (pointerId) => {
		if (!talking) {
			return;
		}
		if (
			pointerId !== undefined &&
			talkPointerId !== undefined &&
			pointerId !== talkPointerId
		) {
			return; // A different press's release can't end this hold.
		}
		sendCtrl("talk-end");
		stopCapture();
		talking = false;
		talkPointerId = undefined;
		updateButtons();
	};

	const onBlur = () => {
		talkEnd();
	};

	return {
		listenToggle,
		/**
		 * Wire the hold-to-talk button under root: pointerdown (primary)
		 * starts talk + capture; every release path ends it.
		 *
		 * @param {HTMLElement|Element} root
		 */
		bindTalk(root) {
			talkBtn = /** @type {HTMLButtonElement} */ (
				root.querySelector(".js-audio-talk")
			);
			talkWrap = /** @type {HTMLElement} */ (
				root.querySelector(".js-audio-talk-wrap")
			);
			if (talkBtn === undefined || talkBtn === null) {
				return;
			}
			talkBtn.addEventListener("pointerdown", (e) => {
				if (e.button !== 0) {
					return;
				}
				e.preventDefault();
				talkStart(e.pointerId);
			});
			for (const ev of ["pointerup", "pointerleave", "pointercancel"]) {
				talkBtn.addEventListener(ev, (e) => {
					talkEnd(/** @type {PointerEvent} */ (e).pointerId);
				});
			}
		},
		/** @param {HTMLElement|Element} root */
		bindListen(root) {
			listenBtn = /** @type {HTMLButtonElement} */ (
				root.querySelector(".js-audio-listen")
			);
			if (listenBtn === undefined || listenBtn === null) {
				return;
			}
			listenBtn.onclick = () => {
				listenToggle();
			};
		},
		/** Re-emit caps marks — call after binding both controls. */
		applyAudioCaps,
		setGain,
		getGain,
		refreshGain,
		/** Feed a server `caps` payload through mark/busy handling —
		 * used by harnesses that drive the wire directly. */
		onCaps(caps) {
			marks = new Set(caps.unsupported || []);
			if (caps.talkFree === true) {
				talkBusy = false;
			}
			applyAudioCaps();
		},
		/** Attach lifecycle stops (window blur). Idempotent. */
		attach() {
			if (!attached) {
				attached = true;
				window.addEventListener("blur", onBlur);
			}
		},
		/** Release the hold + socket + capture. Idempotent. */
		detach() {
			talkEnd();
			sendCtrl("listen-stop");
			if (ws !== undefined) {
				ws.close();
				ws = undefined;
			}
			if (attached) {
				attached = false;
				window.removeEventListener("blur", onBlur);
			}
		},
	};
}

/**
 * Seed the FE-known mark set from the stored monitor config — the
 * `unsupported` list survives restarts, so controls render disabled
 * before the first `caps` arrives.
 *
 * @param {Element|HTMLElement} root
 * @param {string[]} unsupported
 */
export function applyAudioMarks(root, unsupported) {
	const marks = new Set(unsupported);
	/** @param {Element|null} el @param {boolean} marked */
	const set = (el, marked) => {
		if (el === null) {
			return;
		}
		const wrap = el.closest(".js-audio-wrap");
		if (marked) {
			if (wrap !== null) {
				wrap.setAttribute("title", UNSUPPORTED_TITLE);
			}
			// @ts-ignore
			el.disabled = true;
			el.classList.add("ptz-unsupported");
		} else {
			if (wrap !== null) {
				clearWrapTitle(wrap);
			}
			// @ts-ignore
			el.disabled = false;
			el.classList.remove("ptz-unsupported");
		}
	};
	set(root.querySelector(".js-audio-listen"), marks.has("listen"));
	set(root.querySelector(".js-audio-talk"), marks.has("talk"));
}

const iconSpeakerPath = "assets/icons/feather/volume-2.svg";
const iconMicPath = "assets/icons/feather/mic.svg";

/**
 * Audio cluster for a /live tile bar: speaker toggle (listen) + mic
 * hold-to-talk. Talk controls need `isSecureContext`; listen works on
 * plain `http://` (WS + AudioContext only).
 *
 * @param {string} csrfToken
 * @param {string} monitorId
 */
export function newLiveAudio(csrfToken, monitorId) {
	const elem = /** @type {HTMLElement} */ (
		htmlToElem(/* HTML */ `
			<div class="audio-cluster flex items-center">
				<span class="js-audio-wrap" title="listen" data-hint="listen"
					><button
						type="button"
						class="js-audio-listen feed-btn p-1 bg-transparent"
					>
						<img
							class="icon-filter"
							style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
							src="${iconSpeakerPath}"
							alt="listen"
						/></button
				></span>
				<span
					class="js-audio-wrap js-audio-talk-wrap"
					title="hold to talk"
					data-hint="hold to talk"
					><button
						type="button"
						class="js-audio-talk feed-btn p-1 bg-transparent"
					>
						<img
							class="icon-filter"
							style="height: calc(var(--scale) * 1.5rem); aspect-ratio: 1;"
							src="${iconMicPath}"
							alt="talk"
						/></button
				></span>
				<div class="js-audio-error text-red"></div>
			</div>
		`)
	);
	const errorElem = /** @type {HTMLElement} */ (elem.querySelector(".js-audio-error"));
	const controller = newAudioController(csrfToken, () => monitorId, errorElem);
	controller.bindListen(elem);
	controller.bindTalk(elem);
	controller.attach();

	// Per-direction context gates — talk is secure-context-only.
	if (!window.isSecureContext) {
		const talk = /** @type {HTMLButtonElement} */ (
			elem.querySelector(".js-audio-talk")
		);
		talk.disabled = true;
		const wrap = elem.querySelector(".js-audio-talk-wrap");
		if (wrap !== null) {
			wrap.setAttribute("title", "requires a secure context");
		}
	}
	return { elem, controller };
}
