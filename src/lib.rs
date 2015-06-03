extern crate mio;
extern crate coroutine;
extern crate nix;

use std::cell::RefCell;
use std::sync::Arc;
use std::io;
use std::fmt;
use std::fmt::Display;
use mio::{TryRead, TryWrite};

impl fmt::Display for Coroutine {
    fn fmt(&self, fmt : &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(fmt, "{:?}, {:?}",
               self.io.borrow().stream,
               self.io.borrow().stream.peer_addr()
              )
    }
}
pub struct Coroutine {
    coroutine : coroutine::coroutine::Handle,
    io: Arc<RefCell<IO>>,
    peer_hup: bool,
    interest: mio::Interest,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
enum State {
    BlockedOnWrite,
    BlockedOnRead,
    Running,
    Finished,
}

#[derive(Debug)]
struct IO {
    state : State,
    stream: mio::tcp::TcpStream,
}

#[derive(Clone)]
pub struct IOHandle {
    io : Arc<RefCell<IO>>
}

/* TODO: Is this OK? Since io is Arc, it seems OK */
unsafe impl Send for IOHandle {

}

impl Coroutine {
    pub fn new<F, H>(
        stream: mio::tcp::TcpStream, event_loop: &mut mio::EventLoop<H>, token: mio::Token, f : F
        ) -> Coroutine
        where
        F : FnOnce(&mut IOHandle) + Send + 'static,
        H : mio::Handler
        {
            let mut io_handle = IOHandle {
                io: Arc::new(RefCell::new(IO {
                    stream: stream,
                    state: State::Running,
                })),
            };

            let mut coroutine = Coroutine {
                io: io_handle.io.clone(),
                coroutine: coroutine::coroutine::Coroutine::spawn(move || {
                    f(&mut io_handle);
                    io_handle.io.borrow_mut().stream.shutdown(mio::tcp::Shutdown::Both).unwrap();
                    io_handle.io.borrow_mut().state = State::Finished;
                }),
                peer_hup: false,
                interest: mio::Interest::none(),
            };
            coroutine.coroutine.resume().ok().expect("resume() failed");


            coroutine.interest = match coroutine.io.borrow().state {
                State::Running => panic!("wrong state"),
                State::BlockedOnRead => mio::Interest::readable(),
                State::BlockedOnWrite => mio::Interest::writable(),
                State::Finished => mio::Interest::hup(),
            };

            event_loop.register_opt(
                &coroutine.io.borrow_mut().stream, token,
                coroutine.interest, mio::PollOpt::edge() | mio::PollOpt::oneshot()
                ).ok().expect("register_opt failed");

            coroutine
        }

    pub fn is_finished(&self) -> bool {
        self.io.borrow().state == State::Finished && self.interest == mio::Interest::none()
    }

    pub fn readable<H>(&mut self, event_loop: &mut mio::EventLoop<H>, token: mio::Token, hint: mio::ReadHint)
        where H : mio::Handler {

            if hint.is_hup() {
                self.hup(event_loop, token);
                return;
            }

            if self.io.borrow().state == State::BlockedOnRead {
                self.io.borrow_mut().state = State::Running;
                self.coroutine.resume().ok().expect("resume() failed");
            }

            self.reregister(event_loop, token)
        }

    pub fn writable<H>(&mut self, event_loop: &mut mio::EventLoop<H>, token: mio::Token)
        where H : mio::Handler {

            if self.io.borrow().state == State::BlockedOnWrite {
                self.io.borrow_mut().state = State::Running;
                self.coroutine.resume().ok().expect("resume() failed");
            }

            self.reregister(event_loop, token)
        }

    fn hup<H>(&mut self, event_loop: &mut mio::EventLoop<H>, token: mio::Token)
        where H : mio::Handler {
            if self.interest == mio::Interest::hup() {
                self.interest = mio::Interest::none();
                event_loop.deregister(&self.io.borrow_mut().stream).ok().expect("deregister() failed");
            } else {
                self.peer_hup = true;
                self.reregister(event_loop, token)
            }
        }

    fn reregister<H>(&mut self,
                     event_loop: &mut mio::EventLoop<H>, token : mio::Token
                    )
        where H : mio::Handler {

            let io = self.io.borrow_mut();

            self.interest = match io.state {
                State::Running => panic!("wrong state"),
                State::BlockedOnRead => mio::Interest::readable(),
                State::BlockedOnWrite => mio::Interest::writable(),
                State::Finished => mio::Interest::hup(),
            };

            event_loop.reregister(
                &io.stream, token,
                self.interest, mio::PollOpt::edge() | mio::PollOpt::oneshot()
                ).ok().expect("reregister failed")
        }
}

impl io::Read for IOHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let res = {
                let mut io = self.io.borrow_mut();
                io.stream.try_read(buf)
            };
            match res {
                Ok(None) => {
                    self.io.borrow_mut().state = State::BlockedOnRead;
                    coroutine::Coroutine::block();
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

impl io::Write for IOHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let res = {
                let mut io = self.io.borrow_mut();
                io.stream.try_write(buf)
            };
            match res {
                Ok(None) => {
                    self.io.borrow_mut().state = State::BlockedOnWrite;
                    coroutine::Coroutine::block();
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

    /* TODO */
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

