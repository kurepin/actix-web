use std::mem;
use std::rc::Rc;

use futures::{Async, Poll, Future};

use task::Task;
use error::Error;
use payload::Payload;
use middlewares::{Middleware, Finished, Started, Response};
use h1writer::Writer;
use httprequest::HttpRequest;
use httpresponse::HttpResponse;

type Handler = Fn(&mut HttpRequest, Payload) -> Task;
pub(crate) type PipelineHandler<'a> = &'a Fn(&mut HttpRequest, Payload) -> Task;

pub struct Pipeline(PipelineState);

enum PipelineState {
    None,
    Starting(Start),
    Handle(Box<Handle>),
    Finishing(Box<Finish>),
    Error(Box<(Task, HttpRequest)>),
    Task(Box<(Task, HttpRequest)>),
}

impl Pipeline {

    pub fn new(mut req: HttpRequest, payload: Payload,
               mw: Rc<Vec<Box<Middleware>>>, handler: PipelineHandler) -> Pipeline {
        if mw.is_empty() {
            let task = (handler)(&mut req, payload);
            Pipeline(PipelineState::Task(Box::new((task, req))))
        } else {
            match Start::init(mw, req, handler, payload) {
                StartResult::Ready(res) => {
                    Pipeline(PipelineState::Handle(res))
                },
                StartResult::NotReady(res) => {
                    Pipeline(PipelineState::Starting(res))
                },
            }
        }
    }

    pub fn error<R: Into<HttpResponse>>(resp: R) -> Self {
        Pipeline(PipelineState::Error(Box::new((Task::reply(resp), HttpRequest::for_error()))))
    }

    pub(crate) fn disconnected(&mut self) {
        match self.0 {
            PipelineState::Starting(ref mut st) =>
                st.disconnected(),
            PipelineState::Handle(ref mut st) =>
                st.task.disconnected(),
            _ =>(),
        }
    }

    pub(crate) fn poll_io<T: Writer>(&mut self, io: &mut T) -> Poll<bool, ()> {
        loop {
            let state = mem::replace(&mut self.0, PipelineState::None);
            match state {
                PipelineState::Task(mut st) => {
                    let req:&mut HttpRequest = unsafe{mem::transmute(&mut st.1)};
                    let res = st.0.poll_io(io, req);
                    self.0 = PipelineState::Task(st);
                    return res
                }
                PipelineState::Starting(mut st) => {
                    match st.poll() {
                        Async::NotReady => {
                            self.0 = PipelineState::Starting(st);
                            return Ok(Async::NotReady)
                        }
                        Async::Ready(h) =>
                            self.0 = PipelineState::Handle(h),
                    }
                }
                PipelineState::Handle(mut st) => {
                    let res = st.poll_io(io);
                    if let Ok(Async::Ready(r)) = res {
                        if r {
                            self.0 = PipelineState::Finishing(st.finish());
                            return Ok(Async::Ready(false))
                        } else {
                            self.0 = PipelineState::Handle(st);
                            return res
                        }
                    } else {
                        self.0 = PipelineState::Handle(st);
                        return res
                    }
                }
                PipelineState::Error(mut st) => {
                    let req:&mut HttpRequest = unsafe{mem::transmute(&mut st.1)};
                    let res = st.0.poll_io(io, req);
                    self.0 = PipelineState::Error(st);
                    return res
                }
                PipelineState::Finishing(_) | PipelineState::None => unreachable!(),
            }
        }
    }

    pub(crate) fn poll(&mut self) -> Poll<(), Error> {
        loop {
            let state = mem::replace(&mut self.0, PipelineState::None);
            match state {
                PipelineState::Handle(mut st) => {
                    let res = st.poll();
                    match res {
                        Ok(Async::NotReady) => {
                            self.0 = PipelineState::Handle(st);
                            return Ok(Async::NotReady)
                        }
                        Ok(Async::Ready(())) | Err(_) => {
                            self.0 = PipelineState::Finishing(st.finish());
                        }
                    }
                }
                PipelineState::Finishing(mut st) => {
                    let res = st.poll();
                    self.0 = PipelineState::Finishing(st);
                    return Ok(res)
                }
                PipelineState::Error(mut st) => {
                    let res = st.0.poll();
                    self.0 = PipelineState::Error(st);
                    return res
                }
                PipelineState::Task(mut st) => {
                    let res = st.0.poll();
                    self.0 = PipelineState::Task(st);
                    return res
                }
                _ => {
                    self.0 = state;
                    return Ok(Async::Ready(()))
                }
            }
        }
    }
}

struct Handle {
    idx: usize,
    req: HttpRequest,
    task: Task,
    middlewares: Rc<Vec<Box<Middleware>>>,
}

impl Handle {
    fn new(idx: usize,
           req: HttpRequest,
           task: Task,
           mw: Rc<Vec<Box<Middleware>>>) -> Handle
    {
        Handle {
            idx: idx, req: req, task:task, middlewares: mw }
    }

    fn poll_io<T: Writer>(&mut self, io: &mut T) -> Poll<bool, ()> {
        self.task.poll_io(io, &mut self.req)
    }

    fn poll(&mut self) -> Poll<(), Error> {
        self.task.poll()
    }

    fn finish(mut self) -> Box<Finish> {
        Box::new(Finish {
            idx: self.idx,
            req: self.req,
            fut: None,
            resp: self.task.response(),
            middlewares: self.middlewares
        })
    }
}

/// Middlewares start executor
struct Finish {
    idx: usize,
    req: HttpRequest,
    resp: HttpResponse,
    fut: Option<Box<Future<Item=(), Error=Error>>>,
    middlewares: Rc<Vec<Box<Middleware>>>,
}

impl Finish {

    pub fn poll(&mut self) -> Async<()> {
        loop {
            // poll latest fut
            if let Some(ref mut fut) = self.fut {
                match fut.poll() {
                    Ok(Async::NotReady) => return Async::NotReady,
                    Ok(Async::Ready(())) => self.idx -= 1,
                    Err(err) => {
                        error!("Middleware finish error: {}", err);
                        self.idx -= 1;
                    }
                }
            }
            self.fut = None;

            match self.middlewares[self.idx].finish(&mut self.req, &self.resp) {
                Finished::Done => {
                    if self.idx == 0 {
                        return Async::Ready(())
                    } else {
                        self.idx -= 1
                    }
                }
                Finished::Future(fut) => {
                    self.fut = Some(fut);
                },
            }
        }
    }
}

type Fut = Box<Future<Item=(HttpRequest, Option<HttpResponse>), Error=(HttpRequest, HttpResponse)>>;

/// Middlewares start executor
struct Start {
    idx: usize,
    hnd: *mut Handler,
    disconnected: bool,
    payload: Option<Payload>,
    fut: Option<Fut>,
    middlewares: Rc<Vec<Box<Middleware>>>,
}

enum StartResult {
    Ready(Box<Handle>),
    NotReady(Start),
}

impl Start {

    fn init(mw: Rc<Vec<Box<Middleware>>>,
            req: HttpRequest, handler: PipelineHandler, payload: Payload) -> StartResult
    {
        Start {
            idx: 0,
            fut: None,
            disconnected: false,
            hnd: handler as *const _ as *mut _,
            payload: Some(payload),
            middlewares: mw,
        }.start(req)
    }

    fn disconnected(&mut self) {
        self.disconnected = true;
    }

    fn prepare(&self, mut task: Task) -> Task {
        if self.disconnected {
            task.disconnected()
        }
        task.set_middlewares(
            MiddlewaresResponse::new(self.idx, Rc::clone(&self.middlewares)));
        task
    }

    fn start(mut self, mut req: HttpRequest) -> StartResult {
        loop {
            if self.idx >= self.middlewares.len() {
                let task = (unsafe{&*self.hnd})(
                    &mut req, self.payload.take().expect("Something is completlywrong"));
                return StartResult::Ready(
                    Box::new(Handle::new(self.idx-1, req, self.prepare(task), self.middlewares)))
            } else {
                req = match self.middlewares[self.idx].start(req) {
                    Started::Done(req) => {
                        self.idx += 1;
                        req
                    }
                    Started::Response(req, resp) => {
                        return StartResult::Ready(
                            Box::new(Handle::new(
                                self.idx, req, self.prepare(Task::reply(resp)), self.middlewares)))
                    },
                    Started::Future(mut fut) => {
                        match fut.poll() {
                            Ok(Async::NotReady) => {
                                self.fut = Some(fut);
                                return StartResult::NotReady(self)
                            }
                            Ok(Async::Ready((req, resp))) => {
                                self.idx += 1;
                                if let Some(resp) = resp {
                                    return StartResult::Ready(
                                        Box::new(Handle::new(
                                            self.idx, req,
                                            self.prepare(Task::reply(resp)), self.middlewares)))
                                }
                                req
                            }
                            Err((req, resp)) => {
                                return StartResult::Ready(Box::new(Handle::new(
                                    self.idx, req,
                                    self.prepare(Task::reply(resp)), self.middlewares)))
                            }
                        }
                    },
                }
            }
        }
    }

    fn poll(&mut self) -> Async<Box<Handle>> {
        'outer: loop {
            match self.fut.as_mut().unwrap().poll() {
                Ok(Async::NotReady) => return Async::NotReady,
                Ok(Async::Ready((mut req, resp))) => {
                    self.idx += 1;
                    if let Some(resp) = resp {
                        return Async::Ready(Box::new(Handle::new(
                            self.idx, req,
                            self.prepare(Task::reply(resp)), Rc::clone(&self.middlewares))))
                    }
                    if self.idx >= self.middlewares.len() {
                        let task = (unsafe{&*self.hnd})(
                            &mut req, self.payload.take().expect("Something is completlywrong"));
                        return Async::Ready(Box::new(Handle::new(
                            self.idx-1, req,
                            self.prepare(task), Rc::clone(&self.middlewares))))
                    } else {
                        loop {
                            req = match self.middlewares[self.idx].start(req) {
                                Started::Done(req) => {
                                    self.idx += 1;
                                    req
                                }
                                Started::Response(req, resp) => {
                                    return Async::Ready(Box::new(Handle::new(
                                        self.idx, req,
                                        self.prepare(Task::reply(resp)),
                                        Rc::clone(&self.middlewares))))
                                },
                                Started::Future(mut fut) => {
                                    self.fut = Some(fut);
                                    continue 'outer
                                },
                            }
                        }
                    }
                }
                Err((req, resp)) => {
                    return Async::Ready(Box::new(Handle::new(
                        self.idx, req,
                        self.prepare(Task::reply(resp)),
                        Rc::clone(&self.middlewares))))
                }
            }
        }
    }
}


/// Middlewares response executor
pub(crate) struct MiddlewaresResponse {
    idx: usize,
    fut: Option<Box<Future<Item=HttpResponse, Error=HttpResponse>>>,
    middlewares: Rc<Vec<Box<Middleware>>>,
}

impl MiddlewaresResponse {

    fn new(idx: usize, mw: Rc<Vec<Box<Middleware>>>) -> MiddlewaresResponse {
        let idx = if idx == 0 { 0 } else { idx - 1 };
        MiddlewaresResponse {
            idx: idx,
            fut: None,
            middlewares: mw }
    }

    pub fn response(&mut self, req: &mut HttpRequest, mut resp: HttpResponse)
                    -> Option<HttpResponse>
    {
        loop {
            resp = match self.middlewares[self.idx].response(req, resp) {
                Response::Response(r) => {
                    if self.idx == 0 {
                        return Some(r)
                    } else {
                        self.idx -= 1;
                        r
                    }
                },
                Response::Future(fut) => {
                    self.fut = Some(fut);
                    return None
                },
            };
        }
    }

    pub fn poll(&mut self, req: &mut HttpRequest) -> Poll<Option<HttpResponse>, ()> {
        if self.fut.is_none() {
            return Ok(Async::Ready(None))
        }

        loop {
            // poll latest fut
            let mut resp = match self.fut.as_mut().unwrap().poll() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(resp)) | Err(resp) => {
                    self.idx += 1;
                    resp
                }
            };

            loop {
                if self.idx == 0 {
                    return Ok(Async::Ready(Some(resp)))
                } else {
                    match self.middlewares[self.idx].response(req, resp) {
                        Response::Response(r) => {
                            self.idx -= 1;
                            resp = r
                        },
                        Response::Future(fut) => {
                            self.fut = Some(fut);
                            break
                        },
                    }
                }
            }
        }
    }
}