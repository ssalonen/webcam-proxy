#![warn(unused_extern_crates)]

#[macro_use]
extern crate lazy_static;
use serde::Deserialize;

use tracing_subscriber::EnvFilter;

use webcam_proxy::Server;

use chrono_tz::Tz;
use hyper::Uri;
use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use tokio::runtime;

#[derive(Deserialize, Debug)]
pub struct Config {
    server: ConfigServer,
    annotations: Option<ConfigAnnotations>,
    webcam: ConfigWebcam,
}

#[derive(Deserialize, Debug)]
pub struct ConfigServer {
    listen: String,
    auth: BTreeMap<String, String>,
}

#[derive(Deserialize, Debug)]
pub struct ConfigAnnotations {
    tz: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ConfigWebcam {
    url: String,
    save_path: Option<String>,
}

lazy_static! {
    static ref CONFIG: Config = {
        let args: Vec<String> = env::args().collect();
        if args.len() != 2 {
            panic!("Usage: app path/to/config.toml");
        }
        let config_path = &args[1];
        let config_file_path = Path::new(config_path);
        let mut config_file = File::open(config_file_path).expect("Failed to open config");
        let mut contents = String::new();
        config_file
            .read_to_string(&mut contents)
            .expect("Failed to read config");
        toml::from_str(&contents).expect("error while reading config")
    };
    static ref SERVER: Server = {
        let download_uri = Uri::from_str(&CONFIG.webcam.url).expect("Invalid webcam URL");
        let tz: Option<Tz> = CONFIG
            .annotations
            .as_ref()
            .and_then(|a| a.tz.as_ref().map(|t| t.parse().unwrap()));
        Server::new(
            download_uri,
            CONFIG.server.auth.clone(),
            CONFIG.webcam.save_path.as_ref().map(Path::new),
            tz,
        )
    };
}
async fn async_main() {
    // println!("{:?}", CONFIG);

    let handle = SERVER.run_server(CONFIG.server.listen.parse().expect("Invalid server.listen"));
    handle.await;
    println!("Server up!");
}
fn main() {
    // a builder for `FmtSubscriber`.
    let filter = EnvFilter::from_default_env();

    tracing_subscriber::fmt().with_env_filter(filter).init();

    let rt = runtime::Runtime::new().unwrap();
    rt.block_on(async_main());
}
// curl http://localhost:3000/stream/1
