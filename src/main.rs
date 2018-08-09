extern crate hyper;
extern crate futures;
extern crate rand;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::iter;
use std::sync::{Arc, Mutex};
use rand::{Rng, thread_rng};
use rand::distributions::Alphanumeric;
use futures::{future, sync};
use futures::{Async, Poll};
use hyper::{Body, Chunk, HeaderMap, Method, Request, Response, Server, StatusCode};
use hyper::body::Payload;
use hyper::header::{self, HeaderValue};
use hyper::rt::{Future, Stream};
use hyper::service::service_fn;

// TODO: timeout
const _TIMEOUT_SECS: u64 = 5; //5 * 60;

static TYPE_TEXT: &'static str = "text/plain; charset=utf-8";
static TYPE_HTML: &'static str = "text/html; charset=utf-8";
static _URL: &'static str = "http://localhost:3000/";
static FAVICON: &'static [u8] = include_bytes!("favicon.ico");
static UPLOADER_HTML: &'static str = include_str!("uploader.html");

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

struct Forwarder {
    body: Option<Body>,
    complete: Option<sync::oneshot::Sender<Response<Rendezvous>>>,
}

impl Payload for Forwarder {
    type Data = Chunk;
    type Error = hyper::Error;

    fn poll_data(&mut self) -> Poll<Option<Chunk>, hyper::Error> {
        if let Some(ref mut body) = self.body {
            match body.poll() {
                Ok(Async::Ready(None)) => {},
                x => return x
            }
        }
        self.body.take().unwrap().fuse();
        self.complete.take().unwrap().send(
            Response::builder()
                .header(header::CONTENT_TYPE, TYPE_TEXT)
                .body(Bod(Body::from("Upload success!")))
                .unwrap()
        );
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
type InFlightMap = Arc<Mutex<HashMap<String, Option<Forwarder>>>>;

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

fn service_home() -> BoxFut
{
    std_response!(TYPE_HTML, UPLOADER_HTML)
}

fn service_favicon() -> BoxFut
{
    std_response!("image/x-icon", FAVICON)
}

fn service_request_token(in_flight: &InFlightMap) -> BoxFut
{
    let mut rng = thread_rng();
    let token = iter::repeat(())
        .map(|()| rng.sample(Alphanumeric))
        .take(12)
        .collect::<String>();

    in_flight.lock().unwrap().insert(token.clone(), None);

    std_response!(TYPE_TEXT, token)
}

fn service_upload(req: Request<Body>, in_flight: &InFlightMap) -> BoxFut
{
    let (parts, body) = req.into_parts();
    let (complete, completion) = sync::oneshot::channel();
    let query = parts.uri.query();
    if query.is_none() {
        // TODO HTTP error codes for errors
        return std_response!(TYPE_TEXT, "Missing token");
    }
    let token = String::from(query.unwrap());

    match in_flight.lock().unwrap().entry(token.clone()) {
        Entry::Occupied(mut entry) => {
            entry.insert(Some(Forwarder {
                body: Some(body),
                complete: Some(complete),
            }))
        },
        Entry::Vacant(_) => {
            return std_response!(TYPE_TEXT, "Unknown token");
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
        // TODO consolidate error handling
        return std_response!(TYPE_HTML, r"<b>Token missing</b>");
    }

    let token = String::from(query.unwrap());
    match in_flight.lock().unwrap().remove(&token) {
        Some(Some(forwarder)) => {
            Box::new(future::ok(
                    Response::builder()
                    .body(Fwd(forwarder))
                    .unwrap()))
        },
        Some(None) =>  std_response!(TYPE_HTML, "<b>Uploader never connected for upload</b>"),
        None => std_response!(TYPE_HTML, "<b>Unknown token</b>"),
    }
}

fn service_not_found() -> BoxFut
{
    let mut response = Response::builder();

    response.header(header::CONTENT_TYPE, HeaderValue::from_static(TYPE_HTML));
    response.status(StatusCode::NOT_FOUND);

    Box::new(future::ok(response.body(Bod(Body::from(r"<b>404 Not Found</b>"))).unwrap()))
}

fn service(in_flight: InFlightMap) -> impl Fn(Request<Body>) -> BoxFut
{
    move |req| match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => service_home(),
        (&Method::GET, "/favicon.ico") => service_favicon(),
        (&Method::POST, "/requestToken") => service_request_token(&in_flight),
        (&Method::POST, "/upload") => service_upload(req, &in_flight),
        (&Method::GET, "/download") => service_download(req, &in_flight),
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
