extern crate futures;
extern crate futures_timer;
extern crate hyper;
extern crate rand;
extern crate url;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_derive;
extern crate serde;
extern crate toml;

use futures::{future, sync};
use futures::{Async, Poll};
use futures_timer::Delay;
use hyper::body::Payload;
use hyper::header::{self, HeaderValue};
use hyper::rt::{Future, Stream};
use hyper::service::service_fn;
use hyper::{Body, Chunk, HeaderMap, Method, Request, Response, Server, StatusCode, Uri};
use rand::prelude::*;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::iter;
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_bind")]
    bind: String,

    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,

    #[serde(default = "default_timeout_scan_interval_secs")]
    timeout_scan_interval_secs: u64,

    #[serde(default = "default_download_retry_ms")]
    download_retry_ms: u64,

    #[serde(default = "default_download_max_retries")]
    download_max_retries: u64,

    #[serde(default = "default_token_length")]
    token_length: usize,

    #[serde(default = "default_max_content_length")]
    max_content_length: u64,
}

fn default_bind() -> String {
    String::from("127.0.0.1:3000")
}
fn default_timeout_secs() -> u64 {
    60 * 60
}
fn default_timeout_scan_interval_secs() -> u64 {
    60
}
fn default_download_retry_ms() -> u64 {
    200
}
fn default_download_max_retries() -> u64 {
    9
}
fn default_token_length() -> usize {
    10
}
fn default_max_content_length() -> u64 {
    1024 * 1024
}

lazy_static! {
    static ref CONFIG: Arc<Config> = Arc::new({
        let mut config_string = String::new();
        let args: Vec<OsString> = env::args_os().collect();
        if args.len() >= 2 {
            let mut config_file = match File::open(&args[1]) {
                Err(e) => {
                    eprintln!("Error opening configuration file: {}", e);
                    process::exit(1);
                }
                Ok(file) => file,
            };

            match config_file.read_to_string(&mut config_string) {
                Err(e) => {
                    eprintln!("Error reading configuration file: {}", e);
                    process::exit(1);
                }
                _ => {}
            };
        }
        match toml::from_str(&config_string) {
            Err(e) => {
                eprintln!("Error parsing configuration file: {}", e);
                process::exit(1);
            }
            Ok(c) => c,
        }
    });
}

static TYPE_TEXT: &'static str = "text/plain; charset=utf-8";
static TYPE_HTML: &'static str = "text/html; charset=utf-8";

static FAVICON: &'static [u8] = include_bytes!("favicon.ico");
static UPLOADER_HTML: &'static str = include_str!("uploader.html");
static CLIENT_JS: &'static str = include_str!("client.js");

static BASE58: &'static [char] = &[
    '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'J', 'K',
    'L', 'M', 'N', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'a', 'b', 'c', 'd', 'e',
    'f', 'g', 'h', 'i', 'j', 'k', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y',
    'z',
];

#[cfg_attr(debug_assertions, derive(Debug))]
enum RendezvousPayload {
    Bod(Body),
    Fwd(Forwarder),
}

use RendezvousPayload::{Bod, Fwd};

impl Payload for RendezvousPayload {
    type Data = Chunk;
    type Error = hyper::Error;

    fn poll_data(&mut self) -> Poll<Option<Chunk>, hyper::Error> {
        match self {
            Fwd(f) => f.poll_data(),
            Bod(b) => b.poll_data(),
        }
    }
    fn poll_trailers(&mut self) -> Poll<Option<HeaderMap>, hyper::Error> {
        match self {
            Fwd(f) => f.poll_trailers(),
            Bod(b) => b.poll_trailers(),
        }
    }
    fn is_end_stream(&self) -> bool {
        match self {
            Fwd(f) => f.is_end_stream(),
            Bod(b) => b.is_end_stream(),
        }
    }
    fn content_length(&self) -> Option<u64> {
        match self {
            Fwd(f) => f.content_length(),
            Bod(b) => b.content_length(),
        }
    }
}

#[cfg_attr(debug_assertions, derive(Debug))]
struct Forwarder {
    length: u64,
    bytes_sent: u64,
    uploader: Option<(Body, sync::oneshot::Sender<Response<RendezvousPayload>>)>,
}

impl Forwarder {
    fn handle_last_chunk(&mut self) {
        let (_, complete) = self.uploader.take().unwrap();
        if let Err(_) = complete.send(
            Response::builder()
                .header(header::CONTENT_TYPE, TYPE_TEXT)
                .body(Bod(Body::from("Sent!")))
                .unwrap(),
        ) {
            // hit an error
            // TODO what if we can't talk back to the uploader? Should this
            // be considered an error for the downloader?
        }
    }
}

impl Payload for Forwarder {
    type Data = Chunk;
    type Error = hyper::Error;

    fn poll_data(&mut self) -> Poll<Option<Chunk>, hyper::Error> {
        let last_chunk;

        if let Some((ref mut body, _)) = self.uploader {
            match body.poll() {
                Ok(Async::Ready(None)) => {
                    // TODO hit EOF, this shouldn't happen anymore
                    return Ok(Async::Ready(None));
                }
                Ok(Async::Ready(Some(chunk))) => {
                    self.bytes_sent += chunk.len() as u64;

                    if self.bytes_sent < self.length {
                        return Ok(Async::Ready(Some(chunk)));
                    } else if self.bytes_sent > self.length {
                        // TODO report an error? (to uploader and downloader)
                        // But I can't produce a hyper::Error, and it's too late to send an HTTP
                        // error (?).
                        // Hopefully sending more than Content-Length will cause an error in the
                        // browser.
                        last_chunk = chunk;
                    } else {
                        // fall through to handle_last_chunk below
                        last_chunk = chunk;
                    }
                }
                x => return x,
            }
        } else {
            // fused
            return Ok(Async::Ready(None));
        }

        self.handle_last_chunk();
        Ok(Async::Ready(Some(last_chunk)))
    }

    fn is_end_stream(&self) -> bool {
        if let Some((ref body, _)) = self.uploader {
            body.is_end_stream()
        } else {
            true
        }
    }

    fn content_length(&self) -> Option<u64> {
        Some(self.length)
    }
}

#[cfg_attr(debug_assertions, derive(Debug))]
struct Paste {
    secret: String,
    length: u64,
    expiration: Instant,
    uploaders: VecDeque<Forwarder>,
}

type BoxFut = Box<Future<Item = Response<RendezvousPayload>, Error = hyper::Error> + Send>;
// We usually don't care about Ok vs Err here, Err just lets us exit early with ?
type BoxFutRes = Result<BoxFut, BoxFut>;

type InFlightMap = Arc<Mutex<HashMap<String, Paste>>>;

macro_rules! std_response {
    ($t:expr, $s:expr) => {{
        let mut response = Response::builder();
        response.header(header::CONTENT_TYPE, HeaderValue::from_static($t));
        Box::new(future::ok(response.body(Bod(Body::from($s))).unwrap()))
    }};
}

macro_rules! status_response {
    ($status:expr, $t:expr, $s:expr) => {{
        let mut response = Response::builder();
        response.header(header::CONTENT_TYPE, HeaderValue::from_static($t));
        response.status($status);
        Box::new(future::ok(response.body(Bod(Body::from($s))).unwrap()))
    }};
}

fn service_home() -> BoxFutRes {
    Ok(std_response!(TYPE_HTML, UPLOADER_HTML))
}

fn service_favicon() -> BoxFutRes {
    Ok(std_response!("image/x-icon", FAVICON))
}

fn service_js() -> BoxFutRes {
    Ok(std_response!(
        "application/javascript; charset=utf-8",
        CLIENT_JS
    ))
}

fn generate_id_pair() -> (String, String) {
    let gen = || {
        let mut rng = thread_rng();
        iter::repeat_with(|| rng.choose(BASE58).unwrap())
            .take(CONFIG.token_length)
            .collect::<String>()
    };
    (gen(), gen())
}

fn query_id(uri: &Uri, only: bool) -> Result<String, BoxFut> {
    let mut id = None;

    if let Some(s) = uri.query() {
        for (k, v) in url::form_urlencoded::parse(s.as_ref()) {
            match k.as_ref() {
                "id" => id = Some(v.into_owned()),
                _ if only => {
                    return Err(status_response!(
                        StatusCode::BAD_REQUEST,
                        TYPE_HTML,
                        "<b>Supported arguments id \"id\"</b>"
                    ));
                }
                _ => {}
            }
        }
    };

    if id.is_none() {
        return Err(status_response!(
            StatusCode::BAD_REQUEST,
            TYPE_HTML,
            "<b>Id missing</b>"
        ));
    }

    Ok(id.unwrap())
}

fn query_id_and_secret(uri: &Uri, only: bool) -> Result<(String, String), BoxFut> {
    let mut id = None;
    let mut secret = None;

    if let Some(s) = uri.query() {
        for (k, v) in url::form_urlencoded::parse(s.as_ref()) {
            match k.as_ref() {
                "id" => id = Some(v.into_owned()),
                "secret" => secret = Some(v.into_owned()),
                _ if only => {
                    return Err(status_response!(
                        StatusCode::BAD_REQUEST,
                        TYPE_TEXT,
                        "Supported arguments are \"id\" and \"secret\""
                    ));
                }
                _ => {}
            }
        }
    };

    let id = if let Some(t) = id {
        t
    } else {
        return Err(status_response!(
            StatusCode::BAD_REQUEST,
            TYPE_TEXT,
            "Missing id"
        ));
    };
    let secret = if let Some(s) = secret {
        s
    } else {
        return Err(status_response!(
            StatusCode::BAD_REQUEST,
            TYPE_TEXT,
            "Missing secret"
        ));
    };

    Ok((id, secret))
}

fn query_length(uri: &Uri, only: bool) -> Result<u64, BoxFut> {
    let mut length = None;

    if let Some(s) = uri.query() {
        for (k, v) in url::form_urlencoded::parse(s.as_ref()) {
            match k.as_ref() {
                "length" => length = Some(v),
                _ if only => {
                    return Err(status_response!(
                        StatusCode::BAD_REQUEST,
                        TYPE_TEXT,
                        "Supported argument is \"length\""
                    ));
                }
                _ => {}
            }
        }
    }

    let length = if let Some(l) = length {
        l
    } else {
        return Err(status_response!(
            StatusCode::BAD_REQUEST,
            TYPE_TEXT,
            "Expected argument \"length\""
        ));
    };
    let length = if let Ok(l) = u64::from_str_radix(&length, 10) {
        l
    } else {
        return Err(status_response!(
            StatusCode::BAD_REQUEST,
            TYPE_TEXT,
            "\"length\" should be a decimal integer"
        ));
    };
    if length > CONFIG.max_content_length {
        return Err(status_response!(
            StatusCode::BAD_REQUEST,
            TYPE_TEXT,
            "Content is too long"
        ));
    }

    Ok(length)
}

fn service_request_id(uri: &Uri, in_flight: &InFlightMap) -> BoxFutRes {
    let length = query_length(uri, true)?;

    loop {
        let (id, secret) = generate_id_pair();

        let combo = id.clone() + "," + &secret;
        match in_flight.lock().unwrap().entry(id) {
            Entry::Occupied(_) => {
                continue;
            }
            Entry::Vacant(entry) => {
                entry.insert(Paste {
                    secret,
                    length,
                    expiration: Instant::now() + Duration::from_secs(CONFIG.timeout_secs),
                    uploaders: VecDeque::new(),
                });
                return Ok(std_response!(TYPE_TEXT, combo));
            }
        }
    }
}

fn service_retire_id(uri: &Uri, in_flight: &InFlightMap) -> BoxFutRes {
    let (id, secret) = query_id_and_secret(uri, true)?;

    match in_flight.lock().unwrap().entry(id) {
        Entry::Occupied(mut entry) => {
            {
                let paste = entry.get_mut();

                if paste.secret != secret {
                    return Err(status_response!(
                        StatusCode::FORBIDDEN,
                        TYPE_TEXT,
                        "Bad secret"
                    ));
                }
            }
            entry.remove_entry();
            return Ok(std_response!(TYPE_TEXT, "Removed"));
        }
        Entry::Vacant(_) => {
            return Err(status_response!(
                StatusCode::NOT_FOUND,
                TYPE_TEXT,
                "Unknown id"
            ));
        }
    };
}

fn service_upload(req: Request<Body>, in_flight: &InFlightMap) -> BoxFutRes {
    let (header, body) = req.into_parts();

    let (id, secret) = query_id_and_secret(&header.uri, true)?;

    let length = if let Some(length) = body.content_length() {
        if length > CONFIG.max_content_length {
            return Err(status_response!(
                StatusCode::PAYLOAD_TOO_LARGE,
                TYPE_TEXT,
                "Content is too long"
            ));
        }
        length
    } else {
        return Err(status_response!(
            StatusCode::LENGTH_REQUIRED,
            TYPE_TEXT,
            "Content-Length must be speicfied"
        ));
    };

    let (complete, completion) = sync::oneshot::channel();

    match in_flight.lock().unwrap().entry(id) {
        Entry::Occupied(mut entry) => {
            let paste = entry.get_mut();
            if paste.secret != secret {
                return Err(status_response!(
                    StatusCode::FORBIDDEN,
                    TYPE_TEXT,
                    "Bad secret"
                ));
            }
            if paste.length != length {
                return Err(status_response!(
                    StatusCode::BAD_REQUEST,
                    TYPE_TEXT,
                    "Wrong length"
                ));
            }

            // TODO not sure if we really want someone to be able to
            // queue up many uploads, actually...
            // Probably needs at least an upper limit.
            paste.uploaders.push_back(Forwarder {
                length: paste.length,
                bytes_sent: 0,
                uploader: Some((body, complete)),
            });
        }
        Entry::Vacant(_) => {
            return Err(status_response!(
                StatusCode::NOT_FOUND,
                TYPE_TEXT,
                "Unknown id"
            ));
        }
    };

    // TODO technically we'd want this to be an Err when this fails somehow
    Ok(Box::new(
        completion
            .or_else(|_| {
                future::ok(
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::CONTENT_TYPE, TYPE_TEXT)
                        .body(Bod(Body::from("BAD NEWS")))
                        .unwrap(),
                )
            })
            .map_err(|_: sync::oneshot::Canceled| unreachable!()),
    ))
}

fn service_download(uri: &Uri, in_flight: &InFlightMap) -> BoxFutRes {
    let id = query_id(uri, true)?;

    // TODO refactor for less ugliness
    fn download(id: String, mut retries: u64, in_flight: InFlightMap) -> BoxFut {
        match in_flight.lock().unwrap().entry(id.clone()) {
            Entry::Occupied(mut entry) => {
                let paste = entry.get_mut();
                match paste.uploaders.pop_front() {
                    Some(forwarder) => {
                        if forwarder.uploader.is_some()
                            && !forwarder.uploader.as_ref().unwrap().1.is_canceled()
                        {
                            paste.expiration =
                                Instant::now() + Duration::from_secs(CONFIG.timeout_secs);
                            return Box::new(future::ok(
                                Response::builder()
                                    .header(header::CONTENT_TYPE, TYPE_TEXT)
                                    .body(Fwd(forwarder))
                                    .unwrap(),
                            ));
                        } else {
                            // fall through to retry
                        }
                    }
                    None => {
                        // fall through to retry
                    }
                }
            }
            Entry::Vacant(_) => {
                return status_response!(StatusCode::NOT_FOUND, TYPE_HTML, "<b>Unknown id</b>");
            }
        };

        Box::new(
            Delay::new(Duration::from_millis(CONFIG.download_retry_ms))
            .or_else(|_| future::ok(())) // TODO probably should not retry on timer errors?
            .and_then(move |_| {
                if retries > 0 {
                    retries -= 1;
                    download(id, retries, in_flight)
                } else {
                    status_response!(
                            StatusCode::SERVICE_UNAVAILABLE,
                            TYPE_HTML,
                            "<b>No uploader currently available</b>"
                            )
                }
            }
            ),
        )
    };

    Ok(download(id, CONFIG.download_max_retries, in_flight.clone()))
}

fn service_not_found() -> BoxFutRes {
    let mut response = Response::builder();

    response.header(header::CONTENT_TYPE, HeaderValue::from_static(TYPE_HTML));
    response.status(StatusCode::NOT_FOUND);

    Err(Box::new(future::ok(
        response
            .body(Bod(Body::from(r"<b>404 Not Found</b>")))
            .unwrap(),
    )))
}

fn service_dump(_in_flight: &InFlightMap) -> BoxFutRes {
    #[cfg(debug_assertions)]
    println!("{:?}", _in_flight);
    service_not_found()
}

fn service(in_flight: InFlightMap) -> impl Fn(Request<Body>) -> BoxFut {
    move |req| {
        let result = match (req.method(), req.uri().path()) {
            // user-facing
            (&Method::GET, "/") => service_home(),
            (&Method::GET, "/favicon.ico") => service_favicon(),
            (&Method::GET, "/client.js") => service_js(),

            // API v1
            (&Method::POST, "/1/id/request") => service_request_id(req.uri(), &in_flight),
            (&Method::POST, "/1/id/retire") => service_retire_id(req.uri(), &in_flight),
            (&Method::POST, "/1/file/upload") => service_upload(req, &in_flight),
            (&Method::GET, "/1/file/download") => service_download(req.uri(), &in_flight),

            // debug
            (&Method::GET, "/dump") => service_dump(&in_flight),

            // everything else
            _ => service_not_found(),
        };

        match result {
            Ok(r) => r,
            Err(r) => r,
        }
    }
}

fn schedule_timeout(in_flight: InFlightMap) {
    hyper::rt::spawn(
        Delay::new(Duration::from_secs(CONFIG.timeout_scan_interval_secs))
            .or_else(|_| future::ok(()))
            .and_then(move |_| {
                process_timeout(&in_flight);
                schedule_timeout(in_flight);
                future::ok(())
            }),
    );
}

fn process_timeout(in_flight: &InFlightMap) {
    let now = Instant::now();

    // TODO some way of reporting time left to client
    in_flight.lock().unwrap().retain(|_, v| v.expiration > now);
}

fn main() {
    lazy_static::initialize(&CONFIG);

    let addr = CONFIG.bind.parse().unwrap();

    let in_flight = Arc::new(Mutex::new(HashMap::new()));

    let server_clone = in_flight.clone();
    let http_server = Server::bind(&addr)
        .serve(move || service_fn(service(server_clone.clone())))
        .map_err(|e| eprintln!("server error: {}", e));

    let timeout_clone = in_flight.clone();
    let timeout_kickoff = future::lazy(move || future::ok(schedule_timeout(timeout_clone.clone())));

    let server = Future::join(http_server, timeout_kickoff).map(|_| ());

    hyper::rt::run(server);
}
