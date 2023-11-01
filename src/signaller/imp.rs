use std::collections::BTreeMap;
use std::collections::HashSet;


use gst::glib;
use gst::glib::prelude::*;
use gst::subclass::prelude::*;
use gst_webrtc::WebRTCSessionDescription;
use gstrswebrtc::signaller::WebRTCSignallerRole;
use tokio::net::TcpStream;
use url::Url;
use gst::glib::once_cell::sync::Lazy;
use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tokio::connect_async;
use async_tungstenite::stream::Stream;
use async_tungstenite::tungstenite::Message as WsMessage;
use tokio_native_tls::TlsStream;
use futures::prelude::*;
use futures::channel::mpsc;
use gst_plugin_webrtc_signalling_protocol as p;
use tokio::{task, time::timeout};
use anyhow::{anyhow, Error};
use gstrswebrtc::RUNTIME;
use gstrswebrtc::signaller::{Signallable, SignallableImpl};
use gstrswebrtc::utils::{gvalue_to_json, serialize_json_object};
use std::ops::ControlFlow;
use std::sync::Mutex;
use std::str::FromStr;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value as JsonValue;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtc-pixelstreaming-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC PixelStreaming signaller"),
    )
});

#[derive(Debug, Deserialize)]
struct PSResponse {
    #[serde(flatten)]
    other: BTreeMap<String, JsonValue>,
}

#[derive(Default)]
pub struct Signaller {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

pub struct Settings {
    uri: Url,
    producer_peer_id: Option<String>,
    cafile: Option<String>,
    role: WebRTCSignallerRole,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            uri: Url::from_str("ws://192.168.1.4:3333").unwrap(),
            producer_peer_id: None,
            cafile: Default::default(),
            role: Default::default(),
        }
    }
}

#[derive(Default)]
struct State {
    /// Sender for the websocket messages
    websocket_sender: Option<mpsc::Sender<p::IncomingMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    receive_task_handle: Option<task::JoinHandle<()>>,
    producers: HashSet<String>,
    client_id: Option<String>,
}

impl Signaller {
    fn raise_error(&self, msg: String) {
        self.obj()
            .emit_by_name::<()>("error", &[&format!("Error: {msg}")]);
    }

    fn uri(&self) -> Url {
        self.settings.lock().unwrap().uri.clone()
    }

    async fn connect(&self) -> Result<(), Error> {
        gst::debug!(CAT, imp: self, "Connecting");

        let mut uri = self.uri();
        uri.set_query(None);

        // Connect to the server
        let (ws_stream, _) = connect_async(uri)
            .await
            .expect("Error during the WebSocket handshake.");

        gst::debug!(CAT, imp: self, "Connected!");

        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        let (websocket_sender, mut websocket_receiver) = mpsc::channel::<p::IncomingMessage>(1000);
        let send_task_handle =
            RUNTIME.spawn(glib::clone!(@weak-allow-none self as this => async move {
                while let Some(msg) = websocket_receiver.next().await {
                    gst::log!(CAT, "Sending websocket message {:?}", msg);
                    ws_sink
                        .send(WsMessage::Text(serde_json::to_string(&msg).unwrap()))
                        .await?;
                }

                let msg = "Done sending";
                this.map_or_else(|| gst::info!(CAT, "{msg}"),
                    |this| gst::info!(CAT, imp: this, "{msg}")
                );

                ws_sink.send(WsMessage::Close(None)).await?;
                ws_sink.close().await?;

                Ok::<(), Error>(())
            }));

        let obj = self.obj();
        let meta =
            if let Some(meta) = obj.emit_by_name::<Option<gst::Structure>>("request-meta", &[]) {
                println!("META: {:?}", meta);
                gvalue_to_json(&meta.to_value())
            } else {
                None
            };

        let receive_task_handle =
            RUNTIME.spawn(glib::clone!(@weak-allow-none self as this => async move {
                while let Some(msg) = tokio_stream::StreamExt::next(&mut ws_stream).await {
                    if let Some(ref this) = this {
                        if let ControlFlow::Break(_) = this.handle_message(msg, &meta) {
                            break;
                        }
                    } else {
                        break;
                    }
                }

                let msg = "Stopped websocket receiving";
                this.map_or_else(|| gst::info!(CAT, "{msg}"),
                    |this| gst::info!(CAT, imp: this, "{msg}")
                );
            }));

        let mut state = self.state.lock().unwrap();
        state.websocket_sender = Some(websocket_sender);
        state.send_task_handle = Some(send_task_handle);
        state.receive_task_handle = Some(receive_task_handle);


        Ok(())
    }

    fn send(&self, msg: p::IncomingMessage) {
        let state = self.state.lock().unwrap();
        if let Some(mut sender) = state.websocket_sender.clone() {
            RUNTIME.spawn(glib::clone!(@weak self as this => async move {
                if let Err(err) = sender.send(msg).await {
                    this.obj().emit_by_name::<()>("error", &[&format!("Error: {}", err)]);
                }
            }));
        }
    }

    fn handle_message(
        &self,
        msg: Result<WsMessage, async_tungstenite::tungstenite::Error>,
        meta: &Option<serde_json::Value>,
    ) -> ControlFlow<()> {
        match msg {
            Ok(WsMessage::Text(msg)) => {
                // gst::trace!(CAT, imp: self, "Received message {}", msg);
                let parsed: PSResponse = serde_json::from_str(&msg).unwrap();
                // println!("parsed: {:?}", parsed);
                if let Some(msg_type) = parsed.other.get("type") {
                    let msg_type = msg_type.as_str().unwrap();
                    match msg_type {
                        "offer" => {
                            if let Some(offer) = parsed.other.get("sdp") {
                                // println!("offer: {:?}", offer);
                                let sdp_str = offer.as_str().unwrap();
                                let sdp = match gst_sdp::SDPMessage::parse_buffer(sdp_str.as_bytes()) {
                                    Ok(sdp) => sdp,
                                    Err(_) => {
                                        self.raise_error("Couldn't parse Answer SDP".to_string());
                                        return ControlFlow::Break(());
                                    }
                                };
                                println!("got sdp offer {:?}", sdp);
                                let desc = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, sdp);
                                self.obj().emit_by_name::<()>(
                                    "session-description",
                                    &[&"unique", &desc],
                                );
                            }
                        },
                        "iceCandidate" => {
                            if let Some(candidate) = parsed.other.get("candidate") {
                                let candidate = candidate.as_object().unwrap();

                                let sdp_mid = candidate.get("sdpMid").unwrap().as_str().unwrap().to_string();
                                let mline = candidate.get("sdpMLineIndex").unwrap().as_i64().unwrap() as u32;
                                let candidate = candidate.get("candidate").unwrap().as_str().unwrap().to_string();

                                self.obj().emit_by_name::<()>(
                                    "handle-ice",
                                    &[&"unique", &mline, &Some(sdp_mid), &candidate],
                                );
                            }
                        }
                        _ => {
                            gst::warning!(CAT, imp: self, "unknown signalling message: {}", msg);
                        }
                    }
                }
            }
            Ok(WsMessage::Close(reason)) => {
                gst::info!(CAT, imp: self, "websocket connection closed: {:?}", reason);
                return ControlFlow::Break(());
            }
            Ok(_) => (),
            Err(err) => {
                self.obj()
                    .emit_by_name::<()>("error", &[&format!("Error receiving: {}", err)]);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
    // async fn signal_task(&self, mut signal_events: signal_client::SignalEvents) {
    //     loop {
    //         match wait_async(&self.signal_task_canceller, signal_events.recv(), 0).await {
    //             Ok(Some(signal)) => match signal {
    //                 signal_client::SignalEvent::Message(signal) => {
    //                     self.on_signal_event(*signal).await;
    //                 }
    //                 signal_client::SignalEvent::Close => {
    //                     gst::debug!(CAT, imp: self, "Close");
    //                     self.raise_error("Server disconnected".to_string());
    //                     break;
    //                 }
    //             },
    //             Ok(None) => {}
    //             Err(err) => match err {
    //                 WaitError::FutureAborted => {
    //                     gst::debug!(CAT, imp: self, "Closing signal_task");
    //                     break;
    //                 }
    //                 WaitError::FutureError(err) => self.raise_error(err.to_string()),
    //             },
    //         }
    //     }
    // }

    // async fn on_signal_event(&self, event: proto::signal_response::Message) {
    //     println!("on_signal_event: {:?}", event);
    // }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "MyCustomWebRTCSinkSignaller";
    type Type = super::MyCustomSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl SignallableImpl for Signaller {
    fn start(&self) {
        gst::info!(CAT, imp: self, "Starting");
        RUNTIME.spawn(glib::clone!(@weak self as this => async move {
            if let Err(err) = this.connect().await {
                this.obj().emit_by_name::<()>("error", &[&format!("Error receiving: {}", err)]);
            }
        }));
    }


    fn stop(&self) {
        gst::info!(CAT, imp: self, "Stopping now");

        let mut state = self.state.lock().unwrap();
        let send_task_handle = state.send_task_handle.take();
        let receive_task_handle = state.receive_task_handle.take();
        if let Some(mut sender) = state.websocket_sender.take() {
            RUNTIME.block_on(async move {
                sender.close_channel();

                if let Some(handle) = send_task_handle {
                    if let Err(err) = handle.await {
                        gst::warning!(CAT, imp: self, "Error while joining send task: {}", err);
                    }
                }

                if let Some(handle) = receive_task_handle {
                    if let Err(err) = handle.await {
                        gst::warning!(CAT, imp: self, "Error while joining receive task: {}", err);
                    }
                }
            });
        }
        state.producers.clear();
        state.client_id = None;
    }

    fn send_sdp(&self, session_id: &str, sdp: &WebRTCSessionDescription) {
        gst::debug!(CAT, imp: self, "Sending SDP {sdp:#?}");

        let role = self.settings.lock().unwrap().role;
        let is_consumer = matches!(role, WebRTCSignallerRole::Consumer);

        let msg = p::IncomingMessage::Peer(p::PeerMessage {
            session_id: session_id.to_owned(),
            peer_message: p::PeerMessageInner::Sdp(if is_consumer {
                p::SdpMessage::Answer {
                    sdp: sdp.sdp().as_text().unwrap(),
                }
            } else {
                p::SdpMessage::Offer {
                    sdp: sdp.sdp().as_text().unwrap(),
                }
            }),
        });

        self.send(msg);
    }

    fn add_ice(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: u32,
        _sdp_mid: Option<String>,
    ) {
        gst::debug!(
            CAT,
            imp: self,
            "Adding ice candidate {candidate:?} for {sdp_m_line_index:?} on session {session_id}"
        );

        let msg = p::IncomingMessage::Peer(p::PeerMessage {
            session_id: session_id.to_string(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: candidate.to_string(),
                sdp_m_line_index,
            },
        });

        self.send(msg);
    }

    fn end_session(&self, session_id: &str) {
        gst::debug!(CAT, imp: self, "Signalling session done {}", session_id);

        let state = self.state.lock().unwrap();
        let session_id = session_id.to_string();
        if let Some(mut sender) = state.websocket_sender.clone() {
            RUNTIME.spawn(glib::clone!(@weak self as this => async move {
                if let Err(err) = sender
                    .send(p::IncomingMessage::EndSession(p::EndSessionMessage {
                        session_id,
                    }))
                    .await
                {
                    this.obj().emit_by_name::<()>("error", &[&format!("Error: {}", err)]);
                }
            }));
        }
    }
}

impl ObjectImpl for Signaller {}
