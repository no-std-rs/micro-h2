//! Malformed messages and valid response phases, using independent h2 clients.

use std::time::Duration;

use micro_h2::frame::{FrameType, flags};
use micro_h2::{Connection, Error, Event, FrameHeader, frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;

fn wire(kind: FrameType, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0; frame::HEADER_LEN + payload.len()];
    frame::write_frame(kind, flags, stream, payload, &mut out).unwrap();
    out
}

fn block(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut encoder = fluke_hpack::Encoder::new();
    encoder.set_max_table_size(256);
    encoder.encode(
        fields
            .iter()
            .map(|(name, value)| (name.as_bytes(), value.as_bytes())),
    )
}

fn headers(fields: &[(&str, &str)], end: bool) -> Vec<u8> {
    wire(
        FrameType::Headers,
        flags::END_HEADERS | if end { flags::END_STREAM } else { 0 },
        1,
        &block(fields),
    )
}

fn connection(method: &str) -> Connection {
    let mut connection = Connection::new();
    connection
        .request(method, "/", "example.test", "http", &[], b"", &mut [0; 256])
        .unwrap();
    connection
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("message-validation peer stalled")
}

// Keep the raw peer alive until the response finishes. EOF must never manufacture
// the error we are trying to prove. A stream reset and a connection error are both
// valid rejection outcomes; the small client conservatively returns fatal Protocol.
async fn reference(method: &str, frames: &[Vec<u8>]) -> Result<(u16, Vec<u8>), h2::Error> {
    let (client_io, mut peer) = tokio::io::duplex(64 * 1024);
    let (mut sender, driver) = h2::client::handshake(client_io).await.unwrap();
    let driver = tokio::spawn(driver);
    let (response, _) = sender
        .send_request(
            http::Request::builder()
                .method(method)
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
        let mut prefix = [0; frame::HEADER_LEN];
        peer.read_exact(&mut prefix).await.unwrap();
        let header = FrameHeader::parse(&prefix).unwrap();
        let mut payload = vec![0; header.length];
        peer.read_exact(&mut payload).await.unwrap();
        if header.kind == FrameType::Headers {
            break;
        }
    }
    for bytes in frames {
        peer.write_all(bytes).await.unwrap();
    }
    let result = async {
        let response = response.await?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok((status, bytes))
    }
    .await;
    driver.abort();
    let _ = driver.await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_response_fields_match_independent_peer_rejections_in_parallel() {
    bounded(async {
        let cases = [
            vec![(":status", "200"), ("X", "y")],
            vec![(":status", "200"), ("x", "a\u{1}b")],
            vec![(":status", "200"), ("x", "a\u{7f}b")],
            vec![(":status", "200"), ("connection", "close")],
            vec![(":status", "200"), ("proxy-connection", "close")],
            vec![(":status", "200"), ("keep-alive", "yes")],
            vec![(":status", "200"), ("transfer-encoding", "chunked")],
            vec![(":status", "200"), ("upgrade", "websocket")],
            vec![(":status", "200"), ("te", "gzip")],
            vec![("x", "y"), (":status", "200")],
            vec![(":status", "200"), (":status", "200")],
            vec![(":status", "bad")],
            vec![(":status", "200"), ("content-length", "-1")],
            vec![
                (":status", "200"),
                ("content-length", "18446744073709551616"),
            ],
            vec![(":status", "200"), ("content-length", "1")],
        ];
        let mut tasks = JoinSet::new();
        for fields in cases {
            tasks.spawn(async move {
                let frame = headers(&fields, true);
                let mut calls = 0;
                assert_eq!(
                    connection("GET")
                        .recv(&frame, |_, _| calls += 1, &mut [0; 64])
                        .err(),
                    Some(Error::Protocol),
                    "{fields:?}"
                );
                assert_eq!(calls, 0, "malformed fields reached the callback");
                assert!(
                    reference("GET", &[frame]).await.is_err(),
                    "h2 accepted {fields:?}"
                );
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_streams_and_data_before_final_headers_match_the_reference_peer() {
    bounded(async {
        let mut tasks = JoinSet::new();
        for frame in [
            wire(FrameType::Data, flags::END_STREAM, 3, b"x"),
            wire(FrameType::Data, flags::END_STREAM, 2, b"x"),
            wire(FrameType::RstStream, 0, 3, &[0, 0, 0, 8]),
            wire(FrameType::WindowUpdate, 0, 3, &[0, 0, 0, 1]),
            wire(
                FrameType::Headers,
                flags::END_HEADERS,
                2,
                &block(&[(":status", "200")]),
            ),
            wire(
                FrameType::Headers,
                flags::END_HEADERS,
                3,
                &block(&[(":status", "200")]),
            ),
            wire(FrameType::Data, flags::END_STREAM, 1, b"x"),
        ] {
            tasks.spawn(async move {
                assert_eq!(
                    connection("GET")
                        .recv(&frame, |_, _| panic!("unexpected header"), &mut [0; 64])
                        .err(),
                    Some(Error::Protocol)
                );
                assert!(reference("GET", &[frame]).await.is_err());
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn response_body_length_mismatches_match_the_reference_peer() {
    bounded(async {
        let mut tasks = JoinSet::new();
        for (length, body) in [
            ("0", b"x".as_slice()),
            ("2", b"x".as_slice()),
            ("1", b"xx".as_slice()),
        ] {
            tasks.spawn(async move {
                let frames = [
                    headers(&[(":status", "200"), ("content-length", length)], false),
                    wire(FrameType::Data, flags::END_STREAM, 1, body),
                ];
                let mut connection = connection("GET");
                connection
                    .recv(&frames[0], |_, _| {}, &mut [0; 64])
                    .unwrap();
                assert_eq!(
                    connection.recv(&frames[1], |_, _| {}, &mut [0; 64]).err(),
                    Some(Error::Protocol)
                );
                assert!(reference("GET", &frames).await.is_err());
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await;
}

#[test]
fn generic_field_and_response_phase_rules_are_checked_before_callbacks() {
    // These RFC 9113 checks are explicit even where h2's HTTP field parser is
    // more permissive. The independent peer is not a replacement for the RFC.
    for fields in [
        vec![("x", "y")],
        vec![(":status", "200"), ("", "y")],
        vec![(":status", "200"), ("a:b", "y")],
        vec![(":status", "200"), ("x", "\0")],
        vec![(":status", "200"), ("x", "a\r\nb")],
        vec![(":status", "200"), ("x", " y")],
        vec![(":status", "200"), ("x", "y\t")],
        vec![(":status", "200"), (":method", "GET")],
        vec![(":status", "200"), (":unknown", "y")],
        vec![(":status", "101")],
        vec![(":status", "099")],
        vec![(":status", "103")],
        vec![(":status", "205"), ("content-length", "1")],
        vec![
            (":status", "200"),
            ("content-length", "1"),
            ("content-length", "2"),
        ],
        vec![(":status", "200"), ("te", "trailers")],
    ] {
        let mut calls = 0;
        assert_eq!(
            connection("GET")
                .recv(&headers(&fields, true), |_, _| calls += 1, &mut [0; 64])
                .err(),
            Some(Error::Protocol),
            "{fields:?}"
        );
        assert_eq!(calls, 0);
    }
    let mut connection = connection("GET");
    connection
        .recv(
            &headers(&[(":status", "103")], false),
            |_, _| {},
            &mut [0; 64],
        )
        .unwrap();
    assert_eq!(
        connection
            .recv(&wire(FrameType::Data, 0, 1, b"x"), |_, _| {}, &mut [0; 64])
            .err(),
        Some(Error::Protocol)
    );
}

#[test]
fn status_code_bounds_are_validated_before_delivery() {
    for status in ["600", "999"] {
        let mut calls = 0;
        assert_eq!(
            connection("GET")
                .recv(
                    &headers(&[(":status", status)], true),
                    |_, _| calls += 1,
                    &mut [0; 64]
                )
                .err(),
            Some(Error::Protocol)
        );
        assert_eq!(calls, 0);
    }
    for status in ["100", "199", "200", "299", "599"] {
        let mut seen = None;
        let end_stream = !status.starts_with('1');
        assert_eq!(
            connection("GET")
                .recv(
                    &headers(&[(":status", status)], end_stream),
                    |name, value| {
                        assert_eq!(name, ":status");
                        seen = Some(value.to_owned());
                    },
                    &mut [0; 64]
                )
                .unwrap()
                .0,
            Event::Headers {
                stream: 1,
                end_stream
            }
        );
        assert_eq!(seen.as_deref(), Some(status));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn informational_bodyless_and_padded_responses_remain_interoperable() {
    bounded(async {
        let mut tasks = JoinSet::new();
        for (method, status, length) in [
            ("HEAD", "200", "123"),
            ("GET", "204", "0"),
            ("GET", "205", "0"),
            ("GET", "304", "123"),
        ] {
            tasks.spawn(async move {
                let frames = [headers(
                    &[(":status", status), ("content-length", length)],
                    true,
                )];
                let mut connection = connection(method);
                assert!(matches!(
                    connection
                        .recv(&frames[0], |_, _| {}, &mut [0; 64])
                        .unwrap()
                        .0,
                    Event::Headers {
                        end_stream: true,
                        ..
                    }
                ));
                assert_eq!(connection.open_streams(), 0);
                assert_eq!(
                    reference(method, &frames).await.unwrap(),
                    (status.parse().unwrap(), vec![])
                );
            });
        }
        tasks.spawn(async {
            let frames = [
                headers(&[(":status", "103")], false),
                headers(&[(":status", "200"), ("content-length", "2")], false),
                wire(
                    FrameType::Data,
                    flags::PADDED | flags::END_STREAM,
                    1,
                    &[2, b'h', b'i', 0, 0],
                ),
            ];
            let mut connection = connection("GET");
            let mut statuses = Vec::new();
            for frame in &frames {
                connection
                    .recv(
                        frame,
                        |name, value| {
                            if name == ":status" {
                                statuses.push(value.to_owned());
                            }
                        },
                        &mut [0; 64],
                    )
                    .unwrap();
            }
            assert_eq!(statuses, ["103", "200"]);
            assert_eq!(connection.open_streams(), 0);
            assert_eq!(
                reference("GET", &frames).await.unwrap(),
                (200, b"hi".to_vec())
            );
        });
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await;
}

#[test]
fn invalid_requests_leave_stream_slots_identifiers_and_output_untouched() {
    for fields in [
        vec![("X", "y")],
        vec![(":status", "200")],
        vec![(":path", "/override")],
        vec![("connection", "close")],
        vec![("te", "gzip")],
        vec![("x", "bad\nvalue")],
        vec![("content-length", "0")],
        vec![("content-length", "2")],
    ] {
        let mut connection = Connection::new();
        let mut out = [0xaa; 256];
        for _ in 0..8 {
            assert_eq!(
                connection.request("POST", "/", "h", "https", &fields, b"x", &mut out),
                Err(Error::Protocol)
            );
            assert_eq!(connection.open_streams(), 0);
            assert_eq!(out, [0xaa; 256]);
        }
        assert_eq!(
            connection
                .request(
                    "POST",
                    "/",
                    "h",
                    "https",
                    &[("content-length", "1"), ("te", "trailers")],
                    b"x",
                    &mut out
                )
                .unwrap()
                .0,
            1
        );
    }
}

#[test]
fn invalid_schemes_leave_request_state_and_output_untouched() {
    for scheme in [
        "", "ht tp", "1http", "https://", "h_ttp", "http/", "üttps", "+http", "http\tx",
    ] {
        let mut connection = Connection::new();
        let mut out = [0xaa; 256];
        for _ in 0..8 {
            assert_eq!(
                connection.request("GET", "/", "example.test", scheme, &[], b"", &mut out),
                Err(Error::Protocol),
                "{scheme:?}"
            );
            assert_eq!(out, [0xaa; 256]);
            assert_eq!(connection.open_streams(), 0);
        }
        assert_eq!(
            connection
                .request("GET", "/", "example.test", "https", &[], b"", &mut out)
                .unwrap()
                .0,
            1
        );
    }
    for scheme in ["http", "HTTPS", "git+ssh", "x.y", "x-y", "h2", "a"] {
        let mut connection = Connection::new();
        let mut out = [0; 256];
        let (stream, len) = connection
            .request("GET", "/", "example.test", scheme, &[], b"", &mut out)
            .unwrap();
        assert_eq!(stream, 1);
        let mut emitted = None;
        micro_h2::hpack::Decoder::new(256)
            .decode(&out[frame::HEADER_LEN..len], |name, value| {
                if name == ":scheme" {
                    emitted = Some(value.to_owned());
                }
            })
            .unwrap();
        assert_eq!(emitted.as_deref(), Some(scheme));
    }
}

#[test]
fn duplicate_lengths_and_connect_pseudo_headers_are_unambiguous() {
    let mut connection = connection("GET");
    connection
        .recv(
            &headers(
                &[
                    (":status", "200"),
                    ("content-length", "1, 1"),
                    ("content-length", "01"),
                ],
                false,
            ),
            |_, _| {},
            &mut [0; 64],
        )
        .unwrap();
    connection
        .recv(
            &wire(FrameType::Data, flags::END_STREAM, 1, b"x"),
            |_, _| {},
            &mut [0; 64],
        )
        .unwrap();
    let mut connection = Connection::new();
    let mut out = [0; 256];
    let (_, len) = connection
        .request("CONNECT", "", "example.test:443", "", &[], b"", &mut out)
        .unwrap();
    let mut fields = Vec::new();
    micro_h2::hpack::Decoder::new(256)
        .decode(&out[frame::HEADER_LEN..len], |name, value| {
            fields.push((name.to_owned(), value.to_owned()))
        })
        .unwrap();
    assert_eq!(
        fields,
        [
            (":method".into(), "CONNECT".into()),
            (":authority".into(), "example.test:443".into())
        ]
    );
    for status in ["200", "204", "205"] {
        let mut tunnel = Connection::new();
        tunnel
            .request("CONNECT", "", "example.test:443", "", &[], b"", &mut out)
            .unwrap();
        tunnel
            .recv(
                &headers(&[(":status", status), ("content-length", "123")], false),
                |_, _| {},
                &mut out,
            )
            .unwrap();
        assert!(matches!(
            tunnel
                .recv(
                    &wire(FrameType::Data, flags::END_STREAM, 1, b"x"),
                    |_, _| {},
                    &mut out
                )
                .unwrap()
                .0,
            Event::Data {
                data: b"x",
                end_stream: true,
                ..
            }
        ));
        assert_eq!(tunnel.open_streams(), 0);
    }
    let mut connection = Connection::new();
    let (_, len) = connection
        .request(
            "POST",
            "/",
            "h",
            "http",
            &[
                ("content-length", "1, 1"),
                ("content-length", "01"),
                ("te", "TRAILERS"),
            ],
            b"x",
            &mut out,
        )
        .unwrap();
    let header = FrameHeader::parse(&out[..len]).unwrap();
    let mut fields = Vec::new();
    micro_h2::hpack::Decoder::new(256)
        .decode(
            &out[frame::HEADER_LEN..frame::HEADER_LEN + header.length],
            |name, value| {
                if name == "content-length" || name == "te" {
                    fields.push((name.to_owned(), value.to_owned()));
                }
            },
        )
        .unwrap();
    assert_eq!(
        fields,
        [
            ("content-length".into(), "1".into()),
            ("te".into(), "trailers".into())
        ]
    );
}

#[test]
fn closed_streams_discard_late_events_but_keep_hpack_and_connection_credit() {
    for reset in [false, true] {
        let mut connection = connection("GET");
        let mut out = [0; 256];
        connection
            .request("GET", "/second", "h", "https", &[], b"", &mut out)
            .unwrap();
        let mut encoder = fluke_hpack::Encoder::new();
        encoder.set_max_table_size(256);
        if reset {
            connection
                .recv(
                    &wire(FrameType::RstStream, 0, 1, &[0, 0, 0, 8]),
                    |_, _| {},
                    &mut out,
                )
                .unwrap();
        } else {
            let block = encoder.encode([
                (b":status".as_slice(), b"200".as_slice()),
                (b"a".as_slice(), b"old".as_slice()),
            ]);
            connection
                .recv(
                    &wire(
                        FrameType::Headers,
                        flags::END_HEADERS | flags::END_STREAM,
                        1,
                        &block,
                    ),
                    |_, _| {},
                    &mut out,
                )
                .unwrap();
        }
        let late = encoder.encode([
            (b":status".as_slice(), b"200".as_slice()),
            (b"x".as_slice(), b"late".as_slice()),
        ]);
        for frame in [
            wire(FrameType::Headers, 0, 1, &late[..1]),
            wire(FrameType::Continuation, flags::END_HEADERS, 1, &late[1..]),
            wire(FrameType::RstStream, 0, 1, &[0; 4]),
            wire(FrameType::WindowUpdate, 0, 1, &[0, 0, 0, 1]),
        ] {
            assert_eq!(
                connection
                    .recv(&frame, |_, _| panic!("late headers leaked"), &mut out)
                    .unwrap(),
                (Event::Nothing, 0)
            );
        }
        let late_data = wire(FrameType::Data, 0, 1, &[0; 16_384]);
        let mut credit = 0;
        for _ in 0..32 {
            let (event, written) = connection.recv(&late_data, |_, _| {}, &mut out).unwrap();
            assert_eq!(event, Event::Nothing);
            if written != 0 {
                let header = FrameHeader::parse(&out[..written]).unwrap();
                assert_eq!((header.kind, header.stream), (FrameType::WindowUpdate, 0));
                credit += u32::from_be_bytes(out[9..13].try_into().unwrap());
            }
        }
        assert_eq!(credit, micro_h2::conn::RECEIVE_WINDOW / 2);
        let next = encoder.encode([
            (b":status".as_slice(), b"200".as_slice()),
            (b"x".as_slice(), b"late".as_slice()),
        ]);
        let mut seen = Vec::new();
        connection
            .recv(
                &wire(
                    FrameType::Headers,
                    flags::END_HEADERS | flags::END_STREAM,
                    3,
                    &next,
                ),
                |n, v| seen.push((n.to_owned(), v.to_owned())),
                &mut out,
            )
            .unwrap();
        assert_eq!(
            seen,
            [
                (":status".into(), "200".into()),
                ("x".into(), "late".into())
            ]
        );
        assert_eq!(connection.open_streams(), 0);
    }
}

#[test]
fn streamed_data_enforces_the_same_state_and_content_length_as_recv() {
    let frame = wire(
        FrameType::Data,
        flags::END_STREAM | flags::PADDED,
        1,
        &[2, b'h', b'i', 0, 0],
    );
    let header = FrameHeader::parse(&frame).unwrap();
    let mut buffered = connection("GET");
    let mut streamed = connection("GET");
    let first = headers(&[(":status", "200"), ("content-length", "2")], false);
    for connection in [&mut buffered, &mut streamed] {
        connection.recv(&first, |_, _| {}, &mut [0; 64]).unwrap();
    }
    let mut out_a = [0; 64];
    let mut out_b = [0; 64];
    assert_eq!(
        buffered.recv(&frame, |_, _| {}, &mut out_a).unwrap().1,
        streamed
            .finish_data_with_length(header, 2, &mut out_b)
            .unwrap()
    );
    assert_eq!(out_a, out_b);
    assert_eq!(buffered.open_streams(), streamed.open_streams());
    assert_eq!(
        Connection::new().finish_data(
            FrameHeader {
                length: 1,
                kind: FrameType::Data,
                flags: 0,
                stream: 1
            },
            &mut out_b
        ),
        Err(Error::Protocol)
    );
    assert_eq!(
        connection("GET").finish_data(
            FrameHeader {
                length: 1,
                kind: FrameType::Data,
                flags: 0,
                stream: 1
            },
            &mut out_b
        ),
        Err(Error::Protocol)
    );
}

#[test]
fn streamed_data_retries_credit_without_committing_body_bytes_or_closure() {
    let length = micro_h2::conn::RECEIVE_WINDOW / 2;
    let text = length.to_string();
    let mut buffered = connection("GET");
    let mut streamed = connection("GET");
    let first = headers(&[(":status", "200"), ("content-length", &text)], false);
    for connection in [&mut buffered, &mut streamed] {
        connection.recv(&first, |_, _| {}, &mut [0; 64]).unwrap();
    }
    let data = [0; 16_384];
    let mut out_a = [0; 64];
    let mut out_b = [0; 64];
    for part in 0..32 {
        let frame = wire(
            FrameType::Data,
            if part == 31 { flags::END_STREAM } else { 0 },
            1,
            &data,
        );
        let header = FrameHeader::parse(&frame).unwrap();
        if part == 31 {
            assert_eq!(
                buffered.recv(&frame, |_, _| {}, &mut out_a[..13]).err(),
                Some(Error::BufferTooSmall)
            );
            assert_eq!(
                streamed.finish_data(header, &mut out_b[..13]),
                Err(Error::BufferTooSmall)
            );
            assert_eq!(buffered.open_streams(), 1);
            assert_eq!(streamed.open_streams(), 1);
        }
        let written = buffered.recv(&frame, |_, _| {}, &mut out_a).unwrap().1;
        assert_eq!(streamed.finish_data(header, &mut out_b).unwrap(), written);
        assert_eq!(out_a[..written], out_b[..written]);
    }
    assert_eq!(buffered.open_streams(), 0);
    assert_eq!(streamed.open_streams(), 0);
}

#[test]
fn bodyless_responses_and_unsupported_trailers_cannot_deliver_content() {
    for (method, status) in [
        ("HEAD", "200"),
        ("GET", "204"),
        ("GET", "205"),
        ("GET", "304"),
    ] {
        for streamed in [false, true] {
            let mut connection = connection(method);
            connection
                .recv(
                    &headers(&[(":status", status)], false),
                    |_, _| {},
                    &mut [0; 64],
                )
                .unwrap();
            let data = wire(FrameType::Data, flags::END_STREAM, 1, b"x");
            let error = if streamed {
                connection
                    .finish_data(FrameHeader::parse(&data).unwrap(), &mut [0; 64])
                    .err()
            } else {
                connection.recv(&data, |_, _| {}, &mut [0; 64]).err()
            };
            assert_eq!(
                error,
                Some(Error::Protocol),
                "{method} {status}, streamed={streamed}"
            );
        }
    }
    let mut connection = connection("GET");
    connection
        .recv(
            &headers(&[(":status", "200")], false),
            |_, _| {},
            &mut [0; 64],
        )
        .unwrap();
    let mut calls = 0;
    assert_eq!(
        connection
            .recv(
                &headers(&[("x", "trailer")], true),
                |_, _| calls += 1,
                &mut [0; 64]
            )
            .err(),
        Some(Error::Protocol)
    );
    assert_eq!(calls, 0);
}

#[tokio::test]
async fn normalized_request_fields_are_accepted_by_a_real_h2_server() {
    bounded(async {
        let (mut io, peer) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut connection = h2::server::handshake(peer).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            // Drive the transport while the request body and response are used.
            let driver = tokio::spawn(async move {
                let _ = connection.accept().await;
            });
            assert_eq!(
                request.headers().get_all("content-length").iter().count(),
                1
            );
            assert_eq!(request.headers()["content-length"], "1");
            assert_eq!(request.headers()["te"], "trailers");
            let mut body = request.into_body();
            assert_eq!(body.data().await.unwrap().unwrap().as_ref(), b"x");
            assert!(body.data().await.is_none());
            response
                .send_response(
                    http::Response::builder().status(200).body(()).unwrap(),
                    true,
                )
                .unwrap();
            driver.await.unwrap();
        });
        let mut connection = Connection::new();
        let mut out = [0; 256];
        let len = connection.start(&mut out).unwrap();
        io.write_all(&out[..len]).await.unwrap();
        let (_, len) = connection
            .request(
                "POST",
                "/",
                "example.test",
                "http",
                &[
                    ("content-length", "1, 1"),
                    ("content-length", "01"),
                    ("te", "TRAILERS"),
                ],
                b"x",
                &mut out,
            )
            .unwrap();
        io.write_all(&out[..len]).await.unwrap();
        loop {
            let mut prefix = [0; frame::HEADER_LEN];
            io.read_exact(&mut prefix).await.unwrap();
            let header = FrameHeader::parse(&prefix).unwrap();
            let mut frame = vec![0; frame::HEADER_LEN + header.length];
            frame[..frame::HEADER_LEN].copy_from_slice(&prefix);
            io.read_exact(&mut frame[frame::HEADER_LEN..])
                .await
                .unwrap();
            let mut status = None;
            let (event, written) = connection
                .recv(
                    &frame,
                    |name, value| {
                        if name == ":status" {
                            status = Some(value.to_owned());
                        }
                    },
                    &mut out,
                )
                .unwrap();
            if written != 0 {
                io.write_all(&out[..written]).await.unwrap();
            }
            if matches!(
                event,
                Event::Headers {
                    end_stream: true,
                    ..
                }
            ) {
                assert_eq!(status.as_deref(), Some("200"));
                break;
            }
        }
        drop(io);
        server.await.unwrap();
    })
    .await;
}
