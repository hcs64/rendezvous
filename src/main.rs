extern crate futures;
extern crate hyper;
extern crate rand;
extern crate url;

use rand::prelude::*;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::iter;
use std::sync::{Arc, Mutex};
//use rand::distributions::{Distribution, Uniform};
use futures::{future, sync};
use futures::{Async, Poll};
use hyper::body::Payload;
use hyper::header::{self, HeaderValue};
use hyper::rt::{Future, Stream};
use hyper::service::service_fn;
use hyper::{Body, Chunk, HeaderMap, Method, Request, Response, Server, StatusCode, Uri};

// TODO: timeout
const _TIMEOUT_SECS: u64 = 5; //5 * 60;

const TOKEN_LENGTH: usize = 12;
const MAX_CONTENT_LENGTH: u64 = 1024 * 1024;

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
enum Rendezvous {
    Bod(Body),
    Fwd(Forwarder),
}

use Rendezvous::{Bod, Fwd};

impl Payload for Rendezvous {
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
    uploader: Option<(Body, sync::oneshot::Sender<Response<Rendezvous>>)>,
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
                },
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
                },
                x => return x
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
    uploaders: VecDeque<Forwarder>,
}

type BoxFut = Box<Future<Item = Response<Rendezvous>, Error = hyper::Error> + Send>;
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

type BoxFutRes = Result<BoxFut, BoxFut>;

fn service_home() -> BoxFutRes {
    Ok(std_response!(TYPE_HTML, UPLOADER_HTML))
}

fn service_favicon() -> BoxFutRes {
    Ok(std_response!("image/x-icon", FAVICON))
}

fn service_js() -> BoxFutRes {
    Ok(std_response!("application/javascript; charset=utf-8", CLIENT_JS))
}

fn generate_id_pair() -> (String, String) {
    let gen = || {
        let mut rng = thread_rng();
        iter::repeat_with(|| rng.choose(BASE58).unwrap())
            .take(TOKEN_LENGTH)
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
                    return Err(status_response!(StatusCode::BAD_REQUEST,
                                                TYPE_HTML,
                                                "<b>Supported arguments id \"id\"</b>"));
                }
                _ => {}
            }
        }
    };

    if id.is_none() {
        return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_HTML, "<b>Id missing</b>"));
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
                    return Err(status_response!(StatusCode::BAD_REQUEST,
                                                TYPE_TEXT,
                                                "Supported arguments are \"id\" and \"secret\""));
                }
                _ => {}
            }
        }
    };

    let id = if let Some(t) = id { t } else {
        return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_TEXT, "Missing id"));
    };
    let secret = if let Some(s) = secret { s } else {
        return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_TEXT, "Missing secret"));
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
                    return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_TEXT,
                                                "Supported argument is \"length\""));
                }
                _ => {}
            }
        }
    }

    let length = if let Some(l) = length { l } else {
        return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_TEXT,
                                    "Expected argument \"length\""));
    };
    let length = if let Ok(l) = u64::from_str_radix(&length, 10) { l } else {
        return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_TEXT,
                                    "\"length\" should be a decimal integer"));
    };
    if length > MAX_CONTENT_LENGTH {
        return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_TEXT,
                                    "Content is too long"));
    }

    Ok(length)
}

fn service_request_id(uri: &Uri, in_flight: &InFlightMap) -> BoxFutRes {
    let length = query_length(uri, true)?;

    loop {
        let (id, secret) = generate_id_pair();

        let combo = id.clone() + "," + &secret;
        match in_flight .lock().unwrap().entry(id) {
            Entry::Occupied(_) => {
                continue;
            }
            Entry::Vacant(entry) => {
                entry.insert(Paste {
                    secret,
                    length,
                    uploaders: VecDeque::new(),
                });
                return Ok(std_response!(TYPE_TEXT, combo))
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
                    return Err(status_response!(StatusCode::FORBIDDEN, TYPE_TEXT, "Bad secret"));
                }
            }
            entry.remove_entry();
            return Ok(std_response!(TYPE_TEXT, "Removed"));
        },
        Entry::Vacant(_) => {
            return Err(status_response!(StatusCode::NOT_FOUND, TYPE_TEXT, "Unknown id"));
        }
    };
}

fn service_upload(req: Request<Body>, in_flight: &InFlightMap) -> BoxFutRes {
    let (header, body) = req.into_parts();

    let (id, secret) = query_id_and_secret(&header.uri, true)?;

    let length = if let Some(length) = body.content_length() {
        if length > MAX_CONTENT_LENGTH {
            return Err(status_response!(StatusCode::PAYLOAD_TOO_LARGE,
                                        TYPE_TEXT,
                                        "Content is too long"));
        }
        length
    } else {
        return Err(status_response!(StatusCode::LENGTH_REQUIRED,
                                    TYPE_TEXT,
                                    "Content-Length must be speicfied"));
    };

    let (complete, completion) = sync::oneshot::channel();

    match in_flight.lock().unwrap().entry(id) {
        Entry::Occupied(mut entry) => {
            let paste = entry.get_mut();
            if paste.secret != secret {
                return Err(status_response!(StatusCode::FORBIDDEN, TYPE_TEXT, "Bad secret"));
            }
            if paste.length != length {
                return Err(status_response!(StatusCode::BAD_REQUEST, TYPE_TEXT, "Wrong length"));
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
            return Err(status_response!(StatusCode::NOT_FOUND, TYPE_TEXT, "Unknown id"));
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
            .map_err(|_: sync::oneshot::Canceled| unreachable!())
    ))
}

fn service_download(uri: &Uri, in_flight: &InFlightMap) -> BoxFutRes {
    let id = query_id(uri, true)?;

    match in_flight.lock().unwrap().entry(id) {
        Entry::Occupied(mut entry) => {
            match entry.get_mut().uploaders.pop_front() {
                // TODO need to check if a forwarder is closed and skip
                Some(forwarder) => Ok(Box::new(future::ok(
                    Response::builder()
                        .header(header::CONTENT_TYPE, TYPE_TEXT)
                        .body(Fwd(forwarder))
                        .unwrap()
                ))),
                None => Err(status_response!(
                    StatusCode::SERVICE_UNAVAILABLE, TYPE_HTML, "<b>No uploader available</b>"))
            }
        }
        Entry::Vacant(_) => {
            Err(status_response!(StatusCode::NOT_FOUND, TYPE_HTML, "<b>Unknown id</b>"))
        }
    }
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

fn service_dump(in_flight: &InFlightMap) -> BoxFutRes {
    #![cfg(debug_assertions)]
    {
        println!("{:?}", in_flight);
    }
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

fn main() {
    let addr = ([0, 0, 0, 0], 3000).into();

    let in_flight = Arc::new(Mutex::new(HashMap::new()));

    let server = Server::bind(&addr)
        .serve(move || service_fn(service(in_flight.clone())))
        .map_err(|e| eprintln!("server error: {}", e));

    hyper::rt::run(server);
}
