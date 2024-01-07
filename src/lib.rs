use std::time::{Duration, Instant};

use actix::{Actor, ActorContext, AsyncContext, Handler, Message, StreamHandler};
use actix_web::{web, HttpRequest, HttpResponse};
use actix_web_actors::ws;
use tokio::{sync::watch, task::AbortHandle};

pub struct NotifierAppData {
    sender: watch::Sender<NotifyMessage>,
}

impl NotifierAppData {
    pub fn new() -> Self {
        Default::default()
    }

    // since SendError only happens in one case (there are no receivers),
    // we can omit the error type and just return NotifyMessage
    pub async fn send(&self, message: NotifyMessage) -> Result<(), NotifyMessage> {
        self.sender.send(message).map_err(|err| err.0)
    }
}

impl Default for NotifierAppData {
    fn default() -> Self {
        Self {
            sender: watch::channel(NotifyMessage::NoPayload).0,
        }
    }
}

pub trait NotifierAppDataWrapper {
    fn get_notifier_app_data(&self) -> &NotifierAppData;
}

#[macro_export]
macro_rules! impl_notifier_app_data_wrapper {
    ($name:ident) => {
        #[derive(Default)]
        pub struct $name($crate::NotifierAppData);

        impl $name {
            pub fn new() -> Self {
                Self($crate::NotifierAppData::new())
            }
        }

        impl std::ops::Deref for $name {
            type Target = $crate::NotifierAppData;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl $crate::NotifierAppDataWrapper for $name {
            fn get_notifier_app_data(&self) -> &actix_notifier::NotifierAppData {
                &self.0
            }
        }
    };
}

/// Allows creating a websocket handler with custom durations
/// for the heartbeat interval and the heartbeat timeout (both in milliseconds).
/// Keep in mind that a ping is not necessarily sent every heartbeat interval,
/// because it waits for the client to sent a pong and only after the pong is received
/// the interval checks if the delta of the current time and the time of the last pong
/// are larger than the specified duration.
pub async fn websocket_handler_with_durations<
    B: NotifierAppDataWrapper,
    const HBI: u64,
    const HBT: u64,
>(
    req: HttpRequest,
    stream: web::Payload,
    app_data: web::Data<B>,
) -> Result<HttpResponse, actix_web::Error> {
    let rx = app_data.get_notifier_app_data().sender.subscribe();

    let notifiy_websocket = NotifyWebsocket::<HBI, HBT> {
        rx: Some(rx),
        abort_handle: None,
        heart_beat_state: HeartBeatState::default(),
    };
    ws::start(notifiy_websocket, &req, stream)
}

// since this is a convenient wrapper function, which provides default values for the duration
// constants, we can inline it
#[inline(always)]
/// creates a websocket handler with a heartbeat interval of 60 seconds and a heartbeat timeout of 20 seconds
pub async fn websocket_handler<B: NotifierAppDataWrapper>(
    req: HttpRequest,
    stream: web::Payload,
    app_data: web::Data<B>,
) -> Result<HttpResponse, actix_web::Error> {
    websocket_handler_with_durations::<_, 60_000, 20_000>(req, stream, app_data).await
}

enum HeartBeatState {
    // the last time client has answered by "ponging"
    LastPong(Instant),
    // the bytes we pinged and when we did it
    // alternatively use an array instead of vec
    Ping(Vec<u8>, Instant),
}

impl Default for HeartBeatState {
    fn default() -> Self {
        let now = std::time::Instant::now();
        Self::LastPong(now)
    }
}

// HBI is the heartbeat interval in milliseconds
// HBT is the heartbeat timeout in milliseconds
struct NotifyWebsocket<const HBI: u64, const HBT: u64> {
    // this needs to be an option so that we can take it out and pass it to the tokio task
    rx: Option<watch::Receiver<NotifyMessage>>,
    abort_handle: Option<AbortHandle>,
    heart_beat_state: HeartBeatState,
}

impl<const HBI: u64, const HBT: u64> Drop for NotifyWebsocket<HBI, HBT> {
    fn drop(&mut self) {
        if let Some(abort_handle) = self.abort_handle.take() {
            abort_handle.abort();
        }
    }
}

#[derive(Clone, Debug)]
pub enum NotifyMessage {
    /// NoPayload will send an empty text ("")
    NoPayload,
    Text(String),
    /// allows to send a utf-8 string without allocation
    StaticText(&'static str),
    Binary(Vec<u8>),
    /// allows to send bytes without allocation
    StaticBinary(&'static [u8]),
}

impl Message for NotifyMessage {
    type Result = ();
}

impl<const HBI: u64, const HBT: u64> Handler<NotifyMessage> for NotifyWebsocket<HBI, HBT> {
    type Result = ();

    fn handle(&mut self, message: NotifyMessage, ctx: &mut Self::Context) {
        match message {
            NotifyMessage::NoPayload => {
                ctx.text("");
            }
            NotifyMessage::Text(text) => {
                ctx.text(text);
            }
            NotifyMessage::StaticText(text) => {
                ctx.text(text);
            }
            NotifyMessage::Binary(binary) => {
                ctx.binary(binary);
            }
            NotifyMessage::StaticBinary(binary) => {
                ctx.binary(binary);
            }
        }
    }
}

impl<const HBI: u64, const HBT: u64> Actor for NotifyWebsocket<HBI, HBT> {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let loop_address = ctx.address();

        // see why the rx is an option in the struct
        let mut loop_receiver = match self.rx.take() {
            Some(rx) => rx,
            None => {
                return;
            }
        };

        let loop_join_handle = tokio::spawn(async move {
            loop {
                let message = match loop_receiver.changed().await {
                    Ok(_) => loop_receiver.borrow_and_update().clone(),
                    // error only occurs if the receiver has been dropped
                    // and if that happened self has been dropped already
                    Err(_) => {
                        break;
                    }
                };
                loop_address.do_send(message);
            }
        });
        self.abort_handle = Some(loop_join_handle.abort_handle());

        // it is not necessary to cancel the run_interval manually, since it won't be called
        // anymore after self has been dropped
        ctx.run_interval(Duration::from_millis(HBI), |act, ctx| {
            let now = Instant::now();
            // the process of pinging and receiving the pong is not instant
            // and thus the interval might be run again,
            // even though the specified duration has not passed yet.
            // to prevent this we add a little bit of tolerance
            let now_including_tolerance = now + Duration::from_millis(3_000);
            let heart_beat_state = &mut act.heart_beat_state;
            match heart_beat_state {
                HeartBeatState::LastPong(last_pong) => {
                    if now_including_tolerance.saturating_duration_since(*last_pong)
                        >= Duration::from_millis(HBI)
                    {
                        // TODO: what should we send here?
                        let bytes = b"ping";
                        ctx.ping(bytes);
                        *heart_beat_state = HeartBeatState::Ping(bytes.to_vec(), now);
                    }
                }
                HeartBeatState::Ping(_, last_ping) => {
                    if now_including_tolerance.saturating_duration_since(*last_ping)
                        >= Duration::from_millis(HBT)
                    {
                        ctx.close(Some(ws::CloseReason {
                            // TODO: is this the proper code?
                            code: ws::CloseCode::Away,
                            description: Some("heartbeat timeout".to_string()),
                        }));
                        ctx.stop();
                    }
                }
            }
        });
    }
}

impl<const HBI: u64, const HBT: u64> StreamHandler<Result<ws::Message, ws::ProtocolError>>
    for NotifyWebsocket<HBI, HBT>
{
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match msg {
            Err(_) => {
                ctx.stop();
                return;
            }
            Ok(msg) => msg,
        };

        match msg {
            ws::Message::Ping(msg) => {
                ctx.pong(&msg);
            }
            ws::Message::Close(reason) => {
                ctx.close(reason);
                ctx.stop();
            }
            // TODO: research more about continuation and implement it properly
            ws::Message::Continuation(_) => {
                ctx.stop();
            }
            ws::Message::Text(_) => {}
            ws::Message::Pong(msg) => {
                let heart_beat_state = &mut self.heart_beat_state;
                match heart_beat_state {
                    HeartBeatState::Ping(bytes, _) => {
                        if bytes == &msg {
                            *heart_beat_state = HeartBeatState::LastPong(Instant::now());
                        }
                        // TODO: what should we do if the bytes are not the same?
                    }
                    // we received a pong without sending a ping, so we just ignore it
                    HeartBeatState::LastPong(_) => {}
                }
            }
            ws::Message::Binary(_) => {}
            ws::Message::Nop => {}
        }
    }
}
