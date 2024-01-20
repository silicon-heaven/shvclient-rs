pub mod appnodes;
pub mod shvnode;

use crate::shvnode::{find_longest_prefix, process_local_dir_ls, ShvNode, METH_PING};
use async_std::future;
use async_std::io::BufReader;
use async_std::net::TcpStream;
use async_std::sync::Mutex;
use duration_str::parse;
use futures::{select, AsyncReadExt, Future, FutureExt};
use log::*;
use shv::client::{ClientConfig, LoginParams};
use shv::metamethod::MetaMethod;
use shv::rpcframe::RpcFrame;
use shv::rpcmessage::{RpcError, RpcErrorCode};
use shv::util::login_from_url;
use shv::{client, RpcMessage, RpcMessageMetaTags, RpcValue};
use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

pub type Sender<K> = async_std::channel::Sender<K>;
pub type Receiver<K> = async_std::channel::Receiver<K>;

#[derive(Clone, Default)]
pub struct DeviceState(pub Option<Arc<Mutex<Box<dyn Any + Send + Sync>>>>);

impl DeviceState {
    pub fn new<T: Any + Send + Sync>(val: T) -> Self {
        DeviceState(Some(Arc::new(Mutex::new(Box::new(val)))))
    }
}

#[derive(Clone)]
pub struct RequestData {
    pub mount_path: String,
    pub request: RpcMessage,
}

pub enum RpcCommand {
    SendMessage {
        message: RpcMessage,
    },
    RpcCall {
        request: RpcMessage,
        response_sender: Sender<RpcFrame>,
    },
}

pub enum RequestResult {
    Response(RpcValue),
    Error(RpcError),
}

type HandlerOutcome = ();

pub type HandlerFn = Box<
    dyn Fn(
        RequestData,
        Sender<RpcCommand>,
        DeviceState,
    ) -> Pin<Box<dyn Future<Output = HandlerOutcome>>>,
>;

pub struct Route {
    handler: HandlerFn,
    methods: Vec<String>,
}

impl Route {
    pub fn new<F, O, I>(methods: I, handler: F) -> Self
    where
        F: 'static + Fn(RequestData, Sender<RpcCommand>, DeviceState) -> O,
        O: 'static + Future<Output = HandlerOutcome>,
        I: IntoIterator,
        I::Item: Into<String>,
    {
        Self {
            handler: Box::new(move |r, s, d| Box::pin(handler(r, s, d))),
            methods: methods.into_iter().map(|x| x.into()).collect(),
        }
    }
}

#[derive(Default)]
pub struct ShvDevice {
    mounts: BTreeMap<String, ShvNode>,
    state: DeviceState,
}

impl ShvDevice {
    pub fn mount<P, M, R>(&mut self, path: P, defined_methods: M, routes: R) -> &mut Self
    where
        P: AsRef<str>,
        M: Into<Vec<MetaMethod>>,
        R: Into<Vec<Route>>,
    {
        let path = path.as_ref();
        let node = ShvNode::new(defined_methods).add_routes(routes.into());
        self.mounts.insert(path.into(), node);
        self
    }

    pub fn register_state<T: Any + Send + Sync>(&mut self, state: T) -> &mut Self {
        self.state = DeviceState::new(state);
        self
    }

    pub fn run(&self, config: &ClientConfig) -> shv::Result<()> {
        async_std::task::block_on(self.try_main_reconnect(config))
    }

    async fn try_main_reconnect(&self, config: &ClientConfig) -> shv::Result<()> {
        if let Some(time_str) = &config.reconnect_interval {
            match parse(time_str) {
                Ok(interval) => {
                    info!("Reconnect interval set to: {:?}", interval);
                    loop {
                        match self.try_main(config).await {
                            Ok(_) => {
                                info!("Finished without error");
                                return Ok(());
                            }
                            Err(err) => {
                                error!("Error in main loop: {err}");
                                info!("Reconnecting after: {:?}", interval);
                                async_std::task::sleep(interval).await;
                            }
                        }
                    }
                }
                Err(err) => {
                    return Err(err.into());
                }
            }
        } else {
            self.try_main(config).await
        }
    }

    async fn try_main(&self, config: &ClientConfig) -> shv::Result<()> {
        let url = Url::parse(&config.url)?;
        let (scheme, host, port) = (
            url.scheme(),
            url.host_str().unwrap_or_default(),
            url.port().unwrap_or(3755),
        );
        if scheme != "tcp" {
            return Err(format!("Scheme {scheme} is not supported yet.").into());
        }
        let address = format!("{host}:{port}");
        // Establish a connection
        info!("Connecting to: {address}");
        let stream = TcpStream::connect(&address).await?;
        let (reader, mut writer) = stream.split();

        let mut brd = BufReader::new(reader);
        let mut frame_reader = shv::connection::FrameReader::new(&mut brd);
        let mut frame_writer = shv::connection::FrameWriter::new(&mut writer);

        // login
        let (user, password) = login_from_url(&url);
        let heartbeat_interval = config.heartbeat_interval_duration()?;
        let login_params = LoginParams {
            user,
            password,
            mount_point: (&config.mount.clone().unwrap_or_default()).to_owned(),
            device_id: (&config.device_id.clone().unwrap_or_default()).to_owned(),
            heartbeat_interval,
            ..Default::default()
        };

        info!("Connected OK");
        info!("Heartbeat interval set to: {:?}", heartbeat_interval);
        client::login(&mut frame_reader, &mut frame_writer, &login_params).await?;

        let mut pending_rpc_calls: HashMap<i64, Sender<RpcFrame>> = HashMap::new();
        let (rpc_command_sender, rpc_command_receiver) =
            async_std::channel::unbounded::<RpcCommand>();
        let mut fut_heartbeat_timeout =
            Box::pin(future::timeout(heartbeat_interval, future::pending::<()>())).fuse();

        loop {
            let fut_receive_frame = frame_reader.receive_frame();
            select! {
                heartbeat_timeout = fut_heartbeat_timeout => if let Err(_) = heartbeat_timeout {
                    // send heartbeat
                    let message = RpcMessage::new_request(".app", METH_PING, None);
                    rpc_command_sender.send(RpcCommand::SendMessage{ message }).await?;
                },
                rpc_command_result = rpc_command_receiver.recv().fuse() => match rpc_command_result {
                    Ok(rpc_command) => {
                        match rpc_command {
                            RpcCommand::SendMessage{message} => {
                                // reset heartbeat timer
                                fut_heartbeat_timeout = Box::pin(future::timeout(heartbeat_interval, future::pending::<()>())).fuse();
                                frame_writer.send_message(message).await?;
                            },
                            RpcCommand::RpcCall{request, response_sender} => {
                                let req_id = request.request_id().expect("request_id must be set");
                                if pending_rpc_calls.insert(req_id, response_sender).is_some() {
                                    panic!("request_id {req_id} for async RpcCall has been already registered");
                                }
                                rpc_command_sender.send(RpcCommand::SendMessage{ message: request }).await?;
                            }
                        }
                    },
                    Err(err) => {
                        panic!("Couldn't get RpcCommand from the channel: {err}");
                    },
                },
                receive_frame_result = fut_receive_frame.fuse() => match receive_frame_result {
                    Ok(None) => {
                        return Err("Broker socket closed".into());
                    }
                    Ok(Some(frame)) => {
                        if frame.is_request() {
                            if let Ok(mut rpcmsg) = frame.to_rpcmesage() {
                                if let Ok(mut resp) = rpcmsg.prepare_response() {
                                    let shv_path = frame.shv_path().unwrap_or_default();
                                    let local_result = process_local_dir_ls(&self.mounts, &frame);
                                    match local_result {
                                        None => {
                                            if let Some((mount, path)) = find_longest_prefix(&self.mounts, &shv_path) {
                                                rpcmsg.set_shvpath(path);
                                                let node = self.mounts.get(mount).unwrap();
                                                node.process_request(
                                                    RequestData { mount_path: mount.into(), request: rpcmsg },
                                                    rpc_command_sender.clone(),
                                                    self.state.clone()).await;
                                            } else {
                                                let method = frame.method().unwrap_or_default();
                                                resp.set_error(RpcError::new(RpcErrorCode::MethodNotFound, &format!("Invalid shv path {}:{}()", shv_path, method)));
                                                rpc_command_sender.send(RpcCommand::SendMessage { message: resp }).await?;
                                            }
                                        }
                                        Some(result) => {
                                            match result {
                                                RequestResult::Response(r) => {
                                                    resp.set_result(r);
                                                    rpc_command_sender.send(RpcCommand::SendMessage { message: resp }).await?;
                                                }
                                                RequestResult::Error(e) => {
                                                    resp.set_error(e);
                                                    rpc_command_sender.send(RpcCommand::SendMessage { message: resp }).await?;
                                                }
                                            }
                                        }
                                    };
                                } else {
                                    warn!("Invalid request frame received.");
                                }
                            } else {
                                warn!("Invalid shv request");
                            }
                        } else if frame.is_response() {
                            if let Some(req_id) = frame.request_id() {
                                if let Some(response_sender) = pending_rpc_calls.remove(&req_id) {
                                    response_sender.send(frame).await?;
                                }
                            }
                        } else if frame.is_signal() {
                            warn!("Signal frame handling to be implemented");
                        }
                    }
                    Err(e) => {
                        error!("Receive frame error - {e}");
                    }
                }
            }
        }
    }
}