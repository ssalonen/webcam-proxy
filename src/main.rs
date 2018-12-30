#![warn(unused_extern_crates)]

#[macro_use]
extern crate lazy_static;
use serde::Deserialize;
use toml;

use webcam_proxy::Server;

use hyper::Uri;
use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

#[derive(Deserialize, Debug)]
pub struct Config {
    server: ConfigServer,
    webcam: ConfigWebcam,
}

#[derive(Deserialize, Debug)]
pub struct ConfigServer {
    listen: String,
    auth: BTreeMap<String, String>,
}

#[derive(Deserialize, Debug)]
pub struct ConfigWebcam {
    url: String,
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
        Server::new(download_uri, CONFIG.server.auth.clone())
    };
}
fn main() {
    // println!("{:?}", CONFIG);

    let handle = SERVER.spawn(CONFIG.server.listen.parse().expect("Invalid server.listen"));
    println!("Server up!");
    handle.join().expect("Cannot join server thread!");
}
// curl http://localhost:3000/stream/1
