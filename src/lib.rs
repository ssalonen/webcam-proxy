#![warn(unused_extern_crates)]
use core::convert::Infallible;
use core::fmt::Display;
use std::time::{Duration, Instant};

use failure::Fail;
#[macro_use]
extern crate lazy_static;

use tracing::{debug, instrument};

use bytes::Bytes;
use futures::prelude::*;
use futures::StreamExt;
use futures_locks_pre::RwLock;
use hyper::{Body, Request, Response, StatusCode, Uri};
use hyper_tls::HttpsConnector;
use image::jpeg::JPEGEncoder;
use image::Rgba;
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut};
use imageproc::rect::Rect;
use rusttype::{Font, Scale};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::sync::watch::{Receiver, Sender};
use tokio::time::{delay_for, timeout};

// use tokio::prelude::*;

static DOWNLOAD_DELAY: Duration = Duration::from_secs(1);
static DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5);
static IMAGE_STALE_THRESHOLD: Duration = Duration::from_secs(90);

static WIDTH: u32 = 1280;
static HEIGHT: u32 = 960;
lazy_static! {
    static ref FONT: Font<'static> = {
        let font_data: &[u8] = include_bytes!("../fonts/DejaVuSansMono.ttf");
        let font: Font<'static> = Font::from_bytes(font_data).expect("Font error");
        font
    };
    static ref WHITE_RGBA: Rgba<u8> = Rgba([255u8, 255u8, 255u8, 255u8]);
    static ref BLACK_RGBA: Rgba<u8> = Rgba([0u8, 0u8, 0u8, 255u8]);
}

// type Clients = HashMap<usize, RwLock<Client>>;
type HttpClient = hyper::Client<HttpsConnector<hyper::client::connect::HttpConnector>>;

pub struct Server {
    // clients: Mutex<Clients>,
    download_url: Uri,
    image_data: RwLock<ImageData>,
    auth: BTreeMap<String, String>,
    broadcast_rx: Receiver<Vec<u8>>,
    broadcast_tx: Sender<Vec<u8>>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Server(\"{:?}\")",
            self.download_url.path()[..10].to_owned()
        )
    }
}

pub struct ImageData {
    last_successful_image_raw: Vec<u8>,
    last_successful_image: Vec<u8>,
    last_success: Option<Instant>,
    last_error: Option<Instant>,
    last_snapshot_sent: Option<Instant>,
}

#[derive(Debug, Fail)]
enum DownloadError {
    #[fail(display = "generic download error")]
    Generic {},
    #[fail(display = "nonsuccesfull http status: {}", status)]
    NonSuccessfullStatus { status: u16 },
    #[fail(display = "download timeout")]
    Timeout {},
    #[fail(display = "downloading error")]
    DownloadingError {},
}
// impl std::error::Error for DownloadError {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServerError();

impl std::error::Error for ServerError {
    fn description(&self) -> &str {
        "Generic error"
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Generic error occurred")
    }
}

impl Server {
    pub fn new(download_url: Uri, auth: BTreeMap<String, String>) -> Server {
        use image::RgbImage;
        let image_buffer = RgbImage::new(WIDTH, HEIGHT);
        let mut image_jpeg_buffer = vec![];
        let mut encoder = JPEGEncoder::new(&mut image_jpeg_buffer);
        encoder
            .encode(&image_buffer, WIDTH, HEIGHT, image::ColorType::Rgb8)
            .expect("Could not create blank start image!");
        let (broadcast_tx, broadcast_rx) = watch::channel(image_jpeg_buffer.clone());
        Server {
            // clients: Mutex::new(HashMap::new()),
            download_url,
            auth,
            broadcast_rx,
            broadcast_tx,
            image_data: RwLock::new(ImageData {
                last_successful_image_raw: image_jpeg_buffer.clone(),
                last_successful_image: image_jpeg_buffer,
                last_success: Default::default(),
                last_error: Default::default(),
                last_snapshot_sent: Default::default(),
            }),
        }
    }

    #[instrument]
    async fn check_image_stale_async(&self) -> Result<(), image::ImageError> {
        let stale = {
            let image_data = self.image_data.clone().read().await;
            match image_data.last_success {
                Some(last_success) => last_success.elapsed() > IMAGE_STALE_THRESHOLD,
                _ => true,
            }
        };
        debug!("Image stale: {:?}", stale);
        if stale {
            let mut image_data = self.image_data.clone().write().await;
            let image_bits = image_data.last_successful_image_raw.clone();
            let mut image =
                image::load_from_memory_with_format(&image_bits, image::ImageFormat::Jpeg)?;
            draw_filled_rect_mut(
                &mut image,
                Rect::at(500, 500).of_size(100, 100),
                *BLACK_RGBA,
            );
            draw_text_mut(
                &mut image,
                *WHITE_RGBA,
                500,
                500,
                Scale::uniform(24.0),
                &FONT,
                "STALE!",
            );
            let last_successful_image = &mut (*image_data).last_successful_image;
            last_successful_image.truncate(0);
            let mut encoder = JPEGEncoder::new(&mut image_data.last_successful_image);
            encoder.encode(&image.to_rgb(), WIDTH, HEIGHT, image::ColorType::Rgb8)?;
        }
        Ok(())
    }

    #[instrument]
    async fn download_picture_async_loop(&'static self, http_client: Arc<HttpClient>) {
        loop {
            // We ignore download errors, could have something
            // more fancy here, e.g. exponential retry on errors
            let _ = self.download_picture_async(&http_client).await;
            delay_for(DOWNLOAD_DELAY).await;
        }
    }

    #[instrument]
    async fn download_picture_async(
        &'static self,
        http_client: &HttpClient,
    ) -> Result<(), DownloadError> {
        if let Err(e) = self._download_picture_async(http_client).await {
            {
                let mut image_data = self.image_data.clone().write().await;
                image_data.last_error = Some(Instant::now());
            }
            Err(e)
        } else {
            Ok(())
        }
    }

    #[instrument]
    async fn _download_picture_async(
        &'static self,
        http_client: &HttpClient,
    ) -> Result<(), DownloadError> {
        let response = http_client
            .get(self.download_url.clone())
            .await
            .map_err(|_| DownloadError::Generic {})?;

        let (parts, body) = response.into_parts();
        let body: hyper::Body = match parts.status {
            StatusCode::OK => {
                debug!("Download OK");
                Ok(body)
            }
            status => {
                debug!("Status error");
                Err(DownloadError::NonSuccessfullStatus {
                    status: status.as_u16(),
                })
            }
        }?;
        debug!("Consuming body");
        let body_data: Vec<u8> = match timeout(
            DOWNLOAD_TIMEOUT,
            body.try_fold(Vec::new(), |mut acc, chunk| async move {
                acc.extend_from_slice(&*chunk);
                Ok(acc)
            }),
        )
        .await
        {
            Err(_) => Err(DownloadError::Timeout {}),
            Ok(Err(_)) => Err(DownloadError::DownloadingError {}),
            Ok(Ok(body_data)) => Ok(body_data),
        }?;
        debug!("Body consumed, storing image_data");

        // let mut image_data = try_ready!(self.image_data.write().poll().or(Err(())));
        {
            let mut image_data = self.image_data.clone().write().await;
            image_data.last_successful_image_raw = body_data.clone();
            image_data.last_successful_image = body_data;
            image_data.last_success = Some(Instant::now());
        }
        debug!("Image data consumed");

        self.update_all_mjpeg().await;
        Ok(())
    }

    #[instrument]
    async fn update_all_mjpeg(&self) {
        let image_data = self.image_data.clone().read().await;
        let image = image_data.last_successful_image.clone();
        self.broadcast_tx.broadcast(image).unwrap();
    }

    pub fn run_server(&'static self, listen: SocketAddr) -> impl Future {
        use hyper::server::conn::AddrStream;
        use hyper::service::{make_service_fn, service_fn};

        let https = HttpsConnector::new();
        let http_client = Arc::new(hyper::Client::builder().build::<_, hyper::Body>(https));

        let handler = move |req: Request<Body>| self.serve(req);
        let http_server = hyper::Server::bind(&listen).serve(make_service_fn(
            move |_socket: &AddrStream| async move { Ok::<_, Infallible>(service_fn(handler)) },
        ));
        let maintenance = async move {
            loop {
                // self.remove_stale_clients().await;
                delay_for(Duration::from_secs(1)).await;
            }
        };
        let stale_monitor = async move {
            loop {
                self.check_image_stale_async().await.unwrap();
                self.update_all_mjpeg().await;
                delay_for(Duration::from_secs(5)).await;
            }
        };

        let download_loop = self.download_picture_async_loop(http_client);
        future::join4(http_server, maintenance, stale_monitor, download_loop)
    }

    #[instrument]
    pub async fn serve(
        &'static self,
        request: Request<Body>,
    ) -> Result<Response<Body>, ServerError> {
        debug!("headers: {:?}", request.headers());
        // Extract channel from uri path (last segment)
        if let Some(query) = request.uri().query() {
            let parsed_args = url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect::<HashMap<String, String>>();
            let user = parsed_args.get("usr");
            let password = parsed_args.get("pwd");
            let auth_ok = match (user, password) {
                (Some(user), Some(password)) => self.auth.get(user) == Some(password),
                _ => false,
            };
            if !auth_ok {
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::from("Unauthorized"))
                    .expect("Could not create response"));
            }
        // debug!("parsedArgs {:?}", parsed_args);
        } else {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid query string"))
                .expect("Could not create response"));
        }
        debug!("path {:?}", request.uri().path());
        match request.uri().path() {
            "/snapshot" => {
                debug!("Acquiring image_data lock");
                let image_bytes = {
                    let mut image_data = self.image_data.clone().write().await;
                    image_data.last_snapshot_sent = Some(Instant::now());
                    image_data.last_successful_image.clone()
                };
                debug!("Acquired image_data lock");

                Ok(Response::builder()
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "close")
                    .header("Content-Type", "image/jpeg")
                    .header("Pragma", "no-cache")
                    .body(Body::from(image_bytes))
                    .expect("Could not create response"))
            }
            "/stream" => {
                let image_stream = self
                    .broadcast_rx
                    .clone()
                    .map(|image| {
                        stream::iter(vec![
                            Bytes::from("--BOUNDARY\r\n"),
                            Bytes::from("Content-Type: image/jpeg\r\n"),
                            Bytes::from(format!("Content-Length: {}\r\n", image.len())),
                            Bytes::from("\r\n"),
                            Bytes::from(image),
                            Bytes::from("\r\n"),
                        ])
                        .map(|bytes| -> Result<Bytes, Infallible> { Ok(bytes) })
                    })
                    .flatten();
                let body = Body::wrap_stream(image_stream);
                // self.add_client(kill_tx).await;
                Ok(Response::builder()
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "close")
                    .header(
                        "Content-Type",
                        "multipart/x-mixed-replace; boundary=BOUNDARY",
                    )
                    .header("Pragma", "no-cache")
                    .body(body)
                    .expect("Could not create response"))
            }
            _ => Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid path"))
                .expect("Could not create response")),
        }
    }
}
