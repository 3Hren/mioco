# mioco

<p align="center">
  <a href="https://travis-ci.org/dpc/mioco">
      <img src="https://img.shields.io/travis/dpc/mioco/master.svg?style=flat-square" alt="Build Status">
  </a>
  <a href="https://crates.io/crates/mioco">
      <img src="http://meritbadge.herokuapp.com/mioco?style=flat-square" alt="crates.io">
  </a>
  <a href="https://gitter.im/dpc/mioco">
      <img src="https://img.shields.io/badge/GITTER-join%20chat-green.svg?style=flat-square" alt="Gitter Chat">
  </a>
  <br>
  <strong><a href="//dpc.github.io/mioco/">Documentation</a></strong>
</p>


## Introduction

Scalable, asynchronous IO coroutine-based handling (aka MIO COroutines).

Using `mioco` you can handle scalable, asynchronous [`mio`][mio]-based IO, using set of synchronous-IO
handling functions. Based on asynchronous [`mio`][mio] events `mioco` will cooperatively schedule your
handlers.

You can think of `mioco` as of *Node.js for Rust* or *[green threads][green threads] on top of [`mio`][mio]*.

`mioco` is still very experimental, but already usable. For real-life project using
`mioco` see [colerr][colerr].

Read [Documentation](//dpc.github.io/mioco/) for details.

If you need help, try asking on [#mioco gitter.im][mioco gitter]. If still no
luck, try [rust user forum][rust user forum].

To report a bug or ask for features use [github issues][issues].

[rust]: http://rust-lang.org
[mio]: //github.com/carllerche/mio
[colerr]: //github.com/dpc/colerr
[mioco gitter]: https://gitter.im/dpc/mioco
[rust user forum]: https://users.rust-lang.org/
[issues]: //github.com/dpc/mioco/issues

## Building & running

Note: You must be using [nightly Rust][nightly rust] release. If you're using
[multirust][multirust], which is highly recommended, switch with `multirust default
nightly` command.

    cargo build --release
    make echo

[nightly rust]: https://doc.rust-lang.org/book/nightly-rust.html
[multirust]: https://github.com/brson/multirust

# Semi-benchmarks

`mioco` comes with tcp echo server example, that is being benchmarked here.

Beware: This is amateurish and naive comparison!

Using: https://gist.github.com/dpc/8cacd3b6fa5273ffdcce

```
# mioco echo example
% GOMAXPROCS=64 ./tcp_bench  -c=128 -t=10  -a="127.0.0.1:5555"
Benchmarking: 127.0.0.1:5555
128 clients, running 26 bytes, 10 sec.

Speed: 171022 request/sec, 171022 response/sec
Requests: 1710222
Responses: 1710221

# server_libev (simple libev based server):
% GOMAXPROCS=64 ./tcp_bench  -c=128 -t=10  -a="127.0.0.1:5000"
Benchmarking: 127.0.0.1:3100
128 clients, running 26 bytes, 10 sec.

Speed: 210485 request/sec, 210485 response/sec
Requests: 2104856
Responses: 2104854
```

Using: https://github.com/dpc/benchmark-echo

```
# rust mioco echo example :5555
Throughput: 148697.85 [reqests/sec], errors: 0
Throughput: 145282.09 [reqests/sec], errors: 0
Throughput: 157900.81 [reqests/sec], errors: 0
Throughput: 155722.21 [reqests/sec], errors: 0
Throughput: 160203.92 [reqests/sec], errors: 0

c ./server_libev 3100
Throughput: 192770.52 [reqests/sec], errors: 0
Throughput: 156105.10 [reqests/sec], errors: 0
Throughput: 162632.05 [reqests/sec], errors: 0
Throughput: 179868.05 [reqests/sec], errors: 0
Throughput: 187706.06 [reqests/sec], errors: 0
```
