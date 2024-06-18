use shvclient::appnodes::DotAppNode;
use tokio::sync::RwLock;

use clap::Parser;
use futures::{select, FutureExt};
use log::*;
use shv::metamethod::MetaMethod;
use shv::{client::ClientConfig, util::parse_log_verbosity};
use shv::{RpcMessage, RpcMessageMetaTags};
use shvclient::{no_response, MethodsGetter, RequestHandler};
use shvclient::clientnode::{ClientNode, PROPERTY_METHODS, SIG_CHNG};
use shvclient::{ClientCommand, ClientEvent, ClientEventsReceiver, Route, Sender, AppData};
use simple_logger::SimpleLogger;

#[derive(Parser, Debug)]
//#[structopt(name = "device", version = env!("CARGO_PKG_VERSION"), author = env!("CARGO_PKG_AUTHORS"), about = "SHV call")]
struct Opts {
    /// Config file path
    #[arg(long)]
    config: Option<String>,
    /// Create default config file if one specified by --config is not found
    #[arg(short, long)]
    create_default_config: bool,
    ///Url to connect to, example tcp://admin@localhost:3755?password=dj4j5HHb, localsocket:path/to/socket
    #[arg(short = 's', long)]
    url: Option<String>,
    #[arg(short = 'i', long)]
    device_id: Option<String>,
    /// Mount point on broker connected to, note that broker might not accept any path.
    #[arg(short, long)]
    mount: Option<String>,
    /// Device tries to reconnect to broker after this interval, if connection to broker is lost.
    /// Example values: 1s, 1h, etc.
    #[arg(short, long)]
    reconnect_interval: Option<String>,
    /// Client should ping broker with this interval. Broker will disconnect device, if ping is not received twice.
    /// Example values: 1s, 1h, etc.
    #[arg(long, default_value = "1m")]
    heartbeat_interval: String,
    /// Verbose mode (module, .)
    #[arg(short, long)]
    verbose: Option<String>,
}

fn init_logger(cli_opts: &Opts) {
    let mut logger = SimpleLogger::new();
    logger = logger.with_level(LevelFilter::Info);
    if let Some(module_names) = &cli_opts.verbose {
        for (module, level) in parse_log_verbosity(module_names, module_path!()) {
            logger = logger.with_module_level(module, level);
        }
    }
    logger.init().unwrap();
}

fn load_client_config(cli_opts: &Opts) -> shv::Result<ClientConfig> {
    let mut config = if let Some(config_file) = &cli_opts.config {
        ClientConfig::from_file_or_default(config_file, cli_opts.create_default_config)?
    } else {
        Default::default()
    };
    if let Some(url) = &cli_opts.url {
        config.url = url.clone()
    }
    config.device_id = cli_opts.device_id.clone();
    config.mount = cli_opts.mount.clone();
    config.reconnect_interval = cli_opts.reconnect_interval.clone();
    config.heartbeat_interval = cli_opts.heartbeat_interval.clone();
    Ok(config)
}

type State = RwLock<i32>;

async fn emit_chng_task(
    client_cmd_tx: Sender<ClientCommand>,
    mut client_evt_rx: ClientEventsReceiver,
    app_data: AppData<State>,
) -> shv::Result<()> {
    info!("signal task started");

    let mut cnt = 0;
    let mut emit_signal = true;
    loop {
        select! {
            rx_event = client_evt_rx.recv_event().fuse() => match rx_event {
                Ok(ClientEvent::Connected) => {
                    emit_signal = true;
                    warn!("Device connected");
                },
                Ok(ClientEvent::Disconnected) => {
                    emit_signal = false;
                    warn!("Device disconnected");
                },
                Err(err) => {
                    error!("Device event error: {err}");
                    return Ok(());
                },
            },
            _ = futures_time::task::sleep(futures_time::time::Duration::from_secs(3)).fuse() => { }

        }
        if emit_signal {
            let sig = RpcMessage::new_signal("status/delayed", SIG_CHNG, Some(cnt.into()));
            // dev_cmd_tx.send(DeviceCommand::SendMessage { message: sig }).await?;
            client_cmd_tx.unbounded_send(ClientCommand::SendMessage { message: sig })?;
            info!("signal task emits a value: {cnt}");
            cnt += 1;
        }
        let state = app_data.read().await;
        info!("state: {state}");
    }
}

#[tokio::main]
pub(crate) async fn main() -> shv::Result<()> {
    let cli_opts = Opts::parse();
    init_logger(&cli_opts);

    log::info!("=====================================================");
    log::info!("{} starting", std::module_path!());
    log::info!("=====================================================");

    let client_config = load_client_config(&cli_opts).expect("Invalid config");

    let counter = AppData::new(RwLock::new(-10));
    let cnt = counter.clone();

    let app_tasks = move |client_cmd_tx, client_evt_rx| {
        tokio::task::spawn(emit_chng_task(client_cmd_tx, client_evt_rx, counter));
    };

    async fn dyn_methods_getter(_path: String, _: Option<AppData<RwLock<i32>>>) -> Option<Vec<&'static MetaMethod>> {
        Some(PROPERTY_METHODS.iter().collect())
    }
    async fn dyn_handler(_request: RpcMessage, _client_cmd_tx: Sender<ClientCommand>) {
    }

    let stateless_node = shvclient::fixed_node!{
        device_handler(request, client_cmd_tx) {
            "something" [IsGetter, Browse] (param: Int) => {
                println!("param: {}", param);
                Some(Ok(shv::RpcValue::from("name result")))
            }
            "42" [IsGetter, Browse] => {
                Some(Ok(shv::RpcValue::from(42)))
            }
        }
    };

    let delay_node = shvclient::fixed_node!(
        delay_handler<State>(request, client_cmd_tx, app_data) {
            "getDelayed" [IsGetter, Browse] => {
                let mut resp = request.prepare_response().unwrap_or_default();
                tokio::task::spawn(async move {
                    let mut app_data = app_data;
                    let mut counter = app_data
                        .as_mut()
                        .expect("Missing state for delay node")
                        .write()
                        .await;
                    let ret_val = {
                        *counter += 1;
                        *counter
                    };
                    drop(counter);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    resp.set_result(ret_val);
                    if let Err(e) = client_cmd_tx.unbounded_send(ClientCommand::SendMessage { message: resp }) {
                        error!("delay_node_process_request: Cannot send response ({e})");
                    }
                });

                // The response is sent in the task above, so we need
                // to tell the library to not send any response.
                no_response!()

                // Otherwise, return either RpcValue or RpcError
                // Some(Ok(shv::RpcValue::from(true)))
            }
        }
    );

    shvclient::Client::new(DotAppNode::new("simple_device_tokio"))
        .mount("stateless", stateless_node)
        .mount("status/delayed", delay_node)
        .mount("status/dyn", ClientNode::dynamic(
                MethodsGetter::new(dyn_methods_getter),
                RequestHandler::stateless(dyn_handler)))
        .with_app_data(cnt)
        .run_with_init(&client_config, app_tasks)
        // .run(&client_config)
        .await
}
