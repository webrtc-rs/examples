use anyhow::Result;
use sfu::signal::SocketMessage;
use std::sync::Arc;
use webrtc::{track::track_remote::TrackRemote, rtp::packet::Packet, rtp_transceiver::rtp_codec::RTPCodecType};
use flume::{Sender, Receiver};

mod sfu;

#[derive(Debug, Clone)]
pub enum PeerChanCommand {
    SendIceCandidate {
        uuid: String,
        candidate: String,
    },
    ReceiveIceCandidate {
        uuid: String,
        candidate: String,
    },
    SendOffer {
        uuid: String
    },
    ReceiveOffer {
        uuid: String,
        sdp: String,
        tx: Sender<SocketMessage>
    },
    ReceiveAnswer {
        uuid: String,
        sdp: String,
    },
    OnTrack {
        uuid: String,
        track: Arc<TrackRemote>
    },
    DistributePacket {
        uuid: String,
        packet: Packet,
        kind: RTPCodecType
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let new_conn_rx = sfu::signal::ws_sdp_signaler(8081).await;

    let (peer_chan_tx, peer_chan_rx) = flume::unbounded::<PeerChanCommand>();

    let tx_clone_1 = peer_chan_tx.clone();
    let tx_clone_2 = peer_chan_tx.clone();

    tokio::spawn(async move {
        sfu::media::handle_peer_connection_commands(peer_chan_rx, tx_clone_1.clone()).await.unwrap();
    });

    while let Ok((uuid, socket_tx, socket_rx)) = new_conn_rx.recv_async().await {
        handle_new_connection(&uuid, tx_clone_2.clone(), socket_tx, socket_rx).await.unwrap();
    };

    Ok(())
}

// Handler to spin off for every new connection.
async fn handle_new_connection(_uuid: &String, peer_chan_tx: Sender<PeerChanCommand>, socket_tx: Sender<SocketMessage>, socket_rx: Receiver<SocketMessage>) -> Result<()> {
    tokio::spawn(async move {
        println!("Handling a new connection.");
        while let Ok(signal) = socket_rx.recv_async().await {
            match signal {
                SocketMessage { event, uuid: id, data: sdp } if event == "offer" => {
                    println!("\nReceiving offer: {:?}, for uuid: {:?}\n", sdp, id);
                    peer_chan_tx.send(PeerChanCommand::ReceiveOffer {
                        uuid: id.to_owned(),
                        tx: socket_tx.clone(),
                        sdp
                    }).unwrap();
                },
                SocketMessage { event, uuid: id, data: sdp } if event == "answer" => {
                    println!("\nReceiving answer: {:?}, for uuid: {:?}\n", sdp, id);
                    peer_chan_tx.send(PeerChanCommand::ReceiveAnswer {
                        uuid: id.to_owned(),
                        sdp
                    }).unwrap();
                },
                SocketMessage { event, uuid: id, data: candidate } if event == "candidate" => {
                    println!("\nReceiving ice candidate, for uuid: {:?}\n", id);
                    peer_chan_tx.send(PeerChanCommand::ReceiveIceCandidate {
                        uuid: id.to_owned(),
                        candidate
                    }).unwrap();
                },
                _ => ()
            }
        };
    });

    Ok(())
}
