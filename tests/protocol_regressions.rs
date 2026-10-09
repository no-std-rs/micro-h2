//! Public-API regressions against real h2 peers, with independent connections
//! and every supported stream slot active at the same time.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use micro_h2::conn::{MAX_STREAMS, RECEIVE_WINDOW};
use micro_h2::frame::{FrameType, flags};
use micro_h2::{Connection, Error, Event, FrameHeader, frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::Barrier;
use tokio::task::{JoinHandle, JoinSet};

const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTIONS: usize = 8;
const WAVES: usize = 2;
const REQUEST_SIZE: usize = 15 * 1024;
const RESPONSE_SIZE: usize = RECEIVE_WINDOW as usize + REQUEST_SIZE;

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("protocol regression timed out")
}

fn wire(kind: FrameType, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; frame::HEADER_LEN + payload.len()];
    frame::write_frame(kind, flags, stream, payload, &mut bytes).unwrap();
    bytes
}

async fn read_frame(io: &mut DuplexStream) -> Vec<u8> {
    let mut prefix = [0; frame::HEADER_LEN];
    io.read_exact(&mut prefix).await.expect("frame header");
    let header = FrameHeader::parse(&prefix).unwrap();
    assert!(header.length <= frame::DEFAULT_MAX_FRAME);
    let mut bytes = vec![0; frame::HEADER_LEN + header.length];
    bytes[..frame::HEADER_LEN].copy_from_slice(&prefix);
    io.read_exact(&mut bytes[frame::HEADER_LEN..])
        .await
        .expect("frame payload");
    bytes
}

#[derive(Default)]
struct Response {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    complete: bool,
}

struct Client {
    connection: Connection,
    io: DuplexStream,
    updates: Vec<(u32, u32)>,
    retry_credit: bool,
    credit_retries: usize,
    compressed_headers: usize,
}

impl Client {
    async fn start(mut io: DuplexStream) -> Self {
        let mut connection = Connection::new();
        let mut out = [0; 128];
        let written = connection.start(&mut out).unwrap();
        io.write_all(&out[..written]).await.unwrap();
        let mut client = Self {
            connection,
            io,
            updates: Vec::new(),
            retry_credit: false,
            credit_retries: 0,
            compressed_headers: 0,
        };
        // Process the reference peer's negotiated settings before opening streams.
        let mut settings = false;
        let mut acknowledged = false;
        while !settings || !acknowledged {
            let (header, _) = client.receive().await;
            if header.kind == FrameType::Settings {
                if header.has(flags::ACK) {
                    acknowledged = true;
                } else {
                    settings = true;
                }
            }
        }
        client
    }

    fn try_request(
        &mut self,
        path: &str,
        body: &[u8],
        out: &mut [u8],
    ) -> Result<(u32, usize), Error> {
        self.connection.request(
            if body.is_empty() { "GET" } else { "POST" },
            path,
            "example.test",
            "http",
            &[],
            body,
            out,
        )
    }

    async fn request(&mut self, path: &str, body: &[u8]) -> u32 {
        let mut out = vec![0; body.len() + 256];
        let (stream, written) = loop {
            match self.try_request(path, body, &mut out) {
                Ok(request) => break request,
                Err(Error::FlowControl) => {
                    // Credits may follow the previous batch's final response.
                    // The response barrier prevents this from discarding new response bytes.
                    let (header, response) = self.receive().await;
                    assert!(matches!(
                        header.kind,
                        FrameType::WindowUpdate | FrameType::Settings
                    ));
                    assert!(response.headers.is_empty() && response.body.is_empty());
                }
                Err(error) => panic!("request {path}: {error:?}"),
            }
        };
        self.io.write_all(&out[..written]).await.unwrap();
        stream
    }

    async fn receive(&mut self) -> (FrameHeader, Response) {
        let bytes = read_frame(&mut self.io).await;
        let header = FrameHeader::parse(&bytes).unwrap();
        let mut response = Response::default();
        let mut out = [0; 64];
        let capacity = if self.retry_credit && header.kind == FrameType::Data {
            // One WINDOW_UPDATE fits; a simultaneous connection/stream pair does not.
            13
        } else {
            out.len()
        };
        let result = self.connection.recv(
            &bytes,
            |name, value| response.headers.push((name.to_owned(), value.to_owned())),
            &mut out[..capacity],
        );
        let (event, written) = match result {
            Err(Error::BufferTooSmall) if self.retry_credit && header.kind == FrameType::Data => {
                self.credit_retries += 1;
                self.connection.recv(&bytes, |_, _| {}, &mut out).unwrap()
            }
            result => result.expect("micro-h2 accepts the independent peer's frame"),
        };
        match event {
            Event::Data {
                data, end_stream, ..
            } => {
                response.body.extend_from_slice(data);
                response.complete = end_stream;
            }
            Event::Headers { end_stream, .. } => {
                response.complete = end_stream;
                if header.length <= 3 {
                    self.compressed_headers += 1;
                }
            }
            Event::Reset { stream, code } => panic!("unexpected reset on {stream}: {code}"),
            Event::GoAway { code } => panic!("unexpected GOAWAY: {code}"),
            Event::Nothing => {}
        }
        let mut offset = 0;
        while offset < written {
            let reply = FrameHeader::parse(&out[offset..written]).unwrap();
            if reply.kind == FrameType::WindowUpdate {
                let increment =
                    u32::from_be_bytes(out[offset + 9..offset + 13].try_into().unwrap());
                self.updates.push((reply.stream, increment));
            }
            offset += frame::HEADER_LEN + reply.length;
        }
        self.io.write_all(&out[..written]).await.unwrap();
        (header, response)
    }

    async fn collect(&mut self, streams: &[u32]) -> BTreeMap<u32, Response> {
        let mut responses: BTreeMap<_, _> = streams
            .iter()
            .map(|&id| (id, Response::default()))
            .collect();
        while responses.values().any(|response| !response.complete) {
            let (header, received) = self.receive().await;
            if let Some(response) = responses.get_mut(&header.stream) {
                assert!(!response.complete, "response continued after END_STREAM");
                response.headers.extend(received.headers);
                response.body.extend(received.body);
                response.complete = received.complete;
            }
        }
        responses
    }
}

fn request_body(connection: usize, wave: usize, slot: usize) -> Vec<u8> {
    (0..REQUEST_SIZE)
        .map(|index| ((index + connection * 17 + wave * 13 + slot * 29) % 251) as u8)
        .collect()
}

fn echo_server(
    io: DuplexStream,
    long_value: String,
    ready: Arc<Barrier>,
    initial_window: u32,
    stream_limit: usize,
    response_size: usize,
) -> JoinHandle<usize> {
    tokio::spawn(async move {
        let mut connection = h2::server::Builder::new()
            .initial_window_size(initial_window)
            .max_concurrent_streams(stream_limit as u32)
            .handshake(io)
            .await
            .unwrap();
        let mut handlers = JoinSet::new();
        let mut accepted = 0;
        loop {
            tokio::select! {
                request = connection.accept() => {
                    let Some(request) = request else { break };
                    let (request, mut sender) = request.unwrap();
                    accepted += 1;
                    let ready = ready.clone();
                    let value = long_value.clone();
                    handlers.spawn(async move {
                        let path = request.uri().path().to_owned();
                        let mut received = request.into_body();
                        let mut body = Vec::new();
                        while let Some(chunk) = received.data().await {
                            let chunk = chunk.unwrap();
                            body.extend_from_slice(&chunk);
                            received.flow_control().release_capacity(chunk.len()).unwrap();
                        }
                        if response_size != 0 {
                            let parts: Vec<_> = path.split('/').collect();
                            assert_eq!(parts[1], "round");
                            let coordinates: Vec<usize> = parts[2..].iter().map(|part| part.parse().unwrap()).collect();
                            assert_eq!(body, request_body(coordinates[0], coordinates[1], coordinates[2]));
                        } else {
                            assert!(body.is_empty());
                        }
                        // No response starts until every participating stream has received its request.
                        ready.wait().await;
                        let response = http::Response::builder().status(200).header("x", value).body(()).unwrap();
                        let mut send = sender.send_response(response, response_size == 0).unwrap();
                        if response_size != 0 {
                            let mut remaining = Bytes::from(body.iter().copied().cycle().take(response_size).collect::<Vec<_>>());
                            while !remaining.is_empty() {
                                send.reserve_capacity(remaining.len().min(8192));
                                let capacity = std::future::poll_fn(|cx| send.poll_capacity(cx)).await.unwrap().unwrap();
                                assert!(capacity > 0);
                                let chunk = remaining.split_to(capacity.min(remaining.len()));
                                send.send_data(chunk, remaining.is_empty()).unwrap();
                                tokio::task::yield_now().await;
                            }
                        }
                    });
                }
                Some(handler) = handlers.join_next(), if !handlers.is_empty() => {
                    handler.expect("echo task panicked");
                }
            }
        }
        while let Some(handler) = handlers.join_next().await {
            handler.expect("echo task panicked");
        }
        accepted
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round_trips_fill_every_stream_slot_on_eight_parallel_connections() {
    bounded(async {
        let ready = Arc::new(Barrier::new(CONNECTIONS * MAX_STREAMS));
        let mut connections = JoinSet::new();
        for (connection, length) in [127, 128, 129, 130, 144, 159, 200, 223]
            .into_iter()
            .enumerate()
        {
            let ready = ready.clone();
            connections.spawn(async move {
                let value = "z".repeat(length);
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                let server = echo_server(
                    server_io,
                    value.clone(),
                    ready,
                    65_535,
                    MAX_STREAMS,
                    RESPONSE_SIZE,
                );
                let mut client = Client::start(client_io).await;
                // Repeat failures past the slot bound, then complete real peer exchanges.
                for _ in 0..MAX_STREAMS + 1 {
                    assert_eq!(
                        client.try_request("/failed", b"body", &mut []),
                        Err(Error::BufferTooSmall)
                    );
                }
                assert_eq!(client.connection.open_streams(), 0);
                for wave in 0..WAVES {
                    let mut streams = Vec::new();
                    let mut sent = BTreeMap::new();
                    for slot in 0..MAX_STREAMS {
                        let body = request_body(connection, wave, slot);
                        let stream = client
                            .request(&format!("/round/{connection}/{wave}/{slot}"), &body)
                            .await;
                        assert_eq!(stream, ((wave * MAX_STREAMS + slot) * 2 + 1) as u32);
                        streams.push(stream);
                        sent.insert(stream, body);
                    }
                    assert_eq!(client.connection.open_streams(), MAX_STREAMS);
                    assert_eq!(
                        client.try_request("/overflow", b"", &mut [0; 128]),
                        Err(Error::TooManyStreams)
                    );
                    for (stream, response) in client.collect(&streams).await {
                        assert_eq!(
                            response.headers,
                            [
                                (":status".to_owned(), "200".to_owned()),
                                ("x".to_owned(), value.clone())
                            ]
                        );
                        assert_eq!(response.body.len(), RESPONSE_SIZE);
                        let request = &sent[&stream];
                        assert!(
                            response
                                .body
                                .iter()
                                .enumerate()
                                .all(|(index, byte)| *byte == request[index % request.len()]),
                            "connection {connection}, stream {stream}: echo corruption"
                        );
                    }
                    assert_eq!(client.connection.open_streams(), 0);
                    for stream in streams {
                        assert!(
                            client.updates.iter().any(|(id, _)| *id == stream),
                            "no stream credit for {stream}"
                        );
                    }
                }
                assert!(client.updates.iter().any(|(id, _)| *id == 0));
                // h2 indexes entries up to 3/4 of the advertised table; prove those cases use it.
                if length <= 159 {
                    assert!(
                        client.compressed_headers > 0,
                        "reference peer never used a compact indexed block"
                    );
                }
                drop(client);
                assert_eq!(server.await.unwrap(), WAVES * MAX_STREAMS);
            });
        }
        while let Some(connection) = connections.join_next().await {
            connection.expect("parallel connection panicked");
        }
    })
    .await;
}

#[tokio::test]
async fn negotiated_zero_body_window_and_one_stream_limit_allow_retries() {
    bounded(async {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = echo_server(
            server_io,
            "zero-window".into(),
            Arc::new(Barrier::new(1)),
            0,
            1,
            0,
        );
        let mut client = Client::start(client_io).await;
        for _ in 0..MAX_STREAMS + 1 {
            assert_eq!(
                client.try_request("/blocked", b"x", &mut [0; 128]),
                Err(Error::FlowControl)
            );
        }
        for expected_stream in [1, 3] {
            let stream = client.request("/empty", b"").await;
            assert_eq!(stream, expected_stream);
            assert_eq!(
                client.try_request("/too-many", b"", &mut [0; 128]),
                Err(Error::TooManyStreams)
            );
            let responses = client.collect(&[stream]).await;
            assert!(responses[&stream].body.is_empty());
            assert!(
                responses[&stream]
                    .headers
                    .contains(&(":status".into(), "200".into()))
            );
            assert_eq!(client.connection.open_streams(), 0);
        }
        drop(client);
        assert_eq!(server.await.unwrap(), 2);
    })
    .await;
}

#[tokio::test]
async fn retrying_a_short_credit_buffer_finishes_a_real_flow_controlled_echo() {
    bounded(async {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = echo_server(
            server_io,
            "credit".into(),
            Arc::new(Barrier::new(1)),
            65_535,
            1,
            RESPONSE_SIZE,
        );
        let mut client = Client::start(client_io).await;
        client.retry_credit = true;
        let body = request_body(0, 0, 0);
        let stream = client.request("/round/0/0/0", &body).await;
        let responses = client.collect(&[stream]).await;
        let response = &responses[&stream];
        assert_eq!(response.body.len(), RESPONSE_SIZE);
        assert!(
            response
                .body
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte == body[index % body.len()])
        );
        assert!(
            client.credit_retries >= 2,
            "did not cross both credit thresholds"
        );
        let connection_credit: Vec<_> = client
            .updates
            .iter()
            .filter(|(id, _)| *id == 0)
            .map(|(_, credit)| *credit)
            .collect();
        let stream_credit: Vec<_> = client
            .updates
            .iter()
            .filter(|(id, _)| *id == stream)
            .map(|(_, credit)| *credit)
            .collect();
        assert_eq!(connection_credit, stream_credit);
        assert_eq!(connection_credit.iter().sum::<u32>(), RECEIVE_WINDOW);
        drop(client);
        assert_eq!(server.await.unwrap(), 1);
    })
    .await;
}

#[test]
fn fragmented_indexed_headers_finish_each_stream_without_leaking_continuation_state() {
    std::thread::scope(|scope| {
        for length in [127, 128, 129, 130, 144, 159, 200, 223] {
            scope.spawn(move || {
                let mut connection = Connection::new();
                let mut encoder = fluke_hpack::Encoder::new();
                encoder.set_max_table_size(256);
                let mut reference = fluke_hpack::Decoder::new();
                reference.set_max_table_size(256);
                let streams: Vec<_> = (0..MAX_STREAMS)
                    .map(|_| {
                        connection
                            .request(
                                "GET",
                                "/fragmented",
                                "example.test",
                                "http",
                                &[],
                                b"",
                                &mut [0; 256],
                            )
                            .unwrap()
                            .0
                    })
                    .collect();
                let value = "z".repeat(length);
                for (slot, stream) in streams.into_iter().enumerate() {
                    // Keep an older entry so a skipped long insertion changes index 62's meaning.
                    let field = if slot == 0 {
                        (b"a".as_slice(), b"old".as_slice())
                    } else {
                        (b"x".as_slice(), value.as_bytes())
                    };
                    let block = encoder.encode([(b":status".as_slice(), b"200".as_slice()), field]);
                    let expected: Vec<_> = reference
                        .decode(&block)
                        .unwrap()
                        .into_iter()
                        .map(|(name, value)| {
                            (
                                String::from_utf8(name).unwrap(),
                                String::from_utf8(value).unwrap(),
                            )
                        })
                        .collect();
                    let last = block.len() - 1;
                    let fragments = [
                        wire(FrameType::Headers, flags::END_STREAM, stream, &block[..1]),
                        wire(FrameType::Continuation, 0, stream, &block[1..last]),
                        wire(
                            FrameType::Continuation,
                            flags::END_HEADERS,
                            stream,
                            &block[last..],
                        ),
                    ];
                    let mut decoded = Vec::new();
                    for (part, bytes) in fragments.iter().enumerate() {
                        let (event, written) = connection
                            .recv(
                                bytes,
                                |name, value| decoded.push((name.to_owned(), value.to_owned())),
                                &mut [0; 64],
                            )
                            .unwrap();
                        assert_eq!(written, 0);
                        if part < 2 {
                            assert!(matches!(event, Event::Nothing));
                            assert!(decoded.is_empty(), "headers escaped before END_HEADERS");
                        } else {
                            assert!(matches!(
                                event,
                                Event::Headers { stream: id, end_stream: true } if id == stream
                            ));
                            assert_eq!(decoded, expected);
                        }
                    }
                    assert_eq!(connection.open_streams(), MAX_STREAMS - slot - 1);
                }
            });
        }
    });
}

/// Feed the identical malicious frames to a real h2 client. The raw peer is
/// deliberately independent of micro-h2's connection/HPACK state machine.
async fn reference_rejects(frames: Vec<Vec<u8>>) {
    let (client_io, mut peer) = tokio::io::duplex(64 * 1024);
    let (mut sender, connection) = h2::client::handshake(client_io).await.unwrap();
    let driver = tokio::spawn(connection);
    let (response, _) = sender
        .send_request(
            http::Request::builder()
                .uri("http://example.test/")
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    let mut preface = [0; 24];
    peer.read_exact(&mut preface).await.unwrap();
    assert_eq!(preface, frame::CLIENT_PREFACE);
    peer.write_all(&wire(FrameType::Settings, 0, 0, &[]))
        .await
        .unwrap();
    loop {
        let bytes = read_frame(&mut peer).await;
        if FrameHeader::parse(&bytes).unwrap().kind == FrameType::Headers {
            break;
        }
    }
    for bytes in frames {
        peer.write_all(&bytes).await.unwrap();
    }
    let error = driver
        .await
        .unwrap()
        .expect_err("h2 accepted the malformed peer");
    // h2 0.4 normalizes these decoder failures to a connection PROTOCOL_ERROR.
    assert_eq!(error.reason(), Some(h2::Reason::PROTOCOL_ERROR));
    assert!(response.await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_and_control_frame_regressions_match_the_reference_peer() {
    bounded(async {
        let mut cases = JoinSet::new();
        let first = wire(FrameType::Headers, 0, 1, &[0x88]);
        for interloper in [
            wire(FrameType::Continuation, flags::END_HEADERS, 3, &[0x88]),
            wire(FrameType::Ping, 0, 0, &[0; 8]),
            wire(FrameType::Headers, flags::END_HEADERS, 3, &[0x88]),
        ] {
            let first = first.clone();
            cases.spawn(async move {
                let mut connection = Connection::new();
                connection
                    .request("GET", "/", "example.test", "http", &[], b"", &mut [0; 256])
                    .unwrap();
                connection.recv(&first, |_, _| {}, &mut [0; 64]).unwrap();
                assert_eq!(
                    connection.recv(&interloper, |_, _| {}, &mut [0; 64]).err(),
                    Some(Error::Protocol)
                );
                reference_rejects(vec![first, interloper]).await;
            });
        }
        for bytes in [
            wire(FrameType::Continuation, flags::END_HEADERS, 1, &[0x88]),
            wire(FrameType::Settings, flags::ACK, 0, &[0; 6]),
            wire(FrameType::Settings, 0, 1, &[]),
            wire(FrameType::Ping, 0, 0, &[0; 7]),
            wire(FrameType::RstStream, 0, 0, &[0; 4]),
            wire(FrameType::WindowUpdate, 0, 0, &[0; 4]),
        ] {
            cases.spawn(async move {
                assert_eq!(
                    Connection::new()
                        .recv(&bytes, |_, _| {}, &mut [0; 64])
                        .err(),
                    Some(Error::Protocol)
                );
                reference_rejects(vec![bytes]).await;
            });
        }
        while let Some(case) = cases.join_next().await {
            case.unwrap();
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_hpack_blocks_are_rejected_by_micro_h2_and_the_reference_peer() {
    bounded(async {
        let mut overflow = vec![0x88, 0x00, 1, b'x', 0x7f];
        overflow.extend_from_slice(&[0x80; 9]);
        overflow.push(0x02);
        overflow.extend_from_slice(&[b'z'; 127]);
        assert!(fluke_hpack::Decoder::new().decode(&overflow).is_err());
        let mut cases = JoinSet::new();
        for block in [overflow, vec![0x88, 0x20]] {
            cases.spawn(async move {
                let bytes = wire(
                    FrameType::Headers,
                    flags::END_HEADERS | flags::END_STREAM,
                    1,
                    &block,
                );
                let mut connection = Connection::new();
                connection
                    .request("GET", "/", "example.test", "http", &[], b"", &mut [0; 256])
                    .unwrap();
                assert_eq!(
                    connection.recv(&bytes, |_, _| {}, &mut [0; 64]).err(),
                    Some(Error::Hpack)
                );
                reference_rejects(vec![bytes]).await;
            });
        }
        while let Some(case) = cases.join_next().await {
            case.unwrap();
        }
    })
    .await;
}
