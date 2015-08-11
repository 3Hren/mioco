// Copyright 2015 Dawid Ciężarkiewicz <dpc@dpc.pw>
// See LICENSE-MPL2 file for more information.

//! Scalable, asynchronous IO coroutine-based handling (aka MIO COroutines).
//!
//! Using `mioco` you can handle scalable, asynchronous [`mio`][mio]-based IO, using set of synchronous-IO
//! handling functions. Based on asynchronous [`mio`][mio] events `mioco` will cooperatively schedule your
//! handlers.
//!
//! You can think of `mioco` as of *Node.js for Rust* or *[green threads][green threads] on top of [`mio`][mio]*.
//!
//! [green threads]: https://en.wikipedia.org/wiki/Green_threads
//! [mio]: https://github.com/carllerche/mio
//!
//! See `examples/echo.rs` for an example TCP echo server.
//!
/*!
```
// MAKE_DOC_REPLACEME
```
*/

#![cfg_attr(test, feature(convert))]
#![feature(result_expect)]
#![feature(reflect_marker)]
#![feature(rc_weak)]
#![warn(missing_docs)]

extern crate spin;
extern crate mio;
extern crate coroutine;
extern crate nix;
#[macro_use]
extern crate log;
extern crate bit_vec;

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::io;

use mio::{TryRead, TryWrite, Token, Handler, EventLoop, EventSet};
use std::any::Any;
use std::marker::{PhantomData, Reflect};
use mio::util::Slab;

use std::collections::VecDeque;
use spin::Mutex;
use std::sync::{Arc};

use bit_vec::BitVec;

/// Read/Write/Both
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RW {
    /// Read
    Read,
    /// Write
    Write,
    /// Any / Both (depends on context)
    Both,
}

impl RW {
    fn has_read(&self) -> bool {
        match *self {
            RW::Read | RW::Both => true,
            RW::Write => false,
        }
    }

    fn has_write(&self) -> bool {
        match *self {
            RW::Write | RW::Both => true,
            RW::Read => false,
        }
    }
}

/// Last Event
///
/// Read and/or Write + index of the handle in the order of `wrap` calls.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LastEvent {
    index : EventSourceIndex,
    rw : RW,
}

impl LastEvent {
    /// Index of the EventSourceShared handle
    pub fn index(&self) -> EventSourceIndex {
        self.index
    }

    /// Was the event a read
    pub fn has_read(&self) -> bool {
        self.rw.has_read()
    }

    /// Was the event a write
    pub fn has_write(&self) -> bool {
        self.rw.has_write()
    }
}

/// State of `mioco` coroutine
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum State {
    BlockedOn(RW),
    Running,
    Finished,
}

/// Sends notify `Message` to the mioco Event Loop.
pub type MioSender = mio::Sender<<Server as mio::Handler>::Message>;

/// `mioco` can work on any type implementing this trait
pub trait Evented : Any {
    /// Convert to &Any
    fn as_any(&self) -> &Any;
    /// Convert to &mut Any
    fn as_any_mut(&mut self) -> &mut Any;

    /// Register
    fn register(&self, event_loop : &mut EventLoop<Server>, token : Token, interest : EventSet);

    /// Reregister
    fn reregister(&self, event_loop : &mut EventLoop<Server>, token : Token, interest : EventSet);

    /// Deregister
    fn deregister(&self, event_loop : &mut EventLoop<Server>, token : Token);
}

impl<T> Evented for T
where T : mio::Evented+Reflect+'static {
    fn as_any(&self) -> &Any {
        self as &Any
    }

    fn as_any_mut(&mut self) -> &mut Any {
        self as &mut Any
    }

    fn register(&self, event_loop : &mut EventLoop<Server>, token : Token, interest : EventSet) {
        event_loop.register_opt(
            self, token,
            interest,
            mio::PollOpt::edge(),
            ).expect("register_opt failed");
    }

    fn reregister(&self, event_loop : &mut EventLoop<Server>, token : Token, interest : EventSet) {
        event_loop.reregister(
            self, token,
            interest,
            mio::PollOpt::edge(),
            ).expect("reregister failed");
    }

    fn deregister(&self, event_loop : &mut EventLoop<Server>, _token : Token) {
        event_loop.deregister(self).expect("deregister failed");
    }
}

impl<T> Evented for MailboxMiocoHandle<T>
where T:Reflect+'static {
    fn as_any(&self) -> &Any {
        self as &Any
    }

    fn as_any_mut(&mut self) -> &mut Any {
        self as &mut Any
    }

    /// Register
    fn register(&self, event_loop : &mut EventLoop<Server>, token : Token, interest : EventSet) {
        let mut lock = self.shared.lock();

        lock.token = Some(token);
        lock.sender = Some(event_loop.channel());
        lock.interest = interest;

        if interest.is_readable() && !lock.inn.is_empty() {
            lock.interest = EventSet::none();
            lock.sender.as_ref().unwrap().send(token).unwrap()
        }
    }

    /// Reregister
    fn reregister(&self, _event_loop : &mut EventLoop<Server>, token : Token, interest : EventSet) {
        let mut lock = self.shared.lock();

        lock.interest = interest;

        if interest.is_readable() && !lock.inn.is_empty() {
            lock.interest = EventSet::none();
            lock.sender.as_ref().unwrap().send(token).unwrap()
        }
    }

    /// Deregister
    fn deregister(&self, _event_loop : &mut EventLoop<Server>, _token : Token) {
        let mut lock = self.shared.lock();
        lock.interest = EventSet::none();
    }
}

impl Evented for Timer {
    fn as_any(&self) -> &Any {
        self as &Any
    }

    fn as_any_mut(&mut self) -> &mut Any {
        self as &mut Any
    }

    fn register(&self, event_loop : &mut EventLoop<Server>, token : Token, _interest : EventSet) {
        let mut state = self.state.borrow_mut();
        trace!("register timer: {:?}:{:?}", token, state.iteration);
        debug_assert!(state.status == TimerStatus::Stopped
                      , "register() called on timer that is already in use! ({:?})", state.status);
        match event_loop.timeout_ms((token, state.iteration), self.delay) {
            Ok(timeout) => {
                state.handle = Some(timeout);
                state.status = TimerStatus::Running;
            },
            Err(reason)=> {
                error!("Could not create mio::Timeout: {:?}", reason);
            }
        }
    }

    fn reregister(&self, event_loop : &mut EventLoop<Server>, token : Token, _interest : EventSet) {
        trace!("enter reregister timer: {:?}", token);
        let mut state = self.state.borrow_mut();
        if state.status == TimerStatus::Canceled {
            event_loop.clear_timeout(state.handle.unwrap());
            state.status = TimerStatus::Stopped;
        }
        debug_assert!(state.status == TimerStatus::Stopped
                      , "reregister() called on timer that is already in use! ({:?})", state.status);
        match event_loop.timeout_ms((token, state.iteration), self.delay) {
            Ok(timeout) => {
                trace!("reregistered timer: {:?}:{:?}", token, state.iteration);
                state.handle = Some(timeout);
                state.status = TimerStatus::Running;
            },
            Err(reason)=> {
                error!("Could not create mio::Timeout: {:?}", reason);
            }
        }
    }

    fn deregister(&self, event_loop : &mut EventLoop<Server>, _token : Token) {
        trace!("deregister timer: {:?}", _token);
        {
            let mut state = self.state.borrow_mut();
            if let Some(timer) = state.handle {
                event_loop.clear_timeout(timer);
                state.handle = None
            }
        }
    }
}

type RefCoroutine = Rc<RefCell<Coroutine>>;

/// `mioco` coroutine
///
/// Referenced by EventSourceShared running within it.
struct Coroutine {
    /// Coroutine of Coroutine itself. Stored here so it's available
    /// through every handle and `Coroutine` itself without referencing
    /// back
    handle : Option<coroutine::coroutine::Handle>,

    /// Current state
    state : State,

    /// Last event that resumed the coroutine
    last_event: LastEvent,

    /// Last register tick
    last_tick : u32,

    /// All handles, weak to avoid `Rc`-cycle
    io : Vec<Weak<RefCell<EventSourceShared>>>,

    /// Mask of handle indexes that we're blocked on
    blocked_on : BitVec<usize>,

    /// Mask of handle indexes that are registered in Server
    registered : BitVec<usize>,

    /// `Server` shared data that this `Coroutine` is running in
    server_shared : RefServerShared,

    /// Newly spawned `Coroutine`-es
    children_to_start : Vec<RefCoroutine>,
}


impl Coroutine {
    fn new(server : RefServerShared) -> Self {
        Coroutine {
            state: State::Running,
            handle: None,
            last_event: LastEvent{ rw: RW::Read, index: EventSourceIndex(0)},
            io: Vec::with_capacity(4),
            blocked_on: Default::default(),
            registered: Default::default(),
            server_shared: server,
            children_to_start: Vec::new(),
            last_tick: !0,
        }
    }

    /// After `resume()` on the `Coroutine.handle` finished,
    /// the `Coroutine` have blocked or finished and we need to
    /// perform the following maintenance
    fn after_resume(&mut self, event_loop: &mut EventLoop<Server>) {
        // If there were any newly spawned child-coroutines: start them now
        for coroutine in &self.children_to_start {
            let handle = {
                let co = coroutine.borrow_mut();
                co.handle.as_ref().map(|c| c.clone()).unwrap()
            };
            trace!("Resume new child coroutine");
            {
                if let Err(reason) = handle.resume() {
                    warn!("Co resume failed: {:?} in after_resume()", reason);
                    let mut co = coroutine.borrow_mut();
                    co.state = State::Finished;
                    co.blocked_on.clear_all();
                    co.server_shared.borrow_mut().coroutines_no -= 1;
                    if co.server_shared.borrow().coroutines_no == 0 {
                        event_loop.shutdown();
                    }
                } else {
                    coroutine.borrow_mut().reregister(event_loop);
                }
            }
        }
        self.children_to_start.clear();

        trace!("Reregister coroutine");
        self.reregister(event_loop);
    }

    fn reregister(&mut self, event_loop: &mut EventLoop<Server>) {
        if self.state == State::Finished {
            trace!("Coroutine: deregistering");
            self.deregister_all(event_loop);
            let mut shared = self.server_shared.borrow_mut();
            shared.coroutines_no -= 1;
            if shared.coroutines_no == 0 {
                debug!("Shutdown event loop - 0 coroutines left");
                event_loop.shutdown();
            }
        } else {
            self.reregister_blocked_on(event_loop)
        }
    }

    fn deregister_all(&mut self, event_loop: &mut EventLoop<Server>) {
        let mut shared = self.server_shared.borrow_mut();

        for i in 0..self.io.len() {
            let io = self.io[i].upgrade().unwrap();
            let mut io = io.borrow_mut();
            io.deregister(event_loop);
            trace!("Removing source token={:?}", io.token);
            shared.sources.remove(io.token).expect("cleared empty slot");
        }
    }

    fn reregister_blocked_on(&mut self, event_loop: &mut EventLoop<Server>) {

        let rw = match self.state {
            State::BlockedOn(rw) => rw,
            _ => panic!("This should not happen"),
        };

        // TODO: count leading zeros + for i in 0..32 {
        for i in 0..self.io.len() {
            match (self.registered[i], self.blocked_on[i])  {
                (false, false) | (true, true) => {},
                (false, true) => {
                    let io = self.io[i].upgrade().unwrap();
                    let mut io = io.borrow_mut();
                    io.reregister(event_loop, rw);
                },
                (true, false) => {
                    let io = self.io[i].upgrade().unwrap();
                    let io = io.borrow();
                    io.unreregister(event_loop);
                }
            }
        }

//        self.registered = self.blocked_on;
        for (mut target, src) in unsafe { self.registered.storage_mut().iter_mut().zip(self.blocked_on.storage().iter()) } {
            *target = *src
        }
    }
}

type RefEventSourceShared = Rc<RefCell<EventSourceShared>>;

/// Wrapped mio IO (mio::Evented+TryRead+TryWrite)
///
/// `Handle` is just a cloneable reference to this struct
struct EventSourceShared {
    coroutine: RefCoroutine,
    token: Token,
    index: usize, /// Index in MiocoHandle::handles
    io : Box<Evented+'static>,
    peer_hup: bool,
    registered: bool,
}

impl EventSourceShared {
    /// Handle `hup` condition
    fn hup(&mut self, _event_loop: &mut EventLoop<Server>, _token: Token) {
            self.peer_hup = true;
        }

    /// Reregister oneshot handler for the next event
    fn reregister(&mut self, event_loop: &mut EventLoop<Server>, rw : RW) {
            let mut interest = mio::EventSet::none();

            if !self.peer_hup {
                interest = interest | mio::EventSet::hup();

                if rw.has_read() {
                    interest = interest | mio::EventSet::readable();
                }
            }

            if rw.has_write() {
                interest = interest | mio::EventSet::writable();
            }

            if !self.registered {
                self.registered = true;
                Evented::register(&*self.io, event_loop, self.token, interest);
            } else {
                Evented::reregister(&*self.io, event_loop, self.token, interest);
             }
        }

    /// Un-reregister events we're not interested in anymore
    fn unreregister(&self, event_loop: &mut EventLoop<Server>) {
            debug_assert!(self.registered);
            let interest = mio::EventSet::none();
            Evented::reregister(&*self.io, event_loop, self.token, interest);
        }

    /// Un-reregister events we're not interested in anymore
    fn deregister(&mut self, event_loop: &mut EventLoop<Server>) {
            if self.registered {
                Evented::deregister(&*self.io, event_loop, self.token);
                self.registered = false;
            }
        }
}

/// `mioco` wrapper over raw structure implementing `mio::Evented` trait
#[derive(Clone)]
struct EventSource {
    inn : RefEventSourceShared,
}

/// `mioco` wrapper over raw mio IO structure
///
/// Create using `MiocoHandle::wrap()`
///
/// It implements standard library `Read` and `Write` and other
/// blocking-semantic operations, that switch and resume handler function
/// to build cooperative scheduling on top of asynchronous operations.
#[derive(Clone)]
pub struct TypedEventSource<T> {
    inn : RefEventSourceShared,
    _t: PhantomData<T>,
}

/// Index identification of a `TypedEventSource` used in `select`-like operations.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EventSourceIndex(usize);

impl EventSourceIndex {
    fn as_usize(&self) -> usize {
        self.0
    }
}

impl<T> TypedEventSource<T>
where T : Reflect+'static {
    /// Mark the `EventSource` blocked and block until `Server` does
    /// not wake us up again.
    fn block_on(&self, rw : RW) {
        {
            let inn = self.inn.borrow();
            inn.coroutine.borrow_mut().state = State::BlockedOn(rw);
            inn.coroutine.borrow_mut().blocked_on.clear_all();
            inn.coroutine.borrow_mut().blocked_on.set(inn.index, true);
        }
        trace!("coroutine blocked on {:?}", rw);
        coroutine::Coroutine::block();
        {
            let inn = self.inn.borrow_mut();
            debug_assert!(rw.has_read() || inn.coroutine.borrow().last_event.has_write());
            debug_assert!(rw.has_write() || inn.coroutine.borrow().last_event.has_read());
            debug_assert!(inn.coroutine.borrow().last_event.index().as_usize() == inn.index);
        }
    }

    /// Access raw mio type
    pub fn with_raw<F, R>(&self, f : F) -> R
        where F : Fn(&T) -> R {
        let io = &self.inn.borrow().io;
        f(io.as_any().downcast_ref::<T>().unwrap())
    }

    /// Access mutable raw mio type
    pub fn with_raw_mut<F, R>(&mut self, f : F) -> R
        where F : Fn(&mut T) -> R {
        let mut io = &mut self.inn.borrow_mut().io;
        f(io.as_any_mut().downcast_mut::<T>().unwrap())
    }

    /// Index identificator of a `TypedEventSource`
    pub fn index(&self) -> EventSourceIndex {
        EventSourceIndex(self.inn.borrow().index)
    }
}

impl EventSource {
    /// Readable event handler
    ///
    /// This corresponds to `mio::Handler::readable()`.
    pub fn ready(&mut self
                 , event_loop: &mut EventLoop<Server>
                 , token: Token
                 , events : EventSet
                 , tick : u32) {
        if events.is_hup() {
            let mut inn = self.inn.borrow_mut();
            inn.hup(event_loop, token);
        }

        let my_index = {
            let inn = self.inn.borrow();
            let index = inn.index;
            let mut co = inn.coroutine.borrow_mut();
            let prev_last_tick = co.last_tick;
            co.last_tick = tick;
            if prev_last_tick == tick {
                None
            } else if !co.blocked_on.get(index).unwrap() {
                // spurious event, probably after select in which
                // more than one event sources were reported ready
                // in one group of events, and first event source
                // deregistered the later ones
                debug!("spurious event for event source couroutine is not blocked on");
                None
            } else if let State::BlockedOn(rw) = co.state {
                match rw {
                    RW::Read if !events.is_readable() && !events.is_hup() => {
                        debug!("spurious not read event for coroutine blocked on read");
                        None
                    },
                    RW::Write if !events.is_writable() => {
                        debug!("spurious not write event for coroutine blocked on write");
                        None
                    },
                    RW::Both if !events.is_readable() && !events.is_hup() && !events.is_writable() => {
                        debug!("spurious unknown type event for coroutine blocked on read/write");
                        None
                    },
                    _ => {
                        co.registered.set(index, false);
                        Some(index)
                    }
                }
            } else {
                debug_assert!(co.state == State::Finished);
                return;
            }
        };

        if let Some(my_index) = my_index {
            // Wake coroutine on HUP, as it was read, to potentially let it fail the read and move on
            let event = match (events.is_readable() | events.is_hup(), events.is_writable()) {
                (true, true) => RW::Both,
                (true, false) => RW::Read,
                (false, true) => RW::Write,
                (false, false) => panic!(),
            };
            let handle = {
                let inn = self.inn.borrow();
                let coroutine_handle = inn.coroutine.borrow().handle.as_ref().map(|c| c.clone()).unwrap();
                inn.coroutine.borrow_mut().state = State::Running;
                inn.coroutine.borrow_mut().last_event = LastEvent {
                    rw: event,
                    index: EventSourceIndex(my_index),
                };
                coroutine_handle
            };

            if let Err(reason) = handle.resume() {
                warn!("Co resume failed in ready(): {:?}", reason);
                let inn = self.inn.borrow();
                inn.coroutine.borrow_mut().state = State::Finished;
                inn.coroutine.borrow_mut().blocked_on.clear_all();
            }
        }

        let coroutine = {
            let inn = &self.inn.borrow();
            inn.coroutine.clone()
        };

        let mut co = coroutine.borrow_mut();

        co.after_resume(event_loop);

    }
}

impl<T> TypedEventSource<T>
where T : mio::TryAccept+Reflect+'static {
    /// Block on accept
    pub fn accept(&self) -> io::Result<T::Output> {
        loop {
            let res = {
                let mut inn = self.inn.borrow_mut();
                inn.io.as_any_mut().downcast_mut::<T>().unwrap().accept()
            };

            match res {
                Ok(None) => {
                    self.block_on(RW::Read)
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }
}

impl<T> std::io::Read for TypedEventSource<T>
where T : TryRead+Reflect+'static {
    /// Block on read
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let res = {
                let mut inn = self.inn.borrow_mut();
                inn.io.as_any_mut().downcast_mut::<T>().unwrap().try_read(buf)
            };

            match res {
                Ok(None) => {
                    self.block_on(RW::Read)
                },
                Ok(Some(r))  => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }
}

impl<T> std::io::Write for TypedEventSource<T>
where T : TryWrite+Reflect+'static {
    /// Block on write
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            let res = {
                let mut inn = self.inn.borrow_mut();
                inn.io.as_any_mut().downcast_mut::<T>().unwrap().try_write(buf)
            };

            match res {
                Ok(None) => {
                    self.block_on(RW::Write)
                },
                Ok(Some(r)) => {
                    return Ok(r);
                },
                Err(e) => {
                    return Err(e)
                }
            }
        }
    }

    /// Flush. This currently does nothing
    ///
    /// TODO: Should we do something with the flush? --dpc */
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Mioco Handle
///
/// Use this from withing coroutines to perform `mioco`-provided functionality
pub struct MiocoHandle {
    coroutine : Rc<RefCell<Coroutine>>,
}

fn select_impl_set_mask_from_indices(indices : &[EventSourceIndex], blocked_on : &mut BitVec<usize>) {
    {
        blocked_on.clear_all();
        for &index in indices {
            blocked_on.set(index.as_usize(), true);
        }
    }
}

fn select_impl_set_mask_rc_handles(handles : &[Weak<RefCell<EventSourceShared>>], blocked_on: &mut BitVec<usize>) {
    {
        blocked_on.clear_all();
        for handle in handles {
            let io = handle.upgrade().unwrap();
            blocked_on.set(io.borrow().index, true);
        }
    }
}

impl MiocoHandle {

    /// Create a `mioco` coroutine handler
    ///
    /// `f` is routine handling connection. It must not use any real blocking-IO operations, only
    /// `mioco` provided types (`TypedEventSource`) and `MiocoHandle` functions. Otherwise `mioco`
    /// cooperative scheduling can block on real blocking-IO which defeats using mioco.
    pub fn spawn<F>(&self, f : F)
        where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static {
            let coroutine_ref = spawn_impl(f, self.coroutine.borrow().server_shared.clone());
            self.coroutine.borrow_mut().children_to_start.push(coroutine_ref);
        }

    /// Register `mio`'s native io type to be used within `mioco` coroutine
    ///
    /// Consumes the `io`, returns a mioco wrapper over it. Use this wrapped IO
    /// to perform IO.
    pub fn wrap<T : 'static>(&mut self, io : T) -> TypedEventSource<T>
    where T : Evented {
        let token = {
            let co = self.coroutine.borrow();
            let mut shared = co.server_shared.borrow_mut();
            shared.sources.insert_with(|token| {
                EventSource {
                    inn: Rc::new(RefCell::new(
                                 EventSourceShared {
                                     coroutine: self.coroutine.clone(),
                                     io: Box::new(io),
                                     token: token,
                                     peer_hup: false,
                                     index: self.coroutine.borrow().io.len(),
                                     registered: false,
                                 }
                                 )),
                }
            })
        }.expect("run out of tokens");
        trace!("Added source token={:?}", token);

        let io = {
            let co = self.coroutine.borrow();
            let shared = co.server_shared.borrow_mut();
            shared.sources[token].inn.clone()
        };

        let handle = TypedEventSource {
            inn: io.clone(),
            _t: PhantomData,
        };

        self.coroutine.borrow_mut().io.push(io.clone().downgrade());
        let len = self.coroutine.borrow().io.len();
        trace!("setting lengths to {}", len);
        self.coroutine.borrow_mut().blocked_on.grow(len, false);
        self.coroutine.borrow_mut().registered.grow(len, false);

        handle
    }

    /// Create a new timer which will provide a timeout event after
    /// `delay` milliseconds.
    pub fn timeout(&mut self, delay: u64) -> TypedEventSource<Timer> {
        self.wrap(Timer::new(delay))
    }

    /// Pause the current co-routine for `delay` milliseconds.
    pub fn sleep(&mut self, delay: u64) {
        let timeout = self.timeout(delay).index();
        self.select_read_from(&[timeout]);
    }

    /// Wait till a read event is ready
    fn select_impl(&mut self, rw : RW) -> LastEvent {
        self.coroutine.borrow_mut().state = State::BlockedOn(rw);
        coroutine::Coroutine::block();
        debug_assert!(self.coroutine.borrow().state == State::Running);

        self.coroutine.borrow().last_event
    }

    /// Wait till an event is ready
    ///
    /// The returned value contains event type and the index id of the `TypedEventSource`.
    /// See `TypedEventSource::index()`.
    pub fn select(&mut self) -> LastEvent {
        {
            let Coroutine {
                ref io,
                ref mut blocked_on,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_rc_handles(&**io, blocked_on);
        }
        self.select_impl(RW::Both)
    }

    /// Wait till a read event is ready
    ///
    /// See `MiocoHandle::select`.
    pub fn select_read(&mut self) -> LastEvent {
        {
            let Coroutine {
                ref io,
                ref mut blocked_on,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_rc_handles(&**io, blocked_on);
        }
        self.select_impl(RW::Read)
    }

    /// Wait till a read event is ready.
    ///
    /// See `MiocoHandle::select`.
    pub fn select_write(&mut self) -> LastEvent {
        {
            let Coroutine {
                ref io,
                ref mut blocked_on,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_rc_handles(&**io, blocked_on);
        }
        self.select_impl(RW::Write)
    }

    /// Wait till any event is ready on a set of Handles.
    ///
    /// See `TypedEventSource::index()`.
    /// See `MiocoHandle::select()`.
    pub fn select_from(&mut self, indices : &[EventSourceIndex]) -> LastEvent {
        {
            let Coroutine {
                ref mut blocked_on,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_from_indices(indices, blocked_on);
        }

        self.select_impl(RW::Both)
    }

    /// Wait till write event is ready on a set of Handles.
    ///
    /// See `MiocoHandle::select_from`.
    pub fn select_write_from(&mut self, indices : &[EventSourceIndex]) -> LastEvent {
        {
            let Coroutine {
                ref mut blocked_on,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_from_indices(indices, blocked_on);
        }

        self.select_impl(RW::Write)
    }

    /// Wait till read event is ready on a set of Handles.
    ///
    /// See `MiocoHandle::select_from`.
    pub fn select_read_from(&mut self, indices : &[EventSourceIndex]) -> LastEvent {
        {
            let Coroutine {
                ref mut blocked_on,
                ..
            } = *self.coroutine.borrow_mut();

            select_impl_set_mask_from_indices(indices, blocked_on);
        }

        self.select_impl(RW::Read)
    }
}

type RefServerShared = Rc<RefCell<ServerShared>>;
/// Data belonging to `Server`, but referenced and manipulated by `Coroutine`-es
/// belonging to it.
struct ServerShared {
    /// Slab allocator
    /// TODO: dynamically growing slab would be better; or a fast hashmap?
    /// FIXME: See https://github.com/carllerche/mio/issues/219 . Using an allocator
    /// in which just-deleted entries are not potentially reused right away might prevent
    /// potentical sporious wakeups on newly allocated entries.
    sources : Slab<EventSource>,

    /// Number of `Coroutine`-s running in the `Server`.
    coroutines_no : u32,
}

impl ServerShared {
    fn new() -> Self {
        ServerShared {
            sources: Slab::new(1024),
            coroutines_no: 0,
        }
    }
}

fn spawn_impl<F>(f : F, server : RefServerShared) -> RefCoroutine
where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static {


    struct SendFnOnce<F>
    {
        f : F
    }

    // We fake the `Send` because `mioco` guarantees serialized
    // execution between coroutines, switching between them
    // only in predefined points.
    unsafe impl<F> Send for SendFnOnce<F>
        where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static
        {

        }

    struct SendRefCoroutine {
        coroutine: RefCoroutine,
    }

    // Same logic as in `SendFnOnce` applies here.
    unsafe impl Send for SendRefCoroutine { }

    trace!("Coroutine: spawning");
    server.borrow_mut().coroutines_no += 1;

    let coroutine_ref = Rc::new(RefCell::new(Coroutine::new(server)));

    let sendref = SendRefCoroutine {
        coroutine: coroutine_ref.clone(),
    };

    let send_f = SendFnOnce {
        f: f,
    };

    let coroutine_handle = coroutine::coroutine::Coroutine::spawn(move || {
        trace!("Coroutine: started");
        let mut mioco_handle = MiocoHandle {
            coroutine: sendref.coroutine,
        };

        let SendFnOnce { f } = send_f;

        let _res = f(&mut mioco_handle);

        mioco_handle.coroutine.borrow_mut().state = State::Finished;
        mioco_handle.coroutine.borrow_mut().blocked_on.clear_all();
        trace!("Coroutine: finished");
    });

    coroutine_ref.borrow_mut().handle = Some(coroutine_handle);

    coroutine_ref
}

/// `Server` registered in `mio::EventLoop` and implementing `mio::Handler`.
pub struct Server {
    shared : RefServerShared,
    tick : u32,
}

impl Server {
    fn new(shared : RefServerShared) -> Self {
        Server {
            shared: shared,
            tick : 0,
        }
    }
}

impl mio::Handler for Server {
    type Timeout = (Token, u32);
    type Message = Token;


    fn ready(&mut self, event_loop: &mut mio::EventLoop<Server>, token: mio::Token, events: mio::EventSet) {
        // TODO: move to tick()
        self.tick += 1;
        // It's possible we got an event for a Source that was deregistered
        // by finished coroutine. In case the token is already occupied by
        // different source, we will wake it up needlessly. If it's empty, we just
        // ignore the event.
        trace!("Server::ready(token={:?})", token);
        let mut source = match self.shared.borrow().sources.get(token) {
            Some(source) => source.clone(),
            None => {
                trace!("Server::ready() ignored");
                return
            },
        };
        source.ready(event_loop, token, events, self.tick);
        trace!("Server::ready finished");

    }

    fn notify(&mut self, event_loop: &mut EventLoop<Server>, msg: Self::Message) {
        // TODO: move to tick()
        self.tick += 1;

        let token = msg;
        trace!("Server::notify(token={:?})", token);
        let mut source = match self.shared.borrow().sources.get(token) {
            Some(source) => source.clone(),
            None => {
                trace!("Server::notify() ignored");
                return
            },
        };
        source.ready(event_loop, token, EventSet::readable(), self.tick);
        trace!("Server::notify() finished");
    }

    fn timeout(&mut self, event_loop: &mut EventLoop<Self>, msg: Self::Timeout) {
        // TODO: move to tick()
        self.tick += 1;
        let (token, msg_iteration) = msg;
        trace!("Server::timeout(token={:?}, iteration={:?})", token, msg_iteration);
        let mut source = match self.shared.borrow().sources.get(token) {
            Some(source) => source.clone(),
            None => {
                trace!("Server::timeout() ignored");
                return
            },
        };
        let (status, current_iteration) = {
            let mut inn = source.inn.borrow_mut();
            let timer = inn.io.as_any_mut().downcast_mut::<Timer>().unwrap();
            let status = timer.state.borrow().status;
            let iteration = timer.state.borrow().iteration;
            (status, iteration)
        };
        let ev = if status == TimerStatus::Running && msg_iteration == current_iteration {
                     EventSet::readable()
                 } else {
                     trace!("Server::timeout() not delivered for state: {:?}, iteration: {:?}"
                           , status, current_iteration);
                     EventSet::none()
                 };
        {
            let mut inn = source.inn.borrow_mut();
            let timer = inn.io.as_any_mut().downcast_mut::<Timer>().unwrap();
            timer.fired();
        }
        source.ready(event_loop, token, ev, self.tick);
        trace!("Server::timeout() finished");
    }
}

/// Mioco struct
pub struct Mioco {
    event_loop : EventLoop<Server>,
    server : Server,
}

impl Mioco {
    /// Create new `Mioco` instance
    pub fn new() -> Self {
        let shared = Rc::new(RefCell::new(ServerShared::new()));
        Mioco {
            event_loop: EventLoop::new().expect("new EventLoop"),
            server: Server::new(shared.clone()),
        }
    }

    /// Start mioco handling
    ///
    /// Takes a starting handler function that will be executed in `mioco` environment.
    ///
    /// Will block until `mioco` is finished - there are no more handlers to run.
    ///
    /// See `MiocoHandle::spawn()`.
    pub fn start<F>(&mut self, f : F)
        where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static,
        {
            let Mioco {
                ref mut server,
                ref mut event_loop,
            } = *self;

            let shared = server.shared.clone();

            let coroutine_ref = spawn_impl(f, shared);

            let coroutine_handle = coroutine_ref.borrow().handle.as_ref().map(|c| c.clone()).unwrap();

            trace!("Initial resume");
            if let Err(reason) = coroutine_handle.resume() {
                warn!("Co resume failed: {:?} in start()", reason);
                let mut co = coroutine_ref.borrow_mut();
                co.state = State::Finished;
                co.blocked_on.clear_all();
            }
            coroutine_ref.borrow_mut().after_resume(event_loop);

            let coroutines_no = server.shared.borrow().coroutines_no;
            if  coroutines_no > 0 {
                trace!("Start event loop");
                event_loop.run(server).unwrap();
            } else {
                trace!("No coroutines to start event loop with");
            }
        }

}

/// Create a Mailbox
///
/// Mailbox can be used to deliver notifications to handlers from different threads
pub fn mailbox<T>() -> (MailboxOutsideHandle<T>, MailboxMiocoHandle<T>) {

    let shared = MailboxShared {
        token: None,
        sender: None,
        inn: VecDeque::new(),
        interest: EventSet::none(),
    };

    let shared = Arc::new(Mutex::new(shared));

    (MailboxOutsideHandle::new(shared.clone()), MailboxMiocoHandle::new(shared))
}

type RefMailboxShared<T> = Arc<Mutex<MailboxShared<T>>>;
type MailboxQueue<T> = Option<T>;

struct MailboxShared<T> {
    /// Token, put here when the MiocoHandle is consumed with `wrap()` and
    /// first registered,
    token : Option<Token>,
    sender : Option<MioSender>,
    inn : VecDeque<T>,
    interest : EventSet,
}

/// Outside Mailbox Handle
///
/// Use from outside the coroutine handler.
///
/// Create with `mailbox()`
pub struct MailboxOutsideHandle<T> {
    shared : RefMailboxShared<T>,
}

/// Inside Mailbox Handle
///
/// Use from within coroutine handler.
///
/// Create with `mailbox()`.
pub struct MailboxMiocoHandle<T> {
    shared : RefMailboxShared<T>,
}

impl<T> MailboxOutsideHandle<T> {
    fn new(shared : RefMailboxShared<T>) -> Self {
        MailboxOutsideHandle {
            shared: shared
        }
    }
}

impl<T> MailboxMiocoHandle<T> {
    fn new(shared : RefMailboxShared<T>) -> Self {
        MailboxMiocoHandle {
            shared: shared
        }
    }
}

impl<T> MailboxOutsideHandle<T> {
    /// Non-blocking send
    ///
    /// Will deliver `t` to corespondong `MailboxMiocoHandle`.
    pub fn send(&self, t : T) -> io::Result<()> {
        let mut lock = self.shared.lock();
        let MailboxShared {
            ref mut sender,
            ref mut token,
            ref mut inn,
            ref mut interest,
        } = *lock;

        inn.push_back(t);

        if interest.is_readable() {
            if let &mut Some(token) = token {
                let sender = sender.as_ref().unwrap();
                sender.send(token).unwrap()
            }
        }

        Ok(())
    }
}

impl<T> TypedEventSource<MailboxMiocoHandle<T>>
where T : Reflect+'static {
    /// Receive `T` sent using corresponding `MailboxOutsideHandle::send()`.
    ///
    /// Will block coroutine if no elements are available.
    pub fn recv(&mut self) -> T {
        loop {
            {
                let mut inn = self.inn.borrow_mut();
                let handle = inn.io.as_any_mut().downcast_mut::<MailboxMiocoHandle<T>>().unwrap();
                let mut lock = handle.shared.lock();

                if let Some(t) = lock.inn.pop_front() {
                    return t;
                }
            }

            self.block_on(RW::Read)
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TimerStatus  {
    Running,
    Stopped,
    Done,
    Canceled,
}

struct TimerState {
    handle: Option<mio::Timeout>,
    status: TimerStatus,
    iteration: u32
}

/// A Timer is used in `select` functions to create a timeout event.
pub struct Timer {
    // Delay in ms
    delay: u64,
    // Ref to created mio Timeout after registration
    state: RefCell<TimerState>
}

impl Timer {
    fn new (delay: u64) -> Timer {
        let state = TimerState { handle: None,
                                 status: TimerStatus::Stopped,
                                 iteration: 0};
        Timer { delay: delay, state: RefCell::new(state) }
    }

    fn fired (&self) {
        let mut state = self.state.borrow_mut();
        state.status = TimerStatus::Done;
    }

    fn reset (&self) {
        trace!("resetting timer");
        let mut state = self.state.borrow_mut();
        match state.status {
            TimerStatus::Done => state.status = TimerStatus::Stopped,
            TimerStatus::Running => state.status = TimerStatus::Canceled,
            TimerStatus::Canceled => {},
            TimerStatus::Stopped => {},
        }
        state.iteration += 1;
    }
}

impl TypedEventSource<Timer> {
    /// Read a timer to block on it until it is done.
    pub fn read (&self) -> io::Result<()> {
        let status = self.with_raw(|timer| { timer.state.borrow().status });
        match status {
            TimerStatus::Stopped|TimerStatus::Canceled => {
                Err(io::Error::new(io::ErrorKind::Other, "Timer has not been registered"))
            },
            TimerStatus::Running => {
                self.block_on(RW::Read);
                self.read()
            },
            TimerStatus::Done => Ok(()),
        }
    }

    /// Reset a previously used timer so that it can be used again.
    /// Any pending notification will be ignored and a new timeout
    /// will be created when the timer is re-registered.
    pub fn reset (&self) {
        self.with_raw(|timer| { timer.reset() });
    }
}


/// Shorthand for creating new `Mioco` instance and starting it right away.
pub fn start<F>(f : F)
    where F : FnOnce(&mut MiocoHandle) -> io::Result<()> + 'static,
{
    let mut mioco = Mioco::new();
    mioco.start(f);
}

#[cfg(test)]
mod tests;
