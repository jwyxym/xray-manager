use axum::{Router, http::{HeaderValue, header}, response::Html, routing::get};
use tower_http::set_header::SetResponseHeaderLayer;

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
	Router::new()
		.route("/admin", get(index))
		.route("/admin/", get(index))
		.route("/admin/style.css", get(|| async {
			([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../front/style.css"))
		}))
		.route("/admin/app.js", get(|| async {
			([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../front/app.js"))
		}))
		.layer(SetResponseHeaderLayer::overriding(header::CONTENT_SECURITY_POLICY,
			HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")))
		.layer(SetResponseHeaderLayer::overriding(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
}

async fn index() -> Html<&'static str> { Html(include_str!("../front/index.html")) }
