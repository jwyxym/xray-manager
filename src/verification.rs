use std::{collections::HashMap, io::Cursor, net::{IpAddr, SocketAddr}, sync::Arc, time::Duration};
use axum::{extract::{ConnectInfo, State}, http::{HeaderMap, StatusCode}, Json};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{Rgb, RgbImage, ImageFormat};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, message::{Mailbox, header::{ContentType, ContentTransferEncoding}}, transport::smtp::authentication::Credentials};
use rand::{Rng, rngs::OsRng};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;
use crate::{AppState, auth::*, config::SmtpConfig, entities::user_emails};

const CAPACITY: usize = 4096;
struct Challenge { digest: String, expires: i64, attempts: u8, ready: bool }
#[derive(Default)]
struct Store {
	images: HashMap<String, Challenge>,
	emails: HashMap<String, Challenge>,
	rates: HashMap<String, (i64, u32)>,
}
impl Store {
	fn prune(&mut self) {
		let timestamp = now();
		self.images.retain(|_, v| v.expires > timestamp);
		self.emails.retain(|_, v| v.expires > timestamp);
		self.rates.retain(|_, v| v.0 > timestamp);
	}
	fn limit(&mut self, key: String, seconds: i64, max: u32) -> ApiResult<()> {
		self.prune();
		if !self.rates.contains_key(&key) && self.rates.len() >= CAPACITY { return Err(rate_limited()); }
		let rate = self.rates.entry(key).or_insert((now() + seconds, 0));
		if rate.1 >= max { return Err(rate_limited()); }
		rate.1 += 1;
		Ok(())
	}
}

pub struct Verification {
	store: Mutex<Store>,
	mailer: Option<AsyncSmtpTransport<Tokio1Executor>>,
	from: Option<Mailbox>,
	mail_slots: Semaphore,
	image_slots: Semaphore,
	secret: String,
}

impl Verification {
	pub fn new(config: Option<&SmtpConfig>) -> anyhow::Result<Self> {
		let (mailer, from) = if let Some(config) = config {
			if let Some(title) = &config.title {
				anyhow::ensure!(!title.trim().is_empty() && !title.contains(['\r', '\n']), "smtp.title must be a nonempty single line");
			}
			anyhow::ensure!(!config.host.trim().is_empty() && config.port > 0 && !config.username.is_empty() && !config.password.is_empty(), "invalid [smtp] configuration");
			let from: Mailbox = config.from.parse().map_err(|_| anyhow::anyhow!("invalid smtp.from address"))?;
			// relay uses implicit TLS from the first byte, with certificate validation.
			let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)?
				.port(config.port).timeout(Some(Duration::from_secs(15)))
				.credentials(Credentials::new(config.username.clone(), config.password.clone())).build();
			(Some(mailer), Some(from))
		} else { (None, None) };
		Ok(Self { store: Mutex::new(Store::default()), mailer, from, mail_slots: Semaphore::new(4),
			image_slots: Semaphore::new(4), secret: Uuid::new_v4().to_string() })
	}
	fn digest(&self, key: &str, code: &str) -> String { token_hash(&format!("{}:{key}:{code}", self.secret)) }
	pub async fn consume_image(&self, id: &str, answer: &str) -> ApiResult<()> {
		let mut store = self.store.lock().await;
		store.prune();
		let challenge = store.images.remove(id).ok_or_else(|| bad("captcha_invalid_or_expired"))?;
		if !equal(&challenge.digest, &self.digest(id, answer)) { return Err(bad("captcha_invalid_or_expired")); }
		Ok(())
	}
	pub async fn consume_email(&self, purpose: &str, email: &str, user: &str, answer: &str) -> ApiResult<()> {
		let key = format!("{purpose}:{email}:{user}");
		let mut store = self.store.lock().await;
		store.prune();
		let challenge = store.emails.get_mut(&key).ok_or_else(|| bad("email_code_invalid_or_expired"))?;
		if !challenge.ready { return Err(bad("email_code_invalid_or_expired")); }
		if !equal(&challenge.digest, &self.digest(&key, answer)) {
			challenge.attempts += 1;
			if challenge.attempts >= 5 { store.emails.remove(&key); }
			return Err(bad("email_code_invalid_or_expired"));
		}
		store.emails.remove(&key);
		Ok(())
	}
	async fn consume_change_pair(&self, old: &str, new: &str, scope: &str, old_code: &str, new_code: &str) -> ApiResult<()> {
		let keys = [format!("change_email_old:{old}:{scope}"), format!("change_email_new:{new}:{scope}")];
		let answers = [old_code, new_code];
		let mut store = self.store.lock().await;
		store.prune();
		let mut valid = true;
		for (key, answer) in keys.iter().zip(answers) {
			if let Some(challenge) = store.emails.get_mut(key) {
				if !challenge.ready { valid = false; continue; }
				if !equal(&challenge.digest, &self.digest(key, answer)) {
					valid = false;
					challenge.attempts += 1;
					if challenge.attempts >= 5 { store.emails.remove(key); }
				}
			} else { valid = false; }
		}
		if !valid { return Err(bad("email_code_invalid_or_expired")); }
		for key in &keys { store.emails.remove(key); }
		Ok(())
	}
}

fn change_scope(user: &str, old: &str, new: &str) -> String {
	token_hash(&json!([user, old, new]).to_string())
}

fn equal(a: &str, b: &str) -> bool {
	a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}
fn rate_limited() -> ApiError { ApiError(StatusCode::TOO_MANY_REQUESTS, "verification_rate_limited") }
fn digits() -> String { (0..6).map(|_| char::from(b'0' + OsRng.gen_range(0..10))).collect() }
pub fn normalize_email(value: &str) -> ApiResult<String> {
	let email = value.trim().to_ascii_lowercase();
	if email.len() > 254 || !email.is_ascii() || email.contains(['<', '>', ' ', '\r', '\n'])
		|| email.parse::<lettre::Address>().is_err() { return Err(bad("invalid_email")); }
	Ok(email)
}
pub async fn bound_email(s: &AppState, user: &str) -> ApiResult<String> {
	user_emails::Entity::find().filter(user_emails::Column::UserId.eq(user)).one(&s.db).await?
		.map(|row| row.email).ok_or_else(|| bad("email_not_bound"))
}

pub async fn captcha(State(s): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> ApiResult<Json<Value>> {
	let _slot = s.verification.image_slots.try_acquire().map_err(|_| rate_limited())?;
	{
		let mut store = s.verification.store.lock().await;
		store.limit(format!("image:{}", peer.ip()), 60, 30)?;
		if store.images.len() >= CAPACITY { return Err(rate_limited()); }
	}
	let answer = digits();
	let copy = answer.clone();
	let png = tokio::task::spawn_blocking(move || render_image(&copy)).await
		.map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "captcha_generation_failed"))?
		.map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "captcha_generation_failed"))?;
	let id = Uuid::new_v4().to_string();
	let expires = now() + 120;
	s.verification.store.lock().await.images.insert(id.clone(), Challenge {
		digest: s.verification.digest(&id, &answer), expires, attempts: 0, ready: true,
	});
	Ok(Json(json!({"captcha_id":id,"image_base64":STANDARD.encode(png),"mime_type":"image/png","expires_at":expires})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailRequest {
	purpose: String,
	email: Option<String>,
	captcha_id: String,
	captcha_code: String,
}

pub async fn send_email(State(s): State<Arc<AppState>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(input): Json<EmailRequest>) -> ApiResult<Json<Value>> {
	let verify = &s.verification;
	let mailer = verify.mailer.as_ref().ok_or(ApiError(StatusCode::SERVICE_UNAVAILABLE, "smtp_not_configured"))?;
	let _slot = verify.mail_slots.try_acquire().map_err(|_| rate_limited())?;
	let (email, user) = match input.purpose.as_str() {
		"register" => (normalize_email(input.email.as_deref().ok_or_else(|| bad("invalid_email"))?)?, String::new()),
		"change_email_old" | "change_email_new" => {
			let user = authenticate(&s, &headers).await?;
			let old = bound_email(&s, &user.id).await?;
			let new = normalize_email(input.email.as_deref().ok_or_else(|| bad("invalid_email"))?)?;
			if old == new { return Err(bad("email_unchanged")); }
			if user_emails::Entity::find_by_id(&new).one(&s.db).await?.is_some() { return Err(conflict("email_taken")); }
			let scope = change_scope(&user.id, &old, &new);
			(if input.purpose == "change_email_old" { old } else { new }, scope)
		}
		"change_password" => {
			let user = authenticate(&s, &headers).await?;
			if input.email.is_some() { return Err(bad("password_email_must_use_bound_address")); }
			(bound_email(&s, &user.id).await?, user.id)
		}
		_ => return Err(bad("invalid_verification_purpose")),
	};
	verify.consume_image(&input.captcha_id, &input.captcha_code).await?;
	if input.purpose == "register" && user_emails::Entity::find_by_id(&email).one(&s.db).await?.is_some() {
		return Err(conflict("email_taken"));
	}
	let key = format!("{}:{email}:{user}", input.purpose);
	let code = digits();
	let digest = verify.digest(&key, &code);
	let expires = now() + 600;
	{
		let mut store = verify.store.lock().await;
		reserve_send(&mut store, peer.ip(), &email)?;
		if !store.emails.contains_key(&key) && store.emails.len() >= CAPACITY { return Err(rate_limited()); }
		store.emails.insert(key.clone(), Challenge { digest: digest.clone(), expires, attempts: 0, ready: false });
	}
	let label = match input.purpose.as_str() {
		"register" => "注册账户",
		"change_email_old" => "换绑邮箱（验证原邮箱）",
		"change_email_new" => "换绑邮箱（验证新邮箱）",
		_ => "修改密码",
	};
	let smtp = s.config.smtp.as_ref().expect("configured SMTP");
	// Literal replacement, not a Rust format string: preserve other braces and all leading zeroes.
	let title = smtp.title.as_ref().map(|template| template.replace("{:?}", &code))
		.unwrap_or_else(|| format!("Xray Manager · {label}验证码"));
	let body = smtp.body.as_ref().map(|template| template.replace("{:?}", &code))
		.unwrap_or_else(|| format!("您正在{label}，验证码为：{code}\n\n验证码 10 分钟内有效，请勿向他人透露。\n如果不是您本人操作，请忽略此邮件。"));
	let message = Message::builder().from(verify.from.clone().expect("configured sender"))
		.to(email.parse().map_err(|_| bad("invalid_email"))?).subject(title)
		.header(ContentType::TEXT_PLAIN)
		.header(ContentTransferEncoding::Base64)
		.body(body)
		.map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "email_message_failed"))?;
	let result = tokio::time::timeout(Duration::from_secs(25), mailer.send(message)).await;
	let mut store = verify.store.lock().await;
	if matches!(result, Ok(Ok(_))) {
		if let Some(challenge) = store.emails.get_mut(&key).filter(|v| v.digest == digest) { challenge.ready = true; }
		Ok(Json(json!({"ok":true,"expires_at":expires,"resend_after_seconds":60})))
	} else {
		if store.emails.get(&key).is_some_and(|v| v.digest == digest) { store.emails.remove(&key); }
		tracing::warn!("SMTPS verification email delivery failed or timed out");
		Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "email_delivery_failed"))
	}
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeEmailRequest {
	password: String,
	new_email: String,
	old_email_code: String,
	new_email_code: String,
}

pub async fn change_email(State(s): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<ChangeEmailRequest>) -> ApiResult<Json<Value>> {
	let _slot = s.password_slots.try_acquire().map_err(|_| ApiError(StatusCode::TOO_MANY_REQUESTS, "auth_busy"))?;
	let user = authenticate(&s, &headers).await?;
	let email = normalize_email(&input.new_email)?;
	if input.password.len() > 128 || !verify_password(input.password, user.password_hash.clone()).await { return Err(unauthorized()); }
	let _guard = s.mutations.lock().await;
	let fresh = authenticate(&s, &headers).await?;
	if fresh.password_hash != user.password_hash { return Err(conflict("password_changed_retry")); }
	let old = bound_email(&s, &user.id).await?;
	if old == email { return Err(bad("email_unchanged")); }
	if user_emails::Entity::find_by_id(&email).one(&s.db).await?.is_some() { return Err(conflict("email_taken")); }
	let scope = change_scope(&user.id, &old, &email);
	s.verification.consume_change_pair(&old, &email, &scope, &input.old_email_code, &input.new_email_code).await?;
	let txn = s.db.begin().await?;
	let deleted = user_emails::Entity::delete_many().filter(user_emails::Column::Email.eq(&old))
		.filter(user_emails::Column::UserId.eq(&user.id)).exec(&txn).await?;
	if deleted.rows_affected != 1 { txn.rollback().await?; return Err(conflict("email_changed_retry")); }
	user_emails::ActiveModel { email: Set(email), user_id: Set(user.id.clone()), verified_at: Set(now()) }.insert(&txn).await?;
	crate::entities::tokens::Entity::delete_many().filter(crate::entities::tokens::Column::UserId.eq(&user.id)).exec(&txn).await?;
	txn.commit().await?;
	Ok(Json(json!({"ok":true,"login_required":true})))
}

fn reserve_send(store: &mut Store, ip: IpAddr, email: &str) -> ApiResult<()> {
	store.limit(format!("cooldown:{email}"), 60, 1)?;
	store.limit(format!("email:{email}"), 3600, 5)?;
	store.limit(format!("mail-ip:{ip}"), 3600, 20)?;
	store.limit("mail-global".into(), 3600, 200)?;
	Ok(())
}

fn render_image(code: &str) -> Result<Vec<u8>, image::ImageError> {
	// Pixel glyphs, per-digit offsets/shear and noise; no answer in SVG/text metadata.
	const GLYPHS: [[u8; 7]; 10] = [
		[14,17,19,21,25,17,14], [4,12,4,4,4,4,14], [14,17,1,2,4,8,31],
		[30,1,1,14,1,1,30], [2,6,10,18,31,2,2], [31,16,16,30,1,1,30],
		[14,16,16,30,17,17,14], [31,1,2,4,8,8,8], [14,17,17,14,17,17,14], [14,17,17,15,1,1,14],
	];
	let mut rng = OsRng;
	let mut image = RgbImage::from_pixel(216, 72, Rgb([245, 248, 250]));
	for _ in 0..600 {
		let x = rng.gen_range(0..216); let y = rng.gen_range(0..72);
		image.put_pixel(x, y, Rgb([rng.gen_range(150..230), rng.gen_range(160..235), rng.gen_range(170..240)]));
	}
	for (index, digit) in code.bytes().enumerate() {
		let y0: i32 = rng.gen_range(12..24);
		let shear: i32 = rng.gen_range(-3..=3);
		let color = Rgb([rng.gen_range(20..90), rng.gen_range(30..100), rng.gen_range(40..110)]);
		for y in 0..35i32 { for x in 0..25i32 {
			if GLYPHS[(digit - b'0') as usize][y as usize / 5] & (1 << (4 - x / 5)) != 0 {
				let px = 8 + index as i32 * 34 + x + shear * (y - 17) / 17;
				if (0..216).contains(&px) { image.put_pixel(px as u32, (y0 + y) as u32, color); }
			}
		} }
	}
	for _ in 0..3 {
		let base: f32 = rng.gen_range(15.0..57.0);
		let phase: f32 = rng.gen_range(0.0..6.0);
		for x in 0..216 { let y = (base + ((x as f32 / 22.0) + phase).sin() * 5.0) as u32; image.put_pixel(x, y, Rgb([130, 160, 170])); }
	}
	let mut output = Cursor::new(Vec::new());
	image.write_to(&mut output, ImageFormat::Png)?;
	Ok(output.into_inner())
}
