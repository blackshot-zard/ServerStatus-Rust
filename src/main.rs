// #![deny(warnings)]

#[macro_use]
extern crate log;

extern crate pretty_env_logger;
use bytes::Buf;
use clap::Parser;
use http_auth_basic::Credentials;
use once_cell::sync::OnceCell;
use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

mod config;
mod notifier;
mod payload;
mod stats;

use hyper::service::{make_service_fn, service_fn};
use hyper::{header, Body, Method, Request, Response, Server, StatusCode};
type GenericError = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, GenericError>;

static APP_VERSION: &'static str = concat!(
    "v",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("GIT_HASH"),
    ") - BUILD_TS:",
    env!("BUILD_ST")
);
static G_CONFIG: OnceCell<crate::config::Config> = OnceCell::new();
static NOTFOUND: &[u8] = b"Not Found";
static UNAUTHORIZED: &[u8] = b"Unauthorized";

#[derive(RustEmbed)]
#[folder = "web"]
#[prefix = "/"]
struct Asset;

async fn stats_report(
    req: Request<Body>,
    stats_mgr: &Arc<stats::StatsMgr>,
) -> Result<Response<Body>> {
    // auth
    let mut auth_ok = false;
    if let Some(auth) = req.headers().get(hyper::header::AUTHORIZATION) {
        let auth_header_value = String::from(auth.to_str()?);
        if let Ok(credentials) = Credentials::from_header(auth_header_value) {
            if G_CONFIG
                .get()
                .unwrap()
                .auth(&credentials.user_id, &credentials.password)
            {
                auth_ok = true;
            }
        }
    }
    if !auth_ok {
        return Ok(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(UNAUTHORIZED.into())
            .unwrap());
    }
    // auth end

    let mut buffer = Vec::new();
    let whole_body = hyper::body::aggregate(req).await?;
    let json_size = whole_body.reader().read_to_end(&mut buffer)?;
    let json_data = String::from_utf8(buffer).unwrap();

    // report
    stats_mgr.report(&json_data).unwrap();

    let mut resp = HashMap::new();
    resp.insert(&"code", serde_json::Value::from(0 as i32));
    resp.insert(&"size", serde_json::Value::from(json_size));
    let resp_str = serde_json::to_string(&resp)?;

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp_str))?;
    Ok(response)
}

async fn get_stats_json(stats_mgr: &Arc<stats::StatsMgr>) -> Result<Response<Body>> {
    let res = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(stats_mgr.get_stats_json()))
        .unwrap();
    Ok(res)
}

#[allow(unused)]
async fn proc_admin_cmd(
    req: Request<Body>,
    stats_mgr: &Arc<stats::StatsMgr>,
) -> Result<Response<Body>> {
    // TODO
    return Ok(Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(UNAUTHORIZED.into())
        .unwrap());
}

async fn main_service_func(
    req: Request<Body>,
    stats_mgr: Arc<stats::StatsMgr>,
) -> Result<Response<Body>> {
    let req_path = req.uri().path();
    match (req.method(), req_path) {
        (&Method::POST, "/report") => stats_report(req, &stats_mgr).await,
        (&Method::GET, "/json/stats.json") => get_stats_json(&stats_mgr).await,
        (&Method::POST, "/admin") => proc_admin_cmd(req, &stats_mgr).await,
        (&Method::GET, "/") | (&Method::GET, "/index.html") => {
            let body = Body::from(Asset::get("/index.html").unwrap().data);
            Ok(Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(Body::from(body))
                .unwrap())
        }
        _ => {
            match req.method() {
                &Method::GET => {
                    if req_path.starts_with("/js/")
                        | req_path.starts_with("/css/")
                        | req_path.starts_with("/img/")
                    {
                        if let Some(data) = Asset::get(&req_path) {
                            let ct = mime_guess::from_path(req_path);
                            let resp = Response::builder()
                                .header(header::CONTENT_TYPE, ct.first_raw().unwrap())
                                .body(Body::from(data.data))
                                .unwrap();
                            return Ok(resp);
                        } else {
                            error!("can't get => {:?}", req_path);
                        }
                    }
                }
                _ => {}
            }

            // Return 404 not found response.
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(NOTFOUND.into())
                .unwrap());
        }
    }
}

async fn shutdown_signal() {
    // Wait for the CTRL+C signal
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}

#[derive(Parser, Debug)]
#[clap(author, version = APP_VERSION, about, long_about = None)]
struct Args {
    #[clap(short, long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();
    let args = Args::parse();

    let cfg = crate::config::parse_config(&args.config);
    debug!("{:?}", cfg);
    G_CONFIG.set(cfg).unwrap();

    let mut stats_mgr_ = stats::StatsMgr::new();
    stats_mgr_.init(G_CONFIG.get().unwrap()).unwrap();
    let stats_mgr = Arc::new(stats_mgr_);

    let addr = G_CONFIG.get().unwrap().addr.parse().unwrap();

    let http_service = make_service_fn(move |_| {
        // Move a clone into the `service_fn`.
        let stats_mgr = stats_mgr.clone();
        async {
            Ok::<_, GenericError>(service_fn(move |req| {
                // Clone again to ensure that client outlives this closure.
                main_service_func(req, stats_mgr.clone())
            }))
        }
    });

    println!("Listening on http://{}", addr);
    let server = Server::bind(&addr).serve(http_service);
    let graceful = server.with_graceful_shutdown(shutdown_signal());
    if let Err(e) = graceful.await {
        eprintln!("server error: {}", e);
    }

    Ok(())
}