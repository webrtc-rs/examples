use anyhow::Result;
use webrtc::api::media_engine::{MIME_TYPE_VP8, MIME_TYPE_OPUS};
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;
use flume::Sender;
use flume::Receiver;
use std::collections::HashMap;
use crate::sfu::signal::SocketMessage;
use crate::PeerChanCommand;

#[derive(Clone)]
pub struct Peer {
    // The peer connection itself
    pub pc: Arc<RTCPeerConnection>,
    // Copy of the socket to transmit back on
    pub tx: Sender<SocketMessage>,
    pub local_video_tracks: HashMap<String, Arc<TrackLocalStaticRTP>>,
    pub local_audio_tracks: HashMap<String, Arc<TrackLocalStaticRTP>>,
    // The remote tracks on which rtp packets are read in from
    pub remote_video_track: Option<Arc<TrackRemote>>,
    pub remote_audio_track: Option<Arc<TrackRemote>>,
    // The id for this peer in the call
    pub uuid: String,
}

impl fmt::Debug for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Peer")
            .field("uuid", &self.uuid)
            .finish()
    }
}

impl Peer {
    async fn add_local_tracks(&mut self, uuid: String) -> anyhow::Result<()> {
        println!("Adding track to {}", uuid);
        for s in MEDIA.clone() {
            let local_track = Arc::new(TrackLocalStaticRTP::new(
                    RTCRtpCodecCapability {
                        mime_type: if s == "video" {
                            MIME_TYPE_VP8.to_owned()
                        } else {
                            MIME_TYPE_OPUS.to_owned()
                        },
                        ..Default::default()
                    },
                    format!("track-{}", s),
                    format!("webrtc-rs-{}", uuid)
            ));

            // Add this newly created local track to the PeerConnection
            let rtp_sender = self.pc
                .add_track(Arc::clone(&local_track) as Arc<dyn TrackLocal + Send + Sync>)
                .await?;

            // Read incoming RTCP packets forever.
            // Before these packets are returned they are processed by interceptors. For things
            // like NACK this needs to be called.
            let m = s.to_owned();
            tokio::spawn(async move {
                let mut rtcp_buf = vec![0u8; 1500];
                while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                println!("{} rtp_sender.read loop exit", m);
                Result::<()>::Ok(())
            });

            if s == "video" {
                self.local_video_tracks.insert(uuid.to_owned(), local_track);
            } else if s == "audio" {
                self.local_audio_tracks.insert(uuid.to_owned(), local_track);
            }
        }

        Ok(())
    }

    async fn set_pc_callbacks(&mut self, peer_chan_tx: Sender<PeerChanCommand>) -> anyhow::Result<()> {
        // Set the handler for when renegotiation needs to happen
        let tx_clone = peer_chan_tx.clone();
        let uuid = self.uuid.clone();
        self.pc
            .on_negotiation_needed(Box::new(move || {
                println!("{} Peer Connection needs negotiation.", uuid);
                let cloned_tx = tx_clone.clone();
                let cloned_id = uuid.clone();

                Box::pin(async move {
                    cloned_tx.send(PeerChanCommand::SendOffer {
                        uuid: cloned_id.to_owned(),
                    }).unwrap();
                })
            }))
        .await;

        // Set the handler for receiving a new trickle ice candidate.
        let tx_clone = peer_chan_tx.clone();
        let mut uuid = self.uuid.clone();
        self.pc
            .on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
                let cloned_tx = tx_clone.clone();
                let cloned_id = uuid.clone();

                Box::pin(async move {
                    if !candidate.is_none() {
                        cloned_tx.send(PeerChanCommand::SendIceCandidate {
                            uuid: cloned_id.to_owned(),
                            candidate: serde_json::to_string(&candidate.unwrap()).unwrap(),
                        }).unwrap();
                    }
                })
            })).await;

        self.pc
            .on_ice_gathering_state_change(Box::new(move |s: RTCIceGathererState| {
                println!("On ice gathering state change, {}", s);
                Box::pin(async {})
            })).await;

        self.pc
            .on_ice_connection_state_change(Box::new(move |s: RTCIceConnectionState| {
                println!("On ice connection state change, {}", s);
                Box::pin(async {})
            })).await;


        self.pc
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                println!("Peer Connection State has changed: {}", s);
                Box::pin(async {})
            })).await;

        // Set a handler for when a new remote track starts, this handler copies inbound RTP packets,
        // replaces the SSRC and sends them back
        let tx_clone = peer_chan_tx.clone();
        uuid = self.uuid.clone();
        self.pc
            .on_track(Box::new(
                    move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
                        if let Some(track) = track {
                            tx_clone.send(PeerChanCommand::OnTrack {
                                uuid: uuid.to_owned(),
                                track
                            }).unwrap();
                        }
                        Box::pin(async {})
                    },
            )).await;

        Ok(())
    }
}

const MEDIA: [&'static str; 2] = ["audio", "video"];

// This is ran in a tokio task, that holds all the shared state. It's communicated to by channels.
pub async fn handle_peer_connection_commands(peer_chan_rx: Receiver<PeerChanCommand>, peer_chan_tx: Sender<PeerChanCommand>) -> Result<()> {
    let api = crate::sfu::api::prepare_api()?;

    let mut peers: HashMap<String, Peer> = HashMap::new();

    while let Ok(cmd) = peer_chan_rx.recv_async().await {
        use PeerChanCommand::*;

        match cmd {
            DistributePacket { uuid, packet, kind } => {
                match kind {
                    RTPCodecType::Video => {
                        for (_k, p) in &peers {
                            match p.local_video_tracks.get(&uuid) {
                                Some(track) => {
                                    if let Err(err) = track.write_rtp(&packet).await {
                                        println!("output track write_rtp got error: {}", err);
                                        break;
                                    }
                                }
                                None => ()
                            }
                        }
                    }
                    RTPCodecType::Audio => {
                        for (_k, p) in &peers {
                            match p.local_audio_tracks.get(&uuid) {
                                Some(track) => {
                                    if let Err(err) = track.write_rtp(&packet).await {
                                        println!("output track write_rtp got error: {}", err);
                                        break;
                                    }
                                }
                                None => ()
                            }
                        }
                    }
                    _ => ()
                }
            }
            SendIceCandidate { uuid, candidate } => {
                let peer = peers.get(&uuid).unwrap();

                peer.tx.send(SocketMessage {
                    event: String::from("candidate"),
                    data: candidate,
                    uuid: uuid.to_owned()
                }).unwrap();
            }
            ReceiveIceCandidate { uuid, candidate } => {
                let peer = peers.get(&uuid).unwrap();
                let pc = Arc::clone(&peer.pc);
                let can: RTCIceCandidateInit = serde_json::from_str(&candidate).unwrap();
                pc.add_ice_candidate(can).await.unwrap();
            }
            SendOffer { uuid } => {
                println!("Renegotiating for {}...", uuid);
                let peer = peers.get_mut(&uuid).unwrap();
                let pc = Arc::clone(&peer.pc);

                let offer = pc.create_offer(None).await?;
                let offer_string = serde_json::to_string(&offer)?;

                pc.set_local_description(offer).await.unwrap();

                peer.tx.send(SocketMessage {
                    event: String::from("offer"),
                    data: offer_string,
                    uuid: uuid.to_owned()
                }).unwrap();
            }
            ReceiveOffer { uuid, sdp, tx } => {
                let tx_clone = tx.clone();
                match peers.get(&uuid) {
                    Some(peer) => {
                        let pc = Arc::clone(&peer.pc);
                        let offer = RTCSessionDescription::offer(sdp).unwrap();
                        pc.set_remote_description(offer).await.unwrap();
                    }
                    None => {
                        let config = crate::sfu::api::prepare_configuration()?;

                        let mut peer = Peer {
                            pc: Arc::new(api.new_peer_connection(config).await?),
                            uuid: uuid.clone(),
                            local_video_tracks: HashMap::new(),
                            local_audio_tracks: HashMap::new(),
                            remote_video_track: None,
                            remote_audio_track: None,
                            tx,
                        };
                        let pc = Arc::clone(&peer.pc);

                        println!("Creating new RTCPeerConnection.");

                        let offer = RTCSessionDescription::offer(sdp).unwrap();
                        pc.set_remote_description(offer).await.unwrap();

                        peer.set_pc_callbacks(peer_chan_tx.clone()).await.unwrap();

                        // Step 1: Adding onto every other peer, we will add a local track that will read from this
                        // new peer's remote track.
                        for (_key, p) in &mut peers {
                            p.add_local_tracks(uuid.clone()).await?;
                            println!("{} track count: {}", p.uuid, p.local_video_tracks.len());
                        }

                        // Step 2: Inserting into this peer's local_video/audio_tracks, we will create one local track for
                        // every other currently existing peer.
                        for (_key, p) in &peers {
                            peer.add_local_tracks(p.uuid.clone()).await?;
                            println!("{} track count: {}", p.uuid, p.local_video_tracks.len());
                        }

                        let answer = pc.create_answer(None).await?;
                        let answer_string = serde_json::to_string(&answer)?;

                        pc.set_local_description(answer).await.unwrap();

                        tx_clone.send(SocketMessage {
                            event: String::from("answer"),
                            data: answer_string.to_owned(),
                            uuid: uuid.to_owned()
                        }).unwrap();

                        // Step 3: Add the new peer to the peers list.
                        peers.insert(uuid.to_owned(), peer);
                    }
                }
            }
            ReceiveAnswer { uuid, sdp } => {
                let peer = peers.get(&uuid).unwrap();
                let pc = Arc::clone(&peer.pc);

                let answer = RTCSessionDescription::answer(sdp).unwrap();
                pc.set_remote_description(answer).await.unwrap();
            },
            OnTrack { uuid, track } => {
                let peer = peers.get(&uuid).unwrap();
                let pc = Arc::clone(&peer.pc);

                println!("Remote track is starting.");

                // Send a PLI on an interval so that the publisher is pushing a keyframe every rtcpPLIInterval
                // This is a temporary fix until we implement incoming RTCP events, then we would push a PLI only when a viewer requests it
                let media_ssrc = track.ssrc();

                if track.kind() == RTPCodecType::Video {
                    tokio::spawn(async move {
                        let mut result = Result::<usize>::Ok(0);
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(Duration::from_secs(3));
                            tokio::pin!(timeout);

                            tokio::select! {
                                _ = timeout.as_mut() =>{
                                    result = pc.write_rtcp(&[Box::new(PictureLossIndication{
                                        sender_ssrc: 0,
                                        media_ssrc,
                                    })]).await.map_err(Into::into);
                                }
                            };
                        }
                    });
                }

                let cloned_peer_chan_tx = peer_chan_tx.clone();
                let id = uuid.clone();
                tokio::spawn(async move {
                    println!(
                        "Track has started, of type {}: {}",
                        track.payload_type(),
                        track.codec().await.capability.mime_type
                    );
                    // Read RTP packets being sent to webrtc-rs
                    while let Ok((rtp, _)) = track.read_rtp().await {
                        cloned_peer_chan_tx.send(PeerChanCommand::DistributePacket {
                            uuid: id.to_owned(),
                            packet: rtp,
                            kind: track.kind()
                        }).unwrap();
                    }

                    println!(
                        "on_track finished, of type {}: {}",
                        track.payload_type(),
                        track.codec().await.capability.mime_type
                    );
                });

            },
        }
    }

    Ok(())
}
