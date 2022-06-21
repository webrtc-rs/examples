use anyhow::Result; use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use std::net::SocketAddr;
use flume::Receiver;
use flume::Sender;
use futures::{sink::SinkExt, stream::StreamExt};
use hyper_tungstenite::{tungstenite, HyperWebsocket};
use std::convert::Infallible;
use tungstenite::Message;
use uuid::Uuid;
use serde::{Deserialize, Serialize};
use tokio_util::codec::{BytesCodec, FramedRead};

static INDEX: &str = "examples/sfu/index.html";
static NOTFOUND: &[u8] = b"Not Found";

fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(NOTFOUND.into())
        .unwrap()
}

async fn simple_file_send(filename: &str) -> Result<Response<Body>, anyhow::Error> {
    // Serve a file by asynchronously reading it by chunks using tokio-util crate.
    if let Ok(file) = tokio::fs::File::open(filename).await {
        let stream = FramedRead::new(file, BytesCodec::new());
        let body = Body::wrap_stream(stream);
        return Ok(Response::new(body));
    }

    Ok(not_found())
}

fn uuid() -> String {
    Uuid::new_v4().to_hyphenated().to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SocketMessage {
    pub event: String,
    pub data: String,
    pub uuid: String,
}

type Connection = (String, Sender<SocketMessage>, Receiver<SocketMessage>);

pub async fn ws_sdp_signaler(port: u16) -> Receiver<Connection> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    // A channel for passing new connections (which themselves contain channels) to the main task
    let (conn_chan_tx, conn_chan_rx) = flume::unbounded::<Connection>();
    let (conn_chan_2_tx, conn_chan_2_rx) = flume::unbounded::<Connection>();

    let mut connections: i32 = 0;
    tokio::spawn(async move {
        println!("Creating connections passer");
        while let Ok(channels) = conn_chan_rx.recv() {
            println!("Got new connection, length is: {:?}", connections);
            connections = connections + 1;
            conn_chan_2_tx.send(channels).unwrap();
        }
    });

    let make_svc = make_service_fn(move |_conn: &AddrStream| {
        let conn_chan_tx_clone = conn_chan_tx.clone();

        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                handle_request(req, uuid(), conn_chan_tx_clone.clone())
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_svc);

    tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    });

    conn_chan_2_rx
}

/// Handle a HTTP or WebSocket request.
async fn handle_request(
    request: Request<Body>,
    uuid: String,
    conn_tx: Sender<Connection>,
) -> Result<Response<Body>, anyhow::Error> {
    // Check if the request is a websocket upgrade request.
    if hyper_tungstenite::is_upgrade_request(&request) {
        let (response, websocket) = hyper_tungstenite::upgrade(request, None)?;

        // Spawn a task to handle the websocket connection.
        tokio::spawn(async move {
            if let Err(e) = serve_websocket(websocket, uuid, conn_tx).await {
                eprintln!("Error in websocket connection: {}", e);
            }
        });

        // Return the response so the spawned future can continue.
        Ok(response)
    } else {
        simple_file_send(INDEX).await
    }
}

/// Handle a websocket connection.
async fn serve_websocket(
    websocket: HyperWebsocket,
    uuid: String,
    conn_tx: Sender<Connection>,
) -> Result<(), anyhow::Error> {
    let (out_tx, out_rx) = flume::unbounded::<SocketMessage>();
    let (in_tx, in_rx) = flume::unbounded::<SocketMessage>();

    let (mut sink, mut stream) = websocket.await?.split();

    conn_tx.send((uuid.to_owned(), out_tx, in_rx)).unwrap();

    tokio::spawn(async move {
        while let Ok(message) = out_rx.recv_async().await {
            sink.send(Message::text(serde_json::to_string(&message).unwrap())).await.unwrap();
        }
    });

    while let Some(message) = stream.next().await {
        match message? {
            Message::Text(msg) => {
                in_tx.send(serde_json::from_str(&msg).unwrap()).unwrap();
            }
            Message::Close(msg) => {
                if let Some(msg) = &msg {
                    println!(
                        "Received close message with code {} and message: {}",
                        msg.code, msg.reason
                    );
                } else {
                    println!("Received close message");
                }
            }
            _ => (),
        }
    }

    Ok(())
}
