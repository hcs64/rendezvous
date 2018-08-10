extern crate hyper;
extern crate futures;
extern crate rand;
extern crate url;

use std::collections::{HashMap, VecDeque};
use std::collections::hash_map::Entry;
use std::iter;
use std::sync::{Arc, Mutex};
use rand::prelude::*;
//use rand::distributions::{Distribution, Uniform};
use futures::{future, sync};
use futures::{Async, Poll};
use hyper::{Body, Chunk, HeaderMap, Method, Request, Response, Server, StatusCode};
use hyper::body::Payload;
use hyper::header::{self, HeaderValue};
use hyper::rt::{Future, Stream};
use hyper::service::service_fn;

// TODO: timeout
const _TIMEOUT_SECS: u64 = 5; //5 * 60;

const TOKEN_LENGTH: usize = 12;

static TYPE_TEXT: &'static str = "text/plain; charset=utf-8";
static TYPE_HTML: &'static str = "text/html; charset=utf-8";

static FAVICON: &'static [u8] = include_bytes!("favicon.ico");
static UPLOADER_HTML: &'static str = include_str!("uploader.html");
static CLIENT_JS: &'static str = include_str!("client.js");

static BASE58: &'static [char] =
&['1','2','3','4','5','6','7','8','9','A','B','C','D','E','F','G','H','J','K','L','M','N','P','Q','R','S','T','U','V','W','X','Y','Z','a','b','c','d','e','f','g','h','i','j','k','m','n','o','p','q','r','s','t','u','v','w','x','y','z'];

#[derive(Debug)]
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

#[derive(Debug)]
struct Forwarder {
    inner: Option<(Body, sync::oneshot::Sender<Response<Rendezvous>>)>
}

impl Payload for Forwarder {
    type Data = Chunk;
    type Error = hyper::Error;

    fn poll_data(&mut self) -> Poll<Option<Chunk>, hyper::Error> {
        if let Some((ref mut body, _)) = self.inner {
            match body.poll() {
                Ok(Async::Ready(None)) => {},
                x => return x
            }
        } else {
            // fused!
            return Ok(Async::Ready(None))
        }

        let (_, complete) = self.inner.take().unwrap();
        if let Err(_) = complete.send(
            Response::builder()
                .header(header::CONTENT_TYPE, TYPE_TEXT)
                .body(Bod(Body::from("Upload success!")))
                .unwrap()
        ) {
            // not really much to do if we can't talk back to the uploader
        }
        Ok(Async::Ready(None))
    }
    // TODO I needed to cheat with these because otherwise we wouldn't actually
    // be polled for the final EOF, but I'd like to set content length
    // all the same...
    /*
    fn is_end_stream(&self) -> bool {
        if let Some(ref body) = self.body {
            body.is_end_stream()
        } else {
            true
        }
    }
    */
    /*
    fn content_length(&self) -> Option<u64> {
        if let Some(ref body) = self.body {
            body.content_length()
        } else {
            None
        }
    }
    */
}

type BoxFut = Box<Future<Item=Response<Rendezvous>, Error=hyper::Error> + Send>;
type Endpoint = (String, VecDeque<Forwarder>);
type InFlightMap = Arc<Mutex<HashMap<String, Endpoint>>>;

macro_rules! std_response {
    ( $t:expr, $s:expr ) => {
        {
            let mut response = Response::builder();
            response.header(
                header::CONTENT_TYPE,
                HeaderValue::from_static($t));
            Box::new(future::ok(response.body(Bod(Body::from($s))).unwrap()))
        }
    }
}

macro_rules! status_response {
    ( $status:expr, $t:expr, $s:expr ) => {
        {
            let mut response = Response::builder();
            response.header(
                header::CONTENT_TYPE,
                HeaderValue::from_static($t));
            response.status($status);
            Box::new(future::ok(response.body(Bod(Body::from($s))).unwrap()))
        }
    }
}

fn service_home() -> BoxFut
{
    std_response!(TYPE_HTML, UPLOADER_HTML)
}

fn service_favicon() -> BoxFut
{
    std_response!("image/x-icon", FAVICON)
}

fn service_js() -> BoxFut
{
    std_response!("application/javascript; charset=utf-8", CLIENT_JS)
}

fn generate_token_pair() -> (String, String) {
    let gen = || {
        let mut rng = thread_rng();
        iter::repeat_with(|| rng.choose(BASE58)
                                 .unwrap())
                                 .take(TOKEN_LENGTH)
                                 .collect::<String>()
    };
    (gen(), gen())
}

fn service_request_token(in_flight: &InFlightMap) -> BoxFut
{
    let (token, secret) = generate_token_pair();
    let combo = token.clone() + "," + &secret;
    in_flight.lock()
             .unwrap()
             .insert(token, (secret, VecDeque::new()));

    std_response!(TYPE_TEXT, combo)
}

fn service_upload(req: Request<Body>, in_flight: &InFlightMap) -> BoxFut
{
    let mut token = None;
    let mut secret = None;

    let (parts, body) = req.into_parts();
    let (complete, completion) = sync::oneshot::channel();
    if let Some(s) = parts.uri.query() {
        for (k, v) in url::form_urlencoded::parse(s.as_ref()).into_owned() {
            match k.as_ref() {
                "token" => token = Some(v),
                "secret" => secret = Some(v),
                _ => {}
            }
        }
    }

    if token.is_none() {
        return status_response!(StatusCode::NOT_FOUND, TYPE_TEXT,
                                "Missing token");
    }
    if secret.is_none() {
        return status_response!(StatusCode::FORBIDDEN, TYPE_TEXT,
                                "Missing secret");
    }
    let token = token.unwrap().to_string();
    let secret = secret.unwrap();

    match in_flight.lock().unwrap().entry(token) {
        Entry::Occupied(mut entry) => {
            let endpoint = entry.get_mut();
            if endpoint.0 == secret {
                // TODO not sure if we really want someone to be able to
                // queue up many uploads, actually...
                endpoint.1.push_back(Forwarder {
                    inner: Some((body, complete)),
                });
            } else {
                return status_response!(StatusCode::FORBIDDEN, TYPE_TEXT,
                                        "Bad secret");
            }
        },
        Entry::Vacant(_) => {
            return status_response!(StatusCode::NOT_FOUND, TYPE_TEXT,
                                 "Unknown token");
        },
    };

    Box::new(completion.or_else(|_|
        future::ok(
            Response::builder()
                .header(header::CONTENT_TYPE, TYPE_TEXT)
                .body(Bod(Body::from("BAD NEWS")))
                .unwrap())
    )
    .map_err(|_: sync::oneshot::Canceled| -> hyper::Error {
        unreachable!()
    }))
}

fn service_download(req: Request<Body>, in_flight: &InFlightMap) -> BoxFut
{
    let query = req.uri().query();
    if query.is_none() {
        return status_response!(StatusCode::NOT_FOUND, TYPE_HTML,
                                r"<b>Token missing</b>");
    }

    let token = String::from(query.unwrap());
    match in_flight.lock().unwrap().entry(token) {
        Entry::Occupied(mut entry) => {
            match entry.get_mut().1.pop_front() {
                // TODO need to check if a forwarder is closed and skip
                Some(forwarder) => Box::new(future::ok(
                    Response::builder()
                        .body(Fwd(forwarder))
                        .unwrap())),
                None => status_response!(StatusCode::SERVICE_UNAVAILABLE,
                                         TYPE_HTML,
                                         "<b>No uploader available</b>"),
            }
        },
        Entry::Vacant(_) => status_response!(StatusCode::NOT_FOUND, TYPE_HTML,
                                             "<b>Unknown token</b>"),
    }
}

fn service_not_found() -> BoxFut
{
    let mut response = Response::builder();

    response.header(header::CONTENT_TYPE, HeaderValue::from_static(TYPE_HTML));
    response.status(StatusCode::NOT_FOUND);

    Box::new(future::ok(response.body(Bod(Body::from(r"<b>404 Not Found</b>"))).unwrap()))
}

fn service_dump(in_flight: &InFlightMap) -> BoxFut
{
    println!("{:?}", in_flight);
    service_not_found()
}

fn service(in_flight: InFlightMap) -> impl Fn(Request<Body>) -> BoxFut
{
    move |req| match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => service_home(),
        (&Method::GET, "/favicon.ico") => service_favicon(),
        (&Method::GET, "/client.js") => service_js(),
        (&Method::POST, "/token/request") => service_request_token(&in_flight),
        (&Method::POST, "/upload") => service_upload(req, &in_flight),
        (&Method::GET, "/download") => service_download(req, &in_flight),
        (&Method::GET, "/dump") => service_dump(&in_flight),
        _ => service_not_found(),
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
