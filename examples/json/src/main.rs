extern crate actix;
extern crate actix_web;
extern crate bytes;
extern crate futures;
extern crate env_logger;
extern crate serde_json;
#[macro_use] extern crate serde_derive;
#[macro_use] extern crate json;

use actix::*;
use actix_web::*;
#[cfg(target_os = "linux")] use actix::actors::signal::{ProcessSignals, Subscribe};

use bytes::BytesMut;
use futures::{Future, Stream};
use json::JsonValue;

#[derive(Debug, Serialize, Deserialize)]
struct MyObj {
    name: String,
    number: i32,
}

/// This handler uses `HttpRequest::json()` for loading serde json object.
fn index(mut req: HttpRequest) -> Box<Future<Item=HttpResponse, Error=Error>> {
    req.json()
        .from_err()  // convert all errors into `Error`
        .and_then(|val: MyObj| {
            println!("model: {:?}", val);
            Ok(httpcodes::HTTPOk.build().json(val)?)  // <- send response
        })
        .responder()
}


const MAX_SIZE: usize = 262_144;  // max payload size is 256k

/// This handler manually load request payload and parse serde json
fn index_manual(mut req: HttpRequest) -> Box<Future<Item=HttpResponse, Error=Error>> {
    // readany() returns asynchronous stream of Bytes objects
    req.payload_mut().readany()
        // `Future::from_err` acts like `?` in that it coerces the error type from
        // the future into the final error type
        .from_err()

        // `fold` will asynchronously read each chunk of the request body and
        // call supplied closure, then it resolves to result of closure
        .fold(BytesMut::new(), move |mut body, chunk| {
            // limit max size of in-memory payload
            if (body.len() + chunk.len()) > MAX_SIZE {
                Err(error::ErrorBadRequest("overflow"))
            } else {
                body.extend_from_slice(&chunk);
                Ok(body)
            }
        })
        // `Future::and_then` can be used to merge an asynchronous workflow with a
        // synchronous workflow
        .and_then(|body| {
            // body is loaded, now we can deserialize serde-json
            let obj = serde_json::from_slice::<MyObj>(&body)?;
            Ok(httpcodes::HTTPOk.build().json(obj)?) // <- send response
        })
        .responder()
}

/// This handler manually load request payload and parse json-rust
fn index_mjsonrust(mut req: HttpRequest) -> Box<Future<Item=HttpResponse, Error=Error>> {
    req.payload_mut().readany().concat2()
        .from_err()
        .and_then(|body| {
            // body is loaded, now we can deserialize json-rust
            let result = json::parse(std::str::from_utf8(&body).unwrap()); // return Result
            let injson: JsonValue = match result { Ok(v) => v, Err(e) => object!{"err" => e.to_string() } };
            Ok(HttpResponse::build(StatusCode::OK)
                .content_type("application/json")
                .body(injson.dump()).unwrap())

        })
        .responder()
}

fn main() {
    ::std::env::set_var("RUST_LOG", "actix_web=info");
    let _ = env_logger::init();
    let sys = actix::System::new("json-example");

    let addr = HttpServer::new(|| {
        Application::new()
            // enable logger
            .middleware(middleware::Logger::default())
            .resource("/manual", |r| r.method(Method::POST).f(index_manual))
            .resource("/mjsonrust", |r| r.method(Method::POST).f(index_mjsonrust))
            .resource("/", |r| r.method(Method::POST).f(index))})
        .bind("127.0.0.1:8080").unwrap()
        .shutdown_timeout(1)
        .start();

    if cfg!(target_os = "linux") { // Subscribe to unix signals
        let signals = Arbiter::system_registry().get::<ProcessSignals>();
        signals.send(Subscribe(addr.subscriber()));
    }

    println!("Started http server: 127.0.0.1:8080");
    let _ = sys.run();
}
