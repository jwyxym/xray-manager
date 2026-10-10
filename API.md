# xray-manager HTTP 接口文档

本文对应当前 `src/api.rs`、`src/auth.rs` 和 `src/lifecycle.rs` 的实现。接口示例使用 curl，命令语法适用于 Bash、Git Bash、WSL；响应中的 UUID、Token、时间戳和凭据均为示例。

## 1. 通用约定

管理员 WebUI 入口为 `/admin`（也支持 `/admin/`），例如 `http://127.0.0.1:8080/admin`。未登录时显示登录页；登录后验证 `is_admin`，所有管理请求仍由服务端校验权限。页面支持激活码生成、复制、分页查询，以及普通用户禁用和恢复。

页面源码位于 `front/index.html`、`front/style.css`、`front/app.js`，由 Rust 编译时嵌入，通过 `/admin/style.css`、`/admin/app.js` 提供，无需前端包管理器或独立 Web 服务。修改前端后需要重新编译并启动后端。登录 Token 保存在当前标签页的 sessionStorage 中；关闭标签页不等于调用服务端退出接口，主动退出请使用页面的退出按钮。

默认基础地址为 `http://127.0.0.1:8080`，实际由 `config.toml` 顶层 `listen` 决定。远程访问时换成服务端 API 地址。API 地址与 Xray 代理端口是两个不同的入口。

需要认证的请求携带：

```http
Authorization: Bearer USER_TOKEN
```

Token 为登录接口返回的 64 字符不透明字符串，不是 JWT。`Bearer ` 前缀按当前实现区分大小写。管理员接口使用管理员登录获得的 Token。

- 有 JSON 请求体的接口使用 `Content-Type: application/json`，请求体上限 16 KiB。
- 所有成功响应均为 HTTP `200`，包括注册、创建会话和生成激活码。
- 响应没有统一 `data` 外层包装。响应带有 `Cache-Control: no-store`。
- 时间戳为 Unix 秒，不是毫秒；持续时间字段也以秒为单位。
- JSON 请求体不接受未定义字段，例如注册不能携带 `is_admin`。
- 标为“用户”的接口要求有效 Token 且账号未被禁用。未激活用户仍可登录、兑换激活码和查看个人信息。
- 管理员身份不免除请求代理连接时的激活时间检查。

本文的 `USER_TOKEN`、`ADMIN_TOKEN`、`SESSION_ID`、`USER_ID` 和 `ACTIVATION_CODE` 均需替换为真实值。

## 2. 接口总览

| 方法 | 路径 | 权限 | 用途 |
|---|---|---|---|
| GET | `/health` | 无 | HTTP 存活检查 |
| GET | `/api/auth/captcha` | 无 | 获取数字图片验证码 |
| POST | `/api/auth/email-code` | 按用途 | 发送邮箱验证码 |
| POST | `/api/auth/change-email` | 用户 | 同时验证原邮箱和新邮箱后换绑 |
| POST | `/api/auth/register` | 无 | 注册普通用户 |
| POST | `/api/auth/login` | 无 | 登录并获取 Token |
| POST | `/api/auth/logout` | 用户 | 撤销当前 Token |
| POST | `/api/auth/password` | 用户 | 修改密码 |
| GET | `/api/me` | 用户 | 查询当前用户 |
| POST | `/api/activation/redeem` | 用户 | 兑换激活码 |
| POST | `/api/connections` | 已激活用户 | 创建临时代理账户 |
| GET | `/api/connections` | 用户 | 查询自己的会话 |
| POST | `/api/connections/{id}/heartbeat` | 会话所属用户 | 发送心跳 |
| DELETE | `/api/connections/{id}` | 会话所属用户 | 关闭会话、删除临时账户 |
| POST | `/api/admin/activation-codes` | 管理员 | 批量生成激活码 |
| GET | `/api/admin/activation-codes` | 管理员 | 查询激活码 |
| GET | `/api/admin/users` | 管理员 | 查询用户 |
| POST | `/api/admin/users/{id}/disabled` | 管理员 | 禁用或恢复普通用户 |

## 3. 公共对象

### 用户对象

```json
{
	"id": "a83a42e5-16c4-4507-a210-6e26bc9a063a",
	"username": "alice",
	"is_admin": false,
	"disabled": false,
	"active_until": 1800086400,
	"active": true,
	"created_at": 1800000000
}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `id` | string | 用户 UUID |
| `username` | string | 规范化为小写的用户名 |
| `is_admin` | boolean | 是否管理员 |
| `disabled` | boolean | 是否被禁用 |
| `active_until` | integer | 激活到期时间；新注册用户为 0 |
| `active` | boolean | 仅表示 `active_until > 当前时间`，不包含禁用状态判断 |
| `created_at` | integer | 注册时间 |

### 会话对象

```json
{
	"id": "a95de2ca-ae80-44a7-a68e-4a1e66e4394c",
	"status": "active",
	"created_at": 1800000000,
	"expires_at": 1800086400,
	"heartbeat_at": 1800000030,
	"seen_online": true,
	"closed_at": null,
	"close_reason": null
}
```

`status` 为 `provisioning`（正在创建）、`active`（账户可用）、`closing`（等待清理）或 `closed`（已关闭）。`active` 不保证当前存在代理连接。`seen_online` 保留用于兼容历史记录；当前版本不再采样代理在线状态，新会话保持 false，不能用它判断连接是否可用。`closed_at`、`close_reason` 在尚未赋值时为 null。

会话对象不包含临时凭据；凭据仅在创建连接成功的响应中返回一次。

## 4. 存活检查

```bash
curl http://127.0.0.1:8080/health
```

响应：`{"status":"ok"}`。此接口只检查 HTTP 服务可响应，不执行数据库或 Xray 实时健康检查。

## 5. 注册与登录

### 验证码与 SMTPS 配置

注册流程中的图片验证码仅在发送注册邮箱验证码时校验，用于防止邮件滥发。最终提交注册只需要用户名、密码、邮箱和邮箱验证码，不再携带图片验证码。登录仍需要图片验证码；修改密码仍需要图片验证码和邮箱验证码，因此发送修改密码邮件和最终提交修改密码要使用两张不同图片。图片提交后即失效，输错也需要刷新。登录不需要邮箱验证码。

在 `config.toml` 顶层配置邮件服务：

```toml
[smtp]
host = "smtp.example.com"
port = 465
username = "noreply@example.com"
password = "邮箱服务商提供的SMTP密码或授权码"
from = "Xray Manager <noreply@example.com>"
title = "Xray Manager 验证码：{:?}"
body = """
您的验证码是：{:?}
验证码 10 分钟内有效，请勿向他人透露。
如果不是您本人操作，请忽略此邮件。
"""
```

采用 SMTPS（连接建立时即 TLS），校验服务器证书，不降级为明文，也不是 587 端口的 STARTTLS。未配置 `[smtp]` 时，发送邮件返回 503；不会跳过邮箱验证。修改配置后重启生效。

`title`、`body` 为可选的自定义标题和纯文本正文，适用于所有用途的验证码邮件。标题必须是非空单行，正文可用 TOML 三引号编写多行文本。两者中每一处字面量 `{:?}` 都会替换为本次 6 位验证码，保留开头的 0，不带额外引号；其他花括号原样保留。不写占位符就原样发送该模板，请至少在标题或正文中包含一次。分别省略字段时，该部分使用原有的按用途生成的默认内容。邮件仍使用 UTF-8 和 Base64 传输编码，正文不作为 HTML 渲染。

获取图片：

```bash
curl http://127.0.0.1:8080/api/auth/captcha
```

响应形状：

```json
{
	"captcha_id": "图片验证码UUID",
	"image_base64": "PNG图片的Base64内容",
	"mime_type": "image/png",
	"expires_at": 1800000120
}
```

前端图片 `src` 使用 `data:image/png;base64,${image_base64}`。图片是带干扰的 6 位数字 PNG，可能以 0 开头；答案用字符串传输。有效期 120 秒、仅一次尝试，接口不返回答案。数字图片属于基础防自动化措施，不保证阻止 OCR。

发送注册邮件：

```bash
curl -X POST http://127.0.0.1:8080/api/auth/email-code \
	-H 'Content-Type: application/json' \
	-d '{"purpose":"register","email":"alice@example.com","captcha_id":"CAPTCHA_ID","captcha_code":"012345"}'
```

发送修改密码邮件（必须登录，只发到当前绑定邮箱，不能传 `email` 字段）：

```bash
curl -X POST http://127.0.0.1:8080/api/auth/email-code \
	-H 'Authorization: Bearer USER_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"purpose":"change_password","captcha_id":"CAPTCHA_ID","captcha_code":"012345"}'
```

发送成功返回 `{"ok":true,"expires_at":1800000600,"resend_after_seconds":60}`，不返回验证码。邮件包含 6 位数字码，有效期 10 分钟、成功验证后即消费，最多输错 5 次；重发后同一用途和绑定范围的旧码失效。代码绑定用途、邮箱；修改密码还绑定用户 ID，换绑验证码同时绑定用户 ID、原邮箱及目标新邮箱，不能跨用途、跨邮箱、跨用户使用。最终操作发生数据库等错误时，已消费的验证码也需要重新获取。

限额：图片每个直连 IP 每分钟 30 次；邮件同一邮箱 60 秒内 1 次、每小时 5 次；同一 IP 每小时 20 次、整个进程每小时 200 次。失败的发送尝试也可能占用限额。限额和未消费验证码在进程内保存，重启后需要重新获取，单进程最多保留 4096 条图片/邮件记录。不信任 `X-Forwarded-For`，反向代理部署下 IP 限额按代理地址累计。

邮箱去除首尾空格并统一小写，只接受 ASCII 邮箱地址；同一邮箱只能绑定一个用户。普通用户注册时邮箱绑定与账户创建在同一数据库事务中完成。

### 5.1 注册

```bash
curl -X POST http://127.0.0.1:8080/api/auth/register \
	-H 'Content-Type: application/json' \
	-d '{"username":"alice","password":"alice-password-123","email":"alice@example.com","email_code":"654321"}'
```

| 字段 | 类型 | 约束 |
|---|---|---|
| `username` | string | 必填，3–64 个 ASCII 字母、数字、下划线或连字符；转小写，不自动去除空格 |
| `password` | string | 必填，10–128 个 UTF-8 字节，不是字符数 |
| `email` | string | 必填，接收验证码的邮箱 |
| `email_code` | string | 必填，邮件内的 6 位数字，用途为 register |

先完成发送注册邮件的图片验证，再提交上述四个字段即可。注册接口不接受 `captcha_id`、`captcha_code`；携带这些额外字段会被请求校验拒绝。邮箱验证码仍必须已成功发送、未过期、未使用，且用途为 register、邮箱与本次注册一致。

返回用户对象，`is_admin: false`、`disabled: false`、`active_until: 0`、`active: false`。注册不自动登录，不返回 Token。

用户名重复返回 `409 username_taken`。

### 5.2 登录

```bash
curl -X POST http://127.0.0.1:8080/api/auth/login \
	-H 'Content-Type: application/json' \
	-d '{"username":"alice","password":"alice-password-123","captcha_id":"CAPTCHA_ID","captcha_code":"012345"}'
```

响应：

```json
{
	"token": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
	"token_type": "Bearer",
	"expires_at": 1800604800,
	"user": {
		"id": "a83a42e5-16c4-4507-a210-6e26bc9a063a",
		"username": "alice",
		"is_admin": false,
		"disabled": false,
		"active_until": 0,
		"active": false,
		"created_at": 1800000000
	}
}
```

`expires_at` 是当前 Token 的到期时间，由 `token_ttl_seconds` 决定。多次登录可产生多个有效 Token，不会自动撤销旧 Token。没有刷新 Token 接口，到期后需要重新登录。

管理员与普通用户都需要 `captcha_id`、`captcha_code`。每次登录尝试后刷新图片，密码输错也不能复用上一张。

账号不存在、密码错误或账号禁用返回 `401 unauthorized`。

### 5.3 退出登录

```bash
curl -X POST http://127.0.0.1:8080/api/auth/logout \
	-H 'Authorization: Bearer USER_TOKEN'
```

无需请求体。响应：`{"ok":true}`。

只撤销当前 Token，不撤销其他登录 Token，也不立即关闭代理会话。如需同时断开代理，应先调用关闭会话接口，再退出登录。已失效 Token 再次退出返回 401。

### 5.4 修改密码

```bash
curl -X POST http://127.0.0.1:8080/api/auth/password \
	-H 'Authorization: Bearer USER_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"old_password":"alice-password-123","new_password":"alice-new-password-456","email_code":"654321","captcha_id":"NEW_CAPTCHA_ID","captcha_code":"012345"}'
```

五个字段均为必填 string，新密码为 10–128 个 UTF-8 字节。先用 `purpose=change_password` 发送邮件，收到邮件后另取新图片再提交此接口。响应：`{"ok":true,"login_required":true}`。

成功后撤销该用户的全部 Token，并将未关闭的代理会话标记为待清理，后台随后删除临时账户。旧密码错误返回 401；并发修改导致密码已经变化时返回 `409 password_changed_retry`。

### 5.5 当前用户信息

```bash
curl http://127.0.0.1:8080/api/me \
	-H 'Authorization: Bearer USER_TOKEN'
```

返回用户对象，并额外包含 `email` 字段（当前绑定邮箱；未绑定为 null）。用户列表与登录响应的用户对象不增加此字段。

### 5.6 邮箱换绑（双邮箱验证）

必须已登录并已有绑定邮箱。旧账户首次绑定接口已移除，`/api/auth/bind-email` 不再提供，`purpose=bind_email` 不再支持。没有绑定邮箱时返回 `email_not_bound`，不允许绕过原邮箱验证。

第一步：取一张图片验证码，向当前绑定邮箱发送原邮箱验证码。请求中的 `email` 是**目标新邮箱**，实际收件地址由后端从当前绑定读取：

```bash
curl -X POST http://127.0.0.1:8080/api/auth/email-code \
	-H 'Authorization: Bearer USER_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"purpose":"change_email_old","email":"new@example.com","captcha_id":"CAPTCHA_ID_1","captcha_code":"012345"}'
```

第二步：取另一张图片验证码，向同一个目标新邮箱发送新邮箱验证码：

```bash
curl -X POST http://127.0.0.1:8080/api/auth/email-code \
	-H 'Authorization: Bearer USER_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"purpose":"change_email_new","email":"new@example.com","captcha_id":"CAPTCHA_ID_2","captcha_code":"123456"}'
```

前两步可以交换顺序，但必须指定相同的新邮箱。两封邮件各自返回有效期和重发等待时间，不直接返回验证码。

第三步：提交当前密码、新邮箱和两个邮箱收到的验证码，无需再次获取或提交图片验证码：

```bash
curl -X POST http://127.0.0.1:8080/api/auth/change-email \
	-H 'Authorization: Bearer USER_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"password":"alice-password-123","new_email":"new@example.com","old_email_code":"654321","new_email_code":"123456"}'
```

以上四个字段均为必填 string。图片验证码仅用于前两步发送邮件时防止滥发；最终换绑接口不接受 `captcha_id`、`captcha_code`，携带这些额外字段会被请求校验拒绝。新邮箱不能与原邮箱相同，也不能已被其他用户绑定。

只有两个邮箱验证码均有效才一起消费；其中一个错误时，正确的另一个不会被消费，错误码累计尝试次数，5 次失效。修改目标邮箱后必须获取对应的新验证码，旧目标的原邮箱验证码也不能复用。

更新邮箱与撤销该用户全部登录 Token 在同一数据库事务中完成，成功返回 `{"ok":true,"login_required":true}`，需要重新登录。不会直接关闭已有代理会话，但前端需要用新 Token 继续心跳。数据库失败时事务回滚，原绑定不变，已消费验证码需重新获取。

## 6. 兑换激活码

```bash
curl -X POST http://127.0.0.1:8080/api/activation/redeem \
	-H 'Authorization: Bearer USER_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"code":"ACTIVATION_CODE"}'
```

`code` 为必填 UUID 字符串。响应示例：`{"active_until":1800086400}`。

增加时间的规则是：

```text
新到期时间 = max(原到期时间, 当前时间) + 激活码时长
```

每个码只能兑换一次，重复兑换也不会再次延长。成功兑换会记录使用人和使用时间。同一个码并发兑换通过事务与条件更新保证单次消费。

- 格式不正确或不存在：`400 invalid_activation_code`。
- 已使用或超过兑换截止时间：`409 activation_code_used_or_expired`。
- 兑换成功会延长用户激活时间，但不会延长已经创建的连接会话的固定 `expires_at`。

## 7. 代理连接

### 7.1 创建临时账户并获取配置

```bash
curl -X POST http://127.0.0.1:8080/api/connections \
	-H 'Authorization: Bearer USER_TOKEN'
```

无需请求体。要求用户当前已激活。每个用户的未关闭会话上限由 config.toml 顶层的 `max_sessions_per_user` 配置，省略时默认为 1，必须为正整数。例如设置 `max_sessions_per_user = 3` 可同时创建 3 个会话；`provisioning`、`active` 和 `closing` 都占用名额。每个会话有独立的临时凭据、心跳和关闭接口。配置修改后需重启服务。

以下是未配置 `xray.public_address` 时的 VLESS + REALITY 响应示例，公钥与 UUID 仅示意结构：

```json
{
	"session_id": "a95de2ca-ae80-44a7-a68e-4a1e66e4394c",
	"expires_at": 1800086400,
	"heartbeat_interval_seconds": 30,
	"requires_server_address": true,
	"config": {
		"inbounds": [
			{
				"tag": "socks",
				"listen": "127.0.0.1",
				"port": 10808,
				"protocol": "socks",
				"settings": {"auth":"noauth","udp":true}
			}
		],
		"outbounds": [
			{
				"tag": "proxy",
				"protocol": "vless",
				"settings": {
					"vnext": [{
						"port": 1443,
						"users": [{
							"id": "043ac3be-9c23-46e7-943a-f9ff35a1a0a0",
							"encryption": "none",
							"flow": "xtls-rprx-vision"
						}]
					}]
				},
				"streamSettings": {
					"network": "raw",
					"security": "reality",
					"realitySettings": {
						"serverName": "www.bilibili.com",
						"fingerprint": "chrome",
						"password": "REALITY_PUBLIC_KEY",
						"shortId": "5ad0a72406c577d7",
						"spiderX": "/"
					}
				}
			}
		]
	}
}
```

| 字段 | 含义 |
|---|---|
| `session_id` | 心跳与删除使用的会话 ID；列表中的同一字段叫 `id` |
| `expires_at` | 创建时的用户激活到期时间与 `当前时间 + session_ttl_seconds` 两者较小值 |
| `heartbeat_interval_seconds` | 建议心跳间隔：`max(heartbeat_timeout_seconds / 3, 1)`，整数除法 |
| `requires_server_address` | true 表示前端必须补齐连接地址 |
| `config` | 交给前端 Xray 内核的配置；缺少地址时需要先补齐 |

如果配置了非空 `xray.public_address`，响应会带有 `address`，且 `requires_server_address` 为 false。REALITY 的 `password` 是从服务端私钥推导的公钥，不是网站登录密码，也不是临时用户 UUID。

Trojan 模式的目标配置位于 `config.outbounds[0].settings.servers[0]`，其中包含 `port`、临时 `password` 以及可选的 `address`，而不是 VLESS 的 `vnext` 和 `users`。

常见错误：未激活为 `403 activation_required`；未关闭会话数量已达上限（含等待清理）为 `409 connection_already_exists_or_cleanup_pending`；Xray 创建失败为 `503 xray_unavailable`。

创建请求失败、超时或响应丢失时，不要假定账户一定没创建。先查询自己的会话列表，关闭旧会话或等待后台清理后再请求。临时凭据无法通过列表重新读取。

### 7.2 前端补齐服务器地址

API 和 Xray 对外使用同一个主机地址时，可以使用 API URL 的 hostname。前端页面的域名不一定等于 API 域名，不应直接假定 `window.location.hostname` 就是代理地址。

```javascript
// result 是创建连接接口的响应；apiBaseUrl 是完整 API 基础 URL。
const outbound = result.config.outbounds[0];
const endpoint = outbound.settings.vnext?.[0] ?? outbound.settings.servers?.[0];

if (result.requires_server_address) {
	if (!endpoint) throw new Error("缺少代理目标配置");
	// 移除 URL.hostname 对 IPv6 地址附带的方括号。
	endpoint.address = new URL(apiBaseUrl).hostname.replace(/^\[|\]$/g, "");
}

// 然后将 result.config 交给前端 Xray 内核。
```

API 使用 CDN、反向代理或与 Xray 分开部署时，应由前端另外指定可直达 Xray 的主机地址。不要用 API 的端口替换返回的代理端口。REALITY 的 `serverName` 是 SNI，不要随 `address` 一起改动。普通 TLS 模式则需要确保 SNI/证书校验名与服务端证书相符，必要时在后端设置 `xray.tls_server_name`。

### 7.3 心跳

```bash
curl -X POST http://127.0.0.1:8080/api/connections/SESSION_ID/heartbeat \
	-H 'Authorization: Bearer USER_TOKEN'
```

无需请求体，返回更新后的会话对象。前端应在获得会话后按响应间隔定期发送。

心跳只更新 `heartbeat_at`，不延长固定的 `expires_at`，也不能复活已关闭或已超时的会话。非 active 状态、会话过期、激活过期或上一次心跳已经超时，返回 `409 session_expired_or_closed`。会话不存在或属于其他用户返回 `404 session_not_found`。

### 7.4 查询自己的会话

```bash
curl http://127.0.0.1:8080/api/connections \
	-H 'Authorization: Bearer USER_TOKEN'
```

响应为 `{"items":[会话对象]}`，没有记录时为 `{"items":[]}`。按创建时间倒序，最多 100 条，包含历史已关闭记录。当前接口不支持分页，也不返回连接配置或凭据。

### 7.5 关闭会话

```bash
curl -X DELETE http://127.0.0.1:8080/api/connections/SESSION_ID \
	-H 'Authorization: Bearer USER_TOKEN'
```

无需请求体。响应：`{"ok":true}`。同一用户重复关闭自己的已关闭会话也成功。会话不存在或属于其他用户返回 404；删除暂时失败返回 503，后台继续重试。

前端主动断开时应同时停止自己的 Xray 会话。后端删除用户会撤销后续认证，但不保证立即终止已经建立的代理连接。

## 8. 管理员接口

管理员从 `config.toml` 的顶层 `[admin]` 读取，配置后服务启动时自动创建或同步：

```toml
[admin]
username = "admin"
password = "替换成你的管理员密码"
email = "admin@example.com"
```

用户名与密码遵循普通注册的格式限制。也可以执行 `manager.exe --config config.toml create-admin` 单独同步，不启动 Xray；不再接受 `--username` 或读取密码环境变量。

`email` 可选：服务端配置被视为管理员本地可信配置，启动时直接绑定该地址；不能占用其他用户的邮箱。管理员尚未绑定邮箱时，应先通过此配置初始化。管理员本地配置同步不要求图片或邮件验证码；HTTP 登录、修改密码和邮箱换绑仍要求对应验证。通过 HTTP 换绑后，若 `[admin].email` 仍保留旧邮箱，下次启动会按配置重新绑定，应同步更新配置或移除该字段以保留数据库里的新邮箱。

同名管理员密码相同时不重复创建或撤销登录；密码不同则更新密码，撤销全部 Token 并标记代理会话待清理。同名普通用户不会自动升级，程序报错。配置中的管理员会恢复为未禁用状态。删除 `[admin]` 不会删除数据库内已有管理员；修改配置用户名会创建另一个管理员，不会删除原管理员。通过 HTTP 修改管理员密码后，若配置仍是旧密码，下次启动会按配置覆盖。

没有通过 HTTP 创建管理员或提升权限的接口。管理员使用普通登录接口取得 `ADMIN_TOKEN`。

### 8.1 生成激活码

```bash
curl -X POST http://127.0.0.1:8080/api/admin/activation-codes \
	-H 'Authorization: Bearer ADMIN_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"duration_seconds":86400,"count":2,"expires_at":null}'
```

| 字段 | 类型 | 必填 | 约束 |
|---|---|---|---|
| `duration_seconds` | integer | 是 | 1–315360000；单个码增加的激活秒数 |
| `count` | integer | 是 | 1–100；本次生成数量 |
| `expires_at` | integer/null | 否 | 兑换截止时间，设置时必须大于当前时间；省略或 null 表示不限兑换时间 |

响应：

```json
{
	"codes": [
		"879fb453-6114-469f-8e76-d2e744c71a67",
		"727c8cab-77b7-47f8-88b9-6c36c7e1cb01"
	],
	"duration_seconds": 86400,
	"expires_at": null
}
```

常用时长：1 小时 `3600`，1 天 `86400`，7 天 `604800`，30 天 `2592000`。`expires_at` 不表示用户激活到期时间；用户增加多少时间由 `duration_seconds` 决定。

### 8.2 查询激活码

```bash
curl 'http://127.0.0.1:8080/api/admin/activation-codes?offset=0' \
	-H 'Authorization: Bearer ADMIN_TOKEN'
```

`offset` 是非负整数，默认 0。每页固定最多 100 条，下一页使用 `offset=100`、`offset=200`。没有 `limit`、总数或筛选参数。

响应：

```json
{
	"items": [{
		"code": "879fb453-6114-469f-8e76-d2e744c71a67",
		"duration_seconds": 86400,
		"expires_at": null,
		"used_at": 1800000000,
		"used_by": "a83a42e5-16c4-4507-a210-6e26bc9a063a",
		"expired": true
	}]
}
```

未使用时 `used_at`、`used_by` 为 null。`expired` 表示“已使用或兑换截止时间已到”，因此即使 `expires_at` 为 null，使用后也会显示 true。按创建时间倒序、ID 升序排列。

### 8.3 查询用户

```bash
curl 'http://127.0.0.1:8080/api/admin/users?offset=0' \
	-H 'Authorization: Bearer ADMIN_TOKEN'
```

响应为 `{"items":[用户对象]}`。`offset` 规则与激活码列表相同，最多 100 条，按创建时间倒序、ID 升序排列，没有总数字段。

### 8.4 禁用或恢复用户

```bash
curl -X POST http://127.0.0.1:8080/api/admin/users/USER_ID/disabled \
	-H 'Authorization: Bearer ADMIN_TOKEN' \
	-H 'Content-Type: application/json' \
	-d '{"disabled":true}'
```

`disabled` 为必填 boolean。禁用成功响应：`{"ok":true,"cleanup_pending":true}`。

禁用会撤销该用户全部 Token，并将未关闭会话标记为待清理。`cleanup_pending` 为 true 不代表已完成清理，也不代表一定存在会话。

恢复时发送 `{"disabled":false}`，响应 `{"ok":true,"cleanup_pending":false}`。恢复不会找回旧 Token 或代理账户，用户需要重新登录；原有激活到期时间保留，不因禁用暂停计时。

目标用户不存在返回 `404 user_not_found`。不能通过此接口操作自己的禁用状态（`cannot_disable_self`）或其他管理员（`cannot_disable_admin`），包括发送 false 的情况。

## 9. 自动关闭规则

| 条件 | 行为/关闭原因 |
|---|---|
| 上次心跳距今达到 `heartbeat_timeout_seconds` | `heartbeat_timeout` |
| 会话固定有效期结束 | `session_expired` |
| 用户激活到期、被禁用或用户记录不存在 | `activation_expired_or_disabled` |
| 前端调用删除 | `client_disconnect` |
| 修改密码 / 管理员禁用 | `password_changed` / `user_disabled` |
| 创建 Xray 账户失败 | `provision_failed` |
| 后台发现未完成的创建/关闭操作，且没有已记录原因 | `incomplete_operation` |
| 服务端重启 | `server_restarted` |

后台按 `cleanup_interval_seconds` 轮询，操作失败会重试，关闭不保证瞬时完成。已记录的关闭原因通常保留；重启协调会将遗留未关闭会话的原因设置为 `server_restarted`。

连接存活只按前端心跳判断，不再因没有代理请求或在线统计为零而清理。旧配置中的 `connect_grace_seconds` 和 `offline_grace_seconds` 可保留，但不再生效。心跳只更新 `heartbeat_at`，不延长固定的 `expires_at`；激活到期、会话到期、禁用及主动关闭仍会清理代理账户。服务重启后前端需要重新请求连接。

## 10. 错误响应

业务错误格式：

```json
{"error":"activation_required"}
```

| HTTP 状态 | error | 含义 |
|---|---|---|
| 400 | `username_must_be_3_to_64_ascii_letters_digits_underscore_hyphen` | 用户名格式错误 |
| 400 | `password_must_be_10_to_128_bytes` | 新密码长度错误 |
| 400 | `password_hash_failed` | 密码处理失败 |
| 400 | `captcha_invalid_or_expired` | 图片错误、过期或已被使用，需要刷新 |
| 400 | `email_code_invalid_or_expired` | 邮箱验证码错误、过期、未发送完成、已使用或尝试次数耗尽 |
| 400 | `invalid_email` | 邮箱格式不合法或缺失 |
| 400 | `invalid_verification_purpose` | 邮件用途不支持 |
| 400 | `email_not_bound` | 当前账户没有绑定邮箱 |
| 400 | `password_email_must_use_bound_address` | 修改密码邮件不能指定收件地址 |
| 400 | `invalid_activation_code` | 激活码格式错误或不存在 |
| 400 | `invalid_code_parameters` | 生成数量、时长或兑换截止时间不合法 |
| 400 | `cannot_disable_self` | 不能通过禁用接口操作自己 |
| 400 | `cannot_disable_admin` | 不能通过禁用接口操作管理员 |
| 401 | `unauthorized` | Token 缺失/失效、账号禁用或密码错误 |
| 403 | `admin_required` | 需要管理员身份 |
| 403 | `activation_required` | 用户未激活或激活已到期 |
| 404 | `user_not_found` | 管理员操作的用户不存在 |
| 404 | `session_not_found` | 会话不存在或不属于当前用户 |
| 409 | `username_taken` | 用户名已存在 |
| 409 | `email_taken` | 邮箱已被绑定 |
| 400 | `email_unchanged` | 新邮箱与当前绑定相同 |
| 409 | `email_changed_retry` | 当前绑定发生变化，重新开始换绑流程 |
| 409 | `password_changed_retry` | 密码已被并发修改 |
| 409 | `activation_code_used_or_expired` | 激活码已使用或过期 |
| 409 | `connection_already_exists_or_cleanup_pending` | 未关闭会话数量已达 max_sessions_per_user 上限（含等待清理） |
| 409 | `session_expired_or_closed` | 心跳目标不可继续保活 |
| 429 | `auth_busy` | 密码处理并发槽位已满，稍后重试 |
| 429 | `verification_rate_limited` | 验证码限额或处理容量已满 |
| 500 | `captcha_generation_failed` | 图片生成失败 |
| 500 | `email_message_failed` | 邮件内容构建失败 |
| 500 | `database_error` | 数据库操作失败 |
| 503 | `xray_unavailable` | Xray 操作失败或关闭操作尚待清理 |
| 503 | `smtp_not_configured` | 没有配置 SMTPS |
| 503 | `email_delivery_failed` | SMTPS 发送失败或超时 |

JSON 语法错误、字段缺失/未知、类型不符、查询参数解析错误、请求体过大等由 Axum 提取器处理，可能返回 400、413、415、422 及文本响应，不保证上述 JSON 错误格式。未知路由或不支持的方法也不保证 JSON。前端应先判断 HTTP 状态和 Content-Type，再决定如何解析错误。

`auth_busy` 是密码处理并发限制；验证码接口的 `verification_rate_limited` 还包括上述邮箱/IP 限额。错误响应没有固定 `Retry-After`。

## 11. 推荐调用顺序

1. 管理员生成激活码。
2. 获取图片 → 发送注册邮箱验证码 → 使用邮件验证码注册 → 获取新图片并登录 → 兑换激活码。
3. 创建连接，保存 `session_id`，按需补齐 `config` 中的地址。
4. 启动前端 Xray，同时按响应间隔定时发送心跳。
5. 主动断开时停止前端代理、取消心跳并删除会话。
6. 需要退出登录时，再撤销 Token。

收到 401 时重新登录；收到心跳 409 时停止使用旧会话，查询状态并按需重新申请；创建连接收到 409 时先处理已有会话。重新登录本身不会延长激活时间，也不会自动创建代理账户。

### 对外连接端口

在 config.toml 的 [xray] 下可设置 public_port = 443，例如 Nginx stream 对外监听 443、转发到 Xray 监听的 1443 时，客户端配置会返回端口 443。此设置不会修改 Xray 自身的监听端口。

public_port 可省略，此时返回受管理入站的 port；显式配置须为 1–65535。public_address 仅填写域名或 IP，不带协议和端口。即使 public_address 留空、由前端补齐地址，也可以单独配置 public_port。