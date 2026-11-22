// SPDX-License-Identifier: GPL-2.0-or-later

// @ts-check

import {
	fetchGet,
	fetchPost,
	fetchPostJson,
	sanitizeMonitorName,
	fetchPut,
	fetchDelete,
	sortByName,
	removeEmptyValues,
	relativePathname,
	htmlToElem,
} from "./libs/common.js";
import {
	newForm,
	newField,
	newErrorField,
	newRawSelectField,
	newRawSelectCustomField,
	inputRules,
	fieldTemplate,
	newPasswordField,
} from "./components/form.js";
import { newModal } from "./components/modal.js";

/** @typedef {import("./components/form.js").InputRule} InputRule */

/**
 * @typedef Category
 * @property {Element} navElem
 * @property {Element} elem
 */

/** @param {Category[]} categories */
function rendererCategories(categories) {
	const navElems = [];
	const categoryElems = [];
	for (const category of Object.values(categories)) {
		navElems.push(category.navElem);
		categoryElems.push(category.elem);
	}

	return [
		htmlToElem(
			/* HTML */ `
				<nav
					id="js-settings-navbar"
					class="settings-navbar shrink-0 h-full bg-color2"
				></nav>
			`,
			[htmlToElem(`<ul class="h-full" style="overflow-y: auto;"></ul>`, navElems)],
		),
		...categoryElems,
	];
}

const backIconPath = "assets/icons/feather/arrow-left.svg";

const backIconHTML = /* HTML */ `
	<img
		class="icon-filter"
		style="width: calc(var(--scale) * 3rem);"
		src="${backIconPath}"
	/>
`;

function newCategoryTitle(title = "") {
	return htmlToElem(/* HTML */ `
		<span
			class="w-full m-auto text-center text-2 text-color"
			style="margin-right: calc(var(--scale) * 3rem);"
			>${title}</span
		>
	`);
}

/**
 * @param {string} data
 * @param {string} label
 */
function newCategoryNav(data, label, c = "text-color") {
	return htmlToElem(/* HTML */ `
		<li
			class="flex items-center py-1 px-4 border border-color3 hover:bg-color3"
			data="${data}"
		>
			<span class="text-2 ${c}">${label}</span>
		</li>
	`);
}

const addBtnHTML = /* HTML */ `
	<button
		class="js-add-btn shrink-0 mt-2 ml-4 mr-auto px-2 rounded-md bg-green hover:bg-green2"
	>
		<span class="text-2 text-color">Add</span>
	</button>
`;

/**
 * @param {string} title
 * @param {string} icon
 * @param {Form} form
 * onNav should return the sub-category title (only visible on mobile).
 * @param {(element: Element) => string} onNav
 */
function newCategory(title, icon, form, onNav) {
	/** @type {() => void} */
	let onOpen;

	const $nav = htmlToElem(/* HTML */ `
		<ul
			class="flex flex-col h-full"
			style="
				overflow-y: scroll;
				padding-bottom: calc(var(--spacing) * 12);
			"
		></ul>
	`);

	/** @type {HTMLElement} */
	// @ts-ignore
	const $backBtn = htmlToElem(`<nav class="flex shrink-0">${backIconHTML}</nav>`);
	const $category = htmlToElem(
		/* HTML */ `
			<div
				class="settings-category flex flex-col shrink-0 h-full bg-color2"
				style="z-index: 0; overflow-y: auto;"
			></div>
		`,
		[
			htmlToElem(
				`<div class="settings-menubar px-2 border border-color3 bg-color2"></div>`,
				[$backBtn, newCategoryTitle(title)],
			),
			$nav,
		],
	);

	/** @type {HTMLElement} */
	// @ts-ignore
	const $subBackBtn = htmlToElem(`<nav class="flex shrink-0">${backIconHTML}</nav>`);
	const $subTitle = newCategoryTitle();
	const $subcategory = htmlToElem(
		`<div class="settings-sub-category flex flex-col w-full bg-color3"></div>`,
		[
			htmlToElem(
				/* HTML */ `
					<div
						class="settings-menubar px-2 border border-color3 bg-color2"
						style="height: var(--topbar-height);"
					></div>
				`,
				[$subBackBtn, $subTitle],
			),
			form.elem(),
		],
	);

	const closeSubcategory = () => {
		for (const element of $category.querySelectorAll(`.settings-nav-btn-selected`)) {
			element.classList.remove("settings-nav-btn-selected");
		}
		$subcategory.classList.remove("settings-subcategory-open");
	};
	$subBackBtn.onclick = closeSubcategory;

	/** @param {Element} subNavElem */
	const openSubcategory = (subNavElem) => {
		closeSubcategory();

		if (!subNavElem.classList.contains("js-add-btn")) {
			subNavElem.classList.add("settings-nav-btn-selected");
		}

		$subcategory.classList.add("settings-subcategory-open");
	};

	const $wrapper = htmlToElem(
		//
		`<div class="settings-category-wrapper"></div>`,
		[$category, $subcategory],
	);

	const close = () => {
		$wrapper.classList.remove("settings-category-selected");
		// @ts-ignore
		for (const element of document.querySelectorAll(".settings-nav-btn-selected")) {
			element.classList.remove("settings-nav-btn-selected");
		}
	};
	$backBtn.onclick = close;

	/** @type {HTMLImageElement} */
	// @ts-ignore
	const $navIcon = htmlToElem(/* HTML */ `
		<img
			class="mr-2 icon-filter"
			style="
				aspect-ratio: 1;
				height: calc(var(--scale) * 2.4rem);
				font-size: calc(var(--scale) * 2.7rem);
			"
		/>
	`);
	$navIcon.src = icon;

	const $navTitle = htmlToElem(`<span class="text-2 text-color"></span>`);
	$navTitle.textContent = title;

	const navElem = htmlToElem(
		/* HTML */ `
			<li
				class="flex items-center py-1 pl-4 border border-color3 hover:bg-color3"
				style="padding-right: calc(var(--spacing) * 14);"
			></li>
		`,
		[$navIcon, $navTitle],
	);
	navElem.addEventListener("click", () => {
		// Highlight category button.
		for (const element of document.querySelectorAll(".settings-nav-btn-selected")) {
			element.classList.remove("settings-nav-btn-selected");
		}
		navElem.classList.add("settings-nav-btn-selected");

		// Show category.
		for (const element of document.querySelectorAll(".settings-category-selected")) {
			element.classList.remove("settings-category-selected");
		}
		$wrapper.classList.add("settings-category-selected");

		closeSubcategory();
		onOpen();
	});

	return {
		navElem,
		elem: $wrapper,

		/** @param {() => void} cb */
		setOnOpen(cb) {
			onOpen = cb;
		},

		/** @param {Element[]} elements */
		setNav(elements) {
			$nav.replaceChildren(...elements);
			for (const element of $nav.children) {
				element.addEventListener("click", () => {
					openSubcategory(element);
					const title = onNav(element);
					$subTitle.textContent = title;
				});
			}
		},
		/** @param {string} title */
		setTitle(title) {
			$subTitle.innerHTML = title;
		},

		closeSubcategory,
	};
}

/**
 * @typedef Monitor
 * @property {string} id
 * @property {string} name
 */

/** @typedef {{[x: string]: Monitor}} Monitors */

/** @typedef {import("./components/form.js").Form} Form */

/**
 * @template T
 * @typedef {import("./components/form.js").Field<T>} Field
 */

/**
 * @template T
 * @typedef {import("./components/form.js").Fields<T>} Fields
 */

/**
 * Camera-scan modal: runs POST /api/scan, renders results with
 * textContent only (camera strings are untrusted), and prefills the
 * monitor form on pick. A 409 response is reported as an in-progress
 * scan rather than a failure.
 *
 * @param {string} token
 * @returns {{elem: Element, open: () => void, onPick: (p: object) => void}}
 */
/** Masks an embedded password in a stream url for display. @param {string} url */
const maskUrlPassword = (url) =>
	url.replace(/:\/\/([^/:@]+):[^/@]*@/, "://$1:•••@");

function newScanModal(token) {
	const hostField = newField({
		input: "text",
		label: "Camera IP",
		placeholder: "optional — empty scans LAN",
	});
	const userField = newField({
		input: "text",
		label: "Username",
		placeholder: "optional",
	});
	const passField = newField({
		input: "password",
		label: "Password",
		placeholder: "optional",
	});
	const statusElem = document.createElement("div");
	statusElem.className = "px-2 pb-1 text-1.5";
	const listElem = document.createElement("ul");

	const scanBtn = htmlToElem(/* HTML */ `
		<button class="m-2 px-2 rounded-lg bg-color2 hover:bg-color3">
			<span class="text-2 text-color">Scan</span>
		</button>
	`);

	const modal = newModal("Camera scan", [
		htmlToElem("<ul></ul>", [
			hostField.elems[0],
			userField.elems[0],
			passField.elems[0],
		]),
		htmlToElem("<div class=\"flex\"></div>", [scanBtn]),
		statusElem,
		listElem,
	]);

	/** @type {(p: object) => void} */
	let onPick = () => {};

	/** @param {HTMLElement} parent @param {string} text */
	const row = (parent, text) => {
		const el = document.createElement("span");
		el.textContent = text;
		parent.append(el);
	};

	/** Display label for a stream; password masked, codec flagged. @param {any} st */
	const streamLabel = (st) =>
		`${st.role}: ${maskUrlPassword(st.rtspUrl)}${
			st.profileName ? ` (${st.profileName})` : ""
		}${st.live === false ? " [dead — no media]" : ""}${
			st.videoCodec === "h264" ? "" : ` [${st.videoCodec} — unsupported]`
		}`;

	const renderResults = (result) => {
		listElem.replaceChildren();
		statusElem.textContent = result.truncated
			? "scan timed out — partial results"
			: `${result.cameras.length} camera(s) found`;
		for (const cam of result.cameras) {
			const item = document.createElement("li");
			item.className = "px-2 pb-1 border-b-2 border-color1";
			const header = document.createElement("div");
			header.className = "text-1.5";
			row(header, cam.ip);
			if (cam.name) {
				row(header, ` — ${cam.name}`);
			}
			if (cam.hardware) {
				row(header, ` (${cam.hardware})`);
			}
			const badges = [];
			if (cam.needsAuth) {
				badges.push("auth");
			}
			if (cam.ptz) {
				badges.push("ptz");
			}
			if (cam.backchannel) {
				badges.push("audio");
			}
			if (badges.length > 0) {
				row(header, ` [${badges.join(" ")}]`);
			}
			item.append(header);
			// Per-camera slot selection: defaults to the role-derived
			// assignment. Both the dropdowns and the per-stream →main/→sub
			// buttons drive the same `sel` state.
			const sel = {
				main: cam.streams.find((st) => st.role === "main"),
				sub: cam.streams.find((st) => st.role === "sub"),
			};
			const selects = {
				main: newRawSelectField("Main stream", []),
				sub: newRawSelectField("Sub stream", []),
			};
			const syncSelects = () => {
				for (const slot of ["main", "sub"]) {
					const $input = selects[slot].$input;
					$input.value =
						sel[slot] === undefined
							? ""
							: String(cam.streams.indexOf(sel[slot]));
				}
			};
			for (const slot of ["main", "sub"]) {
				const $input = selects[slot].$input;
				const none = document.createElement("option");
				none.value = "";
				none.textContent = "— none —";
				$input.append(none);
				for (const [i, st] of cam.streams.entries()) {
					const opt = document.createElement("option");
					opt.value = String(i);
					opt.textContent = streamLabel(st);
					$input.append(opt);
				}
				$input.onchange = () => {
					sel[slot] =
						$input.value === ""
							? undefined
							: cam.streams[Number($input.value)];
				};
			}
			syncSelects();
			item.append(
				htmlToElem("<ul></ul>", [
					selects["main"].elem,
					selects["sub"].elem,
				]),
			);
			for (const stream of cam.streams) {
				const s = document.createElement("div");
				s.className = "px-1";
				const text = document.createElement("span");
				text.className = "text-1.5";
				text.textContent = streamLabel(stream);
				s.append(text);
				for (const slot of ["main", "sub"]) {
					const assign = htmlToElem(/* HTML */ `
						<button
							class="ml-1 px-1 rounded-lg bg-color2 hover:bg-color3"
						>
							<span class="text-1.5 text-color"></span>
						</button>
					`);
					assign.querySelector("span").textContent = `→${slot}`;
					assign.onclick = () => {
						sel[slot] = stream;
						syncSelects();
						statusElem.textContent = `${slot} ← ${maskUrlPassword(
							stream.rtspUrl,
						)}`;
					};
					s.append(assign);
				}
				item.append(s);
			}
			if (cam.streams.length > 0) {
				const pickBtn = htmlToElem(/* HTML */ `
					<button class="m-2 px-2 rounded-lg bg-green hover:bg-green2">
						<span class="text-2 text-color">Use</span>
					</button>
				`);
				/** Injects modal-supplied creds into a creds-free stream url. @param {string} url */
				const injectCreds = (url) => {
					const user = userField.value();
					const pass = passField.value();
					if (user === "" && pass === "") {
						return url;
					}
					if (/^rtsp:\/\/[^/@]*@/.test(url)) {
						return url;
					}
					return url.replace(
						"rtsp://",
						`rtsp://${encodeURIComponent(user)}:${encodeURIComponent(pass)}@`,
					);
				};
				pickBtn.onclick = () => {
					const picked = [sel.main, sel.sub].filter((st) => st !== undefined);
					const unsupported = picked.filter((st) => st.videoCodec !== "h264");
					const dead = picked.filter((st) => st.live === false);
					const warnings = [
						...(unsupported.length > 0
							? [
									`picked stream(s) use ${unsupported
										.map((st) => st.videoCodec)
										.join(", ")} — SentryShot only plays h264`,
								]
							: []),
						...(dead.length > 0
							? ["picked stream(s) sent no media during the probe — dead"]
							: []),
					];
					if (warnings.length > 0 && !confirm(`${warnings.join("; ")}. Apply anyway?`)) {
						return;
					}
					onPick({
						source: "rtsp",
						sourcertsp: {
							protocol: "tcp",
							mainStream:
								sel.main === undefined ? "" : injectCreds(sel.main.rtspUrl),
							subStream:
								sel.sub === undefined ? "" : injectCreds(sel.sub.rtspUrl),
						},
						name: sanitizeMonitorName(cam.name, cam.ip),
					});
					modal.close();
				};
				item.append(pickBtn);
			}
			listElem.append(item);
		}
	};

	scanBtn.onclick = async () => {
		statusElem.textContent = "scanning…";
		listElem.replaceChildren();
		const { status, body } = await fetchPostJson(
			"/api/scan",
			{
				host: hostField.value(),
				username: userField.value(),
				password: passField.value(),
			},
			token,
		);
		if (status === 409) {
			statusElem.textContent = "a scan is already in progress — retry shortly";
			return;
		}
		if (status !== 200 || body === null) {
			statusElem.textContent = `scan failed (${status})`;
			return;
		}
		renderResults(body);
	};

	return {
		elem: modal.elem,
		open() {
			if (!modal.elem.isConnected) {
				document.body.append(modal.elem);
			}
			modal.open();
		},
		set onPick(f) {
			onPick = f;
		},
	};
}

/**
 * @param {string} token
 * @param {Fields<any>} fields
 * @param {() => string} getMonitorId
 * @param {Monitors} monitors
 * @returns {Category}
 */
function newMonitor(token, fields, getMonitorId, monitors) {
	const title = "Monitors";

	const form = newForm(fields);

	const onSave = () => {
		saveMonitor(form);
	};
	form.addButton("save", onSave);

	const onDelete = () => {
		if (confirm("delete monitor?")) {
			deleteMonitor(form.fields.id.value());
		}
	};
	const scanModal = newScanModal(token);
	form.addButton("scan", () => {
		scanModal.open();
	});
	scanModal.onPick = (prefill) => {
		// patch, not set — the prefill carries only name+streams;
		// set() would clear untouched fields like the monitor id.
		form.patch(prefill);
	};

	form.addButton("delete", onDelete);

	/** @param {HTMLElement} subNavElem */
	const onNav = (subNavElem) => {
		const monitorId = fields["id"].elems[0];
		const monitorIdInput = monitorId.querySelector("input");

		/** @type {Monitor} */
		let monitor = {};
		let title;

		// @ts-ignore
		if (subNavElem.attributes.data === undefined) {
			// Add button.
			monitor["id"] = randomString(5);
			title = "Add";
			monitorIdInput.disabled = false;
		} else {
			// @ts-ignore
			const id = subNavElem.attributes.data.value;
			monitor = monitors[id];
			title = monitor.name;
			monitorIdInput.disabled = true;
		}

		form.set(monitor);
		return title;
	};
	const category = newCategory(title, "assets/icons/feather/video.svg", form, onNav);

	const load = async () => {
		// Update global `montiors` object so the groups category has updated values.
		const m = await fetchGet(
			new URL(relativePathname("api/monitors")),
			"could not fetch monitors",
		);
		for (const key in monitors) {
			delete monitors[key];
		}
		for (const key in m) {
			monitors[key] = m[key];
		}

		const elems = [];
		for (const m of sortByName(monitors)) {
			elems.push(newCategoryNav(m.id, m.name));
		}
		elems.push(htmlToElem(addBtnHTML));

		category.setNav(elems);
	};
	category.setOnOpen(load);

	/** @param {Form} form */
	const saveMonitor = async (form) => {
		const err = form.validate();
		if (err !== undefined) {
			alert(`invalid form: ${err}`);
			return;
		}

		const id = getMonitorId();
		const monitor = monitors[id] || {};
		for (const key of Object.keys(form.fields)) {
			monitor[key] = form.fields[key].value();
		}

		const ok = await fetchPut(
			new URL(relativePathname("api/monitor")),
			monitor,
			token,
			"failed to save monitor",
		);
		if (!ok) {
			return;
		}

		const pathname = relativePathname("api/monitor/restart");
		const params = new URLSearchParams({ id });
		fetchPost(
			new URL(`${pathname}?${params}`),
			monitor,
			token,
			"failed to restart monitor",
		);

		category.closeSubcategory();
		load();
	};

	/** @param {string} monitorID */
	const deleteMonitor = async (monitorID) => {
		const pathname = relativePathname("api/monitor");
		const params = new URLSearchParams({ id: monitorID });
		const ok = await fetchDelete(
			new URL(`${pathname}?${params}`),
			token,
			"failed to delete monitor",
		);
		if (!ok) {
			return;
		}

		load();
	};

	return category;
}

/**
 * @typedef {import("./libs/common.js").MonitorGroup} MonitorGroup
 * @typedef {import("./libs/common.js").MonitorGroups} MonitorGroups
 */

/**
 * @param {string} token
 * @param {Fields<any>} fields
 * @param {MonitorGroups} groups
 * @returns {Category}
 */
function newMonitorGroups(token, fields, groups) {
	const title = "Groups";

	const form = newForm(fields);

	const saveGroup = async () => {
		const err = form.validate();
		if (err !== undefined) {
			alert(`invalid form: ${err}`);
			return;
		}

		const id = form.fields.id.value();
		if (groups[id] === undefined) {
			// @ts-ignore
			groups[id] = {};
		}
		/** @typedef {MonitorGroup} */
		const group = groups[id];
		for (const key of Object.keys(form.fields)) {
			group[key] = form.fields[key].value();
		}

		const ok = await fetchPut(
			new URL(relativePathname("api/monitor-groups")),
			groups,
			token,
			"failed to save monitor groups",
		);
		if (!ok) {
			return;
		}

		category.closeSubcategory();
		load();
	};
	form.addButton("save", () => {
		saveGroup();
	});

	/** @param {string} groupId */
	const deleteGroup = async (groupId) => {
		delete groups[groupId];

		const ok = await fetchPut(
			new URL(relativePathname("api/monitor-groups")),
			groups,
			token,
			"failed to save monitor groups",
		);
		if (!ok) {
			return;
		}

		category.closeSubcategory();
		load();
	};

	form.addButton("delete", () => {
		if (confirm("delete group?")) {
			deleteGroup(form.fields.id.value());
		}
	});

	/** @param {HTMLElement} subNavElem */
	const onNav = (subNavElem) => {
		form.reset();
		/** @typedef {MonitorGroup} */
		let group = {};
		let title;

		// @ts-ignore
		if (subNavElem.attributes.data === undefined) {
			// New group.
			group["id"] = randomString(5);
			title = "Add";
		} else {
			// @ts-ignore
			const id = subNavElem.attributes.data.value;
			group = groups[id];
			title = group.name;
		}

		// Set fields.
		for (const key of Object.keys(form.fields)) {
			form.fields[key].set(group[key], group);
		}

		return title;
	};
	const category = newCategory(title, "assets/icons/feather/group.svg", form, onNav);

	const load = () => {
		const elems = [];
		for (const g of sortByName(groups)) {
			elems.push(newCategoryNav(g.id, g.name));
		}
		elems.push(htmlToElem(addBtnHTML));

		category.setNav(elems);
	};

	category.setOnOpen(load);
	return category;
}

/**
 * @typedef Account
 * @property {string} id
 * @property {string} username
 * @property {boolean} isAdmin
 */

/** @typedef {{[x: string]: Account}} Accounts */

/**
 * @typedef AccountFields
 * @property {Field<string>} id
 * @property {Field<string>} username
 * @property {Field<boolean>} isAdmin
 * @property {Field<string>} password
 */

/**
 * @param {string} token
 * @param {Fields<any>} fields
 * @returns {Category}
 */
function newAccount(token, fields) {
	const title = "Accounts";

	const form = newForm(fields);

	const onSave = () => {
		saveAccount(form);
	};
	form.addButton("save", onSave);

	const onDelete = () => {
		if (confirm("delete account?")) {
			deleteAccount(form.fields.id.value());
		}
	};
	form.addButton("delete", onDelete);

	let accounts;

	/** @param {HTMLElement} subNavElem */
	const onNav = (subNavElem) => {
		let id, username, isAdmin, title;
		// @ts-ignore
		if (subNavElem.attributes.data === undefined) {
			// New account.
			id = randomString(16);
			title = "Add";
			username = "";
			isAdmin = false;
		} else {
			// @ts-ignore
			id = subNavElem.attributes.data.value;
			username = accounts[id]["username"];
			isAdmin = accounts[id]["isAdmin"];
			title = username;
		}

		form.reset();
		/** @type {AccountFields} */
		const formFields = form.fields;
		formFields.id.set(id);
		formFields.username.set(username);
		formFields.isAdmin.set(isAdmin);

		return title;
	};
	const category = newCategory(title, "assets/icons/feather/users.svg", form, onNav);

	/**
	 * @param {Accounts} accounts
	 * @returns {Account[]}
	 */
	function sortByUsername(accounts) {
		const accounts2 = Object.values(accounts);
		accounts2.sort((a, b) => {
			if (a["username"] > b["username"]) {
				return 1;
			}
			return -1;
		});
		return accounts2;
	}

	const load = async () => {
		accounts = await fetchGet(
			new URL(relativePathname("api/accounts")),
			"failed to get accounts",
		);

		const elems = [];
		for (const u of sortByUsername(accounts)) {
			const c = u.isAdmin === true ? "text-red" : "text-color";
			elems.push(newCategoryNav(u.id, u.username, c));
		}
		elems.push(htmlToElem(addBtnHTML));

		category.setNav(elems);
	};
	category.setOnOpen(load);

	/** @param {Form} form */
	const saveAccount = async (form) => {
		const err = form.validate();
		if (err !== undefined) {
			alert(`invalid form: ${err}`);
			return;
		}
		const account = {
			id: form.fields.id.value(),
			username: form.fields.username.value(),
			isAdmin: form.fields.isAdmin.value(),
			plainPassword: form.fields.password.value(),
		};

		const ok = await fetchPut(
			new URL(relativePathname("api/account")),
			removeEmptyValues(account),
			token,
			"failed to save account",
		);
		if (!ok) {
			return;
		}

		category.closeSubcategory();
		load();
	};

	/** @param {string} accountID */
	const deleteAccount = async (accountID) => {
		const pathname = relativePathname("api/account");
		const params = new URLSearchParams({ id: accountID });
		const ok = await fetchDelete(
			new URL(`${pathname}?${params}`),
			token,
			"failed to delete account",
		);
		if (!ok) {
			return;
		}

		category.closeSubcategory();
		load();
	};

	return category;
}

/** @param {number} length */
function randomString(length) {
	const charSet = "234565789abcdefghjkmnpqrstuvwxyz";
	let output = "";
	for (let i = 0; i < length; i++) {
		output += charSet.charAt(Math.floor(Math.random() * charSet.length));
	}
	return output;
}

/**
 * @param {{[x: string]: { id: string, name: string} }} monitors
 * @returns {Field<string[]>}
 */
function newSelectMonitorField(monitors) {
	/**
	 * @typedef Checkbox
	 * @property {Element} elem
	 * @property {() => boolean} value
	 */

	/**
	 * @param {string} name
	 * @param {boolean} checked
	 * @return {Checkbox}
	 */
	const newCheckbox = (name, checked) => {
		/** @type {HTMLInputElement} */
		// @ts-ignore
		const $input = htmlToElem(/* HTML */ `
			<input
				class="checkbox-checkbox w-full h-full"
				style="z-index: 1; outline: none; -moz-appearance: none; -webkit-appearance: none;"
				type="checkbox"
			/>
		`);
		$input.checked = checked;

		const elem = htmlToElem(
			/* HTML */ `
				<div
					class="monitor-selector-item relative flex items-center px-2 border border-color1"
					style="width: auto; font-size: calc(var(--scale) * 1.8rem);"
				></div>
			`,
			[
				htmlToElem(/* HTML */ `
					<span class="mr-auto pr-2 text-color" style="user-select: none;"
						>${name}</span
					>
				`),
				htmlToElem(
					/* HTML */ `
						<div
							class="flex justify-center items-center rounded-md bg-color2"
							style="width: 0.8em; height: 0.8em; user-select: none;"
						></div>
					`,
					[
						$input,
						htmlToElem(/* HTML */ `
							<div
								class="checkbox-box absolute"
								style="
									width: 0.62em;
									height: 0.62em;
									border-radius: calc(var(--scale) * 0.25rem);
								"
							></div>
						`),
						htmlToElem(/* HTML */ `
							<img
								class="checkbox-check absolute"
								style="width: 0.8em; filter: invert();"
								src="assets/icons/feather/check.svg"
							/>
						`),
					],
				),
			],
		);
		elem.addEventListener("click", (e) => {
			if (e.target instanceof HTMLInputElement) {
				return;
			}
			$input.checked = !$input.checked;
		});

		return {
			elem,
			value() {
				return $input.checked;
			},
		};
	};

	const elem = htmlToElem(
		`<li class="flex items-center p-2 border-b-2 border-color1"></li>`,
	);

	/** @type {{[x: string]: Checkbox}} */
	let checkboxes = {};

	return {
		elems: [elem],
		set(value) {
			if (value === undefined) {
				value = [];
			}

			checkboxes = {};
			const elems = [];
			const sortedMonitors = sortByName(monitors);
			for (const monitor of sortedMonitors) {
				const id = monitor["id"];
				const checked = value.includes(id);
				const field = newCheckbox(monitor.name, checked);
				elems.push(field.elem);
				checkboxes[id] = field;
			}

			elem.replaceChildren(
				htmlToElem(`<div class="monitor-selector"></div>`, elems),
			);
		},
		value() {
			const value = [];
			for (const [id, checkbox] of Object.entries(checkboxes)) {
				if (checkbox.value()) {
					value.push(id);
				}
			}
			return value;
		},
	};
}

/**
 * @typedef SourceField
 * @property {() => string} validateSource
 * @property {() => void} render
 * @property {() => void} open
 */

/**
 * @param {string[]} options
 * @param {(name: string) => Field<any>} getField
 * @returns {Field<string>}
 */
function newSourceField(options, getField) {
	const value = () => {
		return field.$input.value;
	};

	/** @type {() => SourceField} */
	const selectedSourceField = () => {
		const selectedSource = `source${value()}`;
		// @ts-ignore
		return getField(selectedSource);
	};

	const field = newRawSelectCustomField(options, "Source");
	field.$editBtn.onclick = () => {
		selectedSourceField().open();
	};

	return {
		elems: [field.elem],
		value,
		set(input) {
			field.$input.value = input === undefined ? "rtsp" : input;
			field.$error.textContent = "";
		},
		validate() {
			const err = selectedSourceField().validateSource();
			field.$error.textContent = err === undefined ? "" : err;
			return err;
		},
	};
}

/**
 * @typedef RtspFields
 * @property {Field<string>} protocol
 * @property {Field<string>} mainStream
 * @property {Field<string>} subStream
 */

/** @returns {Field<string>} */
function newSourceRTSP() {
	const fields = {
		protocol: fieldTemplate.select("Protocol", ["tcp", "udp"], "tcp"),
		mainStream: newErrorField([inputRules.notEmpty], {
			input: "text",
			label: "Main stream",
			placeholder: "rtsp://x.x.x.x/main",
			doc: "Main camera feed, full resolution. Used when recording",
		}),
		subStream: newField({
			input: "text",
			label: "Sub stream",
			placeholder: "rtsp://x.x.x.x/sub (optional)",
			doc: "If your camera support a sub stream of lower resolution. Both inputs can be viewed from the live page",
		}),
	};

	const form = newForm(fields);
	const modal = newModal("RTSP source", [form.elem()]);

	let value = {};
	const elem = document.createElement("div");

	let isRendered = false;
	const render = () => {
		if (isRendered) {
			return;
		}
		elem.append(modal.elem);

		isRendered = true;
		form.set(value);
	};

	return {
		// @ts-ignore
		open() {
			render();
			modal.open();
		},
		elems: [elem],
		value() {
			if (isRendered) {
				form.get(value);
			}
			return removeEmptyValues(value);
		},
		set(input) {
			value = input === undefined ? {} : input;
			if (isRendered) {
				form.set(value);
			}
		},
		// Validation is done through the source field.
		validateSource() {
			// Have to render form to validate.
			render();
			const err = form.validate();
			if (err !== undefined) {
				return `RTSP source: ${err}`;
			}
		},
	};
}

/**
 * @typedef PtzConfig
 * @property {boolean} enable
 * @property {string} onvifAddr
 */

/** @type {InputRule[]} */
const ptzOnvifAddrRules = [
	[/^(?!\s*$)(?!http:\/\/)/, "must be empty or an http:// URL"],
	[/^http:\/\/($|[#/?])/, "missing host"],
	[/^http:\/\/[^#/?]*@/, "credentials not allowed"],
	[/[#?]/, "query and fragment not allowed"],
];

/** @returns {Field<PtzConfig>} */
function newPtzField() {
	const fields = {
		enable: fieldTemplate.toggle("Enable PTZ", false),
		onvifAddr: newErrorField(ptzOnvifAddrRules, {
			input: "text",
			label: "ONVIF address",
			placeholder: "http://x.x.x.x/onvif/device_service (optional)",
			doc: "ONVIF device service address. Leave empty to derive it from the main stream host",
		}),
	};

	const form = newForm(fields);
	return {
		elems: [...fields.enable.elems, ...fields.onvifAddr.elems],
		value() {
			/** @type {PtzConfig} */
			const value = { enable: false, onvifAddr: "" };
			form.get(value);
			return value;
		},
		set(input) {
			form.set(input === undefined ? {} : input);
		},
		validate() {
			return form.validate();
		},
	};
}

/** @returns {Field<string>} */
function newIdField() {
	let value;
	return {
		elems: [],
		value() {
			return value;
		},
		set(input) {
			value = input;
		},
	};
}

/** @typedef {import("./libs/common.js").UiData} UiData */

/** @param {UiData} uiData */
function init(uiData) {
	if (!uiData.isAdmin) {
		return;
	}

	/** @type {Fields<any>} */
	const monitorFields = {};
	const getMonitorId = () => {
		return monitorFields.id.value();
	};
	/** @param {string} name */
	const getMonitorField = (name) => {
		return monitorFields[name];
	};

	/** @type {InputRule} */
	const maxLength24 = [/^.{25}/, "maximum length is 24 characters"];
	monitorFields.id = newErrorField(
		[inputRules.noSpaces, inputRules.notEmpty, inputRules.englishOnly, maxLength24],
		{
			input: "text",
			label: "ID",
			doc: "Monitor identifier. The monitor's recordings are tied to this ID",
		},
	);
	monitorFields.name = fieldTemplate.text(
		"Name",
		"my_monitor",
		"",
		"Arbitrary display name, can probably be any UTF8 character",
	);
	monitorFields.enable = fieldTemplate.toggle("Enable monitor", true);
	monitorFields.source = newSourceField(["rtsp"], getMonitorField);
	monitorFields.sourcertsp = newSourceRTSP();
	monitorFields.alwaysRecord = fieldTemplate.toggle("Always record", false);
	monitorFields.videoLength = fieldTemplate.number("Video length (min)", "15", 15);
	monitorFields.ptz = newPtzField();
	//timestampOffset: fieldTemplate.integer("Timestamp offset (ms)", "500", "500"),
	/* SETTINGS_LAST_MONITOR_FIELD */

	const monitor = newMonitor(
		uiData.csrfToken,
		monitorFields,
		getMonitorId,
		uiData.monitors,
	);

	/** @type {Fields<any>} */
	const monitorGroupsFields = {};
	monitorGroupsFields.id = newIdField();
	monitorGroupsFields.name = fieldTemplate.text("Name", "my_monitor_group");
	monitorGroupsFields.monitors = newSelectMonitorField(uiData.monitors);

	const group = newMonitorGroups(
		uiData.csrfToken,
		monitorGroupsFields,
		uiData.monitorGroups,
	);

	/** @type {Fields<any>} */
	const accountFields = {
		id: newIdField(),
		username: newErrorField(
			[inputRules.notEmpty, inputRules.noSpaces, inputRules.noUppercase],
			{
				input: "text",
				label: "Username",
				placeholder: "name",
			},
		),
		isAdmin: fieldTemplate.toggle("Admin"),
		password: newPasswordField(),
	};
	const account = newAccount(uiData.csrfToken, accountFields);

	document
		.querySelector(".js-content")
		.replaceChildren(...rendererCategories([monitor, group, account]));
}

export { init, newScanModal, newPtzField };
