#![warn(unused_extern_crates)]
#[macro_use]
extern crate lazy_static;

use bytes::Bytes;
use chrono::prelude::*;
use core::convert::Infallible;
use core::time::Duration as StdDuration;
use futures::prelude::*;
use futures::StreamExt;
use humantime::format_duration;
use hyper::{Body, Request, Response, StatusCode, Uri};
use hyper_tls::HttpsConnector;
use image::jpeg::JPEGEncoder;
use image::Rgba;
use imageproc::drawing::{draw_filled_rect_mut, draw_text_mut};
use imageproc::rect::Rect;
use img_hash::{HasherConfig, ImageHash};
use rusttype::{Font, Scale};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use time::Duration as OldDuration;
use tokio::sync::watch;
use tokio::sync::watch::{Receiver, Sender};
use tokio::sync::RwLock;
use tokio::time::{delay_for, timeout};
use tracing::{debug, info, instrument, warn};

// TODO: adjust if no clients (no active streams and some time since last snapshot)
static DOWNLOAD_DELAY: StdDuration = StdDuration::from_secs(2);
static DOWNLOAD_TIMEOUT: StdDuration = StdDuration::from_secs(5);
static IMAGE_STALE_THRESHOLD: StdDuration = StdDuration::from_secs(90);

static WIDTH: u32 = 1280;
static HEIGHT: u32 = 720;
//static WIDTH: u32 = 1920;
//static HEIGHT: u32 = 1080;
lazy_static! {
    static ref FONT: Font<'static> = {
        let font_data: &[u8] = include_bytes!("../fonts/DejaVuSansMono.ttf");
        let font: Font<'static> = Font::try_from_bytes(font_data).expect("Font error");
        font
    };
    static ref WHITE_RGBA: Rgba<u8> = Rgba([255u8, 255u8, 255u8, 255u8]);
    static ref BLACK_RGBA: Rgba<u8> = Rgba([0u8, 0u8, 0u8, 255u8]);
}
type HttpClient = hyper::Client<HttpsConnector<hyper::client::connect::HttpConnector>>;

#[derive(Default)]
pub struct StreamTracker<S, O, E>(Arc<AtomicUsize>, S)
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static;

impl<S, O, E> StreamTracker<S, O, E>
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static,
{
    pub fn new(counter: Arc<AtomicUsize>, stream: S) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        info!("{}", "new stream tracker");
        Self(counter, stream)
    }
}

impl<S, O, E> Drop for StreamTracker<S, O, E>
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        info!("{}", "drop stream tracker");
    }
}

impl<S, O, E> Stream for StreamTracker<S, O, E>
where
    S: Stream<Item = Result<O, E>> + Send + Sync + 'static,
{
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // safe since we never move nor leak &mut
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.1) };
        inner.poll_next(cx)
    }
}

pub struct Server {
    download_url: Uri,
    image_data: RwLock<ImageData>,
    auth: BTreeMap<String, String>,
    broadcast_rx: Receiver<Vec<u8>>,
    broadcast_tx: Sender<Vec<u8>>,
    active_streams: Arc<AtomicUsize>,
    save_path: Option<&'static Path>,
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

fn draw_text(image: &mut image::DynamicImage, text: &str, row: u32) {
    draw_filled_rect_mut(
        image,
        Rect::at(0, (HEIGHT - 24 * (row + 1)) as i32).of_size(WIDTH, 24),
        *BLACK_RGBA,
    );
    draw_text_mut(
        image,
        *WHITE_RGBA,
        0,
        HEIGHT - 24 * (row + 1),
        Scale::uniform(24.0),
        &FONT,
        &text,
    );
}

fn encode_image(
    image: &image::DynamicImage,
    dest_image_buf: &mut Vec<u8>,
) -> Result<(), image::ImageError> {
    dest_image_buf.truncate(0);
    let mut encoder = JPEGEncoder::new(dest_image_buf);
    encoder.encode(&image.to_rgb(), WIDTH, HEIGHT, image::ColorType::Rgb8)?;
    Ok(())
}

pub struct ImageData {
    last_successful_image_raw: Vec<u8>,
    last_successful_image: Vec<u8>,
    image_hash: Option<ImageHash>,
    hash_diff: Option<u32>,
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
    pub fn new(
        download_url: Uri,
        auth: BTreeMap<String, String>,
        save_path: Option<&'static Path>,
    ) -> Server {
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
                image_hash: Default::default(),
                hash_diff: Default::default(),
                last_success: Default::default(),
                stale: false,
            }),
            active_streams: Arc::new(AtomicUsize::new(0)),
            save_path,
        }
    }

    #[instrument]
    async fn check_image_stale_async(&self) -> Result<(), image::ImageError> {
        let (last_download, stale) = {
            let image_data = self.image_data.read().await;
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
            {
                let mut image_data = self.image_data.write().await;
                let image_bits = image_data.last_successful_image_raw.clone();
                image_data.stale = true;
                image_data.image_hash = None;
                let mut image =
                    image::load_from_memory_with_format(&image_bits, image::ImageFormat::Jpeg)?;
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
                draw_text(&mut image, &stale_text, 0);
                // let last_successful_image = &mut (*image_data).last_successful_image;
                encode_image(&image, &mut image_data.last_successful_image)?;
            }
            self.update_all_mjpeg().await;
        } else {
            // Opportunistic -- acquire write lock only when needed
            let stale = self.image_data.read().await.stale;
            if stale {
                let mut image_data = self.image_data.write().await;
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
            debug!("Downloading...");
            let _ = self.download_picture_async(&http_client).await;
            let after = std::time::Instant::now();
            debug!("Downloaded in {} s.", (after - before).as_secs());
            if after > before + DOWNLOAD_DELAY {
                // Download immediately again, no delay
            } else {
                delay_for(before + DOWNLOAD_DELAY - after).await;
            }
            info!(
                "Active streams: {}",
                self.active_streams.fetch_add(0, Ordering::SeqCst)
            );
        }
    }

    #[instrument]
    async fn download_picture_async(
        &'static self,
        http_client: &HttpClient,
    ) -> Result<(), DownloadError> {
        let response: Response<Body> =
            match timeout(DOWNLOAD_TIMEOUT, http_client.get(self.download_url.clone())).await {
                Err(_) => Err(DownloadError::Timeout),
                Ok(Err(_)) => Err(DownloadError::DownloadingError),
                Ok(Ok(response)) => Ok(response),
            }?;
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
            let mut image_data = self.image_data.write().await;
            image_data.last_successful_image_raw = body_data.clone();
            image_data.last_success = Some(Local::now());
            let mut image =
                image::load_from_memory_with_format(&body_data, image::ImageFormat::Jpeg)
                    .map_err(|_| DownloadError::DownloadingError)?;
            let hasher = HasherConfig::new().hash_size(16, 16).to_hasher();
            let hash = hasher.hash_image(&image);
            let hash_diff = match &image_data.image_hash {
                Some(prev_hash) => prev_hash.dist(&hash),
                None => 9999,
            };
            let last_download_utc: DateTime<Utc> = image_data.last_success.unwrap().into();
            let text: String = format!(
                "{}: {} (diff: {})",
                last_download_utc.format("%Y-%m-%d %H:%M:%SZ").to_string(),
                hash.to_base64(),
                hash_diff
            );
            draw_text(&mut image, &text, 0);
            encode_image(&image, &mut image_data.last_successful_image)
                .map_err(|_| DownloadError::DownloadingError)?;
            image_data.image_hash = Some(hash);
            image_data.hash_diff = Some(hash_diff);
        }
        debug!("Image data consumed");
        self.update_all_mjpeg().await;
        Ok(())
    }

    #[instrument]
    async fn update_all_mjpeg(&self) {
        let image = {
            let image_data = self.image_data.read().await;
            image_data.last_successful_image.clone()
        };
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

        let image_storer_rx = self.broadcast_rx.clone();
        let store_images = async move {
            use tokio::io::AsyncWriteExt;
            if let Some(save_path) = self.save_path {
                info!("Starting saving images");
                let mut image_stream =
                    tokio::time::throttle(std::time::Duration::from_secs(10), image_storer_rx);
                while let Some(image) = image_stream.next().await {
                    info!("Got image...saving");
                    let (last_success, stale, hash_diff) = {
                        let image_data = self.image_data.read().await;
                        info!("Got lock...saving");
                        (
                            image_data.last_success,
                            image_data.stale,
                            image_data.hash_diff,
                        )
                    };
                    if let Some(last_success) = last_success {
                        info!("Got successful image...saving");
                        if !stale {
                            info!("not stale...saving");
                            let last_success_utc: DateTime<Utc> = last_success.into();
                            let filename = format!(
                                "diff={hash_diff:04},time={isodate}.jpg",
                                hash_diff = hash_diff.unwrap(),
                                isodate = last_success_utc.format("%Y-%m-%dT%H%M%SZ").to_string()
                            );
                            let folder_abs = save_path.join(
                                last_success_utc
                                    .format("year=%Y-week=%V,weekday=%u")
                                    .to_string(),
                            );
                            let folder_create = tokio::fs::create_dir_all(&folder_abs).await;
                            if folder_create.is_ok() {
                                info!("ok folder created");
                            } else if let Err(folder_create) = folder_create {
                                warn!("err creating folder {:?}: {}", folder_abs, folder_create);
                            }
                            let file = tokio::fs::File::create(folder_abs.join(filename)).await;
                            if let Ok(mut file) = file {
                                info!("ok file");
                                if let Err(err) = file.write_all(&image).await {
                                    warn!("Error writing file: {}", err);
                                }
                            } else if let Err(file) = file {
                                warn!("err file {}", file);
                            }
                        }
                    }
                }
            }
        };

        let download_loop = self.download_picture_async_loop(http_client);
        future::join4(http_server, stale_monitor, download_loop, store_images)
    }

    pub async fn serve(
        &'static self,
        request: Request<Body>,
    ) -> Result<Response<Body>, Infallible> {
        debug!(
            "Request in, user-agent: {:?}",
            request.headers().get("user-agent")
        );
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
                debug!("-> Bad request: auth not ok");
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::from("Unauthorized"))
                    .expect("Could not create response"));
            }
        } else {
            debug!("-> Bad request: no query string");
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
                    let image_data = self.image_data.read().await;
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
                let body = Body::wrap_stream::<_, Bytes, Infallible>(StreamTracker::new(
                    self.active_streams.clone(),
                    image_stream,
                ));
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
