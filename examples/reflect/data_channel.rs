use std::sync::Arc;

use tokio::sync::mpsc::{self, Receiver};
use webrtc::data_channel::{data_channel_message::DataChannelMessage, RTCDataChannel};

#[derive(Debug)]
pub enum Message {
    Closed,
    Opened,
    Message(DataChannelMessage),
}

pub struct DataChannel {
    inner: Arc<RTCDataChannel>,
    incoming_rx: Receiver<Message>,
}

impl DataChannel {
    pub async fn new(inner: Arc<RTCDataChannel>) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel(1000);

        {
            let incoming_tx = incoming_tx.clone();
            inner
                .on_open(Box::new(move || {
                    let incoming_tx = incoming_tx.clone();

                    Box::pin(async move {
                        let _ = incoming_tx.send(Message::Opened).await;
                    })
                }))
                .await;
        }

        {
            let incoming_tx = incoming_tx.clone();
            inner
                .on_close(Box::new(move || {
                    let incoming_tx = incoming_tx.clone();

                    Box::pin(async move {
                        let _ = incoming_tx.send(Message::Closed).await;
                    })
                }))
                .await;
        }

        {
            let incoming_tx = incoming_tx.clone();
            inner
                .on_message(Box::new(move |message| {
                    let incoming_tx = incoming_tx.clone();

                    Box::pin(async move {
                        let _ = incoming_tx.send(Message::Message(message)).await;
                    })
                }))
                .await;
        }

        Self { inner, incoming_rx }
    }

    pub async fn next_message(&mut self) -> Message {
        self.incoming_rx
            .recv()
            .await
            .expect("Failed to receive incoming data channel message")
    }

    pub async fn send_text(&self, v: String) {
        self.inner
            .send_text(v)
            .await
            .expect("failed to send message");
    }
}
