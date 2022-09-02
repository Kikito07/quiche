// Copyright (C) 2019, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

#[macro_use]
extern crate log;

use std::net::ToSocketAddrs;
use std::str::FromStr;

use ring::rand::*;

const MAX_DATAGRAM_SIZE: usize = 1350;

const MAX_REQUEST_SIZE: usize = 50000000000;


const USAGE: &str = "Usage:
  generic-http3-client [options] ADDRESS PORT
  generic-http3-client -h | --help

Options:
  -X PATH                  The path of the keylog file on which to dump the TLS keys [default: ./keys.log]
  -G BYTES                 The size of the request to perform [default: 50000]
  -U                       If set, do an upload instead of a download.
  --wire-version VERSION   The version number to send to the server [default: 00000001].
  -verify                  If set, verifies the remote certificate
  -h --help                Show this screen.
";


fn main() {
    let mut buf = [0; 65535];
    let mut out = [0; MAX_DATAGRAM_SIZE];
    
    // Parse CLI parameters.
    let docopt = docopt::Docopt::new(USAGE).unwrap();
    let args: ClientArgs = ClientArgs::with_docopt(&docopt);
    let url = format!("https://{}:{}/{}", args.address, args.port, args.request_size);
    println!("{}", url);
    let url = url::Url::parse(url.as_str()).unwrap();

    let capacity = if args.upload { args.request_size } else { 1 };
    let mut random_upload_buffer =  Vec::<u8>::with_capacity(capacity);

    if args.upload {
        let mut tmp = Vec::<u8>::with_capacity(500000);
        for _ in 0..tmp.capacity() {
            let a: u8 = rand::random();
            tmp.push(a);
        }
        while random_upload_buffer.len() < args.request_size {
            random_upload_buffer.extend_from_slice(&tmp[0..std::cmp::min(tmp.len(), args.request_size - random_upload_buffer.len())])
        }
    }

    if args.request_size > MAX_REQUEST_SIZE {
        panic!("too large request size !");
    }

    // Setup the event loop.
    let poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    // Resolve server address.
    let peer_addr = url.to_socket_addrs().unwrap().next().unwrap();

    // Bind to INADDR_ANY or IN6ADDR_ANY depending on the IP family of the
    // server address. This is needed on macOS and BSD variants that don't
    // support binding to IN6ADDR_ANY for both v4 and v6.
    let bind_addr = match peer_addr {
        std::net::SocketAddr::V4(_) => "0.0.0.0:0",
        std::net::SocketAddr::V6(_) => "[::]:0",
    };

    // Create the UDP socket backing the QUIC connection, and register it with
    // the event loop.
    let socket = std::net::UdpSocket::bind(bind_addr).unwrap();

    let socket = mio::net::UdpSocket::from_socket(socket).unwrap();
    poll.register(
        &socket,
        mio::Token(0),
        mio::Ready::readable(),
        mio::PollOpt::edge(),
    )
    .unwrap();

    // Create the configuration for the QUIC connection.
    let mut config = quiche::Config::new(args.version).unwrap();

    // *CAUTION*: this should not be set to `false` in production!!!
    config.verify_peer(!args.no_verify);

    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();

    let keylog;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(args.keylog_path)
        .unwrap();

    keylog = Some(file);

    config.log_keys();


    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config.set_disable_active_migration(true);

    let mut http3_conn = None;

    // Generate a random source connection ID for the connection.
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    SystemRandom::new().fill(&mut scid[..]).unwrap();

    let scid = quiche::ConnectionId::from_ref(&scid);

    // Create a QUIC connection and initiate handshake.
    let local_addr = socket.local_addr().unwrap();
    let mut conn =
        quiche::connect(url.domain(), &scid, local_addr,peer_addr, &mut config).unwrap();


    if let Some(keylog) = &keylog {
        if let Ok(keylog) = keylog.try_clone() {
            conn.set_keylog(Box::new(keylog));
        }
    }

    info!(
        "connecting to {:} from {:} with scid {}",
        peer_addr,
        socket.local_addr().unwrap(),
        hex_dump(&scid)
    );

    let (write, send_info) = conn.send(&mut out).expect("initial send failed");

    while let Err(e) = socket.send_to(&out[..write], &send_info.to) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            debug!("send() would block");
            continue;
        }

        panic!("send() failed: {:?}", e);
    }

    debug!("written {}", write);

    let h3_config = quiche::h3::Config::new().unwrap();

    // Prepare request.
    let mut path = String::from(url.path());

    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }

    let post = b"POST";
    let get = b"GET";

    let method = if args.upload {
        &post[..]
    } else {
        &get[..]
    };

    let req = vec![
        quiche::h3::Header::new(b":method", method),
        quiche::h3::Header::new(b":scheme", url.scheme().as_bytes()),
        quiche::h3::Header::new(
            b":authority",
            url.host_str().unwrap().as_bytes(),
        ),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b"user-agent", b"quiche"),
    ];

    let req_start = std::time::Instant::now();
    
    let mut elapsed: i128 = -1;

    let mut total_bytes: usize = 0;

    let mut req_upload_bytes_sent = 0;
    let mut req_headers_sent = false;
    let mut req_sent = false;
    let mut post_request_stream_id = None;

    loop {
        poll.poll(&mut events, conn.timeout()).unwrap();

        // Read incoming UDP packets from the socket and feed them to quiche,
        // until there are no more packets to read.
        'read: loop {
            // If the event loop reported no events, it means that the timeout
            // has expired, so handle it without attempting to read packets. We
            // will then proceed with the send loop.
            if events.is_empty() {
                debug!("timed out");

                conn.on_timeout();

                break 'read;
            }

            let (len, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,

                Err(e) => {
                    // There are no more UDP packets to read, so end the read
                    // loop.
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        debug!("recv() would block");
                        break 'read;
                    }

                    panic!("recv() failed: {:?}", e);
                },
            };

            debug!("got {} bytes", len);

            let recv_info = quiche::RecvInfo {
                to: local_addr,
                from,
            };

            // Process potentially coalesced packets.
            let read = match conn.recv(&mut buf[..len], recv_info) {
                Ok(v) => v,

                Err(e) => {
                    error!("recv failed: {:?}", e);
                    continue 'read;
                },
            };

            debug!("processed {} bytes", read);
        }

        debug!("done reading");

        if conn.is_closed() {
            info!("connection closed, {:?}", conn.stats());
            break;
        }

        // Create a new HTTP/3 connection once the QUIC connection is established.
        if conn.is_established() && http3_conn.is_none() {
            http3_conn = Some(
                quiche::h3::Connection::with_transport(&mut conn, &h3_config)
                    .unwrap(),
            );
        }

        // Send HTTP requests once the QUIC connection is established, and until
        // all requests have been sent.
        if let Some(h3_conn) = &mut http3_conn {
            if !req_sent {
                info!("sending HTTP request {:?}", req);
                if !req_headers_sent {
                    post_request_stream_id = Some(h3_conn.send_request(&mut conn, &req, false).unwrap());
                    req_headers_sent = true;
                }

                if args.upload {
                    if args.request_size > 0 && args.request_size < MAX_REQUEST_SIZE && req_upload_bytes_sent < args.request_size {
                        if let Some(stream_id) = post_request_stream_id {
                            match h3_conn.send_body(&mut conn, stream_id, &random_upload_buffer[req_upload_bytes_sent..], true) {
                                Ok(n) => req_upload_bytes_sent += n,
                                Err(quiche::h3::Error::Done) => {},
                                Err(e) => panic!("error in send_body: {:?}", e),
                            };
                            info!("{}/{}", req_upload_bytes_sent, args.request_size);
                        }
                    }
                }
                if req_upload_bytes_sent == args.request_size {
                    req_sent = true;
                }
            }
        }

        if let Some(http3_conn) = &mut http3_conn {
            // Process HTTP/3 events.
            loop {
                match http3_conn.poll(&mut conn) {
                    Ok((stream_id, quiche::h3::Event::Headers { list, .. })) => {
                        info!(
                            "got response headers {:?} on stream id {}",
                            list, stream_id
                        );
                    },

                    Ok((stream_id, quiche::h3::Event::Data)) => {
                        while let Ok(read) =
                            http3_conn.recv_body(&mut conn, stream_id, &mut buf)
                        {
                            debug!(
                                "got {} bytes of response data on stream {}",
                                read, stream_id
                            );

                            total_bytes += read;

                            if args.upload {
                                println!("server reported receiving {} bytes", u64::from_str(unsafe { std::str::from_utf8_unchecked(&buf[..read]) } ).unwrap())
                            }

                        }
                    },

                    Ok((_stream_id, quiche::h3::Event::Finished)) => {
                        elapsed = req_start.elapsed().as_micros() as i128;
                        info!(
                            "response received in {:?}, closing...",
                            req_start.elapsed()
                        );

                        conn.close(true, 0x00, b"kthxbye").unwrap();
                    },

                    Ok((_stream_id, quiche::h3::Event::Reset(e))) => {
                        error!(
                            "request was reset by peer with {}, closing...",
                            e
                        );

                        conn.close(true, 0x00, b"kthxbye").unwrap();
                    },

                    Ok((_flow_id, quiche::h3::Event::Datagram)) => (),

                    Ok((_, quiche::h3::Event::PriorityUpdate)) => unreachable!(),

                    Ok((goaway_id, quiche::h3::Event::GoAway)) => {
                        info!("GOAWAY id={}", goaway_id);
                    },

                    Err(quiche::h3::Error::Done) => {
                        break;
                    },

                    Err(e) => {
                        error!("HTTP/3 processing failed: {:?}", e);

                        break;
                    },
                }
            }
        }

        // Generate outgoing QUIC packets and send them on the UDP socket, until
        // quiche reports that there are no more packets to be sent.
        loop {
            let (write, send_info) = match conn.send(&mut out) {
                Ok(v) => v,

                Err(quiche::Error::Done) => {
                    debug!("done writing");
                    break;
                },

                Err(e) => {
                    error!("send failed: {:?}", e);

                    conn.close(false, 0x1, b"fail").ok();
                    break;
                },
            };

            if let Err(e) = socket.send_to(&out[..write], &send_info.to) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    debug!("send() would block");
                    break;
                }

                panic!("send() failed: {:?}", e);
            }

            debug!("written {}", write);
        }

        if conn.is_closed() {
            info!("connection closed, {:?}", conn.stats());
            break;
        }
    }
    println!("got {} bytes in total", total_bytes);
    println!("{} ms", (elapsed as f64)/1000.0);
    println!("goodput : {} Mbps", ((total_bytes as f64) * 8.0)/((elapsed as f64)));
    println!("done!!!");
}

fn hex_dump(buf: &[u8]) -> String {
    let vec: Vec<String> = buf.iter().map(|b| format!("{:02x}", b)).collect();

    vec.join("")
}


/// Application-specific arguments that compliment the `CommonArgs`.
struct ClientArgs {
    version: u32,
    request_size: usize,
    upload: bool,
    no_verify: bool,
    keylog_path: String,
    address: String,
    port: u16,
}

pub trait Args {
    fn with_docopt(docopt: &docopt::Docopt) -> Self;
}


impl Args for ClientArgs {
    fn with_docopt(docopt: &docopt::Docopt) -> Self {
        let args = docopt.parse().unwrap_or_else(|e| e.exit());

        let version = args.get_str("--wire-version");
        let version = u32::from_str_radix(version, 16).unwrap();

        let request_size = args.get_str("-G");
        let request_size = usize::from_str_radix(request_size, 10).unwrap();

        let keylog_path = args.get_str("-X").to_string();

        let no_verify = !args.get_bool("-verify");

        let upload = args.get_bool("-U");

        let address = args.get_str("ADDRESS").to_string();
        let port = args.get_str("PORT");
        let port = u16::from_str_radix(port, 10).unwrap();
        ClientArgs {
            version,
            request_size,
            no_verify,
            upload,
            keylog_path,
            address,
            port,
        }
    }
}
