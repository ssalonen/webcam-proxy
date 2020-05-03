#![warn(unused_extern_crates)]
#[macro_use]
extern crate lazy_static;

use bytes::Bytes;
use chrono::prelude::*;
use core::convert::Infallible;
use core::time::Duration as StdDuration;
use futures::prelude::*;
use futures::StreamExt;
use futures_locks_pre::RwLock;
use humantime::format_duration;
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use time::Duration as OldDuration;
use tokio::sync::watch;
use tokio::sync::watch::{Receiver, Sender};
use tokio::time::{delay_for, timeout};
use tracing::{debug, info, instrument};

// TODO: adjust if no clients (no active streams and some time since last snapshot)
static DOWNLOAD_DELAY: StdDuration = StdDuration::from_secs(1);
static DOWNLOAD_TIMEOUT: StdDuration = StdDuration::from_secs(5);
static IMAGE_STALE_THRESHOLD: StdDuration = StdDuration::from_secs(90);

// FIXME: not static over all apps
static STREAMS_ACTIVE: AtomicUsize = AtomicUsize::new(0);
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
type HttpClient = hyper::Client<HttpsConnector<hyper::client::connect::HttpConnector>>;

#[derive(Default)]
pub struct StreamTracker<S, O, E>(S)
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static;

impl<S, O, E> StreamTracker<S, O, E>
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static,
{
    pub fn new(stream: S) -> Self {
        STREAMS_ACTIVE.fetch_add(1, Ordering::SeqCst);
        info!("{}", "new stream tracker");
        Self(stream)
    }
}

impl<S, O, E> Drop for StreamTracker<S, O, E>
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static,
{
    fn drop(&mut self) {
        STREAMS_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        info!("{}", "drop stream tracker");
    }
}

impl<S, O, E> Stream for StreamTracker<S, O, E>
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static,
{
    type Item = Result<O, E>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // safe since we never move nor leak &mut
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.0) };
        inner.poll_next(cx)
    }
}

pub struct Server {
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
    stale: bool,
    last_success: Option<DateTime<Local>>,
}

#[derive(Debug, Error)]
enum DownloadError {
    #[error("nonsuccesfull http status: {status:?}")]
    NonSuccessfullStatus { status: u16 },
    #[error("download timeout")]
    Timeout,
    #[error("generic download error")]
    DownloadingError,
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
            download_url,
            auth,
            broadcast_rx,
            broadcast_tx,
            image_data: RwLock::new(ImageData {
                last_successful_image_raw: image_jpeg_buffer.clone(),
                last_successful_image: image_jpeg_buffer,
                last_success: Default::default(),
                stale: false,
            }),
        }
    }

    #[instrument]
    async fn check_image_stale_async(&self) -> Result<(), image::ImageError> {
        let (last_download, stale) = {
            let image_data = self.image_data.clone().read().await;
            (
                image_data.last_success,
                match image_data.last_success {
                    Some(last_success) => {
                        Local::now().signed_duration_since(last_success)
                            > OldDuration::from_std(IMAGE_STALE_THRESHOLD).unwrap()
                    }
                    _ => true,
                },
            )
        };
        debug!("Image stale: {:?}", stale);
        if stale {
            let mut image_data = self.image_data.clone().write().await;
            let image_bits = image_data.last_successful_image_raw.clone();
            image_data.stale = true;
            let mut image =
                image::load_from_memory_with_format(&image_bits, image::ImageFormat::Jpeg)?;
            draw_filled_rect_mut(
                &mut image,
                Rect::at(500, 500).of_size(100, 100),
                *BLACK_RGBA,
            );
            let stale_text: String = match last_download {
                Some(instant) => format!(
                    "STALE. Last image {} ago at {}",
                    format_duration(
                        Local::now()
                            .signed_duration_since(instant)
                            .to_std()
                            .unwrap()
                    ),
                    instant.format("%Y-%m-%d %H:%M:%S").to_string()
                ),
                None => "STALE".to_owned(),
            };
            draw_text_mut(
                &mut image,
                *WHITE_RGBA,
                500,
                500,
                Scale::uniform(24.0),
                &FONT,
                &stale_text,
            );
            let last_successful_image = &mut (*image_data).last_successful_image;
            last_successful_image.truncate(0);
            let mut encoder = JPEGEncoder::new(&mut image_data.last_successful_image);
            encoder.encode(&image.to_rgb(), WIDTH, HEIGHT, image::ColorType::Rgb8)?;
            self.broadcast_tx
                .broadcast(image_data.last_successful_image.clone())
                .unwrap();
        } else {
            // Opportunistic -- acquire write lock only when needed
            let stale = self.image_data.clone().read().await.stale;
            if stale {
                let mut image_data = self.image_data.clone().write().await;
                image_data.stale = false;
            }
        }
        Ok(())
    }

    #[instrument]
    async fn download_picture_async_loop(&'static self, http_client: Arc<HttpClient>) {
        loop {
            // We ignore download errors, could have something
            // more fancy here, e.g. exponential retry on errors
            let before = std::time::Instant::now();
            let _ = self.download_picture_async(&http_client).await;
            let after = std::time::Instant::now();
            if after > before + DOWNLOAD_DELAY {
                // Download immediately again, no delay
            } else {
                delay_for(before + DOWNLOAD_DELAY - after).await;
            }
            info!(
                "STREAMS_ACTIVE: {}",
                STREAMS_ACTIVE.fetch_add(0, Ordering::SeqCst)
            );
        }
    }

    #[instrument]
    async fn download_picture_async(
        &'static self,
        http_client: &HttpClient,
    ) -> Result<(), DownloadError> {
        let response = http_client
            .get(self.download_url.clone())
            .await
            .map_err(|_| DownloadError::DownloadingError)?;

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
            Err(_) => Err(DownloadError::Timeout),
            Ok(Err(_)) => Err(DownloadError::DownloadingError),
            Ok(Ok(body_data)) => Ok(body_data),
        }?;
        debug!("Body consumed, storing image_data");
        {
            let mut image_data = self.image_data.clone().write().await;
            image_data.last_successful_image_raw = body_data.clone();
            image_data.last_successful_image = body_data;
            image_data.last_success = Some(Local::now());
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

        let stale_monitor = async move {
            loop {
                self.check_image_stale_async().await.unwrap();
                self.update_all_mjpeg().await;
                delay_for(std::time::Duration::from_secs(5)).await;
            }
        };

        let download_loop = self.download_picture_async_loop(http_client);
        future::join3(http_server, stale_monitor, download_loop)
    }

    #[instrument]
    pub async fn serve(
        &'static self,
        request: Request<Body>,
    ) -> Result<Response<Body>, Infallible> {
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
                    let image_data = self.image_data.clone().read().await;
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
                let rx_handle = self.broadcast_rx.clone();
                // let weak_ref = std::rc::Weak(std::rc::Rc::new(rx_handle));
                let image_stream = rx_handle
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
                let body =
                    Body::wrap_stream::<_, Bytes, Infallible>(StreamTracker::new(image_stream));
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
