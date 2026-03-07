use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};

type Body = Full<Bytes>;

const HTML: &str = include_str!("../../static/app.html");

pub fn serve_app() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html; charset=utf-8")
        .header("Cache-Control", "no-store, no-cache, must-revalidate")
        .body(Full::new(Bytes::from(HTML)))
        .unwrap()
}
