//! #72 acceptance: the bridge over real HTTP against an in-process daemon
//! -- loopback, GET-only, same-origin, no mutation endpoint; the shared
//! catalogue; status / inspect / relations / impact parity with direct
//! daemon queries and the CLI rendering; continuation; unsupported never
//! a zero; disconnected / incompatible; the daemon outliving the server.

use std::fs;

use brainprint_core::{
    PROTOCOL_VERSION,
    present::{self, Locale, Msg, text},
    protocol::{EndpointPaths, HandshakeResponse, Listener, Request, Response, framing, query::*},
};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use super::{api, listen, serve};
use crate::{
    client,
    surface::{Daemon, compact, delivery},
    tui::tests::{TestDir, start, stop, workspace},
};

/// Start the bridge on a free loopback port; returns its port.
async fn bridge(daemon: Daemon) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = listen(0).await.expect("listen");
    let address = listener.local_addr().expect("address");
    assert!(address.ip().is_loopback(), "{address}");
    let task = tokio::spawn(serve(listener, daemon, Locale::En));
    (address.port(), task)
}

/// One raw HTTP/1.1 request: (status code, body).
async fn http(port: u16, method: &str, path: &str, headers: &[(&str, String)]) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    let mut host = format!("127.0.0.1:{port}");
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            host.clone_from(value);
        } else {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    request.push_str(&format!("Host: {host}\r\n\r\n"));
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("head");
    let code = head
        .split(' ')
        .nth(1)
        .expect("status")
        .parse()
        .expect("code");
    (code, body.to_owned())
}

async fn get(port: u16, path: &str) -> Value {
    let (code, body) = http(port, "GET", path, &[]).await;
    assert_eq!(code, 200, "{path}: {body}");
    serde_json::from_str(&body).expect("json")
}

fn data(reply: &Value) -> &Value {
    assert_eq!(reply["connection"], "connected", "{reply}");
    &reply["data"]
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn lines(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line.as_str().expect("line").to_owned())
        .collect()
}

#[tokio::test]
async fn the_bridge_is_loopback_get_only_and_same_origin() {
    let runtime_root = TestDir::create("web-security");
    let daemon = Daemon {
        endpoint: EndpointPaths::from_runtime_root(runtime_root.0.clone()),
        workspace: runtime_root.0.to_string_lossy().into_owned(),
        config: None,
    };
    let (port, server) = bridge(daemon).await;

    let (code, page) = http(port, "GET", "/", &[]).await;
    assert_eq!(code, 200);
    assert!(page.starts_with("<!doctype html>"), "{}", &page[..40]);
    assert!(!page.contains(" src=\""), "the page is self-contained");

    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        assert_eq!(
            http(port, method, "/api/status", &[]).await.0,
            405,
            "{method}"
        );
    }
    for path in [
        "/api/sync",
        "/api/rebuild",
        "/api/uninit",
        "/api/doctor",
        "/api/work/start",
        "/api/source",
        "/api/exec",
        "/../etc/passwd",
    ] {
        assert_eq!(http(port, "GET", path, &[]).await.0, 404, "{path}");
    }
    let foreign = [
        ("Host", "evil.example".to_owned()),
        ("Host", format!("evil.example:{port}")),
        ("Origin", "http://evil.example".to_owned()),
        (
            "Origin",
            format!("http://127.0.0.1:{}", port.wrapping_add(1)),
        ),
    ];
    for header in foreign {
        assert_eq!(
            http(port, "GET", "/api/catalogue", std::slice::from_ref(&header))
                .await
                .0,
            403,
            "{header:?}"
        );
    }
    let own = ("Origin", format!("http://localhost:{port}"));
    let localhost = ("Host", format!("localhost:{port}"));
    assert_eq!(
        http(port, "GET", "/api/catalogue", &[own, localhost])
            .await
            .0,
        200
    );

    // A browser-held target is only ever an id the bridge handed out.
    let path_target = encode(r#"{"Resource":{"Path":"/etc/passwd"}}"#);
    let (code, _) = http(
        port,
        "GET",
        &format!("/api/inspect?target={path_target}"),
        &[],
    )
    .await;
    assert_eq!(code, 400);
    server.abort();
}

/// A browser's idle preconnect must not hold up a real request.
#[tokio::test]
async fn an_idle_connection_never_blocks_a_request() {
    let runtime_root = TestDir::create("web-idle");
    let (port, server) = bridge(Daemon {
        endpoint: EndpointPaths::from_runtime_root(runtime_root.0.clone()),
        workspace: runtime_root.0.to_string_lossy().into_owned(),
        config: None,
    })
    .await;
    let _idle = TcpStream::connect(("127.0.0.1", port)).await.expect("idle");
    let answered = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        http(port, "GET", "/api/catalogue", &[]),
    )
    .await
    .expect("answered while another connection sits idle");
    assert_eq!(answered.0, 200);
    server.abort();
}

#[tokio::test]
async fn the_catalogue_is_the_shared_one_with_english_fallback() {
    for (tag, locale) in [("en", Locale::En), ("ko", Locale::Ko), ("xx", Locale::En)] {
        let catalogue = api::catalogue(Locale::from_tag(tag));
        assert_eq!(catalogue["locale"], locale.tag());
        let messages = catalogue["messages"].as_object().expect("messages");
        assert_eq!(messages.len(), Msg::ALL.len());
        for &msg in Msg::ALL {
            assert_eq!(
                messages[msg.key()],
                text(locale, msg),
                "{tag} {}",
                msg.key()
            );
        }
        assert_eq!(catalogue["changes"].as_array().expect("changes").len(), 6);
    }
}

#[tokio::test]
async fn the_bridge_passes_the_daemons_truth_through() {
    let home = TestDir::create("web-home");
    let root = workspace();
    // A Resource with more declarations than one page holds.
    let many: String = (0..90)
        .map(|index| format!("export function many{index}(): number {{\n  return {index};\n}}\n"))
        .collect();
    fs::write(root.0.join("src/many.ts"), many).expect("many");
    let (running, endpoint) = start(&home.0).await;
    let mut connection = client::connect(&endpoint).await.expect("connect");
    let init = client::init(&mut connection, root.0.to_string_lossy().into_owned())
        .await
        .expect("init");
    drop(connection);
    let daemon = || Daemon {
        endpoint: endpoint.clone(),
        workspace: root.0.to_string_lossy().into_owned(),
        config: None,
    };
    let direct = daemon();
    let (port, server) = bridge(daemon()).await;

    // Status: the same path-scoped status the CLI and TUI read.
    let status = get(port, "/api/status").await;
    let status = data(&status);
    let mut cli = client::connect(&endpoint).await.expect("connect");
    let cli_status = client::status(&mut cli, Some(direct.workspace.clone()))
        .await
        .expect("status");
    drop(cli);
    let mut bridged = status["status"].clone();
    let mut expected = serde_json::to_value(&cli_status).expect("json");
    for value in [&mut bridged, &mut expected] {
        value["uptime_seconds"] = Value::Null;
    }
    assert_eq!(bridged, expected);
    assert_eq!(status["client_protocol_version"], PROTOCOL_VERSION);
    let report = &status["status"]["workspace"]["Initialized"];
    assert_eq!(report["workspace_id"], init.workspace_id.as_str());
    assert_eq!(report["basis"]["Stable"]["generation_no"], 1);
    assert_eq!(status["keys"]["currentness"], Msg::WorkspaceCurrent.key());
    assert!(
        status["keys"]["capabilities"]
            .as_array()
            .expect("capabilities")
            .iter()
            .all(|row| row["state"] == Msg::CapabilityPerQuery.key())
    );
    // Locale is a catalogue choice: it never reaches a fact endpoint.
    let korean = get(port, "/api/status?locale=ko").await;
    assert_eq!(data(&korean)["keys"], status["keys"]);

    // Find → token → inspect: the CLI's own rendering of the same answer.
    let found = get(port, "/api/find?q=helper").await;
    let found = data(&found);
    let candidate = found["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .find(|candidate| {
            candidate["label"]
                .as_str()
                .is_some_and(|l| l.contains("Function helper"))
        })
        .expect("helper")
        .clone();
    let token = candidate["token"].as_str().expect("token").to_owned();
    let target = api::target(&token).expect("id target");
    let inspect = get(port, &format!("/api/inspect?target={}", encode(&token))).await;
    let inspect = data(&inspect);
    let direct_inspect = direct
        .query(QueryOperationWire::Inspect(InspectWire {
            target: target.clone(),
            delivery: delivery(None),
        }))
        .await
        .unwrap_or_else(|_| panic!("direct inspect"));
    assert_eq!(lines(&inspect["body"]), compact(&direct_inspect));
    assert_eq!(inspect["currentness"], Msg::WorkspaceCurrent.key());
    // #78: the body cites `helper` (canonical line 0) at its editor lines, never a Debug span.
    let body = lines(&inspect["body"]).join("\n");
    assert!(body.contains("helper src/shared.ts:1-3"), "{body}");
    assert!(!body.contains("line:"), "{body}");
    assert_eq!(inspect["more_available"], false);

    // Relations: each direction summarized exactly as `present` does.
    let relations = get(port, &format!("/api/relations?target={}", encode(&token))).await;
    let relations = data(&relations);
    let QueryResultWire::Relations(direct_relations) = direct
        .query(QueryOperationWire::Relations(RelationsWire {
            target: target.clone(),
            direction: RelationDirectionWire::Both,
            kinds: Vec::new(),
        }))
        .await
        .unwrap_or_else(|_| panic!("direct relations"))
    else {
        panic!("relations")
    };
    for (bridged, answer) in relations["answers"]
        .as_array()
        .expect("answers")
        .iter()
        .zip(&direct_relations.answers)
    {
        let summary = present::relation_summary(answer);
        assert_eq!(bridged["direction"], summary.direction.key());
        assert_eq!(bridged["confirmed"], summary.confirmed);
        assert_eq!(bridged["coverage"], summary.coverage.key());
        assert_eq!(bridged["none"].as_str(), summary.none.map(Msg::key));
    }
    let incoming = &relations["answers"][1];
    assert_eq!(incoming["direction"], Msg::LabelIncoming.key());
    assert_eq!(incoming["confirmed"], 1);
    assert_eq!(incoming["kinds"][0]["kind"], "Calls");
    // #78: the body states each direction's coverage, never confirmed rows alone.
    let body = lines(&relations["body"]);
    assert!(
        body.iter()
            .any(|line| line.starts_with("Incoming: 1 confirmed, coverage ")),
        "{body:?}"
    );

    // Impact: the daemon's traversal for the chosen change form.
    let impact = get(
        port,
        &format!("/api/impact?target={}&change=1", encode(&token)),
    )
    .await;
    let impact = data(&impact);
    assert_eq!(impact["change"], Msg::ImpactRename.key());
    let direct_impact = direct
        .query(QueryOperationWire::Impact(ImpactWire {
            target: target.clone(),
            change: ChangeKindWire::Structural(ImpactIntentWire::Rename),
            delivery: delivery(None),
        }))
        .await
        .unwrap_or_else(|_| panic!("direct impact"));
    assert_eq!(lines(&impact["body"]), compact(&direct_impact));

    // An unsupported file: a Resource whose relations are Unsupported,
    // never "none under complete coverage".
    let go = get(port, "/api/find?q=src/main.go").await;
    let go_token = data(&go)["candidates"][0]["token"]
        .as_str()
        .expect("go")
        .to_owned();
    let go_relations = get(
        port,
        &format!("/api/relations?target={}", encode(&go_token)),
    )
    .await;
    let outgoing = &data(&go_relations)["answers"][0];
    assert_eq!(outgoing["direction"], Msg::LabelOutgoing.key());
    assert_eq!(outgoing["coverage"], Msg::CoverageUnsupported.key());
    assert_eq!(outgoing["none"], Msg::CoverageUnsupported.key());
    let body = lines(&data(&go_relations)["body"]);
    assert!(
        body.iter()
            .any(|line| line.starts_with("Outgoing: Unsupported")),
        "#78: an unsupported zero is never a silent body: {body:?}"
    );

    // Continuation: a bounded page, then the next one the daemon names.
    let many = get(port, "/api/find?q=src/many.ts").await;
    let many_token = data(&many)["candidates"][0]["token"]
        .as_str()
        .expect("many")
        .to_owned();
    let first = get(
        port,
        &format!("/api/inspect?target={}", encode(&many_token)),
    )
    .await;
    let first = data(&first);
    assert_eq!(first["more_available"], true, "{first}");
    assert!(first["omitted_items"].as_u64().expect("omitted") > 0);
    let next_token = first["continuation"].as_str().expect("continuation");
    let second = get(
        port,
        &format!(
            "/api/inspect?target={}&continuation={}",
            encode(&many_token),
            encode(next_token)
        ),
    )
    .await;
    let second = data(&second);
    let direct_second = direct
        .query(QueryOperationWire::Inspect(InspectWire {
            target: api::target(&many_token).expect("id"),
            delivery: delivery(api::continuation(next_token)),
        }))
        .await
        .unwrap_or_else(|_| panic!("direct second page"));
    assert_eq!(lines(&second["body"]), compact(&direct_second));
    assert_ne!(second["body"], first["body"]);
    let (code, _) = http(
        port,
        "GET",
        &format!(
            "/api/inspect?target={}&continuation=bogus",
            encode(&many_token)
        ),
        &[],
    )
    .await;
    assert_eq!(code, 400);

    // Stopping the bridge leaves the daemon running.
    server.abort();
    let _ = server.await;
    let mut alive = client::connect(&endpoint).await.expect("daemon alive");
    client::status(&mut alive, None).await.expect("status");
    drop(alive);

    // A stopped daemon: no data, never the old answer.
    let (port, server) = bridge(daemon()).await;
    stop(running).await;
    let gone = get(port, "/api/status").await;
    assert_eq!(gone["connection"], "disconnected", "{gone}");
    assert!(gone.get("data").is_none());
    // Restarted: the next request is answered again.
    let (running, _) = start(&home.0).await;
    let back = get(port, "/api/status").await;
    assert_eq!(
        data(&back)["status"]["workspace"]["Initialized"]["workspace_id"],
        init.workspace_id.as_str()
    );
    server.abort();
    stop(running).await;
}

#[tokio::test]
async fn an_incompatible_daemon_is_named_and_serves_no_data() {
    let runtime_root = TestDir::create("web-mismatch");
    let endpoint = EndpointPaths::from_runtime_root(runtime_root.0.clone());
    #[cfg(unix)]
    let mut listener = {
        fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
        Listener::bind(&endpoint.socket_path).expect("listener")
    };
    #[cfg(windows)]
    let mut listener = Listener::bind(&endpoint.pipe_name).expect("listener");
    let fake = tokio::spawn(async move {
        let mut connection = listener.accept().await.expect("accept");
        let Request::Handshake(handshake) = framing::read_message(&mut connection)
            .await
            .expect("handshake")
        else {
            panic!("handshake first")
        };
        framing::write_message(
            &mut connection,
            &Response::Handshake(HandshakeResponse::VersionMismatch {
                server_protocol_version: PROTOCOL_VERSION - 1,
                client_protocol_version: handshake.protocol_version,
            }),
        )
        .await
        .expect("reply");
    });
    let (port, server) = bridge(Daemon {
        endpoint,
        workspace: runtime_root.0.to_string_lossy().into_owned(),
        config: None,
    })
    .await;
    let reply = get(port, "/api/status").await;
    fake.await.expect("fake daemon");
    assert_eq!(reply["connection"], "incompatible", "{reply}");
    assert!(reply.get("data").is_none());
    let detail = reply["detail"].as_str().expect("detail");
    assert!(
        detail.contains("the running brainprintd is older"),
        "{detail}"
    );
    server.abort();
}
