use anyhow::Result;
use clap::{App, AppSettings, Arg};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use tokio::time::Duration;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_receiver::RTCRtpReceiver;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

mod data_channel;

static TRACK_ADDED: AtomicBool = AtomicBool::new(false);

#[tokio::main]
async fn main() -> Result<()> {
    let mut app = App::new("reflect")
        .version("0.1.0")
        .author("Rain Liu <yliu@webrtc.rs>")
        .about("An example of how to send back to the user exactly what it receives using the same PeerConnection.")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandsNegateReqs)
        .arg(
            Arg::new("FULLHELP")
                .help("Prints more detailed help information")
                .long("fullhelp"),
        )
        .arg(
            Arg::new("debug")
                .long("debug")
                .short('d')
                .help("Prints debug log information"),
        ).arg(
            Arg::new("audio")
                .long("audio")
                .short('a')
                .help("Enable audio reflect"),
        ).arg(
            Arg::new("video")
                .long("video")
                .short('v')
                .help("Enable video reflect"),
        );

    let matches = app.clone().get_matches();

    if matches.is_present("FULLHELP") {
        app.print_long_help().unwrap();
        std::process::exit(0);
    }

    let audio = matches.is_present("audio");
    let video = matches.is_present("video");
    if !audio && !video {
        println!("one of audio or video must be enabled");
        std::process::exit(0);
    }
    let debug = matches.is_present("debug");
    if debug {
        env_logger::Builder::new()
            .format(|buf, record| {
                writeln!(
                    buf,
                    "{}:{} [{}] {} - {}",
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    record.level(),
                    chrono::Local::now().format("%H:%M:%S.%6f"),
                    record.args()
                )
            })
            .filter(None, log::LevelFilter::Trace)
            .init();
    }

    // Everything below is the WebRTC-rs API! Thanks for using it ❤️.

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();

    // Setup the codecs you want to use.
    if audio {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    ..Default::default()
                },
                payload_type: 120,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;
    }

    // We'll use a VP8 and Opus but you can also define your own
    if video {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
    }

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);
    let (dc_connected_tx, dc_connected_rx) = oneshot::channel();

    let dc_tx = Arc::new(Mutex::new(Some(dc_connected_tx)));
    peer_connection
        .on_data_channel(Box::new(move |dc| {
            let dc_tx = dc_tx.clone();
            Box::pin(async move {
                let mut lock = dc_tx.lock().await;

                if let Some(dc_tx) = lock.take() {
                    let managed_dc = data_channel::DataChannel::new(dc).await;
                    let _ = dc_tx.send(managed_dc);
                }
            })
        }))
        .await;

    // I expect that this should be called due to `add_track` for the reflect track below
    {
        let pc = Arc::clone(&peer_connection);
        peer_connection
            .on_negotiation_needed(Box::new(move || {
                let pc = Arc::clone(&pc);
                log::info!(
                    "Negotiation needed fired. Track added {}",
                    TRACK_ADDED.load(Ordering::SeqCst)
                );

                Box::pin(async move {
                    // Here we'd use the data channel to negotiate
                    log::info!(
                        "Negotiation needed op ran. Track added {}",
                        TRACK_ADDED.load(Ordering::SeqCst)
                    );

                    let offer = pc.create_offer(None).await.expect("Failed to create offer");

                    log::info!("Created offer: {:?}", offer);
                })
            }))
            .await;
    }

    // Wait for the offer to be pasted
    let line = signal::must_read_stdin()?;
    let desc_data = signal::decode(line.as_str())?;
    let offer = serde_json::from_str::<RTCSessionDescription>(&desc_data)?;

    // Set the remote SessionDescription
    peer_connection.set_remote_description(offer).await?;

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            println!("Peer Connection State has changed: {}", s);

            if s == RTCPeerConnectionState::Failed {
                // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                println!("Peer Connection has gone to failed exiting");
                let _ = done_tx.try_send(());
            }

            Box::pin(async {})
        }))
        .await;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Output the answer in base64 so we can paste it in browser
    if let Some(local_desc) = peer_connection.local_description().await {
        let json_str = serde_json::to_string(&local_desc)?;
        let b64 = signal::encode(&json_str);
        println!("{}", b64);
    } else {
        println!("generate local_description failed!");
    }

    //let timeout = tokio::time::sleep(Duration::from_secs(20));
    //tokio::pin!(timeout);
    //
    // Set a handler for when a new remote track starts, this handler copies inbound RTP packets,
    // replaces the SSRC and sends them back
    let pc = Arc::downgrade(&peer_connection);
    peer_connection
        .on_track(Box::new(
            move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
                let track = match track {
                    None => return Box::pin(async {}),
                    Some(t) => t,
                };
                let weak_pc = pc.clone();

                Box::pin(async move {
                    let pc = match weak_pc.upgrade() {
                        Some(p) => p,
                        None => return,
                    };

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
                                        if let Some(pc) = weak_pc.upgrade(){
                                            result = pc.write_rtcp(&[Box::new(PictureLossIndication{
                                                    sender_ssrc: 0,
                                                    media_ssrc,
                                            })]).await.map_err(Into::into);
                                        }else{
                                            break;
                                        }
                                    }
                                };
                            }
                        });
                    }

                    let kind = if track.kind() == RTPCodecType::Audio {
                        "audio"
                    } else {
                        "video"
                    };

                    let output_track = Arc::new(TrackLocalStaticRTP::new(
                        RTCRtpCodecCapability {
                            mime_type: if kind == "video" {
                                MIME_TYPE_VP8.to_owned()
                            } else {
                                MIME_TYPE_OPUS.to_owned()
                            },
                            ..Default::default()
                        },
                        format!("reflected-{}", track.ssrc()),
                        "webrtc-rs".to_owned(),
                    ));

                    TRACK_ADDED.store(true, Ordering::SeqCst);
                    // Add this newly created track to the PeerConnection
                    // This should trigger negotiation, but doesn't
                    let rtp_sender = pc
                        .add_track(Arc::clone(&output_track) as Arc<dyn TrackLocal + Send + Sync>)
                        .await
                        .expect("Failed to add output track");
                    log::info!("Created output track {}", kind);

                    // Read incoming RTCP packets
                    // Before these packets are returned they are processed by interceptors. For things
                    // like NACK this needs to be called.
                    let m = kind.to_owned();
                    tokio::spawn(async move {
                        let mut rtcp_buf = vec![0u8; 1500];
                        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                        println!("{} rtp_sender.read loop exit", m);
                        Result::<()>::Ok(())
                    });

                    let output_track2 = Arc::clone(&output_track);
                    tokio::spawn(async move {
                        println!(
                            "Track has started, of type {}: {}",
                            track.payload_type(),
                            track.codec().await.capability.mime_type
                        );
                        // Read RTP packets being sent to webrtc-rs
                        while let Ok((rtp, _)) = track.read_rtp().await {
                            if let Err(err) = output_track2.write_rtp(&rtp).await {
                                println!("output track write_rtp got error: {}", err);
                                break;
                            }
                        }

                        println!(
                            "on_track finished, of type {}: {}",
                            track.payload_type(),
                            track.codec().await.capability.mime_type
                        );
                    });
                })
            },
        ))
        .await;

    let mut dc = dc_connected_rx.await.expect("Failed to get data channel");
    let next_message = dc.next_message().await;
    // Wait for open
    assert!(matches!(next_message, data_channel::Message::Opened));

    println!("Press ctrl-c to stop");

    tokio::select! {
        _ = done_rx.recv() => {
            println!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("");
        }
    };

    peer_connection.close().await?;

    Ok(())
}
