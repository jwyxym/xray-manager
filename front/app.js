"use strict";

const $ = (selector) => document.querySelector(selector);
const storageKey = "xray-manager-admin-token";
let token = null;
try { token = sessionStorage.getItem(storageKey); } catch { /* Storage may be disabled. */ }
let currentView = "codes";
let captchaId = "";
let captchaVersion = 0;
const pages = { codes: { offset: 0, rows: [], version: 0 }, users: { offset: 0, rows: [], version: 0 } };
const errors = {
	unauthorized: "登录已失效，或用户名、密码不正确。请重新登录。",
	admin_required: "此账户没有管理员权限。",
	auth_busy: "登录请求较多，请稍后重试。",
	database_error: "数据库操作失败，请稍后重试。",
	invalid_code_parameters: "请检查数量、激活时长和兑换截止时间。",
	cannot_disable_self: "不能禁用当前管理员。",
	cannot_disable_admin: "不能禁用管理员账户。",
	user_not_found: "该用户不存在，请刷新列表。",
	username_must_be_3_to_64_ascii_letters_digits_underscore_hyphen: "用户名需为 3–64 位字母、数字、下划线或连字符。",
	captcha_invalid_or_expired: "图片验证码错误或已过期，请输入新图片中的数字。",
	verification_rate_limited: "验证码请求过于频繁，请稍后重试。",
	captcha_generation_failed: "验证码生成失败，请稍后刷新。",
};

function saveToken(value) {
	token = value;
	try {
		if (value) sessionStorage.setItem(storageKey, value);
		else sessionStorage.removeItem(storageKey);
	} catch { /* In-memory login still works. */ }
}

function showLogin(message = "") {
	saveToken(null);
	for (const page of Object.values(pages)) { page.version++; page.offset = 0; page.rows = []; }
	$("#codes-body").replaceChildren();
	$("#users-body").replaceChildren();
	$("#generated-codes").value = "";
	$("#generated").hidden = true;
	$("#dashboard").hidden = true;
	$("#login-view").hidden = false;
	$("#login-error").textContent = message;
	$("#notice").hidden = true;
	refreshCaptcha();
}

async function refreshCaptcha() {
	const version = ++captchaVersion;
	captchaId = "";
	$("#login-form").elements.captcha_code.value = "";
	$("#refresh-captcha").disabled = true;
	try {
		const result = await request("/api/auth/captcha", { auth: false });
		if (version !== captchaVersion) return;
		captchaId = result.captcha_id;
		$("#captcha-image").src = `data:image/png;base64,${result.image_base64}`;
	} catch (error) { if (version === captchaVersion) $("#login-error").textContent = error.message; }
	finally { if (version === captchaVersion) $("#refresh-captcha").disabled = false; }
}

function notice(message, isError = false) {
	const node = $("#notice");
	node.textContent = message;
	node.className = isError ? "notice error" : "notice";
	node.hidden = false;
}

async function request(path, { method = "GET", body, auth = true } = {}) {
	const requestToken = token;
	const controller = new AbortController();
	const timer = setTimeout(() => controller.abort(), 20000);
	try {
		const headers = {};
		if (auth) headers.Authorization = `Bearer ${requestToken}`;
		if (body !== undefined) headers["Content-Type"] = "application/json";
		const response = await fetch(path, { method, headers, signal: controller.signal,
			body: body === undefined ? undefined : JSON.stringify(body), cache: "no-store", credentials: "omit" });
		const raw = await response.text();
		let data;
		try { data = JSON.parse(raw); } catch { data = {}; }
		// Do not let a response from a previous login update the current page.
		if (auth && requestToken !== token) throw new Error("登录状态已变化，请重试。");
		if (!response.ok) {
			const message = errors[data.error] || `请求失败（HTTP ${response.status}），请检查输入或稍后重试。`;
			if (auth && (response.status === 401 || response.status === 403)) showLogin(message);
			throw new Error(message);
		}
		return data;
	} catch (error) {
		if (error.name === "AbortError") throw new Error("请求超时。生成操作可能已成功，请先查询激活码记录，避免重复生成。");
		if (error instanceof TypeError) throw new Error("无法连接服务端，请检查网络后重试。");
		throw error;
	} finally { clearTimeout(timer); }
}

async function busy(button, action) {
	button.disabled = true;
	try { await action(); } catch (error) { notice(error.message, true); }
	finally { button.disabled = false; }
}

function element(tag, text, className) {
	const node = document.createElement(tag);
	if (text !== undefined) node.textContent = text;
	if (className) node.className = className;
	return node;
}
function cell(...nodes) {
	const td = element("td");
	for (const node of nodes) td.append(typeof node === "string" ? document.createTextNode(node) : node);
	return td;
}
function date(timestamp, fallback = "—") {
	if (!timestamp) return fallback;
	return new Date(timestamp * 1000).toLocaleString("zh-CN", { hour12: false });
}
function duration(seconds) {
	if (seconds % 86400 === 0) return `${seconds / 86400} 天`;
	if (seconds % 3600 === 0) return `${seconds / 3600} 小时`;
	return `${seconds} 秒`;
}
async function copy(text) {
	try { await navigator.clipboard.writeText(text); notice("已复制到剪贴板。"); }
	catch {
		$("#generated").hidden = false;
		$("#generated-codes").value = text;
		$("#generated-summary").textContent = "待复制内容";
		$("#generated-codes").focus();
		$("#generated-codes").select();
		notice("浏览器不允许自动复制，已选中内容，请手动复制。");
	}
}

function renderCodes(rows) {
	$("#codes-count").textContent = rows.length;
	$("#codes-available").textContent = rows.filter((r) => !r.expired).length;
	$("#codes-used").textContent = rows.filter((r) => r.used_at !== null).length;
	for (const row of rows) {
		const status = row.used_at !== null ? ["已兑换", ""] : row.expired ? ["已过期", "bad"] : ["可兑换", "good"];
		const button = element("button", "复制", "text-button");
		button.type = "button";
		button.addEventListener("click", () => copy(row.code));
		const tr = element("tr");
		tr.append(cell(element("code", row.code)), cell(duration(row.duration_seconds)),
			cell(element("span", status[0], `badge ${status[1]}`)), cell(date(row.expires_at, "不限期")),
			cell(element("code", row.used_by || "—"), element("small", date(row.used_at))), cell(button));
		$("#codes-body").append(tr);
	}
}

function renderUsers(rows) {
	$("#users-count").textContent = `本页 ${rows.length} 位用户`;
	for (const row of rows) {
		const action = cell("—");
		if (!row.is_admin) {
			const button = element("button", row.disabled ? "恢复账户" : "禁用账户", `text-button ${row.disabled ? "" : "danger"}`);
			button.type = "button";
			button.addEventListener("click", () => busy(button, async () => {
				if (!row.disabled && !confirm(`确定禁用 ${row.username}？该用户将被退出登录，代理账户进入清理队列。`)) return;
				await request(`/api/admin/users/${encodeURIComponent(row.id)}/disabled`, { method: "POST", body: { disabled: !row.disabled } });
				notice(row.disabled ? "账户已恢复，用户可以重新登录。" : "账户已禁用，相关代理账户将由后台清理。");
				await loadPage("users");
			}));
			action.replaceChildren(button);
		}
		const tr = element("tr");
		tr.append(cell(element("strong", row.username), element("small", row.id)),
			cell(element("span", row.is_admin ? "管理员" : "普通用户", "badge")),
			cell(element("span", row.disabled ? "已禁用" : "正常", `badge ${row.disabled ? "bad" : "good"}`)),
			cell(date(row.active_until, "未激活"), element("small", row.active ? "激活有效" : "未激活或已到期")),
			cell(date(row.created_at)), action);
		$("#users-body").append(tr);
	}
}

function tableMessage(kind, message) {
	const td = cell(message);
	td.colSpan = 6;
	td.className = "empty";
	const tr = element("tr");
	tr.append(td);
	$(`#${kind}-body`).replaceChildren(tr);
}

async function loadPage(kind, offset = pages[kind].offset) {
	const page = pages[kind];
	const version = ++page.version;
	const buttons = [...document.querySelectorAll(`[data-page^="${kind}:"]`)];
	buttons.forEach((button) => { button.disabled = true; });
	tableMessage(kind, "正在加载…");
	try {
		const path = kind === "codes" ? "activation-codes" : "users";
		const data = await request(`/api/admin/${path}?offset=${offset}`);
		if (version !== page.version || !token) return;
		page.rows = data.items;
		page.offset = offset;
		$(`#${kind}-body`).replaceChildren();
		if (kind === "codes") renderCodes(page.rows); else renderUsers(page.rows);
		if (!page.rows.length) tableMessage(kind, offset ? "这一页没有更多记录，可以返回上一页。" : "暂无记录");
		$(`#${kind}-page`).textContent = `第 ${offset / 100 + 1} 页 · 本页 ${page.rows.length} 条`;
	} catch (error) {
		if (version !== page.version) return;
		tableMessage(kind, "加载失败，请点击刷新列表重试。");
		notice(error.message, true);
	} finally {
		if (version === page.version) {
			buttons[0].disabled = page.offset === 0;
			buttons[1].disabled = page.rows.length < 100;
		}
	}
}

async function selectView(kind) {
	currentView = kind;
	$("#codes-view").hidden = kind !== "codes";
	$("#users-view").hidden = kind !== "users";
	const title = kind === "codes" ? "激活码管理" : "用户管理";
	$("#page-title").textContent = title;
	$("#breadcrumb").textContent = title;
	$("#page-description").textContent = kind === "codes" ? "创建激活码，查看兑换状态与使用记录。" : "查看账户与激活状态，管理用户访问权限。";
	document.querySelectorAll("[data-view]").forEach((button) => {
		button.classList.toggle("active", button.dataset.view === kind);
		if (button.dataset.view === kind) button.setAttribute("aria-current", "page");
		else button.removeAttribute("aria-current");
	});
	await loadPage(kind);
}

async function enter(user) {
	$("#admin-name").textContent = user.username;
	$("#login-view").hidden = true;
	$("#dashboard").hidden = false;
	$("#notice").hidden = true;
	await selectView("codes");
}

$("#login-form").addEventListener("submit", async (event) => {
	event.preventDefault();
	const form = event.currentTarget;
	const button = form.querySelector("button[type=submit]");
	button.disabled = true;
	$("#login-error").textContent = "";
	try {
		const fields = new FormData(form);
		if (!captchaId) throw new Error("请先获取图片验证码。");
		const submittedCaptcha = captchaId;
		captchaId = "";
		const result = await request("/api/auth/login", { method: "POST", auth: false,
			body: { username: fields.get("username"), password: fields.get("password"),
				captcha_id: submittedCaptcha, captcha_code: fields.get("captcha_code") } });
		form.elements.password.value = "";
		if (!result.user.is_admin) {
			// Revoke the token created for an ordinary account; never save it in browser storage.
			await fetch("/api/auth/logout", { method: "POST", headers: { Authorization: `Bearer ${result.token}` }, signal: AbortSignal.timeout(5000) }).catch(() => {});
			throw new Error("此账户不是管理员，无法进入管理控制台。");
		}
		saveToken(result.token);
		await enter(result.user);
	} catch (error) { $("#login-error").textContent = error.message; }
	finally {
		button.disabled = false;
		if (!$("#login-view").hidden) await refreshCaptcha();
	}
});

$("#logout").addEventListener("click", () => busy($("#logout"), async () => {
	let message = "已退出登录。";
	try { await request("/api/auth/logout", { method: "POST" }); }
	catch { message = "本地已退出；服务端撤销未确认，旧 Token 将在到期后失效。"; }
	showLogin(message);
}));

$("#generate-form").addEventListener("submit", (event) => {
	event.preventDefault();
	const form = event.currentTarget;
	busy(form.querySelector("button[type=submit]"), async () => {
		const fields = new FormData(form);
		const seconds = Number(fields.get("duration")) * Number(fields.get("unit"));
		const count = Number(fields.get("count"));
		const expires = fields.get("expires") ? Math.floor(new Date(fields.get("expires")).getTime() / 1000) : null;
		if (!Number.isSafeInteger(seconds) || seconds < 1 || seconds > 315360000) throw new Error("激活时长必须为 1–315360000 秒。");
		if (!Number.isInteger(count) || count < 1 || count > 100) throw new Error("每次可生成 1–100 个激活码。");
		if (expires !== null && (!Number.isFinite(expires) || expires <= Date.now() / 1000)) throw new Error("兑换截止时间必须晚于当前时间。");
		const result = await request("/api/admin/activation-codes", { method: "POST", body: { duration_seconds: seconds, count, expires_at: expires } });
		$("#generated-codes").value = result.codes.join("\n");
		$("#generated").hidden = false;
		$("#generated-summary").textContent = `已生成 ${result.codes.length} 个激活码，每个增加 ${duration(result.duration_seconds)}。`;
		notice("激活码生成成功。");
		await loadPage("codes", 0);
	});
});

$("#copy-generated").addEventListener("click", () => copy($("#generated-codes").value));
$("#refresh").addEventListener("click", () => busy($("#refresh"), () => loadPage(currentView)));
document.querySelectorAll("[data-view]").forEach((button) => button.addEventListener("click", () => selectView(button.dataset.view)));
document.querySelectorAll("[data-page]").forEach((button) => button.addEventListener("click", () => {
	const [kind, delta] = button.dataset.page.split(":");
	loadPage(kind, Math.max(0, pages[kind].offset + Number(delta) * 100));
}));

async function restore() {
	if (!token) { await refreshCaptcha(); return; }
	const button = $("#login-form button[type=submit]");
	button.disabled = true;
	try {
		const user = await request("/api/me");
		if (!user.is_admin) { showLogin("此账户没有管理员权限。"); return; }
		await enter(user);
	} catch (error) { showLogin(error.message); }
	finally { button.disabled = false; }
}
$("#refresh-captcha").addEventListener("click", refreshCaptcha);
restore();
