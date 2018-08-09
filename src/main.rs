extern crate hyper;
extern crate futures;
extern crate futures_timer;
extern crate rand;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::iter;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use rand::{Rng, thread_rng};
use rand::distributions::Alphanumeric;
use futures::future;
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use hyper::header::{self, HeaderValue};
use hyper::rt::{Future, Stream};
use hyper::service::service_fn;
use futures_timer::Delay;

// TODO: timeout doesn't work
const TIMEOUT_SECS: u64 = 5; //5 * 60;

type BoxFut = Box<Future<Item=Response<Body>, Error=hyper::Error> + Send>;
type InFlightMap = Arc<Mutex<HashMap<String, Option<Body>>>>;

fn rendezvous(in_flight: InFlightMap) -> impl Fn(Request<Body>) -> BoxFut {
    move |req: Request<Body>| -> BoxFut {
        let mut response = Response::new(Body::empty());

        match (req.method(), req.uri().path()) {
            (&Method::GET, "/") => {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"));
                *response.body_mut() = Body::from(
r#"<!doctype html>
<html>
<body>
<textarea cols=20 rows=10 id='content'></textarea>
<br>
<button id='submit-button'>Submit</button>
<br>
<textarea cols=20 rows=5 id='log'></textarea>
<script>
'use strict';


let content = document.getElementById('content');
let submit = document.getElementById('submit-button');
let log = document.getElementById('log');

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
</html>"#);
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
                *response.body_mut() = Body::from(token);
            },
            (&Method::POST, "/upload") => {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"));

                let (parts, body) = req.into_parts();
                let query = parts.uri.query();
                if query.is_none() {
                    // TODO HTTP error code
                    *response.body_mut() = Body::from(r"<b>Missing token</b>");
                    return Box::new(future::ok(response));
                }
                let token = String::from(query.unwrap());

                let (mut sender, mut download_body) = Body::channel();

                match in_flight.lock().unwrap().entry(token.clone()) {
                    Entry::Occupied(mut o) => o.insert(Some(download_body)),
                    Entry::Vacant(_) => {
                        *response.body_mut() = Body::from(r"<b>Unknown token</b>");
                        return Box::new(future::ok(response));
                    },
                };

                *response.body_mut() = Body::from(r"Some error");

                // TODO need a way of timing out the token even without an upload
                let upload_started = Instant::now();
                let mut aborted = false;
                let mut in_flight = in_flight.clone();
                let mut sender = Some(sender);
                let forwarder = body.for_each(move |data| {
                    if aborted {
                        sender.take();
                        return Ok(());
                    }
                    match sender {
                        // TODO need some way of creating an error here, or need to find a better
                        // approach than for_each
                        None => Ok(()),
                        Some(ref mut sender) => {
                            // TODO: really want a way of killing the stream if we abort below
                            let timer = Delay::new_at(upload_started + Duration::from_secs(TIMEOUT_SECS));
                            {
                                let poll_result = future::poll_fn(|| sender.poll_ready())
                                    .select2(timer)
                                    .wait();

                                match poll_result {
                                    Ok(future::Either::A(_)) => {},
                                    Ok(future::Either::B(_)) => {
                                        eprintln!("{} timed out", &token);
                                        // TODO test abort
                                        in_flight.lock().unwrap().remove(&token);
                                        aborted = true;
                                        return Ok(());
                                    },
                                    Err(_) => {
                                        eprintln!("{} hit an error", &token);
                                        in_flight.lock().unwrap().remove(&token);
                                        aborted = true;
                                        return Ok(());
                                    },
                                }
                            }

                            if sender.send_data(data).is_err() {
                                eprintln!("{} unexpectedly wasn't able to accept the chunk", &token);
                                // TODO consolidate cleanup
                                in_flight.lock().unwrap().remove(&token);
                                aborted = true;
                                return Ok(());
                            }

                            Ok(())
                        }
                    }
                }).map(|_| {
                    *response.body_mut() = Body::from(r"OK!");
                    response
                });
                
                return Box::new(forwarder);
            },
            (&Method::GET, "/download") => {
                let query = req.uri().query();
                if query.is_none() {
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/html; charset=utf-8"));
                    *response.body_mut() = Body::from(r"<b>Token missing</b>");

                    // TODO consolidate error handling
                    return Box::new(future::ok(response));
                }

                let token = String::from(query.unwrap());
                match in_flight.lock().unwrap().remove(&token) {
                    Some(Some(body)) => {
                        // TODO don't repeat setting headers everywhere
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/plain; charset=utf-8"));
                        *response.body_mut() = body;
                    },
                    Some(None) => {
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/html; charset=utf-8"));
                        *response.body_mut() = Body::from(r"<b>Uploader never connected with token</b>");
                    },
                    None => {
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/html; charset=utf-8"));
                        *response.body_mut() = Body::from(r"<b>Bad token</b>");
                    },
                };
            },
            _ => {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"));
                *response.status_mut() = StatusCode::NOT_FOUND;
                *response.body_mut() = Body::from(r"<b>404 Not Found</b>");
            }
        };

        Box::new(future::ok(response))
    }
}

fn main() {
    let addr = ([127, 0, 0, 1], 3000).into();

    let in_flight = Arc::new(Mutex::new(HashMap::new()));

    let server = Server::bind(&addr)
        .serve(move || service_fn(rendezvous(in_flight.clone())))
        .map_err(|e| eprintln!("server error: {}", e));

    hyper::rt::run(server);
}
