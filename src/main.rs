extern crate hyper;
extern crate futures;
extern crate futures_timer;
extern crate rand;

use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::iter;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use rand::{Rng, thread_rng};
use rand::distributions::Alphanumeric;
use futures::{future, stream, sync};
use futures::{Async, Poll};
use futures::stream::PollFn;
use hyper::{Body, Chunk, HeaderMap, Method, Request, Response, Server, StatusCode};
use hyper::body::Payload;
use hyper::header::{self, HeaderValue};
use hyper::rt::{Future, Stream};
use hyper::service::service_fn;
use futures_timer::Delay;

// TODO: timeout doesn't work
const TIMEOUT_SECS: u64 = 5; //5 * 60;

const URL: &'static str = "http://localhost:3000/";

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
                    .header(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/html; charset=utf-8"))
                    .body(Bod(Body::from(r"GOTCHA SUCKAS")))
                    .unwrap());
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

fn service(in_flight: InFlightMap) -> impl Fn(Request<Body>) -> BoxFut 
{
    move |req: Request<Body>| -> BoxFut {
        let mut response = Response::new(Bod(Body::empty()));

        match (req.method(), req.uri().path()) {
            (&Method::GET, "/") => {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"));
                *response.body_mut() = Bod(Body::from(
r#"<!doctype html>
<html>
<body>
<textarea cols=20 rows=10 id='content'></textarea>
<br>
<button id='submit-button'>Submit</button>
<br>
<div id='link-div'></div>
<br>
<textarea cols=20 rows=5 id='log'></textarea>
<script>
'use strict';

let content = document.getElementById('content');
let submit = document.getElementById('submit-button');
let log = document.getElementById('log');
let linkdiv = document.getElementById('link-div');

let reportStatus = function (newStatus) {
    log.value = newStatus + '\n' + log.value;
};

content.value = '';
log.value = '';

submit.addEventListener('click', function () {
    let data = content.value;
    let xhr = new XMLHttpRequest();
    let token = '';
    xhr.addEventListener('load', function () {
        // TODO: validate token
        token = this.responseText;

        reportStatus('Got token ' + token);
        let link = document.createElement('a');
        link.href = 'http://localhost:3000/download?' + token;
        link.target = '_blank';
        link.innerText = token;
        while (linkdiv.firstChild) {
            linkdiv.removeChild(linkdiv.firstChild);
        }
        linkdiv.appendChild(link);

        let xhr = new XMLHttpRequest();
        xhr.addEventListener('load', function () {
            reportStatus('On it\'s way! ' + xhr.responseText);
        });
        xhr.addEventListener('error', function () {
            reportStatus('XHR error');
        });
        xhr.open('POST', '/upload?' + token, true);
        xhr.send(content.value);
        
        reportStatus('Uploading...');
    });
    xhr.addEventListener('error', function () {
        reportStatus('XHR error');
    });
    xhr.open('POST', '/requestToken', true);
    xhr.send();
    reportStatus('Requesting token');
});
</script>
</body>
</html>"#));
            },
            (&Method::POST, "/requestToken") => {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"));
                let mut rng = thread_rng();
                let token = iter::repeat(())
                    .map(|()| rng.sample(Alphanumeric))
                    .take(12)
                    .collect::<String>();
                in_flight.lock().unwrap().insert(token.clone(), None);
                *response.body_mut() = Bod(Body::from(token));
            },
            (&Method::POST, "/upload") => {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"));

                let (parts, body) = req.into_parts();
                let (complete, completion) = sync::oneshot::channel();
                *response.body_mut() = Bod(Body::from(r"Thanks bud!"));
                let query = parts.uri.query();
                if query.is_none() {
                    // TODO HTTP error code
                    *response.body_mut() = Bod(Body::from(r"<b>Missing token</b>"));
                    return Box::new(future::ok(response));
                }
                let token = String::from(query.unwrap());

                let mut entry = match in_flight.lock().unwrap().entry(token.clone()) {
                    Entry::Occupied(mut entry) => {
                        entry.insert(Some(Forwarder {
                            body: Some(body),
                            complete: Some(complete),
                        }))
                    },
                    Entry::Vacant(_) => {
                        *response.body_mut() = Bod(Body::from(r"<b>Unknown token</b>"));
                        return Box::new(future::ok(response));
                    },
                };

                return Box::new(completion.or_else(|e| {
                    future::ok(Response::new(Bod(Body::from(r"BAD NEWS"))))
                })
                .map_err(|_: sync::oneshot::Canceled| -> hyper::Error {
                    unreachable!()
                }));
            },
            (&Method::GET, "/download") => {
                let query = req.uri().query();
                if query.is_none() {
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/html; charset=utf-8"));
                    *response.body_mut() = Bod(Body::from(r"<b>Token missing</b>"));

                    // TODO consolidate error handling
                    return Box::new(future::ok(response));
                }

                let token = String::from(query.unwrap());
                match in_flight.lock().unwrap().remove(&token) {
                    Some(Some(forwarder)) => {
                        // TODO don't repeat setting headers everywhere
                        /*
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/plain; charset=utf-8"));
                        */

                        return Box::new(future::ok(Response::builder()
                            .body(Fwd(forwarder))
                            .unwrap()));
                    },
                    Some(None) => {
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/html; charset=utf-8"));
                        *response.body_mut() = Bod(Body::from(r"<b>Uploader never connected with token</b>"));
                    },
                    None => {
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/html; charset=utf-8"));
                        *response.body_mut() = Bod(Body::from(r"<b>Bad token</b>"));
                    },
                };
            },
            _ => {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"));
                *response.status_mut() = StatusCode::NOT_FOUND;
                *response.body_mut() = Bod(Body::from(r"<b>404 Not Found</b>"));
            }
        };

        Box::new(future::ok(response))
    }
}

fn main() {
    let addr = ([127, 0, 0, 1], 3000).into();

    let in_flight = Arc::new(Mutex::new(HashMap::new()));

    let server = Server::bind(&addr)
        .serve(move || service_fn(service(in_flight.clone())))
        .map_err(|e| eprintln!("server error: {}", e));

    hyper::rt::run(server);
}
