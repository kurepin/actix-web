#![allow(unused_variables)]
extern crate actix;
extern crate actix_web;
extern crate env_logger;
extern crate futures;

use actix::*;
use actix_web::*;
#[cfg(target_os = "linux")] use actix::actors::signal::{ProcessSignals, Subscribe};

use futures::{Future, Stream};
use futures::future::{result, Either};


fn index(mut req: HttpRequest) -> Box<Future<Item=HttpResponse, Error=Error>>
{
    println!("{:?}", req);

    req.multipart()            // <- get multipart stream for current request
        .from_err()            // <- convert multipart errors
        .and_then(|item| {     // <- iterate over multipart items
            match item {
                // Handle multipart Field
                multipart::MultipartItem::Field(field) => {
                    println!("==== FIELD ==== {:?}", field);

                    // Field in turn is stream of *Bytes* object
                    Either::A(
                        field.map_err(Error::from)
                            .map(|chunk| {
                                println!("-- CHUNK: \n{}",
                                         std::str::from_utf8(&chunk).unwrap());})
                            .finish())
                },
                multipart::MultipartItem::Nested(mp) => {
                    // Or item could be nested Multipart stream
                    Either::B(result(Ok(())))
                }
            }
        })
        .finish()  // <- Stream::finish() combinator from actix
        .map(|_| httpcodes::HTTPOk.response())
        .responder()
}

fn main() {
    ::std::env::set_var("RUST_LOG", "actix_web=info");
    let _ = env_logger::init();
    let sys = actix::System::new("multipart-example");

    let addr = HttpServer::new(
        || Application::new()
            .middleware(middleware::Logger::default()) // <- logger
            .resource("/multipart", |r| r.method(Method::POST).a(index)))
        .bind("127.0.0.1:8080").unwrap()
        .start();

    if cfg!(target_os = "linux") { // Subscribe to unix signals
        let signals = Arbiter::system_registry().get::<ProcessSignals>();
        signals.send(Subscribe(addr.subscriber()));
    }

    println!("Starting http server: 127.0.0.1:8080");
    let _ = sys.run();
}
